use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::io::Read;
use std::net::{IpAddr, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

/// Maximum response bytes pulled off the wire; bounds memory and tokens.
const MAX_RESPONSE_BYTES: u64 = 1_048_576; // 1 MiB
/// Characters of extracted text handed back to the model.
const MAX_TEXT_CHARS: usize = 20_000;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// A current Chrome/Windows x64 UA: many sites 403 bare or custom agents, so
/// `web_fetch` impersonates a mainstream browser to get the real page.
const USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/140.0.0.0 Safari/537.36";

#[derive(Debug, Deserialize)]
pub struct WebFetchInput {
    pub url: String,
    /// When true, return the untransformed body instead of extracted text.
    pub raw: Option<bool>,
}

#[derive(Debug, Serialize)]
pub struct WebFetchReport {
    pub url: String,
    pub status: u16,
    pub content_type: String,
    pub title: Option<String>,
    pub truncated: bool,
    pub text: String,
}

#[derive(Debug)]
pub enum WebError {
    InvalidUrl(String),
    BlockedHost(String),
    Http(String),
}

impl std::fmt::Display for WebError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidUrl(message) => write!(f, "invalid url: {message}"),
            Self::BlockedHost(host) => write!(f, "blocked host for safety: {host}"),
            Self::Http(message) => write!(f, "fetch failed: {message}"),
        }
    }
}

impl std::error::Error for WebError {}

