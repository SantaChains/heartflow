# AGENTS.md

本仓库的 agent 指令文件。heartflow 逐级向上读取 `<dir>/AGENTS.md` 与 `<dir>/AGENTS.local.md`，仅当同目录无 `AGENTS.md` 时回退旧名 `CLAUDE.md`（排他回退，避免双注入）——`crates/runtime/src/prompt.rs:291`。

## 定位与硬约束

- crate 名 `heartflow`，二进制 `hf`，edition 2021，MSRV 1.88（下限来自依赖，声明在根 `Cargo.toml` 的 `[workspace.package] rust-version`），工具链由 `rust-toolchain.toml` 钉在 1.93.1 + rustfmt/clippy。
- 身份不变量：`FRONTIER_MODEL_NAME = "heartflow"`（`crates/runtime/src/prompt.rs:39`）。系统提示词禁止模型冒充其他厂商身份，改提示词时保留该约束。
- 版本号唯一维护点：根 `Cargo.toml` 的 `[workspace.package]`；各 crate 一律 `version.workspace = true`。
- 分层边界不可破：`api`（传输，仅 reqwest/serde/tokio）→ `provider`（协议桥接）→ `runtime`（会话循环，不感知传输细节）；`cli` 只做装配与交互。`runtime` 不得引用具体协议类型。

## 依赖拓扑与发布顺序

```text
cli ─→ {provider, runtime, api, tools, mcp, store, commands}
provider ─→ {api, runtime}      tools ─→ runtime      store ─→ runtime      commands ─→ runtime
runtime / api / mcp ─→ 无内部依赖
```

`runtime`、`api`、`mcp` 无内部依赖，故 `scripts/release.sh` 按
`heartflow-runtime → heartflow-api → heartflow-provider → heartflow-mcp → heartflow-tools → heartflow-store → heartflow-commands → heartflow` 逐个发。

## 构建与质量门

```bash
cargo build --release          # target/release/hf(.exe)
cargo run -p heartflow         # 直接进 REPL
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings -A clippy::pedantic
cargo test --workspace         # 单测:cargo test -p <crate> <name>;重点 crates/store/tests/io_correctness.rs
```

`clippy::all` 为零告警是硬门；pedantic 已降为提示（存量基线 ~27 处，效率优先，不强求清零）。冒烟：`cargo run -p heartflow -- --help`、`... -- doctor`、`... -- system-prompt`。

**panic 预算棘轮（提交前手动跑）**：`python3 scripts/check_panic_budget.py`（`--list` 逐个列出分类，`--update` 刷新基线）。只统计 `crates/*/src/**/*.rs` 的**生产**行——剔除 `#[cfg(test)]` 项、测试文件、注释与字符串内容；基线在 `scripts/panic_budget.json`，**`debt` 只许变小，新增文件带 panic 一律失败**。刻意保留的 panic（即找不到「同样清晰且局部」的非 panic 写法）必须在命中行**行尾**或**紧邻上一行**标注 `// panic-ok: <理由 ≥8 字符>`，否则计为新增债。当前基线 `debt=2`（`crates/cli/src/main.rs:398,414`）/ `justified=8`。真的清理掉存量债之后再跑 `--update`——**不要为了让门禁变绿而标注**，棘轮被频繁刷新就等于没有。

**预算基线必须入库**：`scripts/panic_budget.json` 是**源码树的属性**（换台机器跑出同样的数），不是机器本地量测，**不得写进 `.gitignore`**。注意它与 `scripts/bench-gate.sh` 的基线约定**相反**——后者是机器本地性能数，存 `TMPDIR`、从不提交。基线缺失时门禁以 exit 1 报 `no baseline`；此时正确做法是恢复基线，**不是**随手 `--update`（那会把已有回归一并赦免，棘轮当场失效）。

