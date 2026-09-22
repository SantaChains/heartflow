//! The provider/model catalog: a registry of the *known universe* of providers
//! and their models, deliberately distinct from `config.toml` (which selects the
//! one *active* transport via `resolve_spec`/`materialize`). This file never
//! drives which endpoint a turn hits; it only feeds the REPL `/model` status
//! line and completion dropdown, and gives `hf --provider NAME` a discoverable
//! shortlist.
//!
//! Three layers, merged in order:
//! 1. a compiled-in seed of common domestic OpenAI-compatible providers
//!    ([`default_catalog`]), shipped with the binary so completion is useful on
//!    a fresh install and improves as heartflow is upgraded;
//! 2. a user-editable `~/.heartflow/provider.toml` overlay;
//! 3. models heartflow records itself when `hf models` discovers an endpoint's
//!    list or `/model NAME` switches to a model ([`persist_model`]).
//!
//! Only layers 2 and 3 are ever written back to disk; the seed stays compiled in
//! so a version bump can refresh it without migrating a file. Parsing is fully
//! fault-isolated (a bad field is skipped with a warning, never fatal), matching
//! `config.rs`.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tracing::warn;

use crate::config::{quote_toml_string, string_field, ProviderProtocol};

/// Where a catalog model entry came from. Drives merge precedence (a user or
/// discovered entry overrides a seed entry with the same id) and keeps the seed
/// out of the persisted file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelSource {
    /// Compiled into the binary via [`default_catalog`]; never persisted.
    Seed,
    /// Hand-added by the user in `provider.toml`, or recorded via `/model`.
    User,
    /// Recorded from a live `GET {base}/models` self-discovery (`hf models`).
    Discovered,
}

impl ModelSource {
    fn parse(text: &str) -> Option<Self> {
        match text.trim().to_ascii_lowercase().as_str() {
            "seed" => Some(Self::Seed),
            "user" => Some(Self::User),
            "discovered" => Some(Self::Discovered),
            _ => None,
        }
    }

    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Seed => "seed",
            Self::User => "user",
            Self::Discovered => "discovered",
        }
    }
}

/// One model in the catalog.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatalogModel {
    /// The provider's model id, exactly as sent in a request.
    pub id: String,
    /// Context window in tokens when confidently known; `None` rather than a
    /// guess (the plan's "不确定的留空而非臆造").
    pub context_window: Option<u64>,
    /// Free-form tag (e.g. "reasoning", "vision", "free tier").
    pub note: Option<String>,
    pub source: ModelSource,
    /// Unix seconds of the last record/discovery, for freshness hints.
    pub last_seen: Option<u64>,
}

/// One provider entry: connection scaffolding plus its known models. The
/// scaffolding is reference-only (a hint for `api_key_env`, a match target for
/// host lookup); the active transport still comes from `config.toml`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatalogProvider {
    pub protocol: Option<ProviderProtocol>,
    pub base_url: Option<String>,
    pub api_key_env: Option<String>,
    pub models: Vec<CatalogModel>,
}

impl CatalogProvider {
    fn blank() -> Self {
        Self {
            protocol: None,
            base_url: None,
            api_key_env: None,
            models: Vec::new(),
        }
    }
}

/// The whole catalog: providers keyed by a stable slug (`deepseek`, `moonshot`).
/// A `BTreeMap` keeps render and completion order deterministic.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProviderCatalog {
    pub providers: BTreeMap<String, CatalogProvider>,
}

impl ProviderCatalog {
    /// The provider whose `base_url` shares a host with `base_url`, so a profile
    /// resolved from `config.toml` maps onto its catalog entry regardless of the
    /// `/v1` suffix or a trailing slash.
    #[must_use]
    pub fn provider_by_host(&self, base_url: &str) -> Option<&CatalogProvider> {
        let host = host_of(base_url);
        if host.is_empty() {
            return None;
        }
        self.providers.values().find(|provider| {
            provider
                .base_url
                .as_deref()
                .is_some_and(|url| host_of(url) == host)
        })
    }

