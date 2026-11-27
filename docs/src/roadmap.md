# 路线图

以下方向已明确规划但**尚未实现**，仅作路线记录。2026-10-19 批次新增 §1–§6 六条主线，2026-10-21 批次新增 §7（推理引擎生态深研）、§8（bash job 监控 / job 超时 / 挂起检测）并续批扩写 §1（多子代理 spawn 面 / pi 的 job 系统 / 队列三件套设计规格——源自 `archive/pi-research/pi-subagents` 源码实证与 `openmemory.md.bak` 的队列需求回溯），全部来自外部项目调研，本地研究副本均**不入库**：pi-subagents/MergeSearch 在 `archive/pi-research/`；ds4（antirez DwarfStar 推理引擎）、easy-llm-cli（Gemini-CLI 派生 agentic CLI）、kimi-code（`MoonshotAI/kimi-code`，MIT © 2026 Moonshot AI，agent-core-v2 已源码核实）、DeepInfer（deepseek-ai，Apache 2.0）与 deepseek-recipe（deepseek-ai，MIT，纯 Rust）在 `.workbuddy/`。当前**不执行**，只锁定设计判据与借鉴对象。2026-10-22 批次新增 §10（PandaCode / pi / codex / claude 设计借鉴与取舍——源自 `archive/pandacode` 浅克隆源码实证，聚焦 mcp 与 code-mode 升级取舍），聚焦「采纳 / 改造 / 不做」三档结论。2026-11-20 批次新增 §15（zaly / maki / codex 三仓 agent 循环本体借鉴——凭据自动发现、停机保险丝、后台任务+heartbeat、压缩护栏、取消传播、子代理治理；源自 `gitmine/{zaly,maki,codex}` 浅克隆源码实证，全文证据见根目录 `agents.dev.md`）与 §16（自身稳定性重构止血四相 ST-0~ST-4——流中挂断零防护、autopilot 错误不分级、压缩护栏默认关等七项根因，全文见根目录 `stability.md`）。

> **kimi-code 自研度结论（源码核实）**：agent 核心 `packages/agent-core-v2` 是**一方自研包**，**非 gemini-cli / claude-code / codex 二改**（grep 三家零命中）；仅 TUI 层源自外部 `pi-tui`（入库为 `@moonshot-ai/pi-tui`，README 致谢），并采纳行业开放协议 MCP/ACP 与 Agent Skills 约定。另有一方包可对标 heartflow：`@moonshot-ai/kosong`（"LLM abstraction layer"，直接包 `@anthropic-ai/sdk` + `openai` SDK ≈ heartflow `api`+`provider`）、`@moonshot-ai/kimi-telemetry`（一方遥测）、`acp-server`。**下面标注「源码实证」的条目均已对照 `.workbuddy/kimi-code` 当前 HEAD 核实行号。**


> **结构（2026-10-26 重排）**：本文件只保留**活动面**——已落地状态的记录与下一个待办；研究存档、采纳判决与「先不做」清单全部移入 [roadmap-deferred.md](roadmap-deferred.md)。粒度原则：每个工作流一节，节内只写「现状 → 下一动作」。

> **状态总览（2026-10-26）**：
> | 工作流 | 状态 | 下一动作 |
> |---|---|---|
> | §11 硬骨头序列（11.0–11.5） | ✅ 全部落地（陪测卡在用户手上） | push 发 v1.5.x |
> | §4 compaction 七相 + 停机矩阵 + soak | ✅ 落地 | — |
> | §1 队列/mission/steer/泳道轮转 | ✅ 落地 | — |
> | C-1 模式系统（spec/loop 档、/web 三态、/effort、键位） | ✅ 核心落地 | spec overlay 确认流 + loop 自动续跑（下相） |
> | C-2 本地小模型赛道 | 📋 设计定案 | Ollama embeddings 先行实验 |
> | §12 Provider 身份链 | 📋 设计定案（混线 401 事故已解剖） | 三条判据裁决后实施 |
> | §13 提示词内参数自举 / §14 TUI 视觉体系 | 📋 设计规格 | 未排期 |
> | Tool Choice / Response Format | ✅ 贯通 | — |
> | KI-003/004（粘贴截断 / eza ANSI） | ✅ 已修（3c62a8d） | 实机复验 |

## A. 执行路线图（要改 heartflow 代码的硬骨头，按依赖排序）

> A 组为落地任务：双 Agent 架构（§1）、工程稳定性与持久性（§4）、编辑后反馈（§6）三方向的设计规格与已落地状态，以及把「要动大代码」条目抽出的执行序列（§11）。B 组调研是 A 组的依据来源。2026-10-24 批次追加 §12–§14 三份设计规格（provider 身份/凭据链、提示词内参数自举、TUI 视觉体系）：细节与边界已想透，尚未落地，落地与否由用户裁决。

### 1. 双 Agent 架构：监工 / 工人

**现状（代码实证）**：全仓 grep 无 `subagent` / `supervisor` / `delegate` 任何概念，工具面是单会话单循环（`crates/tools/src/lib.rs:137` 固定注册 + `execute_tool` 分发）。这是绿地设计。交互面已有单 FIFO 跟随队列（`crates/cli/src/core.rs:19`）与 Hermes 任务环（`crates/cli/src/plan.rs:249`），但**无 job 系统、无多子代理 spawn 面**——本节末尾三节（多子代理 / pi 的 job / 队列三件套）分别补齐。

**设想**：监工全权负责项目——项目方向、搜索文档、清理信息、得出 API、可行性判断、工程管理、取舍；**默认等候，收到工人消息才工作**。工人持有上下文隔离的子会话，遇决策点才上报。

**与 pi-subagents（nicobailon，本地副本 `archive/pi-research/pi-subagents`）监管协议的对应关系**：

| 维度 | pi-subagents 既有设计 | heartflow 落点判断 |
|---|---|---|
| 唤醒 | `parent-wake.ts`：父会话 idle 才触发 turn（`PARENT_WAKE_TEXT`），busy 时只 append 不启动；10s pending 上限，用户 abort 后挂起 | 同构：监工循环 = 现有 agent loop + 一条「消息到达才唤醒」的门 |
| 上报 | `contact_supervisor` 三 reason：`need_decision` / `interview_request` / `progress_update`（`native-supervisor-channel.ts:102`） | 照单全收，三 reason 就是最小充分集 |
| 通道 | 文件邮箱：`requests/` + `replies/` 目录，per (runId, agent, childIndex)；64 KiB 消息上限；500ms 轮询 + 5s 安全轮询；原子写 JSON 0600 | heartflow 是 Rust 单体：进程内 `mpsc` 优先，跨进程（bash 长任务）才需要文件邮箱 |
| 角色 | scout / researcher / evidence-auditor / worker / reviewer / oracle / delegate | 工人 = worker + scout 合并；监工 = reviewer + oracle 合一，但**允许不同模型**（pi 的监工就是父会话本身，同模型） |
| 上下文隔离 | 子会话独立 context；`subagents_enable` 懒加载保护 prompt cache | 天然成立；注意 heartflow 的 compact 现状（见 §4） |
| 预算与护栏 | `maxSubagentSpawnsPerRun` 默认 64；递归护栏；watchdog 对抗审查 | 必须自带 spawn 计数与深度上限，否则监工-工人互派会指数爆炸 |
| 耐久记录 | missions：durable record + delivery receipts + goal mission（token 预算 + needs-attention 通知）；**纪律：扩展永不自行启动或重规划 goal 工作** | 监工「全权负责项目」= mission owner；「不自行启动」这条纪律直接继承——监工只在收到消息或用户输入时行动 |

**不同点（不要照抄）**：pi 是 TS 扩展 + 独立 runner 进程，heartflow 应在 `runtime` 层加「子会话 + 消息总线」，工人可以是同进程 agent loop 实例（快、可流式）或子进程（隔离、抗崩溃），由任务类型决定。**明确不做**：不做通用多 agent 平台、不做跨机器编排。

#### 借鉴点子（pi-subagents 深度研读，设计/架构级，待落地）

从 `src/intercom/native-supervisor-channel.ts` 与 `src/shared/parent-wake.ts` 提炼七条可直接照搬的协议级模式：

1. **文件信箱 + 原子 0600 写入 + 生命周期 GC**：worker 写 `requests/<id>.json`、监工回 `replies/<id>.json`，目录 `mode 0700`、写入走 `writeAtomicJson`（temp+rename，:122-133/:238/:573-584）；请求有 `pending/resolved/expired/inactive/missing/wrong-session` 六态（:517-528），resolved/expired/inactive 即删源文件；空通道 60s 后 GC（:394-434）。→ heartflow 跨进程工人（bash 长任务）走同样的磁盘信箱，崩溃不丢问；进程内工人用 `mpsc` 即可。
2. **reason 三分法含 fire-and-forget**：`progress_update` 设 `expectsReply=false`，监工收到即删、不触发回合（:213/:825-830）；仅 `need_decision`/`interview_request` 唤醒监工。→ 这正是「监工默认等候，收到工人消息才工作」的最小实现：汇报不唤醒，阻塞决策才唤醒。
3. **按需门控轮询（空闲零 CPU）**：`hasTransportDemand()` 为真才起 500ms 轮询，否则停（:769-780/:980-990）。→ 监工空闲时彻底不占轮询。
4. **双阶超时 + 排队续命**：ask 默认 10min（`PI_INTERCOM_ASK_TIMEOUT_MS` 可配，:29），父会话正忙、notice 被排在忙 run 后时，`extendQueuedNotices` 把 `expiresAt` 续半程（:961-978）——问因父忙而不超时。
5. **settle 提醒预算 + 禁止自动批准**：父回合即将 settle 而仍有未答 ask，注入「Silent supervisor wake」提醒；预算耗尽再注入 `BLOCKED` 条目，但明文「Do not auto-approve」（:877-930）。→ 工人不被静默饿死，监工绝不代工人越权批准。
6. **会话继承重匹配**：`requestMatchesOwner` + `inheritedSessionIds` + `ownsRun`（:436-449）、`noticeDelivered`/`parentRunSettled`（:940-950）——压缩/重载后，在飞 ask 凭 runId 重新匹配到当前会话，不丢。
7. **有界资源**：单消息 64KiB 上限（:28）、关联表 256 条 + 10min 保留（:34-35）。→ 信箱不会无限涨。

唤醒契约（`parent-wake.ts`）：仅 idle 父会话才真正触发回合（`PARENT_WAKE_TEXT` 限定监工被唤醒时「不重跑已完成工作、不越权、只在本授权内答复」），busy 只 append；10s pending 上限，过则视为放弃（notice 已在会话里，仅丢回合，:3-6/:57-78）。reload 不丢 reservation、真 shutdown 才清（:92-94）。

#### 借鉴点子（kimi-code：Goal 模式 + 状态机 ReAct，源码实证，待落地 §1/§4）

1. **Goal 模式 = mission 完成定义 + 三轴预算 + 四态状态机 + 续跑驱动器（源码实证，`packages/agent-core-v2/src/features/goal/`）**：goal 功能四工具 `CreateGoalTool` / `GetGoalTool` / `SetGoalBudgetTool` / `UpdateGoalTool`（`goalFeature.ts:27-41`）；`GoalSnapshot`（`types.ts:24-34`）= `objective` + **`completionCriterion`**（完成判据，即 mission 的「验收标准」；粘贴稿所谓 "rubric" 的源码实际术语）+ `status` + `turnsUsed/tokensUsed/wallClockMs` + `budget`；`GoalStatus = active|paused|blocked|complete` 四态（`types.ts:1`）；**三轴预算** `GoalBudgetLimits = tokenBudget|turnBudget|wallClockBudgetMs` + `GoalBudgetReport` 剩余/达成/超支（`types.ts:5-22`，比 §4 pi-subagents 纯 token 预算更细）。
   - **UpdateGoalTool 的完成/阻塞纪律（`tools/update-goal/update-goal.md`，可直接抄的提示词工程）**：`complete` 仅当「目标满足且所有显式验证通过」——**弱/间接证据不算完成、预算将尽或想停不算完成、只出了计划/首遍/部分结果不算**；`blocked` 需**同一阻塞连续 ≥3 个 goal 回合**才可标记（防一遇阻就弃），且「工作大/难/慢/不确定/待验证」都不构成 blocked；「**多数活跃 goal 回合不应调此工具**，做完一个有用切片且仍有实质工作就正常结束回合，**runtime 会在下一 goal 回合提示继续**」——即 `goal_continue` 由 runtime 驱动而非模型自觉；「状态是机器可读信号，完成摘要/阻塞解释由模型在下一句写」——机器信号与人话分离。
   - 每回合注入 active/blocked/paused 三态 reminder（`features/goal/injection/`）+ `goalDeadlineScheduler`（截止调度）。
   - → heartflow 落点：这是 §1 监工 mission「**何时算干完 + 预算 + 状态 + 谁驱动续跑**」的完整范本——mission 应带 `completionCriterion` + 三轴预算 + 四态，并把 UpdateGoal 的「反过早完成 / 反过早放弃」纪律写进 mission 系统提示；runtime 自动续跑 goal 回合 = §1「回合串联」的驱动器（比单纯 `goal_continue` 注入更强：默认就续，除非显式 complete/blocked）。
2. **事件驱动状态机 ReAct（源码实证，对照 kimi `human/agent/turn.ts`）**：经典 ReAct while 循环的外部介入是「纸条 + 轮询」——循环只能在**迭代边界**看到 abort，LLM 调用期间是**死区**，Esc 白花一整次调用；状态机把介入点变成**图上的边**。已核实的结构：`{ type: 'turn.abort' }` 事件类型（:199）与 `invoke`(LLM 调用) 态（:460/:492）**并列**，abort 直达进行中的调用（迁移 :470/:706/:723/:803/:817/:865 → `#turn.aborted` :812）；`llm.done` 的 guard「`toolCalls.length > 0`→acting / 否则 done」**就是 ReAct 本身**（:575）；`retrying` 是**带定时器的状态**（`after: { retryDelay: 'thinking' }` :719-720，等待期间 abort 依然可达、定时器随状态迁移一起清除）；`mailbox + run-to-completion` = 零锁并发；`snapshot()` 让状态成为外部可读的**数据**（`loopService.ts`）。→ heartflow 落点：`runtime` agent loop（P1 已异步化、P2 并发模型规划中）应把「回合」建模为状态机，使 (a) abort 能打断**在飞**的 LLM/工具调用（对应 §4 ds4 cooperative interruption + easy-llm-cli `execute(AbortSignal)`，修本仓「abort 只在回合边界生效」的隐患）；(b) 「等待/重试」是一等状态而非 try/except 嵌套（对应 `crates/api/src/retry.rs` 重试路径）；(c) `snapshot()` 供 §4 可观测性与 §1 监工观测工人状态。同进程工人循环直接复用这台小机器（mailbox 语义天然适配 §1 消息总线）。

#### 设计规格：mission 完成判据 + 三轴预算（kimi-code Goal 范本，待落地 §1，仅记录）

范本已源码核实（`.workbuddy/kimi-code/packages/agent-core-v2/src/features/goal/`，详见 §1 上文条目）。本规格把它落成 heartflow 的数据结构与推进相。

**数据结构（设计，未实现）**：

```rust
/// 监工全权负责项目的耐久任务单元（pi-subagents mission + kimi goal 合体）。
struct Mission {
    id: MissionId,
    objective: String,
    criteria: Vec<CompletionCriterion>,   // 全部满足才可 Complete（kimi completionCriterion）
    status: MissionStatus,                // Active | Paused | Blocked | Complete
    budget: MissionBudget,                // token / turn / wall_clock 三轴，None = 不限
    used: MissionUsage,                   // turns_used / tokens_used / wall_clock_ms
    ledger: MissionLedger,                // durable state.json + delivery receipts
}

enum CompletionCriterion {
    CommandSucceeds { cmd: String },          // 验收命令退出码 0
    FileMatches { path: String, pattern: String },
    NoDiagnostics { scope: PathBuf },         // cargo check / LSP 干净
    HumanApproved,                            // 用户/监工显式批准
    FreeText { check: String },               // 模型自述判据（弱证据，须搭配上面任一条）
}
```

**D1.1 完成纪律（UpdateGoalTool 提示词契约，可直接抄）**：`complete` 仅当「objective 满足且所有显式验证通过」；弱/间接证据、预算将尽、只出计划/首遍/部分结果——都不算完成；`blocked` 需同一阻塞连续 ≥3 个 mission 回合（防一遇阻就弃）；「多数活跃回合不应调 update，做完一个有用切片即正常结束回合，**runtime 下一回合提示继续**」——续跑由 runtime 驱动而非模型自觉；机器状态信号与人话摘要分离（`tools/update-goal/update-goal.md`）。

**D1.2 三轴预算 + needs-attention**：任一轴将尽（≥90%）→ 回合末注入「剩余预算 + 下一就绪动作」提醒监工；超支 → 转 `budget-exhausted`：**停提醒但不关 mission**（pi-subagents `missions.md:42-64` 口径——大项目跨多次会话续命，不因单次预算耗尽被杀）。

**D1.3 runtime 续跑驱动器**：每 turn 开头注入 mission reminder（status + criteria 进度 + 预算剩余 + `goal_continue` 语义「上一回合未完成，继续」）；截止调度（kimi `goalDeadlineScheduler` 式）：到期未完成 → 提醒监工，**不自动杀 mission**。

**D1.4 落点映射**：监工 = mission owner（create/pause/resume/complete 四权限，「全权负责项目」的落地）；工人 = 执行体，经 §1 `contact_supervisor` 三 reason 上报（`progress_update` 不唤醒）；mission ledger = pi-subagents `state.json`（文件锁 + 原子写 + 256KiB 上限，`missions.md:24`）；恢复优先级：`mission.list/show` 先于一切新动作（压缩/重启后第一动作，与 §1 会话继承重匹配同源）。

**推进相**：P-A mission ledger（durable state + receipts，纯存储层可离线测）→ P-B criteria + UpdateGoal 纪律进 mission 系统提示 → P-C runtime 续跑注入 + 截止调度 → P-D 观测（snapshot 供 §4 可观测性 + 监工观测工人状态）。

**明确不做**：通用任务平台、跨机器编排、**自动批准**（pi-subagents settle 纪律「Do not auto-approve」明文保留——监工绝不代工人越权）。

#### 多子代理：spawn 面、并行组与容量治理（pi-subagents 源码实证，待落地）

**现状（代码实证）**：工具面是单会话单循环——`crates/tools/src/lib.rs:366` `execute_tool` 固定分发 15+ 工具，`AgentRuntime` 一实例持有 `session()` + `executor()`（`crates/runtime/src/conversation.rs:1039-1079`，另有 `reset_for_task`/`seed_plan` 供任务环换新鲜上下文）。**没有 spawn 面**：模型无法让「另一个自己」并行做事，§1 的监工/工人因此还缺执行体。

**pi-subagents 的多子代理机制（`archive/pi-research/pi-subagents`，源码实证）**：

