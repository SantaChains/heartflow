# stability.md — 自身架构分析、稳定性隐患与重构路线（2026-11-20）

> 背景：用户实测「任务进行中不能长期稳定运行，会意外重置」。本文回答三件事：① 现有回合循环的真实形状（代码实证）；② 「意外重置」的根因清单（每条带 `文件:行号`）；③ 重构方案与分相路线（对齐 zaly / maki / codex 的成熟模式，判据沿用 `agents.dev.md`）。
> 方法：只写核实过的。每条隐患给出「触发条件 → 用户可见症状 → 修复落点」。已排除的嫌疑也列出，防止日后重查。

---

## 1. 现有回合循环的真实形状

REPL 主循环（`cli/main.rs:1047-1108`）→ `run_turn_interactive`（`cli/turn.rs:341`，吞错返回 `TurnOutcome`）→ `AgentRuntime::run_turn_with_blocks`（`runtime/conversation.rs:625-827`）单循环：

```
每迭代：
  ① 迭代上限软着陆（>64 iterations，保留已完成工作，conversation.rs:645-659）
  ② 取消检查 + steer 消费（conversation.rs:662-676）
  ③ 预压缩（>50% 窗口，Model 摘要可降级，conversation.rs:683-698）
  ④ 组请求：repair_tool_pairing(build_replay_messages(...))（conversation.rs:704-711）
  ⑤ 流式请求 → consume_stream（biased select! 取消优先，conversation.rs:829-905）
  ⑥ 流错误 → overflow 硬触发重试一次 / 否则整回合 Err（conversation.rs:717-742）
  ⑦ 工具授权 → 并发执行（写类串行）→ 回到 ①
```

后置：`maybe_auto_compact`（REPL 回合后）+ `maybe_autopilot`（loop 模式续跑，`autopilot.rs:65-131`）+ 静默会话保存（失败也保存，`turn.rs:450-455`）。

**已排除的嫌疑**（核实过，不是根因）：
- ❌ 回合失败杀死 REPL——`run_turn_interactive` 内部吞错渲染「Turn failed」，REPL 存活（`turn.rs:418-461`）。
- ❌ config 热重载丢会话——`reload_config` 用 `runtime.session().clone()` 保会话重建（`main.rs:3094`）。
- ❌ panic 失控——panic 棘轮 debt=0 / justified=7，生产 panic 面受控。

---

## 2. 根因清单：为什么长任务会「意外重置」

### S1 · 流中挂断零防护 ★ 最可能的直接元凶

**证据**：请求**建立**有重试（`api/client.rs:126-133` `send_with_retry`），但流**开始后**裸奔：`MessageStream::next_event` 的 `self.response.chunk().await`（`client.rs:263`）**无 idle timeout、无重连**。`drive_stream` 任一 `next_event().await?` 失败 → 一条 `AgentEvent::Error` → `conversation.rs:717` 整回合判死。

**症状**：长任务跑几十分钟，服务器半死连接 / 移动网络切换 / 代理抖动，一次流中断 = 整回合白跑（工具已执行的部分虽在 transcript，但模型回复丢失），体验即「突然断了/重置了」。

**对照**：codex 的 `stream_idle_timeout_ms`（连接静默即判死重连）+ `stream_max_retries`（`model-provider-info/src/lib.rs:213-217`）；zaly 每步是独立请求，失败只影响单步。

**修复落点**：
1. `MessageStream` 加 idle timeout（默认 ~90s，`HEARTFLOW_STREAM_IDLE_TIMEOUT_MS` 可配）——超时视为可重试的传输错误。
2. `drive_stream` 错误分级：transport 类（connect/timeout/reset）→ 上抛为**可重试**事件；`conversation.rs` 收到可重试传输错误时**重建请求重试同一步**（N≤2），已收 blocks 保留。
3. error 路径把已收 blocks 推进 transcript（对齐 `!finished` 分支，`conversation.rs:744-751`），消除「屏幕上有字、上下文里没有」的漂移（同属 S7）。

### S2 · autopilot 停机判据无错误分级 ★ loop 模式任务死的直接机制

**证据**：`autopilot.rs:106-127`——任何 `outcome.ok == false` 都记 blocked claim，3 连（`BLOCKED_STREAK_NEEDED`）→ mission 翻 **Blocked（持久）**，autopilot 停机。错误**不分级**：429 突发、瞬时断连、auth 失效、二次 overflow 全部同罪。中间无成功回合，streak 不清零。

