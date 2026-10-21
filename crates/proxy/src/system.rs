//! Windows system proxy (`WinINET` settings) reading.
//!
//! Clash/V2Ray "system proxy" toggles write `HKCU\Software\Microsoft\Windows\
//! CurrentVersion\Internet Settings`, which reqwest does **not** read in this
//! build (its `system-proxy` feature is off), so heartflow reads the three
//! values itself. The parsing half is a pure function over the raw strings so
//! the quirks below are unit-testable on any platform; only the registry read
//! is `cfg(windows)`.
//!
//! Quirks handled (same set hyper-util covers behind its feature flag):
//! - `ProxyServer` is either `host:port` or a per-protocol list
//!   `http=…;https=…;ftp=…;socks=…`; the `https=` entry wins, then `http=`.
//! - `ProxyOverride` is `;`-separated; `*.` prefixes are stripped and IPv4
//!   wildcards like `192.168.*` become CIDR. `<local>` is dropped because
//!   loopback is always bypassed anyway.

use url::Url;

/// What the registry says, plus anything worth warning about.
pub(super) struct SystemProxy {
    pub(super) url: Url,
    pub(super) no_proxy: Vec<String>,
    pub(super) warnings: Vec<String>,
}

/// Read the `WinINET` proxy settings. Returns `None` when the key is missing,
/// `ProxyEnable` is 0, or `ProxyServer` is empty.
#[cfg(windows)]
pub(super) fn read() -> Option<SystemProxy> {
    use windows_registry::CURRENT_USER;
    let settings = CURRENT_USER
        .open(r"Software\Microsoft\Windows\CurrentVersion\Internet Settings")
        .ok()?;
    if settings.get_u32("ProxyEnable").unwrap_or(0) == 0 {
        return None;
    }
    let server = settings.get_string("ProxyServer").unwrap_or_default();
    if server.trim().is_empty() {
        return None;
    }
    let overrides = settings.get_string("ProxyOverride").unwrap_or_default();
    let mut warnings = Vec::new();
    let Some(url) = parse_proxy_server(&server).or_else(|| parse_socks_entry(&server)) else {
        warnings.push(format!(
            "system ProxyServer {server:?} has no usable http/https/socks entry; ignored"
        ));
        return None;
    };
    Some(SystemProxy {
        url,
        no_proxy: parse_override(&overrides),
        warnings,
    })
}

/// Pick the proxy URL out of a `ProxyServer` value.
///
/// `host:port` (no `=`) is the whole value; a per-protocol list prefers
/// `https=`, then `http=`. A missing scheme defaults to `http://`.
fn parse_proxy_server(server: &str) -> Option<Url> {
    if !server.contains('=') {
        return env_parse(server.trim());
    }
    let entries = protocol_entries(server);
    entries
        .iter()
        .find(|(protocol, _)| *protocol == "https")
        .or_else(|| entries.iter().find(|(protocol, _)| *protocol == "http"))
        .and_then(|(_, value)| env_parse(value))
}

/// `socks=host:port` is accepted as a last resort (some clients write it).
fn parse_socks_entry(server: &str) -> Option<Url> {
    protocol_entries(server)
        .into_iter()
        .find(|(protocol, _)| *protocol == "socks")
        .and_then(|(_, value)| env_parse(&format!("socks5://{value}")))
}

fn protocol_entries(server: &str) -> Vec<(String, &str)> {
    server
        .split(';')
        .filter_map(|entry| entry.split_once('='))
        .map(|(protocol, value)| (protocol.trim().to_ascii_lowercase(), value.trim()))
        .collect()
}

/// Reuse the env module's URL validation (same accepted schemes).
fn env_parse(raw: &str) -> Option<Url> {
    super::env::parse_proxy_url(raw)
}

/// Normalize a `ProxyOverride` list into `no_proxy` entries.
fn parse_override(raw: &str) -> Vec<String> {
    raw.split(';')
        .map(str::trim)
        .filter(|entry| !entry.is_empty() && *entry != "<local>")
        .map(normalize_entry)
        .collect()
}

fn normalize_entry(entry: &str) -> String {
    let lowered = entry.trim().to_ascii_lowercase();
    let stripped = lowered.strip_prefix("*.").unwrap_or(&lowered);
    ipv4_wildcard_to_cidr(stripped).unwrap_or_else(|| stripped.to_string())
}

/// `192.168.*` → `192.168.0.0/16` (mirrors hyper-util's normalization).
fn ipv4_wildcard_to_cidr(value: &str) -> Option<String> {
    let parts: Vec<&str> = value.split('.').collect();
    let wildcard = parts.iter().position(|part| *part == "*")?;
    if wildcard == 0 || wildcard > 3 || parts[wildcard..].iter().any(|part| *part != "*") {
        return None;
    }
    let mut octets = [0_u8; 4];
    for (index, part) in parts[..wildcard].iter().enumerate() {
        octets[index] = part.parse().ok()?;
    }
    Some(format!(
        "{}.{}.{}.{}/{}",
        octets[0],
        octets[1],
        octets[2],
        octets[3],
        wildcard * 8
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_host_port() {
        let url = parse_proxy_server("127.0.0.1:7890").expect("url");
        assert_eq!(url.as_str(), "http://127.0.0.1:7890/");
    }

    #[test]
    fn per_protocol_list_prefers_https_entry() {
        let url =
            parse_proxy_server("http=1.2.3.4:8080;https=5.6.7.8:8443;ftp=9.9.9.9:21").expect("url");
        assert_eq!(url.as_str(), "http://5.6.7.8:8443/");
    }

    #[test]
    fn per_protocol_list_falls_back_to_http_entry() {
        let url = parse_proxy_server("http=1.2.3.4:8080;ftp=9.9.9.9:21").expect("url");
        assert_eq!(url.as_str(), "http://1.2.3.4:8080/");
    }

    #[test]
    fn socks_entry_is_a_last_resort() {
        let url = parse_socks_entry("socks=127.0.0.1:1080").expect("url");
        assert_eq!(url.scheme(), "socks5");
        assert_eq!(url.port(), Some(1080));
    }

    #[test]
    fn override_list_normalization() {
        let entries = parse_override("<local>;*.example.com;192.168.*;10.0.0.1;");
        assert_eq!(entries, vec!["example.com", "192.168.0.0/16", "10.0.0.1"]);
    }

    #[test]
    fn wildcard_conversion_edges() {
        assert_eq!(ipv4_wildcard_to_cidr("10.*").as_deref(), Some("10.0.0.0/8"));
        assert_eq!(ipv4_wildcard_to_cidr("*.example.com"), None);
        assert_eq!(ipv4_wildcard_to_cidr("192.168.*.5"), None);
    }
}
