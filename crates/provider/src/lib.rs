//! Provider 配置解析与 API 流式桥接:把 `[provider]` 配置归并成传输档案,
//! 并将阻塞的 `api` 客户端缝接成异步 `TurnStream`。

pub mod adapter;
pub mod cassette;
pub mod config;
pub mod load;

pub use adapter::{AnthropicStreamClient, TransportClient};
pub use cassette::{read_entries, CassetteClient, CassetteMode, RecordedEntry, CASSETTE_ENV};
pub use config::{
    builtin_base_url, builtin_provider_names, config_file_paths, env_base_url_override,
    load_merged_settings, set_config_override, CredentialSource, ProviderProfile, ProviderProtocol,
    ProviderSelection, ProviderSettings, StatusConfig, CONFIG_VERSION,
};
pub use load::{
    catalog_file_paths, catalog_write_path, default_catalog, load_catalog, persist_discovered,
    persist_model, persist_reasoning_effort, persist_selection, CatalogModel, CatalogProvider,
    ModelSource, ProviderCatalog,
};
