//! Environment-variable proxy reading.
//!
//! The lookup order mirrors curl/Go: a scheme-specific variable wins over the
//! generic `ALL_PROXY`, which wins over the other scheme's variable. Only the
//! four canonical spellings of each name are consulted (upper and lower), and
//! an empty value counts as unset. `NO_PROXY` (upper preferred) carries the
//! bypass list.
//!
//! The core is map-driven so the precedence rules are unit-testable without
//! mutating the process environment; [`from_process`] is the thin std wrapper.

use std::collections::BTreeMap;

use url::Url;

/// Schemes a proxy URL may use. `socks5h` resolves the target hostname at the
/// proxy, which matters for split-DNS setups.
const SUPPORTED_SCHEMES: [&str; 6] = ["http", "https", "socks4", "socks4a", "socks5", "socks5h"];

/// Checked in order; the first non-empty hit is the effective proxy.
const ADDRESS_VARS: [&str; 6] = [
    "HTTPS_PROXY",
    "https_proxy",
    "ALL_PROXY",
    "all_proxy",
    "HTTP_PROXY",
    "http_proxy",
];

const NO_PROXY_VARS: [&str; 2] = ["NO_PROXY", "no_proxy"];

/// What the environment says, plus anything worth warning about.
pub(super) struct EnvProxy {
    pub(super) url: Url,
    pub(super) no_proxy: Vec<String>,
    pub(super) warnings: Vec<String>,
}

/// Read the process environment.
pub(super) fn from_process() -> Option<EnvProxy> {
    let map: BTreeMap<String, String> = std::env::vars().collect();
    from_map(&map)
}

/// Pure core: same logic as [`from_process`] over an explicit map.
pub(super) fn from_map(map: &BTreeMap<String, String>) -> Option<EnvProxy> {
    let mut warnings = Vec::new();
    let mut url = None;
    for name in ADDRESS_VARS {
        let Some(raw) = map.get(name).map(String::as_str).map(str::trim) else {
            continue;
        };
        if raw.is_empty() {
            continue;
        }
        match parse_proxy_url(raw) {
            Some(parsed) => {
                url = Some(parsed);
                break;
            }
            None => warnings.push(format!(
                "{name}={raw:?} is not a supported proxy URL (want one of {SUPPORTED_SCHEMES:?}); ignored"
            )),
        }
    }
    let no_proxy = NO_PROXY_VARS
        .iter()
        .find_map(|name| map.get(*name))
        .map(|raw| parse_no_proxy(raw))
        .unwrap_or_default();
    url.map(|url| EnvProxy {
        url,
        no_proxy,
        warnings,
    })
}

/// Validate and normalize a proxy URL. A missing scheme defaults to `http`
/// (matching how `127.0.0.1:7890` is written in practice); an unsupported
/// scheme is rejected so the caller can warn instead of failing the request.
pub(super) fn parse_proxy_url(raw: &str) -> Option<Url> {
    let with_scheme = if raw.contains("://") {
        raw.to_string()
    } else {
        format!("http://{raw}")
    };
    let url = Url::parse(&with_scheme).ok()?;
    if !SUPPORTED_SCHEMES.contains(&url.scheme()) {
        return None;
    }
    // A proxy URL without a host is unusable even though it parses.
    url.host_str()?;
    Some(url)
}

/// Split a `NO_PROXY`-style list into normalized entries.
pub(super) fn parse_no_proxy(raw: &str) -> Vec<String> {
    raw.split(',')
        .map(|entry| {
            let entry = entry.trim().to_ascii_lowercase();
            entry
                .strip_prefix('.')
                .map_or_else(|| entry.clone(), ToString::to_string)
        })
        .filter(|entry| !entry.is_empty())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(key, value)| ((*key).to_string(), (*value).to_string()))
            .collect()
    }

    #[test]
    fn scheme_specific_beats_generic() {
        let env = map(&[
            ("HTTPS_PROXY", "http://secure:1"),
            ("ALL_PROXY", "http://generic:2"),
            ("HTTP_PROXY", "http://plain:3"),
        ]);
        let found = from_map(&env).expect("env proxy");
        assert_eq!(found.url.as_str(), "http://secure:1/");
    }

    #[test]
    fn all_proxy_beats_http_proxy() {
        let env = map(&[
            ("ALL_PROXY", "http://generic:2"),
            ("HTTP_PROXY", "http://plain:3"),
        ]);
        let found = from_map(&env).expect("env proxy");
        assert_eq!(found.url.as_str(), "http://generic:2/");
    }

    #[test]
    fn lowercase_variants_are_read() {
        let env = map(&[("https_proxy", "http://lower:1")]);
        let found = from_map(&env).expect("env proxy");
        assert_eq!(found.url.as_str(), "http://lower:1/");
    }

    #[test]
    fn empty_value_counts_as_unset() {
        let env = map(&[("HTTPS_PROXY", ""), ("HTTP_PROXY", "http://plain:3")]);
        let found = from_map(&env).expect("env proxy");
        assert_eq!(found.url.as_str(), "http://plain:3/");
    }

    #[test]
    fn unsupported_scheme_warns_and_falls_through() {
        let env = map(&[
            ("HTTPS_PROXY", "ftp://nope:1"),
            ("HTTP_PROXY", "http://plain:3"),
        ]);
        let found = from_map(&env).expect("env proxy");
        assert_eq!(found.url.as_str(), "http://plain:3/");
        assert_eq!(found.warnings.len(), 1);
        assert!(found.warnings[0].contains("ftp://nope:1"));
    }

    #[test]
    fn missing_scheme_defaults_to_http() {
        let env = map(&[("HTTPS_PROXY", "127.0.0.1:7890")]);
        let found = from_map(&env).expect("env proxy");
        assert_eq!(found.url.scheme(), "http");
        assert_eq!(found.url.port(), Some(7890));
    }

    #[test]
    fn socks_scheme_is_accepted() {
        assert!(parse_proxy_url("socks5://127.0.0.1:1080").is_some());
        assert!(parse_proxy_url("socks5h://127.0.0.1:1080").is_some());
    }

    #[test]
    fn hostless_url_is_rejected() {
        assert!(parse_proxy_url("http://").is_none());
    }

    #[test]
    fn no_proxy_upper_beats_lower_and_normalizes() {
        let env = map(&[
            ("HTTPS_PROXY", "http://secure:1"),
            ("NO_PROXY", ".Example.COM, 10.0.0.0/8 "),
            ("no_proxy", "ignored.example"),
        ]);
        let found = from_map(&env).expect("env proxy");
        assert_eq!(found.no_proxy, vec!["example.com", "10.0.0.0/8"]);
    }

    #[test]
    fn no_proxy_without_any_proxy_is_not_a_proxy() {
        let env = map(&[("NO_PROXY", "example.com")]);
        assert!(from_map(&env).is_none());
    }
}
