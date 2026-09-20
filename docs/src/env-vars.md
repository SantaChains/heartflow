# 环境变量

| 变量 | 作用 |
|------|------|
| ANTHROPIC_AUTH_TOKEN / ANTHROPIC_API_KEY | 默认 anthropic 模式密钥 |
| DEEPSEEK_API_KEY | 内置 deepseek provider 密钥 |
| HEARTFLOW_LOG | 日志级别门，默认 warn，输出至 stderr |
| HEARTFLOW_PERMISSION_MODE | read-only / workspace-write（默认）/ full，REPL 内 /mode 可切换 |
| HEARTFLOW_SHELL | 覆盖 bash 工具的 shell 程序 |
| HEARTFLOW_AUTO_COMPACT_TOKENS | 模型上下文窗口 tokens；设后即启用回合内 `>50%` 预压缩，未设则回退到 `config.toml` 的 `[provider] context_window`，两者皆无则关闭。用 `hf --provider NAME models` 查窗口 |
| HEARTFLOW_REPLAY_VERBATIM_TAIL | 恢复注入时逐字保留的最近 tool_result 条数（默认 12；`>0` 生效，否则回落默认） |
| HEARTFLOW_IMAGE_API_KEY / HEARTFLOW_IMAGE_BASE_URL | 配置后启用 `generate_image` 工具 |
| HEARTFLOW_RESTART_DEPTH | 内部重启深度守卫（自举用，勿手工设置） |
