# REPL 命令

```text
/help     帮助              /model [NAME] 显示或切换模型
/mode     [NAME] 显示或切换权限模式（read-only/workspace-write/full）
/status   会话状态          /compact      手动强制压缩会话历史（忽略阈值）
/pin      切换末条消息的永不压缩标记（跨 /compact 逐字存活）
/checkpoints 列出 shadow-git 快照（写类工具执行前自动记录）
/restore N 恢复 tracked 文件到第 N 个快照（也可传 sha；/checkpoints 编号）
/focus    切换聚焦模式（隐藏 mascot 伴侣、收窄为最小 chrome）
/save     立即持久化        /clear        开启新会话
/sessions 列出已存会话      /open N       跳回第 N 个已存会话（同 /sessions 编号）
/remember T 追加一条长期记忆到 ~/.heartflow/MEMORY.md（自动去重）
/mcp      列出 MCP 服务器与工具
/search Q 全文检索历史      /exit         退出（自动保存并打印 resume 命令；/quit 为别名）
/init     生成 AGENTS.md 骨架  /expand [ID]  展开上次折叠的工具输出
/guide T  本地零 token 组装“前情/现状/下一步”三段引导草稿，供编辑后发送
/queue [pop|clear]  查看/撤回回合运行期间排队的后续消息（入队注入随 TUI 事件循环上线）
!CMD  不经模型直接跑一条 shell 命令取输出（Windows 走 pwsh，危险命令先确认，输出同工具一样可折叠/展开）
/plan GOAL 规划先行（写仅门禁到 plans/）  /plan approve 逐任务新鲜上下文执行+收尾复盘  /plan end 退出规划
/restart 以全新的配置与 MCP 装载重启进程（链式重启受深度上限约束）
```

## 模型与补全

`/model`（无参）打印当前**真实**传输身份，而非写死某一厂商：

```text
provider: deepseek
protocol: openai
base_url: https://api.deepseek.com/v1
model: deepseek-v4-flash
known models: deepseek-v4-flash, deepseek-v4-pro, deepseek-flash
switch with: /model NAME
```

`provider` 是目录键（未命中目录则 `custom`，env-anthropic 路径为 `anthropic (env)`），`protocol` 是真实协议（`anthropic` | `openai` | `openai-responses`）。`known models` 来自 provider/模型目录（`~/.heartflow/provider.toml` + 内置种子）；`hf models` 自举发现与 `/model NAME` 切换都会把模型回写进目录，下一回合即可补全。传输失败时看这里的 `protocol`/`base_url` 就能确认真正在用哪个协议，不会被 "anthropic" 之类字样误导。

补全不止命令名：在 `/model `、`/mode `、`/open ` 后敲空格，会按命令弹出参数候选表格——分别是当前 provider 的目录模型（附上下文窗口与来源）、权限三档、近期会话序号。↑/↓ 选择，Tab 或 Enter 只补全当前参数位（不整行重置），Esc 取消高亮；候选多时表格随高亮滚动。

## 权限模式

`read-only` / `workspace-write` / `full` 三档，工具级覆盖，REPL 内 `/mode` 热切换（`auto` 归一为 `full`；`HEARTFLOW_PERMISSION_MODE` 另接受 `plan`）。`/plan` 规划模式是硬门禁：仅可写 `.heartflow/plans/*.md`，其余写/bash 一律拒绝；审批后进入 Hermes 任务环逐任务在新鲜上下文执行，收尾把复盘写入 `.heartflow/reflections/`。`read-only` 与 `plan` 两档自动放行标注为只读的 MCP 工具。

## 检查点（shadow-git 快照）

写类工具（`write_file` / `edit_file` / `apply_patch`）执行前自动把整个项目提交进一个**独立影子 git 仓库** `~/.heartflow/checkpoints/<项目哈希>`——你自己的 git 仓库不会被触碰。影子仓库始终以 `--git-dir` + `--work-tree` 显式寻址，强制 `core.autocrlf=false`（防恢复时改写行尾），身份/签名/hook 全部覆盖，项目自身 `.gitignore` 照常生效，`.heartflow/` 会话数据永不入库。