/// Fetch `url` over HTTP(S) and return extracted text. Guards against SSRF by
/// allowing only http/https, rejecting loopback/private/link-local/metadata
/// addresses (by literal IP, DNS resolution, and final-redirect re-check), and
/// capping body size and wall-clock time.
pub fn web_fetch(input: &WebFetchInput) -> Result<WebFetchReport, WebError> {
    let url =
        reqwest::Url::parse(&input.url).map_err(|error| WebError::InvalidUrl(error.to_string()))?;
    ensure_public(&url)?;

    let client = reqwest::blocking::Client::builder()
        .user_agent(USER_AGENT)
        .timeout(REQUEST_TIMEOUT)
        .redirect(reqwest::redirect::Policy::limited(5))
        .build()
        .map_err(|error| WebError::Http(error.to_string()))?;

    // Opt-in session cookies: when HEARTFLOW_COOKIE_JAR names a file, replay
    // stored cookies for this host and capture any the server sets. Unset keeps
    // web_fetch stateless so no one is surprised by a state file on disk.
    let jar_path = cookie_jar_path();
    let mut jar = jar_path
        .as_ref()
        .map(|path| load_cookie_jar(path))
        .unwrap_or_default();
    let host = url.host_str().unwrap_or_default().to_ascii_lowercase();

    let mut request = client.get(url.clone());
    if let Some(header) = build_cookie_header(&jar, &host) {
        request = request.header(reqwest::header::COOKIE, header);
    }
    let response = request
        .send()
        .map_err(|error| WebError::Http(error.to_string()))?;

    // A redirect can move an allowed host onto an internal one; re-check final URL.
    let final_url = response.url().clone();
    if final_url != url {
        ensure_public(&final_url)?;
    }
    let set_cookies: Vec<String> = response
        .headers()
        .get_all(reqwest::header::SET_COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .map(str::to_string)
        .collect();
    let status = response.status().as_u16();
    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("")
        .to_string();

    let mut buf = Vec::new();
    response
        .take(MAX_RESPONSE_BYTES + 1)
        .read_to_end(&mut buf)
        .map_err(|error| WebError::Http(error.to_string()))?;
    let over_cap = buf.len() as u64 > MAX_RESPONSE_BYTES;
    if over_cap {
        buf.truncate(usize::try_from(MAX_RESPONSE_BYTES).unwrap_or(usize::MAX));
    }
    let body = String::from_utf8_lossy(&buf).into_owned();

    let wants_raw = input.raw.unwrap_or(false);
    let is_html = !wants_raw
        && (content_type.contains("html")
            || body.trim_start().starts_with("<!DOCTYPE")
            || body.trim_start().starts_with("<html"));
    let title = is_html.then(|| extract_title(&body)).flatten();
    let mut text = if is_html {
        html_to_markdown(&body)
    } else {
        body
    };

    let mut truncated = over_cap;
    if text.chars().count() > MAX_TEXT_CHARS {
        text = text.chars().take(MAX_TEXT_CHARS).collect();
        truncated = true;
    }

    if let (Some(path), Some(final_host)) = (jar_path.as_deref(), final_url.host_str()) {
        store_set_cookies(&mut jar, &set_cookies, final_host);
        save_cookie_jar(path, &jar);
    }

    Ok(WebFetchReport {
        url: final_url.to_string(),
        status,
        content_type,
        title,
        truncated,
        text,
    })
}

/// Persistent name->value cookies keyed by host scope. Deliberately minimal: it
/// ignores `Path`/`Secure`/`Expires` attributes and only scopes by `Domain` (or
/// the response host), which covers login/session replay for the built-in
/// `web_fetch` without pulling a cookie crate into the offline dependency set.
type CookieJar = BTreeMap<String, BTreeMap<String, String>>;

/// Jar file path when cookie persistence is enabled via `HEARTFLOW_COOKIE_JAR`.
fn cookie_jar_path() -> Option<PathBuf> {
    env::var_os("HEARTFLOW_COOKIE_JAR").map(PathBuf::from)
}

fn load_cookie_jar(path: &Path) -> CookieJar {
    fs::read_to_string(path)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

fn save_cookie_jar(path: &Path, jar: &CookieJar) {
    if let Ok(text) = serde_json::to_string_pretty(jar) {
        let _ = fs::write(path, text);
    }
}

/// A stored `scope` applies to `host` on exact match or parent-domain suffix
/// (`example.com` covers `www.example.com`, never `evilexample.com`).
fn host_matches(scope: &str, host: &str) -> bool {
    host == scope || host.ends_with(&format!(".{scope}"))
}

/// Build a `Cookie` header from every jar entry whose scope applies to `host`.
fn build_cookie_header(jar: &CookieJar, host: &str) -> Option<String> {
    let mut pairs = Vec::new();
    for (scope, cookies) in jar {
        if host_matches(scope, host) {
            pairs.extend(
                cookies
                    .iter()
                    .map(|(name, value)| format!("{name}={value}")),
            );
        }
    }
    (!pairs.is_empty()).then(|| pairs.join("; "))
}

/// Split a `Set-Cookie` value into (name, value, optional Domain attribute).
fn parse_set_cookie(raw: &str) -> Option<(String, String, Option<String>)> {
    let mut segments = raw.split(';');
    let (name, value) = segments.next()?.split_once('=')?;
    let name = name.trim();
    if name.is_empty() {
        return None;
    }
    let domain = segments.find_map(|attr| {
        let (key, val) = attr.split_once('=')?;
        key.trim()
            .eq_ignore_ascii_case("domain")
            .then(|| val.trim().to_string())
    });
    Some((name.to_string(), value.trim().to_string(), domain))
}

/// Merge captured `Set-Cookie` headers into `jar`, scoping each by its `Domain`
/// attribute (leading dot stripped) or the response host when absent.
fn store_set_cookies(jar: &mut CookieJar, set_cookies: &[String], response_host: &str) {
    let response_host = response_host.to_ascii_lowercase();
    for raw in set_cookies {
        let Some((name, value, domain)) = parse_set_cookie(raw) else {
            continue;
        };
        let scope = domain.map_or_else(
            || response_host.clone(),
            |domain| domain.trim_start_matches('.').to_ascii_lowercase(),
        );
        if scope.is_empty() {
            continue;
        }
        jar.entry(scope).or_default().insert(name, value);
    }
}

/// Reject non-http(s) schemes and any host that resolves to a private, loopback,
/// link-local, multicast, or otherwise internal address.
pub(crate) fn ensure_public(url: &reqwest::Url) -> Result<(), WebError> {
    if !matches!(url.scheme(), "http" | "https") {
        return Err(WebError::InvalidUrl(format!(
            "unsupported scheme '{}'",
            url.scheme()
        )));
    }
    let host = url
        .host_str()
        .ok_or_else(|| WebError::InvalidUrl("url has no host".to_string()))?;
    let host_lower = host.to_ascii_lowercase();
    if host_lower == "localhost"
        || [".localhost", ".local", ".internal"]
            .iter()
            .copied()
            .any(|suffix| host_lower.ends_with(suffix))
    {
        return Err(WebError::BlockedHost(host.to_string()));
    }

    if let Ok(ip) = host.parse::<IpAddr>() {
        return if is_blocked_ip(ip) {
            Err(WebError::BlockedHost(host.to_string()))
        } else {
            Ok(())
        };
    }

    let port = url.port_or_known_default().unwrap_or(443);
    let ips = (host, port)
        .to_socket_addrs()
        .map_err(|_| WebError::BlockedHost(format!("{host} (unresolved)")))?
        .map(|addr| addr.ip())
        .collect::<Vec<_>>();
    if ips.is_empty() {
        return Err(WebError::BlockedHost(format!("{host} (no addresses)")));
    }
    if ips.iter().copied().any(is_blocked_ip) {
        return Err(WebError::BlockedHost(host.to_string()));
    }
    Ok(())
}

/// Internal-use address ranges an agent must never reach (SSRF / cloud metadata).
fn is_blocked_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || v4.is_unspecified()
                || v4.is_multicast()
                || v4.is_broadcast()
        }
        IpAddr::V6(v6) => {
            // Order matters: some toolchains map `::1` through `to_ipv4`, so
            // check native IPv6 internal ranges before any embedded-IPv4 fallback.
            if v6.is_loopback() || v6.is_unspecified() || v6.is_multicast() {
                return true;
            }
            // fc00::/7 unique-local addresses.
            if (v6.segments()[0] & 0xfe00) == 0xfc00 {
                return true;
            }
            if let Some(v4) = v6.to_ipv4() {
                return is_blocked_ip(IpAddr::V4(v4));
            }
            false
        }
    }
}

