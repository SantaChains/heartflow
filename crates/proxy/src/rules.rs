//! Clash-style routing rules, shared by the resolver and unit-tested here.
//!
//! A rule line is `TYPE,VALUE,ACTION` (whitespace around commas is tolerated,
//! `ACTION` is `PROXY` or `DIRECT`, case-insensitive). The first matching rule
//! wins; `MATCH` is the catch-all. Anything unparseable is reported as an
//! error string so the resolver can surface it as a warning instead of
//! silently dropping user configuration.

use regex::Regex;
use std::net::{Ipv4Addr, Ipv6Addr};
use url::{Host, Url};

/// A host extracted from a URL, owned. `Url::host` borrows (`Host<&str>`),
/// while both the rule engine and the resolver want owned keys.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum HostKey {
    Domain(String),
    V4(Ipv4Addr),
    V6(Ipv6Addr),
}

/// Normalize a borrowed URL host into a [`HostKey`]. Domain hosts are
/// lowercased (the URL spec already guarantees this, but be explicit).
pub(super) fn host_key<S: AsRef<str>>(host: &Host<S>) -> HostKey {
    match host {
        Host::Domain(domain) => HostKey::Domain(domain.as_ref().to_ascii_lowercase()),
        Host::Ipv4(address) => HostKey::V4(*address),
        Host::Ipv6(address) => HostKey::V6(*address),
    }
}

/// What a rule decides for the URLs it matches.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum RuleAction {
    Proxy,
    Direct,
}

/// A compiled matcher. `Regex` lives behind a variant so the rule table stays
/// `Clone + Send + Sync` without a trait object.
#[derive(Debug, Clone)]
pub(super) enum RuleMatcher {
    /// Exact host equality.
    Domain(String),
    /// Host equals the value or is a subdomain of it.
    DomainSuffix(String),
    /// Host contains the value.
    DomainKeyword(String),
    /// Host is an IP literal inside the network. Domain hosts never match:
    /// the resolver deliberately does not resolve DNS before routing.
    IpCidr(Cidr),
    /// Matches the full URL string.
    Regex(Regex),
    /// Always matches.
    Match,
}

/// One parsed rule: matcher plus the action to take when it hits.
#[derive(Debug, Clone)]
pub(super) struct CompiledRule {
    pub(super) matcher: RuleMatcher,
    pub(super) action: RuleAction,
}

impl CompiledRule {
    pub(super) fn matches(&self, url: &Url) -> bool {
        let Some(host) = url.host() else {
            return false;
        };
        let host = host_key(&host);
        match &self.matcher {
            RuleMatcher::Domain(value) => {
                matches!(&host, HostKey::Domain(domain) if domain == value)
            }
            RuleMatcher::DomainSuffix(value) => match &host {
                HostKey::Domain(domain) => {
                    domain == value || domain.ends_with(&format!(".{value}"))
                }
                _ => false,
            },
            RuleMatcher::DomainKeyword(value) => match &host {
                HostKey::Domain(domain) => domain.contains(value.as_str()),
                _ => false,
            },
            RuleMatcher::IpCidr(net) => net.contains(&host),
            RuleMatcher::Regex(pattern) => pattern.is_match(url.as_str()),
            RuleMatcher::Match => true,
        }
    }
}

