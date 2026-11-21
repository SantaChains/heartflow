# 路线图存档：先不做与研究借鉴

> 本文件收纳**当前不执行**的内容：外部项目调研与借鉴判决（B 组全部）、「明确不做」清单、次级评估项。主文件 [roadmap.md](roadmap.md) 只保留活动面。任何条目升级为活动时，移回主文件并在状态总览登记。

## B. 外部调研与借鉴（只记录不执行）

> B 组为外部项目源码研读与设计判据锁定，当前**不落地、只作 A 组的依据来源**。

### 2. 搜索与研究能力（web_search / web_fetch 增强）

**现状（代码实证）**：`crates/tools/src/web.rs:857` `web_search` 只有 DuckDuckGo HTML 单引擎、keyless；`web_fetch` 浏览器 UA + 5 跳重定向 + SSRF 护栏（`ensure_public`，`:409`）。无爬虫、无深度研究、无 provider 链。

**借鉴 MergeSearch（shaonianG，本地副本 `archive/pi-research/MergeSearch`，Python）**：

- 多平台聚合 MCP 服务四工具：`search` / `fetch` / `crawl` / `research`——**crawl 与 research 是 heartflow 缺的两档**；
- Key 池化 + 平台间 failover/轮询/随机 + 失败 Key 拉黑 5 分钟；
- `proxy_router.py` 是完整可移植的 Clash 风格分流引擎（`DOMAIN` / `DOMAIN-SUFFIX` / `DOMAIN-KEYWORD` / `IP-CIDR` / `REGEX` / `MATCH`，direct/auto/global 三模式）——**直接对应 §3 代理条目**；
- 用量统计持久化（`stats.json`）。

**借鉴 pi-web-access（nicobailon）**：

- 30+ provider 统一抽象 + `searchRouting` 有序路由 + `fallbackOn` 条件（如 `invalid-response`）；explicit-only provider 不进 auto 链；
- `maxInlineContentChars`（30K）结果预算——**搜索结果的「进上下文体量」必须是个显式预算，不是副产品**；
- 内容协商优先取 Markdown、`redirects: "same-origin"` 选项、`data:` URI 消毒（防 base64 图片占满上下文）；
- 凭证安全范式：错误信息脱敏、跨源重定向剥密钥、SSRF 全局防护。

**落点**：provider 链 + fallback（DeepSeek 服务端搜索已有协议口子，见原路线图）、`crawl` 工具（有界深度/页数）、结果预算参数。**明确不做**：视频理解、YouTube、浏览器渲染（归 browser use 条目）。
- **多引擎 + 爬虫 + 清洗设计（2026-10-24 网络研究定案，用户点名批次）**：
  - **多搜索引擎**：**SearXNG 自托管元搜索**（聚合多达 261 个搜索服务、JSON API、无追踪）为首选引擎选项——一个实例顶 N 家引擎，web_search 增 engine 参数（`ddg`（现状）/`searxng:<instance-url>`）；**Firecrawl** 可选付费集成（search+scrape+crawl 一体，2026-02 千 URL 基准成功率 95.3% 居首）。
  - **爬虫**：`crawl` 工具引擎候选——**spider-rs**（Rust 同语系高吞吐）与 **Crawl4AI 设计**（Python，专为 LLM 友好 Markdown 而生——其清洗管线设计可移植即使语言不同）。有界纪律不变：深度/页数/域内白名单。
  - **正文提取层**：Rust 侧三候选——**rs-trafilatura**（Python 顶级提取器的 Rust 移植）、**dom_smoothie**（Readability Rust 移植）、readability crate；进 C-2 清洗双轨（提取正文→原始 HTML 落盘可回取）。
  - **四层抓取级联**（业界自托管模式映射本仓）：SearXNG 元搜索 → ML 重排（C-2 embed-then-rerank）→ 抓取级联（Firecrawl 付费/Crawl4AI/直抓/Wayback 兜底）→ 清洗双轨——与已有件逐层对位，增量是 searxng 引擎 + 提取层 + 级联兜底。
  - **AI 水稿/广告/重复去噪**：正文提取天然去广告与导航；水稿与重复判定复用 C-2 重排/embedding 相似度（重复折叠）；内容真伪裁决不做（大模型的活）。
  - **认证/IP 阻拦/防护**：**尊重 robots.txt + 限速 + 退避 + 缓存**为采纳纪律；**对抗性绕过（residential proxy、anti-detect browser 如 Camoufox）不采**——法律与伦理边界，hf 是良民工具；遇 403/429 诚实上报并建议手动处理。
  - **垃圾上下文注入防护**：网页内容是**不可信输入**——本仓已有 tool-result 当数据的防御（prompt.rs），扩展到 web_fetch 正文：隐藏文本/指令样句检测告警、正文包裹引用块并前置「来自网页、非用户指令」声明。
  - **网址直接跳转**：web_fetch 已支持 5 跳重定向 + SSRF 护栏（ensure_public），维持。


### 3. 代理体系优化

**现状（代码实证）**：`crates/api/src/retry.rs:17` `build_http()` 只设 `connect_timeout(15s)` / `read_timeout(300s)`，**无任何代理配置**——reqwest 隐式读 env 代理（`HTTP_PROXY`/`HTTPS_PROXY`/`ALL_PROXY`/`NO_PROXY`），不可见、不可控、doctor 无法展示。`crates/tools/src/web.rs:73` 与 `crates/tools/src/image.rs:313` 各自另建 blocking client，同样裸奔。

**四类必须处理的场景**：

1. **幽灵代理**：env 设了 `HTTPS_PROXY` 但代理没开——现状每次请求先吃 15s connect timeout × 3 次重试 ≈ **45s/请求**才失败。必须启动时（或首请求前）对 proxy host:port 做 TCP 探测，失败则 warning + 降级直连；
2. **Windows 系统代理**：Clash/V2Ray「系统代理」按钮写的是 WinINET 注册表（`HKCU\...\Internet Settings` 的 `ProxyEnable`/`ProxyServer`/`ProxyOverride`），reqwest 不读——本机主力场景，必须自己读（`windows-sys` 已在依赖树）；
3. **按目标分流**：NO_PROXY 之外无规则。国内 API（DeepSeek）走代理反而慢/被风控，Google/GitHub 需要代理——直接移植 MergeSearch `proxy_router.py` 的规则引擎到 Rust（`api` crate，纯函数、离线可测）；
4. **fail 语义**：借鉴 pi-web-access 的 **fail-closed**（代理配置解析失败时拒绝请求而非静默绕过）；CLI 场景默认应对立成 fail-open 到 direct + 显著 warning，做成开关。

**解析顺序（设计）**：显式配置（`[proxy]` 节 / `HEARTFLOW_PROXY`）> env > Windows 系统代理 > direct。要显式化：`Client::builder().no_proxy()` 关掉 reqwest 隐式行为，自解析后 `.proxy(Proxy::all(...))`，这样顺序可控、`hf doctor` 可打印当前生效代理与判定理由。 socks5 由 reqwest  socks feature 覆盖，探测时按 scheme 分支。