1. **并行组（parallel groups）**：run 的步骤不是单纯链——`normalizeParallelGroups(run.parallelGroups, steps, chainStepCount)`（`src/runs/background/async-job-tracker.ts:156`）把步骤分成串行链 + 同组并行；job 状态带 `currentStep`/`chainStepCount`/`parallelGroups`/`hasParallelGroups`/`activeParallelGroup`，widget 只渲染**活跃并行组**的步（:157-162）——进度视图不被 64 个工人同时刷爆。→ heartflow 落点：`subagent` 工具一次 spawn N 个工人时，参数就是「chain 步骤表 + 并行组标注」，观测面按组折叠。
2. **child-safe state**：子进程拿到全新 state（`src/extension/fanout-child.ts:41-64` `createChildSafeState`）——不继承父的轮询器/watcher/completionSeen，只带必要配置。→ heartflow 落点：子进程工人 = 新 `AgentRuntime` + 显式传入的 mission/budget/权限，**不共享父的任何可变状态**（对应 §4 ds4 单写者纪律）。
3. **能力天花板（交集语义）**：派生子代理时能力取交集——`intersectSubagentCapabilityCeilings`（`src/runs/background/async-resume.ts:10`）+ `intersectThinkingCeilings`（:18）+ tool budget 三档 soft/hard/block。子代理只能**更窄不能更宽**。→ heartflow 落点：直接对应本仓权限门（`crates/cli/src/permissions.rs` 的 mode + Prompt gate）——工人权限档 ≤ 监工，取交集而非继承。
4. **容量治理（跨进程槽位）**：`src/runs/background/active-async-capacity.ts` 用文件化 slot 预留（`ActiveAsyncCapacityOwner`：reservationToken + generation + ownerSessionKey + slot，:16-31），abandoned slot 5min–24h 后释放（默认 20min，:11-14）；放槽前要 **PID 活性 + process-terminal proof 双重证据**。→ heartflow 落点：进程内先用 `Semaphore`（默认并发上限待 D 定）；跨进程槽位留给 bash 长任务外置时再搬（与 §4 pi-job-listener 条目合流）。
5. **恢复/续跑二态**：`AsyncResumeTarget.kind: "live" | "revive"`（`async-resume.ts:42-64`）——活着就 reattach（挂监视器），死了就 revive（新进程接旧 `sessionFile` + `managedWorktree` 保留工作树 + `launchContractDigest` 校验启动契约未变）。→ heartflow 落点：§1「会话继承重匹配」的执行体；`launchContractDigest` 对应「配置热重载后旧 worker 的契约校验」。
6. **headless 排空**：`src/runs/background/auto-drain.ts:41` `drainOutstandingWork`——无头会话退出前**排空本 session 全部后台工作（含排空期间新增的）**，默认 30min 超时（:9），session 变更即停，有未决 supervisor request 即 break。→ heartflow 落点：`hf -p`/pipe 模式退出前的收尾；与 §7.2 优雅停机同族。

**落点**：新工具 `subagent`（`execute_tool` 加分支，tools crate）或 runtime 层 `spawn_worker(objective, budget, capability_ceiling)`；worker = 同进程 `AgentRuntime` 实例（默认，快、可流式、共享 MCP 连接）或子进程（隔离，由参数决定）。**明确不做**：通用多 agent 平台、跨机器编排、worker 自建工具（工具面仍由宿主注册表单点分发）。

#### pi 的 job：异步任务系统（pi-subagents 源码实证，待落地 §1/§4）

job = 「提交后不必原地等」的任务单元。pi-subagents 把这套做成了完整契约；heartflow 现在**只有**回合内队列（见下节），没有任何跨回合/跨会话的任务存活机制。

1. **磁盘目录即状态机**：每个 async run 一个目录——`status.json`（状态快照）+ `events.jsonl`（追加事件流）+ `control/`（控制收件箱）+ `results/`（结果）。状态集 `queued/running/complete/failed/partial/paused/rejected/stopped`（`src/runs/background/async-job-tracker.ts:67-68`），外部 provider 再加 `blocked`（`src/api/external-job-provider.ts:11`）。→ heartflow 落点：`store` crate（SQLite）存索引 + `~/.heartflow/jobs/<id>/` 存目录；状态机先在纯存储层落地（离线可测）。
2. **观察者三件套**（`async-job-tracker.ts`）：per-job `fs.watch`（status.json / events.jsonl / control 目录，:620-675）+ **5s liveness sweep 兜底**（:58，漏事件的安全网）+ 25ms 刷新 debounce（:59）；事件读取是**字节游标增量**（:286-411：mid-line 起点检测、1MB 单行上限、2MB 扫描窗、只推进到最后一个完整行）——「不重扫、确定即发射」铁律的磁盘版。→ heartflow 落点：`notify` crate 可选（Windows 上 fs.watch 本就不可靠，sweep 是主、watch 是优化——pi 的 `shouldUseNativeFsWatch` 是同款判断）；游标式读法直接对应本仓「流式两路径不按 delta 重解析」。
3. **恢复与对账**：会话切换/重载后 `restoreActiveJobs` 按 sessionId 重新挂上 queued/running 的 job（:849-871）；`stale-run-reconciler` 对账「目录在但进程不在」的孤儿 run；status.json 缺失时 queued→running 的宽恕推进（:583-587）——**观察缺失不等于任务失败**。
4. **保留与 GC**：终态后 10s 才清理（:88/:222-234）；fleet 视图只保留最近 20 条终态（:57/:70-77）。→ heartflow 落点：job 列表有界，`/jobs` 不无限涨。
5. **控制收件箱（跨 OS 的关键修正）**：Windows 不能跨进程投递信号（`process.kill` 抛 `ENOSYS`，原 SIGUSR2 方案在 Windows 上**完全不可用**）→ 改成文件收件箱：`control/{interrupt,timeout,stop}.json` + `steer-requests/`，原子写（temp+rename）、消费即删（幂等）、`*-inbox-closed.json` 终态标记 + **写后复检的竞态处理**（写了才发现已关闭 → 删掉并报错，`src/runs/background/control-channel.ts:269-273/:310-314`）。→ heartflow 落点：子进程工人的唯一控制通道（与 §1 文件信箱同构）；这条同时是 Windows 优先环境的直接答案。
6. **调度（scheduled-runs，`src/runs/background/scheduled-runs.ts:26-79`）**：九动作 `schedule.create/list/show/history/pause/resume/run/run-due/delete`；trigger 三型 `once`/`interval`/`calendar`（calendar = day|week + HH:mm + IANA 时区，`src/runs/background/calendar-schedule.ts:13-30`，DST 取早、32 天搜索窗、不存在的时间跳过）；**`overlap: "skip"` 是唯一重叠语义**（不排队、不并行，上一个没完这一个就 skipped）；`catchUp: "none"|"latest"`；`quiet`（成功不唤醒父会话）；`sessionOnly`；`DEFAULT_MAX_PENDING=20`（:39）/`MAX_HISTORY=100`（:40）/`STALE_LAUNCH_CLAIM_MS=5min`（:41）；`dueReason: "timer"|"run-due"|"manual"`——**到期工作由外部 launcher 调 `run-due` 触发，无需常驻 daemon**。→ heartflow 落点：`hf jobs run-due` 子命令 + 启动时调用一次即获得定时能力；时区是缺口——heartflow 无 Temporal 等价物，先支持本地时区 + UTC（诚实记录：IANA 全量需引 chrono-tz 或自写，属依赖评估项）。
7. **外部 job provider 协议（`src/api/external-job-provider.ts`）**：五操作 `start/followUp/status/result/reattach`（:12/:50-57），把「job」抽象成**可指向外部系统**的句柄（`providerJobId` + `handleUrl` + `blockingJobId`）；校验极严——未知字段拒绝、NUL 拒绝、长度闸门、registry 版本不一致即抛（:98-178）。→ heartflow 落点：未来 bash 长任务、MCP 后台任务、远程 CI 的统一 `JobHandle` trait；§4 表格的 pi-job-listener 条目是它的更简单前身（会话外存活 + adopt 旧 pid）。
8. **排空与容量**：见上节 auto-drain 与 active-async-capacity，job 与多子代理共用同一套治理。

**standalone 纪律（`docs/standalone-background.md`，方法论）**：checksum-pinned 发布物 + 18 模式生命周期矩阵（隔离 namespace 逐模式跑）+ **「结果文件落盘 ≠ 退出证明」——必须观察到进程关闭、验证 PID 已死才收尾**（:34-36）。→ heartflow 对应「线下测试后必须真机验证」：job 的测试不能只断言状态文件，要断言进程真的没了。

**明确不做**：常驻 daemon（run-due 外部触发即可）、分布式编排、workflow DAG（pi 的 scripted workflow 是另一层，记录不搬）。

#### 设计规格：heartflow 队列三件套（排队 / 引导直接发送 / 队列编辑）

**需求回溯（`openmemory.md.bak`，用户原话）**：L226「队列模式,是在运行是发送新聊天,排在队列,等任务完成进行.还有借鉴qoder,引导功能:结合现在的消息,总结之前工作,明确后续任务后发送给ai」；L428「队列(多个队列也要支持和兼容.还有类似qoder的引导机制,但是默认是等上个任务完成,开启后一个)和多窗口会话」；L180「队列模式,当前工作时切换另一个section」。UI 计划侧：L445（T5 回合内输入与多队列——drain 要区分「注入当前回合」与「作为下一个任务开启」）、L446（T6 引导 overlay = 可编辑确认的两栏视图）。

**现状（代码实证，2026-10-21 重新核实）**：

- `FollowUpQueue`（`crates/cli/src/core.rs:19-95`）是**单 FIFO、上限 32**（:26），整队合并成**一条**注入（`merge_injection`，:101-107，`[queued follow-ups, oldest first]` 头）；`cancel_last` 只能撤最近一条（:81-83）。
- `HeartModel`（:116-160）= running 旗 + queue；注释自陈「队列只在回合运行时填充」。
- REPL：运行时入队（`crates/cli/src/main.rs:889-892`，满则提示 `/queue`）；回合边界 `deliver_queued_injection`（:947-976）把整队作为**一个新回合**递送，失败**整队**重排队。
- TUI：另有空闲输入暂存队列（`crates/cli/src/tui.rs:1825`）；回合内 submit 进 `app.model` 队列，`run_one_turn` 在回合边界推送合并注入（:2057 注释）；live turn 守卫（:1889-1891）——回合期间循环被 `run_one_turn` 内联 await 停住。
- Hermes 任务环（`crates/cli/src/plan.rs:249-346`）：串行 for + 每任务 `reset_for_task` 新鲜上下文 + 确定性 verify（:311，turn ok + 0 tool errors + ledger 空）。`.bak:438` 原话：任务环与队列「默认等上一个完成再开下一个」是**同一件事的两面**。

**借鉴 pi steering（源码实证，`control-channel.ts` + `steering.ts`）——递送语义必须一等地步**：

| pi 机制（行号） | heartflow 现状 | 落点 |
|---|---|---|
| `SteerDeliveryMode = "steer" \| "follow_up" \| "auto"`（control-channel.ts:64） | 只有一种语义（合并为下一条） | `DeliveryMode` 进队列项 |
| steer = 注入当前回合；follow_up = 排队为下一回合（revival briefs）；auto = runner 按忙闲定（:276-315 / :374-379） | 「注入当前回合」做不到（在飞回合被 await 停住） | 默认 FollowUp；Steer 挂前置（见 P-D） |
| 投递状态机 `routed/queued/scheduled → delivered/late/failed/recovered` + 六计数 + recent 20 保留（steering.ts:77-81/:56-74/:19） | 进队列后**看不见命运**（静默合并） | 队列项状态 + `/queue` 观测面 |
| 「child completed before consuming steering」诚实命名（:35-39） | 回合先结束 = 静默丢失语义 | 递送失败按**条目**重排 + 明示 |
| 预算结转（剩余 timeout/tool budget 带到替换 run，:217-241） | 无 | 恢复/续跑时结转 |
| FIFO 文本匹配认领（等文本的已接受 steer 认领最旧条目，:27-32） | 无 | 引导草稿回执核对 |

**数据结构（设计，未实现）**：

```rust
/// 递送语义（pi SteerDeliveryMode 同构）。
pub enum DeliveryMode {
    /// 注入当前回合——需要「在飞回合可被事件循环够到」前置，默认关闭
    Steer,
    /// 作为下一个任务开启（= 现行 drain_injection 的逐条精确版）
    FollowUp,
    /// 运行时按工人忙闲决定（默认）
    Auto,
}

pub enum QueueSource { User, Guide, Mission, Subagent }

pub struct QueueEntry {
    id: u64,
    text: String,
    mode: DeliveryMode,
    source: QueueSource,
    state: QueueEntryState,   // Pending | Delivered | Withdrawn | Superseded
}

/// 多队列 = 每 section 一条主队列 + 命名泳道（qoder 式「引导 lane」）。
pub struct QueueSet {
    lanes: BTreeMap<String, VecDeque<QueueEntry>>,
    next_id: u64,
}
```

**决策点（D 编号，待拍板）**：

- **D2.1 多队列形态**：per-section 主队列 + 命名 lane（.bak L428「多个队列也要支持和兼容」），还是全局单队 + 每项带目标 section？倾向前者——多 section（`tui.rs:1103` `MAX_SECTIONS=9`）天然对齐，「每 section 自有队列」即多队列的最小实现，且 L180「当前工作时切换另一个section」要求队列归属可视。
- **D2.2 合并 vs 逐条**：现行 `merge_injection` 整队并一条；pi 是逐条 steer（每条独立状态与收据）。**建议默认逐条**——逐条才能支撑队列编辑与状态可见；合并降级为 opt-in（`/queue merge`），它是模型上下文优化不是语义。
- **D2.3 默认语义**：确认 `.bak:428` 已写明的「默认是等上个任务完成,开启后一个」= `FollowUp` 默认；`Steer` 仅在前置落地后经 `Auto` 可用。
- **D2.4 队列编辑面**：withdraw（`cancel_last` 扩成任意条目）+ edit（取出回输入框回炉）+ reorder（上下移）+ 改派（换 lane/section）。REPL 侧 `/queue` 家族（list/edit/withdraw/merge/send-now）；TUI 侧键位落在 editor + reducer（.bak L655/L704-705 的落点映射仍有效）。
- **D2.5 引导直接发送**：`/guide <TASK>` 三段草稿（`build_guide`，core.rs:175-190，零 token 已就绪）→ **可编辑确认**（.bak T6 两栏视图）→ 确认后「直接发送」= 按所选 mode 入队或立即开跑，绕过暂存。qoder 式「结合现在的消息,总结之前工作,明确后续任务」正是 work_log/state/task 三段的现成实现。
- **D2.6 进度行**：.bak L226「对话栏上留一行显示大进度,但是不要扣减他的进程」= 队列/任务环进状态栏一行，不干扰主 transcript（与 §5 pi-footer 条目合流）。

**推进相**：

- **P-A 多队列数据结构**（core.rs 纯状态、离线可测：lane/entry/mode/source/state + 不变量「条目不丢、withdraw 可还原、递送后状态必终态化」）——零 UI 依赖，先行。
- **P-B REPL 逐条递送**：`deliver_queued_injection` 改逐条 + 失败按条目重排（不再整队）；`/queue` 家族命令。
- **P-C TUI 三件套**：回合内输入路由补 mode 与计数显示 + 队列编辑键位 + 引导 overlay（T6）。
- **P-D Steer 模式（注入在飞回合）**：**前置 = 在飞回合必须能被事件循环够到**——`run_one_turn` 内联 await + live turn 守卫（`tui.rs:1889-1935`）是根因；与 P2 并发模型、§4 kimi 状态机 ReAct、§7.2 优雅停机同源，排在其后。
- **P-E 统一语义**：与 Hermes 任务环合流（.bak L447 T7：任务环进度渲染 + 与多队列统一）；队列项可升级为 §1 job（跨会话存活 + 调度）——**「提交后不必原地等」从队列跃迁为 job 的点**。

**明确不做**：强插打断当前回合（pi 的 steer 也是 runner 在安全点消费，不是抢断 LLM 调用；heartflow 的 abort 边界问题见 §4 状态机条目）；跨机器队列同步；合并为默认（D2.2）。

**落地状态（2026-10-21，P-A/P-B/job MVP 已上线）**：

