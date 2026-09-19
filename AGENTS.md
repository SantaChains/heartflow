# AGENTS.md

本文件为在此仓库中工作的 agent 提供指引。heartflow 加载仓库指令时以 `AGENTS.md` 为主(中性跨工具标准),并为兼容旧仓库继续读取 `CLAUDE.md`。

## 项目概览

heartflow 是一个 Rust 实现的终端 AI agent(仓库名 `heartflow`,二进制 `hf`)。REPL 中通过 SSE 真流式与模型协作,执行 shell、读写文件、检索代码、挂载 MCP 工具,并以任务循环自迭代完成多步工作。

- 语言/工具链:Rust 1.85+(本机为 nightly),edition 2021
- 形态:Cargo workspace,`members = ["crates/*"]`,每个 crate 都是独立库,`cli` 产出二进制 `hf`
- 身份约束:`FRONTIER_MODEL_NAME = "heartflow"`(`crates/runtime/src/prompt.rs:39`)。系统提示词强制“永远是 heartflow,不得冒充其他厂商/身份”。改动提示词时保留此约束。

## 常用命令

所有命令在仓库根(含 `Cargo.toml`)执行。

```bash
cargo build --release          # 产物 target/release/hf(.exe)
cargo run -p heartflow         # 直接进入 REPL
cargo fmt --all -- --check     # 格式门
cargo clippy --workspace --all-targets -- -D warnings -A clippy::pedantic   # 质量门:阻断 clippy::all 正确性;pedantic 降为提示(效率优先)
cargo test --workspace         # 全部测试
cargo test -p store            # 单 crate 测试(store/tests/io_correctness.rs 为 I/O 正确性重点)
```

单测粒度:`cargo test -p <crate> <test_name>`。

CLI 冒烟:`cargo run -p heartflow -- --help`、`... -- doctor`、`... -- system-prompt`。

## Workspace 结构

依赖方向单向:`cli → {runtime, api, tools, mcp, store, commands}`;`runtime` 不感知传输细节,`api/runtime/cli` 三层边界不得破。

```text
crates/
├── api        传输层:Anthropic / OpenAI Chat / OpenAI Responses 客户端、SSE 解析、重试。仅依赖 reqwest/serde/tokio。
├── runtime    会话循环:流消费、工具调度、compact、系统提示词、权限、bash/file_ops、agent 资产发现。
│              agent 循环核心在 conversation.rs(ConversationRuntime、ToolExecutor、TurnStream、AgentEvent)。
├── tools      原生工具的线上规格(wire spec)与执行:bash/read/write/edit/glob/grep、search_files(nucleo 模糊检索)、apply_patch(事务式多文件编辑)、todo、ask_user、web_fetch、verify_graphics、generate_image。新工具(search_files/apply_patch)的入参 schema 由 schemars 从输入类型派生;其余工具仍为手写 `json!` schema。
├── mcp        MCP 客户端:stdio JSON-RPC 2.0 传输。
├── commands   请求/响应数据结构(薄)。
├── store      系统级 SQLite 历史库:JSONL 权威 + best-effort 镜像到 ~/.heartflow/heartflow.db(FTS5 trigram 检索、用量聚合、integrity_check)。
└── cli        hf 入口:REPL、clap CLI、配置解析、渲染、行编辑、权限交互。
```

## 运行时数据与配置

- 会话:`~/.heartflow/sessions/*.jsonl`(JSONL 为权威存储,原子写入)
- 历史库:`~/.heartflow/heartflow.db`(从 JSON 派生的缓存;损坏可删除重建,不阻断保存)
- 配置优先级:`CLI 参数 > 项目 .heartflow/config.toml > 用户 ~/.heartflow/config.toml > 内置 provider 表 > 环境变量`。同名字段逐项覆盖,坏字段跳过告警,单条不阻断启动。REPL 每回合按 mtime 热重载。
- Agent 资产:规则 `~/.agent/rules/*.md`(全量注入,上限 32 个/单个 32KB)与 `.agent/rules/`(项目层覆盖用户层);技能 `~/.agent/skills/*/SKILL.md` 仅注入 name/description 元数据。
- 权限模式:`read-only / workspace-write(默认, 由 HEARTFLOW_PERMISSION_MODE 控制) / full`,REPL 内 `/mode` 热切换,工具级可覆盖。

关键环境变量:`ANTHROPIC_AUTH_TOKEN`/`ANTHROPIC_API_KEY`、`DEEPSEEK_API_KEY`、`HEARTFLOW_LOG`(默认 warn,输出 stderr)、`HEARTFLOW_PERMISSION_MODE`、`HEARTFLOW_SHELL`、`HEARTFLOW_IMAGE_API_KEY`(生图工具,可选)。

## 代码约定