**症状**：长任务里三阵网络抖动（哪怕间隔一小时），mission 死，回来看到「blocked after 3 consecutive failed turns」——用户视角就是「任务自己重置了」。且 Blocked 是**持久状态**，不自动恢复。

**修复落点**：
1. `TurnOutcome` 增加 `error_class`（复用 §12 判据 3 的分类：transport / auth / overflow / model / other），transport 类**不计入** blocked streak（或计 0.5 权重：连续 transport 失败先退避重试同一 nudge，连续 3 次同一步失败才计 1 claim）。
2. transport 类失败在 autopilot 内做指数退避续跑（30s→2m→8m），带「第 N 次传输重试」提示；人类可打断。
3. Blocked mission 给出恢复路径：`/jobs` 或 mission 工具显式 `unblock`，并在 reminder 里写明（kimi 的「blocked 需同一阻塞连续 ≥3 个 goal 回合」判据——**同一**阻塞，不是任意三次）。

### S3 · 压缩护栏已实现但默认全关

**证据**：`hard_trigger: false`、`tail_budget_tokens: 0`（count-only cut，`compact.rs:481-484`）都是默认关。maki 的两个血泪教训（保护尾按字节、保留区 ≤50% 窗口防小窗自毁）实现了一半：D4.3 有 tail budget 但要 env 开；硬触发重试只有一次，二次 overflow 判真死（`conversation.rs:723-742`），无「压缩后仍超窗」的降级路径（砍保护尾再压一次）。

**症状**：长任务下保护尾被巨型工具结果占据（count-only 切割挡不住），压缩后仍超窗 → 硬触发重试一次再失败 → 回合死 → S2 计 claim → 任务死。

**修复落点**：默认值翻转或按窗口大小自动启用：`context_window_tokens > 0` 时 tail budget 默认 `min(32k tokens, 窗口/8)`；硬触发重试改两级——第一级正常压缩，仍 overflow 则第二级「保护尾减半再压」（对齐 maki `MAX_RESERVED_PERCENT` 50% 上限的自毁防护）。

### S4 · config 热重载清模式标志

**证据**：`main.rs:1036-1040`——config 变动 → `planning = false; speccing = false`。任务进行中 config.toml 被改（agent 的 edit_file 完全可能改到）→ plan/spec 模式的回合契约段静默消失，下一回合模型丢上下文约束。

**修复落点**：reload 只在**非任务中**清标志；或改成「重建 runtime 后重放当前 mode 的语义」——mode 本来就从 `lock_state` 持久读，`planning`/`speccing` 这两个内存布尔与持久 mode 有漂移窗口，统一为单一事实源。

### S5 · steer 静默蒸发

**证据**：`conversation.rs:560-565`——每回合 `set_steer_mailbox` 重建，旧 handle drop；注释自认 "drops any stale handle a finished turn left behind"。steer 只在迭代边界消费（`conversation.rs:671-676`）：**最终无工具的回合**里到达的 steer 未消费，回合结束 mailbox 销毁，用户消息凭空丢失。

**修复落点**：回合收尾时 drain 未消费 steer → 返回给调用方（`TurnSummary.pending_steers`）→ REPL 端「你的 steer 将随下一回合注入」提示 + requeue 到队列头部。zaly 的对应物是 inject 队列跨回合存活（`agent.ts:296-317`），hf 的队列本就持久，缺的只是这条回收线。

### S6 · 无行为层熔断（StopPolicy 对照）

**证据**：有步数软上限（`max_iterations=64`）、续跑上限（`max_continuations=3`），但没有：token 预算、连续工具错误熔断（成功清零）、同参重复调用检测。模型打转（同参反复 read_file）只烧钱不停车，直到 64 迭代软着陆。

**修复落点**：`agents.dev.md` A2 的 StopPolicy 表照抄——哈希 `name\0params`，连续 3 次 + 窗口 10 内重复 4 次双臂；挂 `conversation.rs` 迭代边界，与 S1/S2 共用 `error_class` 通道。

### S7 · 流错误丢弃已收 blocks（并入 S1.3）

**证据**：`conversation.rs:717-743` error 分支 `return Err` 时 `blocks` 直接丢弃；只有 `!finished` 分支（744-757）保留。同一事件流里模型已输出的半截回答不进 transcript。

