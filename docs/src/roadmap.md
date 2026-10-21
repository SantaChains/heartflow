# 路线图

以下方向已明确规划但**尚未实现**，仅作路线记录。2026-10-19 批次新增 §1–§6 六条主线，2026-10-21 批次新增 §7（推理引擎生态深研）、§8（bash job 监控 / job 超时 / 挂起检测）并续批扩写 §1（多子代理 spawn 面 / pi 的 job 系统 / 队列三件套设计规格——源自 `archive/pi-research/pi-subagents` 源码实证与 `openmemory.md.bak` 的队列需求回溯），全部来自外部项目调研，本地研究副本均**不入库**：pi-subagents/MergeSearch 在 `archive/pi-research/`；ds4（antirez DwarfStar 推理引擎）、easy-llm-cli（Gemini-CLI 派生 agentic CLI）、kimi-code（`MoonshotAI/kimi-code`，MIT © 2026 Moonshot AI，agent-core-v2 已源码核实）、DeepInfer（deepseek-ai，Apache 2.0）与 deepseek-recipe（deepseek-ai，MIT，纯 Rust）在 `.workbuddy/`。当前**不执行**，只锁定设计判据与借鉴对象。2026-10-22 批次新增 §10（PandaCode / pi / codex / claude 设计借鉴与取舍——源自 `archive/pandacode` 浅克隆源码实证，聚焦 mcp 与 code-mode 升级取舍），聚焦「采纳 / 改造 / 不做」三档结论。

> **kimi-code 自研度结论（源码核实）**：agent 核心 `packages/agent-core-v2` 是**一方自研包**，**非 gemini-cli / claude-code / codex 二改**（grep 三家零命中）；仅 TUI 层源自外部 `pi-tui`（入库为 `@moonshot-ai/pi-tui`，README 致谢），并采纳行业开放协议 MCP/ACP 与 Agent Skills 约定。另有一方包可对标 heartflow：`@moonshot-ai/kosong`（"LLM abstraction layer"，直接包 `@anthropic-ai/sdk` + `openai` SDK ≈ heartflow `api`+`provider`）、`@moonshot-ai/kimi-telemetry`（一方遥测）、`acp-server`。**下面标注「源码实证」的条目均已对照 `.workbuddy/kimi-code` 当前 HEAD 核实行号。**


> **分组索引（2026-10-22 重排）**：本文按性质分三组——**A. 执行路线图**（要改 heartflow 自身代码的硬骨头，按依赖排序，含 §1/§4/§6 落地方向与 §11 执行序列）、**B. 外部调研与借鉴**（只记录不执行，§2/§3/§5/§7/§8/§9/§10）、**C. 功能增强候选**（轻量待评估，原有小项）。原 §1–§11 编号保留作交叉引用锚点，组内子标题整体降级一级。

## A. 执行路线图（要改 heartflow 代码的硬骨头，按依赖排序）

