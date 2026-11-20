# agents.dev.md — 外部 agent 项目借鉴清单（zaly / maki / codex）

> 对象（第三批）：`folke/zaly`（Hackable terminal coding agent，Bun/TS monorepo，MIT，本地克隆 `gitmine/zaly`）、`tontinton/maki`（Rust workspace，本地克隆 `gitmine/maki`）、`openai/codex`（Rust，`codex-rs` 约 100 crate，本地浅克隆 `gitmine/codex`）。三仓均为 2026-11-20 克隆的浅拷贝（`--depth 1`），行号对应当日 HEAD。
> 受益方：heartflow（9 crate，分层 api → provider → runtime → cli）。
> 方法：读源码，**每条结论指到 `文件:行号`**；无法证实的一律标注「待验证」。判据沿用 `best.dev.md`：**值得抄的不是功能，而是「把模糊的工程问题变成可回归的数值」的机制**。
> 与前两批的关系：jcode 批（best.dev.md）管工程纪律棘轮；本批管 **agent 循环本体**——凭据自动发现、停机保险丝、后台任务、压缩、取消传播、子代理。用户点名的「GitHub Copilot」无开源 agent 内核可研（CLI 为闭源发行物），其 Rust 同类以 codex 为准。

---

## 0. 导读：三仓定位与分档

| 仓 | 语言 | 体量 | 一句话定位 | 本批价值 |
|---|---|---|---|---|
| zaly | Bun/TS | 7 公共包 | 可 hack 的终端 agent，事件驱动 | **凭据链 + agent 循环 + 压缩**的完整范本，注释即设计文档 |
| maki | Rust | 14 crate | 原生 Rust TUI agent，60 FPS 启动 | **编译期清单纪律 + 取消传播 + 压缩护栏**的 Rust 落法 |
| codex | Rust | ~100 crate | OpenAI 官方 agent，Responses API 中心 | **provider 配置契约 + 压缩工程化 + 子代理治理** |

分档沿用 best.dev.md 惯例：**A = 值得落地**，**B = 已有等价物，不再投入**，**C = 不抄，记录理由**。

---

## A 档：值得落地

### A1 · 环境变量自动发现凭据（零配置启动）★ 用户点名、最高优先

**三仓范本**：

- **zaly**：凭据解析是**五级瀑布**（`packages/ai/src/auth/manager.ts:96-134`）：① model 显式 apiKey → ② 本地 store 的 api-key → ③ store 的 OAuth（带 60s 提前刷新 `REFRESH_LEEWAY_MS`，`:50`）→ ④ provider 声明的 apiKey → ⑤ **provider 声明的 env 变量名单逐个试**（`#fromEnv`，`:242-249`）。第 ⑤ 级的关键是名单是**复数**：`provider.env: string[]`（`models/catalog.ts:299`），数据来自 models.dev 目录，provider 天生携带「我认这几个变量」的知识，新增 provider 不用改解析器。
- **zaly 附加巧思**：secret 字符串支持 `${VAR}` / `$VAR` 插值（缺失即整体判无效并逐名列出，不静默吞，`manager.ts:288-306`）与 `!command` bash 求值（带缓存 + 同命令 Promise 串行化防并发重复执行，`:307-324`）。凭据求值是**惰性、按需、可缓存**的，不是启动时一次性抓全。
- **maki**：每个 provider 的 env 变量名是 `ProviderSpec.api_key_env: &'static str`（`maki-providers/src/spec.rs:53`），且 spec 结构**故意无 `Default`、无 `#[non_exhaustive]`、行内禁 `..`**——加字段即全部行编译失败，逼每行回答（`:39-48` 注释原文：*adding a field here breaks every row until someone answers for each one*）。这是「清单即类型」的编译期纪律。
- **codex**：`ModelProviderInfo.env_key: Option<String>` + `env_key_instructions: Option<String>`（`model-provider-info/src/lib.rs:171-175`）+ `env_http_headers: HashMap<header, ENV_VAR>`（`:210`，headers 也可来自 env）；命令式凭据 `auth.credential_export`（`:244-260`，**不经过 shell** 直接 exec）。错误信息可以直接引用 env_key_instructions 指导用户设变量。