**实现对照 MergeSearch `proxy_router.py`（heartflow-proxy 已落地，以下为差异判据）**：

- 规则引擎特征完全同构：`DOMAIN`/`DOMAIN-SUFFIX`/`DOMAIN-KEYWORD`/`IP-CIDR`/`REGEX`/`MATCH`，首条命中生效，闭包按目标分流（MergeSearch `proxy_router.py:13-107`，heartflow `crates/proxy/src/rules.rs:94-143`）；REGEX 两方都匹配**整 URL**（MergeSearch `:58` ↔ heartflow `rules.rs:86`），语义一致；
- **一处发散需定夺**：MergeSearch `auto` 未命中规则时回退 `auto_default`（默认 `direct`，可配，`proxy_router.py:78/:101`）；heartflow 取「有 rules 未命中→直连、无 rules→全局」。两方案均可，heartflow 当前选「无 rules 即全局」因为 `[proxy]` 显式地址本身即表达「要用代理」，空规则表罕见；若将来想让 `auto` 默认保守直连，加 `auto_default` 旋钮即可与 MergeSearch 对齐；
- MergeSearch 兼容旧写法（`*.domain` / 裸域名 / `re:` 前缀，`:62-71`）并预留 `GEOIP,CN`（忽略，:59）；heartflow 严格只认 `TYPE,VALUE,ACTION`，未知类型报错（更显式 fail-closed）。地理分流（GEOIP）作为未来可选项记录。

**与 easy-llm-cli 的沙箱出口代理正交**：easy-llm-cli 的 `GEMINI_SANDBOX_PROXY_COMMAND`（`docs/examples/proxy-script.md`，一个只允许 `example.com:443` 的 CONNECT allow-list 代理）管控的是**沙箱内部进程的网络出口**，与 heartflow §3 的「API 出站代理」是两层不同语义：§3 是 host→API 的 egress 选择，沙箱代理是 sandbox→egress 的 allow-list。若将来做 §4 沙箱，网络出口管控可复用此「用户自带 allow-list 代理脚本」模式，二者不冲突、可叠加。


### 5. 工具面与上下文预算

- **pi-tbox**：跨扩展的 toolset 开关/分组/focus + **上下文字符账本**（core floor vs extension budget 分开计数）+ 基线钉住（`defaults`/`save`/`restore`）。heartflow 工具已 15+ 且 MCP 全量下发，需要同款账本与 focus 模式；
- **pi-mcp-adapter**：单代理工具 ~200 token、MCP server 懒加载、自动探测/导入既有配置。与现有 `crates/mcp` 的关系是「替代方案」而非补丁——若 MCP 工具描述吃掉上下文成为实测问题，再评估；
- **pi-plan-mode / Plannotator**：plan 作为可审阅工件。heartflow 已有 `plan` 权限档（只写 `.heartflow/plans/*.md`）与 `/plan`，缺的是计划的可审阅呈现；
- **rpiv-ask-user-question**：结构化选项提问。heartflow 已有 `ask_user`，借鉴其选项渲染进权限门确认流；
- **pi-footer**：状态栏定制。低优先，现有 TUI footer 已够用。

#### 借鉴点子（easy-llm-cli 工具/上下文面，待落地 §5）

1. **BaseTool 契约（`docs/core/tools-api.md`）**：每个工具实现 `name`(API 内名)/`displayName`(人读)/`description`(给模型的说明)/`parameterSchema`(JSON Schema)/`validateToolParams()`(参数校验)/`getDescription()`(执行前给用户的「将要做什么」人读预览)/`shouldConfirmExecute()`(是否需用户确认)/`execute()`(带 `AbortSignal` 可取消)；返回 `ToolResult { llmContent, returnDisplay }` 把「喂给模型的历史」与「展示给用户的内容」分离（:7-19,61）。→ heartflow `crates/tools` 已是类似结构，可借鉴两点：(a) `getDescription()` 把「确认前的人读预览」从 `shouldConfirmExecute()` 拆出，权限门确认流更干净；(b) `llmContent`/`returnDisplay` 分离让诊断/回显不被塞进模型历史（与 §6 degradation ledger 同向）。
2. **ToolRegistry 双发现 + MCP 命名前缀（`docs/core/tools-api.md`）**：registry 既支持 `toolDiscoveryCommand`（产出 JSON `FunctionDeclaration` 注册为 `DiscoveredTool`）也支持 MCP 发现；多 MCP server 时工具名加 `serverAlias__actualToolName` 前缀（:21-27,71）。→ heartflow `crates/mcp` 已接 MCP，工具名前缀冲突规避可直接抄 `server__tool` 约定；命令式发现是「不写代码即加工具」的轻量扩展面，比 §4 的扩展系统更低门槛。
3. **memport：上下文模块化导入（`docs/core/memport.md`）**：`GEMINI.md` 内用 `@./file.md` 语法把大上下文拆成可复用组件；带**环检测**（:80-91）、**最大嵌套深度 10**（:99）、**路径白名单校验**（:93-95）、**只允许 `.md`**（:11）。→ heartflow 系统提示/项目上下文可借鉴：(a) 把巨型 system prompt 拆成 `@`-import 的模块化片段，按需组合；(b) 环检测 + 深度上限是防「上下文递归爆栈」的现成护栏，对应 §5 上下文预算；(c) 这与 §1 监工/工人「共享模块化项目上下文」也同构。
4. **token 缓存 + `/stats` 用量可见（`docs/cli/token-caching.md` + `docs/quota-and-pricing.md`）**：API key/Vertex 用户自动复用 system instructions+context 的 KV 缓存降低 token 成本（:1-14）；`/stats` 在会话中/退出时展示 token 用量与 cached 节省（quota-and-pricing.md:3）。→ heartflow §5 上下文预算需要「用了多少、cache 命中多少、哪个 provider 烧钱」的实时可见性，`/stats` 是现成 UX；token 缓存意识应进 provider 层（与 §4 第 7 条 compat 矩阵同源：哪些 provider 支持 prompt cache 是能力标志）。

#### 借鉴点子（kimi-code 一方 telemetry 包 + 用户自有 RAG 权限概念，待落地 §5/§4）