/// Extract the first `<title>` text from an HTML document.
fn extract_title(html: &str) -> Option<String> {
    let lower = html.to_ascii_lowercase();
    let start = lower.find("<title")?;
    let gt = lower[start..].find('>')?;
    let content_start = start + gt + 1;
    let end = lower[content_start..].find("</title>")?;
    let raw = &html[content_start..content_start + end];
    let text = html_to_text(raw);
    (!text.is_empty()).then_some(text)
}

/// Strip markup to readable text: drop script/style/head/svg/template blocks,
/// remove remaining tags, decode common entities, and collapse whitespace.
fn html_to_text(html: &str) -> String {
    let lower = html.to_ascii_lowercase();
    let bytes = html.as_bytes();
    let skip = [
        "<script",
        "<style",
        "<head",
        "<noscript",
        "<template",
        "<svg",
    ];
    let mut out = String::with_capacity(html.len() / 2 + 8);
    let mut i = 0_usize;
    while i < bytes.len() {
        let mut advanced = false;
        for tag in skip {
            if lower[i..].starts_with(tag) {
                let name = &tag[1..];
                let close_needle = format!("</{name}");
                if let Some(rel) = lower[i..].find(&close_needle) {
                    let mut j = i + rel + close_needle.len();
                    while j < bytes.len() && bytes[j] != b'>' {
                        j += 1;
                    }
                    i = (j + 1).min(bytes.len());
                    advanced = true;
                    break;
                }
            }
        }
        if advanced {
            continue;
        }
        if bytes[i] == b'<' {
            while i < bytes.len() && bytes[i] != b'>' {
                i += 1;
            }
            i = (i + 1).min(bytes.len());
            out.push(' ');
            continue;
        }
        if bytes[i] == b'&' {
            let window = &lower[i..(i + 12).min(lower.len())];
            if let Some(semi) = window.find(';') {
                let entity = &lower[i + 1..i + semi];
                out.push_str(&decode_entity(entity));
                i += semi + 1;
                continue;
            }
        }
        let ch = html[i..].chars().next().unwrap_or('\u{fffd}');
        out.push(ch);
        i += ch.len_utf8();
    }
    collapse_whitespace(&out)
}

/// Decode the handful of HTML entities that commonly appear in body text.
fn decode_entity(name: &str) -> String {
    match name {
        "amp" => "&".to_string(),
        "lt" => "<".to_string(),
        "gt" => ">".to_string(),
        "quot" => "\"".to_string(),
        "apos" => "'".to_string(),
        "nbsp" => " ".to_string(),
        other => {
            if let Some(hex) = other
                .strip_prefix("#x")
                .or_else(|| other.strip_prefix("#X"))
            {
                u32::from_str_radix(hex, 16)
                    .ok()
                    .and_then(char::from_u32)
                    .map_or_else(|| format!("&{other};"), String::from)
            } else if let Some(dec) = other.strip_prefix('#') {
                dec.parse::<u32>()
                    .ok()
                    .and_then(char::from_u32)
                    .map_or_else(|| format!("&{other};"), String::from)
            } else {
                format!("&{other};")
            }
        }
    }
}

