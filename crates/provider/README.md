# heartflow-provider

provider 配置解析与 API 流式桥接层:把 `[provider]` 配置归并成传输档案(`ProviderProfile`),并按协议分发到 `api` crate 的流式客户端,再将阻塞客户端缝接成异步 `TurnStream`。

## 模块结构

- config.rs 配置解析:`CLI 参数 > 项目 .heartflow/config.toml > 用户 ~/.heartflow/config.toml > 内置 provider 表 > 环境变量` 逐字段覆盖;坏字段跳过告警,单条不阻断。`CONFIG_VERSION` 做 schema 版本锚点,老文件缺字段走默认、未知字段前向兼容。
- config.rs 协议枚举:`ProviderProtocol` 三值——Anthropic(`/v1/messages` 方言)、OpenAi(`chat/completions` 方言,DeepSeek 原生)、OpenAiResponses(OpenAI `/v1/responses` 方言)。配置别名:`openai-responses` / `responses` / `openai_responses`。
- config.rs 内置表:`deepseek`(OpenAi 方言,`https://api.deepseek.com/v1`,密钥环境变量 `DEEPSEEK_API_KEY`)与 `anthropic`(Anthropic 方言)。配置层 `base_url`/`protocol` 等字段可覆盖内置项(代理端点场景)。
- adapter.rs 传输分发:`TransportClient::from_profile` 按协议产出 Anthropic / OpenAi / OpenAiResponses 三个流式客户端;统一实现 `ApiClient::stream`,把 `ApiRequest` 转成各协议 wire body(系统提示词、工具规格、历史消息格式化),SSE 帧经 mpsc 转成 `AgentEvent`(TextDelta / ThinkingDelta / ToolUse / Usage / MessageStop)。
- adapter.rs 适配细节:DeepSeek 的 `prompt_cache_hit_tokens` 映射为 cache_read 用量;`reasoning_content` 增量映射为 ThinkingDelta;content_block_start 的 `{}` 工具入参骨架与增量 JSON 拼接容错。

## 依赖方向

`provider → api`,被 `runtime` 与 `cli` 消费。provider 不感知终端与渲染;api 不感知会话语义。

## 配置接入

```toml
# ~/.heartflow/config.toml
[provider]
name = "deepseek"             # 内置项:deepseek / anthropic;自定义需配 base_url
model = "deepseek-flash"      # 省略时用内置默认
# protocol = "openai-responses" # 自定义端点可改写协议(内置项不可改)
# base_url = "https://api.deepseek.com/v1"
# api_key_env = "DEEPSEEK_API_KEY"
```

密钥来源仅两处:`api_key_env` 指名的环境变量(配置只存变量名,不存值)或本地配置的内联 `api_key`;导出(`hf config export`)不落密钥。