- `/checkpoints` 列出最近 50 个快照（新→旧编号）；
- `/restore N`（或 sha）把 tracked 文件恢复到该快照；**快照之后新建的文件默认保留**（不自动跑 `git clean` 类操作）；
- `/restore N --purge` 显式要求把快照之后新建的文件一并删除：先对当前状态做一次**兜底快照**（输出其编号，`/restore <兜底编号>` 可撤销整个 purge），再按「磁盘可见集 − 目标快照树」的精确差集删除——`.gitignore` 里的文件（如 `target/`）永不触碰，也不递归清空目录。
- 快照失败不阻断写入（warn + 计数，`hf doctor` 可见）——检查点是安全网不是闸门。

## 队列（逐条递送 + 编辑面，§1 P-A/P-B）

队列是**逐条**语义（pi steering 模型）：每条有 id、来源（user/guide/subagent）与生命周期状态，回合结束按到达序**一条一回合**递送；失败只重排**该条**到队首（不再整队重排）。整队合并注入保留为 opt-in（`/queue merge` 切换）——它是上下文体量优化，不是语义。

- `/queue` 列出 pending（含 id 与来源）+ 最近终态台账（delivered/withdrawn/failed，上限 20 条，不丢条目）；
- `/queue send <text>` 手动排队（空闲时也能入队，等当前回合完成后按序执行）；
- `/queue send --lane <name> <text>` 打上**命名泳道**标签（D2.1，qoder 式引导 lane）：泳道是**观测与编辑维度**——`/queue lane <name>` 按泳道查看、列表里以 `@name` 标注；**递送顺序不变**（仍是全局到达序，跨泳道轮转等 TUI P-C 落地再定）；
- `/queue edit <id> <text>` 改文本、`/queue remove <id>` 撤回任意条、`/queue move <id> up|down` 调序；
- `/queue pop` 撤回最后一条（兼容旧语义）、`/queue clear` 清空（进台账可查）。

**队列耐久化（§4 停机安全）**：pending 与台账同步落盘到 `~/.heartflow/sessions/queue.json`（temp+rename 原子写，每次变更即写）——排队消息是用户亲手输入的工作，崩溃/断电/重启**不丢**。恢复规则：`next_id` 从条目重建（防 id 回退碰撞）、limit 钳制到 1..=4096、pending 超限诚实截断（保留最老）；**投递中途崩溃 = 重投**（at-least-once，快照在 take 时仍标 pending），绝不 at-most-once 丢失。损坏的 queue.json 降级为空队并明示（`hf doctor` 也会报），绝不拒绝启动。

## 引导（/guide 三态，§1 D2.5）

`/guide <task>`：从 transcript 与 todo 台账零 token 组装三段草稿（prior work / current state / next task），**默认直接作为下一回合发送**——qoder 式「结合现在的消息、总结之前工作、明确后续任务」的直接发送版。`/guide draft <task>` 只打印草稿供编辑（旧行为），`/guide queue <task>` 入队为下一回合。

## 子代理 job（§1 双 Agent 的工人面）

`/jobs spawn [--write] <task>` 启动**上下文隔离的工人会话**（独立 Session 与客户端，跑完后即弃）：默认 **read-only**（侦察/研究角色，与监工并行写文件是隐患），`--write` 显式 opt-in workspace-write，**full 永不下发给后台工人**。