/// Collapse all runs of whitespace (including newlines) into single spaces.
fn collapse_whitespace(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Decode HTML entities in a tag-free text run (reusing [`decode_entity`]).
fn decode_html_text(text: &str) -> String {
    let lower = text.to_ascii_lowercase();
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i] == b'&' {
            let window = &lower[i..(i + 12).min(lower.len())];
            if let Some(semi) = window.find(';') {
                let entity = &lower[i + 1..i + semi];
                out.push_str(&decode_entity(entity));
                i += semi + 1;
                continue;
            }
        }
        let ch = text[i..].chars().next().unwrap_or('\u{fffd}');
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

/// Ensure `out` sits at the start of a fresh line (no-op at buffer start or when
/// already on a new line), so block elements break without stacking blank rows.
fn ensure_newline(out: &mut String) {
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
}

/// Append decoded text, collapsing any whitespace run to a single space (unless
/// `collapse` is false, i.e. inside a `<pre>` block) and never starting a line
/// with a stray space.
fn push_text(out: &mut String, text: &str, collapse: bool) {
    let decoded = decode_html_text(text);
    if !collapse {
        out.push_str(&decoded);
        return;
    }
    for ch in decoded.chars() {
        if ch.is_whitespace() {
            if !out.is_empty() && !out.ends_with('\n') && !out.ends_with(' ') && !out.ends_with('[')
            {
                out.push(' ');
            }
            continue;
        }
        out.push(ch);
    }
}

/// Extract the value of attribute `key` from an opening tag's interior (the text
/// after `<`). Byte offsets of the lower-cased copy line up with the original
/// because only ASCII folding differs, so the value is sliced case-preservingly
/// (URL paths are case sensitive) and entity-decoded.
fn attr(tag: &str, key: &str) -> Option<String> {
    let lower_tag = tag.to_ascii_lowercase();
    let at = lower_tag.find(key)?;
    let after = &lower_tag[at + key.len()..];
    let ws = after.len() - after.trim_start().len();
    let eq = after.trim_start();
    if !eq.starts_with('=') {
        return None;
    }
    let value_from = at + key.len() + ws + 1; // just past '='
    let rest = tag.get(value_from..)?.trim_start();
    let first = *rest.as_bytes().first()?;
    let value = if first == b'"' || first == b'\'' {
        let s = &rest[1..];
        let end = s.find(first as char)?;
        &s[..end]
    } else {
        let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
        &rest[..end]
    };
    (!value.is_empty()).then(|| decode_html_text(value))
}

/// Turn an HTML document into lightweight, structure-preserving markdown, so a
/// documentation page reaches the model as readable sections rather than one
/// collapsed line. It drops non-prose blocks (`script`/`style`/`head`/...), puts
/// block elements on their own line, marks headings with `#`, list items with
/// `- `, fences `<pre>` as code, and keeps anchors as `[text](url)` so the agent
/// can act on the URL (a keyless, dependency-free idea borrowed from how good
/// agent fetch tools spend tokens). Not a full DOM parser: unrecognised markup
/// degrades to its text content.
#[allow(clippy::too_many_lines)] // one cohesive linear HTML scanner
fn html_to_markdown(html: &str) -> String {
    const SKIP: [&str; 6] = ["script", "style", "head", "noscript", "template", "svg"];
    const BLOCK: [&str; 22] = [
        "p",
        "div",
        "section",
        "article",
        "header",
        "footer",
        "nav",
        "aside",
        "main",
        "blockquote",
        "table",
        "thead",
        "tbody",
        "tfoot",
        "ul",
        "ol",
        "dl",
        "dt",
        "dd",
        "figure",
        "figcaption",
        "form",
    ];
    let lower = html.to_ascii_lowercase();
    let bytes = html.as_bytes();
    let n = bytes.len();
    let mut out = String::with_capacity(html.len() / 2 + 8);
    let mut i = 0usize;
    let mut link_href: Option<String> = None;
    let mut in_pre = false;
    while i < n {
        if bytes[i] == b'<' {
            let mut j = i + 1;
            while j < n && bytes[j] != b'>' {
                j += 1;
            }
            let inner = &html[i + 1..j.min(n)];
            i = if j < n { j + 1 } else { n };
            if inner.is_empty() {
                continue;
            }
            let closing = inner.starts_with('/');
            let body = if closing { &inner[1..] } else { inner };
            if body.starts_with('!') {
                continue; // comment or doctype
            }
            let name_end = body.find(char::is_whitespace).unwrap_or(body.len());
            let name = body[..name_end].to_ascii_lowercase();
            if SKIP.contains(&name.as_str()) {
                if !closing {
                    i = match lower[i..].find(&format!("</{name}")) {
                        Some(rel) => i + rel,
                        None => n,
                    };
                }
                continue;
            }
            match name.as_str() {
                "br" => out.push('\n'),
                "hr" => ensure_newline(&mut out),
                "pre" => {
                    ensure_newline(&mut out);
                    out.push_str("```\n");
                    in_pre = !closing;
                }
                "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => {
                    if closing {
                        ensure_newline(&mut out);
                    } else {
                        ensure_newline(&mut out);
                        let level = name.as_bytes()[1] - b'0';
                        for _ in 0..level {
                            out.push('#');
                        }
                        out.push(' ');
                    }
                }
                "li" => {
                    if closing {
                        ensure_newline(&mut out);
                    } else {
                        ensure_newline(&mut out);
                        out.push_str("- ");
                    }
                }
                "a" => {
                    if closing {
                        if let Some(href) = link_href.take() {
                            out.push_str("](");
                            out.push_str(&href);
                            out.push(')');
                        }
                    } else if let Some(href) = attr(body, "href") {
                        if href.starts_with("http://") || href.starts_with("https://") {
                            link_href = Some(href);
                            out.push('[');
                        }
                    }
                }
                _ => {
                    if BLOCK.contains(&name.as_str()) {
                        ensure_newline(&mut out);
                    }
                }
            }
        } else {
            let mut k = i;
            while k < n && bytes[k] != b'<' {
                k += 1;
            }
            push_text(&mut out, &html[i..k], !in_pre);
            i = k;
        }
    }
    finalize_markdown(&out)
}

