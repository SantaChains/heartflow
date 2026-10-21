# 环境变量

| 变量 | 作用 |
|------|------|
| ANTHROPIC_AUTH_TOKEN / ANTHROPIC_API_KEY / ANTHROPIC_BASE_URL | 环境型 provider 的密钥与基址 |
| DEEPSEEK_API_KEY | 内置 deepseek provider 密钥（默认模型 `deepseek-v4-flash`） |
| HEARTFLOW_LOG | 日志级别门，默认 warn，输出至 stderr |
| HEARTFLOW_PERMISSION_MODE | read-only / workspace-write（交互式默认）/ full（非交互默认）；另接受 plan 与 auto（=full）。REPL 内 /mode 可切换前三个，plan 由 /plan 进入 |
| HEARTFLOW_SHELL | 覆盖 bash 工具的 shell 程序 |
| HEARTFLOW_AUTO_COMPACT_TOKENS | 模型上下文窗口 tokens；设后即启用回合内 `>50%` 预压缩，未设则回退到 `config.toml` 的 `[provider] context_window`，两者皆无则关闭。用 `hf --provider NAME models` 查窗口 |
| HEARTFLOW_COMPACT_TRIGGER | 窗口模式下的软触发口径：`half-window`（默认，估算 ≥ 窗口/2）/ `ds4`（估算 ≥ 85%×窗口，或剩余 ≤ min(8192, 窗口/4)）。仅在窗口模式生效；绝对阈值模式不受影响 |
| HEARTFLOW_COMPACT_TAIL_BUDGET | 压缩保留尾的 token 上限（默认 0 = 关闭，纯条数切割）。超预算时切割点前移、把更多旧消息并入摘要，且绝不孤立 tool_result；单条消息独超预算时保留并如实上报 |
| HEARTFLOW_REPLAY_VERBATIM_TAIL | 恢复注入时逐字保留的最近 tool_result 条数（默认 12；`>0` 生效，否则回落默认） |
| HEARTFLOW_POST_EDIT_DIAGNOSTICS | 设 `0` 关闭写后诊断（写类工具改动 `.rs`/`Cargo.toml` 后自动跑 `cargo check --message-format=short` 并把结果回注进同一工具结果；默认开启） |
| HEARTFLOW_DIAGNOSTICS_TIMEOUT_SECS | 单次写后 `cargo check` 的超时秒数（默认 60；`>0` 生效）。超时按降级账本计数，不阻断工具调用 |
| HEARTFLOW_JOB_SLOTS | 子代理 job 的并发槽位数（默认 3；`>0` 生效）。超出的 job 排队等槽 |
| HEARTFLOW_JOB_MAX_PENDING | queued+running 的 job 积压上限（默认 20；`>0` 生效）。满则 `/jobs spawn` 拒绝 |
| HEARTFLOW_JOB_TIMEOUT_SECS | 单个 running job 的墙钟超时秒数（默认 1800；`0` 关闭）。到期协作式停止（在飞 LLM 调用先完成），终态附注「wall-clock timeout … NOT finished」；计时从 Queued→Running 翻转开始，排队等待不计入 |
| HEARTFLOW_BASH_IDLE_TIMEOUT_MS | 前台命令的「无输出挂起」看门狗毫秒数（默认关闭；`>0` 生效）。命令在两条流上连续这么久没有任何输出即按挂死终止，结果标 `interrupted` + `idle_timeout`，静默前的部分输出保留；后台任务不受此看门狗约束 |
| HEARTFLOW_IMAGE_API_KEY / HEARTFLOW_IMAGE_BASE_URL / HEARTFLOW_IMAGE_MODEL / HEARTFLOW_IMAGE_SIZE | 配置后启用 `generate_image` 工具并参数化端点、模型与尺寸 |
| HEARTFLOW_COOKIE_JAR | 指向一个文件即开启 `web_fetch` 的会话 cookie 复用 |
| HEARTFLOW_PROXY | 传输代理 URL（如 `http://127.0.0.1:7890`、`socks5://127.0.0.1:1080`）。优先级高于 `HTTPS_PROXY`/`HTTP_PROXY`/`ALL_PROXY` 与 Windows 系统代理，低于 `config.toml` 的 `[proxy] address` |
| HEARTFLOW_PROXY_MODE | `direct`（完全不走代理，忽略环境与系统代理）/ `auto`（默认）/ `global`。见配置文档的 `[proxy]` 节 |
| HEARTFLOW_NO_PROXY | 逗号分隔的 bypass 列表（主机、`.后缀`、IP、CIDR、`*`），与 `[proxy] no_proxy` 合并生效 |
| HEARTFLOW_CONFIG_HOME | 覆盖用户配置根（默认 `~/.heartflow`）的 config.toml 查找位置 |
| HEARTFLOW_RESTART_DEPTH | 内部重启深度守卫（`/restart` 链式重启用，勿手工设置） |