- **队列三件套（REPL 侧，TUI 的 P-C 另行）**：`FollowUpQueue` 升级为逐条 entry 队列——每条带 id/来源（user/guide/subagent）/状态，逐条递送（D2.2 默认），失败按条目重排队首；终态台账（recent 20，pi steering recent-20）保证「条目不丢、递送必终态化」（测试钉死不变量）。`/queue` 家族：send（手动排队）/edit/remove/move（D2.4 编辑面）/merge（opt-in）/pop/clear。**引导直接发送（D2.5）**：`/guide <task>` 默认把三段草稿**立即作为下一回合发送**，`/guide draft` 保留旧打印流，`/guide queue` 入队。
- **pi job MVP（`runtime::jobs`）**：`JobRegistry` + `JobRunner` trait（执行与治理解耦，离线可脚本化测试）；状态集取上游 async-run 集与 agent 集**交集子集** `queued/running/complete/failed/stopped`（当日网络核验过两集并存）；并发槽位（默认 3）+ 积压上限（默认 20，满则拒绝）；磁盘 `status.json` + `events.jsonl` 追加流（写失败计数不阻断）；进程死亡对账（启动时 stale queued/running → stopped）；**终态附注纪律**（stopped/failed 的 note 明示「任务未完成」，防监工把截断当成功）；**reason 三分法**（Progress 不唤醒监工，Result/失败经队列注入成为下一回合）。
- **job 墙钟超时（2026-10-21 调研批次补全，§8 业绩标准 #1）**：`DEFAULT_JOB_WALL_TIMEOUT`（30 分钟，对齐 counterpoint 的「30 分钟硬顶在启动时刻武装」与 codexmon WallSec 层）；`HEARTFLOW_JOB_TIMEOUT_SECS` 覆盖（`0` 关闭）；看门狗线程在 `Queued→Running` 翻转时武装（排队等待不计费）；`terminalize` 幂等化——**首个终态裁决获胜**，超时 Stopped 不可被协作 runner 稍后 unwind 出的 Ok/Err 覆写为 Complete/Stopped（测试钉死）；超时附注带 "wall-clock timeout … NOT finished"。
- **监工-工人 MVP**：监工 = 主 REPL 会话；工人 = `/jobs spawn` 的隔离会话（独立 Session/client/权限面），默认 **read-only**（`--write` opt-in workspace-write，full 永不下发）；工人无 spawn 面（深度=1 结构护栏）；协作式 stop（在飞调用不中断，结果丢弃 + 附注）。
- **mission 账本（2026-10-22 批次，§1/§4 P-A 落地）**：`runtime::mission`（`MissionLedger`）——四态 `Active/Paused/Blocked/Complete`（kimi `GoalStatus`）+ 三轴预算（token/turn/wall_ms，kimi `GoalBudgetLimits`）+ 判据五型（`CommandSucceeds`/`FileMatches` 机械型完成时**当场重验**，`HumanApproved`/`NoDiagnostics` 须显式 approve 收据，`FreeText` 唯一弱证据型）；**D1.1 反过早完成/反过早放弃是类型错误不是提示词**——complete 全判据过验才放行（弱证据-only 拒绝 `WeakOnly`）、blocked 需连续 3 次声明且 productive turn 清零连击；**D1.2 预算不杀**——≥90% 提醒一次、耗尽打标停提醒不关 mission；落盘 per-mission `state.json`（temp+rename 原子写 + 256KiB 拒写不截断 + 失败计数）+ receipts（上限 50）+ `events.jsonl`；锁污染一律 `PoisonError::into_inner` 恢复（本仓纪律，棘轮零 debt）。REPL `/mission` 家族 + doctor 磁盘级统计段。诚实边界：~~turn 手动记账（自动注入前置 P2）~~（2026-10-23 已补：`turn_reminder` 每回合系统提示注入 + `auto_record_turn` 单活跃自动记账，见 §11.3）、nodiag 暂人工批准（rust-analyzer 集成属另一量级）；落盘已并入 §4 跨进程文件锁。
- **scheduled-runs（2026-10-22 批次，§1 pi-job #6 落地）**：`runtime::schedule`（`ScheduleStore`）——`once`/`interval` 两触发型（pi 九动作的 MVP 子集）、**overlap=skip 唯一重叠语义（skip 记入历史不静默吞）**、`catchUp none|latest`（none 错过即诚实记 skipped-consumed）、`--quiet` 成功不唤醒监工、history 上限 100；`run_due` **先预订 `last_run_ms` 再派发**（调用方崩溃也不无限重 fire）+ `record_spawn_refused`（积压满诚实记 failed，interval 下周期重试）；REPL 三个 drain 时机（启动/回合边界/`/schedule run-due`）把到期 schedule 派为**普通 read-only 工人 job**——调度零新增执行机制；job 终态经链接表对账回写 schedule 历史（「结果落盘 ≠ 退出证明」）；落盘单 `schedules.json`（原子写 + 256KiB + 损坏不崩溃）。诚实边界：once 只支持相对/epoch 毫秒、calendar 不可表达。**chrono-tz 依赖评估（2026-10-22，结论：可行、缓做）**：本地 `HH:MM`/IANA 触发需要「本地时区名 + 规则库」两件——`iana-time-zone`（极小，取本机 IANA 名）+ `chrono-tz`（内嵌全量 IANA 数据库，编译期 +10s 级、二进制 +~1MiB）；本仓无 chrono，引它即连 chrono 一并进树。DST 边界（不存在的时间、重复时间）是 chrono-tz 的成熟处理面，自研 TZif 解析不划算。裁决：epoch-ms + 相对偏移已覆盖调度自动化场景，等真实本地定时需求出现再引，避免为不存在的功能付编译期税。
- **队列命名泳道（2026-10-22 批次，D2.1 数据结构半边）**：`QueueEntry.lane` 标签 + `send_in_lane`/`lane_names`/`items_in_lane`；REPL `/queue send --lane <name>`、`/queue lane <name>`、列表 `@name` 标注。**纪律：泳道是观测与编辑维度，递送仍是全局到达序**——跨泳道轮转规则需要 TUI 侧（P-C）才有意义，离线拍脑袋定轮转属猜测。
**落地状态（2026-10-21 第二批网络核验，Qoder 官方文档对照）**：Qoder 官方「排队 vs 插话」二分（docs.qoder.com conversation-input）与本仓 `FollowUp` vs `Steer`（P-D）**语义同构**——插话「在安全执行边界交给 Agent，不直接停止当前执行」正是本仓「明确不做强插打断」的官方版；排队消息管理面「插话 / 撤回到输入框编辑 / 移除 / 多条重排」= `/queue send/remove/move/edit` 已覆盖；「设置 → 运行中发送方式」= D2.3 默认 FollowUp；composer 在「压缩中 / 有待处理请求」时提示不支持插话 → TUI P-C 待做项。qoder 式「引导 = 结合当前消息、总结之前工作、明确后续任务后发送」= `/guide` 三段草稿 + 默认直接发送，已实现。生态侧：ACP `_session/steering` 扩展（Claude/Codex 已支持，agent-shell 0.78 的 RET=排队 / M-RET=插话）→ 若将来做 ACP server 可对齐；灵码（Qoder CN）的「消息排队 + 全自动执行模式带终端命令禁止名单」中，禁止名单与既有 `is_dangerous_command` 分级确认同族。

- **待做**：~~P-C（TUI 队列编辑键位 + 引导 overlay——本仓铁律：TUI 派发不盲改，需真机冒烟）~~ **已落地（2026-10-24，§11.1）**：队列编辑面板（Ctrl+Q：编辑/撤条/重排/改派）+ guide overlay 两栏可编辑（T6）+ 状态栏 mode 分流计数；REPL 补 `/queue relane`。~~per-section 泳道递送轮转（D2.1 的 TUI 半边）~~ **已落地（2026-10-24，§11.4）**：section 即泳道、跨 section 公平轮转 + TUI 默认逐条递送（D2.2 补全）+ 回合事件按 section 路由、回合中导航解禁。mission 回合自动记账与提醒注入 ~~（D1.3，前置 P2 事件循环）~~ **已落地（2026-10-23，§11.3）**。跨进程文件锁已落地（§4，2026-10-22 批次）。**P-D Steer 已落地（2026-10-23，§11.2）**：`DeliveryMode`（Steer/FollowUp/Auto，默认 FollowUp）+ runtime `SteerMailbox` 迭代边界注入 + TUI Ctrl+S 全链，未消费 steer 回收为 follow-up（条目不丢）。


### 4. 工程稳定性与持久性（大项目持续运行）

**部分落地（2026-10-22 批次：停机安全 + 长期运行）**：
- **队列耐久化**：pending + 终态台账同步落盘 `sessions/queue.json`（原子写、每次变更即写）；恢复时 `next_id` 从条目重建、limit 钳 1..=4096、pending 超限诚实截断保最老；**投递中途崩溃 = 重投（at-least-once）**，绝不 at-most-once 丢失；损坏降级空队 + 明示 + doctor 告警，绝不拒绝启动。混沌测试钉死（roundtrip/损坏/防回退/重投/钳制/截断 6 条）。
- **退出即对账**：EOF/`/exit`/`/restart` 三路径统一收口 `JobRegistry::finalize_on_shutdown`——非终态 job 立即 Stopped + 「宿主退出，输出为部分」note 落盘，首个终态获胜不可被 late completion 覆写（测试钉死）；不必等下次启动 reconcile。
- **抗损坏台账闭环**：mission load/summarize 对非 UTF-8 state 文件从「静默跳过（永久不可见）」改为「计入 corrupted 台账」——混沌测试抓出的真缺陷；垃圾/截断/错类型/错形状/缺文件五类损坏全部跳过计数、健康邻存不受染、`next_id` 不回退、doctor 可见。
- **跨进程文件锁（2026-10-22 批次）**：`runtime::filelock`（`with_exclusive` + `lock_path`）——sidecar `<data>.lock` + OS 建议性锁（Windows `LockFileEx` / Unix `flock`，经 `fd-lock` 封装：本仓 `unsafe_code = forbid`，平台 unsafe 收在实战检验的依赖内，锁语义自持）。**为什么必须锁**：temp+rename 只保证单次写原子，两进程交错 `写 tmp → 截断 tmp → rename` 会把对方的字节 rename 成自己的名字；**为什么锁 sidecar**：rename 会替换数据文件本体，锁在旧文件对象上等于没锁。设计纪律：bounded 等待 5s（1ms→50ms 退避）超时**跳过写并计数**（绝不无锁裸写）、锁失败与写失败同入 `DISK_WRITE_FAILURES`（doctor 可见）、读者不取锁（rename 快照读本就原子，陈旧快照优于启动阻塞）、**同线程禁止嵌套**（内层无法区分自持与外占）。接入面：schedules/mission `state.json`/job `status.json`/queue.json 四处 persist + mission/job 两处 `events.jsonl` append；顺带把 job `status.json` 从裸 `fs::write` 升级为锁内 temp+rename（裸写崩溃留撕裂文件，破坏启动对账）。测试 7 条含**真跨进程互斥**（重执行测试二进制为持锁子进程，父进程 bounded 尝试必超时、子进程退出后内核释放锁可获取）与双线程 RMW 序列化（无锁必丢更新）。诚实边界：这只序列化写，不是多写者数据库——各进程内存仍是事实源，并发写世界仍是「最后落盘者胜」；真正的单写者 World 仍是 P2。**遗留记录**：~~job id 是进程本地序号，两进程共用 `JOBS_ROOT` 会逻辑互踩~~（2026-10-24 已修：job 目录 `job-<pid>-<id>` 命名空间 + `JobRecord.pid` + 对账三分法——自己的/遗留的照旧停；外来 pid **探活**（Windows `tasklist`/Unix `ps`，不确定视为活），活则不动（第二进程不再误停第一进程的在飞 job）、死则停并注「owning process N exited」；残留：pid 复用可致死主 job 滞留 running，pi 的 process-terminal proof 是后续答案）。
- **依赖治理：终端栈收敛 + dependabot 分支裁决（2026-10-22 批次）**：4 条 dependabot 分支按「最优工程取舍、依赖尽量最新」裁决——**采纳 2、结构化采纳 1、否决后超越 1**。① `actions/checkout v4.4.0→v7.0.1`（release.yml 三处钉 SHA，与 ci.yml/docs.yml 对齐）；② `jsonschema 0.57→0.59`（使用面仅 `validator_for`，编译即验证）；③ **ratatui 0.29→0.30.2 结构化迁移**：dependabot 机械 bump 实证是错的（lock 里 ratatui 0.29+0.30 双拷贝），正确路径 = tui-textarea 钉上游 main rev `0ce5d4b`（crates.io 0.7.0 仍钉 ratatui ^0.29；其 `no-backend` feature 弃掉自带 crossterm 0.28 模块）+ 自写 `editor::tui_input`（crossterm 0.29 KeyEvent → tui-textarea `Input`，逐行镜像上游语义含 Release 事件吞咽防 Windows 双写）+ `Backend::Error` 关联类型适配（0.30 新增；`CompanionTerminal` 收紧 `Error = io::Error`，测试新增 `IoTestBackend` 把 TestBackend 的 `Infallible` 抬升）。**成果：crossterm 双拷贝收敛为单 0.29（与 inquire 0.29 天然同源），比升级前少一个拷贝**；`unstable-rendered-line-info` 移居 ratatui-widgets（umbrella 不再转发，直连开启）。④ crossterm 单独 bump 0.28→0.29 的分支**否决**（ratatui 0.29 钉 0.28，单独升是错）。防再犯：dependabot.yml 对 ratatui/crossterm 加 ignore（解除条件 = tui-textarea 0.7.1 发布）。全树 `cargo update` 刻意不做（brotli 8→9 等无关 major 会破坏可归因性，另列任务）~~已执行（2026-10-24，9209aa8）~~。
- **soak 压缩水位（2026-10-24 落地，`cargo bench -p heartflow-runtime --bench soak`）**：400 回合（工具往返+回复各一次=800 请求）穿越真循环——小窗口使多次压缩与校准重拟合真实发生。三探测：每回合中位耗时 **0.14→0.17→0.15→0.16ms 无漂移**（3x 界内，算法性泄漏无）、保留 184 消息/72KiB（压缩设计生效）、全程零 panic。unsafe_code=forbid 排除分配器钩子，三探测（漂移/保留/结构）皆可移植。小时级真机 soak 仍属陪测面。
- **实验构想（2026-10-24，全部可真测、按长期收益排序，欢迎与用户陪测）**：① **夹具回放即基准**——criterion 片的热点输入直接用已录真实夹具（估计器/markdown/渲染缓存），合成数据会漂、真实形状不会，基准与回归网共用同一事实源；② **Steer 吸收延迟遥测**——Ctrl+S 时间戳 → 下一个请求携带该文本的时间差，聚合进 `hf doctor`（Steer 全链的第一性可测质量指标，若 p95 超 2 个工具步即暴露调度问题）；③ **自托管 dogfood 冒烟 ✅ 已完成（2026-10-24）**——`hf prompt` 读 `release-qa.md` 回答结构三问（节数/末节标题/未勾项数），**三问全对**（真机对照核验）：自托管实验成立，每轮发版前可跑；同时以实证关闭 brotli 残留（见 QA 清单）；④ **fuzzer 种子真实化**——reducer/queue fuzzer 的初始状态从真实会话 jsonl 播种（真实形状 > 合成分布，bug 在真实数据形状上聚集）。本地克隆仓库七批次的可采面均已挖掘入库（§1/§4/§7/§10），`.workbuddy/memory` 为用户工作日志（非外部仓库），`_study/breutil/edit` 为空——**外部研读面已采尽，下一阶段增量来自实验与真实使用**。
- **关节打磨战役（2026-10-24 开工）**：架构关节逐边核验——分层边界零违规（runtime 不触 provider/api、api 不触 runtime、外围 crate 无 cli 反向引用）、错误类型面同构（per-crate thiserror 枚举 + RuntimeError(message,hint) 收口）、持久化纪律 6/6 全走锁内 temp+rename（裸 fs::write 全在测试）、锁纪律三模式一致（前轮已验）。**最大量化缺口 = `# Errors` 文档段**：已发布 crate 公共 API 计 235 处缺失（provider 57 / store 53 / tools 44 / runtime 37 / commands 37 / mcp 7 / cli+proxy 0），pedantic 提示级。战役模式：逐 crate 全清（每段写**真实失败模式**，禁样板谎话——先读实现再写），mcp 已打样清零（7→0）。剩余按 crate 依序推进，优先 provider（对外公共面）。**进度（2026-10-24）**：mcp 7→0、tools 7→0、runtime 37→0、store 53→0（四路全清，子代理逐实现核实）；provider 57 子代理进行中；commands 经代理核验为**基线即零**（早期 per-crate 计数把 clippy 连带重放的 runtime 告警误记到 commands——其唯一导出函数返回 Option，lint 本就不覆盖）。**provider 清零后该战役即完成——2026-10-24 全战役收口**：工作区级计数归零（工作区复验同时揭穿基线虚高——早期 per-crate 计数把连带重放的依赖告警重复计入，真实清单 = mcp 7 + tools 7 + runtime 37 + store 16 + api 15 + provider 5 ≈ 87 段，全部以真实失败模式写就）。
剩余按 crate 依序推进，优先 provider（对外公共面）。
- **停机矩阵（2026-10-24 系统化，缺口审计结论：片断覆盖已齐，补映射文档 + 两个残留判决）**：{回合中/压缩中/落盘中/启动对账} × {崩溃/取消/退出} 的持久化仲裁逐点已有机制与测试——回合中取消仍落盘（TUI `double_interrupt_cancels_the_live_turn`）；落盘中崩溃=锁内 temp+rename（`filelock` 跨进程测试含进程死亡释放）；队列/mission/schedule/job 撕裂文件各自降级测试在（corrupt→空队/计入台账/不崩溃/skip 计数）；会话对（快照+jsonl）撕裂矩阵全覆盖（`drops_torn_segment_tail`/`rejects_...broken_interior`/`base_moved`/`unknown_version`/BOM）；段→快照回退带 stderr 注记（`load_saved_session`）。**残留判决**：① 回合中**硬崩溃**（非取消）丢失在飞回合——设计取舍（回合边界快照 = agent-loop 惯例），ds4 磁盘 KV 检查点式的高频检查点是可选升级、待真实丢失事故再议；② 撕裂快照 → `--resume` 响亮报错拒绝——快照有原子构造、损坏即外部损伤，诚实拒绝正确，不加静默降级。矩阵文档化于此，后续新增持久化面必须在此表登记行。
- **异步/并发/懒加载面审计（2026-10-24，结论：不需要优化，一项记录在案）**：锁纪律三模式逐点核验一致（恢复 `into_inner`：storage lock_state 等七处；降级显式 Err：MCP transport/折叠展开表——污染即拒绝，结果不变错；`if let Ok(guard)` 反模式零命中）。懒加载面：OnceLock 三处（theme/config 覆盖/log filter）+ 条件注册（rga），无双重初始化路径。async 面：spawn_blocking 承载全部阻塞泵（crossterm/bash），TurnStream Drop-abort 有测试，unbounded 通道生产端皆用户速率。**唯一候选记录在案**：jobs 槽位获取是 200ms 轮询（非 Condvar）——正确性有测试，代价 ≤200ms 派发延迟 + 积压×5 次/秒唤醒；不换 Condvar 的理由：触碰首终态获胜仲裁的最敏感同步面，换来的延迟改进对长任务不可感知。**触发条件**：job 变亚秒级任务或积压上限上调时重议。
- **抗冲击测试战役（2026-10-24 开工，第一片落地）**：三路 fixed-seed property fuzz 钉住核心不变量——① queue 全操作面 fuzzer（400 步随机操作序 × 镜像对照：id 不重用、pending 唯一、每 id 恰在一处、**盘上台账恰为终态史最新 ≤20 条**——fuzzer 过程把「台账=有界窗口」从注释升格为断言）；② TUI reducer fuzzer（2000 步随机 Msg：全键族/全事件变体/含控制字节与超长 paste/越 section 流量，无 panic + transcript 单调 + 队列不超限）；③ 配置面敌意文件 fuzz（300 例随机字节/错型 TOML/深嵌套/超长串进 keymap+settings 装载器，永不 panic、风暴后干净文件满功能恢复）。种子固定，任何失败精确复现。**后续片**（按序）：criterion 基准 harness（§9 已解锁：estimate/markdown/渲染缓存三热点先量化）、停机矩阵（回合中/压缩中/落盘中各断点 × 崩溃/退出/取消的持久化仲裁）、soak（长会话内存/句柄水位）。- **依赖树重复版本审计（2026-10-24，`cargo tree --duplicates`）**：17 族重复，逐族裁决——transitive 解析现实（getrandom/syn/winnow/hashbrown/toml/windows-sys 等，Cargo 只统一 semver 兼容版本，跨 major 重复是上游发布节奏所致）**不强钉**（`--precise` 反向钉制造升级摩擦，违长期收益）；我方仅 **base64 双拷贝**（tools 直依 0.23、reqwest 链带 0.22）——裁决：不反向降钉，reqwest 上游跟进 0.23 时自然收敛。长期机制（业界 2025-26 组合，按本仓「作者自推本地跑」CI 纪律适配为**本地门禁**）：`cargo deny check`（bans duplicates/unused + CVE + license）、`cargo machete`（未用直依）、`cargo semver-checks`（发版前 API 破坏面扫描，本仓 9 crate 公共面发 crates.io 的直接保障）——命令已入发布 QA 清单第 1 段，**已启用（2026-10-24：machete 0.9.2 + deny 0.20.2 装机，首跑 advisories/bans 全绿、无 CVE；machete 唯一命中 ratatui-widgets 为特性承载误报——文本匹配看不到 feature 接线，已加 metadata.cargo-machete ignored 并注释）**。
- **待做**（依赖排序）：在飞回合取消 ~~（double-Ctrl+C，前置 P2 事件循环可够到在飞回合）~~ **已落地（随 §11.0，2026-10-22）**、cassette 录制扩展为 ds4 式「模型加载/提示渲染/工具调用/会话状态」集成测试 ~~已落地（2026-10-24，§11.5：两真实夹具 + 离线回放套件）~~、发布 QA 清单落成仓库内可勾选文档 ~~（ds4 `QA_BEFORE_RELEASES.md` 纪律）~~ **已落地（2026-10-24，docs/src/release-qa.md，含历史坑登记）**、全树 `cargo update` ~~（无关 major 的传递刷新，独立小步验证）~~ **已落地（2026-10-24，§11.5）**：126 包传递刷新（brotli 8→9 / rand 0.9→0.10 / shlex 1→2 / windows-sys 0.60→0.59 收敛等，全部传递依赖、仓内零直接引用），更新前后全门禁 + doctor 冒烟仲裁。诚实边界：brotli 9 的真实网络解压路径不受离线测试覆盖，下次真机会话顺带即验。