/// Parse one `TYPE,VALUE,ACTION` line. `MATCH` is the one valueless type:
/// `MATCH,ACTION`. `Err` carries a human-readable reason.
pub(super) fn parse_rule(line: &str) -> Result<CompiledRule, String> {
    let parts: Vec<&str> = line.split(',').map(str::trim).collect();
    let kind = parts
        .first()
        .map(|part| part.to_ascii_uppercase())
        .unwrap_or_default();
    if kind == "MATCH" {
        let action = match parts
            .get(1)
            .map(|part| part.to_ascii_uppercase())
            .as_deref()
        {
            Some("PROXY") => RuleAction::Proxy,
            Some("DIRECT") => RuleAction::Direct,
            _ => return Err(format!("MATCH wants PROXY or DIRECT, got {line:?}")),
        };
        return Ok(CompiledRule {
            matcher: RuleMatcher::Match,
            action,
        });
    }
    if parts.len() < 3 {
        return Err(format!("expected TYPE,VALUE,ACTION, got {line:?}"));
    }
    // A value may itself contain commas (only REGEX realistically does), so
    // everything between the first and last field is the value.
    let action = match parts[parts.len() - 1].to_ascii_uppercase().as_str() {
        "PROXY" => RuleAction::Proxy,
        "DIRECT" => RuleAction::Direct,
        other => return Err(format!("unknown action {other:?} (want PROXY or DIRECT)")),
    };
    let value = parts[1..parts.len() - 1].join(",").to_ascii_lowercase();
    if value.is_empty() {
        return Err(format!("empty value in rule {line:?}"));
    }
    let matcher = match kind.as_str() {
        "DOMAIN" => RuleMatcher::Domain(value),
        "DOMAIN-SUFFIX" => RuleMatcher::DomainSuffix(value),
        "DOMAIN-KEYWORD" => RuleMatcher::DomainKeyword(value),
        "IP-CIDR" => RuleMatcher::IpCidr(
            Cidr::parse(&value).map_err(|error| format!("bad IP-CIDR {value:?}: {error}"))?,
        ),
        "REGEX" => RuleMatcher::Regex(
            Regex::new(&value).map_err(|error| format!("bad REGEX {value:?}: {error}"))?,
        ),
        "MATCH" => RuleMatcher::Match,
        other => return Err(format!("unknown rule type {other:?}")),
    };
    Ok(CompiledRule { matcher, action })
}

/// An IPv4 or IPv6 network in integer form. Written by hand because the only
/// operation needed is `contains` on a literal host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Cidr {
    V4 { network: u32, prefix: u32 },
    V6 { network: u128, prefix: u32 },
}

impl Cidr {
    pub(super) fn parse(text: &str) -> Result<Self, String> {
        let Some((addr, prefix)) = text.split_once('/') else {
            return Err(format!("missing /prefix in {text:?}"));
        };
        let prefix: u32 = prefix
            .parse()
            .map_err(|error| format!("bad prefix {prefix:?}: {error}"))?;
        match addr.parse::<std::net::IpAddr>() {
            Ok(std::net::IpAddr::V4(address)) => {
                if prefix > 32 {
                    return Err(format!("prefix {prefix} exceeds 32 for IPv4"));
                }
                let bits = u32::from_be_bytes(address.octets());
                let mask = mask_v4(prefix);
                Ok(Self::V4 {
                    network: bits & mask,
                    prefix,
                })
            }
            Ok(std::net::IpAddr::V6(address)) => {
                if prefix > 128 {
                    return Err(format!("prefix {prefix} exceeds 128 for IPv6"));
                }
                let bits = u128::from_be_bytes(address.octets());
                let mask = mask_v6(prefix);
                Ok(Self::V6 {
                    network: bits & mask,
                    prefix,
                })
            }
            Err(error) => Err(format!("bad address {addr:?}: {error}")),
        }
    }

    pub(super) fn contains(&self, host: &HostKey) -> bool {
        match (self, host) {
            (Self::V4 { network, prefix }, HostKey::V4(address)) => {
                u32::from_be_bytes(address.octets()) & mask_v4(*prefix) == *network
            }
            (Self::V6 { network, prefix }, HostKey::V6(address)) => {
                u128::from_be_bytes(address.octets()) & mask_v6(*prefix) == *network
            }
            // Domain hosts are never resolved, so they never match a CIDR.
            _ => false,
        }
    }
}

fn mask_v4(prefix: u32) -> u32 {
    if prefix == 0 {
        0
    } else {
        u32::MAX << (32 - prefix)
    }
}