    /// The catalog key for `base_url`, matching [`provider_by_host`](Self::provider_by_host).
    #[must_use]
    pub fn provider_key_by_host(&self, base_url: &str) -> Option<String> {
        let host = host_of(base_url);
        if host.is_empty() {
            return None;
        }
        self.providers.iter().find_map(|(key, provider)| {
            provider
                .base_url
                .as_deref()
                .filter(|url| host_of(url) == host)
                .map(|_| key.clone())
        })
    }

    /// Merge `other` over `self` per provider key: scaffolding fields are taken
    /// only when present, and models are unioned by id with `other` winning. This
    /// is how the user/discovered overlay layers onto the seed.
    pub fn merge(&mut self, other: ProviderCatalog) {
        for (key, provider) in other.providers {
            let entry = self
                .providers
                .entry(key)
                .or_insert_with(CatalogProvider::blank);
            if provider.protocol.is_some() {
                entry.protocol = provider.protocol;
            }
            if provider.base_url.is_some() {
                entry.base_url = provider.base_url;
            }
            if provider.api_key_env.is_some() {
                entry.api_key_env = provider.api_key_env;
            }
            for model in provider.models {
                match entry.models.iter_mut().find(|slot| slot.id == model.id) {
                    Some(slot) => *slot = model,
                    None => entry.models.push(model),
                }
            }
        }
    }

    /// Upsert one model under `key`, creating the provider entry (with its
    /// scaffolding) if absent. Used by [`persist_model`] and the tests.
    pub fn record_model(
        &mut self,
        key: &str,
        base_url: &str,
        protocol: ProviderProtocol,
        model_id: &str,
        context_window: Option<u64>,
        source: ModelSource,
    ) {
        let entry = self.providers.entry(key.to_string()).or_insert_with(|| {
            let mut provider = CatalogProvider::blank();
            provider.protocol = Some(protocol);
            provider.base_url = Some(base_url.to_string());
            provider
        });
        if entry.protocol.is_none() {
            entry.protocol = Some(protocol);
        }
        if entry.base_url.is_none() {
            entry.base_url = Some(base_url.to_string());
        }
        let now = unix_now();
        match entry.models.iter_mut().find(|model| model.id == model_id) {
            Some(model) => {
                if context_window.is_some() {
                    model.context_window = context_window;
                }
                model.source = source;
                model.last_seen = now.or(model.last_seen);
            }
            None => entry.models.push(CatalogModel {
                id: model_id.to_string(),
                context_window,
                note: None,
                source,
                last_seen: now,
            }),
        }
    }

    /// Parse a `provider.toml` table with full fault isolation: unknown shapes
    /// are skipped with a warning rather than failing the whole file.
    #[must_use]
    pub fn from_table(table: &toml::Table, source: &str) -> Self {
        let mut catalog = Self::default();
        let Some(providers) = table.get("providers").and_then(toml::Value::as_table) else {
            return catalog;
        };
        for (name, value) in providers {
            let Some(provider_table) = value.as_table() else {
                warn!(file = %source, provider = %name, "catalog provider must be a table; skipped");
                continue;
            };
            let protocol = string_field(provider_table, "protocol", source)
                .and_then(|text| {
                    let parsed = ProviderProtocol::parse(&text);
                    if parsed.is_none() {
                        warn!(file = %source, provider = %name, value = %text, "unknown catalog protocol; skipped");
                    }
                    parsed
                });
            let mut models = Vec::new();
            if let Some(entries) = provider_table.get("models").and_then(toml::Value::as_array) {
                for entry in entries {
                    let Some(model_table) = entry.as_table() else {
                        warn!(file = %source, provider = %name, "catalog model must be a table; skipped");
                        continue;
                    };
                    let Some(id) = string_field(model_table, "id", source) else {
                        warn!(file = %source, provider = %name, "catalog model missing id; skipped");
                        continue;
                    };
                    models.push(CatalogModel {
                        id,
                        context_window: u64_field(model_table, "context_window", source),
                        note: string_field(model_table, "note", source),
                        source: string_field(model_table, "source", source)
                            .and_then(|text| ModelSource::parse(&text))
                            .unwrap_or(ModelSource::User),
                        last_seen: u64_field(model_table, "last_seen", source),
                    });
                }
            }
            catalog.providers.insert(
                name.clone(),
                CatalogProvider {
                    protocol,
                    base_url: string_field(provider_table, "base_url", source),
                    api_key_env: string_field(provider_table, "api_key_env", source),
                    models,
                },
            );
        }
        catalog
    }