目标：让 heartflow 能**维持大项目的持续运行**，现在只记录设计，不执行。

| 借鉴源 | 机制 | heartflow 落点 |
|---|---|---|
| pi-job-listener | job 注册表跨 `session_shutdown` 存活；adopt 旧 pid+log；Windows kill 不可验证时诚实报 `KILL UNVERIFIED` 而非假装杀干净；作者自陈未用 Job Object（`CreateJobObject` + kill-on-close）是正解 | bash 长命令的会话外存活 + 退出码诚实降级；Job Object 是 Windows 侧标准答案 |
| rpiv-todo | branch replay 让 todo 跨 compact / `/reload` 存活；blockedBy 环检测 | `todo_write` 状态的压缩免疫 |
| pi-subagents missions | durable mission + receipts + goal token 预算 + needs-attention 通知 | §1 监工的耐久账本 |
| ds4（antirez DwarfStar） | session KV 快照 `/save` `/list` `/switch` `/del` `/strip`（剥掉大 payload 只留文本）；**cooperative interruption**（Ctrl+C 协作式中断） | 会话存储经济性；工具执行取消语义 |
| ds4 | `/hints on\|off`——让 agent 附带「编程选择背后的理由」 | 监工-工人汇报协议的原型：工人上报必须带理由链 |
| ds4 | 「模型加载/提示渲染/工具调用/KV 状态/HTTP 服务/编码 agent 一起集成测试」 | 对应本仓 cassette 离线回放夹具的扩展方向 |

**ds4 对 heartflow 的总体判断**：它是本地推理引擎（Metal/CUDA/ROCm），heartflow 是远程 API 客户端，**引擎层无可借鉴**；可指导的是 agent 形态四件——原生 agent 循环（状态同置）、会话快照经济、协作式中断、集成测试纪律——以及它的发布哲学：软件是「供用户用 coding agent 自行改造成工作模板」，而非覆盖一切setup 的成品。

#### 借鉴点子（ds4 + pi-subagents missions/schedules 深度研读，待落地 §1/§4）

**ds4（`AGENT.md`，antirez）四条工程纪律直接对应「大项目持续运行」**：

1. **正确性先于速度**：「Preserve correctness before speed. Do not keep a faster path with unexplained attention/KV cache/logits drift」（:13）。→ heartflow 铁律：任何「更快路径」若行为有未解释漂移，一律不保留；对应本仓「安全降级 vs 结果变错」判据。
2. **长会话磁盘检查点**：「Make long local agent sessions practical through live KV reuse and disk KV checkpoints」（:14）。→ heartflow 应把长项目的上下文/会话状态做磁盘检查点，使超长会话跨重启/压缩存活（与 §1 mission ledger 同源）。
3. **单写者实例锁**：「Do not run multiple huge model processes concurrently. The instance lock is intentional」（:29）。→ 对应 P2 并发模型「单写者 World」——共享资源用一把有意保留的锁，而非并发。
4. **跨后端回归隔离 + 变更测试矩阵**：「SSD streaming/CUDA/distributed/Metal 互不影响」（:10）；「每主变更必验 Metal/SSD/分布式/CUDA 四路径，CUDA 需向用户要机器真测」（:51-56）。→ 对应本仓「分层边界不可破」+「线下测试后必须真机验证」；跨平台路径必须有真实机验证闭环。
5. **压缩策略（ds4 `misc/COMPACT.md`，直接对照本仓 `compact.rs`）**：
   - **双触发**：软触发 = 当前 transcript ≥ 85% 上下文 或 剩余 ≤8192 token（小 ctx 时阈值封顶为 1/4 ctx，:8-12）；硬触发 = tool result 塞不进时立即压缩并**只重试 append 一次**（:13-14）。→ heartflow `compact.rs` 当前只有「按条数切」的单一触发，缺剩余 token 软触发与「塞不下即压缩重试一次」的硬触发；
   - **重建公式**（:18-32）：用 live session 让模型产出「截至 `BOTTOM` 的 durable task-state 摘要」→ 回滚到 BOTTOM（摘要请求不进用户可见会话）→ 取最近 **verbatim tail**（≤10% ctx、封顶 50k token、对齐 `<｜User｜>` 边界）→ 重建 = system + 摘要 + tail。→ heartflow 当前是「丢弃旧历史」式，应改为「摘要 + 保留近期原文尾」式，最活跃的工作区保持原样采样；
   - **摘要契约**（:38-56）：只保留 durable task state（目标/约束、已读/改文件、已跑命令与关键结果、决策/被拒方案/已知 bug、下一步）；**不编造事实**、可重载大体积数据只记 path/range 供工具再取；摘要提示词明令「总结后停手、不继续用户任务」（:53-56）。→ 直接可抄作 heartflow 的 compact prompt 规范；
   - **硬控制边界**（:58-61）：`</think>` 与 DSML 视作硬边界，压缩时绝不执行工具、绝不把标记写进重建态。→ 对应本仓「流式两路径不按 delta 重解析」的同类护栏；
   - **UI**（:63-69）：显示高强度 `COMPACTING` 标记、流式展示摘要、再显示重建进度。→ heartflow 压缩目前静默，需显式状态标记。
6. **live continuation / 磁盘 KV 检查点（ds4 `misc/RESPONSE_API.md` + `misc/ANTHROPIC_LIVE_CONTINUATION.md`）**：
   - 核心判据：「采样得到的 KV 态是 ds4 最忠实的状态」（`ANTHROPIC_LIVE_CONTINUATION.md:7`）；工具调用由 `tool_use_id` 绑定到 live KV 前沿，续跑只 append 后缀、不重建整上下文；
   - **磁盘检查点按可见 transcript 为 key 还原隐藏 KV**（`RESPONSE_API.md:246-250,305-324`）：跨客户端/会话切换、进程重启后从 `key=responses-visible` 恢复，只补 prefill 新后缀（:355-373 重启恢复实测）。→ heartflow 作为客户端，对应「会话状态做磁盘检查点跨重启/压缩存活」（与 §1 mission ledger 同源）；
   - **拒绝孤儿 tool id 即 400**（:271-272,399-406）：未知 `call_id` 且无同请求内 replay 的先前调用 → 明确 400 要求重放完整历史，绝不冷 prefill 一个裸 tool result。→ 直接对应本仓 `compact.rs` 已知关联债（孤儿 `tool_result` 不剔除，:109）；ds4 的「先查 live 绑定、无则 400、不盲目修复」是正确范式；
   - **反模式**（:70-71）：「不要加 RAM 快照缓存去掩盖 live-continuation 不匹配」——不匹配的根因是协议已用 id 绑定前沿，不是缺回退检查点。→ heartflow 不要为掩盖压缩/续跑不一致而堆内存缓存。
7. **per-provider compat 矩阵（ds4 `docs/CLIENTS.md`）**：Pi 客户端配置里的 `compat` 块（`supportsStore` / `supportsDeveloperRole` / `supportsReasoningEffort` / `thinkingFormat` / `requiresReasoningContentOnAssistantMessages` / `maxTokensField` 等，:27-35）把「各 provider 能力差异」显式成能力标志位。→ heartflow `crates/provider` 已桥 Anthropic/OpenAI Chat/Responses，可同样抽一层 `Capability` 标志矩阵，避免在每个请求构造处打 `if provider == x` 补丁（与 §5 工具面能力判断同源）。**kimi-code 的 `@moonshot-ai/kosong`（源码实证，`packages/kosong`，自述 "LLM abstraction layer"，直接依赖 `@anthropic-ai/sdk` + `openai` SDK）是这层的现成参照实现**——heartflow `api`/`provider` 收口多协议客户端时可对照其抽象边界与 per-model `protocol` 选择（`kosong/src/catalog.ts`）。**§7 deepseek-recipe 同族**：它按请求的 `model` 名解析模型版本选 prompt 编码（`ConversionOptions::with_model_alias` 注册别名），且 V4 不收图片就在转换期替换为 `[Unsupported Image]`——「能力降级在转换层显式完成」正是 compat 矩阵的落地形态。
8. **发布前 QA 纪律（ds4 `QA_BEFORE_RELEASES.md`，145KB 发布 QA 清单 + `RESPONSE_API.md` 每设计附 QA Checklist/QA Log）**：每个协议/行为改动都带「单元/集成/回归」三档检查 + 真实客户端（Codex/Claude Code/Pi/opencode）实测 trace。→ 对应本仓「分层边界不可破」+「线下测试后必须真机验证」；建议把发布 QA 清单落成仓库内可勾选文档（与 P2 并发模型验证同源）。

**pi-subagents `docs/missions.md`（耐久任务账本，待落地 §1/§4）**：

- **durable `state.get/set`**：每 mission 一个 `state.json`，文件锁 + 原子写 + 256 KiB 上限（:24）——跨回合/跨重启的持久小状态，worker 与监工共享；
- **goal mission + token 预算 + needs-attention**：`goal:true` 带 `budget.tokens`，idle 时每回合末发「剩余预算 + 下一就绪动作」通知；到预算转 `budget-exhausted`、停通知但不关 mission（:42-64）——这正是「大项目持续运行」的驱动器：监工用预算驱动，不自己决策启动；
- **schedule.run-due 外部启动（无需常驻 daemon）**：定时任务由外部 launcher 调 `schedule.run-due` 触发到期工作，pi-subagents 本身不必是守护进程（:158）；`quiet` 模式（成功不唤醒父会话，:126-157）、`overlap=skip`、保留上限 200 条（:142-162）。→ heartflow 的「job-listener / 维持大项目」可同样不做常驻 daemon，而由 heartbeat 或外部定时器触发到期任务；
- **压缩/重启后从 ledger 恢复为第一动作**：`mission.list`/`mission.show` 先于一切（:74）——与 §1「会话继承重匹配」同源，都是「恢复优先于新动作」。

**已知关联债（2026-10-19 重新核实，已收窄）**：旧描述「`compact.rs:319` 按条数切 + `conversation.rs:245-323` 只 stub 不剔孤儿 `tool_result`」**部分过期**——孤儿防护已有两层：`compact.rs:329-331` 切割边界回退（尾部绝不以孤儿 `tool_result` 开头，测试 `compaction_never_orphans_a_tool_result_at_the_boundary` 在）+ `conversation.rs:372-410` `repair_tool_pairing` 每请求安全网（丢弃无匹配 `tool_use` 的 result，:549 与 `build_replay_messages` 组合应用于每个请求）。剩余债务 = ①按条数切导致保留尾无 token 预算、②无硬触发、③截断摘要信息损失、④续延句缺省略量声明。完整改造规格见下节，按本仓「先埋点观测，有证据再改」推进。

#### 设计规格：compaction 完整改造（ds4 双触发 + kimi-code recipe 收口，**部分落地 2026-10-21**）

**现状核实（代码实证）**：触发 = `should_compact_with_estimate`（`compact.rs:244-257`）单阈值（`context_window_tokens/2` 或绝对 `max_estimated_tokens` 默认 10k），带 `TokenCalibration`（:59-154）provider 上报 usage 仿射校正（451 turn 实测中位误差 62.4%→12.0%）；切割 = `compact_session_in_place:319` 按**消息条数**（`preserve_recent_messages` 默认 4）+ 边界回退；重建 = 续延 system 消息 + pinned + 保留尾（:354-362），摘要为**确定性截断**（`summarize_messages:379-404`，每消息 80→240 char 近因 ramp）非模型生成；续延句已有「continue from where it left off」+ 可选「preserved verbatim」+ 抑制追问（:275-294）。

**落实状态（2026-10-21 批次，188 runtime 测试全绿 / clippy `-D clippy::all` 干净 / panic 棘轮 debt=0）**：

- **Phase 0 埋点观测 ✅ 已落地**：`CompactEvent` 结构化事件 + JSON 落盘（`set_compact_stats_path`，路径 `RwLock<Option<PathBuf>>` 注入）+ `repair_tool_pairing` 丢弃 warn 带 `tool_use_id` + `hf doctor` compaction 统计段（`repair_dropped > 0` 提示证据门）。
- **Phase 1a 软触发口径（D4.1）✅ 已落地（配置门控）**：`CompactionConfig.soft_trigger: SoftTrigger`（`HalfWindow`（默认 = 现状）| `Ds4`：`calibrated ≥ 85% × window` 或 `remaining ≤ min(8192, window/4)`，按 ds4 原文 `misc/COMPACT.md:8-12` 实测更正——路线图早前写的 `max(8192, window/8)` 与原文不符）；env `HEARTFLOW_COMPACT_TRIGGER=ds4` 启用。默认关闭（回滚规格：新行为挂配置、默认 = 现状）。
- **Phase 1b 硬触发（D4.2）✅ 已落地（2026-10-24，配置门控；前置 cassette 已录，§11.5）**：落点经代码核实定在**错误路径**——provider 以「超上下文窗口」拒绝请求（`AgentEvent::Error` 文本）是会话装不下的唯一真值（估计会漂，400 不会；而软门在估计值上恒先于 100% 触发，预估式硬触发是死代码）。`is_context_overflow_error` 分类器按三方言稳定措辞匹配（OpenAI/DeepSeek「maximum context length」、Anthropic「prompt is too long」/`context_length_exceeded`、Responses「exceed context limit」，测试钉死正反例）；命中且 `hard_trigger` 开（env `HEARTFLOW_COMPACT_HARD_TRIGGER=1`）且窗口模式 → `compact_with(WindowFull)` + **重试该请求恰好一次**（`hard_retried` 每回合一次；第二次拒绝如实失败，毒会话不可能在此循环）；新触发档 `ThresholdKind::WindowFull` 进观测事件与统计轴（`window_full`，旧 stats 文件经 `unwrap_or(0)` 兼容）；重试经 `AgentEvent::Truncated` 明示「N older messages compacted, retrying once」。循环级测试：溢出→压缩→恰好一次重试且第二请求更短；重复溢出→真失败且恰好 2 次调用；未武装→立即失败 1 次调用。
- **Phase 2 尾部 token 预算（D4.3）✅ 已落地（配置门控）**：纯函数 `plan_compaction_cut(messages, config) -> CutPlan { keep_from, tail_tokens }`（条数下界 + token 上界，前移吞消息、成对吞 ad+result、绝不空尾、单条独超预算如实上报）；env `HEARTFLOW_COMPACT_TAIL_BUDGET` 启用，默认 0 = 现状。
- **Phase 3a 省略声明（D4.4）✅ 已落地**：`get_compact_continuation_message` 增 `dropped_token_estimate` 参数，续延句补「Roughly N estimated tokens of older conversation were dropped here during compaction.」（N = 被摘要区域启发式估算，明标 estimate；pinned 排除在外）。
- **Phase 3b 摘要模式（D4.5）✅ 已落地（2026-10-24，配置门控）**：`CompactionConfig.summary_mode: SummaryMode`（`Deterministic`（默认 = 现状，离线可复现——已录 cassette 夹具即此模式）/ `Model`，env `HEARTFLOW_COMPACT_SUMMARY=model`）。`Model` 路径：软门触发时先以 `SUMMARIZER_PROMPT`（ds4 durable-task-state 契约：目标约束/文件/命令与结果/决策与被拒/已知问题/下一步；不编造；大体积记 path/range；**总结后停手、绝不继续任务**）发一次边路请求摘要将丢弃区域（输入截 48K 字符、输出截 16K），摘要文本进续延消息并明标「model-generated from the dropped region」；**任何失败（流错误/空回复/无 stop）降级确定性摘要——压缩本身绝不因摘要器故障而失败**；边路事件有意不进 transcript（非回合输出）。事件吞噬 + 降级路径均有测试钉死（成功注入 / 失败降级两分支）。诚实边界：硬触发路径刻意不用 Model 摘要（恢复要快）；`/compact` 与回合后自动压缩仍走确定性路径（仅回合内软门走 Model）。
- **Phase 4 不变量测试 ✅ 已落地（并当场揪出一个真 bug）**：`compaction_pipeline_preserves_tool_pairing_for_random_well_formed_sessions`——200 例确定性 xorshift 随机良构会话（混合 tool_use 广告/结果、pinned 回合）经 compact + replay + repair 后断言三不变量（尾不孤立、result 必有前序 ad、ad 必有后继 result）。**由 property test 揪出并已修**：`/pin` 若落在带 `tool_use` 的 assistant 消息上，其 result 被摘要后线上会悬空 ad（provider 400）——修复 = pinned 广告拖行紧随的 result 一并 verbatim 存活（`pin_following` 逻辑 + 专项测试）。
- **回滚**：新行为全部挂 `CompactionConfig` 新字段（默认值 = 现状），`tail_budget_tokens` / `soft_trigger` / `hard_trigger` / `summary_mode` 可单独回退；触发口径只影响「何时压」，不影响 transcript 结构不变量。**至此 compaction 完整改造七相全部落地**（0/1a/1b/2/3a/3b/4）。

#### 借鉴点子（kimi-code memory 机制，待落地 §4/§5）

kimi-code 的 memory 是「事件溯源 + 折叠」范式，与 ds4 的「摘要+tail」compaction 互补，两条 compaction recipe 可对照取舍：

1. **事件溯源持久化 + fold/replay（源码实证，`agent/contextMemory/contextOps.ts` + `state/eventDispatcherService.ts` + `wire/journal.ts`）**：`contextMemory` 不直接存，而是对事件 `ContextAppendMessage` / `ContextClear` / `ContextApplyCompaction` 做 fold——`defineState('contextMemory', …)`（`contextOps.ts:79`）挂 `.replayable({ blobs: { dehydrate/rehydrate } })`（:80-89）、`.undoable({ onUndo → resetFold(slice(0, cut)) })`（:90-95）、`.on(…)` 事件折叠（:98-106）；dispatch 顺序 = **先同步 `commit` 折叠进内存态、再由 `IWireService` flush/落盘 durable 事件**（`eventDispatcherService.ts:302-305/:364/:478`），`restore()` 时 `wire.readRestorable()/readJournal()` 重放折叠再注入（:302-305）。→ heartflow 落点：会话/回合应按「事件日志 + fold」建模，使压缩/重启/子代理恢复 = **重放折叠**（与 §1 会话继承重匹配、§4 磁盘检查点同源）；heartflow 已有 `AgentEvent` 流（P1 异步化 + cassette 保真 AgentEvent 而非 wire），`IAgentJournal` 正是「AgentEvent 落盘成可重放单一权威」的现成范式；「先折叠后落盘」保证内存态永远是最新的折叠结果（per-agent 持久化 journal 的具体落盘路径在实现层，粘贴稿的 `wire.jsonl` 字面量未逐字复核）。
   - **会话分叉（`wire/journal.ts` 新增借鉴点）**：`IAgentJournal` 带 `switchBranch(input)` / `branches()` / `nextSeq()` / `settled()` + `AgentJournalRef{ tree, branch }`（:3-30）——git-like 的**对话分支 / 回退到某一 turn**。→ heartflow `store`/会话可借鉴「对话可 fork / 切分支」：undo 是回退，branch 是并行探索，二者互补（对应 §1 监工在多个方案间分叉试探）。