**第三方代码归属**：任何衍生/移植自外部项目的代码（脚本、片段、算法）须在根 `NOTICE` 追加版权声明与许可证全文，并在 `README.md` §致谢与第三方代码点名来源。当前唯一项：`scripts/check_panic_budget.py`（衍生自 MIT 许可的 jcode，Copyright (c) 2025 Jeremy Huang）。`archive/` 已在 `.gitignore` 中、**不随仓库分发**，所以归属信息不能只写在存档里。

CI 只有两条链：`release.yml`（发版）、`docs.yml`（文档站，`docs/**`/`scripts/gen-llms.sh` 变动才触发）；`ci.yml.bak` 是刻意停用的质量门，改名 `.bak` 后 GitHub 不识别。Actions 不跑代码检查。

## 接口面

改下列任一公共契约，须同步 README、`docs/src`、根 `llms.txt`/`llms-full.txt`（`bash scripts/gen-llms.sh`）、`bucket/heartflow.json` 的 notes、`.devin/wiki.json`。

### CLI（`crates/cli/src/main.rs:675` 起的 clap 结构）

全局：`--provider NAME`、`--model MODEL`、`--version`（`-v`，`-V` 为可见短别名；内建版本 flag 已禁用）、`--resume[=PATH]`（`require_equals`，裸形式进选择器）、`--run CMD`（`requires = "resume"`）。

子命令：`chat`、`prompt [TEXT...] [-q|--quiet] [--json]`、`search <QUERY...> [--limit N=20] [--json]`、`system-prompt [--cwd PATH] [--date YYYY-MM-DD]`、`config export [SURFACE=config|theme|keymap|settings] [--output FILE]` / `config import FILE`、`doctor [--fix] [--ai]`、`init [--force]`、`models [--provider] [--model] [--balance]`。

交互契约：无参数或 `hf chat` 进 REPL（唯一可弹确认的模式）；子命令永不阻塞等人。退出码 0 成功 / 1 运行时与 provider 错误 / 2 用法错误。

### 权限策略（`crates/cli/src/main.rs:3708` `default_permission_mode`、`:3728` `permission_policy_for_mode`）

默认模式：`HEARTFLOW_PERMISSION_MODE` 优先；未设时交互式 `workspace-write`、非交互 `full`。`/mode` 认 `read-only`/`workspace-write`/`full`（`auto` 归一为 `full`）；环境变量另外接受 `plan`，其硬门禁只允许写 `.heartflow/plans/*.md`。`read-only` 与 `plan` 两档把 `McpToolset::read_only_tool_names` 并入 Allow，使远程只读 MCP 可用。

### REPL（`crates/cli/src/main.rs:1689` `dispatch_slash_command`）

`/help /status /model [NAME] /mode [NAME] /plan [GOAL|approve|end|status] /compact /pin /save /clear /sessions /open N /remember T /search Q /mcp /expand [ID] /queue [pop|clear] /guide TASK /init /restart /exit`（`/quit` 为别名），以及不走模型的 `!CMD` 前缀。

### 原生工具（`crates/tools/src/lib.rs:137` 注册，`:362` `execute_tool` 分发）

固定注册：`bash`、`read_file`、`write_file`、`edit_file`、`glob_search`、`grep_search`、`search_files`（nucleo 模糊路径检索）、`apply_patch`（先全量校验后写入的事务式多文件编辑）、`todo_write`（`crates/tools/src/todo.rs:156`）、`ask_user`、`verify_graphics`、`web_fetch`、`web_search`、`generate_image`。

条件注册：`search_documents`，仅当外部 `rga`（ripgrep-all）在 PATH 上时下发（`crates/runtime/src/doc_search.rs:124` `rga_available`，装配点在 `crates/cli/src/main.rs:3424` `ToolExecutor::specs`），用于 zip/tar/docx/pdf/epub 内文本检索。

执行归属：`ask_user` 的实现留在 CLI 层（需真终端），`tools` crate 只持 wire spec；其余经 `execute_tool`。新增工具须同时补 spec、`execute_tool` 分支、`specs()` 装配与权限策略条目。`search_files`/`apply_patch` 的入参 schema 由 schemars 从输入类型派生，其余为手写 `json!`。

