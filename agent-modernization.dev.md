# Agent 现代化设计裁决：heartflow × 2026 标配 × magpie

> 性质：设计分析，不是实施清单。事实带 file:line；判断标注【裁决】/【建议】/【不做】。
> 对照物：`yetone/magpie`（Go 本地模型网关，MIT，2026-09 创建，数万 star）、2026 年 agent 通行六件套。
> 纪律：本文件只记录裁决与借鉴，未改任何代码、未 commit、未 push。若要落地，待办条目按 AGENTS.md 约定写进 `docs/src/roadmap.md`，本文件不充当第二路线图。

---

## 0. 结论先行

| # | 问题 | 裁决 |
|---|---|---|
| D1 | 要不要用 Go 做网络请求和并发？ | **不做。** reqwest 0.12 + tokio 已是完整答案，换 Go 只增加语言边界，不增加能力。 |
| D2 | 要不要抄 magpie 的路由组？ | **不抄代码，记录概念。** smart 路由"用一次 LLM 调用决定下一次调用哪个 LLM"，在便宜档省钱、在贵档倒贴，且 roadmap 已有 fallback 候选。 |
| D3 | 要不要借鉴"重要数据拦截"？ | **要，且这是唯一必须内建的短板。** heartflow 的脱敏只管落盘，出站请求原文照发。 |
| D4 | 现在最便捷的现代化路径？ | **把 heartflow 接到 magpie 网关**（base_url 指向本地），零代码改动拿到脱敏/路由/观测。内部能力只补 magpie 补不了的部分。 |
| D5 | 其它标配缺口？ | **hooks 缺位最明显**；可观测只有 token 计数，没有延迟分解与上下文构成。均列候选，不紧急。 |

一句话：**外部挂载 magpie 拿"便捷"，内部只补出站脱敏这一处"必需"，Go 一行不写。**

---

## 1. 现状审计（对照 2026 agent 标配）

标配出处：Anthropic/OpenAI 生态已收敛的六件套——AGENTS.md 类规则、Skill、Subagent、Hook、MCP、权限与沙箱；外加上下文管理与可观测性（buildingagenticai.com 2026 mental model、opcnew 六能力综述、easyspawn agent loop 拆解，均已检索核对）。

| 能力 | 业界标配 | heartflow 现状（源码实证） | 判定 |
|---|---|---|---|
| 项目规则 | AGENTS.md / CLAUDE.md | `crates/runtime/src/prompt.rs:291` 逐级读取 + 排他回退 CLAUDE.md；`~/.agent/rules/*.md` 全量注入（AGENTS.md:126） | ✅ 已具备，且比标配更细 |
| Skill | 渐进披露：元数据常驻、正文按需 | `crates/runtime/src/agent_assets.rs:64` `discover_skills`；只注入 name/description，上限 64（AGENTS.md:127） | ✅ 已具备 |
| MCP | 标准协议接外部系统 | `crates/mcp`，stdio + Streamable-HTTP/SSE，`read_only` 标志并入只读权限档（AGENTS.md:103） | ✅ 已具备 |
| 权限分层 | allow/ask/deny + 沙箱 | `crates/cli/src/permissions.rs:21/44`，六档 mode，Prompt 才问 gate；只读/plan 并入 MCP 只读工具 | ✅ 权限具备，⚠️ 沙箱缺位 |
| 上下文管理 | compaction / pin / 文件记忆 | `conversation.rs` 半窗压缩 + `replay_verbatim_tail` + `/pin` + 两层 MEMORY.md（AGENTS.md:130-132） | ✅ 已具备 |
| 检查点/回滚 | 编辑前快照 | `runtime/checkpoint.rs` shadow-git，MVP 已上线（roadmap.md:337） | ✅ 已具备 |
| 编辑后反馈 | 编译器回注同一工具结果 | `runtime/diagnostics` cargo check 回注 `postEditDiagnostics`（roadmap.md:339） | ✅ 领先多数同类 |
| 任务队列 | durable job / 多会话 | `FollowUpQueue` + `/queue` 家族 + `/jobs` + autopilot 64 回合上限（AGENTS.md:63） | ✅ 单进程内具备，跨会话 job 未实现 |
| Subagent | 独立上下文、窄工具集、可并行 | **全仓无 subagent/delegate 概念**，工具面单会话单循环（roadmap.md:29），§1 已列为绿地设计 | ⚠️ 已规划未落地 |
| **Hook** | 生命周期确定性守卫（pre-tool / post-write） | **零命中**：无 `Hook` 类型、`HookEvent`、pre/post 回调；权限门是静态策略，不是事件驱动的确定性代码 | ❌ **真正缺口** |
| **出站脱敏** | 请求出机前拦敏感数据 | **零覆盖**：`redact.rs:1-8` 明确"在内存会话上不改动，模型仍看到原值"；所有 redact 调用点都在落盘路径（storage.rs:122、jobs.rs:417-580） | ❌ **真正缺口（最高优先级）** |
| 可观测性 | 延迟瀑布、上下文构成、缓存命中、成本 | `runtime/usage.rs` 仅四个 token 计数器（input/output/cache_creation/cache_read），context_input_tokens 已正确合计缓存（:20-31）；无时间、无构成分解、无成本 | ⚠️ 只有骨架 |
| Provider 路由 | 多账号故障转移 / 按难度分流 | `api/retry.rs` 单端点指数退避 + Retry-After（:53-121），无跨 provider failover；roadmap.md:581 已记 fallback 候选 | ⚠️ 已记录未落地 |