### 环境性风险（不在代码内，但属「长任务不稳定」全景）

- **D 盘写满**：曾 100% 占满（记忆实录）。长任务高频写会话/队列/日志，写失败 → 队列持久化降级（`main.rs:1125-1133` 有诚实告警）→ 崩溃丢队列。doctor 应加磁盘余量预检。
- **cargo 目标目录/增量缓存膨胀**挤压磁盘——运维项，不是代码项。

---

## 3. 重构方案：回合循环的四个升级（对齐三仓模式)

### R1 · 传输层韧性（S1+S3+S7）——「流会断，断可续」

- `MessageStream` 加 idle timeout；`drive_stream` 错误带分类结构化上抛（不再字符串化后 grep——`is_context_overflow_error` 的字符串匹配现状注明了这是债，`compact.rs:556-560`）。
- `conversation.rs` 一步失败：transport 类 → 原地重建请求重试（≤2 次，退避）；overflow 类 → 压缩重试（两级，见 S3）。
- 全部已收 blocks 落 transcript。
- **验收**：cassette 夹具模拟「message_start 后断流」「半途 idle 超时」「overflow 二连」，三夹具全绿。

### R2 · 错误分级贯通（S2+S6）——「失败有类，类定动作」

- `RuntimeError` 加 `kind: ErrorKind`（Transport/Auth/Overflow/Model/Cancelled/Other），替换 `error.to_string().contains("cancelled")` 这类字符串嗅探（`turn.rs:419` 现状）。
- `TurnOutcome` 加 `error_class`；autopilot 按 S2 分级；REPL 提示按类分流（对齐 §12 判据 3 已定文案）。
- StopPolicy（token 预算 / 连续工具错误 / loop 检测）挂迭代边界。

### R3 · 输入零丢失（S4+S5）——「用户的每个字都有去向」

- steer 回收线（S5）+ 模式标志单一事实源（S4）。
- **验收**：终回合注入 steer 的集成测试；config 热重载在 plan 模式中不丢 mode 段的测试。

### R4 · 压缩默认值翻转（S3 落地相）

- 窗口模式下 `tail_budget_tokens` / `hard_trigger` 默认启用；硬触发两级降级。
- **验收**：commit cassette 回归（压缩路径必须先录夹具——现有纪律）+ 配对计数（`record_repair_drops` 已有，捡现成）。

### 不重构的部分（明确判据）

- 不动 `api → provider → runtime` 分层边界（AGENTS.md 硬约束）；错误分类在 `runtime` 层定义、`api` 层标注，不引反向依赖。
- 不引入后台任务/心跳（A3）——那是 §1 监工前置件，等 R1-R4 稳住单回合再说。**先止血，再扩容**。
- 不把 run_turn 重写成状态机（kimi 式）——P2 并发模型落地时一并做，现在动是双倍风险。

---

## 4. 路线（按依赖排序，供 roadmap 收编）

| 相 | 内容 | 规模 | 验收 |
|---|---|---|---|
| **ST-0** | 录三个流故障 cassette 夹具（断流/idle 超时/overflow 二连） | 小（`HEARTFLOW_CASSETTE=record`，保真在 AgentEvent） | 夹具入库、回放可复现故障 |
| **ST-1** | RuntimeError 错误分级 + 流 idle timeout + 已收 blocks 落 transcript | 中 | ST-0 夹具全绿；`turn.rs:419` 字符串嗅探删除 |
| **ST-2** | 步级传输重试（≤2，退避）+ 压缩两级降级 + tail budget 默认启用 | 中 | overflow 二连夹具 → 压缩后成功 |
| **ST-3** | autopilot 错误分级 + 退避续跑 + Blocked 恢复路径 | 中 | 传输类失败不计 blocked streak 的单测；真机长任务 soak |
| **ST-4** | steer 回收 + 模式标志单一事实源 + StopPolicy 三保险丝 | 中小 | 各自单测 + REPL 冒烟 |

依赖：ST-0 → ST-1 → ST-2 → ST-3；ST-4 独立可并行。全链不改分层边界、不新增依赖。**先录夹具再动流式路径**是既有铁律（`provider/src/cassette.rs`，当前无已录夹具）——ST-0 排最前就是这条纪律的执行。