- 监工-工人唤醒纪律（pi reason 三分法）：工人 `progress` **不唤醒**监工（只在 `/jobs log` 里）；**result 与失败**会作为一条 subagent 队列项注入，成为监工的下一个回合——「监工默认等候，收到工人消息才工作」；
- `/jobs` 列出（queued/running 在前，终态最近 20 条）、`/jobs stop <id>` 协作式停止（在飞调用自然结束、结果丢弃、终态附注明示「任务未完成」）、`/jobs log <id> [n]` 看事件流；
- 记录落盘 `~/.heartflow/sessions/jobs/job-<id>/`（status.json + events.jsonl 追加流）；上次进程死掉时仍标 queued/running 的记录，启动时对账为 stopped（进程死了不等于活在干活）；
- 槽位：并发默认 3（`HEARTFLOW_JOB_SLOTS`），积压上限 20（`HEARTFLOW_JOB_MAX_PENDING`），满则拒绝而非静默堆积；工人不能再派工人（深度=1，结构性护栏）；
- 墙钟超时：单个 running job 默认 30 分钟（`HEARTFLOW_JOB_TIMEOUT_SECS` 覆盖，`0` 关闭）——到期协作式停止，终态附注明示「wall-clock timeout … NOT finished」；计时从排队进入运行开始，排队等待不计费；首个终态裁决获胜，超时裁决不会被稍后 unwind 的工人结果覆写。

**刻意不做**（诚实边界）：Steer 注入在飞回合（前置 = 在飞回合可被事件循环够到，路线图 P-D）、对话回滚、工人侧 spawn 面、`partial`/`paused` 状态（需回合预算）。

## mission 账本（§1/§4 P-A，监工的耐久任务单元）

mission = pi-subagents mission + kimi goal 的合体：一个带**显式完成判据 + 三轴预算（token/回合/墙钟）+ 四态（active/paused/blocked/complete）**的耐久任务单元，落盘 `~/.heartflow/sessions/missions/mission-<id>/`（state.json 原子写 + 256KiB 上限 + receipts 收据流，上限 50 条 + events.jsonl 全量）。反纪律不靠提示词靠代码：

- **反过早完成**：`/mission complete <id>` 只有当**全部判据**过验才放行。机械判据（`cmd <命令>` 退出码 0、`file <路径> <正则>`）完成时**当场重验**；人工判据（`human`、`nodiag <scope>`）必须先 `/mission approve <id> <判据号> [证据]` 拿到收据；`free <主张>` 是唯一弱证据判据——**只有弱证据（或零判据）的 mission 永远拒绝完成**；
- **反过早放弃**：`/mission blocked <id> <原因>` 连续 **3 次**（`BLOCKED_STREAK_NEEDED`）才翻转 blocked 态；中间任何一个 productive turn（`/mission turn <id> [tokens]`）都会清零连击；resume 也会清零；
- **预算不杀**：任一轴 ≥90% 回合末提醒一次（`/mission turn` 的返回里）；耗尽后打上 `budget_exhausted` 标记、**停提醒但不关 mission**（大项目跨多次会话续命）；
- 其余面：`/mission` 列表、`/mission show <id>`（判据勾选态 + 最近 5 条收据）、`/mission pause|resume <id>`、`/mission criterion <id> cmd|file|human|nodiag|free ...` 加判据。机械判据不许 approve（会自验，收据是伪证据）。

**诚实边界**：turn 记账目前是手动面（`/mission turn`），每回合自动记账与提醒注入前置 P2 事件循环；`nodiag` 判据暂为人工批准（rust-analyzer 集成属另一量级）。state.json 与 events.jsonl 的落盘已并入跨进程文件锁（见停机安全章）。

## scheduled-runs（§1 pi-job #6，到期工作外部触发）

`/schedule add <name> interval 30m <prompt>` 或 `/schedule add <name> once +1h <prompt>`（相对）注册定时任务；**无常驻 daemon**——REPL 启动、每回合边界与 `/schedule run-due` 三个时机把到期 schedule 作为**普通工人 job** 派发（read-only，与 `/jobs spawn` 默认同级）。