**heartflow 现状（代码实证）**：

- builtin 只有 deepseek / anthropic 两家，各带**单个** `api_key_env`（`crates/provider/src/config.rs:108-124`）。
- 无任何显式配置时 `resolve_spec` 直接落 `Env { model: DEFAULT_MODEL }`（`config.rs:532-541`）——**此时只认 `ANTHROPIC_*` 三变量**（`api/client.rs:194-214`）。用户机器上装了 `OPENAI_API_KEY` / `ZAI_API_KEY` / `DEEPSEEK_API_KEY` 都不会被识别，必须手写 `config.toml`。这正是用户点名要补的缺口。
- §12（roadmap.md）已定 `CredentialSource::Env { var }` 进 Profile 的设计，但 §12 只管「身份可见」，不管「发现」。

**设计（落点：`config.rs` 的 `resolve_spec` 之前加一层 discover，纯函数可测）**：

1. builtin 表的 `api_key_env: &'static str` 升格为 `api_key_envs: &'static [&'static str]`（对齐 zaly 复数语义；deepseek 仍可只带一个）。
2. `resolve_spec` 的「无显式配置」分支改为：按固定优先级遍历全部 builtin 的 env 名单，首个非空命中者构造 `Resolved` spec，`CredentialSource` 记录命中变量名。**扫描顺序是稳定的文档化契约**，不做「跨供应商择优」（沿 §12 明确不做清单）。
3. 多变量命中时的歧义：`HEARTFLOW_PROVIDER` 环境变量可点名（对齐既有 `HEARTFLOW_PERMISSION_MODE` 风格）；未点名时取名单顺序第一，启动身份行打印「auto-detected provider=X via $VAR」——与 §12 P-A 的身份可见合流。
4. env 名单是**发现**面，inline `api_key` 仍是**回退**面；`materialize` 的「env 名义优先于 inline 值」纪律不变（`config.rs:597-654`，guardrail 注释保留）。
5. doctor 增加「检测到 X 的凭据但 provider 配置为 Y」报告（§12 混线 401 事故的预防面）。
6. codex 的 `env_key_instructions` 值得抄：builtin 表加 `api_key_hint: &'static str`，凭据缺失错误直接带「export ZAI_API_KEY=... 获取地址」级指引，替换现在「environment variable ... is not set」的裸文案。

**明确不做**：不扫 keychain / 浏览器 / 其他工具 dotfiles / shell history（沿 `config.rs:598-604` 纪律；zaly 的 store 级凭据对应的是 OAuth 流，hf 无 OAuth 面，不引入）。不做凭据轮换守护。`!command` bash 求值**暂不做**——hf 的凭据来源守卫是二来源制（env 名 / inline 值），加命令求值就是第三来源，破坏 §12 的可审计性；若未来要做，须带 zaly 式缓存 + 串行化 + 空输出判无效三件套。

### A2 · StopPolicy：结构化停机保险丝（zaly）

**范本**：`packages/agent/src/stop.ts`。四根保险丝 + 一个 loop 检测器，全部是**可配置阈值、可关闭（设 `Infinity`）**的数值：

| 保险丝 | 默认 | 触发条件 | stop.ts 行号 |
|---|---|---|---|
| `maxSteps` | 50 | 每 `run()` 的 provider 往返次数 | `:143-145` |
| loop 检测（连续臂） | 3 | 同 `name + JSON(params)` 哈希连续 N 次 | `:166-179` |
| loop 检测（窗口臂） | 窗口 10 / 重复 4 | 窗口内同哈希重复 N 次（抓 `A B A B` 交替） | `:181-193` |
| `maxToolErrors` | 可选 | 连续失败工具调用数，成功一次即清零 | `:149-154` |
| `tokenBudget` | 可选 | 全 run 累计 input+output | `:155-160` |