/// Collapse runs of newlines to at most one blank line and trim the ends.
fn finalize_markdown(src: &str) -> String {
    let mut out = String::with_capacity(src.len());
    let mut newlines = 0usize;
    for ch in src.chars() {
        if ch == '\n' {
            newlines += 1;
            continue;
        }
        if newlines > 0 {
            out.push('\n');
            if newlines > 1 {
                out.push('\n');
            }
            newlines = 0;
        }
        out.push(ch);
    }
    out.trim().to_string()
}

/// One ranked search result: a title, its URL (the actionable handle to pass to
/// `web_fetch`), and a short snippet.
#[derive(Debug, Clone, Serialize)]
pub struct WebSearchHit {
    pub title: String,
    pub url: String,
    pub snippet: String,
}

/// Output of [`web_search`]. `provider` names the engine used so the model (and
/// a future BYOK path) can tell where results came from.
#[derive(Debug, Serialize)]
pub struct WebSearchReport {
    pub query: String,
    pub provider: &'static str,
    pub hits: Vec<WebSearchHit>,
    pub note: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct WebSearchInput {
    pub query: String,
    pub max_results: Option<usize>,
}

/// Default and cap for the number of results returned.
const SEARCH_DEFAULT_RESULTS: usize = 8;
const SEARCH_MAX_RESULTS: usize = 25;

/// Keyless web search over one engine (`DuckDuckGo`'s HTML endpoint), returning
/// ranked `(title, url, snippet)` references rather than page bodies — the
/// agent then fetches the URL it wants. Deliberately lean: a single engine and a
/// pure, unit-tested parser, not a multi-engine consensus/rerank stack. Empty
/// results are reported honestly (never a fabricated answer).
pub fn web_search(input: &WebSearchInput) -> Result<WebSearchReport, WebError> {
    let limit = input
        .max_results
        .unwrap_or(SEARCH_DEFAULT_RESULTS)
        .clamp(1, SEARCH_MAX_RESULTS);
    let mut endpoint = reqwest::Url::parse("https://html.duckduckgo.com/html/")
        .map_err(|e| WebError::InvalidUrl(e.to_string()))?;
    endpoint.query_pairs_mut().append_pair("q", &input.query);
    ensure_public(&endpoint)?;

    let client = reqwest::blocking::Client::builder()
        .user_agent(USER_AGENT)
        .timeout(REQUEST_TIMEOUT)
        .redirect(reqwest::redirect::Policy::limited(5))
        .build()
        .map_err(|e| WebError::Http(e.to_string()))?;
    let response = client
        .get(endpoint)
        .send()
        .map_err(|e| WebError::Http(e.to_string()))?;
    let status = response.status().as_u16();
    if !(200..300).contains(&status) {
        return Err(WebError::Http(format!("search engine returned {status}")));
    }
    let mut buf = Vec::new();
    response
        .take(MAX_RESPONSE_BYTES + 1)
        .read_to_end(&mut buf)
        .map_err(|e| WebError::Http(e.to_string()))?;
    if buf.len() as u64 > MAX_RESPONSE_BYTES {
        buf.truncate(usize::try_from(MAX_RESPONSE_BYTES).unwrap_or(usize::MAX));
    }
    let body = String::from_utf8_lossy(&buf).into_owned();

    let mut hits = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for hit in parse_duckduckgo(&body) {
        if hit.url.is_empty() || hit.title.is_empty() || !seen.insert(hit.url.clone()) {
            continue;
        }
        hits.push(hit);
        if hits.len() >= limit {
            break;
        }
    }
    let note = hits.is_empty().then(|| {
        "no results from the keyless engine; refine the query or web_fetch a known URL".to_string()
    });
    Ok(WebSearchReport {
        query: input.query.clone(),
        provider: "duckduckgo",
        hits,
        note,
    })
}

/// Parse `DuckDuckGo`'s HTML SERP into ranked hits. Each result is an anchor with
/// `class="result__a"` (the title, behind a `uddg=` redirect) followed by a
/// `class="result__snippet"` anchor. Pure and offline-testable; the live fetch
/// only feeds it bytes.
fn parse_duckduckgo(html: &str) -> Vec<WebSearchHit> {
    let lower = html.to_ascii_lowercase();
    let mut hits = Vec::new();
    let mut from = 0usize;
    while let Some(rel) = lower[from..].find("result__a") {
        let at = from + rel;
        let Some(gt_rel) = html[at..].find('>') else {
            break;
        };
        let gt = at + gt_rel;
        let a_start = html[..at].rfind("<a").unwrap_or(at);
        let href = attr(&html[a_start..gt], "href").unwrap_or_default();
        let Some(close_rel) = lower[gt..].find("</a>") else {
            break;
        };
        let close = gt + close_rel;
        let title = collapse_whitespace(&html_to_text(&html[gt + 1..close]));
        let snippet = match lower[close..].find("result__snippet") {
            Some(rel2) => {
                let sp = close + rel2;
                let sgt = html[sp..].find('>').map_or(sp, |g| sp + g);
                let scl = html[sgt..].find("</a>").map_or(sgt, |c| sgt + c);
                let raw = html.get((sgt + 1).min(scl)..scl).unwrap_or("");
                collapse_whitespace(&html_to_text(raw))
            }
            None => String::new(),
        };
        let url = decode_ddg_redirect(&href);
        if !url.is_empty() && !title.is_empty() {
            hits.push(WebSearchHit {
                title,
                url,
                snippet,
            });
        }
        from = close.max(at + 1);
    }
    hits
}

/// `DuckDuckGo` wraps every result URL as `//duckduckgo.com/l/?uddg=<encoded>`.
/// Unwrap the real target (percent-decoded via the URL parser); pass through a
/// plain http(s) link, and drop anything else (e.g. ad or internal links).
fn decode_ddg_redirect(href: &str) -> String {
    let candidate = if href.strip_prefix("//").is_some() {
        format!("https:{href}")
    } else {
        href.to_string()
    };
    if let Ok(u) = reqwest::Url::parse(&candidate) {
        if let Some(target) = u
            .query_pairs()
            .find(|(k, _)| k == "uddg")
            .map(|(_, v)| v.into_owned())
        {
            return target;
        }
        if matches!(u.scheme(), "http" | "https") {
            return candidate;
        }
    }
    String::new()
}

#[cfg(test)]
mod tests {
    use super::{
        attr, build_cookie_header, collapse_whitespace, decode_ddg_redirect, decode_entity,
        ensure_public, extract_title, host_matches, html_to_markdown, html_to_text, is_blocked_ip,
        load_cookie_jar, parse_duckduckgo, parse_set_cookie, save_cookie_jar, store_set_cookies,
        CookieJar,
    };
    use std::net::IpAddr;