结论：heartflow 的 agent Harness 做得比大多数同类完整。**缺的不是"更多 agent 特性"，而是三件事：出站脱敏、hooks、可观测性深度。**

---

## 2. D1 裁决：不用 Go 重写网络层

### 2.1 事实

- `crates/api/Cargo.toml:13-24`：reqwest 0.12（rustls-tls / gzip / brotli / socks）+ tokio 1（io-util / macros / net / rt-multi-thread / time）。
- `crates/api/src/retry.rs:26-160`：`build_http()` 统一构造客户端、`is_retryable_status`、`parse_retry_after`、指数退避 `RetryPolicy`、全链路 proxy 注入。
- `crates/proxy` 独立 crate：配置 > env > 系统代理 > direct 的五级优先级，回环/LAN 永不走代理，1s TCP 探测后降级（AGENTS.md:101）。
- `crates/provider/src/adapter.rs`：三套协议适配器（Messages / Chat / Responses），流式 + 工具调用 + reasoning 全通，有 cassette 逐字节回放夹具（`HEARTFLOW_CASSETTE`）。
- 分层硬约束：`api`（传输）→ `provider`（桥接）→ `runtime`（会话），runtime 不感知传输细节（AGENTS.md:10）。

### 2.2 为什么 Go 在 magpie 成立、在 heartflow 不成立

magpie 用 Go 的理由是**它自身的产品形态**：单二进制 ~15MB（终端版 ~7MB）、Wails 系统 webview 桌面 GUI、菜单栏常驻、跨平台托盘。Go 对"GUI + 网关 + 单文件分发"是合理选择。

heartflow 是 **CLI/TUI agent**，没有 GUI，没有托盘，没有跨 agent 配置面板需求。它的网络需求是"一条 SSE 流 + 重试 + 代理"，tokio 已经解决，且：

1. **性能不占优**。tokio 多线程运行时 + Rust 零成本抽象，在单连接流式 LLM 场景不输 Go goroutine。换语言不会让首字节更快——TTFT 瓶颈在供应商，不在客户端运行时。
2. **破坏分层与编译纪律**。引入 Go = 要么 FFI（cgo，`unsafe_code = forbid` 直接冲突，AGENTS.md:56），要么独立网关子进程（多一个必须拉起/监控/关停的进程，毁掉单二进制交付）。
3. **现有资产全部作废或需重写**。retry 策略、代理五级链、三种协议适配器、cassette 回放夹具——这些是 heartflow 传输可靠性的积累。Go 版要重做一遍，还要重做端到端冒烟。
4. **没有解决问题**。用户真正痛的"限流中断、密钥外泄、耗时不透明"是**策略问题**（路由、脱敏、观测），不是语言问题。Go 不会自动带来这些。

【裁决】**不为网络与并发引入 Go。** 若哪天要做"跨 agent 统一网关产品"，那是**另一个项目**，不是给 heartflow 换心。

### 2.3 唯一合理的 Go 形态

如果确实想要 magpie 那种网关能力又不想自己实现：**直接用 magpie**。heartflow 的 provider 层支持 `ANTHROPIC_BASE_URL` / 自定义 `base_url`（AGENTS.md:81、97），把 `base_url` 指到 `http://127.0.0.1:3425` 即可。这是【D4】的零改动路径。

---

## 3. D3 核心借鉴：重要数据拦截（出站脱敏）

### 3.1 magpie 实际怎么做的（源码取证，非传闻）

