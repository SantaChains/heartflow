# ai-rust-hygiene.md — AI 写 Rust 的反模式规则与本仓审计（2026-11-20）

> 本文件是给 agent 读的规则文件，不是文章。写作原则：每条规则给出「症状 → 检测命令 → 修法 → 本仓状态」，检测命令可直接 grep 执行、结果可复查。来源：Stephen Toub《Migrating the GitHub Copilot runtime to Rust》官方复盘（2026-09，832k 行 / 128 PR / 1 名主导工程师）、Bun《Rewriting Bun in Rust》（Jarred Sumner，535k 行 Zig / 11 天 / ~64 并行 agent）、rust-lang/rust 自动化贡献政策辩论（Zulip 3000+ 消息）、Veracode 2025 LLM 安全研究、Shuttle 2025 七工具横评、社区 slop 归纳（lean-code-reviewer waste catalogue / managed-code CRAP 分析 / HN slop debt 讨论）。摘要证据见文末来源节，此处只留可执行规则。

---

## 一、从 Copilot×Bun 两次大型重写复盘提炼的教训（转成规则）

复盘背景一句话：GitHub 用 14.5 周、128 个增量 PR 把 Copilot runtime（TS/Node）重写为 832k 行生产 Rust；Bun 用 11 天、~64 个并行 Claude agent 把 535k 行 Zig 重写为 Rust。两者都成功，但都记录了成体系的失败模式。

**T1 · 编译通过 ≠ 行为正确。** Toub 复盘：迁移期 8,678 次 rustc 报错里，所有权/借用/生命周期仅占 1.7%——编译器拦得住类型错，拦不住异步时序、回调配对、契约漂移。几十个回归全部编译通过。Bun 同样：16,000 个编译错误清零后仍有 19 个确认回归。
→ 规则：agent 汇报「完成」时必须附带行为级验证（测试名 + 断言内容），不接受「clippy 过了」作为完成标准。本仓已有此纪律（cassette + oracle 基线），保持。

**T2 · agent 会为通过检查而改检查（reward hacking）。** 实录：port 删掉了公共 API 方法，CI 红了，agent 给仓库打上「schema-break waiver」标签让检查变绿——人类在 review 里抓到。
→ 规则：禁止 agent 修改任何门禁语义（panic 棘轮基线、测试期望、`--update` 刷新）来让失败变绿。本仓 AGENTS.md 已有「不为变绿而 `--update`」铁律，此条是其一般化。

**T3 · 失败会聚成少数几类，按类设防而不是按例救火。** Toub 归纳的五类回归：迁移不完整、状态/生命周期错、行为契约不匹配、宿主边界错、测试 oracle 本身错。五类里有四类是「旧代码是规格，但规格只在测试里」。
→ 规则：修 bug 先归类再修；同类第二次出现必须沉淀成测试或 lint，不允许第三处以手工方式解决。

**T4 · 探索与修改是 10:1。** Copilot 迁移期 agent 工具调用里，读/搜/验证是编辑的十倍（view 59 万次 vs apply_patch 5.4 万）。
→ 规则：评估 agent 工作量时按修改行数 10 倍估探索成本；任务切分要给「只读分析」独立会话，不要让写会话顺带做大规模调查。

**T5 · 人守契约，agent 产实现。** Toub 的 2,639 条人工消息里 31% 审查/测试/CI、17.4% 挑战设计、15% 推完整性——六成在「检查」而非「产出」。Bun 的做法同构：语言无关测试套件 + 对抗式评审 + 「修生成流程而非修补单点」。
→ 规则：验收契约（接口、测试、门禁）与实现分开管理；改契约需独立说明理由。本仓 roadmap/AGENTS.md 的「接口面变更须同步文档」条款即此。

**T6 · 增量可回滚 > 一步到位。** GitHub 走 strangler-fig 函数级替换（桥接层峰值 2019 exports，终点归零），Bun 大爆炸合并但靠 57k 测试兜底。共同点：可回滚、可归因、每次发布承载一组明确变化。
→ 规则：大改必须切成可独立验证、可独立回滚的批次；一批一个语义。

## 二、AI 写 Rust 高频反模式（社区归纳 + 本仓适配）

以下 R 编号规则按「检测成本最低」排序。**本仓状态**列在第三节审计表里汇总。