    #[test]
    fn blocks_internal_addresses() {
        for ip in [
            "127.0.0.1",
            "10.0.0.5",
            "192.168.1.1",
            "172.16.0.1",
            "169.254.169.254",
            "0.0.0.0",
            "::1",
            "fc00::1",
            "::ffff:127.0.0.1",
        ] {
            let ip: IpAddr = ip.parse().expect("literal ip");
            assert!(is_blocked_ip(ip), "{ip} should be blocked");
        }
        for ip in ["93.184.216.34", "2606:2800:220:1:248:1893:25c8:1946"] {
            let ip: IpAddr = ip.parse().expect("public ip");
            assert!(!is_blocked_ip(ip), "{ip} should be allowed");
        }
    }

    #[test]
    fn rejects_private_and_bad_scheme() {
        let scheme = reqwest::Url::parse("ftp://example.com").expect("parse");
        assert!(ensure_public(&scheme).is_err());
        let loopback = reqwest::Url::parse("http://127.0.0.1/secret").expect("parse");
        assert!(ensure_public(&loopback).is_err());
        let meta = reqwest::Url::parse("http://169.254.169.254/latest/meta-data").expect("parse");
        assert!(ensure_public(&meta).is_err());
        let named = reqwest::Url::parse("http://localhost/").expect("parse");
        assert!(ensure_public(&named).is_err());
    }

