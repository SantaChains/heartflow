//! Proxy resolution and routing for every heartflow HTTP transport.
//!
//! # Why this crate exists
//!
//! `reqwest` reads `HTTP(S)_PROXY`/`ALL_PROXY`/`NO_PROXY` implicitly, but this
//! build has its `system-proxy` feature **off**, so on Windows the `WinINET`
//! settings that Clash/V2Ray "system proxy" writes are invisible, `socks5://`
//! proxy URLs are rejected, and a proxy variable that points at a dead port
//! costs a full connect timeout (15 s) per attempt before failing. Nothing
//! about the effective proxy is visible to `hf doctor`, and there is no way to
//! say "route google.com through the proxy but keep api.deepseek.com direct".
//!
//! This crate owns the whole decision so every transport (`api`, `tools`,
//! `mcp`) behaves identically:
//!
//! 1. `[proxy]` in `config.toml` — mode / address / rules / `no_proxy`.
//! 2. `HEARTFLOW_PROXY` / `HEARTFLOW_PROXY_MODE` / `HEARTFLOW_NO_PROXY`.
//! 3. `HTTPS_PROXY` → `ALL_PROXY` → `HTTP_PROXY` (upper and lower spellings),
//!    plus `NO_PROXY`.
//! 4. Windows system proxy (`WinINET` registry).
//! 5. Direct.
//!
//! Once a proxy is chosen it is *probed* (one TCP connect, 1 s budget): a dead
//! proxy degrades to direct with a warning instead of failing every request.
//! Routing is per-URL: loopback and LAN hosts are always direct, `no_proxy`
//! entries bypass, and Clash-style `rules` decide the rest.
//!
//! Mode semantics (the part users get wrong):
//! - `direct` — no proxy anywhere; beats environment and registry too. The
//!   escape hatch for "my proxy variable is stale".
//! - `global` — everything through the proxy (minus bypasses).
//! - `auto` — rules decide; unmatched URLs go **direct** when rules exist.
//!   With an address but *no* rules, `auto` behaves like `global` (nothing to
//!   match against, so proxying everything is the only sensible reading).
//!
//! The crate is deliberately reqwest-free: transports build their own
//! `reqwest::Proxy::custom` closure around [`ProxyResolution::proxy_url_for`].

mod env;
mod rules;
#[cfg(any(windows, test))]
mod system;

use std::net::{TcpStream, ToSocketAddrs};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use url::Url;

/// How long the startup liveness probe waits before calling a proxy dead.
pub const PROBE_TIMEOUT: Duration = Duration::from_secs(1);

/// Routing mode. `Auto` is the default when any proxy source is present.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ProxyMode {
    /// Rules decide; unmatched URLs go direct (or through the address when no
    /// rules exist — see the module docs).
    #[default]
    Auto,
    /// No proxy at all.
    Direct,
    /// Every URL through the proxy.
    Global,
}

impl ProxyMode {
    fn parse(text: &str) -> Option<Self> {
        match text.trim().to_ascii_lowercase().as_str() {
            "auto" => Some(Self::Auto),
            "direct" | "off" => Some(Self::Direct),
            "global" => Some(Self::Global),
            _ => None,
        }
    }
}

/// Which source supplied the effective proxy. Ordered strongest first.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProxySource {
    /// `[proxy]` in `config.toml` or the `HEARTFLOW_PROXY*` variables.
    Config,
    /// `HTTP(S)_PROXY` / `ALL_PROXY`.
    Env,
    /// Windows `WinINET` settings.
    System,
    /// Nothing found: direct.
    None,
}

/// Where a URL is routed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Route {
    Direct,
    Proxy,
}