fn mask_v6(prefix: u32) -> u128 {
    if prefix == 0 {
        0
    } else {
        u128::MAX << (128 - prefix)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn url(text: &str) -> Url {
        Url::parse(text).expect("test url")
    }

    fn host_of(text: &str) -> HostKey {
        host_key(&url(text).host().expect("test host"))
    }

    fn rule(line: &str) -> CompiledRule {
        parse_rule(line).expect("rule parses")
    }

    #[test]
    fn domain_matches_exact_host_only() {
        let rule = rule("DOMAIN,api.example.com,PROXY");
        assert!(rule.matches(&url("https://api.example.com/v1")));
        assert!(!rule.matches(&url("https://www.example.com/v1")));
    }

    #[test]
    fn domain_suffix_covers_subdomains() {
        let rule = rule("DOMAIN-SUFFIX,google.com,DIRECT");
        assert!(rule.matches(&url("https://google.com/")));
        assert!(rule.matches(&url("https://www.google.com/search")));
        assert!(!rule.matches(&url("https://notgoogle.com/")));
    }

    #[test]
    fn domain_keyword_is_substring_match() {
        let rule = rule("DOMAIN-KEYWORD,google,PROXY");
        assert!(rule.matches(&url("https://www.google.com/")));
        assert!(!rule.matches(&url("https://example.com/")));
    }

    #[test]
    fn ip_cidr_matches_literal_hosts_only() {
        let rule = rule("IP-CIDR,10.0.0.0/8,DIRECT");
        assert!(rule.matches(&url("http://10.1.2.3/")));
        assert!(!rule.matches(&url("http://11.1.2.3/")));
        // A domain host is never resolved before routing.
        assert!(!rule.matches(&url("http://internal.example.com/")));
    }

    #[test]
    fn regex_matches_full_url() {
        let rule = rule(r"REGEX,^https://github\.com/,PROXY");
        assert!(rule.matches(&url("https://github.com/antirez/ds4")));
        assert!(!rule.matches(&url("https://gist.github.com/")));
    }

    #[test]
    fn match_is_catch_all() {
        let rule = rule("MATCH,DIRECT");
        assert!(rule.matches(&url("https://anything.example/")));
    }

    #[test]
    fn first_match_wins_over_later_rules() {
        let rules = [rule("DOMAIN-SUFFIX,google.com,DIRECT"), rule("MATCH,PROXY")];
        let target = url("https://google.com/");
        let action = rules
            .iter()
            .find(|rule| rule.matches(&target))
            .map(|rule| rule.action);
        assert_eq!(action, Some(RuleAction::Direct));
    }

    #[test]
    fn malformed_lines_report_reasons() {
        assert!(parse_rule("DOMAIN,example.com").is_err());
        assert!(parse_rule("DOMAIN,example.com,MAYBE").is_err());
        assert!(parse_rule("GEOIP,CN,DIRECT").is_err());
        assert!(parse_rule("IP-CIDR,10.0.0.0/33,DIRECT").is_err());
        assert!(parse_rule("IP-CIDR,notanip/8,DIRECT").is_err());
        assert!(parse_rule("REGEX,[unclosed,PROXY").is_err());
        assert!(parse_rule("MATCH,,PROXY").is_err());
    }

    #[test]
    fn whitespace_and_case_are_tolerated() {
        let rule = rule(" domain-suffix , Example.COM , proxy ");
        assert!(rule.matches(&url("https://www.example.com/")));
        assert_eq!(rule.action, RuleAction::Proxy);
    }

    #[test]
    fn cidr_boundaries() {
        let cidr = Cidr::parse("192.168.1.0/24").expect("cidr");
        assert!(cidr.contains(&host_of("http://192.168.1.255/")));
        assert!(!cidr.contains(&host_of("http://192.168.2.1/")));
        let v6 = Cidr::parse("fd00::/8").expect("cidr v6");
        assert!(v6.contains(&host_of("http://[fd12::1]/")));
        assert!(!v6.contains(&host_of("http://[fe80::1]/")));
    }
}