并发调度：`crates/cli/src/main.rs:3462` `is_concurrent_safe` 决定哪些工具可批跑——只读类（read/glob/grep/search_files/search_documents/verify_graphics/web_fetch/web_search）并行，写类与交互类（bash/write/edit/apply_patch/generate_image/todo_write/ask_user 及非只读 MCP）严格串行。

### 环境变量

| 变量                                                                | 语义                                                          | 读取处                                        |
|---------------------------------------------------------------------|---------------------------------------------------------------|-----------------------------------------------|
| `ANTHROPIC_AUTH_TOKEN` / `ANTHROPIC_API_KEY` / `ANTHROPIC_BASE_URL` | 环境型 provider 的密钥与基址                                  | `crates/api/src/client.rs:159`、provider 解析 |
| `DEEPSEEK_API_KEY`                                                  | 内置 `deepseek` provider 密钥（默认模型 `deepseek-v4-flash`） | `crates/provider/src/config.rs:105`           |
| `HEARTFLOW_LOG`                                                     | tracing 过滤，默认 `warn`，仅 stderr                          | `crates/cli/src/main.rs:185`                  |
| `HEARTFLOW_PERMISSION_MODE`                                         | 见权限策略                                                    | `crates/cli/src/main.rs:3711`                 |
| `HEARTFLOW_SHELL`                                                   | 覆盖 bash 工具的 shell                                        | `crates/runtime/src/bash.rs:338`              |
| `HEARTFLOW_AUTO_COMPACT_TOKENS`                                     | 模型窗口 tokens，优先级高于 `[provider] context_window`       | `crates/cli/src/main.rs:2126`                 |
| `HEARTFLOW_REPLAY_VERBATIM_TAIL`                                    | 回放时逐字保留的近期 `tool_result` 条数，默认 12              | `crates/cli/src/main.rs:2170`                 |
| `HEARTFLOW_IMAGE_API_KEY` / `_BASE_URL` / `_MODEL` / `_SIZE`        | 启用并参数化 `generate_image`                                 | `crates/tools/src/image.rs:166`               |
| `HEARTFLOW_COOKIE_JAR`                                              | 指定文件即开启 `web_fetch` 会话 cookie 复用                   | `crates/tools/src/web.rs:162`                 |
| `HEARTFLOW_CONFIG_HOME`                                             | 覆盖用户配置根（默认 `~/.heartflow`）                         | `crates/runtime/src/config.rs:69`             |
| `HEARTFLOW_RESTART_DEPTH`                                           | `/restart` 链式重启深度守卫，勿手工设置                       | `crates/cli/src/main.rs:148`                  |

### 配置键（`version` 当前为 1，`crates/provider/src/config.rs:12`）

`[provider]`：`name`、`protocol`（`anthropic` | `openai`/`openai-compatible`/`openai_compat` | `openai-responses`/`responses`/`openai_responses`）、`base_url`、`api_key_env`、`api_key`（内联明文，导出时永不写出）、`auth_token_env`、`model`、`max_tokens`、`reasoning_effort`、`context_window`（`crates/provider/src/config.rs:123-140`、`:165-181`）。字段全可选，未识别字段仅 debug 记录，坏字段跳过不阻断。缺省模型 `mimo-v2.5-pro`、缺省 `max_tokens` 4096（`crates/provider/src/config.rs:7-8`）。

`[mcp.servers.NAME]`：`command`、`args`、`env`、`url`、`headers`（值支持 `${VAR}` 展开）、`bearer_token_env`、`read_only`。`url` 与 `command` 二选一，两者皆缺则跳过并告警；`url` 存在即走 Streamable-HTTP/SSE（`crates/cli/src/config.rs:12-29`、`:55-110`）。

`[theme]`：`heading`、`accent`、`muted`、`success`、`error`，可选 `emphasis`、`strong`、`inline_code`、`link`、`quote`；用户层 `~/.heartflow/theme.toml`、项目层 `.heartflow/theme.toml` 逐项覆盖（加载器 `crates/cli/src/theme.rs` 的 `theme_file_paths`）。