/// The `[proxy]` table from `config.toml`. Raw strings only: this type takes
/// part in the layered settings merge, so it stays a plain comparable value.
/// Field-level semantics match `ProviderSettings` — a missing field never
/// erases an earlier layer's value.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProxyConfig {
    /// `direct` | `auto` | `global`.
    pub mode: Option<String>,
    /// Proxy URL, e.g. `http://127.0.0.1:7890` or `socks5://127.0.0.1:1080`.
    pub address: Option<String>,
    /// Clash-style rule lines: `TYPE,VALUE,ACTION`.
    pub rules: Option<Vec<String>>,
    /// Bypass entries (hosts, `.suffix`, IPs, CIDRs, `*`).
    pub no_proxy: Option<Vec<String>>,
}

impl ProxyConfig {
    /// True when the table carries no setting at all, so callers can treat it
    /// as absent instead of as an explicit (empty) override.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.mode.is_none()
            && self.address.is_none()
            && self.rules.is_none()
            && self.no_proxy.is_none()
    }

    /// Field-by-field extraction with the same fault isolation as the rest of
    /// the config layer: one bad entry never poisons the table.
    #[must_use]
    pub fn from_table(table: &toml::Table, source: &str) -> Self {
        let Some(proxy) = table.get("proxy").and_then(toml::Value::as_table) else {
            return Self::default();
        };
        for key in proxy.keys() {
            if !matches!(key.as_str(), "mode" | "address" | "rules" | "no_proxy") {
                tracing::debug!(file = %source, field = %key, "unknown [proxy] field; ignored");
            }
        }
        Self {
            mode: string_field(proxy, "mode", source),
            address: string_field(proxy, "address", source),
            rules: string_array_field(proxy, "rules", source),
            no_proxy: string_array_field(proxy, "no_proxy", source),
        }
    }

    /// Later layers win per field; `None` fields never erase earlier values.
    pub fn merge(&mut self, other: Self) {
        if other.mode.is_some() {
            self.mode = other.mode;
        }
        if other.address.is_some() {
            self.address = other.address;
        }
        if other.rules.is_some() {
            self.rules = other.rules;
        }
        if other.no_proxy.is_some() {
            self.no_proxy = other.no_proxy;
        }
    }
}

fn string_field(table: &toml::Table, key: &str, source: &str) -> Option<String> {
    match table.get(key) {
        None => None,
        Some(toml::Value::String(text)) => Some(text.clone()),
        Some(other) => {
            tracing::warn!(file = %source, field = %key, kind = ?other, "[proxy] field is not a string; skipped");
            None
        }
    }
}

fn string_array_field(table: &toml::Table, key: &str, source: &str) -> Option<Vec<String>> {
    match table.get(key) {
        None => None,
        Some(toml::Value::Array(items)) => {
            let mut values = Vec::with_capacity(items.len());
            for item in items {
                match item {
                    toml::Value::String(text) => values.push(text.clone()),
                    other => tracing::warn!(
                        file = %source,
                        field = %key,
                        kind = ?other,
                        "[proxy] array entry is not a string; skipped"
                    ),
                }
            }
            Some(values)
        }
        Some(other) => {
            tracing::warn!(file = %source, field = %key, kind = ?other, "[proxy] field is not an array of strings; skipped");
            None
        }
    }
}

/// A fully resolved proxy decision. Cheap to share behind an [`Arc`]; the
/// per-request routing path only reads it.
#[derive(Debug, Clone)]
pub struct ProxyResolution {
    source: ProxySource,
    mode: ProxyMode,
    address: Option<Url>,
    rules: Vec<rules::CompiledRule>,
    no_proxy: Vec<String>,
    dead: bool,
    probed: bool,
    warnings: Vec<String>,
}

impl ProxyResolution {
    /// The direct-everything resolution.
    fn direct(source: ProxySource) -> Self {
        Self {
            source,
            mode: ProxyMode::Direct,
            address: None,
            rules: Vec::new(),
            no_proxy: Vec::new(),
            dead: false,
            probed: false,
            warnings: Vec::new(),
        }
    }