1. **RAG 权限校验（用户自有设计概念，**非** kimi-code——源码零命中，如实纠正归属）**：前置原则「**只要用户不能感知的内容，大模型也不可感知**」——权限校验应卡在检索**召回后、注入模型前**。做法：分块时为每 chunk 打 metadata `{ tenant_id, security_level, owner_id, allowed_groups, allowed_users }`；约定「仅当 chunk 的 `security_level` ≤ 用户风险等级才可用，否则丢弃」。**核实结论**：在 `.workbuddy/kimi-code` 全仓 grep `security_level` / `allowed_groups` / `allowed_users` / `tenant_id` / `clearance` 均**零命中**，故这是粘贴稿里的**用户自有设计构想**（或来自他处），不是 kimi-code 功能，不应挂其名。→ 但作为 heartflow 缺口仍高价值：`doc_search`（`runtime` 层 FTS5）与 §2 web_search 结果注入前，加一层**用户可见性门**（按 tenant/owner/group/security_level 过滤），防「把无权内容召回进上下文即等于泄露」。这是路线图目前完全缺失的维度（多项目/多用户场景），与 §4 可观测性、§6 编辑后反馈正交——**记为待自建设计，而非外部借鉴**。
2. **一方遥测包 + Observation/Eval 分层（源码实证 + 心智模型）**：kimi-code 有**一方遥测包** `@moonshot-ai/kimi-telemetry`（`packages/telemetry`，与 §4 easy-llm-cli 的 OTEL 同向，可作 heartflow tracing 的对标）；可观测性建议分两层——**Observation**（Trace：token use / context use / tool result 等指标采集）与 **Eval**（基于 Observation 的响应策略）。→ heartflow §4 遥测落地时不能只做采集面（Observation），必须同时定义**采集后要触发的动作**（Eval 层；此分层为心智模型、非 kimi-code 专有组件）：token 超预算→触发 compaction（呼应 §4 kimi/ds4 compaction 触发）、tool 连续 fail→降级或告警、provider 烧钱→切换。采集无响应 = 白采。


### 7. 推理引擎生态深研（DeepInfer + deepseek-recipe，2026-10-21 批次）

**结论先行**：DeepInfer 引擎本体（deepseek-ai，Apache 2.0，vLLM 深度魔改的 Python/CUDA/CANN 推理框架，Linux-only）对 heartflow——Rust 编写的 **API 客户端**——**无可直接利用的代码**；真正可「直接利用或深度适配」的是配套组件 **deepseek-recipe**（deepseek-ai，**MIT，纯 Rust workspace**，crates.io 已发布 0.2.0）：它做的三协议转换 + 精确 token 编解码 + 流式输出解析，与 heartflow `provider` / `api` / `compact.rs` 是同构面。引擎层剩余价值是方法论（见 7.2）。

#### 7.1 deepseek-recipe：首个「同语言 + 同协议面」的可评估组件（源码实证）

本地副本 `.workbuddy/deepseek-recipe`（`git clone --depth 1`，不入库）。workspace 成员（`Cargo.toml:2-11`）：`deepseek-recipe`（协议转换 + 输出解析）/ `-core`（conversation/message/image/tool 共享类型）/ `-encoding`（V4 与 V4.1 prompt 渲染 + token 编码）/ `-image`（OpenCV 预处理）/ `-macros`（`prompt!` 宏）/ `-python`（PyO3 绑定）/ `server-rs`（Axum 示例）/ `encoding-decoding-demo`（web demo）。edition 2024，dev toolchain 钉 1.97.1（`rust-toolchain.toml`），**未声明 `rust-version`**。

**与 heartflow 的逐项对照**：

| deepseek-recipe（源码实证） | heartflow 现状（代码实证） | 价值判断 |
|---|---|---|
| Messages / Chat Completions / Responses 三协议 → 单一 Conversation 格式（`deepseek-recipe/src/protocol/anthropic/messages/`、`openai/`），模型输出 → 各协议流式响应 | `crates/provider` 桥同三协议（Anthropic / OpenAI Chat / OpenAI Responses） | **直接参照或候选依赖**：自建桥接 vs 引入 recipe 二选一，评估项见下 |
| `tokenizers` 0.23.2 附加 tokenizer → 精确 token ID 编解码，与官方 API 行为**完整对齐**（README + 部署 QA「DeepSeek Recipe」节） | `compact.rs` 字符启发式 + `TokenCalibration` 仿射校正（实测中位误差仍 12.0%） | **精确计数替代估算**：compaction 触发、上下文上限检查、usage/reasoning_tokens 统计可从「估算」升「精确」 |
| `resolve_max_tokens`：未指定时按 thinking mode/effort 给 8192 / 65536 / 131072，prompt+max_tokens 超 `context_limit` 返回 `ConversionError`（`docs/tokenizer.md:78-87`） | provider `max_tokens` 缺省全局常量 4096（`config.rs`） | 语义参照：缺省值应随模型能力（thinking/effort/context）而非全局一刀切 |
| **special-token 注入防护**：渲染记录 `special_ranges`，特殊 token 位置直接映射 token ID，普通文本段**不带 added vocabulary** 编码；用户输入里拼写的 `<suffix>` 等保持普通 token，无法充当控制 token（QA :300-304 / tokenizer.md:65-76） | 系统提示词自建（`prompt.rs`），无显式 special-token 边界管理 | **方法论可抄**：heartflow 的 prompt 模板若含控制标记，同样需要「显式登记 + 边界编码」防注入 |
| **分段并行 tokenization**：特殊 token 边界已知 → 段间独立并行编码（`tokenizers` 的 `TOKENIZERS_PARALLELISM`，QA :304） | `estimate_tokens_from` 增量 O(1) 估算 | 精确化之后，长上下文计数的延迟对冲手段 |
| **流式解析状态机**：跨 chunk UTF-8 缓冲、增量 KMP 匹配分隔标记、reasoning/body/tool-arg 阶段状态、确定即发射；special token 按 **token ID** 识别（`</think>` / DSML），普通 token 拼写的同文本只是内容（QA :306-312） | 「流式两路径不按 delta 重解析」约束 + SSE→事件自建解析 | **设计参照**：heartflow 的 SSE 解析可对照其「不重扫早文本、确定即发射」结构 |
| `ReqwestImageFetcher` 默认连任意地址，官方文档自陈须自装 `DestinationFilter`（README:52） | `web_fetch` 已有 `ensure_public` SSRF 护栏 | heartflow 现状更稳；recipe 属「默认不设防、文档交底」，是护栏重要性的反面参照 |
| V4 不收图片 → 转换期把每个图片替换为 `[Unsupported Image]` 文本（README:49-50） | provider 能力差异散落 `if provider == x` 补丁（§4 第 7 条） | 同族：能力降级应在协议转换层显式完成，而非请求构造处打补丁 |
| `server-rs`：Axum，`/v1/{chat/completions,responses,messages}` 三协议 JSON+SSE，loopback:7777，mock inference，无鉴权且图片抓取不过滤内网（README 自陈） | 用户本机已有 Claude Code relay（127.0.0.1:15721） | 生态参照：本地多协议 relay 的现成骨架；heartflow 自身不做 server，记录备查 |

**依赖评估（未来若评估引入，须先验证，当前不跑 cargo）**：① recipe 未声明 `rust-version`，其 edition 2024 + 依赖树是否突破 heartflow MSRV 1.95 需实跑验证；② `tokenizers` 是重量级原生依赖，Windows 构建体积与 CI 时长成本需量化；③ tokenizer.json 获取路径：HF Hub 运行时下载（http feature）vs 随仓库分发——按本仓 vendored 纪律，未修改上游资产不入库，但「clone 即可跑」检验法要求有确定获取路径，二者需折中；④ 引入属**外部依赖**而非衍生代码，按 AGENTS.md 第三方纪律在 README §致谢点名（NOTICE 仅针对衍生/移植代码）；⑤ 更轻替代：只借鉴 special-token 边界编码与流式状态机方法论，tokenizer 单引 `tokenizers` crate 自接。

#### 7.2 DeepInfer 引擎：方法论借鉴（代码不可用）