- `overlap: skip` 是唯一重叠语义：上一个没跑完的周期这一跳直接 **skipped 且记入历史**（不排队、不并行、不静默吞）；
- `catchUp`：错过的 once 默认 `none`（诚实记一条 skipped-catch-up 并消费掉，不会早上 9 点补跑凌晨的活）；`--quiet` 表示成功不唤醒监工（结果去 `/jobs log`，失败仍唤醒）；
- `run_due` 先预订 `last_run_ms` 再派发——调用方中途死掉也不会导致同周期无限重 fire；spawn 被拒（积压满）记入历史为 failed，interval 下个周期重试；
- `/schedule` 列表（含 due 标记）、`/schedule show <id>` 看运行历史、`/schedule pause|resume|delete <id>`；落盘 `~/.heartflow/sessions/schedules/schedules.json`（原子写 + 256KiB 上限，单文件损坏不崩溃、空表起步 + doctor 告警）。

**诚实边界**：once 只支持相对（`+30m`）与 epoch 毫秒两种写法——本地 `HH:MM`/IANA 时区的依赖评估已完成（`iana-time-zone` + `chrono-tz`，可行但编译期/二进制成本待真实需求再付，见路线图），calendar 触发因此暂不可表达。

## 停机安全（§4，长期运行三保证之一）

- **退出即对账**：EOF（Ctrl+D）、`/exit`、`/restart` 三条退出路径都会立刻把仍在 queued/running 的 job 终态化为 Stopped 并附「宿主退出，输出为部分」note 落盘——**首个终态裁决获胜**，稍后 unwind 出的工人结果不可覆写；不必等下次启动的 stale reconcile 才知道真相。
- **协作式停止的边界**：在飞回合不可中断（double-Ctrl+C 取消属 P2 前置项，见路线图）；job 的 stop 在工具调用边界生效，LLM 调用期内天然等待。
- **崩溃面**：队列落盘（见队列章）、mission/schedule 单文件损坏不崩溃（corrupted 计数 + doctor 告警）、job 记录启动对账。混沌测试钉死：垃圾/截断/错类型/错形状四类损坏全部跳过计数、健康邻存不受染、`next_id` 不回退。
- **诚实边界**：非 UTF-8 的 state 文件按损坏计数（曾修复过「静默跳过导致永久不可见」的真缺陷）。

## 跨进程文件锁（§4，多 `hf` 进程共存的基础卫生）

所有耐久 store（`schedules.json`、`mission-*/state.json`、`job-*/status.json`、`queue.json`）的写入都经 `runtime::filelock::with_exclusive`——sidecar `<数据文件>.lock` + OS 建议性锁（Windows `LockFileEx` / Unix `flock`，平台 unsafe 收在 `fd-lock` 内，本仓 `unsafe_code = forbid`）：

- **为什么必须锁**：temp+rename 只保证单次写原子；两进程交错「写 tmp → 截断 tmp → rename」会把对方的字节 rename 成自己的名字（双方还共用同一个 `.tmp` 路径）；
- **为什么锁 sidecar 而非数据文件**：rename 会替换数据文件本体，锁在旧文件对象上等于没锁；
- **失败语义**：bounded 等待 5s（1ms→50ms 退避），超时**跳过写并计入磁盘失败**（doctor 可见）——绝不无锁裸写；锁在持有者进程死亡时由内核释放，无 stale-lock 问题；
- **读者不取锁**：rename 快照读本就原子，陈旧快照（at-least-once）优于启动阻塞；
- **同线程禁止嵌套**：内层无法区分「自己进程持有」与「别的进程持有」；
- 顺带修复：job `status.json` 从裸 `fs::write` 升级为锁内 temp+rename（裸写崩溃留撕裂文件，破坏启动对账）。

诚实边界：这只序列化写，不是多写者数据库——各进程内存仍是事实源，并发写是「最后落盘者胜」；真正的单写者 World 仍是 P2。job id 是进程本地序号，两进程共用 `JOBS_ROOT` 会逻辑互踩（schema 级遗留，见路线图）。测试含真跨进程互斥与双线程 RMW 序列化两条硬证明。