    /// Run the full resolution chain: config → env → Windows system → direct.
    ///
    /// This performs no probing; [`init_global`] is the probing entry point.
    #[must_use]
    pub fn resolve(config: Option<&ProxyConfig>) -> Self {
        let mut warnings = Vec::new();
        let config = config.cloned().filter(|config| !config.is_empty());

        // An explicit `direct` beats every other source: it is the documented
        // escape hatch for a stale proxy variable or registry entry.
        if let Some(config) = &config {
            if config.mode.as_deref().is_some_and(|mode| {
                matches!(mode.trim().to_ascii_lowercase().as_str(), "direct" | "off")
            }) {
                return Self::direct(ProxySource::Config);
            }
        }

        // Address: config > env > system. A malformed explicit address warns
        // and falls through to the weaker sources rather than failing closed.
        let mut picked: Option<(Url, ProxySource, Vec<String>)> = None;
        if let Some(raw) = config.as_ref().and_then(|config| config.address.as_deref()) {
            match env::parse_proxy_url(raw) {
                Some(url) => picked = Some((url, ProxySource::Config, Vec::new())),
                None => warnings.push(format!(
                    "[proxy] address {raw:?} is not a supported proxy URL; ignored"
                )),
            }
        }
        if picked.is_none() {
            if let Some(env_proxy) = env::from_process() {
                warnings.extend(env_proxy.warnings);
                picked = Some((env_proxy.url, ProxySource::Env, env_proxy.no_proxy));
            }
        }
        if picked.is_none() {
            #[cfg(any(windows, test))]
            if let Some(system) = system::read() {
                warnings.extend(system.warnings);
                picked = Some((system.url, ProxySource::System, system.no_proxy));
            }
        }

        let Some((address, source, source_no_proxy)) = picked else {
            // No proxy anywhere. Keep config-supplied warnings visible for
            // doctor, but everything routes direct.
            let mut resolution = Self::direct(ProxySource::None);
            if config.is_some() {
                resolution.source = ProxySource::Config;
                resolution.warnings.extend(warnings);
            }
            return resolution;
        };

        // no_proxy: config entries first (they win ties), then the source's.
        let mut no_proxy = config
            .as_ref()
            .and_then(|config| config.no_proxy.clone())
            .unwrap_or_default();
        no_proxy.extend(source_no_proxy);
        no_proxy.dedup();

        // Rules exist only in config.
        let mut parsed_rules = Vec::new();
        if let Some(lines) = config.as_ref().and_then(|config| config.rules.as_deref()) {
            for line in lines {
                match rules::parse_rule(line) {
                    Ok(rule) => parsed_rules.push(rule),
                    Err(reason) => warnings.push(format!("[proxy] rule {line:?}: {reason}")),
                }
            }
        }

        // Mode: an explicit config mode wins; env/system sources are inherently
        // global; a config address with no rules proxies everything.
        let mode = config
            .as_ref()
            .and_then(|config| config.mode.as_deref())
            .and_then(|text| {
                ProxyMode::parse(text).or_else(|| {
                    warnings.push(format!(
                        "[proxy] mode {text:?} is not direct/auto/global; treated as auto"
                    ));
                    None
                })
            })
            .unwrap_or_else(|| {
                // Only an explicit config mode can mean "auto": env/system
                // sources are inherently global, and a config address with no
                // rules has nothing to match, so it is global too.
                if source == ProxySource::Config && !parsed_rules.is_empty() {
                    ProxyMode::Auto
                } else {
                    ProxyMode::Global
                }
            });

        Self {
            source,
            mode,
            address: Some(address),
            rules: parsed_rules,
            no_proxy,
            dead: false,
            probed: false,
            warnings,
        }
    }

