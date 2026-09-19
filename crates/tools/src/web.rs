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
    let mut text = if is_html { html_to_text(&body) } else { body };

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

#[cfg(test)]
mod tests {
    use super::{
        build_cookie_header, collapse_whitespace, decode_entity, ensure_public, extract_title,
        host_matches, html_to_text, is_blocked_ip, load_cookie_jar, parse_set_cookie,
        save_cookie_jar, store_set_cookies, CookieJar,
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
}