    /// Render as canonical `provider.toml` text. Called only on the persisted
    /// (user/discovered) layer, so the seed is never written back.
    #[must_use]
    pub fn to_toml_string(&self) -> String {
        use std::fmt::Write as _;

        let mut out = String::from(concat!(
            "# heartflow provider/model catalog: a registry of known providers,\n",
            "# NOT the active config (that is config.toml). Seed providers are\n",
            "# compiled in; this file holds your additions plus models recorded by\n",
            "# `hf models` and `/model`. Edit freely; heartflow hot-reloads it.\n",
        ));
        for (key, provider) in &self.providers {
            let rendered_key = bare_or_quoted(key);
            let _ = writeln!(out, "\n[providers.{rendered_key}]");
            if let Some(protocol) = provider.protocol {
                let _ = writeln!(out, "protocol = \"{}\"", protocol.as_str());
            }
            if let Some(base_url) = &provider.base_url {
                let _ = writeln!(out, "base_url = {}", quote_toml_string(base_url));
            }
            if let Some(api_key_env) = &provider.api_key_env {
                let _ = writeln!(out, "api_key_env = {}", quote_toml_string(api_key_env));
            }
            for model in &provider.models {
                let _ = writeln!(out, "\n[[providers.{rendered_key}.models]]");
                let _ = writeln!(out, "id = {}", quote_toml_string(&model.id));
                if let Some(context_window) = model.context_window {
                    let _ = writeln!(out, "context_window = {context_window}");
                }
                if let Some(note) = &model.note {
                    let _ = writeln!(out, "note = {}", quote_toml_string(note));
                }
                let _ = writeln!(out, "source = \"{}\"", model.source.as_str());
                if let Some(last_seen) = model.last_seen {
                    let _ = writeln!(out, "last_seen = {last_seen}");
                }
            }
        }
        out
    }
}

/// Build one seed provider entry. Each model is `(id, context_window, note)`.
fn seed_provider(
    base_url: &str,
    api_key_env: &str,
    models: &[(&str, Option<u64>, Option<&str>)],
) -> CatalogProvider {
    CatalogProvider {
        // Every seeded domestic provider here speaks the OpenAI chat dialect.
        protocol: Some(ProviderProtocol::OpenAi),
        base_url: Some(base_url.to_string()),
        api_key_env: Some(api_key_env.to_string()),
        models: models
            .iter()
            .map(|(id, context_window, note)| CatalogModel {
                id: (*id).to_string(),
                context_window: *context_window,
                note: (*note).map(str::to_string),
                source: ModelSource::Seed,
                last_seen: None,
            })
            .collect(),
    }
}