- Clippy:workspace 级 `pedantic = warn`,`unsafe_code = forbid`。禁止 `unwrap`/`expect` 出现在可失败路径,用 `Result` + 错误类型传递。提交时以 `clippy::all` 为零告警硬门(正确性);pedantic 属风格建议,按效率优先仅作提示不阻断(存量 ~27 处为历史一次性清扫后的基线,不强求清零)。
- 错误类型:crate 内自定义(`ApiError`、`StoreError`、`RuntimeError`、`ConfigError`),不用 `anyhow` 风格泛型下沉到库。
- 工具注册:线上规格集中在 `crates/tools/src/lib.rs` 的 `mvp_tool_specs()` 与各 `*_tool_spec()`;分发在 `execute_tool`。新增工具须同时补 spec、execute 分支、`ToolRegistry::entries` 并接线到 adapter。`ask_user` 的**执行**在 CLI 层(需真实终端),`tools` crate 只持有 wire spec。
- 全链路 UTF-8:文件读写支持 BOM 剥除、CJK 宽度对齐;Windows 走 PowerShell(pwsh 优先),命令需可移植。中文/空格路径必须正确处理。
- 输出风格:纯文本精简,不使用 emoji、分隔线、无谓注释;只注释关键逻辑。
- 日志:`tracing` + `HEARTFLOW_LOG` 门控,一律 stderr,不污染渲染 stdout。

## 测试要求

- 工具/history/store 的 I/O 与风险路径须经得起测试,且不得损坏数据(见 `crates/store/tests/io_correctness.rs`)。
- 传输层与事件流转换的缺口只有真服务器冒烟能暴露(历史缺陷:SSE `message_stop` 未转发、工具输入拼接损坏)。涉及流式/工具往返的改动须跑端到端冒烟。
- 提交前跑齐质量门:fmt + test + release build(必绿)+ clippy(仅 `all` 阻断,pedantic 提示)。

## 发布流程(自动化)

`.github/workflows/release.yml` 在 push main 时以 Conventional Commits 自动驱动版本、GitHub Release、crates.io 与 Windows 便携包,三段式 `plan(dry) → publish → windows-zip`,零 Node(不引入 semantic-release/cargo 插件链):发布逻辑集中在 `scripts/release.sh`,CI 与本地共用同一事实源。

- 本地预检(必须先于 push,不得用 Actions 试错):`bash scripts/release.sh plan`;`RELEASE_DRY_RUN=1 bash scripts/release.sh publish`(只读预演,需本地装 git-cliff);质量门 fmt/clippy/test 本地全绿后才 push,Actions 只跑真实发布。
- 版本语义:`feat:` → minor,`fix:` → patch,`!` 或 `BREAKING CHANGE:` → major;`chore/docs/test/ci/style/build` 不触发发版,在 plan job 秒级短路(文案与过滤规则见根目录 `cliff.toml`)。
- 提交 scope 用 crate 名,如 `feat(tools): ...`;Release Notes 按中文分栏并加粗 scope。
- 版本号唯一维护点在根 `Cargo.toml` 的 `[workspace.package]`;各 crate 一律 `version.workspace = true`,禁止写死版本号。
- crates.io:api/runtime/mcp/tools/store/commands 裸名已被占用,内部 crate 统一挂 `heartflow-` 前缀发布,`[lib] name` 保持旧 extern 名,依赖经 `[workspace.dependencies]` 的 `package =` 重命名(源码 `use` 不变);publish job 按依赖拓扑逐个发,单 crate 失败自动重试 3 次(sparse index 传播延迟),认证走仓库 secret `CRATES_IO_TOKEN`;版本一次性,同版本不可重发。
- 机器人提交 `chore(release): vX.Y.Z [skip ci]` 自动同步 Cargo.toml/Cargo.lock/CHANGELOG、打 tag、建 GitHub Release;勿手工仿写此类提交。
- Windows amd64 便携 zip(hf.exe 置于压缩包根 + .sha256)随每次发布上传到 Release;本仓库 `bucket/` 目录兼作 scoop bucket,清单 `bucket/heartflow.json` 带 checkver/autoupdate,新 Release 即被 scoop 发现(用户先 `scoop bucket add heartflow <仓库url>` 再 `scoop install heartflow`)。
- git-cliff 版本钉在 `.github/actions/install-git-cliff/action.yml`(模板引擎行为随版本变动,升级需显式改并先过本地 dry-run)。
- 若 main 启用分支保护,需允许 GitHub Actions 直接推送;发布流水线不做代码检查。

## 注意

- `.gitignore` 忽略 `target/`、`.heartflow/`、`archive/`、`.history/`、`.trae/`,以及本地笔记 `openmemory.md`、`ref.md`、`error.md`(个人头脑风暴/参考资料,不发布)。
- 质量门当前以本地为准(fmt + clippy `-D warnings -A clippy::pedantic`(仅正确性阻断)+ test + release);`ci.yml.bak` 为暂存的 CI 工作流,启用时改回 `ci.yml`;许可证 MIT(见 `LICENSE`)。
- 修改 README 中列出的 CLI/REPL 接口时,同步更新 README 与 `--help`。
