//! Provider 配置解析与 API 流式桥接:把 `[provider]` 配置归并成传输档案,
//! 并将阻塞的 `api` 客户端缝接成异步 `TurnStream`。

pub mod adapter;
pub mod config;

pub use adapter::{AnthropicStreamClient, TransportClient};
pub use config::{
    config_file_paths, load_merged_settings, ProviderProfile, ProviderProtocol, ProviderSelection,
    ProviderSettings, CONFIG_VERSION,
};