/// The compiled-in seed: common domestic (plus OpenAI) OpenAI-compatible
/// providers, refreshed against each vendor's official model list as of
/// 2026-09. Endpoints and `api_key_env` names are stable; model ids are the
/// current, officially-documented ones (retired ids are dropped, never kept as
/// stale aliases). Context windows are filled only where a vendor states an
/// exact token count and left `None` otherwise rather than guessed from a
/// marketing "1M"/"200K" figure (the plan's "要准不是全"). Providers whose ids
/// churn fast or whose exact ids we cannot confirm from a primary source
/// (aggregators, Doubao, StepFun, MiniMax, Yi, Baichuan, SiliconFlow) are seeded
/// as scaffolding with an empty model list, to be filled accurately by
/// `hf models` discovery rather than risk a wrong id.
#[must_use]
pub fn default_catalog() -> ProviderCatalog {
    let entries: &[(&str, CatalogProvider)] = &[
        (
            "deepseek",
            seed_provider(
                "https://api.deepseek.com/v1",
                "DEEPSEEK_API_KEY",
                &[
                    ("deepseek-v4.1-flash", None, Some("newest")),
                    ("deepseek-v4-pro", None, Some("flagship")),
                    ("deepseek-v4-flash", None, None),
                    ("deepseek-v3.2", None, None),
                ],
            ),
        ),
        (
            "moonshot",
            seed_provider(
                "https://api.moonshot.cn/v1",
                "MOONSHOT_API_KEY",
                &[
                    ("kimi-k3", Some(1_048_576), Some("flagship")),
                    ("kimi-k2.7-code", Some(262_144), Some("coding")),
                    ("kimi-k2.7-code-highspeed", Some(262_144), None),
                    ("kimi-k2.6", Some(262_144), None),
                ],
            ),
        ),
        (
            "zhipu",
            seed_provider(
                "https://open.bigmodel.cn/api/paas/v4",
                "ZHIPU_API_KEY",
                &[
                    ("glm-5.2", None, Some("flagship")),
                    ("glm-5.1", None, None),
                    ("glm-4.7", None, None),
                    ("glm-4.6", None, None),
                    ("glm-4.5", Some(131_072), None),
                    ("glm-4.5-air", Some(131_072), None),
                    ("glm-4.7-flash", None, Some("free")),
                ],
            ),
        ),
        (
            "qwen",
            seed_provider(
                "https://dashscope.aliyuncs.com/compatible-mode/v1",
                "DASHSCOPE_API_KEY",
                &[
                    ("qwen3.8-max", None, Some("flagship")),
                    ("qwen3-max", Some(262_144), None),
                    ("qwen-max", None, None),
                    ("qwen-plus", None, None),
                    ("qwen-flash", None, None),
                ],
            ),
        ),
        (
            "qianfan",
            seed_provider(
                "https://qianfan.baidubce.com/v2",
                "QIANFAN_API_KEY",
                &[
                    ("ernie-5.0", Some(119_000), Some("flagship")),
                    ("ernie-4.5-turbo-128k", Some(131_072), None),
                    ("ernie-x1.1-preview", Some(65_536), Some("reasoning")),
                ],
            ),
        ),
        (
            "minimax",
            seed_provider("https://api.minimax.chat/v1", "MINIMAX_API_KEY", &[]),
        ),
        (
            "baichuan",
            seed_provider("https://api.baichuan-ai.com/v1", "BAICHUAN_API_KEY", &[]),
        ),
        (
            "siliconflow",
            seed_provider("https://api.siliconflow.cn/v1", "SILICONFLOW_API_KEY", &[]),
        ),
        (
            "doubao",
            seed_provider(
                "https://ark.cn-beijing.volces.com/api/v3",
                "ARK_API_KEY",
                &[],
            ),
        ),
        (
            "yi",
            seed_provider("https://api.lingyiwanwu.com/v1", "YI_API_KEY", &[]),
        ),
        (
            "stepfun",
            seed_provider("https://api.stepfun.com/v1", "STEPFUN_API_KEY", &[]),
        ),
        (
            "openai",
            seed_provider(
                "https://api.openai.com/v1",
                "OPENAI_API_KEY",
                &[("gpt-4o", None, None), ("gpt-4o-mini", None, None)],
            ),
        ),
    ];
    ProviderCatalog {
        providers: entries
            .iter()
            .map(|(key, provider)| ((*key).to_string(), provider.clone()))
            .collect(),
    }
}

/// Layered catalog file paths in merge order. Only the user layer exists today
/// (it is also the write target); a project layer could be appended later.
#[must_use]
pub fn catalog_file_paths(home: &Path) -> Vec<PathBuf> {
    vec![catalog_write_path(home)]
}

/// The single writable catalog file: `~/.heartflow/provider.toml`.
#[must_use]
pub fn catalog_write_path(home: &Path) -> PathBuf {
    home.join(".heartflow").join("provider.toml")
}

/// Load the merged catalog: the compiled-in seed overlaid by every on-disk
/// layer. Missing or broken files degrade to the seed rather than failing.
#[must_use]
pub fn load_catalog(home: &Path) -> ProviderCatalog {
    let mut catalog = default_catalog();
    for path in catalog_file_paths(home) {
        catalog.merge(read_catalog_file(&path));
    }
    catalog
}

/// Read one catalog file with full fault isolation (missing -> empty, unreadable
/// or unparseable -> empty with a warning), mirroring `read_provider_file`.
fn read_catalog_file(path: &Path) -> ProviderCatalog {
    let contents = match fs::read_to_string(path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return ProviderCatalog::default();
        }
        Err(error) => {
            warn!(file = %path.display(), error = %error, "catalog file unreadable; skipped");
            return ProviderCatalog::default();
        }
    };
    // Windows editors may leave a UTF-8 BOM; the TOML parser would reject it.
    let contents = contents.strip_prefix('\u{FEFF}').unwrap_or(&contents);
    match toml::from_str::<toml::Table>(contents) {
        Ok(table) => ProviderCatalog::from_table(&table, &path.display().to_string()),
        Err(error) => {
            warn!(file = %path.display(), error = %error, "catalog file unparseable; skipped");
            ProviderCatalog::default()
        }
    }
}