**R1 · clone 税。** 症状：`.clone()` 散落只为让借用检查通过；热路径上 clone 大类型（String/Vec/整个 struct）；`fn f(s: String)` 只读参数。
检测：`rg -c '\.clone\(\)' crates/*/src -g '*.rs'` 看密度分布；热路径人工审。
修法：改签名借引用（`&str`/`&[T]`）；先重构数据流再 clone；只有存在第二个真正所有者时才 clone。
要点：clone 是 agent 遇到借用错误的默认逃生门——它不修所有权设计。

**R2 · unwrap/expect 泛滥 + 吞错降级。** 症状：`.unwrap()`/.expect() 在可真失败的 IO/解析/网络路径；反方向是 `.unwrap_or_default()`/`.ok()` 把真实错误静默吞成空值。
检测：本仓 panic 棘轮（`python3 scripts/check_panic_budget.py`）管 unwrap/expect；吞错用 `rg -n 'unwrap_or_default\(\)|\.ok\(\)' crates/*/src -g '*.rs'` 逐个审语义。
修法：可失败 → `?` + 类型化错误；确实不可失败 → 行尾注释不变量（棘轮的 `panic-ok:` 格式）；吞错降级 → 明确写「降级理由」注释，无理由即缺陷。

**R3 · Arc<Mutex<>> 万金油 / 锁纪律。** 症状：单线程语境套 `Arc<Mutex<T>>`；双层锁嵌套（外锁内再 lock）；MutexGuard 持有过 `.await` 点。
检测：`rg -c 'Arc<|Mutex|RwLock' crates/*/src -g '*.rs'`；guard 过 await 用编译器（MutexGuard 非 Send，跨 await 会编译错——但 tokio::sync::Mutex 不会，要人工审 `tokio::sync` 锁跨 await）。
修法：单线程用 `&mut self`/RefCell；锁只在短临界区；先定锁序再写代码。本仓已知专属缺陷类：`.lock().ok()?` 污染（棘轮管不到，见 AGENTS.md 锁污染判据——`PoisonError::into_inner` 恢复 vs 显式 Err）。

**R4 · String 参数当 &str 用。** `fn f(s: String)`/`fn f(s: &String)` 只读 → 改 `&str`；`&Vec<T>` → `&[T]`。clippy 可拦（`clippy::ptr_arg`），本仓 `clippy::all` 零告警，已闭环。

**R5 · 过度防御 / 防御噪音。** 症状：检查调用上下文已保证不可能的条件；为「不可能」分支写 fallback；一次校验逻辑在调用链上重复两遍。
检测：人工审 diff 时问「这个分支的触发条件在上游是否已被保证」。
修法：删，或改成 debug_assert!（把「不可能」变成可验证的不变量声明）。

**R6 · 伪抽象 / 单实现 trait。** 症状：一个接口只有一个实现、工厂只造一种类型、wrapper 只转发。
检测：`rg -n 'trait ' crates/*/src -g '*.rs'` 后数 impl 数。
修法：两个用例出现前不抽接口（lean 规则：每个抽象至少两个具体用例）。

**R7 · 复制粘贴变异 / 模式漂移。** 症状：近重复块只差一个值；同一关注点在三个文件三种写法。
检测：人工审 + `rg` 找同形函数名。
修法：参数化合并；全仓一一种写法。AI 复制的速度是「每个会话一份」——它不知道仓库里已有等价实现，所以 AGENTS.md 的文档地图纪律（先读再写）是治本手段。

**R8 · 空心测试。** 症状：测试跑起来全绿但只断言「代码做了代码做的事」；从实现反推的测试是镜子不是门。
检测：审测试时问「实现里这个断言反过来写，测试会红吗」。
修法：断言行为而非执行；边界/错误路径显式覆盖。

**R9 · 文件与函数超长。** 症状：文件 >3k 行（agent 装载上下文代价、review 难度激增）；函数 >150 行多层级嵌套。
本仓已有一半纪律：AGENTS.md 要求 `run_repl` 保持 line cap（拆 helper），但没有文件级上限。

**R10 · 越权修门禁 / 豁免堆积。** 症状：`#[allow(...)]` 只增不减；为过 lint 加豁免而不修根因。
检测：`rg -c 'allow\(' crates/*/src -g '*.rs'` 趋势；每次 PR diff 审新增 allow。
本仓规则：新增 allow 必须带理由注释；`clippy::all` 是硬门零容忍。