2. **compaction recipe（源码实证，`agent/contextMemory/compactionHandoff.ts`，对照 ds4 `misc/COMPACT.md` 与 `compact.rs`）**：消息形态已逐字核实——`[最老用户原话 head] + [中间省略声明 injection "Some of this conversation's user messages were omitted here during compaction: the messages above this note are the oldest user input, the messages below are the most recent, and roughly N tokens in between were dropped"（:142-153，omittedTokens 由 selection 统计）] + [最近用户原话 tail] + [压缩摘要 origin:compaction_summary（:138）] + [继续提醒 injection "Context compaction is complete — continue the work that was in progress when it began."（:162-168）]`；head/tail 的具体 token 预算数值在 selection 逻辑（粘贴稿称 head≤2000 / head+tail≤20000，未逐字复核）。→ **与 ds4 的分野**：ds4 = durable 摘要 + verbatim tail（只保尾部原文）；kimi = **头尾都保用户原话 + 中间明示省略了多少 token + 摘要 + 继续提醒**。两者都留 verbatim，但 kimi 多两条 heartflow 现缺的关键件：(a) **省略声明**把「丢了多少」显式告知模型，防模型以为没丢而答非所问；(b) **继续提醒**防压缩后 agent 误停。这两条直接可抄进 `compact.rs`（连同 ds4 的「总结后停手」「硬触发」一起，构成完整改造规格——已落为 §4「设计规格：compaction 完整改造」）。
3. **undoable fold（`contextOps.ts:98-107` 的 `.undoable({ onUndo })`）**：会话态 undo = 计算 `computeUndoCut` + `resetFold` 回退到切点，是 fold 的逆操作而非删除文件。→ heartflow 会话无 undo；与 easy-llm-cli checkpointing `/restore` 互补——**undo 是「对话态回退」，`/restore` 是「文件态回退」**，二者合为完整回滚。
4. **blobs 脱水/再水化（`.replayable({ blobs: { dehydrate/rehydrate } })`）**：大 payload 脱水成 blob、按需 rehydrate，不常驻 context。→ 对应 ds4「可重载大体积数据只记 path/range」与 §5 上下文预算：工具结果/附件的大体量内容应脱水、注入前按需还原。
5. **单轮 loop 内「看见」已发生事件**：loop 中状态即全部已折叠事件的结果（`foldAppendMessage`/`foldLoopEvent`），dispatch 同步折叠 + `drainQueue`。→ heartflow 对应记忆既有约束「在飞回合必须能被事件循环够到」（`run_one_turn` 内联 await 是根因）；P2 并发模型应保证事件可见性，勿重蹈覆辙。

#### 借鉴点子（easy-llm-cli / Gemini-CLI 派生 深度研读，待落地 §3/§4/§5/§6）

本地副本 `.workbuddy/easy-llm-cli`（即 Gemini CLI 派生，架构见 `docs/architecture.md`：CLI 前端包 + Core 后端包 + `packages/core/src/tools/` 工具模块，与 heartflow `cli`→`provider/runtime/api`→`tools` 同构）。可借鉴点按落点分：

**§4 持久性 / 可观测性 / 安全**：

1. **Checkpointing（变更前 shadow-git 快照 + `/restore`，`docs/checkpointing.md`）**：批准任何改文件系统工具（write/edit/replace）前，自动在 `~/.gemini/history/<project_hash>` 的**独立 shadow git 仓库**提交项目完整状态（不碰用户自身 git），同时存对话历史与该次 tool call；`/restore` 一键把文件 + 对话 + 原 tool call 全回退，并重提该 tool call 让你重跑/改/弃（:1-19）。→ 这是 heartflow 缺的「安全实验回滚」原语，直接对应 §6 编辑后反馈的「改了能撤」；落点：在 `tools` 写类工具执行前挂 shadow-git 快照，`hf` 加 `/restore`。优先级高——比 §6 的事后诊断更底层。
2. **OTEL 可观测性（`docs/telemetry.md`）**：基于 OpenTelemetry 标准，配置优先级 `flag > env > workspace settings > user settings > default`（:13-34，与 heartflow 既有配置优先级同构）；所有 log/metric 带公共 `sessionId`（:132）；log 含 `config`/`user_prompt`/`tool_call`(function_name/args/duration_ms/success/decision/error)/`api_request`/`api_error`/`api_response`(含 input/output/thought/cache/tool token 细分，:138-192)；metric 含 `session.count`/`tool.call.count`/`tool.call.latency`/`api.request.count`/`api.request.latency`/`token.usage`(input/output/thought/cache/tool)/`file.operation.count`（:196-232）。→ heartflow 当前只有 stderr tracing，无结构化遥测；大项目持续运行（§4 目标）必须有 trace/metric 才能定位「哪一回合卡住/哪个工具慢/哪个 provider 烧钱」，建议引入 OTEL，先本地 `:4317` + Jaeger（`npm run telemetry -- --target=local` 模式）。
3. **扩展系统 + 命令级工具排除（`docs/extension.md`）**：`<workspace>/.gemini/extensions` 与 `<home>/.gemini/extensions` 两处加载，`gemini-extension.json` 声明 `mcpServers`/`contextFileName`/`excludeTools`，同名时 workspace 优先（:7-42）；`excludeTools` 支持**命令级**限制，如 `"excludeTools": ["run_shell_command(rm -rf)"]` 只禁 `rm -rf` 不ban 整个 shell（:40）。→ heartflow §5 工具面可借鉴：`excludeTools` 的「工具(子命令)」粒度比整工具开/关更细，是更安全的默认护栏；扩展的 workspace>user 合并语义与 heartflow 配置合并同源。
4. **沙箱 + Seatbelt 档位（`docs/sandbox.md`）**：macOS Seatbelt（`sandbox-exec`）四档 `permissive-open`(默认,限外写/放行网络)/`permissive-closed`(限外写/无网络)/`permissive-proxied`(限外写/走代理)/`restrictive-closed`(最严)；容器 Docker/Podman；Linux UID/GID 映射；启用优先级 `flag(-s)` > `env(GEMINI_SANDBOX)` > `settings.json`(,62-86)。→ heartflow §6/§4 安全：shell/写类工具可加 Seatbelt 默认 `permissive-open` 隔离，挡住误删系统文件；档位语义与「按目标分流」（§3）正交。

5. **行为控制方法论（ds4 `dir-steering/` + `ds4_engram.c`，推理引擎层，heartflow 作为客户端暂不可直接套用，仅记录方法论）**：`dir-steering` 用 paired target/control prompt 捕获每 transformer 层激活、取 `target−control` 平均归一化成 per-layer 方向向量，运行时做低秩激活编辑（不微调）来控制风格/话题/「简洁↔详尽」（README 验证：同 prompt 在 scale −1/0/2 下 67/136/140 词，:115-152）；`ds4_engram` 是 n-gram token 哈希的 KV 检索增强（header 含压缩词表/分块磁盘读/384KiB 有界临时内存）。→ heartflow 靠系统提示词控风格而非激活编辑，但「paired-prompt → 可提取可控方向」的方法论可启发**系统提示词的风格变体设计**（如抽取「简洁/严谨/详尽」方向做成可切换 prompt 变体）；engram 的「有界内存 + 分块磁盘读 + 不整表分配」是长上下文外存检索的工程范式，与 §4 磁盘检查点同源。

**§3 代理（已在 §3 末补沙箱出口代理正交说明）**、**§5 工具/上下文面（见下节）**、**§6 编辑后反馈（checkpointing 的 `/restore` 已在上文 §4 列）** 不重复。


### 6. 编辑后反馈（pi-lens 借鉴）

pi-lens（apmantza/mattgmak）：编辑后即时 LSP 诊断、impact cascade（连带文件诊断）、ast-grep/tree-sitter 结构规则、`symbol_search` 热索引 discovery funnel、诊断分诊（false-positive / suppress / defer / flag 四态）、**degradation ledger**（静默降级行为的有界账本）、read-guard + edit-autopatch。

heartflow 落点：`edit_file` / `apply_patch` 后回注诊断（Rust 项目先接 `cargo check` / rust-analyzer，与 §4 集成测试纪律同向）。degradation ledger 的思路与本仓「安全降级 vs 结果变错」判据同源。

**与 easy-llm-cli checkpointing 互补（§4 已列）**：§6 是「改后诊断」，checkpointing 的 `/restore` 是「改后能撤」——两者合起来才是完整的安全编辑闭环：先 shadow-git 快照（写前）→ 执行 → 回注诊断（写后）→ 异常则 `/restore` 一键回退。优先级：checkpointing 是 §6 的更底层原语，应先于事后诊断落地。

**checkpointing 落地状态（2026-10-21，MVP 已上线）**：`runtime::checkpoint`（shadow-git bare 仓库 `~/.heartflow/checkpoints/<FNV-1a(项目路径)>`，全程 `--git-dir`+`--work-tree` 显式寻址、强制 `core.autocrlf=false` 防行尾改写、身份/签名/hook 全覆盖、`info/exclude` 排除 `.heartflow/`、项目 `.gitignore` 照常生效）；`write_file`/`edit_file`/`apply_patch` 执行前自动快照（失败不阻断写入：warn + 计数 + `hf doctor` 可见——安全网不是闸门）；REPL `/checkpoints`（列 50 条）+ `/restore N|sha`（read-tree + checkout-index 恢复 tracked 文件）。**MVP 刻意不做**：① 对话回滚（只回滚文件）；② 会话内快照隐私治理。**快照后新建文件的清理已落地（2026-10-21 补全）**：`/restore N --purge` 显式 opt-in——先做兜底快照（purge 本身可逆），再按「git 可见集（tracked+untracked，尊重 .gitignore/info-exclude）− 目标快照树」的精确差集删除，永不跑 blanket `git clean`、永不碰 ignored 文件、不递归清目录。**顺带修复真 bug**：`snapshot()` 的 "nothing to commit" 检测只看 stderr，而 `git commit -q` 把该信息写 stdout——无变更快照一直静默走错误路径（被 `maybe_snapshot` 的 Err→None 吞掉伪装成正常）。

**写后诊断回注落地状态（2026-10-21，MVP 已上线，安全编辑闭环补全）**：`runtime::diagnostics`——写类工具改动 `.rs`/`Cargo.toml` 后，向上找 `Cargo.toml`（限 8 级）定位项目根，跑 `cargo check --message-format=short --quiet`（默认 60s 超时，`HEARTFLOW_DIAGNOSTICS_TIMEOUT_SECS` 覆盖；`HEARTFLOW_POST_EDIT_DIAGNOSTICS=0` 总开关），short 格式解析（`file:line:col: level[CODE]: msg`；无位置的 cargo 级错误落 `file="<cargo>"`），结果以 `postEditDiagnostics` 字段**回注进同一工具结果**——模型当轮就能看到编译裁决，省一轮 read→bash→check 往返。**degradation ledger（有界可观测）**：按原因分桶计数（no-manifest / no-cargo / timeout / spawn），`hf doctor` 分级展示（no-manifest 属非 Rust 项目的正常态只展示；timeout/spawn 计入 problems）。**刻意不做**：rust-analyzer 集成（进程寿命管理与 LSP 通信属另一量级）、非 Rust 语言的诊断（等真实需求）、诊断驱动的自动回滚（判断权留给模型与人）。


### 11. 硬骨头作战序列（架构级，按依赖排序执行）

> §1–§10 是方向研究（只记录不执行）。本节把其中**需要动大代码 / 改架构**的条目抽出，按依赖排序成**执行序列**，供迭代落地。铁律：所有 TUI 改动须真机冒烟；所有流式路径改动须先录 cassette（当前无已录夹具）。

#### 11.0 根因锁：P2 并发模型 / 事件循环重构（一切的前提）

- **根因（代码实证）**：`crates/cli/src/tui.rs:1889-1935` 的 live-turn 守卫 + `run_one_turn` 在回合边界内联 `await`，把事件循环停住；`tui.rs:451`/`:527` 注释自陈「the loop intercepts these keys before the reducer sees them」——按键拦截层在 reducer 之前。后果：**在飞回合不可被事件循环够到**。
- **要改的大代码**：把 turn driver 从「内联 await 停住循环」改为「可被事件循环 pump 的任务」；live-turn 守卫改为非阻塞轮询；TUI 事件循环拆为「输入采集 / turn 驱动 / 渲染」三相。涉及 `cli/src/tui.rs` 事件循环 + `cli/src/main.rs` `run_one_turn` + `runtime` World 所有权（单写者，§9 事件循环调度）。
- **解锁项（全部挂在它后面）**：P-D Steer、mission 回合自动记账（D1.3）、在飞回合取消（double-Ctrl+C，§4）、§7.2 优雅停机交接、codexmon 工具卡死超时（§8.1）、子代理看门狗（§8.4）、§10.2 每回合 ephemeral + session resume、§10.4 `agent_settled` 真终态（修 `run_one_turn` 终态判据）。
- **风险**：TUI 交互式，本环境无法真机冒烟，须用户陪测；改动前先录 cassette。
- **落地状态（2026-10-22，循环结构已重构，待真机陪测）**：
  - **三相事件循环 ✅**：`drive_loop`（`tui.rs`）拆为 Phase R（脏则渲染）/ Phase T（回合驱动：出队输入、同步 `begin_turn` + `Msg::TurnStarted`、take runtime+prompter 组装回合）/ Phase I（biased `select!`：回合完成臂优先 settle，共享 Msg 通道臂处理导航/reducer/权限/取消/提交）。回合在飞时循环照常迭代，§11.0 根因（内联 await 停住循环）已消除——回归测试 `keys_reach_the_reducer_while_a_turn_is_live` 锁定。
  - **所有权反转 ✅**：回合 future 改为 `'static` 盒装 future（`Pin<Box<dyn Future<Output = TurnResult<C,T>>>>`），**自持有**该回合的 runtime 与 prompter，结束时经 `TurnResult` 交还；循环持 `Vec<Option<ConversationRuntime>>` 槽位（None ⇔ 该 section 回合在飞）。回合中新建 section 不再与借用冲突，且 future 已具 spawn 条件（本次仍就地 poll，未 `tokio::spawn`——下一步可选）。
  - **`run_one_turn` 已溶解 ✅**：原函数整体并入三相，无遗留外壳。
  - **`TurnSaver` seam ✅**（`storage.rs`）：`DiskSaver` 生产实现 + 测试用 `RecordingSaver`，settle 持久化不再耦合真实磁盘路径。
  - **`guide_snapshot` 泛型化 ✅**：改走 trait 级 `executor().pending_tasks()`，不再下钻具体 executor 类型。
  - **在飞取消接线 ✅**：double-Ctrl+C 经 `CancellationToken` 取消在飞回合，settle 仍持久化（`double_interrupt_cancels_the_live_turn` 锁定）。
  - **验证**：`cargo test --workspace` 654 通过 / 0 失败（含 6 个循环级测试：回合完整跑通并持久化、回合中按键达 reducer、follow-up 合并进下一回合、double-Ctrl+C 取消、回合中禁导航、空闲新建 section 独立跑回合）；clippy `-D clippy::all` 干净；panic 棘轮 debt=0；fmt 干净。
  - **诚实边界（未做）**：① future 仍未真正 `tokio::spawn`（就地 poll，跨线程并发留给 P2 下一步）；② **真机冒烟未做**（本环境无 TTY）——须用户按下方陪测清单逐项验证后才算闭合；③ 流式路径本体（runtime）未动，未录 cassette 不阻塞本项，但后续 §11.5 流式改动仍须先录。
  - **真机陪测清单**：正常多轮对话与 follow-up 合并；回合中 PageUp/PageDown 滚动与按键响应；double-Ctrl+C 取消在飞回合（transcript 出现取消痕迹且会话已保存）；回合中 Ctrl+PgUp/PgDn 被拦（出现 "finish or cancel" 守卫提示）；Ctrl+N 新建 section 并独立跑回合；权限弹窗（Allow/Deny）在回合中正常往返；退出（idle 双击 Ctrl+C / 回合中取消后退出）无卡死、无 panic。

#### 11.1 TUI 升级（用户点名，真机冒烟陪测）

- **P-C TUI 三件套（§1 P-C）**：回合内输入路由补 `DeliveryMode` + 计数显示；队列编辑键位（withdraw/edit/reorder/改派）；引导 overlay（T6 两栏可编辑确认）。
- **KI-001 Alt+Enter 全屏**（known-issues.md）：交互式 PTY 内 Alt+Enter 被 TUI 按键拦截层吃掉，未透传 → 交互态激活时透传 PTY。
- **KI-002 Ctrl+C 提示过短**（known-issues.md）：打断/确认提示走瞬时 `status_note` 被下一帧覆盖 → 改为持久状态栏保留到结算或显式超时。
- **已知未修（§9）**：TUI 流式尾部每帧 clone、表格列宽不按终端封顶 → 端到端冒烟 + cassette 后改。
- **铁律**：TUI 斜杠命令派发（`println!` 与 alt-screen 不兼容）刻意不盲改，只有真机冒烟能验。
- **依赖**：KI-001/002 是「拦截层局部透传」小改，可**先于 11.0** 落地；P-C 三件套建议 11.0 后做最稳。
- **落地状态（2026-10-23，KI-001/002 已修；2026-10-24，P-C 三件套已落地；均待真机冒烟）**：
  - **KI-001 根因更正（代码实证）**：「TUI 拦截层吃掉 Alt+Enter」不成立——无绑定认领该键、textarea 对任意修饰键 Enter 都插换行；「全屏」来自 Windows Terminal 默认键绑定 `alt+enter → toggleFullscreen` 在 ConPTY 之前消费按键。且「透传 PTY」无机制可挂（全仓无 PTY，`!CMD`/bash 子进程 stdin 均 null）。真缺陷 = TUI 与 REPL 输入面不一致：TUI 里 Shift+Enter 因 keymap 匹配掩盖 SHIFT 而**提交**（REPL 是换行），宿主吃掉 Alt+Enter 后用户无可用多行键。修复 = `on_key` 在 keymap 解析前拦截 Shift/Alt+Enter 插换行 + 输入栏标题提示 `shift+enter newline`（与 REPL「裸 Enter 发送、Shift/Alt+Enter 换行」对齐）。
  - **KI-002 修复**：`App::rendered_status_note()` 把武装（`ctrl_c_armed`）与确认打断（新增 `cancel_requested`，直到回合结算）做成**状态派生的粘性覆盖**，流事件（每个 `ToolUse` 重写瞬态 note）盖不掉；提示寿命与状态严格同寿命（他键解除武装/回合结算即消失），免超时器。循环级测试 `the_armed_cancel_hint_survives_streaming_tool_events` + 3 个 App 级测试锁定。
  - **P-C 三件套（2026-10-24）**：
    - **队列编辑面板**：`Action::QueuePanel`（默认 **Ctrl+Q**，可重映射；空队开面板给提示不开死面板）模态浮层，行为读**活队列**（settle 递送后面板不显示幽灵行）：↑/↓ 选择、`e` 取出回输入框回炉（D2.4）、`x` 撤条入台账、`+/-` 前后移、`l` 在 main+既有 lane 间轮转改派、Esc 关闭；回合中可开（队列变更只在 settle 边界生效，不打扰在飞回合）；权限弹窗弹出时压下面板。REPL 侧补 `/queue relane <id> <lane>`（D2.4 改派此前两面皆缺，core 补 `reassign_lane`——lane 是观测/编辑维度，不动到达序）。
    - **引导 overlay 升 T6 两栏可编辑**：草稿装进 overlay 内的 `TextArea`（左栏任务+说明、右栏可编辑草稿、粘贴进草稿），Enter 发送**编辑后**的正文（所见即所发不变，只是所见可改）；Esc 取消任务留输入框不变。
    - **输入路由与计数**：mode 路由已随 §11.2 落地（Enter=FollowUp、Ctrl+S=Steer）；状态栏计数分流 `[N queued · M steering]`。
    - **测试**：core `reassign_lane`（no-op 边界 + 不动序）、App 级 3（面板编辑/撤条/重排/改派全走查、末条撤空自动关、guide 编辑后发送）、循环级 `a_withdrawn_queue_entry_never_runs`（回合中排入→面板撤条→结算后零递送，请求数=1）。
  - **验证**：763 通过 / 0 失败、clippy `-D clippy::all` 干净、fmt 干净、panic 棘轮 debt=0（uv 跑）。**真机冒烟待陪测**（新增 3 张：① 回合中 Ctrl+Q 开面板、e/x/+/-/l 各操作、Esc 关闭；② Ctrl+G 草稿可编辑、Enter 发送编辑后正文；③ 状态栏 queued/steering 分流计数）。