/// Record one model into the on-disk catalog under the provider that owns
/// `base_url`. Re-reads the persisted layer first so a concurrent hand-edit is
/// never clobbered, resolves the key against the *merged* catalog (so a recorded
/// DeepSeek model lands under the seed `deepseek` key, not a host-derived
/// duplicate), then writes atomically. Errors are non-fatal to the caller.
pub fn persist_model(
    home: &Path,
    base_url: &str,
    protocol: ProviderProtocol,
    model_id: &str,
    context_window: Option<u64>,
    source: ModelSource,
) -> std::io::Result<()> {
    let merged = load_catalog(home);
    let key = merged
        .provider_key_by_host(base_url)
        .unwrap_or_else(|| slug_from_host(&host_of(base_url)));
    if key.is_empty() {
        return Err(std::io::Error::other(
            "cannot derive a catalog provider key from base_url",
        ));
    }
    let path = catalog_write_path(home);
    let mut persisted = read_catalog_file(&path);
    persisted.record_model(&key, base_url, protocol, model_id, context_window, source);
    atomic_write(&path, &persisted.to_toml_string())
}

/// Persist several discovered models at once (one `hf models` call), with a
/// single read and a single atomic write.
pub fn persist_discovered(
    home: &Path,
    base_url: &str,
    protocol: ProviderProtocol,
    models: &[(String, Option<u64>)],
) -> std::io::Result<()> {
    let merged = load_catalog(home);
    let key = merged
        .provider_key_by_host(base_url)
        .unwrap_or_else(|| slug_from_host(&host_of(base_url)));
    if key.is_empty() {
        return Err(std::io::Error::other(
            "cannot derive a catalog provider key from base_url",
        ));
    }
    let path = catalog_write_path(home);
    let mut persisted = read_catalog_file(&path);
    for (id, context_window) in models {
        persisted.record_model(
            &key,
            base_url,
            protocol,
            id,
            *context_window,
            ModelSource::Discovered,
        );
    }
    atomic_write(&path, &persisted.to_toml_string())
}

/// Write via a sibling temp file then rename over the target, so a crash never
/// leaves a truncated catalog. Same temp-then-rename shape and retry semantics
/// as `session.rs::save_to_path`.
fn atomic_write(path: &Path, contents: &str) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let file_name = path.file_name().map_or_else(
        || String::from("provider.toml"),
        |name| name.to_string_lossy().into_owned(),
    );
    let temp = path.with_file_name(format!(".{file_name}.tmp-{}", std::process::id()));
    fs::write(&temp, contents)?;
    if let Err(error) = rename_with_transient_retry(&temp, path) {
        let _ = fs::remove_file(&temp);
        return Err(error);
    }
    Ok(())
}

/// Retry the couple-of-milliseconds-wide `rename` window.
///
/// On Windows a real-time AV scanner can hold the freshly written sibling temp
/// file open for a few milliseconds after `fs::write` returns, which makes an
/// otherwise-atomic rename fail with `ACCESS_DENIED` outright. Only
/// `PermissionDenied` is retried — any other error kind is a real failure and is
/// returned on the first attempt, so a genuinely invalid target (a directory, a
/// vanished temp) does not burn two pointless retries. Byte-for-byte the
/// semantics of `heartflow-runtime`'s `session.rs::rename_with_transient_retry`
/// (the two crates cannot share the helper without a common dependency).
fn rename_with_transient_retry(from: &Path, to: &Path) -> std::io::Result<()> {
    const ATTEMPTS: usize = 3;
    const BACKOFF: Duration = Duration::from_millis(50);

    let mut last: Option<std::io::Error> = None;
    for attempt in 0..ATTEMPTS {
        match fs::rename(from, to) {
            Ok(()) => return Ok(()),
            Err(error) => {
                let retryable = error.kind() == std::io::ErrorKind::PermissionDenied;
                last = Some(error);
                if !retryable {
                    break;
                }
                if attempt + 1 < ATTEMPTS {
                    std::thread::sleep(BACKOFF);
                }
            }
        }
    }
    Err(last.unwrap_or_else(|| std::io::Error::other("rename was never attempted")))
}