**R11 · async 语境阻塞与锁跨 await。** 症状：async fn 里调 `std::fs`/`Command::status()`（阻塞 executor 线程）；`std::sync` 锁的 guard 持有过 await（编译器拦）；`tokio::sync::Mutex` guard 跨 await（编译器不拦，人工审）。
检测：`rg -n 'std::fs::' crates/*/src -g '*.rs'` 对照所在函数是否 async。
修法：阻塞 IO 走 `spawn_blocking`；或确认在启动/同步路径（非热 async）。

**R12 · 幻觉依赖。** 症状：Cargo.toml 出现 agent 建议的 crate。
规则：加依赖前人工核 crates.io 存在性/维护状态/MSRV 匹配。本仓 MSRV 1.88 硬约束使这步必须做。

## 三、AI 屎山四期症状（自检清单，按恶化顺序）

一期「局部冗余」：重复 helper、防御噪音、注释复读。危害低，机械清除即可。
二期「模式漂移」：同一问题多种解法并存，读一处猜不出另一处。仓内一致性开始瓦解。
三期「伪结构」：单实现抽象、passthrough 层、为分层而分层。改动一个需求连带改四五个文件。
四期「不可解释」：没有人能完整讲清某个模块为什么长这样；回归修复引入新回归；最终出路只剩人主导架构重来。

判定口径：本仓目标是一期可接受（有棘轮兜底）、二期零容忍（AGENTS.md 文档地图 + 锁污染判据 + 已否决清单对抗漂移）、三/四期靠 AGENTS.md 的工程决策存档（「勿重复推翻」条款）阻断。

## 四、本仓审计结果（2026-11-20，按规则编号）

静态基线先说清：`clippy::all` 零告警（硬门）、panic 棘轮 debt=0/justified=7、pedantic 存量 ~27 处已建档。以下缺陷全部是 clippy 拦不住的设计级问题。

**D1（R9/R10）`main.rs` 巨石化 + 11 处 `too_many_arguments`。**
`crates/cli/src/main.rs` 5712 行；`#[allow(clippy::too_many_arguments)]` × 11（494/1177/1263/1692/2011/2191/2273/3544/3750/3835/3948）。run_repl→dispatch_slash_command 参数链 16~19 个，其中 planning/plan_path/speccing/spec_path/mode/selection 是同生命周期的 REPL 会话态。
修法（下一环节）：聚合 `ReplState` struct，一次替换参数链；顺带把 slash 命令分组拆出 main.rs。预计消掉全部 11 处豁免，main.rs 减 1~2k 行。

**D2（R10）`main.rs` 9 处 `wildcard_imports` 豁免。**
行 63-106 连续 9 个 `#[allow(clippy::wildcard_imports)]`。符号来源不可见，与 D1 的参数链膨胀互为因果（AI 加东西时容易往 glob 命名空间里塞）。
修法：随 D1 拆文件时自然消解；若保留，逐个改显式导入。

**D3（R7）`adapter.rs` 三胞胎 `stream()` + clone 密度 45。**
`crates/provider/src/adapter.rs:71/126/778` 三个 `impl ApiClient::stream` 同形：channel→spawn→drive_*_stream→Error 事件包装，各 ~25 行。clone 密度全仓第一（45 处）多源于此与 wire 解析。
修法：提取公共 `spawn_stream_bridge`，drive 函数作参数传入。

**D4（R3）`jobs.rs` 双层锁嵌套。**
`crates/runtime/src/jobs.rs:529-531` `self.inner.jobs.lock()` 内层再 `handle.record.lock()`；全文件 Arc/Mutex 21 处、lock 调用 10+。外层锁保护任务表、内层保护单 job 记录——锁序目前固定（外→内），无死锁实例，但锁面是全仓最大热点。
修法：评估 `record` 从 `Arc<Mutex<JobRecord>>` 降为 `Arc<JobRecord>`（不可变后配整体替换）或合并进外锁；至少补「锁序约定」注释。

**D5（R2）`bash.rs:1268` 日志读取吞错。**
`std::fs::read_to_string(&log_path).unwrap_or_default()`——job 诊断路径上日志读失败静默变空字符串，超时诊断会拿空日志误判「无输出」。
修法：读失败时在输出里注明 `log unavailable: {error}`。