#### 11.2 P-D Steer（注入在飞回合）

- 前置 11.0。递送语义 `Steer` = 注入当前回合（§1 D2.x）；默认 `FollowUp`，`Steer` 经 11.0 可用。逐条 steer 递送 + 失败按条目重排 + 明示（§1 P-D/P-E）。
- **落地状态（2026-10-23，全链落地，待真机冒烟）**：
  - **runtime 半边**：`SteerMailbox`（`Arc<Mutex<VecDeque<String>>>` 共享信箱，锁污染恢复）+ `ConversationRuntime::set_steer_mailbox`（按回合武装，回合前设置、settle 时复位）；回合循环在**迭代边界**（与取消检查同点——pi「runner 在安全点消费，不抢断 LLM 调用」语义）drain 信箱，把每条 steer 作为**用户消息**追加进 session，发 `AgentEvent::Steered(text)`；无工具调用的末轮自然结束 → 未消费的 steer 留给宿主回收。测试：`steered_text_joins_the_next_request_as_a_user_message`（工具处理器内 push，断言第二请求携带、第一请求不携带、恰好一个事件）。
  - **core 半边**：`DeliveryMode { Steer, FollowUp(默认，D2.3), Auto(保留) }` 进 `QueueEntry`（`#[serde(default)]`，旧 queue.json 无该字段可加载，测试钉死）；`send_with_mode` + `claim_steered`（pi FIFO 文本认领：等文本的最旧 steer 条目认领 `Steered` 事件，Delivered 入台账，幂等）+ `recover_pending_steers`（未消费→FollowUp，条目不丢）+ `drain_injection` 排除 Steer 条目（steers 永不混进下一回合合并注入）。
  - **TUI 半边**：`Action::SteerSubmit`（默认 **Ctrl+S**，可经 keymap.toml 重映射；空闲时回落为普通 submit，永不是死键）；`submit_steer` 回合在飞时入队（mode=Steer 供 `/queue` 观测）+ 暂存给循环推入信箱 + transcript 回显 `(steering)`；`Steered` 事件到达时认领条目 + `· steered:` 行明示；settle 时先 `recover_pending_steers`（transcript `· N late steer(s) queued as follow-up(s)` 明示）再合并注入——**「child completed before consuming steering」诚实命名**。循环级测试：`a_steer_joins_the_running_turns_next_request`（Ctrl+S 后第二请求携带文本、第一请求不携带）+ `a_late_steer_is_recovered_as_the_next_turn`（无工具回合结算后 steer 转下一回合，saves=2）。
  - **诚实边界**：REPL 侧无 in-flight 输入面，steer 仅 TUI 可达（turn.rs 的 `Steered` 臂保持穷尽）；`Auto` 模式保留未实现（steer 保持显式，默认语义不变）；TUI 注入仍是「合并注入」形态（逐条递送的 TUI 化属 P-C 队列编辑键位）。
  - **验证**：755 通过 / 0 失败（+8 新测试）、clippy `-D clippy::all` 干净、fmt 干净、panic 棘轮 debt=0（uv 跑）。**真机冒烟卡**：回合运行中 Ctrl+S 发文本 → 状态栏 `steering` 提示、工具步后模型看到并回应；无工具短回合中 Ctrl+S → 回合结束出现 `late steer(s) queued` 且下一回合执行该文本；`/queue` 列表出现 `·steer` 标注条目。

#### 11.3 mission 回合自动记账 + 在飞回合取消

- **D1.3 runtime 续跑驱动器**：每 turn 开头注入 mission reminder（status + criteria + 预算剩余）；截止调度（到期限提醒，不杀）。前置 11.0 事件循环够到。
- **在飞回合取消（§4）**：double-Ctrl+C 取消在飞回合。前置 11.0。
- **落地状态（2026-10-23，D1.3 全链落地，待真机冒烟；在飞取消已随 §11.0 落地）**：
  - **reminder 注入**：`runtime::mission::turn_reminder`（纯函数）把盘上账本投影为单条系统提示段——Active 带 `goal_continue`（kimi「runtime 驱动续跑而非模型自觉」）+ 判据进度 + 三轴预算 + ≥90% attention；Blocked 带「本回合决定：解阻/改派/暂停」；Paused 一行不带续跑催促（暂停是主动选择，账本应尊重）；budget-exhausted 跳过（D1.2 停提醒不关）、Complete 跳过。经 `ConversationRuntime::set_turn_context` **追加进每请求的 system_prompt 末尾**——刻意不落盘 session（投影非事实源），回合边界由宿主重建；段内容回合内稳定，prompt cache 只在账本真变时重付。REPL（`run_turn_interactive`，doctor/plan 同路）与 TUI（Phase T 回合组装）两面接线。
  - **回合自动记账**：`runtime::mission::auto_record_turn(root, tokens)`——**仅当恰好一个 Active mission** 才把该回合的 tokens(input+output) 记入并触发 needs-attention（多活跃归属是猜测，不写）；**只记成功回合**（失败回合不是有效工作，不该清 blocked streak / 推进 turn 预算）；REPL 打印 attention、TUI 落 transcript 行。手动 `/mission record` 保留。
  - **测试**：`turn_reminder_renders_each_open_state_and_skips_closed_work`（四态渲染 + 两类跳过 + 空账 None）、`auto_record_turn_accounts_the_single_active_mission_only`（记账/D1.2 停提醒/多活跃零写入）、`turn_context_rides_the_system_prompt_and_never_the_session`（段追加、清除即消失、session 无痕）。
  - **诚实边界**：「截止调度」未做独立调度器——墙钟到期即预算耗尽路径，由 reminder 内嵌的 budget_attention 覆盖「到期提醒不杀」（D1.2 预算不杀本就成立），独立 deadline scheduler（日历时刻触发）无真实需求前不加。多活跃 mission 的回合归属仍是空白（等「当前 mission」显式绑定设计）。
  - **验证**：758 通过 / 0 失败、clippy 干净、fmt 干净、panic 棘轮 debt=0。真机冒烟卡：`/mission create` 后任一回合请求应含 reminder（可用 `/mission` 状态对照 tokens/turns 增长）；预算 ≥90% 回合末出现 attention 行。

#### 11.4 per-section 泳道递送轮转（TUI 半边）

- 数据结构（`QueueEntry.lane`）+ REPL 观测面已落地（§1 2026-10-22 批次）；TUI 侧轮转规则（`MAX_SECTIONS=9`，`tui.rs:1103`）未做。前置 11.1 TUI 升级的队列编辑键位。
- **落地状态（2026-10-24，全链落地，待真机冒烟）**：
  - **轮转规则裁决（按 D2.1 既定倾向）**：**section 就是递送泳道**——轮转发生在 section 之间，section 内部保持到达序（lane 维持「观测与编辑维度」纪律，2026-10-22 批次的约束原样保留）。循环空闲且无 fresh 提交待跑时，`Shell::next_queue_section` 从 `delivery_cursor` 起环形扫描，取下一个有待发条目的 section，**一次取一条**跑一个完整回合（`/queue merge` 的 section 仍按 opt-in 整队合并为一条）；游标在被服务的 section 之后推进——公平轮转而非前排独占。这同时补齐了 D2.2 的 TUI 半边：TUI 递送从「整队合并注入」改为**默认逐条**（与 REPL 逐条路径同语义，裸文本、无合并头）。
  - **前置解锁：回合事件按 section 路由**——`Msg::Event/TurnStarted/TurnFinished` 携带 section 标签，后台 section 的回合流进**自己的** transcript/状态栏，绝不污染活动 section；`Msg::PermissionRequest` 仍属活动面。由此**解除 §11.0 时代的「回合中禁止切换 section」封锁**（该封锁是为旧内联循环设的防线）：回合运行中可自由 Ctrl+PgDn/PgUp/Ctrl+T 切换与开新 section——这正是「当前工作时切换另一个 section」的原始诉求；后台回合运行时活动 section 的 Ctrl+C/Esc **转交运行中 section** 处理（双击取消语义全局一致，Esc 不再误退整个 shell）。
  - **自动递送的护栏**：轮转是自动的，失败的条目**自动重排一次**；**同一 id 连续第二次失败终态化为 `Failed`**（台账可见、明示）——自动队列绝不无限重试毒条目。迟到 steer 的回收（§11.2）改为留在本 section 队列里等轮转，不再整队合并注入。
  - **测试**：`follow_ups_submitted_during_a_turn_run_one_per_turn`（逐条递送，每条一个请求）、`section_navigation_works_while_a_turn_is_live`（封锁解除，回合中切换、回合留在原 section）、`queued_work_rotates_into_a_background_section`（标题场景：排队工作在后台 section 自动跑完、操作者已切走、台账 delivered）、`a_poisoned_queue_entry_fails_after_one_retry`（恰好两次尝试后 Failed，请求数=3）。循环测试的 `LoopReport` 新增全 section 台账快照。
  - **验证**：765 通过 / 0 失败、clippy `-D clippy::all` 干净、fmt 干净、panic 棘轮 debt=0（uv）。顺手修掉 `jobs.rs` 槽位测试的固定 120ms sleep 时序断言（满载并行下饿死，改有界轮询）。**真机冒烟卡**：① 回合中 Ctrl+T/Ctrl+PgDn 切换 section，原 section 的回合继续、tab 带 `*`、流式只进原 section；② section A 排两条 follow-up → 切到 section B → A 的条目在后台逐条自动跑完、状态栏计数递减；③ 制造一条必败 follow-up → 观察一次自动重试后 `Failed` 入台账、不再无限请求。

#### 11.5 次级 / 独立（不阻塞主线）

- **§4 cassette 录制扩展**为 ds4 式集成测试：先录夹具（铁律），再动流式路径。
  - **落地状态（2026-10-24，夹具已录 + 离线回放套件上线）**：机制本就全备（`CassetteClient` Record/Replay、请求逐节比对、`HEARTFLOW_CASSETTE` 已接进 hf 装配），缺的是**真实夹具**——现已录两个 DeepSeek 实链路夹具入库（`crates/provider/tests/fixtures/`）：
    - `text-turn.json`：纯文本回合（含 25 条真实 reasoning delta——推理流首次有了离线回归锚点）；录制命令：`HEARTFLOW_CASSETTE=record:crates/provider/tests/fixtures/text-turn.json hf prompt -q "..."`。
    - `tool-turn.json`：真实工具往返（模型真实发起 `bash` 调用、真实执行、第二请求携带真实结果）。
    - **离线回放集成套件**（`crates/provider/tests/fixtures.rs`，4 测试）：从夹具读回**录制的系统提示**（非重建——含日期/git 状态的活提示天然不可复现）+ 夹具供给工具结果的 `FixtureExecutor`（跨平台确定性：录制的 PowerShell 输出不必在 Linux CI 重跑）→ 复穿真 `ConversationRuntime`；任何提示组装/回放投影/工具广告/消息序漂移都会在请求比对处点名失败——**从此流式路径改动有了可跑进 CI 的回归网**（ds4 四轴：模型/协议加载、提示渲染、工具往返、会话配对不变量）。
    - **脱敏护栏**：夹具扫描测试（`sk-` 形值/Bearer 即败）+ 提交前人工核验（环境变量名会出现在系统提示文档段，属正常；值绝不入库）。诚实边界：保真层级是 **AgentEvent**（非 wire 字节，§7.2.4 既定裁决）；带错误工具结果的回合暂不可逐字回放（执行器错误由循环自行格式化）；夹具含日期/git 状态，**改提示组装后须重录**（重录命令即一行 env + `hf prompt`）。
    - provider 公开面新增 `read_entries`/`RecordedEntry`（夹具读取 API）。
- **§10.2 ephemeral + session resume** / **§10.4 `agent_settled` 真终态**：与 11.0 同源，可在 11.0 一并收口。**收口核对（2026-10-24）**：§11.0 后「回合终态」= turn future 解出（重试在 api 层内、压缩在循环内、steer 吸收后循环继续下一迭代——`agent_settled` 语义在 `run_turn` 结构内成立）；§10.2 的「勿让长生命周期进程卡住事件循环」已由三相循环 + §11.4 后台 section 轮转覆盖；heartflow 是 HTTP 客户端无 per-turn 进程可 ephemeral——两条按「架构上已吸收」结案，无需独立改动。
- **跨进程单写者 World（P2 跨进程）**：进程内 Semaphore 先行，跨进程槽位留给 bash 长任务外置（§1）再搬。~~已知缺口：job id 是进程本地序号，两进程共用 `JOBS_ROOT` 会互踩~~（2026-10-24 已修：pid 命名空间 + 探活对账，见 §4 遗留记录更新）。
- **全树 `cargo update`**：无关 major 的传递刷新，独立小步验证（brotli 8→9 等）。
- **§7.1 deepseek-recipe 引入评估**：外部依赖大决策，须先验证 MSRV/构建体积，不在硬骨头首批。

#### 执行顺序建议（明日）

1. **11.0 根因锁**（最大架构块，先做解锁一切）—— 本环境无法真机冒烟，用户陪测。
2. **11.1 TUI 升级 + KI-001/002**（用户点名陪测）—— KI-001/002 小改可先于 11.0。
3. **11.2 / 11.3**（Steer + mission 记账 + 取消，前置 11.0）。
4. **11.4**（per-section 轮转，前置 11.1）。


### 12. Provider 身份与凭据链完备化（2026-10-24 批次，设计规格，未落地）

**实测事故（本机，2026-10-24）**：`/bootstrap deepseek` 返回 401「令牌已过期或验证不正确」。链路还原：用户级 `config.toml` 的 `[provider]` 指向 `https://open.bigmodel.cn/api/coding/paas/v4`（model `glm-5.3`，inline api_key 在），同时 `DEEPSEEK_API_KEY` 在 User/Process 两层非空。`resolve_spec` 是**逐字段合并**：base_url 取 config 层、protocol 取 builtin、`api_key_env` 取 builtin 的 `DEEPSEEK_API_KEY`（`config.rs:544-562`）；`materialize` 里 env 优先于 inline（`config.rs:622-627`）。于是「智谱端点 + DeepSeek 环境键」被交叉成对，401 由 bigmodel 的方言返回。**key 没过期，是三件身份各自漂移**。

**现状（代码实证）**：
- 逐字段合并，builtin 只提供四个默认值（protocol/base_url/api_key_env/model，`config.rs:108-124` 与 `512-584`）；config 任意一层可静默覆盖任一项。
- 凭据二来源 env > inline（`config.rs:597-654`），但**来源信息在合并后丢失**——`ProviderProfile` 不带来源字段，调用方无法说「这把 key 来自哪个变量」。
- 预检只报模型数量（`main.rs:2608-2613`）；401 的 hint 却指向 model id 与 web search（`main.rs:2678-2684`），对 auth error 是错向。
- 分类能力已存在：`ApiError::kind()` 把 401/403 归为 `auth error`（`error.rs:57-62`）。
- 身份展示面已存在：`/model` 已打印 provider/protocol/base_url（`main.rs:2916-2937`），不必新造命令。

**设计判据（三条需裁决）**：
1. **名字即身份**：显式点名 builtin 时锁定 protocol/base_url/api_key_env 三项；config 覆盖只允许显式旗标（`--base-url`/`--api-key-env`）或自定义 `[provider] name`。逐字段静默覆盖正是混线根因。这是 breaking change，代理/网关场景必须留出口，故走「配置门控 + 版本化默认」，不硬切。
2. **凭据来源是身份的一部分**：`CredentialSource` 进 Profile，所有展示面（启动一行、`/model`、bootstrap 预检、doctor、错误分类）都打印「来源 + 变量名 + 掩码」，绝不打印值。
3. **诊断按失败轴分流**：auth error（401/403）→ 端点 host + 凭据来源 + `--token/--token-stdin` 出路；connect/timeout → base_url 可达性；404/400 → 协议或模型；网络类不再提 model id。

**数据结构（设计，未实现）**：

```rust
struct ProviderIdentity {
    name: String,
    protocol: ProviderProtocol,
    base_url: String,
    model: String,
    credential: CredentialSource,
}

enum CredentialSource {
    Env { var: String },
    Inline { file: PathBuf },
    Prompt { scope: SessionId }, // 见 §13
    Missing,                     // env 空且 inline 空，独立类型而非空串
}
```

**推进相**：P-A 身份可见（Profile 增 source 字段，启动/预检/`/model`/doctor 打印，纯加法零语义变更）→ P-B 诊断分流（按 `kind()` 重写 401 分支，补 `--base-url`/`--api-key-env`/`--token-stdin`）→ P-C 语义收敛（builtin 锁全量，`HEARTFLOW_PROVIDER_STRICT=1` 先行，下一主版本默认；旧配置由 doctor 报 override 行）→ P-D 回归矩阵（builtin × config × env × inline 的组合表测试 + 401 文案快照）。

**P-A 落地记录（2026-11-27）**：`ProviderProfile` 增 `credential_source: CredentialSource`（`Env{var}` | `Inline` | `Missing`，`materialize` 按二来源守卫填充——env 赢则 `Env{var}`、否则 `Inline`；`Missing` 仅测试/手工构造可达），`/model` 状态行新增 `key from:` 行（`describe()` 只报来源与变量名，永不打印值）。三个 materialize 语义测试钉住（env 赢/inline 兜底/inline-only 空名不泄入 Env 变体）。启动一行与 doctor 的接入留待 P-B 一并做（同一诊断文案面）。

**回退**：P-A/P-B 纯加法；P-C 挂 env 开关，关掉即回逐字段合并；`config.toml` 结构不变。

**明确不做**：不扫 keychain / 浏览器 / 其他工具 dotfiles（沿 `config.rs:598-604` 纪律）；不自动改写 `config.toml`；不做跨供应商自动择优；错误里不回显 key 的任何片段。