    /// Where this URL goes under the resolution.
    #[must_use]
    pub fn route(&self, url: &Url) -> Route {
        if self.mode == ProxyMode::Direct || self.dead {
            return Route::Direct;
        }
        let Some(address) = self.address.as_ref() else {
            return Route::Direct;
        };
        // Loopback and LAN hosts are never proxied: a local relay or model
        // server must keep working when a system proxy is switched on.
        if is_bypassed_host(url) {
            return Route::Direct;
        }
        if self.no_proxy_matches(url) {
            return Route::Direct;
        }
        match self.mode {
            ProxyMode::Direct | ProxyMode::Global => Route::Proxy,
            ProxyMode::Auto => {
                for rule in &self.rules {
                    if rule.matches(url) {
                        return match rule.action {
                            rules::RuleAction::Proxy => Route::Proxy,
                            rules::RuleAction::Direct => Route::Direct,
                        };
                    }
                }
                // No rule matched: with rules present the user opted into
                // selective routing, so the fallback is direct.
                let _ = address;
                Route::Direct
            }
        }
    }

    /// The proxy URL to use for `url`, or `None` for direct. This is the hook
    /// transports hand to `reqwest::Proxy::custom`.
    #[must_use]
    pub fn proxy_url_for(&self, url: &Url) -> Option<Url> {
        match self.route(url) {
            Route::Proxy => self.address.clone(),
            Route::Direct => None,
        }
    }

    fn no_proxy_matches(&self, url: &Url) -> bool {
        let Some(host) = url.host() else {
            return false;
        };
        let host = rules::host_key(&host);
        let host_text = match &host {
            rules::HostKey::Domain(domain) => domain.clone(),
            rules::HostKey::V4(address) => address.to_string(),
            rules::HostKey::V6(address) => address.to_string(),
        };
        self.no_proxy.iter().any(|entry| {
            entry == "*"
                || host_text == *entry
                || host_text.ends_with(&format!(".{entry}"))
                || cidr_contains(entry, &host)
        })
    }

    /// Which source won.
    #[must_use]
    pub fn source(&self) -> ProxySource {
        self.source
    }

    /// The effective mode.
    #[must_use]
    pub fn mode(&self) -> ProxyMode {
        self.mode
    }

    /// The effective proxy URL, when one is in force.
    #[must_use]
    pub fn address(&self) -> Option<&Url> {
        self.address.as_ref()
    }

    /// True when the startup probe could not reach the proxy.
    #[must_use]
    pub fn is_dead(&self) -> bool {
        self.dead
    }

    /// True when a probe actually ran (only [`init_global`] probes).
    #[must_use]
    pub fn was_probed(&self) -> bool {
        self.probed
    }

    /// Malformed entries skipped during resolution, for `hf doctor`.
    #[must_use]
    pub fn warnings(&self) -> &[String] {
        &self.warnings
    }

    /// One-line summary for `hf doctor`.
    #[must_use]
    pub fn describe(&self) -> String {
        let Some(address) = self.address.as_ref() else {
            return "direct (no proxy configured)".to_string();
        };
        let source = match self.source {
            ProxySource::Config => "config",
            ProxySource::Env => "env",
            ProxySource::System => "system",
            ProxySource::None => "none",
        };
        let probe = if !self.probed {
            ", not probed"
        } else if self.dead {
            ", DEAD (probe failed, routing direct)"
        } else {
            ", probe ok"
        };
        format!(
            "{address} ({source}, mode={:?}, rules={}, no_proxy={}{probe})",
            self.mode,
            self.rules.len(),
            self.no_proxy.len()
        )
    }

    /// Mark the resolution dead after a failed probe. Crate-private: only the
    /// global init path probes.
    fn mark_dead(&mut self) {
        self.dead = true;
        self.probed = true;
    }

    fn mark_probed(&mut self) {
        self.probed = true;
    }
}

/// Hosts that are never proxied, regardless of configuration: loopback,
/// RFC1918 private space, link-local, and IPv6 unique-local.
fn is_bypassed_host(url: &Url) -> bool {
    let Some(host) = url.host() else {
        return false;
    };
    match rules::host_key(&host) {
        rules::HostKey::Domain(domain) => domain == "localhost" || domain.ends_with(".localhost"),
        rules::HostKey::V4(address) => {
            address.is_loopback()
                || address.is_private()
                || address.is_link_local()
                || address.is_unspecified()
                || address.is_broadcast()
        }
        rules::HostKey::V6(address) => {
            address.is_loopback()
                || address.is_unspecified()
                || address.is_unique_local()
                || address.is_unicast_link_local()
        }
    }
}