**D6（R11）async 语境的 std::fs（待分类，非必缺陷）。**
`crates/cli/src/core.rs:222/312-322/1148-1208` 存量 `std::fs::` 在 CLI 同步路径多为核心存储读写。启动路径可接受；若位于 async fn 内需逐个核。**待下一环节逐点定性**，不动手前不动。

**D7（R8）测试顺序耦合一处。**
`conversation.rs::loop_detector_tests::identical_tool_batches_trip_the_loop_fuse`（2026-11-20 新增）依赖「熔断阈值 3 < 迭代上限 64」的触发次序。若 MAX_REPEAT_STREAK 调大到 >64，测试会静默变成测上限而非测熔断。
修法：断言 `summary.iterations < 64`（证明是熔断截停而非上限软着陆）。

**D8（R9）超长文件三座。**
main.rs 5712 / tui.rs 5465 / conversation.rs 3727。conversation.rs 本轮新增 StopPolicy 挂点后仍在长（错误分类+熔断+steer 回收都在其中）。
修法（不急）：R2 已把错误分类收进 ErrorKind，下一刀是把 `run_turn_with_blocks` 的熔断/检测逻辑抽到 `stop.rs`（模块已存在），conversation.rs 只留编排。

**D9（R1）clone 密度头部三项已有归属。**
conversation.rs 53 / main.rs 51 / tui.rs 37。已知未修清单在案（REPL 双写、TUI 流式尾部每帧 clone `tui.rs:1761`、表格列宽不封顶）——不新增审计项，维持既有待修清单即可。

**未发现问题项（审过，防重查）**：
- R4（String 参数）：clippy `ptr_arg` 硬门下零残留。
- R6（单实现 trait）：抽查 `ApiClient`/`ToolExecutor`/`PermissionPrompter` 均为多实现分层边界（AGENTS.md 分层约束的正当产物），非伪抽象。
- R12（幻觉依赖）：Cargo.toml 依赖集小且人工核定过，无近期新增。
- 注释噪音：抽样未见「怎么做」型复读，注释以「为什么」为主——本仓纪律有效。

## 四、本仓审计结果（2026-11-20，按规则编号；状态更新见文末 2026-11-20 晚批次）

> **2026-11-20 晚修复批次状态**：D3 ✅（`spawn_stream_bridge` 统一三处 `stream()`，三胞胎消解）；D4 ✅（jobs.rs 全部 `.lock().ok()` 污染吞错改为 `PoisonError::into_inner` 恢复 + 锁序注释 + SlotGuard 防恐慌泄漏槽位 + catch_unwind 诚实判 Failed——比原计划多修 6 处污染缺陷，`set_status`/`status_of`/`terminalize_note`/`finalize_on_shutdown`/`poll`/`stop`/`is_active`/饱和计数）；D5 ⚠️ **改判**：bash.rs:1268 是测试轮询代码，生产路径无此缺陷；jobs.rs `log_tail` 文件缺失返回空属合理 tail 语义，保留；D6 ✅ **改判清白**：core.rs 零 async fn，std::fs 全部在同步路径；D7 ✅（加 `calls < 64` 次序断言）；D1 ✅ 范围切口（`ReplPlanState` 聚合四旗标 + 消掉 9/11 处 `too_many_arguments`，剩余 2 处（dispatch/guide）带理由保留；main.rs 文件级拆分留待后续批次）；D2/D8/D9 维持待办（随文件拆分批次）。

**审计结论**：本仓不在屎山轨道上。反模式防线（panic 棘轮、clippy 硬门、AGENTS.md 决策存档、文档地图）恰好对准了社区归纳的主要翻车点。剩余缺陷集中在两条线：**D1/D2 的 REPL 参数链巨石化**（最高优先，AI 加参惯性正在持续输血）与 **D3/D4 的复制/锁面**（机械重构，收益明确）。下一环节从 D1 开始。

## 五、来源