### 13. 提示词内参数自举（in-prompt provisioning，2026-10-24 批次，设计规格，未落地）

**需求**：一条消息内带 provider/base_url/api_key/model 即完成自举，不改 `config.toml`、不重启。既有能力边界：`/bootstrap ... --token <key>`（`main.rs:2569-2579`、`2600-2608`）与各 provider 的 env 变量。

**威胁模型（代码实证，四条泄漏路径）**：
1. **会话落盘**：保存时脱敏有两层——结构形规则（URL userinfo / Bearer / 标注 `api_key` / `sk-`、`ghp_`、`AKIA` 前缀，`redact.rs:33-96`）与启动时已知凭据的精确字面量（`main.rs:4378-4392` → `storage.rs:199-222`）。**新粘贴、未登记且不匹配前缀的 key（如智谱 `32hex.16+` 形）不被覆盖**。
2. **cassette**：`RecordedEntry` 保存 system_prompt + messages（`cassette.rs:126-143`），回放逐字比对（`:410-438`）；夹具扫描只查 `sk-` 形（§11.5）——智谱形会入库。
3. **系统提示词通道**：key 一旦进 system prompt，会随每次请求发送并被录制；§11.3 的 mission reminder 正是**追加到 system_prompt 末尾**的通道，任何 key 进该通道等于永久随行。
4. **日志/遥测/argv**：`--token` 进 shell 历史与进程参数；stderr tracing 无脱敏层。

**设计裁决**：提示词**携带**，客户端**截获**，永不上线。新增输入边界解析：`/connect <provider> [model] --base-url <url> --key <secret>`（或同义结构化围栏）在 REPL/TUI **提交前**被本地消费；命中后该行不进 messages，transcript 只留 `[connect: provider=X base=Y key=#N masked]`；key 进内存 `SecretStore`（session 作用域，进程退出即失），并在消费点立刻 `register_secret` 参与落盘脱敏。
- 语义：prompt 是命令解析面，不是上下文面。模型看不到明文；cassette / compact / 日志零暴露。
- 与 §12 合流：`CredentialSource::Prompt{scope}` 参与身份展示，bootstrap 预检复用同一套 diagnostic。
- 残余风险：用户把裸 key 粘进普通聊天行时只能靠形扫描兜底——新增智谱形（`[0-9a-f]{32}\.[A-Za-z0-9]{16,}`）与「≥32 字符、字母数字混合、无常见词」的高熵保守规则；误报以 doctor 计数呈现；漏报仍可能，故 `/connect` 路径强制「只用结构化入口」。

**推进相**：P-A 离线解析器 + 掩码投影（纯函数，可测）→ P-B 接 REPL/TUI 输入边界 + 消费点立即 `register_secret` → P-C `--token-stdin`（不经 argv）与 §12 身份展示合流 → P-D 真机冒烟（粘贴 key 后 transcript / 日志 / 会话文件 / cassette 四处零明文）。

**回退**：整条挂 `HEARTFLOW_PROMPT_PROVISION=1`，默认关；`/bootstrap --token` 旧语义不变。

**明确不做**：不把 key 写任何磁盘文件；不持久化进 mission/session；不让模型看见原文；不做 keychain；不做跨会话复用。

### 14. TUI 视觉体系（nvim 借鉴，2026-10-24 批次，设计规格，未落地）

**现状（代码实证，回答「色块都一样」）**：Theme 只有 10 个前景语义色（`theme.rs:90-101`），无背景/表面/边框/选中 token；所有 `Block` 边框走默认前景（`tui.rs:1411`/`1808`/`1968`/`2039`/`2119`/`2190`），仅权限浮层给了 accent（`:1967-1970`）；顶栏一行 spans（`:1644-1682`）、状态栏一行 spans（`:1823-1900`），都没有分区底色；banner 是三分色平面调色（`mascot.rs:272-283`）。即系统里只有「文字该是什么色」，没有「窗口/面板/状态分区是什么层」——换色相解决不了层次问题。

**nvim 三件可借鉴（机制而非配色）**：
1. highlight group 分层：Normal / StatusLine(NC) / TabLine(Sel) / Pmenu(Sel) / WinSeparator / FloatBorder——主题是「角色 → 样式」的映射表，不是散落常量。
2. 模式即颜色：状态栏底色随 mode 变，不看文字也知道处于什么态。
3. 主次与边框语法：活动/非活动分区明暗差 + 边框字符集（rounded/single/double）区分容器、浮层、分隔。

**设计**：
- Theme v2 增 surfaces：`bar_bg` / `panel_bg` / `popup_bg` / `border` / `border_focus` / `selection_bg` / `tab_active_bg` / `tab_inactive_bg`；保持「一角色一色 + 亮度阶梯」纪律，surface 由现有 Kurosawa 调色板同 hue 暗阶派生，不引第二套色相。
- ModePalette：Normal / Insert / Plan / Approval 四态映射状态栏底色与提示字形（复用现有 `●/○` 语义）。
- BorderGlyph：`rounded`（容器）/ `plain`（分隔）/ `heavy`（焦点浮层），与 `border_focus` 成对使用。
- 降级链：truecolor → 256 → 16，按 `COLORTERM` 探测；无 truecolor 时禁用 bg 色块，退回「前景 + 边框字形 + 主次明暗」三件（nvim termguicolors 同款判据）。
- 兼容：`theme.toml` 新字段全可选，缺省从现有 10 色派生并随 `hf config export theme` 输出，旧文件零改动可载。

**测试（无 TTY 可验）**：样式投影纯函数返回 `Style` 矩阵做快照断言（角色 × 状态），并断言前景/背景相对亮度差 ≥ 阈值；真机只验观感，不验逻辑。

**推进相**：P-A surfaces/border token + 派生默认 + 快照测试 → P-B ModePalette 接状态栏与权限弹层 → P-C 边框字形三档 + tab 主次 + 浮层底色 → P-D 真机陪测（四态 × 三档边框截图对照）。

**回退**：新字段默认派生自旧 10 色，等价旧观感；单项 token 可单独回退。

**明确不做**：不做 GUI/图片渲染；不做主题市场；不引第三方配色库；不改 markdown 语义色的既有映射（REPL 与 TUI 同源纪律）。

**三节执行顺序**：§12 P-A 与 §13 P-A 可并行（都是纯加法 + 纯函数）；§14 P-A 独立；P-C 语义收敛与 P-D 真机均须用户裁决/陪测。

#### C-1. 模式系统四态 + 采样参数面（2026-10-24 用户点名设计，采样管道实现中，模式分相落地）

用户要求的四模式（Tab 切换）、采样参数、强度显示、联网开关，分层设计如下。

**采样参数**（实现中，子代理执行）：config `[provider]` 新键 temperature/top_p/top_k/frequency_penalty/presence_penalty/seed → `SamplingParams` 共享体 serde(flatten) 进 MessageRequest/ChatRequest → 三协议 adapter 贯通（responses 仅 temperature/top_p）。全 None 时请求 JSON 逐字节不变（cassette 夹具仲裁）。**beam search 不适用判决**：chat API 解码在服务端，num_beams 是本地推理概念——如实记录不实现。**补遗（2026-10-24 用户核对发现的遗漏，并入采样战役）**：① **Tool Choice**（auto/required/none）——API 基础设施已在（ChatRequest.tool_choice + ChatToolChoice，当前硬编码 Auto），缺 config 键 `[provider] tool_choice` 贯通；② **Response Format**（json_object/json_schema 强制输出）——ChatRequest 尚无该字段，需加 `response_format` 贯通；两者与 SamplingParams 同战役补齐。③ **强度级别集更正**：/effort 取值按用户点名扩为 **none（关）/ low / medium / high / max（深推理）**——模型相关映射（DeepSeek/GLM 各自支持子集，不支持的态度诚实降级），非早前写的固定三档。**第二个 TUI 判决：不需要**——参数属配置面（config + `hf config export`），TUI 加参数浮层是界面税；强度用状态行显示（见下）。

**四态模式**（同构复用：全部建立在现有 permission-mode + 系统提示段 + 工具过滤三件套上，不造新机制）：
- **agent** = 现默认（workspace-write/full）——已存在。
- **plan** = 现有 plan 权限档（写门禁到 `.heartflow/plans/*.md`——正是「不能动已有文件但可写想法」）；「省 token 强约束」补一段 plan 系统提示（禁读大文件、答案直给）。
- **spec（自研新范式）**：网络搜索先行（web 工具保留）→ 产出 spec 文档（写门禁到 `.heartflow/specs/*.md`）+ task/todo 列表（todo_write）→ **不实现**。落点 = plan 档的姊妹权限档 + 「先搜后写」系统提示段 + spec 模板。与 §1 的 `/guide` 三段草稿同族。
- **loop（hermes/harness 闭环，按研究定案）**：网络研究定案 = Anthropic generator-evaluator 循环 + Hermes 闭环（持久记忆→执行→自纠→避坑）。**本仓积木已全**：mission 账本（四态+判据+预算）+ 自动记账（§11.3）+ 续跑 nudge + full 权限 + MEMORY.md 避坑注入 + 停机守卫（double-Ctrl+C 仍可达）。loop 模式 = full 权限 + mission 自动续跑驱动（判据未满足即自动下一回合）+ blocked 三连检测停机。**分相**：等当前批真机测试后再建（它是权限最高的模式，需要测试过的基础）。
- **Tab 切换判决**：Tab 已绑 toggle_fold——模式轮换用新 keymap 动作 `cycle_mode`（默认 **BackTab/Shift+Tab**，可重映射），折叠保持 Tab。轮换序 agent→plan→spec→loop。**✅ 已落地（2026-10-24）**：keymap 动作 `cycle_mode`（BackTab）+ `toggle_web`（Ctrl+O）入默认键位与可重映射面；drive_loop 消费（mode 走 `set_section_mode_policy` 泛型策略热切换 idle-only，回合中按键诚实提示完成后再切；web 走共享 AtomicU8 即时生效）；App 暂存→loop 消费的两段式与 take_submit 同构。**✅ 已落地（2026-10-24 C-1 末相）**：`/spec <goal|approve [path]|end|status|list>` 镜像 `/plan`——预留 `.heartflow/specs/` 路径、研究先行 brief、`SpecPrompter` 硬门禁、三选确认 overlay（立即执行→复用 plan 任务循环 `run_task_loop` + 反射沉淀；取消→留盘后可 `/spec approve <path>` 再执行；添加修改→`inquire::Text` 回收修订回合再审）；loop 自动续跑驱动 `crates/cli/src/autopilot.rs`——单一 active mission 判据未满足即自动下一回合（`complete` 前置门禁：机械判据复验/WeakOnly 停机），失败回合计 blocked claim（`mark_blocked` 三连翻 Blocked 停机），Ctrl+C 中断停机，硬上限 64 回合防失控。`TurnOutcome` 增 `interrupted` 区分 Ctrl+C 与真错误。
- **思考强度**：reasoning_effort 已在 profile 贯通到请求；补 `/effort [low|medium|high]` REPL 命令（镜像 /model 重建）+ TUI 状态行显示（chrome 扩展一字段）+ keymap `cycle_effort`（默认 Ctrl+E）。
- **联网开关（开/关/本地，2026-10-24 用户细化）**：工具面过滤——开=web_search/web_fetch 正常；关=从工具集剔除；本地=web 剔除但保留 doc_search（rga 本地文档/知识库检索，clone 仓库研读属本地读）。**plan 可搜索**（用户更正：plan 也保留 web）。shape = build_runtime 时的工具注册过滤 + 模式默认值（spec 开、loop 开、plan 开、agent 开；用户可 `/web on|off|local` 覆盖）。**界面显示**：联网态与思维强度一起进状态行（如 `web:local · effort:high`），并配 keymap 动作（联网建议 Ctrl+O、强度轮换 Ctrl+E，均可重映射；Tab 已被补全/折叠占用不参与）。
- **spec 执行确认流（用户细化）**：spec 文档写好后**展示在界面上**（overlay 预览全文），并询问三选：**立即执行 / 取消 / 添加修改（type something）**——用户可改文档或附加内容，agent 结合情况合理修订 spec；已完成的 spec **不必立即执行**，可留在 `.heartflow/specs/` 由用户后续在 spec 模式选择执行。**模式纪律**：其他模式（agent/plan/loop）下要求做 spec 工作时，agent 须提示「这是 spec 模式的工作」并拒绝越界执行（模式职责不串）。
- **粘贴截断缺陷**：见 known-issues.md KI-003（bracketed paste 已启用仍截断——终端未回传粘贴标记假设，待实机复现）。

#### C-2. 本地小模型赛道 + 工具输出清洗（2026-10-24 用户构想批次，取舍归位）

用户构想六束，逐束取舍（网络检索定案）：

- **ONNX/小模型赛道（新开，评估项）**：用户构想——hf 可调用**大模型 + 非 LLM 的 ONNX 小模型**（翻译/OCR/NLP/图像语义），场景样例：一个英文词 + 几个候选翻译，小模型做语义优选、JSON 规范输出、直接替换执行，大模型只做监督。**取舍**：① **Ollama 接口零新代码**——其 OpenAI 兼容端点 + 本仓 openai-compatible provider + 回环防代理保护天然齐备，`/bootstrap ollama http://localhost:11434/v1` 即用（文档化即可）；② **ONNX 直连（ort crate）是评估项**：绑定重、模型分发（OCR=检测+识别双模型、embedding、NER 各一套）与「翻译优选」场景的收益需先以单实验验证（用 ollama 跑 embedding/翻译小模型替代 ort，零绑定成本先试场景价值）；③ **大小模型协作模式**（小模型快速分类/评分出 JSON、大模型监督裁决）记为模式设计资产，与 spec/loop 同族——小模型做「快而糙」，大模型做「慢而对」；④ **ds4 引擎本体不采维持**（Metal/CUDA 重栈判决），但其「引擎内部压缩与洗涤」的方法论已在 §4 compaction 七相落地。
- **向量/重排序接入设计（2026-10-24 网络研究定案，用户点名「不堵塞」为第一约束）**：
  - **两条接入路**：① **Ollama REST**——embeddings 端点原生（nomic-embed-text/mxbai-embed-large/Qwen3-embed），重排模型覆盖不全（上游 issue #17129 活跃）；② **fastembed-rs**——进程内 ONNX（dense/sparse/multivector + TextRerank cross-encoder），HF tokenizers，重依赖评估项。裁决：**先 ① 后 ②**——零绑定成本试场景，fastembed 的 TextRerank 作重排补位候选。
  - **非阻塞架构（本仓积木直配）**：小模型推理走 spawn_blocking/专职 worker **旁路通道**——与 JobRegistry + SteerMailbox 同构，结果以注解/工具结果落回，**大模型热路径永不等待**。2026 业界收敛趋势正是此形态。
  - **五用例（映射已录项）**：① 长期记忆语义检索（C 组「向量记忆」具体化：MEMORY.md/sessions 建 embedding 索引，语义召回增强/替代 FTS5 trigram）；② 翻译优选（用户点名场景）；③ §2 搜索结果 embed-then-rerank 两段式（Cursor 同款）；④ C-2 双轨清洗的相关性打分；⑤ 代码库语义索引——Cursor 架构（AST 分块 + Merkle 增量同步 + embeddings）与 §10.8 tree-sitter 判决联动。
  - **vendor 特色借鉴**：Cursor（AST 分块 + Merkle 增量同步 + 两段式重排）、Continue（embedder/reranker 全可配置全本地最彻底）、本地 RAG 作 **MCP 工具**暴露（检索旁路通道的工具化形态，与 §10.3 MCP host 判决合流）。
- **工具输出清洗（双轨制，采纳）**：省 token 清理 vs 保障原始——**呈现层/上下文层清洗、原始层保底可回取**（本仓 expand/fold 工具输出折叠机制已备双轨骨架）：① ✅ `!CMD` 与 bash 捕获输出统一 **ANSI 剥离 + NO_COLOR=1 注入子进程**（KI-004，防御性净化；2026-10-24 落地：`strip_ansi` 剥 CSI/OSC/charset 序列，前台捕获边界净化，后台 sidecar 存原始，真机验证）；② ✅ 网页内容正文提取（去导航/标签/脚本——readability 层；2026-10-24 落地：`html_to_markdown` SKIP 扩容 nav/header/footer/aside/form/iframe 整块剔除 + `collapse_link_runs` 链接簇折叠（≥4 连续链接主导行→单行标记，句法标点判据保护正文带链接句），零新依赖；真机验证 DeepSeek 定价页无导航残留）；③ md 过度符号压缩（表格/链接转紧凑形，原始可展开）；④ 剥离仅进上下文，磁盘 transcript 恒存原始（后台日志已保原始，前台工具边界的完整双轨待 ContentBlock 双版，现阶段原始保底 = checkpoint + 后台日志）。
- **其他 AI 调用 hf**：判决已录（§10.3 loopback MCP host 按需采纳）+ **现状已可用**：`hf prompt --json`/pipe 模式即 CLI 工具形态（rg/edit/diff 同类），MCP host 是把「全套工具面」暴露给外部 agent 的升级路径。
- **LSP/tree-sitter/ast-grep**：§6 写后诊断（cargo check）已落地；**LSP 不采维持**（进程寿命+误报治理是另一量级，用户对误报污染上下文的顾虑正是当年判决依据）；**ast-grep 轻采纳**——已装则经外部工具注入机制（prompt.rs render_external_tools）天然可用，零代码；**tree-sitter 自研集成缓**（依赖评估，等结构定位真实需求）；语法高亮（edit 工具）缓——markdown IR 管线可扩展非核心。
- **nushell 借鉴**：轻采纳——工具输出的**结构化形态**（JSON mode 可选）与表格渲染可取；nushell 语言本身不实现。
- **搜索/爬虫/正文提取**：§2 已有判决（MergeSearch provider 链 + crawl 工具），本轮补：readability 正文提取并入 web_fetch 清洗双轨。
- **测试异常自动分析**：§6 写后诊断已是雏形（cargo check 失败自动回注）；扩展 = loop 模式相（测试失败 → 自动分析回合）。
- **管道能力**：已具（`hf prompt`/pipe/`--json`/cassette 录放），增强随 §2。

### 15. Agent 循环本体借鉴：凭据自动发现 + 停机保险丝 + 后台任务 + 压缩护栏（2026-11-20 批次，设计规格，未落地）

**来源**：`folke/zaly`（folke，TS 终端 agent）+ `tontinton/maki`（Rust）+ `openai/codex`（Rust），本地克隆 `gitmine/{zaly,maki,codex}`（浅拷贝，不入库）。**全文（含 文件:行号 证据、B/C 档判决、长任务四层保险丝汇总）见根目录 [agents.dev.md](../../agents.dev.md)**，此处只锁方向与落点。