> A 组为落地任务：双 Agent 架构（§1）、工程稳定性与持久性（§4）、编辑后反馈（§6）三方向的设计规格与已落地状态，以及把「要动大代码」条目抽出的执行序列（§11）。B 组调研是 A 组的依据来源。

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
- **mission 账本（2026-10-22 批次，§1/§4 P-A 落地）**：`runtime::mission`（`MissionLedger`）——四态 `Active/Paused/Blocked/Complete`（kimi `GoalStatus`）+ 三轴预算（token/turn/wall_ms，kimi `GoalBudgetLimits`）+ 判据五型（`CommandSucceeds`/`FileMatches` 机械型完成时**当场重验**，`HumanApproved`/`NoDiagnostics` 须显式 approve 收据，`FreeText` 唯一弱证据型）；**D1.1 反过早完成/反过早放弃是类型错误不是提示词**——complete 全判据过验才放行（弱证据-only 拒绝 `WeakOnly`）、blocked 需连续 3 次声明且 productive turn 清零连击；**D1.2 预算不杀**——≥90% 提醒一次、耗尽打标停提醒不关 mission；落盘 per-mission `state.json`（temp+rename 原子写 + 256KiB 拒写不截断 + 失败计数）+ receipts（上限 50）+ `events.jsonl`；锁污染一律 `PoisonError::into_inner` 恢复（本仓纪律，棘轮零 debt）。REPL `/mission` 家族 + doctor 磁盘级统计段。诚实边界：turn 手动记账（自动注入前置 P2）、nodiag 暂人工批准（rust-analyzer 集成属另一量级）；落盘已并入 §4 跨进程文件锁。
- **scheduled-runs（2026-10-22 批次，§1 pi-job #6 落地）**：`runtime::schedule`（`ScheduleStore`）——`once`/`interval` 两触发型（pi 九动作的 MVP 子集）、**overlap=skip 唯一重叠语义（skip 记入历史不静默吞）**、`catchUp none|latest`（none 错过即诚实记 skipped-consumed）、`--quiet` 成功不唤醒监工、history 上限 100；`run_due` **先预订 `last_run_ms` 再派发**（调用方崩溃也不无限重 fire）+ `record_spawn_refused`（积压满诚实记 failed，interval 下周期重试）；REPL 三个 drain 时机（启动/回合边界/`/schedule run-due`）把到期 schedule 派为**普通 read-only 工人 job**——调度零新增执行机制；job 终态经链接表对账回写 schedule 历史（「结果落盘 ≠ 退出证明」）；落盘单 `schedules.json`（原子写 + 256KiB + 损坏不崩溃）。诚实边界：once 只支持相对/epoch 毫秒、calendar 不可表达。**chrono-tz 依赖评估（2026-10-22，结论：可行、缓做）**：本地 `HH:MM`/IANA 触发需要「本地时区名 + 规则库」两件——`iana-time-zone`（极小，取本机 IANA 名）+ `chrono-tz`（内嵌全量 IANA 数据库，编译期 +10s 级、二进制 +~1MiB）；本仓无 chrono，引它即连 chrono 一并进树。DST 边界（不存在的时间、重复时间）是 chrono-tz 的成熟处理面，自研 TZif 解析不划算。裁决：epoch-ms + 相对偏移已覆盖调度自动化场景，等真实本地定时需求出现再引，避免为不存在的功能付编译期税。
- **队列命名泳道（2026-10-22 批次，D2.1 数据结构半边）**：`QueueEntry.lane` 标签 + `send_in_lane`/`lane_names`/`items_in_lane`；REPL `/queue send --lane <name>`、`/queue lane <name>`、列表 `@name` 标注。**纪律：泳道是观测与编辑维度，递送仍是全局到达序**——跨泳道轮转规则需要 TUI 侧（P-C）才有意义，离线拍脑袋定轮转属猜测。
**落地状态（2026-10-21 第二批网络核验，Qoder 官方文档对照）**：Qoder 官方「排队 vs 插话」二分（docs.qoder.com conversation-input）与本仓 `FollowUp` vs `Steer`（P-D）**语义同构**——插话「在安全执行边界交给 Agent，不直接停止当前执行」正是本仓「明确不做强插打断」的官方版；排队消息管理面「插话 / 撤回到输入框编辑 / 移除 / 多条重排」= `/queue send/remove/move/edit` 已覆盖；「设置 → 运行中发送方式」= D2.3 默认 FollowUp；composer 在「压缩中 / 有待处理请求」时提示不支持插话 → TUI P-C 待做项。qoder 式「引导 = 结合当前消息、总结之前工作、明确后续任务后发送」= `/guide` 三段草稿 + 默认直接发送，已实现。生态侧：ACP `_session/steering` 扩展（Claude/Codex 已支持，agent-shell 0.78 的 RET=排队 / M-RET=插话）→ 若将来做 ACP server 可对齐；灵码（Qoder CN）的「消息排队 + 全自动执行模式带终端命令禁止名单」中，禁止名单与既有 `is_dangerous_command` 分级确认同族。

- **待做**：P-C（TUI 队列编辑键位 + 引导 overlay——本仓铁律：TUI 派发不盲改，需真机冒烟）、P-D（Steer，前置 P2 并发模型）、per-section 泳道递送轮转（D2.1 的 TUI 半边；数据结构与 REPL 观测面已落地）、mission 回合自动记账与提醒注入（D1.3，前置 P2 事件循环）。跨进程文件锁已落地（§4，2026-10-22 批次）。