`[keymap]`：动作名 → 键串或键串数组（`submit`/`interrupt`/`escape`/`toggle_fold`/`scroll_up`/`scroll_down`）；空数组解绑（键回落编辑器），`ctrl`/`alt`/`super` 参与匹配、`shift` 忽略、字符大小写不敏感；同 theme 两层逐动作覆盖（`crates/cli/src/keymap.rs`）。

`[settings]`：`scroll_step`（u16，默认 3）、`tool_inline_lines`（usize，默认 3）、`fold_thinking`（bool，默认 true）、`frame_budget_ms`（u64，默认 80，仅启动时读取）；同 theme 两层逐项覆盖（`crates/cli/src/settings.rs`）。

`hf config export [SURFACE]`（`config` 默认 / `theme` / `keymap` / `settings`）导出该面生效值为可编辑模板，永不落密钥。

配置优先级：CLI 参数 > 项目 `.heartflow/config.toml` > 用户 `~/.heartflow/config.toml` > 内置 provider 表 > 环境变量；同名字段逐项覆盖。`ConfigWatcher`（`crates/cli/src/config.rs`）每回合边界按 mtime 逐面探测 config/theme/keymap/settings 四面并热重载：`changed() -> ChangedSurfaces` 逐面上报，config 重建 runtime、theme 换入进程级调色板、keymap/settings 就地重载（改 theme 绝不触发 config 重载）。

### 运行时可写路径

| 路径                                             | 角色                                                                                                                                               |
|--------------------------------------------------|----------------------------------------------------------------------------------------------------------------------------------------------------|
| `~/.heartflow/sessions/<id>.json`                | 权威会话快照，simd-json 序列化后 temp+rename 原子覆盖（`crates/runtime/src/session.rs:157`）                                                       |
| `~/.heartflow/sessions/<id>.jsonl`               | 只读的追加段（header 记录其扩展的快照长度），坏段降级回快照（`crates/runtime/src/session.rs:204`）                                                 |
| `~/.heartflow/heartflow.db`                      | SQLite 镜像缓存：WAL、FTS5 trigram、`user_version` 迁移；`append_messages` 增量追加，转录缩短或原地改动时回退全量重写。JSON 恒为权威，库可删库重建 |
| `~/.heartflow/MEMORY.md`、`.heartflow/MEMORY.md` | 跨会话长期记忆，user/project 两层公平夹紧后注入提示词 Memory 段                                                                                    |
| `~/.agent/rules/*.md`、`.agent/rules/`           | 规则全量注入（按名排序，上限 32 个，单个截断 32KB）                                                                                                |
| `~/.agent/skills/*/SKILL.md`、`.agent/skills/`   | 技能仅注入 name/description 元数据（上限 64 个），正文按需读取                                                                                     |
| `.heartflow/plans/`                              | `/plan` 规划期唯一可写目录；`.heartflow/reflections/` 收尾复盘；可选沉淀为 `.agent/skills/<slug>/SKILL.md`                                         |

## 上下文注入纪律

磁盘转录 ≠ 注入视图。构造请求时 `crates/runtime/src/conversation.rs` 的 `build_replay_messages` 把超出 `replay_verbatim_tail`（默认 12）条的旧 `tool_result` 正文折成占位，落盘仍存全文；`tool_use`/`tool_result` 的配对与顺序不可破；`/pin` 标记的消息逐字存活于压缩之后。窗口压缩阈值取半窗（`>50%`，summarize-then-compact）。

## 代码约定