**不可用判决**：Python 3.12 + vLLM fork + CUDA 13 / CANN 9.2 + Linux + GCC≥13；硬件限 NVIDIA SM100 与昇腾 950；模型限 DeepSeek-V4.1-Flash（默认 1017 checkpoint）。heartflow 是跨平台 Rust 客户端，零代码可复用。但同仓 `doc/inference_deployment_qa.md`（官方部署 QA）暴露的设计逻辑有多条可迁移：

1. **PD 分离原则**（:11/:31）：prefill（8 卡机）与 decode（64–128 卡超节点）是两种异负载，计算/通信/调度分别设计。→ heartflow：会话循环里「ingest（解析/压缩/replay 投影）」与「generate（流式消费）」也是异负载，P2 并发模型应分别定策略，而非一个 loop 均匀摊。
2. **DSpark 置信度调度验证**（:243-256）：draft 一次 5 token，Confidence Head 预测各 draft token 接受率，按边际收益选验证子集；官方表格**诚实给出负增益区间**（16 并发 −1.5%，32 起转正 +1.6%~+11.0%）。→ 方法论：推测式优化必须附「什么工作点收益为负」的实测表；heartflow 的乐观路径（并行只读工具批跑已有）同样该有工作点数据。
3. **SWA bounded replay**（:183-187）：重算成本压成常数（只重放最后 128 token），与上下文长度/层数解耦；decoder 侧近似重建「只用于 decode、绝不进前缀缓存」。→ 对应本仓 `estimate_tokens_from` 增量重估与「压缩后只补后缀」；bounded replay 是「廉价恢复」的通用模式。
4. **batch-invariant / deterministic sampling**（README:27-28，仅 NVIDIA）：同输入同参数同输出。→ heartflow 的客户端对应物是 cassette 回放（保真 AgentEvent 而非 wire）——可复现性是调试基础设施，不是可选特性。
5. **KVCache TTL 按流量分流**（:153）：agent 流量 3 天、chatbot 7 天，代价量化（chatbot 约损 0.5–1% 命中率）；计算节点故障率比存储节点高一个数量级，故缓存外置专存。→ heartflow：会话/transcript/cassette 保留策略应按用途分流并量化代价，不搞一刀切。
6. **优雅停机**（:199）：CE 收到停机通知后 1 分钟 grace，未完成请求连同 KV 一次性转发给其他 CE 续解。→ 对应 §1 队列/多会话前置条件「在飞回合必须能被事件循环够到」与 job-listener 会话外存活：停机时在飞回合要可交接，不是硬杀。
7. **容量纪律**（:203）：quota 利用率 >0.8 禁止缩容，在线实例数恒大于计算需求。→ heartflow P2 单写者 World 的拒绝并行启发：高负载时拒绝新并行任务，而非均匀 degrade。
8. **3FS 存储纪律**（:77-91）：20×30.73TB SSD/节点、≥40GB/s 读 + 5GB/s 写、10+2 EC（任意 10/11 可解码）、GC Worker LRU 水位回收。→ `store` crate（现 SQLite）若走向大规模历史的工程参数参照（记录，非计划）。
9. **数字诚实**：155T token/日、$38/GPU-hour 均为**自报、无第三方复现**（QA 文档口径，多家媒体转引亦标注）。→ 与本仓「量化驱动决策与诚实基准校准」同律：外部性能数字未经独立复现前只作参考、不作目标。

**明确不做/不可用**：引擎代码、kernel 库（FlashMLA/DeepGEMM/DeepEP/TileKernels/DeepSelect）、MoE EP/EPLB、PD 分离部署、KVCache 存储与负载均衡（官方明示未开源）——全部是服务端 GPU 栈，heartflow 不涉足。heartflow 与 DeepInfer 生态的真实交集只有两个：**deepseek-recipe 的协议/tokenizer 面**，以及「客户端如何对齐官方 API 行为」这一命题。


### 8. bash job 监控 / job 超时 / 挂起检测（2026-10-21 网络调研批次）

**调研来源**：`codexmon`（tigercosmos/codexmon，Go 写的 codex CLI 监工：三层超时 + 健康分档）、IdleWatchdog 参考实现（gunbark.dev 两篇 deep-dive）、counterpoint agent（mozart-orchestration `COUNTERPOINT.md`，外部 CLI 防挂纪律）、`agentic-process-monitor` skill（sentinel+heartbeat+子代理轮询）、timetobuildbob「The Session Wasn't Idle — It Was Waiting」（子代理感知的会话看门狗）、Claude Code BashTool 内部机制拆解（CSDN oe1019/159826055：超时自动后台化/进度回调/大输出落盘）。队列与引导的官方语义对照（Qoder docs / ACP steering）已并入 §1 落地状态。

**核心判据（多源一致的活性观）**：
- **「进程活着」不是活性证据**——counterpoint 的 canonical multi-hour codex stall：两个 run 在 0.1s CPU 上静坐数小时，PID 检查永远健康；真正的活性信号是**输出到达**（codexmon 按 lastActivity 分档、IdleWatchdog 按流事件 beat()、agentic-process-monitor 按 heartbeat 文件 mtime）；
- **硬顶必须武装在启动时刻**（counterpoint："A cap that exists only in your polling plan is not a cap"）——靠轮询纪律兜底的超时不是超时；
- **完成后≠成功**——出口码必须可查（Claude Code run_in_background 三坑之一），本仓 sidecar `status.json` 的 `exit_code/success` 字段即此判据的落地。

**落地状态（本批次已实现，208 runtime 测试全绿 / clippy 干净 / panic 棘轮 debt=0）**：

1. **job 墙钟超时 ✅**（§1 已列）：30 分钟默认、env 覆盖、Running 翻转时武装、首终态裁决获胜。
2. **bash 前台空转看门狗 ✅**（`crates/runtime/src/bash.rs`）：`HEARTFLOW_BASH_IDLE_TIMEOUT_MS`（默认关——安静的长构建会被误杀，opt-in 才诚实）；心跳 = 流到达时间戳（`read_stream_bounded` 每个 chunk 打 `beat`）；到期按 pid 终止（Windows 复用 `taskkill /T` 树扫、Unix `kill -9` 直子——与整体超时路径同一限制，诚实记录）；结果类别 `idle_timeout` 与墙钟 `timeout` 分开（同一 `interrupted` 契约 + 不同的 `return_code_interpretation`，模型可同路反应、下游可分组统计）；**静默前的部分输出保留为证据**而非清零（killed_output 构造器统一两条 kill 路径的字段，防漂移）。**后台路径刻意豁免**：安静长跑正是 run_in_background 的用途，其终态由 sidecar 报告。
3. **stdin 纪律 ✅（生来合规，记录免重查）**：counterpoint 头号纪律——交互型 CLI 后台运行时开着 stdin 会在「等一个永远不来的输入」上无限阻塞，而进程表观完全健康；本仓两条 spawn 路径（前台 tokio / 后台 std）都已是 `Stdio::null()`。

**待落地（只记录不执行，按依赖排序）**：