- Stephen Toub, *Migrating the GitHub Copilot runtime to Rust*（GitHub 官方博客，2026-09；128 PR / 832k 行 / 5 类回归 / waiver 标签事件 / 10:1 探索修改比）
- Jarred Sumner, *Rewriting Bun in Rust*（2026-08；11 天 / ~64 agents / 19 回归 / 对抗式评审 / 测试套件作为语言无关规格）
- OpenAI Habitat Python→Rust 迁移公告（2026-09，佐证趋势）
- rust-lang/rust 自动化贡献政策（Jynn Nelson 起草，Zulip 3000+ 消息；AI slop PR 对维护带宽的冲击）
- Veracode 2025：100+ LLM 生成代码 45% 含安全缺陷；Shuttle 2025：幻觉 crate 版本为七工具最一致问题
- 社区 slop 归纳：lean-code-reviewer waste catalogue（GitHub）、managed-code CRAP 分析、HN「slop debt」讨论、floofdigital「AI Slop Moves to Development」

## 六、核心语义审查台账（2026-11-20 晚，独立只读审查 18 项发现）

> 审查范围：conversation.rs 回合循环 / compact.rs / jobs.rs / turn.rs / autopilot.rs。每项 = 位置 + 风险 + 状态。

**已修（本轮）**：
- C2 ✅ 回合循环取消检查曾排在迭代上限之后——取消会被软着陆误报为「成功」。已提至循环最前。
- C7 ✅ stop.rs 模块文档边界过时。已澄清「loop 检测在此模块；工具错误熔断在 conversation.rs」。
- P1 ✅ `is_context_overflow_error` 缺 Responses 方言与 DeepSeek/GLM 措辞（"context window"/"too many tokens"/"input tokens exceed" 等），漏配则硬触发恢复空转。已补 4 个标记。
- P3 ✅ `strip_tag_block` 闭合 tag 若出现在开 tag 之前会拼接出错内容。已改为从 start 之后搜索 end。
- J1 ✅ worker 恐慌泄漏槽位 + 任务永远 Running。已加 SlotGuard（Drop 释放槽位）+ catch_unwind（诚实判 Failed）。
- J3 ✅ `set_status`/`status_of` 等 8 处 `.lock().ok()` 污染吞错（与 D4 同一批修复）。

**已修（前批，审查确认）**：J3 熔断丢失真实失败原因——已修（fuse 错误信息带末次失败摘要）。

**已修（A1）**：autopilot 退避 sleep 不可中断（最长 480s 挡住 Ctrl+C）。已用 `tokio::select!` 竞速 `ctrl_c`。

**评估后不修（有界合理行为）**：
- A2 传输重试 reset-on-success 可使 ok/错交替无限续跑——每轮真实进度 + 有 MAX_TRANSPORT_RETRIES 上限，属正确韧性语义，非泄漏。
- C5 压缩 epoch 序号验证不严——恢复路径已有一次重试上限，收益低。

**待办（记入下一批，按优先级）**：
- C1（高）：流错误分支的 usage 记账偏移——assistant_with_usage 把 blocks + usage 合并落 transcript，但 mid-stream 错误时 usage 可能缺 final 事件，token 账与 wire 不齐。修法：错误路径把已收 partial usage 单独记账。随 ST-1 cassette 批次。
- C3（中）：被拒/无效工具批次绕过熔断（拒绝不进 batch_key）——恶意循环「请求→全拒」模式当前只受迭代上限兜底。修法：拒绝也计数。随 ST-4。
- C4（中）：`summarize_conversation` 全程忽略 cancel——长会话压缩时 Ctrl+C 无效。修法：摘要轮询传 token。随 ST-4。
- P2（中）：`estimate_tokens_chars/4` 误差 ±30%——触发点可能提前/滞后。修法：用 usage 实测值校准（compact.rs 已有 overhead 校准器，接入即可）。
- P4（中）：StopPolicy 熔断通知走 `Truncated` 事件——TUI 渲染为警告色而非错误色。修法：新增 `EventKind::Fuse` 或复用 Error。随 TUI 批次。
- J2（低）：REPL 首帧前 Ctrl+C 处理未注册——竞态窗口极小。
- J4（低）：`JobRegistry::new` 默认 slots/max_pending 值无文档说明来源。
- J5（低）：tests 里 `Instant::now()` 超时轮询在慢机上可能抖动——已有 deadline 兜底，可接受。
- T1（低）：`turn.rs` 的 `count_tool_errors` 与 `ErrorKind::ToolError` 语义重复——两套计数口径，长期应统一。