/// The lowercase host of a URL, stripped of scheme, path, port and any
/// `user:pass@`, so `https://api.deepseek.com/v1` and `https://api.deepseek.com`
/// compare equal.
fn host_of(url: &str) -> String {
    let without_scheme = url.split_once("://").map_or(url, |(_, rest)| rest);
    let authority = without_scheme.split('/').next().unwrap_or(without_scheme);
    let host = authority.rsplit('@').next().unwrap_or(authority);
    host.split(':').next().unwrap_or(host).to_ascii_lowercase()
}

/// A best-effort slug for an unknown host: drop a leading service label
/// (`api`/`open`/`ark`/`chat`) and take the next label. Only used when a
/// `base_url` matches no catalog provider.
fn slug_from_host(host: &str) -> String {
    let mut labels: Vec<&str> = host.split('.').filter(|label| !label.is_empty()).collect();
    while labels.len() > 2 && matches!(labels[0], "api" | "open" | "ark" | "chat") {
        labels.remove(0);
    }
    labels.first().copied().unwrap_or(host).to_string()
}

fn unix_now() -> Option<u64> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .map(|elapsed| elapsed.as_secs())
}

fn u64_field(table: &toml::Table, key: &str, source: &str) -> Option<u64> {
    match table.get(key).map(toml::Value::as_integer) {
        Some(Some(value)) => u64::try_from(value).map_or_else(
            |_| {
                warn!(file = %source, field = key, value, "catalog field out of range; skipped");
                None
            },
            Some,
        ),
        Some(None) => {
            warn!(file = %source, field = key, "catalog field type mismatch; skipped");
            None
        }
        None => None,
    }
}