1. **codexmon 第二层：单工具卡死超时（ToolStuckSec）**：区分「模型在思考」与「某个 MCP/工具调用卡死」——需要工具面 in-flight 跟踪（item.started/completed 事件），前置 = P2 并发模型的事件可见性。
2. **空闲分档（Healthy/Slow/Stalled）**：现在只有「挂/没挂」两态；codexmon 的 Slow 档（30s 无事件）适合喂给 TUI 进度行（§1 D2.6「对话栏留一行显示大进度」），Stalled 档才升级为杀。
3. **CPU 时间作为第二活性信号**（counterpoint：输出增长为主、`ps -o time` 增长为辅；连续两轮 CPU 不动 = 挂死而非思考）：仅在真实误杀报告出现时评估，避免为罕见场景引入每命令的 CPU 采样成本。
4. **超时自动后台化**（Claude Code BashTool 机制：前台命令到点不杀而转后台 + 通知，另有 >15s 助手自动后台、>2s 进度回调）：与「杀了重跑」相比保住已完成的工作；但改变 `interrupted` 语义并与 `no_output_expected` 联动，属流式路径相邻改动——**先录 cassette 再动**（本仓铁律）。
5. **子代理感知的会话看门狗**（timetobuildbob）：平面墙钟会杀死「正在等三个并行子代理」的监工——需要 `subagents_in_flight` 计数器，有子在飞则抑制空闲击杀并重置时钟；前置 = §1 mission 账本 + P2。
6. **job 结构化心跳的误报陷阱**：pi-subagents 的 5s liveness sweep 不能直接套——`WorkerRunner` 在 LLM 调用期间天然沉默（无 emit），按「心跳缺失即挂」会全量误报；正确粒度是**回合/工具调用级 beat**（每完成一个工具调用打点一次），依赖 P2。

**业绩标准（工程验收，自本批次起逐项核对）**：

| # | 标准 | 现状 |
|---|---|---|
| 1 | 任何 running job 必有三种终局之一（complete/failed/stopped，含超时与进程死亡对账）——不允许「永远 Running」 | ✅ 墙钟超时 + 启动对账 |
| 2 | 挂起判定必须有活性证据（输出到达/emit 到达）；「进程活着」永不作为活性证据 | ✅ beat 心跳 |
| 3 | 超时裁决先于结果裁决且不可覆写（首个终态获胜） | ✅ terminalize 幂等 + 测试钉死 |
| 4 | 杀进程必须诚实报告死因类别（timeout / idle_timeout 分开），部分输出保留为证据 | ✅ killed_output 构造器 |
| 5 | Windows 杀树必须真机验证 | ✅ `timeout_sweeps_descendants` 测试 + idle 路径复用同一 `taskkill /T` 机制 |
| 6 | 前台/后台分治：空闲看门狗只管前台，后台长任务以 sidecar 终态为准 | ✅ 后台豁免 |

**明确不做**：常驻监控 daemon（看门狗随命令/任务生命周期生灭）、跨机器 job 监控、基于 GPU/内存的启发式挂起判定（噪声大于信号）。


### 9. 调度与加速理论（2026-10-22 批次，只记录不执行）

**诚实定位**：heartflow 是远程 API 客户端，**硬件榨取（GPU/张量核/带宽）不适用**（ds4 判断已记录）；「极致调度与加速」的可适用面是**回合事件循环与渲染路径**的调度理论。理论储备，按「先埋点观测，有证据再改」推进，未到执行位：

| 方向 | 理论 | 现状与判据 |
|---|---|---|
| 全局分配器 | mimalloc 已是全局分配器（release lto=fat + cgu=1） | 已落地；后续优化先测分配热点再动手 |
| 流式渲染 | 两路径均不按 delta 重解析（非 O(n²)）；md 渲染一套 IR 两投影器 | 已落地；已知未修：TUI 流式尾部每帧 clone、表格列宽不按终端封顶——改动须端到端冒烟 + cassette |
| 事件循环调度 | P2 并发模型（单写者 World + 线程池快照 + token 取消传播）是「在飞回合可被够到」的根；io_uring 式批量唤醒、work-stealing 属其下游 | 前置 P2；未观测到调度瓶颈前不动 |
| 底层榨取取舍（2026-10-24 数学核验批次） | CUDA/asm/GPU 驱动层优化 | **不适用判决维持**：heartflow 是远程 API 客户端（ds4 判决），热路径是字符串估计与终端 half-block IO——无可压榨的张量/GPU 热循环；SIMD/手写 asm 的适用前提（数值密集内循环）在本仓不存在。激进优化的正确对象是测量驱动的分配与 IO（基准片先行）。 |
| 估计器常量数学核验（同批次） | chars/4（ASCII）+ 1 token/字（CJK） | **双重证实，不调参**：文献锚定英文 ~4 chars/token、DeepSeek 系双语分词器 CJK ≈ 1 token/字（UTF-8 3 字节/字 → 字节除法会低估混合中文——现公式的 per-char 分支正是为此）；已录真实夹具实测（系统提示+工具+消息全口径）：估计误差 **-2.0%/-2.2%/-4.1%**，远优于跨负载 12% 中位（内容方差所致）。TokenCalibration 继续吸收残余。 |
| 压缩阈值安全裕度（同批次） | 软触发 85% × 校准误差界 | **闭环**：85% × (1+12% 误差上界) ≈ 95.2% < 100%——软触发有裕度但不厚，D4.2 硬触发恰补余量（400 拒绝 → 压缩重试一次），两层相加构成数学上闭合的过冲防护。半窗默认同理（50%×1.12=56%）。 |
| 弹簧积分器稳定性（同批次） | 临界阻尼显式积分 | **稳定域内**：dt=0.06s、呼吸周期 2.6s → ω·dt ≈ 0.145，深居显式 Euler 稳定区（ω·dt < 2）；既有收敛测试（无振铃过冲）钉死数值行为。 |

| 基准套件 | hyperfine 外部基准（启动/冷回合/渲染吞吐）+ 内建 bench | **已建第一片（2026-10-24）**：`cargo bench -p heartflow-runtime`（零依赖 harness：warmup+中位数，真实夹具输入——合成数据会漂）。首批数字：全量夹具扫描 31.6µs/9105 tokens、**增量尾 <50ns（摊销 O(1) 设计声明的数值验证）**、混合 CJK 1MB 0.18ms。后续热点（markdown/渲染缓存）随 cli lib 化或按需补 |
| IO 批处理 | 队列/mission/schedule 三落盘点都是同步小写（<256KiB）；批处理收益为负（延迟换吞吐，错方向） | **不做**，除非观测到 fs 延迟成为回合边界瓶颈 |

**明确不做**：常驻监控 daemon（看门狗随命令/任务生命周期生灭）、跨机器 job 监控、基于 GPU/内存的启发式挂起判定（噪声大于信号）、RAM 快照缓存掩盖协议不匹配（ds4 反模式）。


### 10. PandaCode / pi / codex / claude 设计借鉴与取舍（2026-10-22 研究，只记录不执行）