- `unsafe_code = forbid`；可失败路径禁用 `unwrap`/`expect`，用 `Result` + crate 内自定义错误（`ApiError`/`StoreError`/`RuntimeError`/`ConfigError`），不把 `anyhow` 风格泛型下沉到库。
- 工具入参在执行前用 `jsonschema` crate 做完整 JSON-Schema 校验（draft 全能力；空/布尔/编译失败的 schema 一律放行，不误拦合法调用）；转发 provider 前对 MCP schema 做规整（object 补 `properties`、array 补 `items`、单元素 `type` 联合折叠）。
- 全链路 UTF-8：BOM 剥除、非 UTF-8 字节经 `chardetng` 嗅探 + `encoding_rs` 解码遗留码页、CJK 宽度对齐；工具输出 32K 截断。Windows 走 PowerShell（`pwsh` 优先），提交的命令须跨平台可移植。
- 日志走 `tracing` + `HEARTFLOW_LOG` 门控，一律 stderr，不污染渲染 stdout。
- 输出风格：纯文本精简，不用 emoji、分隔线、无谓注释，只注释关键逻辑。
- 传输层与事件流的缺口只有真服务器冒烟能暴露（历史缺陷：SSE `message_stop` 未转发、工具输入拼接损坏）。改流式或工具往返必须跑端到端冒烟。

## 发布流程

三段式 `plan(dry) → publish → windows-zip`，零 Node；发布逻辑集中在 `scripts/release.sh`，CI 与本地共用同一事实源。

- 本地预检（必须先于 push，不得用 Actions 试错）：`bash scripts/release.sh plan`；`RELEASE_DRY_RUN=1 bash scripts/release.sh publish`（只读预演，需本地 git-cliff）。质量门本地全绿后才 push。
- 版本语义：`feat:` → minor、`fix:` → patch、`!` 或 `BREAKING CHANGE:` → major；`chore/docs/test/ci/style/build` 不入正文且 plan job 秒级短路（规则见根 `cliff.toml`）。提交 scope 用 crate 名，如 `feat(tools): ...`。
- crates.io：裸名 `api/runtime/mcp/tools/store/commands` 已被占用，内部 crate 统一挂 `heartflow-` 前缀发布，`[lib] name` 与源码 `use` 保持不变（靠 `[workspace.dependencies]` 的 `package =` 重命名）。认证走仓库 secret `CRATES_IO_TOKEN`；单 crate 失败自动重试（429 退避），"already uploaded" 幂等跳过；版本一次性，同版本不可重发。
- 续发：publish 中途失败时 tag 与 Release 已建，重跑整链会因 "tag 之后无 feat/fix" 判 `released=false` 而短路；改用 `RELEASE_TAG=<tag> RELEASE_BUMP=<bump> bash scripts/release.sh publish`。
- 机器人提交 `chore(release): vX.Y.Z [skip ci]` 自动同步 Cargo.toml/Cargo.lock/CHANGELOG、打 tag、建 Release；勿手工仿写。
- Windows amd64 便携 zip（`hf.exe` 置于包根 + `.sha256` + `SHA256SUMS` + SLSA provenance）随发布上传；`bucket/heartflow.json` 的 version/url/hash 由 windows-zip job 按 tag 确定性回写（CI 是唯一写入者，哈希取自本地构建产物）。自托管 bucket 无自动更新机器人，故不配 autoupdate 块，仅留 checkver。
- git-cliff 版本钉在 `.github/actions/install-git-cliff/action.yml`（模板引擎行为随版本变动，升级须显式改并先过本地 dry-run）。所有 action 按 SHA 固定。

## 其他

- `.gitignore` 忽略 `target/`、`.heartflow/`、`archive/`、`.history/`、`.trae/`，以及本地笔记 `openmemory.md`、`ref.md`、`error.md`。
- 文档源在 `docs/src`（mdBook，输出 `docs/book/` 已忽略）；`scripts/gen-llms.sh` 按 llms.txt v2 从 `docs/src/SUMMARY.md` 生成仓库根 `llms.txt`/`llms-full.txt`，改文档后重跑（本机 PATH 的 `bash` 若非 GNU bash 会缺 `mapfile`，用 Git 的 bash）。GitHub Pages 由 `docs.yml` 发布。
- 许可证 Apache-2.0（`LICENSE`）。