**明确不做**（本批）：后台任务注册表（A3）、wakeup 调度器（A4）、子代理（§1 已有规格）——都属于「扩容」，等「止血」四相验收后再排。

---

## 5. Copilot 线结论

`github/copilot-cli` 仓库仅含 README / install.sh / 容器 Dockerfile / 冒烟脚本，**零源码**——agent 内核（与 Copilot coding agent 同款 harness）闭源二进制分发，无可研读。同类问题在开源三仓（zaly/maki/codex）已有完整答案，不追闭源产物。

---

## 6. 落地记录（2026-11-20 第二批：非流式路径修复，全部已验证）

本轮只动**非流式路径**（流式路径 ST-1 仍受「先录夹具」铁律约束，未动）。clippy `-D clippy::all` 零 error，`cargo test --workspace` 全绿零回归。

### R2 · 错误分级（S2/S6 的地基）✅
- `runtime/conversation.rs`：新增 `ErrorKind`（Transport/Auth/Overflow/Model/Cancelled/Other，带 `label()`）；`RuntimeError` 增加 `kind` 字段与 `with_kind`/`cancelled`/`kind()`；`classify_stream_error` 是**唯一**的字符串匹配点（overflow 判据复用 `compact.rs::is_context_overflow_error`），全部冒烟测试钉死（7 条）。
- 全部回合内错误点改用分类构造：流错误 → 分类上抛；`!finished` → Cancelled/Transport；空回复 → Model；三处取消 → `RuntimeError::cancelled`。
- `cli/turn.rs`：`TurnOutcome` 增加 `error_class: Option<ErrorKind>`；`turn.rs:419` 的 `contains("cancelled")` 字符串嗅探**已删除**（改 `error.kind() == Cancelled`）。

### S2 · autopilot 传输类失败退避续跑 ✅
- `cli/autopilot.rs`：transport 类失败不再计入 blocked streak——先指数退避重试同一 nudge（30s→2m→8m，`transport_backoff` 可测），3 次耗尽才记 1 个 claim；auth/overflow/model 类照旧立即计。429 归入 transport 类（突发限流不该杀长任务）。

### R4 · 压缩默认值翻转（S3）✅
- `cli/main.rs`：窗口模式下 `tail_budget_tokens` 默认 `min(32k, window/8)`、`hard_trigger` 默认开；env 覆盖仍双向可用（`HEARTFLOW_COMPACT_TAIL_BUDGET=0` 可回旧 count-only，`HEARTFLOW_COMPACT_HARD_TRIGGER=0` 可关）。4 条单测钉死（含 200k 窗口 → 25k 的缩放档）。

### S6 · 行为层熔断 ✅
- 新模块 `runtime/src/stop.rs`：`StopPolicy` 双臂循环检测（同参哈希连续 3 次 + 滑窗 10 内重复 4 次；键 = 排序后 `name\0input` 的 SipHash，批次内乱序不影响键）。7 条单测。
- 挂点在 `run_turn_with_blocks` 工具授权前：触发 → Model 类错误（transcript 已一致），带「换思路」hint。另有**工具错误熔断**：连续 8 个全错工具批次判死（任一成功清零）。
- 端到端测试 `loop_detector_tests`：同参三轮 → Model 类错误；iteration-limit 测试改为变参调用（原测试每轮同参，恰好被熔断正确拦截——这也证明熔断真的在干活）。

### S4 · 热重载不再踢出 plan/spec 模式 ✅
- `cli/main.rs` REPL 分支：config 变更后不再清 `planning`/`speccing`，改为按当前标志**重放 policy 交换**（`set_runtime_mode_policy("plan"/"spec")`）——与 `/plan`/`/spec` 进入机制同源，模式与策略恢复一致。

### S5 · steer 终回合回收 ✅
- `run_turn_with_blocks` 的无工具收尾路径 drain steer mailbox → 推入 transcript 为 user 消息 + `Steered` 事件——下一回合（autopilot 续跑或用户输入）必然带上，用户输入零丢失。

### 未动（按纪律）
- **S1/S7/R1 流式路径**（idle timeout、步级重试、blocks 落 transcript）：ST-0 夹具未录，铁律不动。
- **ST-3 后半**（Blocked 恢复路径 `/jobs unblock`）：需要 mission 工具面扩展，下批。
- **StopPolicy 的 token 预算臂**：需要 `usage_tracker` 跨迭代口径，随 ST-2。