fn cidr_contains(entry: &str, host: &rules::HostKey) -> bool {
    let Ok(cidr) = rules::Cidr::parse(entry) else {
        return false;
    };
    cidr.contains(host)
}

/// One TCP connect against the proxy endpoint. DNS is resolved first, so a
/// hostname proxy costs one lookup; the connect itself is bounded by
/// [`PROBE_TIMEOUT`].
fn probe(address: &Url) -> bool {
    let Some(host) = address.host_str() else {
        return false;
    };
    let Some(port) = address.port_or_known_default() else {
        return false;
    };
    let addrs = match host.parse::<std::net::IpAddr>() {
        Ok(ip) => vec![std::net::SocketAddr::new(ip, port)],
        Err(_) => (host, port)
            .to_socket_addrs()
            .into_iter()
            .flatten()
            .collect(),
    };
    addrs
        .iter()
        .any(|addr| TcpStream::connect_timeout(addr, PROBE_TIMEOUT).is_ok())
}

// ---------------------------------------------------------------------------
// Process-global resolution
// ---------------------------------------------------------------------------

static GLOBAL: RwLock<Option<Arc<ProxyResolution>>> = RwLock::new(None);

/// Resolve, probe, and install the process-global resolution. Called once at
/// CLI startup and again on config reload. Probing is skipped when no proxy
/// address is in force.
pub fn init_global(config: Option<&ProxyConfig>) {
    let mut resolution = ProxyResolution::resolve(config);
    if let Some(address) = resolution.address.clone() {
        if probe(&address) {
            resolution.mark_probed();
        } else {
            resolution.mark_dead();
            tracing::warn!(
                proxy = %address,
                "proxy did not answer a {PROBE_TIMEOUT:?} TCP probe; requests will go direct",
            );
        }
    }
    // A poisoned lock only loses the cache: every later `resolution()` call
    // recomputes, so the failure mode is repeated work, never a wrong route.
    if let Ok(mut guard) = GLOBAL.write() {
        *guard = Some(Arc::new(resolution));
    }
}