/// A bare TOML key when safe, a quoted string otherwise.
fn bare_or_quoted(key: &str) -> String {
    let bare = !key.is_empty()
        && key
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
    if bare {
        key.to_string()
    } else {
        quote_toml_string(key)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seed_carries_deepseek_and_matches_by_host() {
        let catalog = default_catalog();
        let deepseek = catalog
            .provider_by_host("https://api.deepseek.com/v1")
            .expect("deepseek seeded");
        assert!(deepseek
            .models
            .iter()
            .any(|model| model.id == "deepseek-v4-flash"));
        // The phantom `deepseek-flash` id never existed on the platform and must
        // not be seeded; the current V4 line is present instead.
        assert!(
            !deepseek.models.iter().any(|m| m.id == "deepseek-flash"),
            "deepseek-flash is not a real model and must not be seeded"
        );
        assert!(deepseek
            .models
            .iter()
            .any(|model| model.id == "deepseek-v4.1-flash"));
        // Retired Kimi ids are dropped; the current flagship carries its exact
        // 1M (2^20) window.
        let moonshot = catalog.providers.get("moonshot").expect("moonshot seeded");
        assert!(!moonshot.models.iter().any(|m| m.id == "kimi-k2.5"));
        let k3 = moonshot
            .models
            .iter()
            .find(|model| model.id == "kimi-k3")
            .expect("kimi-k3 seeded");
        assert_eq!(k3.context_window, Some(1_048_576));
        // Qianfan gained confirmed ERNIE ids; qwen-turbo (retired line) is gone.
        assert!(catalog.providers["qianfan"]
            .models
            .iter()
            .any(|model| model.id == "ernie-5.0"));
        assert!(!catalog.providers["qwen"]
            .models
            .iter()
            .any(|model| model.id == "qwen-turbo"));
        // The /v1 suffix and a trailing slash must not break the host match.
        assert_eq!(
            catalog
                .provider_key_by_host("https://api.deepseek.com/")
                .as_deref(),
            Some("deepseek")
        );
        assert_eq!(
            catalog
                .provider_key_by_host("https://open.bigmodel.cn/api/paas/v4")
                .as_deref(),
            Some("zhipu")
        );
    }

    #[test]
    fn record_model_upserts_and_unions_on_merge() {
        let mut seed = default_catalog();
        let before = seed
            .providers
            .get("deepseek")
            .map_or(0, |provider| provider.models.len());

        let mut persisted = ProviderCatalog::default();
        persisted.record_model(
            "deepseek",
            "https://api.deepseek.com/v1",
            ProviderProtocol::OpenAi,
            "deepseek-v4-flash",
            Some(1_000_000),
            ModelSource::Discovered,
        );
        persisted.record_model(
            "deepseek",
            "https://api.deepseek.com/v1",
            ProviderProtocol::OpenAi,
            "a-brand-new-model",
            None,
            ModelSource::Discovered,
        );

        seed.merge(persisted);
        let deepseek = seed.providers.get("deepseek").expect("deepseek present");
        // One existing id updated in place, one new id appended.
        assert_eq!(deepseek.models.len(), before + 1);
        let recorded = deepseek
            .models
            .iter()
            .find(|model| model.id == "deepseek-v4-flash")
            .expect("seeded model still present");
        assert_eq!(recorded.source, ModelSource::Discovered);
        assert!(deepseek
            .models
            .iter()
            .any(|model| model.id == "a-brand-new-model"));
    }

    #[test]
    fn toml_round_trips_the_persisted_layer() {
        let mut persisted = ProviderCatalog::default();
        persisted.record_model(
            "acme",
            "https://api.acme.test/v1",
            ProviderProtocol::OpenAi,
            "acme-large",
            Some(200_000),
            ModelSource::User,
        );
        let text = persisted.to_toml_string();
        let table: toml::Table = toml::from_str(&text).expect("rendered toml should parse");
        let reparsed = ProviderCatalog::from_table(&table, "roundtrip.toml");
        assert_eq!(reparsed, persisted);
        assert_eq!(
            reparsed.providers["acme"].models[0].context_window,
            Some(200_000)
        );
    }

    #[test]
    fn parse_is_fault_isolated() {
        let table: toml::Table = toml::from_str(concat!(
            "[providers.good]\n",
            "base_url = \"https://api.good.test/v1\"\n",
            "protocol = \"openai\"\n",
            "[providers.bad]\n",
            "protocol = \"not-a-protocol\"\n",
            "[[providers.bad.models]]\n",
            "id = \"m\"\n",
            "context_window = \"not-an-int\"\n",
        ))
        .expect("toml should parse");
        let catalog = ProviderCatalog::from_table(&table, "test.toml");
        // The unknown protocol is skipped, not fatal; the good provider survives.
        assert_eq!(
            catalog.providers["good"].protocol,
            Some(ProviderProtocol::OpenAi)
        );
        assert_eq!(catalog.providers["bad"].protocol, None);
        // The out-of-range context_window is dropped but the model id is kept.
        assert_eq!(catalog.providers["bad"].models[0].context_window, None);
    }

    #[test]
    fn persist_model_resolves_the_seed_key_and_survives_reload() {
        let home = std::env::temp_dir().join(format!("hf-catalog-{}", std::process::id()));
        let _ = fs::remove_dir_all(&home);
        persist_model(
            &home,
            "https://api.deepseek.com/v1",
            ProviderProtocol::OpenAi,
            "deepseek-v4-flash",
            Some(1_000_000),
            ModelSource::Discovered,
        )
        .expect("persist should succeed");
        // A recorded seed-provider model must land under the seed key, not a
        // host-derived duplicate, and be visible after a fresh load.
        let reloaded = load_catalog(&home);
        assert!(reloaded.providers.contains_key("deepseek"));
        assert!(!reloaded.providers.contains_key("api"));
        let recorded = reloaded.providers["deepseek"]
            .models
            .iter()
            .find(|model| model.id == "deepseek-v4-flash")
            .expect("recorded model present");
        assert_eq!(recorded.source, ModelSource::Discovered);
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn host_and_slug_helpers() {
        assert_eq!(host_of("https://api.deepseek.com/v1"), "api.deepseek.com");
        assert_eq!(
            host_of("HTTPS://Open.BigModel.cn:443/api"),
            "open.bigmodel.cn"
        );
        assert_eq!(slug_from_host("api.deepseek.com"), "deepseek");
        assert_eq!(slug_from_host("dashscope.aliyuncs.com"), "dashscope");
    }
}
