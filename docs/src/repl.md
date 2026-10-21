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
- `/queue edit <id> <text>` 改文本、`/queue remove <id>` 撤回任意条、`/queue move <id> up|down` 调序；
- `/queue pop` 撤回最后一条（兼容旧语义）、`/queue clear` 清空（进台账可查）。

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