**研究来源**：`aicodeking/pandacode`（Tauri 2 桌面外壳，Rust 后端 + TS 前端，369★/53 fork，4 天新仓；浅克隆于 `archive/pandacode`，**不入库**）。它把 Claude Code / Codex / Kimi / OpenCode / GLM / MiMo / Antigravity / Grok（ACP）/ Pi（RPC）/ DeepSeek（ACP）等编码 harness 归一化在同一事件面背后，自带 **MCP host**（`src-tauri/src/providers/mcp_host.rs`）、**Pi RPC 桥**（`providers/pi.rs` + `providers/pi-extension.mjs`）、**队列/引导**（`message_queue.rs` + `providers/steering.rs`）、**architect+workers 团队**（`architect.rs` + `subagents.rs`）。交叉佐证已落地的 `archive/pi-research/pi-subagents` 与 `.workbuddy/` 的 ds4/easy-llm-cli/kimi-code/DeepInfer/deepseek-recipe。

**诚实定位**：pandacode 是 GUI 桌面 app，heartflow 是 **CLI agent**——GUI 专属面（webview 主题、simulator、bots、deploy、overlay）**直接不适用**，只取它验证过的**架构骨架**：① 归一化 provider 事件面；② 每回合 ephemeral 进程 + 会话 resume；③ loopback MCP host（host 暴露工具给 guest harness）；④ host 桥接 guest（Pi RPC）+ 权限模式在 host 强制；⑤ durable 每任务队列 + 引导直接发送的生命周期二分。下面按「采纳 / 改造 / 不做」给结论，全部带 pandacode 行号实证，并对照 heartflow 真实状态（`crates/mcp` 纯 client、`FollowUpQueue` 单 FIFO、无 Pi 集成、`PermissionPrompter` 已存在）。

#### 10.1 归一化 provider 事件面（采纳骨架，不抄实现）

pandacode 把 9+ 种 harness 的差异收敛到一条 `AgentEvent` 流（`kind`: text/tool/approval/done/error/session/subagent/goal…，行号见 `docs/ipc.md` L177-223），每个 adapter 只产出归一化事件；`providers.rs` 即「one normalized event interface」（`docs/providers.md` 开头）。heartflow 已有 `provider` crate，方向一致——**保持 provider 抽象干净、事件面单一**，新增 harness 只写 adapter 不出新事件种类。**明确不抄**：pandacode 为每 harness 写独立协议解析器（Claude stream-json / Codex app-server / ACP / Pi RPC），heartflow 初期只需 1–2 个 harness，不必全支持。

#### 10.2 每回合 ephemeral + 会话 resume（采纳原则）

`docs/design.md` L7：「Spawn provider processes only for active turns and terminate when complete; resume through provider session IDs.」`docs/providers.md` L127-132：每回合独占一进程，完成即关 stdin 释放；下一回合用保存的 `session_id` resume；Stop = harness 中断消息 + grace 后杀进程组。这**正面回应** heartflow 的「在飞回合必须能被事件循环够到」根因（tui.rs:1889 内联 await 停住循环）——**不要让长生命周期 agent 进程卡住事件循环**，靠 session resume 衔接而非常驻。与此前 §1/§4 的 pi job 磁盘状态机、`run_one_turn` 边界推送同构。

#### 10.3 mcp：client→host 的取舍（核心升级点）

heartflow `crates/mcp` 当前是**纯 client**（stdio + Streamable-HTTP/SSE 双传输，`http.rs` `HttpTransport`；`README.md` 自述）。pandacode `mcp_host.rs` 是**host 侧**：每回合起一个 **loopback-only、随机 Bearer token、鉴权强制** 的极简 MCP server（`Server::start_with_approvals` 绑 `127.0.0.1:0`，token=双 UUID；`read_request` 拒 `Origin`/非 loopback `Host`/超 160KB body/非 POST/旧协议版本，L309-333），把 URL+token 按各 harness 原生格式注入（`claude_config`/`pi_config`/`kimi_config`/`codex_config`，L125-136）。

| 取舍 | 判据 | 结论 |
|---|---|---|
| **client 传输安全化** | heartflow 也连本地 MCP server（loopback），应校验 endpoint 为 loopback、优先 Bearer、绑帧上限 | **采纳（改造）**：在 `HttpTransport`/`client.rs` 加 endpoint-loopback 校验 + 帧上限；不引入 pandacode 全套 server，只取 client 侧纪律 |
| **新增极简 MCP host server** | 若 heartflow 要「code mode」驱动外部编码 agent（Pi/Codex），或要可被外部编排器驱动，host 是把本仓工具（file/git/run/jobs/missions）暴露给 guest 的通道 | **采纳（改造，按需）**：照 `mcp_host.rs` 约 300 行 `Server` 实现一个 per-turn loopback host（Bearer + DNS-rebinding 防护 + 一连接一请求 + 信号量 8 + `notifications/cancelled` 取消 + 单次使用 approval），不抄其 `host_tools` 全套，只暴露 heartflow 自有能力 |
| **rebind 暖进程** | pandacode 暖 agent 先占位 server、adopt 时 rebind 到真回合（同地址同 token，agent 已持有，L52-58） | **暂不做**：heartflow 无暖进程预热需求，留作 P2 并发模型下游 |

#### 10.4 pi 的 codemode：host 桥接 guest + `agent_settled` 生命周期（核心取舍）

**关键结论**：pandacode **不嵌入 Pi 逻辑**——以 `--mode rpc` 把 Pi 当外部进程驱动，经 `--extension` + `PANDACODE_PI_BRIDGE` 注入桥扩展（`pi-extension.mjs`），由 host **强制权限模式**（ask/auto/plan/full），Pi 自身无审批系统（`README.md` L158；`pi-extension.mjs` L44-55：`tool_call` 钩子按 mode 放行/block，`plan` 只放行只读 + `PLAN_APP_TOOLS` 白名单）。这正面回答「pi 的 codemode」：**heartflow 的 code mode 不应 fork/嵌入编码引擎，应走 host-bridges-guest**——heartflow 当 host 持有工具与权限，Pi/Codex 当 guest 经 loopback MCP 回调。

**生命周期二分**（直接修 heartflow `run_one_turn` 活回合守卫）：pandacode `pi.rs` L301-311 —— **`agent_settled`（非 `agent_end`）才是真终态**：重试/compaction/steering 可跟在 `agent_end` 之后；`agent_settled` 才推 `get_session_stats` 并 `done`。heartflow 若把回合结束等同于「子进程退出/末消息」，会漏掉 settle 后的 steering/retry。**采纳**：把「回合真终态」判据从「子进程退出/末消息」改为「host 侧 settle 事件」。

| codemode 取舍 | 判据 | 结论 |
|---|---|---|
| 嵌入编码引擎 vs 驱动外部 agent | 嵌入=维护成本 + 与上游漂移；驱动=复用其登录/沙箱/模型，host 只负责工具+权限 | **采纳 host-bridge-guest**（与 §1 pi-subagents 一致） |
| 权限模式在 host 强制 | Pi 无审批；pandacode 在桥扩展按 mode 拦截，guest 不知情 | **采纳**：heartflow 已有 `PermissionPrompter`（`crates/cli/src/interact.rs`），code mode 复用之，不在 guest 内重造 |
| `agent_settled` 真终态 | pandacode 实测 `agent_end` 后可仍有 retry/steer | **采纳**：修 `run_one_turn` 终态判据 |
| Pi RPC 的 `steer` 命令 | `pi_steer` 发 `{type:"steer",id,message}` 按 id 回执；`phase==0||settled` 拒收（`pi.rs` L432-448） | **采纳**到 §1 队列三件套「引导直接发送」分支（per-harness 投递：Claude 注入流输入 / Codex `turn/steer` / Pi `steer`） |