**设计巧思**：① 优先级排序有语义——loop-detected 压过 max-tool-errors，因为「紧循环里打转」比「错误多」更紧急（`:138-141` 注释）；② 计数器与循环本体分离（policy 独立类，事件流喂数据，`detect()` 一次问询），心脏和刹车不缠在一起；③ token 总量跨 reset 保持（计费显示），步数计数 per-run 重置。

**heartflow 现状**：loop 模式有 64 回合软上限（autopilot `mark_blocked`），但**没有** token 预算、连续工具错误熔断、同参重复调用检测。§4 的停机矩阵可按此表补齐三根保险丝；loop 检测的哈希直接抄 `name\0params`（zaly 自己注明属性序差异致偶发漏报是**接受**的代价——换取零成本哈希，`stop.ts:24-27`）。

### A3 · 后台任务 + heartbeat：长任务不断流（zaly）

**范本**：`packages/agent/src/tasks.ts`（838 行）+ `agent.ts:114-126`。机制链：

1. 工具返回 `Streamable`（可轮询、可中止、可查增量 `hasNew`）→ 超过 grace 窗口（默认 10s，`tasks.ts:18`）不阻塞回合，登记为 task、返回**占位符结果**（带 `meta.task` 让模型可关联）。
2. `task_poll` / `task_stop` / `task_list` 三工具让模型自主管理后台任务；`poll` 推进游标只回增量。
3. **task-done 注入**：round 外完成的任务把结果包成 system 消息注入下一步（`agent.ts:114-116`）——模型无需轮询也能收到完成通知。
4. **heartbeat**：有任务在飞时周期性注入「还在跑什么」的 system note，带 `*new*` 标记提示值得 poll（`agent.ts:117-126`）——回合不会因为「模型等长任务」而饿死。
5. 依赖排队：`waitingFor` 字段让任务排在指定任务完成后（`tasks.ts:98-99`）。

**heartflow 现状**：§1 队列三件套已落地，但工具执行是同步 await 模型（concurrent-safe 白名单并行，写类串行，`tool_exec.rs:128`），**没有**「超时转后台 + 占位符 + 轮询」路径。落地形态建议：`runtime` 层加 `TaskRegistry`，`bash` 工具率先 Streamable 化（长命令天然受益），占位符与 heartbeat 注入沿 §4 的回合内 system 消息通道。这是 §1 监工-工人架构的**前置件**：工人 long-running 先于工人进程存在。

### A4 · Wakeup 调度器：hint 不蒸发（zaly）

**范本**：`agent.ts:396-430`。`scheduleWakeup(delayMs, hint)` 定时注入唤醒；任何其他唤醒源（用户消息 / task-done / heartbeat）使计时器失效时，**hint 折叠成带 `status="cancelled"` 标记的 system 消息**随下一次注入落地——意图不蒸发，模型看得见「原定 T 时叫你，提前醒了」。取消时机精确到状态迁移：只有 idle/paused → streaming 的**外部**唤醒才清计时器，回合内迁移（running-tools → streaming）不清，防止模型刚排的 wakeup 被误杀（`#setStatus`，`:894-910`，注释写得极清楚）。

**heartflow 落点**：loop 模式 autopilot 的续跑驱动器可吸收「hint 折叠」语义——被用户打断的 mission 续跑提示不应凭空消失；`/queue` 泳道轮转里「排队意图被提前消费」同理。

### A5 · 压缩：三压力级 + 背景先行 + 字节预算 + 确定性事实直拷

三仓合起来的可抄清单：