### 4. 工程稳定性与持久性（大项目持续运行）

**部分落地（2026-10-22 批次：停机安全 + 长期运行）**：
- **队列耐久化**：pending + 终态台账同步落盘 `sessions/queue.json`（原子写、每次变更即写）；恢复时 `next_id` 从条目重建、limit 钳 1..=4096、pending 超限诚实截断保最老；**投递中途崩溃 = 重投（at-least-once）**，绝不 at-most-once 丢失；损坏降级空队 + 明示 + doctor 告警，绝不拒绝启动。混沌测试钉死（roundtrip/损坏/防回退/重投/钳制/截断 6 条）。
- **退出即对账**：EOF/`/exit`/`/restart` 三路径统一收口 `JobRegistry::finalize_on_shutdown`——非终态 job 立即 Stopped + 「宿主退出，输出为部分」note 落盘，首个终态获胜不可被 late completion 覆写（测试钉死）；不必等下次启动 reconcile。
- **抗损坏台账闭环**：mission load/summarize 对非 UTF-8 state 文件从「静默跳过（永久不可见）」改为「计入 corrupted 台账」——混沌测试抓出的真缺陷；垃圾/截断/错类型/错形状/缺文件五类损坏全部跳过计数、健康邻存不受染、`next_id` 不回退、doctor 可见。
- **跨进程文件锁（2026-10-22 批次）**：`runtime::filelock`（`with_exclusive` + `lock_path`）——sidecar `<data>.lock` + OS 建议性锁（Windows `LockFileEx` / Unix `flock`，经 `fd-lock` 封装：本仓 `unsafe_code = forbid`，平台 unsafe 收在实战检验的依赖内，锁语义自持）。**为什么必须锁**：temp+rename 只保证单次写原子，两进程交错 `写 tmp → 截断 tmp → rename` 会把对方的字节 rename 成自己的名字；**为什么锁 sidecar**：rename 会替换数据文件本体，锁在旧文件对象上等于没锁。设计纪律：bounded 等待 5s（1ms→50ms 退避）超时**跳过写并计数**（绝不无锁裸写）、锁失败与写失败同入 `DISK_WRITE_FAILURES`（doctor 可见）、读者不取锁（rename 快照读本就原子，陈旧快照优于启动阻塞）、**同线程禁止嵌套**（内层无法区分自持与外占）。接入面：schedules/mission `state.json`/job `status.json`/queue.json 四处 persist + mission/job 两处 `events.jsonl` append；顺带把 job `status.json` 从裸 `fs::write` 升级为锁内 temp+rename（裸写崩溃留撕裂文件，破坏启动对账）。测试 7 条含**真跨进程互斥**（重执行测试二进制为持锁子进程，父进程 bounded 尝试必超时、子进程退出后内核释放锁可获取）与双线程 RMW 序列化（无锁必丢更新）。诚实边界：这只序列化写，不是多写者数据库——各进程内存仍是事实源，并发写世界仍是「最后落盘者胜」；真正的单写者 World 仍是 P2。**遗留记录**：job id 是进程本地序号，两进程共用 `JOBS_ROOT` 会逻辑互踩（schema 级问题，job 目录需带 pid/会话命名空间，另行处理）。
- **依赖治理：终端栈收敛 + dependabot 分支裁决（2026-10-22 批次）**：4 条 dependabot 分支按「最优工程取舍、依赖尽量最新」裁决——**采纳 2、结构化采纳 1、否决后超越 1**。① `actions/checkout v4.4.0→v7.0.1`（release.yml 三处钉 SHA，与 ci.yml/docs.yml 对齐）；② `jsonschema 0.57→0.59`（使用面仅 `validator_for`，编译即验证）；③ **ratatui 0.29→0.30.2 结构化迁移**：dependabot 机械 bump 实证是错的（lock 里 ratatui 0.29+0.30 双拷贝），正确路径 = tui-textarea 钉上游 main rev `0ce5d4b`（crates.io 0.7.0 仍钉 ratatui ^0.29；其 `no-backend` feature 弃掉自带 crossterm 0.28 模块）+ 自写 `editor::tui_input`（crossterm 0.29 KeyEvent → tui-textarea `Input`，逐行镜像上游语义含 Release 事件吞咽防 Windows 双写）+ `Backend::Error` 关联类型适配（0.30 新增；`CompanionTerminal` 收紧 `Error = io::Error`，测试新增 `IoTestBackend` 把 TestBackend 的 `Infallible` 抬升）。**成果：crossterm 双拷贝收敛为单 0.29（与 inquire 0.29 天然同源），比升级前少一个拷贝**；`unstable-rendered-line-info` 移居 ratatui-widgets（umbrella 不再转发，直连开启）。④ crossterm 单独 bump 0.28→0.29 的分支**否决**（ratatui 0.29 钉 0.28，单独升是错）。防再犯：dependabot.yml 对 ratatui/crossterm 加 ignore（解除条件 = tui-textarea 0.7.1 发布）。全树 `cargo update` 刻意不做（brotli 8→9 等无关 major 会破坏可归因性，另列任务）。
- **待做**（依赖排序）：在飞回合取消（double-Ctrl+C，前置 P2 事件循环可够到在飞回合）、cassette 录制扩展为 ds4 式「模型加载/提示渲染/工具调用/会话状态」集成测试、发布 QA 清单落成仓库内可勾选文档（ds4 `QA_BEFORE_RELEASES.md` 纪律）、全树 `cargo update`（无关 major 的传递刷新，独立小步验证）。

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
- **Phase 1b 硬触发（D4.2）⏸ 未落地**：与请求构造/重试路径相邻，属流式路径改动——**动之前必须先录 cassette**（本仓铁律，当前无已录夹具）；且需先核实 `api` 错误枚举是否已区分上下文超限。
- **Phase 2 尾部 token 预算（D4.3）✅ 已落地（配置门控）**：纯函数 `plan_compaction_cut(messages, config) -> CutPlan { keep_from, tail_tokens }`（条数下界 + token 上界，前移吞消息、成对吞 ad+result、绝不空尾、单条独超预算如实上报）；env `HEARTFLOW_COMPACT_TAIL_BUDGET` 启用，默认 0 = 现状。
- **Phase 3a 省略声明（D4.4）✅ 已落地**：`get_compact_continuation_message` 增 `dropped_token_estimate` 参数，续延句补「Roughly N estimated tokens of older conversation were dropped here during compaction.」（N = 被摘要区域启发式估算，明标 estimate；pinned 排除在外）。
- **Phase 3b 摘要模式（D4.5）⏸ 未落地**：`Model` 分支需在压缩路径内走 LLM 调用，同属流式路径相邻改动，先录 cassette 再做；默认 Deterministic = 现状，无用户可见损失。
- **Phase 4 不变量测试 ✅ 已落地（并当场揪出一个真 bug）**：`compaction_pipeline_preserves_tool_pairing_for_random_well_formed_sessions`——200 例确定性 xorshift 随机良构会话（混合 tool_use 广告/结果、pinned 回合）经 compact + replay + repair 后断言三不变量（尾不孤立、result 必有前序 ad、ad 必有后继 result）。**由 property test 揪出并已修**：`/pin` 若落在带 `tool_use` 的 assistant 消息上，其 result 被摘要后线上会悬空 ad（provider 400）——修复 = pinned 广告拖行紧随的 result 一并 verbatim 存活（`pin_following` 逻辑 + 专项测试）。
- **回滚**：新行为全部挂 `CompactionConfig` 新字段（默认值 = 现状），`tail_budget_tokens` / `soft_trigger` 可单独回退；触发口径只影响「何时压」，不影响 transcript 结构不变量。

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