#### 10.5 队列 / 引导二分（升级 heartflow FollowUpQueue）

pandacode 把「引导（steer，进运行回合）」与「排队（queue，回合后 durable）」严格二分（`message_queue.rs` L1-4）：`queued_messages` 是 **per-task、position 有序、可 reorder/update/remove、take 一次性投递、失败 restore 回队首**（`take` L128 + `restore` L137，exactly-once），上限 50/任务、文本 128KiB、重启/失败/stop 后队列仍在。heartflow 当前 `FollowUpQueue` 是**单 FIFO 上限 32、整队合并一条注入**（`crates/cli/src/core.rs:19-107`，§1 L120 已记录）。

| 取舍 | 判据 | 结论 |
|---|---|---|
| durable per-task 队列 + reorder | pandacode 验证：partial reorder 拒绝（L117-119）、take 后不可再 take（L241）、失败 restore 保 id/时间（L137-148） | **采纳（改造）**：把单 FIFO 升级为 per-task 有序 + 编辑 + 撤销（§1 D2.x 决策点落地） |
| steer vs queue 二分 | harness 能否 mid-turn 接受决定走 steer 还是 queue（`steering.rs` L26-63） | **采纳**：heartflow 引导直接发送须 per-harness 分支，不能收编为单一注入 |
| 上限与边界 | 50/任务、128KiB、附件 1MiB、空文本+空附件拒收（L28-40） | **采纳数值纪律** |

#### 10.6 资源纪律（采纳原则）

pandacode 的「无递归文件 watcher、无后台轮询、bounded transcript 页（100 消息/2MiB）、tool 快照上限 64×8KiB、暖进程至多 2 个等待 10 分钟」（`docs/design.md` L46-48、`docs/providers.md` L238-250）。与 heartflow「性能→流畅→美观」「最小改动不回归」一致。**采纳**：任何新增 harness/子代理/queue 都须带显式上限与惰性加载；**不做** pandacode 的 GUI 专属面（simulator/bots/deploy/overlay/webview 主题）。

#### 10.7 明确不做

- 不抄 GUI：webview 主题、iOS/Android simulator、Telegram/Slack/Discord/WhatsApp bots、Vercel/Cloudflare/Railway/AWS 部署、quick overlay——CLI 不适用。
- 不全支持 9+ harness 协议解析器；heartflow 归一化面 + 1–2 harness 即可（避免 pandacode 为每 harness 写独立 parser 的维护面）。
- 不为 code mode 嵌入编码引擎（fork Pi/Codex 内部逻辑）；坚持 host-bridge-guest。
- 不引入 pandacode 的 `host_tools` 全套（team delegate/browser/moodboard/VM），只暴露 heartflow 自有能力。


#### 10.8 Microsoft Edit（archive/edit，2026-10-24 深挖，只记录不执行）

微软的 Rust TUI 编辑器（MS-DOS Editor 致敬，MIT）。**自研即时模式 TUI 框架**（非 ratatui）：`framebuffer.rs` 保留上帧 + `last_attr ^ attr` 位掩码属性差分 + 最小 VT 发射——与本仓 CompanionTerminal「只写变更格」+ render_cache 同设计族，**互相验证而非互抄**；其 `TODO: properly diff contents`（:516）说明连它都觉得字符级差分可再进一步。

| 面 | 判决 | 依据 |
|---|---|---|
| unicode/measurement.rs：UAX #11 East Asian Width + **ambiguous-width 可配置**（进程单次设定，`static mut`+SAFETY 注释——本仓 forbid unsafe 会用 OnceLock） | **采纳（改造，低优先）** | CJK/IME 宽度的正确模型。本仓 CJK 宽度对齐已做，ambiguous 宽字符（·×°等按语境 1/2 格）是精化位；实现载体现成——unicode-width crate 已在依赖树，无需自研表 |
| simd/（lines_fwd/bwd、memchr2、memset：MB 级缓冲的 SIMD 行扫描） | **不做** | 适用前提是编辑器级巨型缓冲；本仓工具输出 32K 截断、markdown 按段解析，基准已证热点在 µs 级——无该形状的热循环 |
| fuzzy.rs（自研模糊匹配，提及 Sublime/fzf） | **已有** | 本仓 nucleo 覆盖同面 |
| icu.rs：**dlopen 动态加载 ICU** + 优雅降级 | **记录（方法论）** | 可选原生依赖的第三条路（静态链接/运行时加载/不依赖）——直接关联 §7.1 recipe 评估（tokenizers 重依赖的替代形态参考） |
| i18n/edit.toml（FTS 本地化） | **记录不做** | CLI 单语现状，等真实多语言需求 |
| fuzz/ 目录 | **已同构** | 本仓抗冲击三 fuzzer 已落地 |
| framebuffer 差分位掩码 | **记录** | render_cache 粒度（version+generation 键）已覆盖本仓需求；属性位掩码是更细一档，待渲染成为实测瓶颈再取 |

**版权纪律**：Microsoft 代码 MIT——本条目仅取设计判据，无代码移植，无 NOTICE 义务；将来移植任何片段须按 AGENTS 追加 NOTICE。

- **模型自举 ✅ 已落地（2026-10-24，`/bootstrap <provider> [model] [--reorganize]`，REPL；2026-10-24 晚补强）**：一条命令完成「换 provider/token → 热重载 → 记忆随行」。**积木已在**：`/model` 切换（自记 provider.toml）、ConfigWatcher 回合边界重建 runtime（新 base_url/api_key 生效）、MEMORY.md 模型无关自动随行。**三缺口全部补齐（2026-10-24 真机验证）**：① 编排命令 `/bootstrap <provider> [model] [--token <key>] [--reorganize]`——三段预检（resolve/可达性/runtime build）全过才切换；② `--reorganize` 由新模型在会话内重写 MEMORY.md（.bak 备份+原子写+transcript 可审计）；③ REPL 斜杠命令天然回合间隙执行（在飞处置不适用，TUI 无斜杠面为诚实边界）。**2026-10-24 晚补强（用户反馈：非 OpenAI 协议未验证就切换）**：可达性预检从「仅 OpenAI 协议、其余 build-time only」升级为**三协议全覆盖**（openai/openai-responses 同用 `/v1/models`，anthropic 走 `GET /v1/models` 带 x-api-key/anthropic-version/auth_token 头——`AnthropicClient::list_models` 新增）；探测失败 abort 不切换（真机验证：坏 token 401 → aborted，活动配置原封不动）。**点名纪律**：点名模型永不更改（仅对照端点清单验证并警告），智能选择（flash 档自动选/首模型兑底）仅作用于未点名时；probe 收敛为 `probe_models` 共享函数（`/bootstrap` 与 `hf models` 同一验证面，`hf models` 的 openai-only 限制同步放开）。**发现缓存**：bootstrap 探测后把端点自身模型清单+context 回写目录（persist_discovered），种子清单过时失效时的自动兑底；失败提示引导显式点名模型 id（透传不变）。**种子刷新（2026-10 网络核实）**：deepseek-flash（V4.1-Flash-1017，1M，官方现行 id；旧断言「deepseek-flash 不存在」已过时并修正）+ deepseek-v4-pro（1M）+ v4-flash（legacy 兼容受理）；Kimi K3.1-Pro/-highspeed（1M）；GLM-5.3/-flash/-flashx（1M，2026-08 上线）+ 5.2（1M）+ 5（200K）；Qwen3.8-max/-flash + 3.7-plus。余额查询仍为 DeepSeek base_url 限定（supports_balance 门）。`--token <key>` 支持内联凭据。**待后续**：web_search 自动查网填参（会话内模型已可用 web_search 查最新清单，工具面已就绪）。
#### 10.9 CLI agent 全景架构研读（2026-10-24 网络研究，只记录不执行）