来源：`yetone/magpie` README、`internal/pii/pii.go`、`internal/cli/groups_cli.go`、`internal/provider/grouptemplate.go`，以及 aniketkarneai.com 2026-10-28 源码分析、ainews.leverageai.com 案例页。

1. **双路识别**：确定性规则（API key、私钥、token、email、带校验和的卡号/证件号）+ 可选本地 CPU 小模型（源码常量 `1,637,572,192` bytes ≈1.6GB）识别规则抓不到的姓名/地址/密码；另有 39MB AstrLink Guard 按固定 revision + SHA-256 拉取。
2. **稳定占位符**：值映射为 `{{API_KEY_k3v9x2mq}}`，**同一值每次映射同一占位符**。这是关键——随机占位符会让每轮 prompt cache 和 thinking signature 全部失效，成本和延迟双输。
3. **双向还原**：回复文本、流式 delta、**工具调用参数**都还原为原值，所以 agent 执行的命令拿到真 key。
4. **三档策略**：Mask / Warn / Block + `Never mask` 名单（"分析这个 key 为什么被拒"这类任务必须看到原值）。
5. **fail-closed**：脱敏开启但本地模型未就绪时，请求 503 扣留，"never sent as written"。
6. **只在内存**：值不落盘，记录只留类型、长度、前几字符；`Insights › Masking` 并排展示"请求原文 vs 供应商收到什么"。
7. **代价**：模型看到占位符，就无法推理该值本身；规则会误判。

### 3.2 heartflow 的差距（精确）

`crates/runtime/src/redact.rs:1-8` 文件头写得很清楚：

> Redaction runs on a *clone* at the single save choke-point, so the in-memory session that talks to the provider is never mutated — the model still sees whatever it legitimately needs within the live turn.

这不是疏漏，是刻意设计（保护落盘、不干预推理）。但它留下一个真实攻击面：**agent 读到的 `.env`、带 token 的日志、配置里的密钥，会原样进入 prompt 发给供应商。** `security.dev.md` §一把防线画成 L1/L2/L3，三层的共同点是"管本地存储"，**没有一层管出站**。

### 3.3 内建规格（建议，分三期）

**P0 — 出站脱敏最小闭环（高价值/低成本）**

- 拦截点：`crates/provider/src/adapter.rs` 的 `stream(&mut self, request: ApiRequest)` 三个实现（:98/:141/:182）之前，对 `MessageRequest`（`crates/api/src/types.rs:32-39`：`messages` / `system` / `tools`）做一次文本变换。选在 `ApiRequest` 层而非 JSON 层，避免三套协议各写一遍。
- 复用 `runtime::redact::redact_text` 的规则集，但**替换策略必须不同**：落盘用常量 `[REDACTED]`，出站用稳定占位符 `{{SECRET_<digest8>}}`（digest 取值的确定性哈希前 8 位）。
- 注册表复用现有 `registered_secrets`（`cli/storage.rs:248`）+ 已通过 `set_job_redaction_secrets` 验证的全局注册模式（`runtime/jobs.rs:192`）。
- 还原点：`TurnStream` 消费侧，对文本 delta 与 `tool_use.input` 做反向替换。**难点**：流式 delta 会把占位符切断（一块 `{{API_`、下一块 `KEY_x}}`），必须在流层加有界 buffer（如 256 字节尾部保留），不能按 delta 独立正则——这与 AGENTS.md:142 已记载的"delta 拼接损坏"同类陷阱。
- fail-closed：规则/注册表不可用时返回 `ApiError`，不静默放行。
- 观测：`compaction-stats.json` 同款计数器（security.dev.md:77 已建议过脱敏计数），只记次数/类型，不记值。

**P1 — 用户可控策略**

- `[settings] outbound_redaction = mask|warn|block|off`（默认 `mask`），`never_mask` 值列表。
- `hf doctor` 报告："本次会话注册 N 个凭据，出站脱敏命中 M 次"。
- 与 roadmap.md:480-485 的 `/connect <provider> --key <secret>` 设计对接：输入边界拦截 + 出站占位符是同一条链的两端，别只做一半。

**P2 — 本地模型识别（不建议急做）**

- 1.6GB 模型对单用户 CLI agent 是沉重依赖，且 heartflow 定位"零外部依赖"。**建议永远不做**，改为让用户自定义正则规则（magpie 也支持 custom words/patterns）。
- 若真要做，用外部 magpie 网关，不要内嵌。

### 3.4 测试矩阵（P0 必须钉死）