#### 11.1 TUI 升级（用户点名，真机冒烟陪测）

- **P-C TUI 三件套（§1 P-C）**：回合内输入路由补 `DeliveryMode` + 计数显示；队列编辑键位（withdraw/edit/reorder/改派）；引导 overlay（T6 两栏可编辑确认）。
- **KI-001 Alt+Enter 全屏**（known-issues.md）：交互式 PTY 内 Alt+Enter 被 TUI 按键拦截层吃掉，未透传 → 交互态激活时透传 PTY。
- **KI-002 Ctrl+C 提示过短**（known-issues.md）：打断/确认提示走瞬时 `status_note` 被下一帧覆盖 → 改为持久状态栏保留到结算或显式超时。
- **已知未修（§9）**：TUI 流式尾部每帧 clone、表格列宽不按终端封顶 → 端到端冒烟 + cassette 后改。
- **铁律**：TUI 斜杠命令派发（`println!` 与 alt-screen 不兼容）刻意不盲改，只有真机冒烟能验。
- **依赖**：KI-001/002 是「拦截层局部透传」小改，可**先于 11.0** 落地；P-C 三件套建议 11.0 后做最稳。

#### 11.2 P-D Steer（注入在飞回合）

- 前置 11.0。递送语义 `Steer` = 注入当前回合（§1 D2.x）；默认 `FollowUp`，`Steer` 经 11.0 可用。逐条 steer 递送 + 失败按条目重排 + 明示（§1 P-D/P-E）。