2026 CLI agent 全景（Claude Code/Codex CLI/Gemini CLI/opencode/Aider 等）的架构创新点，按对 heartflow 的价值分级：

- **OS 内核级沙箱（Codex CLI，采纳方向·大件）**：macOS **Seatbelt** deny-by-default + 参数化可写根、Linux **Landlock + seccomp**、三档沙箱模式——内核强制而非提示词约束。本仓 bash 工具裸跑 + 权限门是应用层防线；内核沙箱是纵深防御的正解。**分相**：陪测批次后评估（Windows 侧对策另研——Job Object 已用于杀树，沙箱化是 Windows Sandbox/AppContainer 赛道）。同场发现：Claude Code 的 MCP/hooks/LSP 跑在沙箱**外**——社区共识是整个 agent 包容器，本仓 MCP 子进程面同此警示。
- **Hooks 系统（Claude Code，采纳设计）**：生命周期钩子 = 确定性副作用（如 **PreCompact hook 在压缩前把状态刷进 plan 文件**——与 C-1 loop 模式的「压缩前落盘」诉求同构）。本仓无钩子系统；落点 = 回合生命周期事件（pre-compact/pre-save/post-tool）暴露确定性回调。
- **Subagent 缓存共享（Claude Code，记录）**：fork-subagent 共享 prompt cache + 渐进上下文压缩——本仓 /jobs 工人已有隔离会话，缺的是 **fork 时共享前缀缓存**（省 token 的 subagent spawn）。
- **无损压缩争议（Codex，研究点）**：社区称其新压缩「lossless」优于摘要式——本仓七相（模型摘要+逐字尾+省略声明+硬触发）是摘要式阵营；「无损」的实现机制（疑似按需重取材）值得下会话研读 open-source CLI 源码对比。
- **延迟工具加载（Claude Code，轻采纳）**：deferred tool loading——本仓条件注册（rga）是雏形，全工具面懒加载（描述后置、调用时展开）待工具面膨胀后再做。
- **学术框架（引用）**：arXiv 2604.14228「The Design Space of AI Agent Systems」将 Claude Code 的 delegation 归类为 isolated-subagent-boundaries + deny-first-with-human-escalation——与本仓权限门+worker 只读默认同型，可作为 C-1 模式系统的理论参照。
- **基准面共识**：Claude Code 强在多文件复杂重构、Gemini CLI 强在 monorepo 级上下文但需近两倍追问——**追问次数是比分数更真实的 UX 指标**（本仓 /effort 与 loop 模式直接作用于它）


### 11. windows-rs 家族对齐与增强（2026-11-21 评估，只记录不执行）

**触发**：工具链切 `stable`（1.100）后 windows-registry 0.5→0.100 落地（读 WinINET，`crates/proxy/src/system.rs:30`，API 未变零适配）。借此对 windows-rs 家族做一轮全景对齐评估。

**现状事实（代码实证）**：
- 直接依赖仅 windows-registry 0.100（proxy 读侧）。windows-sys 三份均为**传递依赖**：0.59 ← fd-lock 4（`crates/runtime/src/filelock.rs:133` 文件锁）、0.61 ← anstyle/dirs-sys/clap 树、0.52 target-gated（需 `--target all` 才可见）；仓内**零直接引用**（旧记录「windows-sys 已在依赖树」指的这批）。
- 终端层 crossterm 0.29 单一 backend（`crates/cli/src/editor.rs`），无直接 Win32 调用；Windows Terminal 光标闪烁已有专项（`viewport_term.rs:37`）。
- bash 工具 `std/tokio::process` + 管道捕获（`crates/runtime/src/bash.rs:201-231`），`CREATE_NO_WINDOW` 已设（`bash.rs:471`），无 ConPTY。
- 存储层：JSON 快照原子 temp+rename（`cli/core.rs:307`、`runtime/compact.rs:159`、`provider/load.rs:612`）+ fd-lock + SQLite WAL `synchronous=NORMAL`（`crates/store/src/lib.rs:138`）。

**候选增强（按性价比排序，全部待验证）**：

1. **windows-registry 写侧——系统代理回写（§3 读侧的镜像，中收益中风险）**：0.100 提供 `set_*` API，可补「hf 设置系统代理」方向（现只读）。风险：写注册表是用户可见副作用，须显式开关 + 原值备份回滚；Clash/V2Ray 同时运行时行为需实测。
2. **Windows 原子写语义核验（小步、低风险，可先做）**：`std::fs::rename` 在 Windows 映射 `MoveFileExW(REPLACE_EXISTING)`，NTFS 上原子、跨卷报错——temp 与目标同目录已保证同卷。可选增强：rename 前对 temp 句柄 `sync_all()`（FlushFileBuffers 等价）缩崩溃窗口。前置：`store/tests/io_correctness.rs` 加 Windows 专项（kill 进程于 rename 前后）。
3. **ConPTY 给 bash 工具挂伪终端（高成本，触发条件制）**：管道捕获下依赖 TTY 的子程序（分页器、颜色、进度条）行为退化。windows-sys `Win32_System_Console` 的 `CreatePseudoConsole` + `PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE` 可挂真伪终端。成本：会话生命周期管理、管道与 ConPTY 并存的双读死锁风险、输出含 VT 序列需下游过滤。**触发条件**：出现实际「无 TTY 行为异常」的命令案例再动；短期用 `--no-pager`/`--color=never` 注入规避（`bash.rs:591` 已有 TERM 处理）。
4. **windows-sys 版本收敛（不做）**：fd-lock 4 锁 0.59，等 fd-lock 上游升级自然收敛到 0.61+；主动动收益为零。

**对齐结论**：本仓 Windows 面现状已经过实战（代理读侧、CREATE_NO_WINDOW、fd-lock、io_correctness 测试套），与 windows-rs 家族**无需要对齐的缺口**；上述 1/2 是自然延伸，3 是触发条件制的储备项。
