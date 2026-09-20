# 环境变量

| 变量 | 作用 |
|------|------|
| ANTHROPIC_AUTH_TOKEN / ANTHROPIC_API_KEY / ANTHROPIC_BASE_URL | 环境型 provider 的密钥与基址 |
| DEEPSEEK_API_KEY | 内置 deepseek provider 密钥（默认模型 `deepseek-v4-flash`） |
| HEARTFLOW_LOG | 日志级别门，默认 warn，输出至 stderr |
| HEARTFLOW_PERMISSION_MODE | read-only / workspace-write（交互式默认）/ full（非交互默认）；另接受 plan 与 auto（=full）。REPL 内 /mode 可切换前三个，plan 由 /plan 进入 |
| HEARTFLOW_SHELL | 覆盖 bash 工具的 shell 程序 |
| HEARTFLOW_AUTO_COMPACT_TOKENS | 模型上下文窗口 tokens；设后即启用回合内 `>50%` 预压缩，未设则回退到 `config.toml` 的 `[provider] context_window`，两者皆无则关闭。用 `hf --provider NAME models` 查窗口 |
| HEARTFLOW_REPLAY_VERBATIM_TAIL | 恢复注入时逐字保留的最近 tool_result 条数（默认 12；`>0` 生效，否则回落默认） |
| HEARTFLOW_IMAGE_API_KEY / HEARTFLOW_IMAGE_BASE_URL / HEARTFLOW_IMAGE_MODEL / HEARTFLOW_IMAGE_SIZE | 配置后启用 `generate_image` 工具并参数化端点、模型与尺寸 |
| HEARTFLOW_COOKIE_JAR | 指向一个文件即开启 `web_fetch` 的会话 cookie 复用 |
| HEARTFLOW_CONFIG_HOME | 覆盖用户配置根（默认 `~/.heartflow`）的 config.toml 查找位置 |
| HEARTFLOW_RESTART_DEPTH | 内部重启深度守卫（`/restart` 链式重启用，勿手工设置） |