- **zaly**：`PRESSURE_LEVELS = [0.75, 0.85, 0.95]`（`agent.ts:30`）；0.9 起动**背景压缩**不阻塞前台回合（`#maybeCompact({backgroundOnly: true})`，`agent.ts:479-503`），0.95 才同步阻塞；同一会话同一尾消息 ID 只试一次压缩（`#compactionAttempt`，防失败后每步重试风暴）；overflow 恢复用 **epoch** 判定——`requestEpoch ≠ contextEpoch` 说明背景压缩已换了上下文，直接 continue 而不是再压一次（`:770-786`）。摘要 = 对话文本 + **文件使用表 + bash 使用表直拷**——确定性事实「复述比复制贵」，不花 output token 重算（`compactions.ts:98-108` 注释）。
- **maki**：保护尾按**字节**预算 64 KiB 而非条数——注释直说教训：条数制「留三个巨大的、扔掉三十个便宜的，一个巨型 MCP dump 塞在保护尾里每次重试都爆窗」（`compaction.rs:29-32`）；摘要输出独立预算 16k（`:36`，「在上下文压力最大的一次请求上预留几万用不掉的预算」）；保留区上限 50% 窗口（`:45`，llama.cpp `n_ctx 4096` 自毁循环案例）；**Overflow 时 Lua 层不可 skip**（只有 Auto/Manual 可被拦，`:60-71`）——provider 已拒收 prompt 时压缩是唯一出路；defer 一律只许一次（防层饿死 gauge）；summary 尝试 ≤3（`:51`）；压缩后续跑提示语明写「恢复 todo 列表、先存 memory」（`:26`）。
- **codex**：`COMPACT_USER_MESSAGE_MAX_TOKENS = 20k`（`compact.rs:69`）；压缩历史替换**先分配 item ID 再构造持久化对象**，保证 live 与持久一致（`compact.rs:77-90`）；pre/post compact hook 分离。

**heartflow 现状**：§4 compaction 七相已落地。对照缺口按性价比排序：① 保护尾改字节预算（maki 教训直接适用）；② 背景压缩先行（zaly 0.9/0.95 双阈）；③ 摘要附确定性使用表；④ 压缩尝试去重与 ≤N 上限。均可在现有七相内做，不动存储层。

### A6 · 取消传播：Drop 触发 + Weak 子链（maki）

**范本**：`maki-agent/src/cancel.rs`（610 行，测试完备）。四条纪律：

1. `CancelTrigger` **fire on Drop**——忘记触发也会清理（`:3`）。
2. `cancelled()` 在 `Notified` future 前后**双检标志**，关死「读完标志到注册等待」的 TOCTOU 窗口（`:69-80`）。
3. 父持**子 Weak 引用**自行传播，无后台传播任务；子被 drop 自然出链（`fire` 里 `std::mem::take` + `Weak::upgrade`，`:25-29`）。
4. `adopt`/`unlink` 在锁内先查标志，父已 fire 时直接内联 fire 子——链接与触发的竞争两面都关死（`:101-121`）。

**heartflow 落点**：P2 并发模型的 token 取消传播直接以此为蓝图。maki 的注释明说设计目标：*parent-to-child propagation needs no background task and a dropped child leaves nothing behind*——比 tokio `CancellationToken` 树（需要显式 drop guard）更适合 hf 的「单写者 World + 快照」形态。`PoisonError::into_inner` 恢复式锁语义与 maki `lock().unwrap_or_else(|e| e.into_inner())`（`cancel.rs:32`）同宗，风格一致。

### A7 · 子代理治理：深度封顶滤工具、转发只带文本（zaly + codex）

- **深度封顶 = 滤工具而非报错**：子代理达 `maxDepth` 时把 `subagent` 工具从继承工具表里**滤掉**——模型看不见工具，递归自然见底，零错误路径（`agent.ts:176` 注释原文：*The model just doesn't see the tool — no error path*）。heartflow §1 的 spawn 计数护栏可加这条：与其 spawn 时报错，不如深度达标者根本拿不到 spawn 工具。
- **Swarm 通信语义**：名字即 id（冲突自动 `-2` 后缀，`swarm.ts:133-141`）；`"user"` 保留名（`:116`）；子→父转发只在 **step-end 自然停**时发生，且只提纯文本（`assistantText` 滤掉 reasoning 与工具调用，`swarm.ts:234-245`）——不漏内部轨迹；消息 role 语义：父发给子用 `user`（对子而言父就是用户），其他一律 `system` 带 `<agent>` 元数据（`:166-173`）。
- **codex 的治理计数**：`AgentRegistry` 记 `total_count: AtomicUsize` 总量上限 + spawn depth（`registry.rs:24-40`、`:93-98`）+ nickname 去重集合。hf §1 已定 spawn 上限 64（pi 借鉴），codex 补的是「**总量**与**深度**分开计」。