**A1 · 环境变量自动发现凭据（零配置启动，用户点名）**：builtin 表 `api_key_env: &'static str` 升格为复数名单 `api_key_envs: &[&str]`（对齐 zaly `provider.env: string[]` 语义，`auth/manager.ts:242-249`）；`resolve_spec` 的无显式配置分支改为按固定文档化优先级扫描名单、首个非空命中构造 Resolved spec；`CredentialSource::Env{var}` 进 Profile（与 §12 P-A 合流）；doctor 报「检测到 X 凭据但 provider 配置为 Y」（§12 混线 401 的预防面）；抄 codex `env_key_instructions`（`model-provider-info/src/lib.rs:171-175`）给 builtin 加凭据缺失指引文案。落点 `crates/provider/src/config.rs`。**不做**：keychain/其他工具 dotfiles 扫描（沿 `config.rs:598-604` 二来源制）、`!command` bash 凭据求值（第三来源破坏可审计性）、跨供应商择优。

**A2 · StopPolicy 停机保险丝（zaly `stop.ts` 全表）**：token 预算、连续工具错误熔断（成功清零）、同参重复调用检测（`name\0params` 哈希：连续 3 次 + 窗口 10 内重复 4 次双臂）——补进 §4 停机矩阵；现有 loop 64 回合上限只是第一根。

**A3 · 后台任务 + heartbeat（zaly `tasks.ts`）**：工具超 grace（10s）转后台注册表、占位符结果、`task_poll/stop/list` 工具面、task-done 注入 system 消息、心跳注入 running 清单。是 §1 监工-工人的前置件；`bash` 工具率先 Streamable 化。

**A4 · Wakeup hint 不蒸发（zaly `agent.ts:396-430`）**：被提前唤醒取消的 wakeup，hint 折叠为 `status="cancelled"` system 消息随下一注入落地；取消时机精确到 idle/paused→streaming 外部迁移。适用于 loop autopilot 续跑与 `/queue` 泳道。

**A5 · 压缩护栏补强（三仓合流）**：保护尾按字节预算 64KiB 而非条数（maki `compaction.rs:29-32`，条数制被单个巨型 MCP dump 打爆的教训）；背景压缩先行 0.9/同步 0.95 双阈（zaly `agent.ts:479-503`）；同会话同尾消息压缩尝试去重 + ≤N 上限；摘要附确定性文件/bash 使用表直拷（`compactions.ts:98-108`）。全部在 §4 七相内做，不动存储层。

**A6 · 取消传播（maki `cancel.rs`）**：Drop 触发 + TOCTOU 双检 + Weak 子链传播（无后台任务、子 drop 自然出链），对接 P2 并发模型的 token 取消传播。

**A7 · 子代理治理**：深度达上限**滤掉 spawn 工具**而非报错（zaly `agent.ts:176`，模型看不见工具就没有错误路径）；总量与深度分开计（codex `registry.rs`）；子→父转发只带纯文本不带 reasoning/工具轨迹（`swarm.ts:234-245`）。

**A8 · 会话事件上下文化**：统一 notify 队列，模型切换/凭据来源/压力档位折成一条 system 消息注入（zaly `agent.ts:372-378`），是 §12 身份可见在对话内的投影。

**落地顺序建议**：A1（函数级，最高性价比）→ A2 → A5①（一行常量级）→ A6（对接 P2）→ A3（§1 前置件）→ 其余按需。**明确不做**（C 档全文见 agents.dev.md）：双运行时测试矩阵、Lua 插件层、强制单一 wire 协议、daemon/cloud 产品面。

**批次二补录（2026-11-20 晚，goose / gemini-cli / aider，证据见根目录 agents.dev.md 批次二节）**：
- **随既定相顺手带**：retry 指数退避加 ±30% 抖动（gemini `utils/retry.ts`，ST-2 落地时一行）；`hf prompt` 管道模式按 `ErrorKind` 派生退出码（goose `ExitOnError` 思路，R2 已有分级）。
- **新增候选方向（只记录不执行）**：StopPolicy 第三臂——纯文本诵经检测（gemini `ContentChantingDetector`，hf 现在只盯工具调用）；scheduler 启动时清理崩溃遗留的 stale running job（goose `crates/goose/src/scheduler/`，长任务崩溃恢复，ST-5 候选）；keyring 双后端 + env 覆盖通道（goose `config/base.rs`，随 A1）；写类工具前 checkpoint 覆盖审计（gemini；hf checkpoint.rs 已有基建）；用户仓库自动提交纪律（aider，需显式开关，侵入性高）；fallback 模型链（gemini 429 切备用，随 A1/§12 有 provider 池后评估）；压缩自校验轮（gemini，D4.5 后评估）。
- **大件只记录**：repo map（aider `repomap.py`，tree-sitter 符号图 + PageRank + token 预算，长任务跨文件全局视野，需独立批次）。
- **不投入**：LLM 循环裁判（多模型基建成本不符）、多级编辑格式梯子（hf 工具协议单一）、压缩双阈值（HalfWindow 已等价，goose `context_mgmt/mod.rs:29,136-141` 佐证）。
- **§1 子代理素材补充**：goose lead-worker 的 `AbortOnDropHandle`（tokio-util 现成件）+ spawn 信号量并发闸 + 独立 Session 模型，落地时采纳。

### 16. 自身稳定性重构：回合循环止血四相（2026-11-20 批次；R2/R4/S4/S5/S6 非流式部分**已落地**，ST-0/1/2 流式部分仍待夹具）

**背景**：用户实测「任务进行中不能长期稳定运行，会意外重置」。**全文（含代码实证、已排除嫌疑清单、重构方案、验收标准、落地记录）见根目录 [stability.md](../../stability.md)**，此处只锁根因与分相。

**根因清单（源码实证）**：S1 流中挂断零防护（`api/client.rs:263` `chunk().await` 无 idle timeout、无 mid-stream 重连，`drive_stream` 任一错误 → 整回合死）；S2 autopilot 停机判据无错误分级（`autopilot.rs:106-127` 任意失败计 blocked claim，3 连 → mission 持久 Blocked——瞬时网络抖动即可杀死长任务）；S3 压缩护栏默认全关（`hard_trigger: false` / `tail_budget_tokens: 0`，`compact.rs:481-484`，二次 overflow 无降级路径）；S4 config 热重载清 planning/speccing 标志（`main.rs:1036-1040`）；S5 steer 静默蒸发（每回合新 mailbox drop 未消费 steer，`conversation.rs:560-565`）；S6 无 StopPolicy 三保险丝（token 预算/连续工具错误/loop 检测）；S7 流错误丢弃已收 blocks（error 分支不进 transcript，`conversation.rs:717-743`）。

**重构方案**：R1 传输层韧性（idle timeout + 步级重试 ≤2 + blocks 落 transcript）→ R2 错误分级贯通（`RuntimeError.kind` 替换字符串嗅探，autopilot 按类分级 + 退避续跑 + Blocked 恢复路径）→ R3 输入零丢失（steer 回收 + 模式标志单一事实源）→ R4 压缩默认值翻转（窗口模式默认启用 tail budget + 硬触发两级降级）。

**分相路线（依赖排序）**：ST-0 先录三个流故障 cassette（断流/idle 超时/overflow 二连——**先录夹具再动流式路径**是既有铁律）→ ST-1 错误分级 + idle timeout + blocks 落 transcript → ST-2 步级重试 + 压缩两级降级 → ST-3 autopilot 分级 + 退避续跑 → ST-4 steer 回收 + StopPolicy。验收 = 夹具全绿 + 真机长任务 soak。

**落地进度（2026-11-20 第二批，非流式路径，clippy 零 error + workspace 测试全绿）**：R2 已落地（`ErrorKind` + `classify_stream_error` 唯一字符串匹配点 + `TurnOutcome.error_class`，turn.rs 嗅探已删）；S2 已落地（transport 类退避 30s→2m→8m 不计 blocked streak，3 次耗尽记 1 claim）；R4 已落地（窗口模式 tail budget `min(32k, window/8)` + hard trigger 默认开，env 双向覆盖）；S6 已落地（新模块 `runtime/src/stop.rs` 双臂 loop 检测 + 连续 8 批全错工具熔断，端到端测试钉死）；S4 已落地（热重载重放 policy 交换，不清标志）；S5 已落地（终回合 drain steer 入 transcript）。**未动**：S1/S7/R1（流式，夹具铁律——注：第三/四批已补 idle timeout 与 blocks 落盘，见 stability.md §7/§8）、Blocked 恢复路径（`/mission resume` 已有，2026-11-21）。

**续批（2026-11-27，长任务韧性治理，全部离线可测、TUI 项挂冒烟清单）**：
- **错误分类结构化（§12 P-B 提前落地）**：`AgentEvent::Error` 增 `kind` 字段（serde default 保旧夹具回放），三 adapter 从 `ApiError` 变体直接映射（`structured_error_kind`），runtime 优先结构化、`classify_stream_error` 退化为兜底；兜底层同时三层化（重试耗尽 → 权威状态码 → 文本标记 → 独立数字，排除 line/column 序数与 ID 内数字误判）。消除「新 provider 措辞不识别 → 误分类 → autopilot 误计 claim」事故面。
- **悬空 `tool_use` 毒化会话修复（P0）**：熔断/流错误/取消收尾点补记 `[not executed]` 结果 + `repair_tool_pairing` 双向安全网——此前一次熔断后该会话每回合被 provider 400 拒绝。
- **D1.2 双半边补齐**：① 驱动半边——`budget_stop`（REPL/TUI 三处）预算耗尽停 autopilot（mission 保持 Active 供提额）；② 回合半边——`set_turn_token_budget` 回合内软着陆保险丝，双宿主从账本武装（`mission::turn_token_budget`），自主驱动超支钳制在单次软着陆内。**StopPolicy 的 token 预算臂就此结案**（以 mission 预算为口径，非全局常量）。
- **台账清账**：C1（流错误已收 usage 入账）、C3（被拒/无效批次计入熔断）、C4（摘要边路响应取消）全部落地；reflection 回注闭合学习环（任务环 fresh context 注入最近 2 份复盘，`plan.rs::reflection_seed_note`）。
- **TUI autopilot S2 补齐**（挂真机冒烟）：传输退避定时臂 + 真失败 blocked claim + nudge 走 section 队列轮转；三层重试叠加维持现状并记录最坏墙钟（≈1.75h 到 Blocked，全程可停，见 stability.md §9）。KI-005 登记 TUI schedule 永不触发的缺口（待与 S2 同批陪测修复）。

**明确不做**（本批）：后台任务注册表/wakeup/子代理（属扩容，等止血四相验收）；不重写 run_turn 为状态机（P2 一并做）；不动分层边界、不新增依赖。


### 17. 子 agent 竞态编辑保护（2026-11-21 批次，调研定案，未落地）

**背景**：并发审计（2026-11-21）确认跨执行体编辑竞态缺口。jobs 的 `JobRunner` 各建独立 conversation（own session、own client，jobs.rs:145-159）与前台 turn 并发跑，最多 `slots` 个 worker 同时工作；单 turn 内写工具虽已串行（`execute_tools` 读写分组），但**跨执行体零防护**——前台 turn、多个 job worker、甚至两个 hf 进程可同时编辑同一工作区文件。

**根因清单（源码实证）**：E1 `edit_file` 是 read→modify→write_text_atomic（file_ops.rs:347-402），读改写窗口无锁，并发时 last-writer-wins **静默丢编辑**，且 `old_string` 唯一性检查基于可能已过期的读取；E2 `write_file` 全量替换语义，并发即丢更新（语义上可接受，但需知会模型）；E3 `apply_patch` 多文件批量同理；E4 `filelock.rs` 明确只护 durable stores（schedules.json/mission/job status/queue.json），不覆盖工作区文件；E5 `checkpoint::maybe_snapshot_cwd` 在每个写工具入口触发，并发写时两线程可能同时拍快照（次级）。

**方案（2026-11-21 网络调研后修订为三层设计，业界对齐：Claude Code subagent `isolation: worktree` 官方机制、CAID 论文 arxiv 2603.21489 的 per-engineer worktree+merge、社区 blockedBy 任务依赖排序、agenthold/agent-coherence 的 write_cas 乐观并发）**：

1. 第一层（始终开启，E1-E2 落地）：悲观逐文件 advisory 锁。`edit_file`/`apply_patch`/`write_file` 的整个读改写临界区套 `filelock::with_exclusive`，锁文件集中放 `.heartflow/locks/`（目标路径哈希命名，避免工作区散落 `.lock`）。OS advisory 锁天然覆盖双 hf 进程与非 git 目录（worktree 隔离做不到这两点，故此层不可省）。`apply_patch` 批量按排序顺序逐文件加锁避免死锁，绝不嵌套批锁。
2. 第二层（可选增强，随 E2 落地）：锁内写前重验内容 hash（CAS），捕获绕过协调面的写入（用户编辑器、git 操作、其他工具）——锁与 worktree 都探测不到的盲区。mismatch 返回类型化冲突「文件已被并发修改，请重读后重试」，由模型侧重读重试，不自动合并。
3. 第三层（独立演进项，E3 后评估）：git worktree per-job 隔离。git 仓库下 jobs 可选 per-job worktree（对齐 Claude Code `isolation: worktree` 语义），编辑冲突从静默丢失变成 git merge 冲突，由集成点收编。代价：分支生命周期管理、checkpoint 语义按 worktree 变化、untracked 文件不共享（node_modules 级目录需重建）、非 git 目录自动降级回第一层。重度并行（slots 多、任务粗粒度）时收益才显著，属扩容项。
4. 明确不入工具层：blockedBy 式文件所有权分区属提示词纪律（模型自守），非强制机制；`write_file` 保持全量替换语义不引入合并。

**分相路线**：E1 锁基建（locks 目录 + 包装函数，纯函数可测）→ E2 三工具接入临界区 + hash 重验 + 竞态测试（两线程同 edit 同文件，断言两次编辑都生效且无静默丢失；外加绕锁写入的 CAS 冲突测试）→ E3 checkpoint 去抖 → E4（评估后决定）per-job worktree。验收 = 竞态测试在 `--test-threads` 下稳定全绿 + 双 hf 进程手工编辑验证。

**明确不做**：不做多文件原子事务、不做 CRDT/内容合并、不锁读工具（读脏页由 checkpoint + 重读兜底）。


## B. 研究存档与先不做清单

已整体迁出至 [roadmap-deferred.md](roadmap-deferred.md)——外部调研（§2/3/5/7/8/9/10 含 Edit 深挖与 CLI 全景）、「不做」判决（beam search/LSP/ds4 引擎/对抗绕过等）与次级待评估项。以下原 C 组活动候选保留：

## C. 功能增强候选（轻量，待评估）

> C 组为轻量功能候选，待评估是否纳入路线。

### 原有小项（保留）

- **Browser use / computer use**：驱动浏览器与 Windows 桌面操作（点击、输入、截屏、表单填充）的原生工具。
- **长期记忆**：基于向量/embedding 检索的跨会话持久记忆，区别于当前的 FTS5 全文检索。
- **DeepSeek 服务端原生联网搜索**：Responses 方言已可用（`protocol = "openai-responses"`），尚未接的是 DeepSeek 服务端内置的 web search。
- **`hf logs` 子命令**：当前日志仅按 `HEARTFLOW_LOG` 走 stderr、不落盘；要支持一条命令直出历史日志需先引入文件 sink（`tracing-appender`）+ 存储目录 + 轮转策略。
- **全局开关 `--no-confirm` / `--color`**：非交互危险命令的确认策略已在 `--help` 的 INTERACTION CONTRACT 文档化；显式开关会改变安全/渲染语义，按需再评估。
- **REPL 流式双写**（自 list.md #74 收编）：流式期间逐段原样打印 markdown 原文（只上 muted 色），回合结束 `MessageStop` 再把整段 assistant_text 重排一次——用户先看到 md 源码、结束时整块跳动（`.bak` T1）。候选修法：(a) 按块边界（空行、未闭合代码围栏）切分，已闭合块渲染一次不再重绘，重排限于最后一个未闭合块，需行数记账 + 光标上移重画；(b) 流式阶段不打印原始 md、改印进度指示。同类未修项：TUI 流式尾部每帧 clone、表格列宽不按终端宽度封顶（list.md #73/#75）。

---

### 国内 provider 面与 DeepSeek 全家桶接线（2026-11-21 收编，来源：官方 api-docs.deepseek.com + docs.bigmodel.cn + therouter.ai 横评，均已核实）

**已落地（同日 commit）**：builtin provider 表从 2 家扩到 6 家（deepseek/zhipu/kimi/qwen/openai/anthropic），`/provider` 可切换、`/provider NAME <Tab>` 两段补全吃 catalog seed（12 家国内主流端点已 seed）；deepseek 默认模型换代 `deepseek-v4-flash` → `deepseek-flash`（V4.1-Flash；旧名退役但仍服务，deepseek-chat/reasoner 已于 2026-07-24 死亡）；REPL `/models` 命令在线拉取当前 endpoint 模型列表并缓存进 catalog，`/model` 补全即刻可用。

**续接落地（同日，接手批次）**：builtin 表扩至 7 家——新增 `zai`（智谱国际端点 `https://api.z.ai/api/paas/v4`，`ZAI_API_KEY`，默认 `glm-5.3`，catalog 同步 seed）；新增 `{NAME}_BASE_URL` 基址重定向环境变量族（`DEEPSEEK_BASE_URL`/`ZAI_BASE_URL`/`ZHIPU_BASE_URL` 等，仅内置名生效、空值忽略），优先级位于显式 config `base_url` 之下、内置默认之上——插在既定优先级链的 env 位，显式配置不被陈旧全局变量静默劫持（本机 `DEEPSEEK_BASE_URL` 误配为完整端点路径曾实证此风险）。docs/AGENTS/bucket/wiki 四面同步，llms 重生成。

**待接线（按价值排序）**：

1. **DeepSeek 全家桶应用 API**（2026-11-13 开放）：服务端完成工具调用与上下文拼接——对话内开 `deepseek_web_search`/`deepseek_read_url` 参数即得联网搜索/读链接（V4.1-Flash 在 Messages/Responses/ChatCompletions 三协议下支持）；独立接口 Search/Fetch（¥5/千次）、OCR（¥1.5/千页）、ASR/TTS 可单独调用包装成 MCP 工具或 heartflow 原生工具。与既有 web 工具面的关系：heartflow 自有 web_search/fetch 保留（跨 provider 通用），DeepSeek 服务端工具作为 deepseek transport 下的可选增强（参数开关，Token+工具费另计）——**接法**：openai-responses 协议请求体加 `tools` 服务端工具声明 + 流事件解析服务端工具输出块。
2. **DeepSeek Harness**（developer preview，agent harness）：与 heartflow 定位重叠度待评估——先读 Harness Guide 判断其 loop/工具编排设计是否有可借鉴处，而非直接集成。
3. **DeepSeek Anthropic 兼容端点**：`https://api.deepseek.com/anthropic`——可作为 deepseek 的第二 transport 形态（Anthropic protocol），价值有限（OpenAI 格式已全覆盖），仅记录。
4. **模型名录维护机制**：各家改名频繁（DeepSeek 2026-07-24 大退役、DashScope 2026-10-10 sunset 30+ 旧 id）——现状 catalog seed + `/models` 在线发现已够用；若再加，考虑 `hf models --all-providers` 批量刷新而非逐家手更 seed。
5. **peak/off-peak 计费感知**（DeepSeek 忙时 01:00-04:00/06:00-10:00 UTC 加价，已宣布未生效）：usage 记账已有时段信息，待 surcharge 实际生效后评估是否在 usage 报告标注忙时请求占比——先埋点不动。