    #[test]
    fn strips_markup_and_decodes_entities() {
        let html = "<html><head><title>T</title></head><body><script>alert(1)</script><style>p{}</style><p>Hello &amp; welcome&#33;</p><div>  spaced   out </div></body></html>";
        let text = html_to_text(html);
        assert!(text.contains("Hello & welcome!"));
        assert!(text.contains("spaced out"));
        assert!(!text.contains("alert"));
        assert!(!text.contains("p{}"));
        assert!(!text.contains('<'));
    }

    #[test]
    fn extracts_title() {
        let html = "<html><head><title> My Page &amp; More </title></head><body>x</body></html>";
        assert_eq!(extract_title(html).as_deref(), Some("My Page & More"));
    }

    #[test]
    fn entity_and_whitespace_helpers() {
        assert_eq!(decode_entity("amp"), "&");
        assert_eq!(decode_entity("#65"), "A");
        assert_eq!(decode_entity("#x41"), "A");
        assert_eq!(decode_entity("unknown"), "&unknown;");
        assert_eq!(collapse_whitespace("  a \n b\tc  "), "a b c");
    }

    #[test]
    fn parses_set_cookie_with_attributes() {
        let (name, value, domain) =
            parse_set_cookie("session=abc123; Path=/; Domain=.Example.com; Secure")
                .expect("valid cookie");
        assert_eq!(name, "session");
        assert_eq!(value, "abc123");
        assert_eq!(domain.as_deref(), Some(".Example.com"));
        assert!(parse_set_cookie("=novalue; Path=/").is_none());
    }

    #[test]
    fn cookie_scope_matches_subdomains_only() {
        assert!(host_matches("example.com", "example.com"));
        assert!(host_matches("example.com", "www.example.com"));
        assert!(!host_matches("example.com", "evilexample.com"));
    }

    #[test]
    fn jar_scopes_cookies_and_blocks_leaks() {
        let mut jar = CookieJar::new();
        store_set_cookies(
            &mut jar,
            &[
                "sid=1; Domain=example.com".to_string(),
                "t=2".to_string(), // no Domain -> scoped to the response host
            ],
            "www.example.com",
        );
        assert_eq!(
            jar.get("example.com")
                .and_then(|c| c.get("sid"))
                .map(String::as_str),
            Some("1")
        );
        assert_eq!(
            jar.get("www.example.com")
                .and_then(|c| c.get("t"))
                .map(String::as_str),
            Some("2")
        );
        let header = build_cookie_header(&jar, "shop.example.com").expect("header");
        assert!(header.contains("sid=1"));
        assert!(
            !header.contains("t=2"),
            "host-specific cookie must not leak to a sibling subdomain"
        );
    }