### A8 · 会话事件上下文化：harness 状态进对话（zaly）

**范本**：README 特性表原文（`zaly/README.md:29-32`）：*zaly keeps the agent aware of session resumes, time, compaction, context pressure, model changes, completed tasks... Important harness state becomes context instead of remaining invisible.* 实现 = notify 队列：任意组件 `agent.notify(type, data)` 把 MetaPart 排队，下一步开始时**整队折成一条 system 消息**注入（`agent.ts:372-378`、`:529-534`）。

**heartflow 落点**：§4 已有 compaction reminder；缺的是**统一通道**——模型切换、凭据来源变化、上下文压力档位都应走同一条 notify 队列而非各自散落。这是 §12「身份可见」在对话内的投影：不只给用户看，也让模型知道自己的 harness 换了底座。

---

## B 档：已有等价物，不再投入

| 项目 | zaly/maki/codex 的做法 | heartflow 等价物 |
|---|---|---|
| 权限预设 | zaly strict/readonly/permissive/yolo 四档 + per-tool 规则（`permissions/presets.ts`） | read-only / workspace-write / full / plan / spec / loop 六档 + gate（`permissions.rs:21-44`）；`yolo` ≈ `full` |
| 会话持久化 | zaly JSONL 尾读 + 增量加载（README:15-19）；codex rollout crate | SQLite store；大历史恢复已是 hf 卖点 |
| 模型热切换 | zaly `/model` + reasoning effort 不重启 | `/model NAME` 已落地并写回 provider.toml |
| 工具结果截断 | maki `truncate_bytes`、codex `approx_token_count` | `utils/truncate.ts` 等价 + §4 masking |
| 重试 | 三仓各有指数退避 | `api/retry.rs`（含 RetryAfter） |
| MCP | codex rmcp-client、maki mcp crate | `mcp` crate + HTTP/stdio 双路 |

---

## C 档：不抄，记录理由

| 项目 | 内容 | 理由 |
|---|---|---|
| zaly | Bun/Node **双运行时**测试矩阵（AGENTS.md:9-11） | hf 无 JS 运行时分裂问题；测试矩阵成本不迁移 |
| zaly | TypeBox schema 生成管线（`packages/dev`） | hf 用 schemars 派生（`tools/lib.rs`），单语言无此需求 |
| maki | Lua 插件分层（`maki-lua`）+ ACP + Aperture 网关 + OTEL | hf 工具面走 MCP 已定；引入解释器违背「最少隐藏行为」；网关/遥测是产品面非机制 |
| maki | 60 FPS TUI 渲染纪律（`maki-ui`） | hf TUI 是 ratatui 惯用形态，不逐帧重绘 |
| codex | 强制 Responses API（`chat` wire 已删，`model-provider-info/src/lib.rs:140-144` 反序列化直接报错） | hf 必须三协议并存（anthropic/openai/openai-responses），删 chat 协议不可行 |
| codex | bazel 构建、app-server-daemon、cloud-tasks、voice、实时 WebSocket | 产品面；hf 无 daemon 定位 |
| codex | `experimental_bearer_token` inline 配置（`lib.rs:179`，文档自陈 discouraged） | hf inline key 已有 export 脱敏纪律，不加「 discourage 但存在」的双面字段 |

---

## 9. 长任务稳定运行：四层保险丝汇总（回应本批核心问题）

用户点名要研究「长期稳定运行、长任务、压缩、loop、子代理」。三仓的答案收敛为**同一形状**——zaly 最完整，maki 最严谨，codex 最工程化：

