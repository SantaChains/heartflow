//! Provider 配置解析与 API 流式桥接:把 `[provider]` 配置归并成传输档案,
//! 并将阻塞的 `api` 客户端缝接成异步 `TurnStream`。

pub mod adapter;
pub mod config;
pub mod load;

pub use adapter::{AnthropicStreamClient, TransportClient};
pub use config::{
    config_file_paths, load_merged_settings, set_config_override, ProviderProfile, ProviderProtocol,
    ProviderSelection, ProviderSettings, CONFIG_VERSION,
};
pub use load::{
    catalog_file_paths, catalog_write_path, default_catalog, load_catalog, persist_discovered,
    persist_model, CatalogModel, CatalogProvider, ModelSource, ProviderCatalog,
};