| 用例 | 期望 |
|---|---|
| 已知字面 key 出现在用户 prompt | 出站为 `{{SECRET_x}}`，落盘为 `[REDACTED]`，终端显示原文 |
| 同一 key 连续两轮 | 占位符完全一致（cache 不失效） |
| key 出现在 `tool_use.input`（如 bash 命令） | 出占位符，回复还原后工具拿到真值 |
| 流式 delta 切断占位符 | 有界 buffer 拼接后还原，不漏半截 `{{API_` |
| 规则集初始化失败 | 请求被拒，不裸发 |
| `never_mask` 命中的值 | 出站原样，计数加一 |

---

## 4. 其它 magpie 借鉴的逐项判定

| magpie 能力 | 判定 | 理由 |
|---|---|---|
| 本地网关 + 协议翻译 | 【不做】内部实现 | heartflow 已有三套适配器；外部挂 magpie 即可。别把 agent 变成网关。 |
| Routing Groups（smart/order/rotate/usage/weight…） | 【记录概念】 | smart 用 LLM 分类决定路由，每回合多一次调用，便宜档才划算（roadmap.md:581 已记 fallback 候选）。先做 order/usage 静态策略，别碰 smart。 |
| 会话亲和（stays: session/turn/off/strict） | 【建议】随 failover 一起 | 会话内固定 provider 保 prompt cache，这是实质收益。 |
| 延迟瀑布（本地脱敏/网关/网络/TTFT/流式） | 【建议】低成本高收益 | `usage.rs` 加四个时间戳即可；TTFT 单列，直接暴露"卡在供应商还是本地"。 |
| 上下文 TreeMap（system/工具定义/文件/工具输出/历史占比） | 【建议】 | 现有 `TokenUsage` 已是正确骨架，缺的是分类计数。可离线分析 transcript，不侵入传输。 |
| 会话回放 + 工具调用归档导出 Markdown | 【不做】优先低 | session JSONL 已有；导出是锦上添花。 |
| 加密多端同步（GitHub/WebDAV/S3） | 【不做】 | 引入云信任边界，与"零遥测、本地优先"冲突。 |
| 外科手术式配置编辑（保留注释缩进、原子写） | 【已有等价】 | heartflow 已用 temp+rename 原子回写 provider.toml/session（AGENTS.md:111/121），ConfigWatcher 五面热重载（:115）。不重复造。 |
| Fail-closed 安全语义 | 【采纳为原则】 | 不止脱敏：权限门、沙箱失败都应 fail-closed。 |

---

## 5. 推荐顺序（按价值/成本）

1. **零成本**：把开发环境的 heartflow `base_url` 指向 magpie，人工验证三协议适配不被网关破坏（重点 cassette 回放是否仍相等）。这是今天的收益。
2. **P0 出站脱敏**：只在 provider `ApiRequest` 层加变换 + 流层有界还原 buffer，配 6 条测试。这是唯一必须内建的功能。
3. **可观测骨架扩展**：`usage.rs` 加时间戳 + 分类计数，`/status` 展示。不改接口面，无需同步 llms。
4. **Hooks**：事件驱动守卫（pre-tool / post-write），与权限门正交；挂在 `tool_exec.rs` 分发点。进 roadmap，不急。
5. **静态路由 failover**：order/usage 策略 + 会话亲和，进 roadmap §12/A1。
6. **明确不做**：Go 重写、smart LLM 路由、内嵌 1.6GB PII 模型、云同步。

---

## 6. 边界与纪律

- 本文件是裁决记录，**不是待办清单**。要实施时按 AGENTS.md:47 的接口面约束决定是否需要同步 README / docs / llms / bucket / wiki。
- magpie 是 MIT。本文只借鉴设计概念，未复制任何源码片段；若将来移植具体实现，按 AGENTS.md:41 在 `NOTICE` 与 README §致谢补版权声明。
- **不 push**：`.github/workflows/release.yml` 是 push-main 触发发版（工作记忆铁律）。本轮未 commit、未 push、未改任何 workflow。
- 所有流式路径改动的前置条件：先录 `HEARTFLOW_CASSETTE` 夹具（AGENTS.md:89），再改代码，否则无法验证逐字节保真。已有夹具可作基线：`crates/provider/tests/fixtures/{text-turn,tool-turn}.json`、根 `fixtures/{first,truncated-turn1}.json`——但出站脱敏会改变请求字节，**必须重录**，不能拿旧夹具硬套。

---

## 7. 收束

heartflow 的 Harness 不落后。真正该做的不是换语言追新，是把"本地脱敏"这条链从磁盘延伸到网络出口——然后接上 magpie，白拿路由和观测。Go 不碰。