1. **预算层**：步数上限（zaly 50 / codex MaxTurns / hf loop 64）+ token 预算（zaly `tokenBudget`）——**数值化、可配置、可关**。
2. **行为层**：loop 检测（同参重复 + 窗口重复）、连续错误熔断（成功清零）——抓「模型打转」与「环境持续拒绝」两类失败。
3. **资源层**：上下文压力三级 + 背景压缩先行 + overflow epoch 恢复 + 保护尾字节预算——上下文不爆窗，压缩不自毁。
4. **活性层**：heartbeat + task-done 注入 + wakeup hint 不蒸发——长任务期间回合永不饿死，被取消的意图不凭空消失。

子代理在其上加两条治理：总量与深度分开计；深度封顶滤工具而非报错。

**落地顺序建议**（供 roadmap 批次裁决）：A1（环境变量发现，纯 `config.rs` 函数级改动）→ A2（停机保险丝，§4 补三根）→ A5①（保护尾字节预算，一行常量级）→ A6（取消传播，对接 P2）→ A3（后台任务，§1 前置件）→ A5②③④ / A4 / A8（按需）。

---

# 批次二（2026-11-20 晚）：goose / gemini-cli / aider

> 仓库存放 `gitmine/{goose,gemini-cli,aider}`（浅克隆，不入库）。本批聚焦上一批没覆盖的三类样本：Block 的 Rust 生产级 agent（goose）、Google 的 TS 大众级 agent（gemini-cli）、Python 生态最成熟的结对 agent（aider）。调研由两个子代理完成，证据均为 `文件:行号`。

## D 档（批次二）：值得采纳 / 记录方向

### D1 · goose（Block，Rust）——工程化最接近 hf 的同类

1. **Secret 双后端 + env 覆盖**（`goose/crates/goose/src/config/base.rs`，keyring 后端与 GOOSE_PROVIDER/GOOSE_API_KEY 环境变量覆盖 config.toml）：API key 优先 OS keychain，env 变量覆盖持久配置。→ hf 已有 `cli/storage.rs::lock_state` 凭据安全存储（§12 身份链），缺的是「keyring 失败自动降级明文文件 + 权限收紧」双后端与 env 覆盖通道。**记方向，随 A1 落地时一并评估**。
2. **错误信息化 + `ExitOnError`**：goose 把每类 provider 错误映射成人类可读文案（含下一步建议），CLI 层 `ExitOnError` 控制失败即退（CI 用）。→ hf 的 `RuntimeError.hint` + `ApiError::suggestion` 已等价；`ExitOnError` 适合 `hf prompt` 管道模式（失败码区分错误类，随 R2 的 ErrorKind 直接派生 exit code）。**记方向**。
3. **压缩双阈值**（`goose/crates/goose/src/context_mgmt/mod.rs:29,136-141`）：`usage_ratio > threshold`（默认 0.8）与绝对上限 `DEFAULT_AUTO_COMPACT_TOKEN_LIMIT = 225_000` 双闸。→ hf 的 D4.1 SoftTrigger 已有 HalfWindow 策略，等价；**不投入**。
4. **子代理独立会话 + `AbortOnDropHandle` + 信号量并发闸**（`goose/crates/goose/src/agents/`，lead-worker 模式）：lead/worker 各自完整 Session，worker 生命周期绑定 handle drop，spawn 并发用信号量封顶。→ 与 hf roadmap §1 子代理规格吻合，`AbortOnDropHandle` 是 tokio-util 现成件（已在依赖树）。**§1 落地时采纳**。
5. **调度器对 stale running 的清理**：scheduler 启动时把上次崩溃遗留的 running job 标 failed（防永不恢复）。→ hf 的 jobs.rs 有 wall_timeout，但**进程崩溃后的 stale 状态恢复**没有；长任务稳定性直接相关。**记方向（ST-5 候选）**。

### D2 · gemini-cli（Google，TS）——循环检测三层与压缩自校验