    #[test]
    fn cookie_jar_persists_to_disk() {
        let path = std::env::temp_dir().join(format!("hf-cookies-{}.json", std::process::id()));
        let mut jar = CookieJar::new();
        store_set_cookies(
            &mut jar,
            &["sid=1; Domain=example.com".to_string()],
            "example.com",
        );
        save_cookie_jar(&path, &jar);
        assert_eq!(load_cookie_jar(&path), jar);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn markdown_preserves_structure_not_one_line() {
        let html = "<h1>Title</h1><p>Para one.</p><ul><li>alpha</li><li>beta</li></ul>\
            <p>See <a href=\"https://x.example/p\">the docs &amp; ref</a> for more.</p>\
            <pre>let x = 1;</pre><script>evil()</script>";
        let md = html_to_markdown(html);
        assert!(md.contains("# Title"), "heading marker: {md:?}");
        assert!(md.contains("Para one."), "paragraph text");
        assert!(
            md.contains("- alpha") && md.contains("- beta"),
            "list items"
        );
        assert!(
            md.contains("[the docs & ref](https://x.example/p)"),
            "link kept as text+url: {md:?}"
        );
        assert!(
            md.contains("```") && md.contains("let x = 1;"),
            "code fence"
        );
        assert!(!md.contains("evil"), "script dropped");
        assert!(!md.contains('<'), "no raw tags survive: {md:?}");
        // The point of the change: the body is multi-line, not one collapsed blob.
        assert!(md.lines().count() >= 5, "structured into lines: {md:?}");
    }

    #[test]
    fn markdown_decodes_entities_and_keeps_cjk() {
        let md = html_to_markdown("<p>\u{4f60}\u{597d} &amp; world &lt;3</p>");
        assert!(
            md.contains("\u{4f60}\u{597d} & world <3"),
            "cjk+entities: {md:?}"
        );
    }

    #[test]
    fn markdown_drops_head_and_whitespace_runs() {
        let md = html_to_markdown(
            "<head><title>T</title><meta x=1></head><body><div>  a   b  </div></body>",
        );
        assert!(!md.contains('T'), "head dropped: {md:?}");
        assert_eq!(md.trim(), "a b", "inner run collapsed to one space");
    }

    #[test]
    fn attr_reads_quoted_and_unquoted_values() {
        assert_eq!(
            attr(r#"a href="https://e.com/x?a=1&amp;b=2" class="r""#, "href").as_deref(),
            Some("https://e.com/x?a=1&b=2")
        );
        assert_eq!(
            attr(r"a href=https://e.com/x title=y", "href").as_deref(),
            Some("https://e.com/x")
        );
        assert_eq!(attr("a rel=\"n\"", "href"), None, "missing attr");
        assert_eq!(
            attr("a hrefx=\"y\"", "href"),
            None,
            "href must be followed by ="
        );
    }

    #[test]
    fn ddg_redirect_unwraps_real_url() {
        let encoded = "//duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.com%2Fdoc&rut=abc";
        assert_eq!(decode_ddg_redirect(encoded), "https://example.com/doc");
        assert_eq!(decode_ddg_redirect("https://e.com/p"), "https://e.com/p");
        assert_eq!(decode_ddg_redirect("/ads/ban.gif"), "", "non-http dropped");
    }

    #[test]
    fn parse_duckduckgo_extracts_ranked_hits() {
        let serp = r#"<div class="result">
            <a rel="nofollow" class="result__a" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Frust-lang.org%2Fbook">The Rust Programming Language</a>
            <a class="result__snippet" href="...">A long &amp; detailed <b>book</b> about Rust.</a>
        </div>
        <div class="result">
            <a class="result__a" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fdoc.rust-lang.org%2Fstd">std - Rust</a>
            <a class="result__snippet" href="...">Standard library docs.</a>
        </div>"#;
        let hits = parse_duckduckgo(serp);
        assert_eq!(hits.len(), 2, "two results");
        assert_eq!(hits[0].url, "https://rust-lang.org/book");
        assert_eq!(hits[0].title, "The Rust Programming Language");
        assert_eq!(hits[0].snippet, "A long & detailed book about Rust.");
        assert_eq!(hits[1].url, "https://doc.rust-lang.org/std");
        assert_eq!(
            parse_duckduckgo("<p>no results here</p>").len(),
            0,
            "empty SERP"
        );
    }
}