#### 11.3 mission 回合自动记账 + 在飞回合取消

- **D1.3 runtime 续跑驱动器**：每 turn 开头注入 mission reminder（status + criteria + 预算剩余）；截止调度（到期限提醒，不杀）。前置 11.0 事件循环够到。
- **在飞回合取消（§4）**：double-Ctrl+C 取消在飞回合。前置 11.0。

#### 11.4 per-section 泳道递送轮转（TUI 半边）

- 数据结构（`QueueEntry.lane`）+ REPL 观测面已落地（§1 2026-10-22 批次）；TUI 侧轮转规则（`MAX_SECTIONS=9`，`tui.rs:1103`）未做。前置 11.1 TUI 升级的队列编辑键位。

#### 11.5 次级 / 独立（不阻塞主线）

- **§4 cassette 录制扩展**为 ds4 式集成测试：先录夹具（铁律），再动流式路径。
- **§10.2 ephemeral + session resume** / **§10.4 `agent_settled` 真终态**：与 11.0 同源，可在 11.0 一并收口。
- **跨进程单写者 World（P2 跨进程）**：进程内 Semaphore 先行，跨进程槽位留给 bash 长任务外置（§1）再搬。
- **全树 `cargo update`**：无关 major 的传递刷新，独立小步验证（brotli 8→9 等）。
- **§7.1 deepseek-recipe 引入评估**：外部依赖大决策，须先验证 MSRV/构建体积，不在硬骨头首批。

#### 执行顺序建议（明日）

1. **11.0 根因锁**（最大架构块，先做解锁一切）—— 本环境无法真机冒烟，用户陪测。
2. **11.1 TUI 升级 + KI-001/002**（用户点名陪测）—— KI-001/002 小改可先于 11.0。
3. **11.2 / 11.3**（Steer + mission 记账 + 取消，前置 11.0）。
4. **11.4**（per-section 轮转，前置 11.1）。


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

**依赖评估（未来若评估引入，须先验证，当前不跑 cargo）**：① recipe 未声明 `rust-version`，其 edition 2024 + 依赖树是否突破 heartflow MSRV 1.88 需实跑验证；② `tokenizers` 是重量级原生依赖，Windows 构建体积与 CI 时长成本需量化；③ tokenizer.json 获取路径：HF Hub 运行时下载（http feature）vs 随仓库分发——按本仓 vendored 纪律，未修改上游资产不入库，但「clone 即可跑」检验法要求有确定获取路径，二者需折中；④ 引入属**外部依赖**而非衍生代码，按 AGENTS.md 第三方纪律在 README §致谢点名（NOTICE 仅针对衍生/移植代码）；⑤ 更轻替代：只借鉴 special-token 边界编码与流式状态机方法论，tokenizer 单引 `tokenizers` crate 自接。

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
| 基准套件 | hyperfine 外部基准（启动/冷回合/渲染吞吐）+ 内建 bench（`cargo bench` 尚无 harness）；量化驱动决策要求基准先行 | **未做**；先录 cassette 再建基准，防「更快但漂移」（ds4 纪律 #1：正确性先于速度） |
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