1. **循环检测三层**（`gemini-cli/packages/core/src/services/loopDetectionService.ts`）：
   - 工具调用哈希滑动窗口（与 hf 新落地的 `StopPolicy` 双臂同构，gemini 阈值 5 连/40% 窗口占比，hf 取 3 连/10 内 4 次——更激进）；
   - **内容诵经检测**（`ContentChantingDetector`）：文本流 50 字符块哈希，检测模型复读同一句——hf 的 StopPolicy 只盯工具调用，**纯文本复读是盲区**。→ **记方向：StopPolicy 增第三臂（文本块哈希），小改动**。
   - **LLM 循环裁判**（`LoopDetectionService` 第三层）：前两层都过但行为可疑时，用一个小模型判断「是否陷入循环」。→ hf 无多模型基建，**不投入**（成本与依赖不符）。
2. **压缩自校验**（`gemini-cli/packages/core/src/services/chatCompressionService.ts`）：压缩后做一次自校验轮（总结 → 自评「是否保住了任务状态」），失败则重压。→ hf 的 D4.5 Model 摘要已有 fallback-to-deterministic，但**没有自校验**。记方向（压缩质量线，ST-2 后评估）。
3. **重试 ±30% 抖动**（`gemini-cli/packages/core/src/utils/retry.ts`）：指数退避乘 `0.7~1.3` 随机抖动，防多客户端同步重试风暴。→ hf 的 `send_with_retry` 是固定 schedule，**ST-2 步级重试落地时直接带上**（一行随机数）。
4. **429 → 备用模型切换**：主模型限流时切到 fallback provider/model 清单。→ hf 单 provider 选择；随 §12 身份链 + A1 后有 provider 池，可做 fallback 链。**记方向（A7 候选）**。
5. **写工具前 checkpoint**：checkpointable 工具执行前强制建 checkpoint，失败可整体回滚。→ hf `checkpoint.rs` 已有快照基建；对照执行面是否所有写类工具都前置 checkpoint 待审计。**记方向（合规面）**。

### D3 · aider（Python）——编辑反馈梯子与提交纪律

1. **repo map**（`aider/repomap.py`）：tree-sitter 建符号图 + PageRank 排序 + token 预算裁剪，让模型「看全仓库而不读全仓库」。→ hf 是终端 agent，长任务跨文件工作时缺全局视野；mdbook/GDScript 站不相关。**记方向（大件，单独批次）**。
2. **编辑反馈梯子**（`aider/coders/base_coder.py`）：模型产出编辑格式解析失败 → `reflected_message` 把**具体解析错误**喂回去重试（有限次数），逐级降格式（diff → whole）。→ hf 的 apply_patch/validate_tool_inputs 已把错误回灌（`validate_tool_inputs` 产 error tool_result），等价于第一级；**多级降格式不适用**（hf 工具协议单一）。**不投入**。
3. **自动提交纪律**：每次成功编辑后 `git commit`（`--no-auto-commits` 可关），长任务天然获得 undo 点。→ hf 长任务在用户仓库工作时无此纪律，一次失手难以回退。**记方向（需谨慎：在用户仓库自动 commit 有侵入性，需显式开关 + 仅 hf 自改文件范围）**。
4. **后台摘要线程**：聊天历史超限自动后台起摘要线程，不阻塞当前回合。→ 与 §15 A5（背景压缩）吻合，佐证方向正确。

## 批次二结论

- **直接排期**：retry 抖动（ST-2 顺手）、ExitOnError 管道退出码（R2 ErrorKind 顺手派生）、scheduler stale-running 清理（ST-5 候选，崩溃恢复）。
- **下一批评估**：StopPolicy 文本诵经第三臂、keyring 双后端（随 A1）、checkpoint 写前覆盖审计。
- **大件只记录**：repo map、fallback 模型链、压缩自校验、子代理（§1 已有规格，goose 的 AbortOnDropHandle + 信号量补充进 §1 素材）。
- **不投入**：LLM 循环裁判、多级编辑格式梯子、压缩双阈值（已有等价）。
