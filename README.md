# heartflow

[![license: Apache-2.0](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](LICENSE)
[![Rust](https://img.shields.io/badge/rust-1.88%2B-orange.svg)](https://www.rust-lang.org/)
[![docs](https://img.shields.io/badge/docs-mdBook-informational)](https://github.com/SantaChains/heartflow/tree/main/docs/src)
[![context7](https://img.shields.io/badge/Context7-enabled-blue)](https://context7.com/santachains/heartflow)
[![deepwiki](https://img.shields.io/badge/DeepWiki-ask-green)](https://deepwiki.com/SantaChains/heartflow)

Rust 实现的终端 AI agent。二进制命令 `hf`，在 REPL 中通过流式输出与模型协作，可执行 shell、读写文件、检索代码、挂载 MCP 工具，并以任务循环自迭代完成多步工作。

仓库：[github.com/SantaChains/heartflow](https://github.com/SantaChains/heartflow)

文档：[文档源 docs/src](https://github.com/SantaChains/heartflow/tree/main/docs/src) · [llms.txt](https://raw.githubusercontent.com/SantaChains/heartflow/main/llms.txt) · [llms-full.txt](https://raw.githubusercontent.com/SantaChains/heartflow/main/llms-full.txt) · [DeepWiki](https://deepwiki.com/SantaChains/heartflow) · [Context7](https://context7.com/santachains/heartflow)。Pages 文档站当前暂存为 `docs.yml.bak`，启用方式见 [docs/src/ai-integration.md](https://github.com/SantaChains/heartflow/blob/main/docs/src/ai-integration.md)。

## 特性

- 真流式架构：SSE 增量经 mpsc 通道推送，思考与正文实时渲染，markdown 与代码高亮输出
- 双协议接入：Anthropic 消息协议与 OpenAI Chat Completions 方言；内置 DeepSeek，自定义 provider 可接任意兼容端点
- 原生工具：bash、read_file、write_file、edit_file、glob_search、grep_search、search_files（nucleo 模糊文件检索，fzf 的非交互正解）、apply_patch（跨多文件事务式批量编辑，先全量校验再写入，任一 old_string 缺失/歧义则整批不落盘）、todo_write、ask_user、verify_graphics、web_fetch（SSRF 防护，仅 http/https，拒绝内网/回环/云元数据地址；HTML 抽取为保留标题/列表/代码/链接结构的 markdown，而非压成一行的文本墙）、web_search（免 key 的 DuckDuckGo 文本检索，返回排好序的标题/链接/摘要引用，供 agent 挑定后 web_fetch 展开）；write/edit/apply_patch 的输出用 `similar` 生成真正的行级带上下文 unified diff（非整文件 -旧/+新 转储）
- MCP 支持：JSON-RPC 2.0 双传输——本地 stdio 与远程 Streamable-HTTP/SSE（`[mcp.servers.NAME]` 给 `command` 走 stdio、给 `url` 走 HTTP，零新依赖复用已内置的 reqwest/tokio），`readOnlyHint` 标注或 `read_only` 配置声明只读工具，原生工具优先
- 任务循环：todo_write 登记计划，未完成任务自动续推，受最大续推次数约束。工具调度按读/写分类：连续的只读工具（read_file/grep/glob/search_files/web_fetch/web_search）并行批跑，写类与交互式工具（bash/write/edit/apply_patch/generate_image/todo_write/ask_user 及非只读 MCP）严格串行，保证写不会与并发读竞态、两个 ask_user 不抢终端
- Hermes 任务环：`/plan approve` 后逐任务执行，每个任务在新鲜上下文里跑（复用同一 runtime，仅重置会话消息、绝不重连 MCP），确定性 verify（无 judge，看工具错误与完成标记），失败按 重试→换策略→询问 升级，收尾落盘复盘
- 上下文工程：`>50%` 窗口预压缩——设 `config.toml` 的 `[provider] context_window` 或环境变量 `HEARTFLOW_AUTO_COMPACT_TOKENS`（= 模型上下文窗口 tokens，env 优先）后，回合内每次请求前若会话估算越过半窗即 summarize-then-compact（旧消息折成可续摘要、近若干条原样保留）；`/compact` 为手动强制压缩（忽略阈值立即压缩）。摘要按近期加权：越靠近存活窗口的轮次保留越多细节（每块 80→240 字预算）；`/pin` 把关键消息标记为永不压缩，逐字存活于每次压缩之后
- 会话持久化：每个对话一份权威 JSON 快照，原子（temp+rename）写入 `~/.heartflow/sessions/<id>.json`，每回合覆盖同一文件而非另存新档；resume/`/open` 沿用同一 `<id>` 原地续写，`/clear` 轮换到新 `<id>`。支持 compact；`/exit` 打印本段 resume 命令，`/open N` 在 REPL 内直接跳回历史会话
- 全文历史检索：每回合自动镜像进系统级 SQLite 库 `~/.heartflow/heartflow.db`（FTS5 trigram，中英文通吃，JSON 仍为权威存储），`hf search` 与 REPL `/search` 跨会话检索
- 自迭代记忆：`~/.heartflow/MEMORY.md`（或项目 `.heartflow/MEMORY.md`）作为跨会话的坑/决策/偏好记录，以极小 token（截断 4KB）注入系统提示词的 Memory 段；`/remember` 手动追加（去重），任务环遇硬坑（多次尝试失败被跳过）自动记录（非向量嵌入）
- Unix 管道组合：stdin 被管道时读入为上下文，`git diff | hf prompt "评审这次改动"`；`--quiet` 只输出答案、`--json` 输出结构化结果，方便脚本串联
- 外部 CLI 工具按需感知：系统提示词只广播主机上确已安装的非交互文本过滤器（jq/yq/gron/jc/rg/fd/tree/tokei/hyperfine/difft/xsv/gh），并约定首次使用前先 `<tool> --help` 学当前 flag 而非臆测；交互式/装饰性 TTY 工具（fzf、git-delta、less）归人类终端，不进 agent 提示（其能力已由原生 search_files 模糊检索与 apply_patch/真实 diff 覆盖）
- 配置热重载：REPL 每回合边界按 mtime 探测配置变更，自动重建 provider 并保留会话（零依赖轮询）
- 自检自愈：`hf doctor [--fix]` 校验配置解析、目录可写、provider 与密钥，并对历史库跑 `PRAGMA integrity_check`（库体过大时自动降级 `quick_check`）；`--fix` 建缺失目录、坏配置备份移开
- 权限模型：read-only / workspace-write / full 三档，工具级覆盖，REPL 内 /mode 热切换；/plan 规划模式（硬门禁：仅可写 `.heartflow/plans/*.md`，其余写/bash 一律拒绝），审批后进入 Hermes 任务环逐任务新鲜上下文执行，收尾把复盘写入 `.heartflow/reflections/`（可选沉淀为 `.agent/skills`）；read-only 与 plan 两档自动放行标注为只读的 MCP 工具（`readOnlyHint`/`read_only`），让远程只读 MCP 在受限模式下亦可用
- 健壮性：connect/read 双超时、子进程 kill_on_drop、UTF-8 全链路（BOM 剥除、PowerShell 编码前缀，非 UTF-8 字节经 `chardetng` 嗅探 + `encoding_rs` 解码 GBK 等遗留码页、不再 lossy 碎字、CJK 宽度对齐）、工具输出 32K 截断、缓存目录剪枝；工具入参执行前用 `jsonschema` crate 做完整 JSON-Schema 校验（draft 全能力；空/布尔/无法编译的 schema 一律放行，绝不误拦合法调用），转发给 provider 前对（MCP）schema 做规整（object 补 `properties`、array 补 `items`、单元素 `type` 联合折叠），MCP 握手（initialize+tools/list）失败按 200/400ms 退避重试 3 次（幂等），工具调用本身不自动重试（非幂等危险）交由模型层决策

## 安装

Windows amd64 便携包与 crates.io 包随每次发布自动产出,四条安装通道:

**scoop(Windows,推荐,带自动更新)**

```powershell
scoop bucket add heartflow https://github.com/SantaChains/heartflow
scoop install heartflow
```

**cargo(crates.io)**

```bash
cargo install heartflow
```

**cargo(直接从 Git,不经过 crates.io)**

```bash
cargo install --git https://github.com/SantaChains/heartflow --locked heartflow
```

**便携 zip 直下**:到 [Releases](https://github.com/SantaChains/heartflow/releases) 下载 `heartflow-<版本>-win-amd64.zip`(内含 `hf.exe`),解压后把目录加入 PATH。

## 构建

需要 Rust 1.85+。

```bash
# 克隆后在仓库根目录（含 Cargo.toml）执行
git clone https://github.com/SantaChains/heartflow.git
cd heartflow
cargo build --release
target/release/hf.exe --help   # Linux/macOS 为 target/release/hf
```

## 快速开始

```bash
# 方式一：环境变量（默认 anthropic 协议）
export ANTHROPIC_AUTH_TOKEN=sk-...

# 方式二：DeepSeek（OpenAI 方言，密钥走环境变量名）
export DEEPSEEK_API_KEY=sk-...
hf --provider deepseek
```

无参数启动即进入 REPL。输入基于 ratatui 内联视口（保留原生 scrollback、真实光标供 CJK/IME 候选）：Enter 发送，Shift/Alt+Enter 或 Ctrl+J 换行，输入 `/` 在下方弹出可选命令列表（↑/↓ 选择、高亮项按 Enter 或 Tab 补全、Esc 取消高亮；无高亮时 Enter 原样发送），空闲时 ↑/↓ 翻历史，Ctrl+C 取消当前回合（空闲行则仅清空），/exit 退出。命令列表需要高度随候选增减的内联视口，而 stock `Viewport::Inline` 高度构造时固定、一改即整屏 clear，故 `crates/cli/src/viewport_term.rs` 按 astrcodey/codex 的 resize-reflow 思路实现了动态高度行内视口终端（仅用 ratatui 公开 Backend/Buffer，零新依赖）；非 `/` 路径仍锁定 2 行，输入体验与既往逐像素一致。

> **进不去 REPL / 输出乱码？** 几乎都是终端环境问题而非程序故障。一是密钥只在别的 shell 会话里设过：在**当前**终端重新 `export`（Windows 用 `setx` 后要重启终端），再 `hf doctor` 复核 provider 与密钥是否解析成功。二是 Windows 控制台默认 GBK 代码页把 UTF-8 显示成乱码（库内字节始终正确）：执行 `chcp 65001` 或 `[Console]::OutputEncoding=[Text.Encoding]::UTF8`，并换用支持中文的等宽字体即可。

## CLI 用法

```text
hf [--provider NAME] [--model MODEL]              进入交互 REPL（等价 hf chat）
hf [--provider NAME] [--model MODEL] prompt TEXT  单次提问，流式输出
hf prompt -q|--quiet TEXT                         只打印答案（去掉进度/用量行，脚本友好）
hf prompt --json TEXT                             输出 {text, usage, session_id} 结构化 JSON
echo TEXT | hf prompt "指令"                       stdin 作为上下文与指令拼接（Unix 管道）
hf search QUERY [--limit N] [--json]              跨会话全文检索历史（非交互，可管道）
hf --resume[=SESSION.json] [--run /compact]       恢复会话（省略 PATH 进选择器），--run 恢复后立即执行 slash 命令
hf config export [--output FILE]                  导出配置（不含密钥）
hf config import FILE                             导入配置（自动备份 .bak）
hf doctor [--fix]                                 诊断环境（含历史库完整性）；--fix 应用安全修复
hf init [--force]                                 在当前目录生成 AGENTS.md 指令骨架（已存在不动，--force 覆盖）
hf --provider NAME models [--balance]              provider 自举：列模型、报当前模型上下文窗口；--balance 才查余额（省额度）
hf system-prompt [--cwd PATH] [--date YYYY-MM-DD] 打印系统提示词
```

## REPL 命令

```text
/help     帮助              /model [NAME] 显示或切换模型
/mode     [NAME] 显示或切换权限模式（read-only/workspace-write/full）
/status   会话状态          /compact      手动强制压缩会话历史（忽略阈值）
/pin      切换末条消息的永不压缩标记（跨 /compact 逐字存活）
/save     立即持久化        /clear        开启新会话
/sessions 列出已存会话      /open N       跳回第 N 个已存会话（同 /sessions 编号）
/remember T 追加一条长期记忆到 ~/.heartflow/MEMORY.md（自动去重）
/mcp      列出 MCP 服务器与工具
/search Q 全文检索历史      /exit         退出（自动保存并打印 resume 命令）
/init     生成 AGENTS.md 骨架  /expand [ID]  展开上次折叠的工具输出
/guide T  本地零 token 组装“前情/现状/下一步”三段引导草稿，供编辑后发送
/queue [pop|clear]  查看/撤回回合运行期间排队的后续消息（入队注入随 TUI 事件循环上线）
!CMD  不经模型直接跑一条 shell 命令取输出（Windows 走 pwsh，危险命令先确认，输出同工具一样可折叠/展开）
/plan GOAL 规划先行（写仅门禁到 plans/）  /plan approve 逐任务新鲜上下文执行+收尾复盘  /plan end 退出规划
```

## 管道与脚本

hf 遵循 Unix 过滤工具约定：当 stdin 被管道或重定向（非交互终端）时自动读入全文，与命令行指令拼接为一个提示词（`指令\n\n<stdin>`）；`--quiet` 只输出回答正文，`--json` 输出 `{text, usage, session_id}`，便于与 `jq`、`Select-String`、`git` 等串联。每回合自动镜像进 SQLite 历史库，`hf search` 因此可在管道里非交互地检索历史。

```bash
git diff HEAD | hf prompt --quiet "评审这次改动，只列风险点"
hf prompt --json "用三句话总结上面这段日志" | jq -r .text
hf search 中文笔记 --json | jq -r '.[].snippet'   # ≥ 3 码点走 FTS5 trigram，短词/中文回退转义 LIKE
```

## 会话历史库

对话以每会话一份 JSON 快照为权威存储写入 `~/.heartflow/sessions/<id>.json`（原子 temp+rename，一对话一文件）；同时 best-effort 镜像进系统级 SQLite 库 `~/.heartflow/heartflow.db`（镜像失败绝不阻断保存）。库采用 WAL、外键级联与 `user_version` 迁移，历史消息存于 `messages`、`messages_fts`（FTS5 trigram）供检索。中文、空格、Windows 全路径均按 UTF-8 正确处理。REPL 内 `/search <Q>` 与非交互 `hf search <Q>` 共用同一检索引擎；查询 ≥ 3 码点走 trigram 索引，更短的词回退到转义后的 `LIKE`。

镜像与会话文件共用同一个稳定 `<id>`（resume/`/open` 沿用、`/clear` 轮换），故一次对话在库中恒为一行、跨进程续写也归并到同一行，且随回合**增量追加**新消息（`append_messages` 仅写尾部），而非每回合重刷一份全量快照——把逐回合镜像从 O(会话长度) 降到 O(新增消息)。当回合使转录**缩短**（compact）或原地改动已镜像行（`/pin` 翻标志、任务环重置换种子）时自动回退全量重写；若发现镜像基数漂移（其他进程改写或库被重建），追加会在不动一行的前提下拒绝并即时回退全量。JSON 始终权威，被拒的追加从不丢失或重复历史，只是让位于已验证的重写。

因为库是从 JSON 派生的缓存，`hf doctor` 会对其跑一次结构体检（只读、非破坏）：默认用完整的 `PRAGMA integrity_check`（逐页/逐索引/含 FTS5 影子表），仅当库超 64 MiB 才降级为跳过索引交叉校验的 `quick_check` 以保持秒级响应。检出损坏时不会静默改数据，而是提示“JSON 转录仍为权威，删除 `heartflow.db` 即会在下次保存时重建检索索引”。

## 配置

优先级从高到低：CLI 参数 > 项目 `.heartflow/config.toml` > 用户 `~/.heartflow/config.toml` > 内置 provider 表 > 环境变量。同名字段逐项覆盖，坏字段跳过并告警，单条配置不阻断启动。

REPL 运行期间编辑并保存任一 `config.toml`，下一回合会自动热重载（保留当前会话与权限模式）。环境异常可用 `hf doctor` 诊断，`hf doctor --fix` 应用安全修复。

```toml
version = 1

[provider.deepseek]
protocol = "openai"          # "openai" 或 "anthropic"
base_url = "https://api.deepseek.com/v1"
api_key_env = "DEEPSEEK_API_KEY"
model = "deepseek-chat"
reasoning_effort = "high"      # 思考等级：openai 协议直传，anthropic 协议映射为 extended thinking 预算
context_window = 65536         # 模型上下文窗口 tokens，驱动回合内 >50% 预压缩（等价于 HEARTFLOW_AUTO_COMPACT_TOKENS，后者优先）

[provider.my-proxy]
protocol = "anthropic"
base_url = "https://my-proxy.example.com"
api_key_env = "MY_PROXY_TOKEN"

[mcp.servers.filesystem]
command = "npx"
args = ["-y", "@modelcontextprotocol/server-filesystem", "/tmp"]

[mcp.servers.remote-search]            # 远程/网络型 MCP：给 url 即走 Streamable-HTTP/SSE（无需 command）
url = "https://mcp.example.com/stream" # 与 command 二选一；两者都缺则跳过并告警
headers = { "X-Tenant" = "acme" }      # 额外请求头；值支持 ${ENV_VAR} 展开
bearer_token_env = "SEARCH_MCP_TOKEN"  # 从此环境变量读 Bearer token（为空则不发）
read_only = true                       # 声明该 server 工具均只读：read-only/plan 模式下也放行
```

### 终端配色（theme.toml）

配色以单一主题源为基准（Tokyo Night 冷色系，流式渲染与输入框共用同一色族），可按语义角色覆盖。文件位于 `~/.heartflow/theme.toml`（用户）与 `.heartflow/theme.toml`（项目，逐项胜出），与 `config.toml` 同优先级链。每个值取 `#RRGGBB`，坏值跳过并告警、绝不阻断启动；缺省沿用内置色板。改后重启生效（首次渲染时解析并进程级缓存）。

```toml
[theme]
heading = "#2ac3de"      # Markdown 标题
accent = "#7aa2f7"       # 提示符 / 活动 spinner / 补全高亮
muted = "#78829f"        # 次要文本（增量、空闲提示）
success = "#9ece6a"      # 完成
error = "#f7768e"        # 失败
# 其余可选：emphasis / strong / inline_code / link / quote
```

### 终端伴侣（mascot）

REPL 内置一个纯几何、零素材、零第三方动画库的小机器形象（`crates/cli/src/mascot.rs`），呼应 agent 状态：启动横幅用 **braille 点阵**（每格 2×4 子像素，与 ratatui `Canvas`/`Marker::Braille` 同源编码）绘出会呼吸/浮动的圆脸 blob，随节律眨眼；空闲输入框右端有一枚单行眼睛伴侣（终端过窄时自动隐藏，从不挤占输入）；工具运行的状态行用扫描表情替代通用 spinner。渲染借鉴 TermAVG 的「像素缓冲 → 终端格点编码」分层，动画由一个临界阻尼弹簧积分器驱动（与所参考的 JS 实现同一类数学），仅在时钟或状态变化时重算，配合 ratatui diff 使静帧开销近零。造型全为自绘几何/点阵，不含任何外部素材/商标，配色沿用上面的主题。半块真彩合成（更高分辨率）作为 ratatui 全屏重构（P4）的升级路径预留。完整的 `idle/thinking/busy/done/error` 状态机与多行彩色投影是就绪的公共 API，随 ratatui 状态栏（P4-c.3）接入后全量点亮。

## Agent 资产

仓库指令文件以 `AGENTS.md`（含 `AGENTS.local.md`、逐级向上到仓库根）为主注入系统提示词；仅当同目录没有 `AGENTS.md` 时才回退读取旧名 `CLAUDE.md`（排他回退，避免双注入）；`hf init` / REPL `/init` 生成中性的 `AGENTS.md` 骨架。规则与技能分用户层与项目层，项目同名覆盖用户。

```text
AGENTS.md / CLAUDE.md    仓库指令（项目上下文，逐级向上聚合）
~/.agent/rules/*.md      全量注入（按名排序，上限 32 个，单个截断 32KB）
~/.agent/skills/*/SKILL.md  仅注入 name/description 元数据，正文按需读取
.agent/...               项目层同构
```

## 环境变量

| 变量                                     | 作用                                                                                                                                                                                                       |
|------------------------------------------|------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| ANTHROPIC_AUTH_TOKEN / ANTHROPIC_API_KEY | 默认 anthropic 模式密钥                                                                                                                                                                                    |
| DEEPSEEK_API_KEY                         | 内置 deepseek provider 密钥                                                                                                                                                                                |
| HEARTFLOW_LOG                            | 日志级别门，默认 warn，输出至 stderr                                                                                                                                                                       |
| HEARTFLOW_PERMISSION_MODE                | read-only / workspace-write（默认）/ full，REPL 内 /mode 可切换                                                                                                                                            |
| HEARTFLOW_SHELL                          | 覆盖 bash 工具的 shell 程序                                                                                                                                                                                |
| HEARTFLOW_AUTO_COMPACT_TOKENS            | 模型上下文窗口 tokens；设后即启用回合内 `>50%` 预压缩（越过半窗 summarize-then-compact），未设则回退到 `config.toml` 的 `[provider] context_window`，两者皆无则关闭。用 `hf --provider NAME models` 查窗口 |

## Workspace 结构

```text
crates/
├── api       传输层：Anthropic/OpenAI 客户端、SSE 解析、重试
├── runtime   会话循环：流消费、工具调度、compact、系统提示词
├── tools     原生工具实现与注册
├── mcp       MCP 客户端（stdio JSON-RPC）
├── commands  请求/响应数据结构
├── store     系统级 SQLite 历史库：FTS5 全文检索、用量聚合、事务写入、best-effort 镜像
└── cli       hf 入口：REPL、配置、渲染、输入编辑
```

## 未来路线图

以下方向已明确规划但**尚未实现**，仅作路线记录：

- **Browser use / computer use**：驱动浏览器与 Windows 桌面操作（点击、输入、截屏、表单填充）的原生工具。
- **长期记忆**：基于向量/embedding 检索的跨会话持久记忆，区别于当前的 FTS5 全文检索。
- **DeepSeek Responses API + 原生联网搜索**：接入 Responses 协议与 DeepSeek 服务端原生 web search。
- **`hf logs` 子命令**：当前日志仅按 `HEARTFLOW_LOG` 走 stderr、不落盘；要支持一条命令直出历史日志需先引入文件 sink（`tracing-appender`）+ 存储目录 + 轮转策略，属新增子系统而非命令面修复，暂缓。
- **全局开关 `--no-confirm` / `--color`**：非交互危险命令的确认策略已在 `--help` 的 INTERACTION CONTRACT 文档化（默认沿用 `HEARTFLOW_PERMISSION_MODE`）；显式的 `--no-confirm`（拒绝而非放行）与 `--color=auto|always|never` 会改变安全/渲染语义，按需再评估。

## 质量门

日常迭代按效率优先：提交前保证下列四条绿。pedantic 级风格 clippy 仅作提示不阻断（一次性 strict 清扫已完成），但 `clippy::all` 正确性 lint 仍为阻断门。

```bash
cargo fmt --all -- --check
cargo test --workspace
cargo build --release
cargo clippy --workspace --all-targets -- -D warnings -A clippy::pedantic   # 阻断正确性；pedantic 仅提示
```

## License

本项目以 [Apache License 2.0](LICENSE) 开源。