/// The process-global resolution, initializing it lazily (config-less, no
/// probe) if [`init_global`] has not run yet.
#[must_use]
pub fn resolution() -> Arc<ProxyResolution> {
    if let Ok(guard) = GLOBAL.read() {
        if let Some(found) = guard.as_ref() {
            return Arc::clone(found);
        }
    }
    let fresh = Arc::new(ProxyResolution::resolve(None));
    if let Ok(mut guard) = GLOBAL.write() {
        *guard = Some(Arc::clone(&fresh));
    }
    fresh
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(pairs: &[(&str, toml::Value)]) -> ProxyConfig {
        let mut table = toml::Table::new();
        let mut proxy = toml::Table::new();
        for (key, value) in pairs {
            proxy.insert((*key).to_string(), value.clone());
        }
        table.insert("proxy".to_string(), toml::Value::Table(proxy));
        ProxyConfig::from_table(&table, "test")
    }

    fn url(text: &str) -> Url {
        Url::parse(text).expect("test url")
    }

    fn address_config(address: &str) -> ProxyConfig {
        config(&[("address", toml::Value::String(address.to_string()))])
    }

    #[test]
    fn config_table_parses_all_fields() {
        let parsed = config(&[
            ("mode", toml::Value::String("auto".into())),
            (
                "address",
                toml::Value::String("http://127.0.0.1:7890".into()),
            ),
            (
                "rules",
                toml::Value::Array(vec![toml::Value::String("MATCH,DIRECT".into())]),
            ),
            (
                "no_proxy",
                toml::Value::Array(vec![toml::Value::String("example.com".into())]),
            ),
        ]);
        assert_eq!(parsed.mode.as_deref(), Some("auto"));
        assert_eq!(parsed.address.as_deref(), Some("http://127.0.0.1:7890"));
        assert_eq!(parsed.rules, Some(vec!["MATCH,DIRECT".to_string()]));
        assert_eq!(parsed.no_proxy, Some(vec!["example.com".to_string()]));
    }

    #[test]
    fn config_table_skips_bad_entries() {
        let parsed = config(&[
            ("mode", toml::Value::Integer(3)),
            ("rules", toml::Value::String("nope".into())),
        ]);
        assert!(parsed.mode.is_none());
        assert!(parsed.rules.is_none());
    }

    #[test]
    fn config_merge_is_field_level() {
        let mut base = address_config("http://a:1");
        base.mode = Some("auto".into());
        let mut layer = address_config("http://b:2");
        layer.no_proxy = Some(vec!["example.com".into()]);
        base.merge(layer);
        assert_eq!(base.address.as_deref(), Some("http://b:2"));
        assert_eq!(base.mode.as_deref(), Some("auto"));
        assert_eq!(base.no_proxy, Some(vec!["example.com".to_string()]));
    }

    #[test]
    fn address_alone_proxies_everything() {
        let resolution = ProxyResolution::resolve(Some(&address_config("http://127.0.0.1:7890")));
        assert_eq!(resolution.source(), ProxySource::Config);
        assert_eq!(resolution.mode(), ProxyMode::Global);
        assert_eq!(
            resolution.route(&url("https://api.deepseek.com/v1")),
            Route::Proxy
        );
        // Loopback is never proxied even in global mode.
        assert_eq!(
            resolution.route(&url("http://127.0.0.1:8000/v1")),
            Route::Direct
        );
        assert_eq!(
            resolution.route(&url("http://localhost:1234/")),
            Route::Direct
        );
    }

    #[test]
    fn direct_mode_beats_everything() {
        let mut cfg = address_config("http://127.0.0.1:7890");
        cfg.mode = Some("direct".into());
        let resolution = ProxyResolution::resolve(Some(&cfg));
        assert_eq!(resolution.mode(), ProxyMode::Direct);
        assert!(resolution.address().is_none());
        assert_eq!(
            resolution.route(&url("https://example.com/")),
            Route::Direct
        );
    }

    #[test]
    fn auto_mode_with_rules_falls_back_to_direct() {
        let cfg = config(&[
            ("mode", toml::Value::String("auto".into())),
            (
                "address",
                toml::Value::String("http://127.0.0.1:7890".into()),
            ),
            (
                "rules",
                toml::Value::Array(vec![
                    toml::Value::String("DOMAIN-SUFFIX,google.com,PROXY".into()),
                    toml::Value::String("DOMAIN-SUFFIX,deepseek.com,DIRECT".into()),
                ]),
            ),
        ]);
        let resolution = ProxyResolution::resolve(Some(&cfg));
        assert_eq!(resolution.mode(), ProxyMode::Auto);
        assert_eq!(
            resolution.route(&url("https://www.google.com/")),
            Route::Proxy
        );
        assert_eq!(
            resolution.route(&url("https://api.deepseek.com/v1")),
            Route::Direct
        );
        // Unmatched: selective routing means direct.
        assert_eq!(
            resolution.route(&url("https://example.com/")),
            Route::Direct
        );
    }

    #[test]
    fn match_rule_can_catch_everything() {
        let cfg = config(&[
            (
                "address",
                toml::Value::String("http://127.0.0.1:7890".into()),
            ),
            (
                "rules",
                toml::Value::Array(vec![
                    toml::Value::String("DOMAIN-SUFFIX,deepseek.com,DIRECT".into()),
                    toml::Value::String("MATCH,PROXY".into()),
                ]),
            ),
        ]);
        let resolution = ProxyResolution::resolve(Some(&cfg));
        assert_eq!(
            resolution.route(&url("https://api.deepseek.com/")),
            Route::Direct
        );
        assert_eq!(resolution.route(&url("https://example.com/")), Route::Proxy);
    }

    #[test]
    fn bad_rules_warn_but_the_rest_survive() {
        let cfg = config(&[
            (
                "address",
                toml::Value::String("http://127.0.0.1:7890".into()),
            ),
            (
                "rules",
                toml::Value::Array(vec![
                    toml::Value::String("DOMAIN-SUFFIX,google.com,PROXY".into()),
                    toml::Value::String("NONSENSE,line,here".into()),
                ]),
            ),
        ]);
        let resolution = ProxyResolution::resolve(Some(&cfg));
        assert_eq!(resolution.warnings().len(), 1);
        assert!(resolution.warnings()[0].contains("NONSENSE"));
        assert_eq!(
            resolution.route(&url("https://www.google.com/")),
            Route::Proxy
        );
    }

    #[test]
    fn no_proxy_entries_bypass() {
        let cfg = config(&[
            (
                "address",
                toml::Value::String("http://127.0.0.1:7890".into()),
            ),
            (
                "no_proxy",
                toml::Value::Array(vec![
                    toml::Value::String("example.com".into()),
                    toml::Value::String("10.0.0.0/8".into()),
                    toml::Value::String("192.168.1.5".into()),
                ]),
            ),
        ]);
        let resolution = ProxyResolution::resolve(Some(&cfg));
        assert_eq!(
            resolution.route(&url("https://example.com/")),
            Route::Direct
        );
        assert_eq!(
            resolution.route(&url("https://sub.example.com/")),
            Route::Direct
        );
        assert_eq!(resolution.route(&url("https://10.1.2.3/")), Route::Direct);
        assert_eq!(
            resolution.route(&url("https://192.168.1.5/")),
            Route::Direct
        );
        assert_eq!(resolution.route(&url("https://other.com/")), Route::Proxy);
    }

    #[test]
    fn lan_and_loopback_hosts_always_bypass() {
        let resolution = ProxyResolution::resolve(Some(&address_config("http://127.0.0.1:7890")));
        for host in [
            "http://127.0.0.1/",
            "http://192.168.0.10/",
            "http://10.0.0.1/",
            "http://[fd00::1]/",
        ] {
            assert_eq!(resolution.route(&url(host)), Route::Direct, "{host}");
        }
    }

    #[test]
    fn empty_config_falls_through_to_env() {
        // With a clean environment the chain ends at direct; the point is
        // that an empty table is not an explicit override.
        let resolution = ProxyResolution::resolve(Some(&ProxyConfig::default()));
        assert!(resolution.address().is_none() || resolution.source() != ProxySource::Config);
    }

    #[test]
    fn dead_resolution_routes_everything_direct() {
        let mut resolution =
            ProxyResolution::resolve(Some(&address_config("http://127.0.0.1:7890")));
        resolution.mark_dead();
        assert!(resolution.is_dead());
        assert_eq!(
            resolution.route(&url("https://example.com/")),
            Route::Direct
        );
        assert_eq!(resolution.proxy_url_for(&url("https://example.com/")), None);
    }

    #[test]
    fn proxy_url_for_matches_route() {
        let resolution = ProxyResolution::resolve(Some(&address_config("http://127.0.0.1:7890")));
        assert_eq!(
            resolution.proxy_url_for(&url("https://example.com/")),
            Some(url("http://127.0.0.1:7890"))
        );
        assert_eq!(
            resolution.proxy_url_for(&url("http://127.0.0.1:8000/")),
            None
        );
    }

    #[test]
    fn describe_is_informative() {
        let resolution = ProxyResolution::resolve(Some(&address_config("http://127.0.0.1:7890")));
        let text = resolution.describe();
        assert!(text.contains("127.0.0.1:7890"), "{text}");
        assert!(text.contains("config"), "{text}");
        // Hermetic against this machine's registry: an explicit direct mode
        // must describe itself as direct.
        let mut direct_cfg = address_config("http://127.0.0.1:7890");
        direct_cfg.mode = Some("direct".into());
        let direct = ProxyResolution::resolve(Some(&direct_cfg));
        assert!(direct.describe().contains("direct"));
    }
}
