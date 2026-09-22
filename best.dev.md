# best.dev.md — 外部项目借鉴清单（jcode / rig / ADK-Rust / Pica / Morphz）

> 对象（第一批）：`archive/jcode`（v0.86.0，101 个 workspace crate，Rust 终端 AI agent，单 server 多 client 架构）。
> 受益方：heartflow（8 crate，35,139 行生产代码 / 60 个文件，无 daemon，SQLite 存储，REPL + 可选 TUI）。
> 方法：读源码与设计文档，**每条结论都指到 `文件:行号`**；无法证实的一律标注为「待验证前提」。
> 诚实边界：jcode 体量约为本项目的 3–5 倍，且自带 server/daemon/swarm/遥测产品面。**它的多数模块对本项目不适用**——本文只收窄到「机制可移植」的部分，不搬运产品面。
> 对象（第二批）：`rig.rs`（= `0xPlaygrounds/rig`）／`zavora-ai/adk-rust`／`picahq/pica`／`morphz-ai/morphz`。同一判据，见「第二批来源」一节的 **D/E/F/G** 档。

---

## 0. 导读：两批来源、七档分档

**第一批 = jcode**（下节 A/B/C 三档）。**第二批 = rig / ADK-Rust / Pica / Morphz**（文末「第二批来源」D/E/F/G 四档）。两批**独立分档**，因为它们的来源与判据不同，混在一起会看不清哪些结论来自哪个项目。

| 档 | 来源 | 含义 | 处置 |
|---|---|---|---|
| **A** | jcode | 机制可移植、与 heartflow 现有约束兼容 | 建议进 `list.md`，按顺序落地 |
| **B** | jcode | jcode 有，但 heartflow **已有等价物** | 明确不再投入（防重复劳动） |
| **C** | jcode | 依赖 jcode 的 server/daemon/云/体量，**不该抄** | 记录理由，避免日后反复讨论 |
| **D** | 第二批 | 真缺、且成本可控 | 建议按文末顺序落地 |
| **E** | 第二批 | heartflow **已有等价物** | 明确不再投入 |
| **F** | 第二批 | 规模/范式不匹配，**不该抄** | 记录理由 |
| **G** | 第二批 | 工程纪律（许可、依赖审计） | 与代码解耦，可独立决定 |

一条贯穿全文的判据：**真正值得抄的不是功能，而是「把一个模糊的工程问题变成可回归的数值」的方法**。jcode 几乎所有高价值机制都遵循同一形状——*先定义度量，再设棘轮，最后才动代码*。第二批沿用同一判据：**先核实本项目现状，再判断缺什么**（本轮据此推翻了我自己的一条先入之见，见 D 档前的「先纠正一条先入之见」）。

---

## A 档：值得落地

### A1 · 架构预算棘轮（budget ratchet）★ 最高性价比

**机制.** 把架构债编码成一个 JSON 基线 + 一个纯标准库的检查脚本，**只禁止恶化，不要求清零**：

| jcode 脚本 | 度量对象 | 基线文件 |
|---|---|---|
| `scripts/check_code_size_budget.py` | 生产 `.rs` 超过阈值（默认 **1200 行**）的文件及其行数 | `scripts/code_size_budget.json` |
| `scripts/check_panic_budget.py` | 生产行中的 `.unwrap(` / `.expect(` / `panic!` / `todo!` | `scripts/panic_budget.json` |
| `scripts/check_swallowed_error_budget.py` | `let _ =` / `.ok()` / `.unwrap_or_default()` | `scripts/swallowed_error_budget.json` |
| `scripts/check_wildcard_reexport_budget.py` | `pub use x::*` | `scripts/wildcard_reexport_budget.json` |
| `scripts/check_warning_budget.sh` | 编译告警数 | `scripts/warning_budget.txt` |

三条策略（`check_code_size_budget.py:7-13`）：
1. 已登记的超标文件**不得增长**；
2. **不允许新增**超标文件；
3. 缩小或消失则打印改进提示，由人决定何时 `--update` 刷新基线。

两个值得学的细节：
- **内联测试模块要被剔除**。`check_panic_budget.py:79-105` 用括号计数扫描跳过 `#[cfg(test)] mod tests {…}`，作者自陈「这是棘轮，不是解析器」——诚实的近似，够用即可。
- **棘轮比大扫除可行**。存量债不可能一次清零，但「不许再多一处」今天就能生效。heartflow 实测：`crates/*/src/**/*.rs` 的 naive 全量命中 **512** 处（含内联测试），剔除 `#[cfg(test)]` 与注释/字符串后的**生产真债只有 10 处**——两个数字差 51 倍。这本身就是「先划清口径，再谈基线」的论据。

**heartflow 现状（本次实测）.**

| 指标 | 实测值 | 说明 |
|---|---|---|
| 生产 `.rs` 文件数 / 总行数 | 60 个 / **35,139 行** | `crates/*/src/*.rs` |
| > 2000 行的文件 | **3** 个 | `cli/main.rs` 3115、`cli/tui.rs` 3009、`runtime/conversation.rs` 2299 |
| 1201–2000 行 | 4 个 | `file_ops.rs` 1934、`provider/adapter.rs` 1310、`tools/web.rs` 1280、`cli/mascot.rs` 1217 |
| 801–1200 行 | 8 个 | 含 `runtime/bash.rs` 1167、`runtime/prompt.rs` 1028 |
| panic 形态行命中（naive grep） | **512** 处 | 含内联测试；直接拿它当基线会把测试误判成债 |
| panic 形态行命中（脚本口径，已剔除测试/注释/字符串） | **10** 处 = 2 债 + 8 合理 hack | 已建基线 `scripts/panic_budget.json`，门禁已验证生效 |
| `pub use x::*` | **0** | 已干净，该预算**不需要**建立 |

**为什么对本项目特别值.** `list.md` 已经在做「按事实逐步完成」，但**进度不可回归**——今天削掉的行数，下个提交就可能长回来。棘轮把 `list.md` 的定性进度变成可执行的 gate，且与正在进行的 cli 拆薄任务直接互补：拆薄期间最容易发生的就是「一边拆、一边把新逻辑写回 `main.rs`」。

**代价.** 一个脚本约 170 行 Python（可直接改写自 jcode 脚本），无新依赖，不进 CI 也不阻塞任何人——只在提交前手动跑。

**已落地（本轮实测）.** `scripts/check_panic_budget.py` 已按 jcode 的三模式骨架改写落地（`--list` / `--update` / 默认门禁），并补了一个 jcode 没有的**第三类 `justified`**——因为「清零」会逼着人把合理的 panic 改写成更糟的东西：

| 类 | 判据 | 门禁行为 |
|---|---|---|
| `test` | 位于 `#[cfg(test)]` 项内，或在 `*_test.rs` / `tests/` 内 | 不计数 |
| `justified` | 命中行带 `// panic-ok: <理由>`（理由 ≥ 8 字符，否则视为未标注） | 计数、打印，**允许增长** |
| `debt` | 其余 | **硬门禁，只许变小** |

区分 justified / debt 的唯一问题：**有没有「同样清晰且局部」的非 panic 写法**。

- 属 `justified`：`Regex::new(<字面量>)`（Rust 没有不可能失败的 regex 构造器，panic 是这个 API 的固有代价）；`build_http()` 里的 `ClientBuilder::build()`（只配了编译期常量，且返回 `Client` 而非 `Result`，改成 `Result` 会把错误穿透到所有调用点）。
- 属 `debt`：`path.parent().expect(…)` 写在**已经返回 `Result`** 的函数里——`ok_or_else(…)?` 同样长度，panic 直接消失。

实测基线：`debt=2`（均在 `cli/main.rs:398,414`，属后者）/ `justified=8`（`redact.rs` ×4、`retry.rs` ×2、`bash.rs` ×2）。已验证：干净树退出 0；注入第 3 处债 → 退出 1 并报 `debt total grew: 2 -> 3`；移除后恢复退出 0。

**可达性审计（「这些 panic 都是真的吗」）.** `justified` / `debt` 分类的是**可消除性**，不是**可达性**——两者独立，必须分开问。逐点核对守卫后：**10 处全部不可达**，即没有一处能由任何输入路径触发。

| 命中 | 守卫 / 不变量 | 可达 |
|---|---|---|
| `redact.rs:40,50,63,74` | 字面量 regex；四者各有 1:1 的测试触发其 `OnceLock` 初始化（`scrubs_url_userinfo…` / `scrubs_bearer…` / `scrubs_labeled…` / `scrubs_prefixed…`）——字面量写错则测试先红 | 否 |
| `retry.rs:23` | 只配 `connect_timeout` / `read_timeout` 两个常量；且 reqwest 根本没有不可能失败的构造器（`Client::new()` 内部同样 panic） | 否（仅剩 TLS 后端初始化失败这类平台级条件） |
| `retry.rs:147` | 全函数对 `last_error` 只赋 `Some`（`:117`、`:122`）；任何离开循环的路径（`:128` 计数 break、`:137` 超时 break、`:144` 循环结束）都已在之前赋过值——运行期恒为 `Some` | 否 |
| `bash.rs:481,483` | 在 `:468,:469` `Stdio::piped()` 之后 `take()`，且该 async 块只求值一次 | 否 |
| `main.rs:398` | `home_dir().join(".heartflow").join("config.toml").parent()` 对任意 `home_dir()` 输出恒为 `Some`（`home_dir()` 本身还兜底成 `.`） | 否 |
| `main.rs:414` | 两个调用点（`main.rs:400`、`doctor.rs:51`）传入的都是 `config_file_paths()` 产物，恒有 file_name | 否（当前调用图内） |

**结论.** 没有一处是「会炸的 bug」；`main.rs` 那 2 处之所以仍记为 `debt`，理由是**有同样清晰、局部、且更耐改的写法**（`ok_or_else(…)?`），不是因为它们真会崩。`backup_existing` 是 `pub(crate) fn(&Path)`，安全性由调用者承担——把 `：414` 改成 `let Some(name) = … else { return Ok(()) }` 是唯一值得顺手做的一处**接口健壮性**改良，等 `main.rs` 拆薄时一并处理即可，不属于缺陷修复。

**与本项目既有约定的关系.** 本仓 `AGENTS.md` 已声明「`clippy::all` 为零告警是硬门；pedantic 已降为提示（存量基线 ~27 处）」。**警告预算正是这条约定的可执行化**：把「~27 处」从注释里的数字变成脚本比对。

---

### A2 · 压缩的两个缺口：临界兜底与工具配对保护 ★★ 本轮新发现

jcode 的 `jcode-compaction-core` 提供了两个 heartflow 缺失的机制，其中第二个暴露出一处**真实的潜在缺陷**。

**(1) 临界兜底（两档阈值）.** jcode 的两档阈值是具名常量，可直接照搬语义：

```rust
// crates/jcode-compaction-core/src/lib.rs
pub const COMPACTION_THRESHOLD: f32 = 0.80;   // :9  常态触发
pub const CRITICAL_THRESHOLD:   f32 = 0.95;   // :13 同步硬压缩兜底
pub const RECENT_TURNS_TO_KEEP: usize = 10;   // :19 保留轮数
```

落点在 `crates/jcode-base/src/compaction.rs:966-1016`：用量越过 `CRITICAL_THRESHOLD` 时不再等后台任务，直接调 `hard_compact_with()` 同步压缩（`:992`、`:1016`），并在 `test_guard_at_95_triggers_hard_compact`（`compaction_tests.rs:388`）里被固定住。

heartflow 只有单档：

```rust
// crates/runtime/src/compact.rs:252-256
if config.context_window_tokens > 0 {
    estimated >= config.context_window_tokens / 2   // 单一半窗阈值
} else {
    estimated >= config.max_estimated_tokens
}
```

半窗触发本身没问题（已在 #35 的实测中校准确），缺的是**兜底**：一旦估算器低估（#35 修好前偏差 −57.5%，修正后仍有 −1.5%、p90 35.6%），或某轮工具输出暴涨，就没有第二道闸。jcode 的 0.95 档成本极低——一个 `else if`。

**(2) 工具配对保护 `safe_compaction_cutoff` — 这条是缺陷，不是优化.**

jcode 在**切分点**上有一句自陈目的的注释，值得逐字引用：

```rust
// crates/jcode-base/src/compaction.rs:903-904
// Adjust cutoff to not split tool call/result pairs
cutoff = safe_compaction_cutoff(active, cutoff);
```

实现落点 `crates/jcode-compaction-core/src/lib.rs:238-264`：扫描*保留侧*的 tool 调用/结果 id，若出现「有 ToolResult 但 ToolUse 已被切掉」的情形（`missing_tool_ids` 非空）就外扩截断点。

heartflow 的压缩按**消息条数**切：

```rust
// crates/runtime/src/compact.rs:319
let keep_from = message_count.saturating_sub(config.preserve_recent_messages); // = 4
let mut messages = std::mem::take(&mut session.messages);
let preserved = messages.split_off(keep_from);
```

而回放投影**只做 stub、不做孤儿剔除**——`conversation.rs:245-323` 的 `build_replay_messages` 逐块保留 `tool_use_id`（`:292-297`），不会丢弃任何孤立 result（全仓 grep `orphan` 零命中）。

**可达性推演（以 `preserve_recent_messages = 4` 为例）：**

```
[… A₁(tool_use x), U₁(tool_result x), A₂(tool_use y), U₂(tool_result y), A₃(text)]
keep_from = 5-4 = 1  →  保留 [U₁, A₂, U₂, A₃]
```

`U₁` 的 `tool_use x` 在 `A₁` 里，已被折进摘要 → **发给提供商的首条消息是一个孤立的 `tool_result`**。Anthropic 要求 `tool_result` 必须对应紧邻前一条 assistant 消息里的 `tool_use`；OpenAI 的 `tool` role 必须引用前序 `tool_calls` id。两者都可能直接 400。

**处置建议（按 list.md 的测试纪律）.** 先把这个当成 **C 系列猜想**：埋点统计实际会话中「`keep_from` 落在 tool 结果上」的比例，>0 即成立。机制修复本身很小（照搬 `safe_compaction_cutoff` 的扫描，把 `keep_from` 向前外扩到 ToolUse 所在消息），**但必须先有观测证据再改**——这正是本项目已确立的纪律。

---

### A3 · 命令风险分级：从 bool 启发式到二阶段级联 ★★

**heartflow 现状.** `crates/runtime/src/bash.rs:639` 的 `is_dangerous_command(command: &str) -> bool` 是一串 `contains` 关键词（`:639` 起约 70 行），返回二元判定。

**jcode 的方案.** 独立零依赖 crate `jcode-command-risk`（2,542 行，含测试），核心是**按爆炸半径分类，而不是按命令名**：

| 设计点 | jcode 实现 | 为什么 |
|---|---|---|
| 四级风险 | `RiskLevel::{Safe, Low, Confirm, Catastrophic}`（`lib.rs:44-57`） | 名字型 denylist 必然漏 `find -delete`/`shred`/`truncate`/`dd`/`>file`（`lib.rs:20-22` 自陈） |
| 分词 + 分段 | `tokenize.rs`（323 行）：按 shell 运算符切段，输出带 `is_truncating_redirect_target` 标记的 `Token` | 重定向本身就是破坏面（`lib.rs:229`） |
| **wrapper 解包** | 剥掉 `sudo/env/nice/timeout/xargs/…`（`lib.rs:162-191`），且按 wrapper 逐个声明「哪个 flag 吃参数」 | 不解包则 `sudo rm -rf ~` 里**根本看不到 `rm`**——任何公共前缀都是完整绕过 |
| 路径感知 | `paths.rs`（321 行）：解析目标是否为 home / root / 凭据目录，`ProtectedPaths` | 「能不能撤销」比「命令叫什么」更接近本质 |
| 绝对拒绝层 | `Catastrophic` 是**不依赖解析正确性**的小集合（`lib.rs:54-56`） | 解析终归可被 `sh -c "$(printf …)"` 绕过，故硬底线不能寄望于解析 |
| 歧义即升级 | `lib.rs:23-25`「解析含糊时升级而非放行」 | 假阳性代价 = 一次反思回合；假阴性代价 = 一个 home 目录 |
| 二阶段级联 | 阶段 1 纯确定性（无网络无模型）；仅当非 `Safe` 才进阶段 2 反思门 | 常见安全路径零开销 |

**为什么对本项目特别值.** heartflow 的危险命令判定直接决定 `/mode` 四档权限的实际强度，而当前实现有两个结构性弱点：① 任何 wrapper 前缀即可绕过；② 无分级，`rm -rf` 与 `rm build/out.txt` 同等对待，导致要么过度拦截、要么放行过宽。

**代价与裁剪.** jcode 的完整实现含 `sh -c` 反思门（需二次模型调用）与持久化 quirks 记忆——**这两层不建议抄**（heartflow 单用户本地、无反思回合预算）。可移植的是 `tokenize` + wrapper 解包 + 路径分类 + 四级枚举，约 600–800 行纯逻辑，零新依赖，且**天然可单测**（jcode 自带 4 个 `*_tests.rs` 文件即为范本）。

---

### A4 · 「同机差分 + rule of three」验收方法论 ★ 直接解开 TUI 验证僵局

**这条修正一个此前的前提.** 上一轮判断「alt-screen 下交互行为无法靠单测覆盖，故 TUI 斜杠派发不宜盲改」。核实后**该表述过强**：heartflow **已经在用** ratatui 的无头测试后端——

- `crates/cli/src/tui.rs:2863`、`:2957`：`Terminal::new(TestBackend::new(w, h))`
- `crates/cli/src/viewport_term.rs:251`、`:259`、`:271`、`:284`、`:292`、`:309`、`:325`、`:342`：同类用法

jcode 在 `jcode-tui` 单 crate 内就有 **2,006 个无头 lib 测试**（`TUI_TEST_FLAKINESS.md:8`）。

**真正的缺口不是「无法测渲染」，而是「事件循环未分解为可测的纯状态机」**——键 → 动作 → 状态 → 回合派发这条链与运行时纠缠在同一处，没有缝。这与 `REFACTORING.md:61-63`（Phase 5：分离 app state / 命令解析 / 事件归约 / 渲染控制）所指的是同一件事。

**可移植的方法论（`RENDER_PARITY_ACCEPTANCE_CRITERIA.md`）.** 它把「换掉一个核心组件」变成可判定的验收，四个要素：

1. **分级平价（L1–L4）**：内容平价 → 行结构平价 → 换行布局平价 → 样式不变量平价（`:8-18`）。每级是**独立的机器可比函数**，且只有在测试里直接断言了才计入覆盖。
2. **零容忍 + rule of three**：判据是 `mismatches == 0`，不存在「可接受的失配率」；统计量用来表达「一次通过**证明了什么**」——N 次零失配 ⇒ 95% 单侧上界 p < 3/N（`:24-39`）。CI 档 5,000 次 → p < 6.0e-4；切换前深跑 100,000 次 → p < 3.0e-5。
3. **失败必须可复现**：每轮 RNG 种子由 `seed = base_seed + i * 0x100000001B3` 导出，任何单例失败都能用 `JCODE_MD_FUZZ_SEED=… JCODE_MD_FUZZ_ITERS=…` 重现（`:62-66`）；失败时**收集最多 5 例再中止**，并原样回显输入以便直接提升为固定语料（`:68-73`）。
4. **生成器覆盖清单**：统计界只覆盖生成器的分布，故关键构造必须有固定语料兜底（`:84-102`，含 3 项显式未覆盖）。

诚实之处：文档第 20–22 行**显式列出刻意不纳入的差异**（空行填充数、装饰字形、非不变量 span 的 `Style` 全等），并要求任何新的有意差异都要补进该清单。

**对 heartflow 的映射.** 本项目已有一处**同类实践的先例**——#35 的 token 估算器就是用「Python 探针 vs Rust 实现交叉验证 + 真实会话 451 轮」定标的，并在工作日志里明确写出「两者原始误差逐位一致（62.4/57.5/83.4/−57.5）才敢用那些数字下结论」。**这条方法论的价值在于把它从一次性操作升级为可重复的验收程序。** 直接受益的候选：渲染层重构、压缩实现替换、估算器再校准。

---

### A5 · 隔离影子环境：把「不敢真机跑」变成「在隔离环境里跑」

**jcode 的做法（`scripts/refactor_shadow.sh`）.** 一个 230 行的包装脚本，做三件事：

1. 用**独立的 home 与 socket**（`JCODE_HOME`/`JCODE_SOCKET`，`:105`）跑构建产物；
2. **硬拒绝碰生产路径**：若 `ref_home == $HOME/.jcode` 直接报错退出（`:62-65`），且只删自己那类 socket，遇到非 socket 路径就拒绝（`:90-102`）；
3. `umask 077` + `chmod 700`，隔离目录默认私有（`:5`、`:73`）。

**heartflow 的同构物已存在.** `crates/runtime/src/config.rs:69` 的 `HEARTFLOW_CONFIG_HOME` 正是那个「隔离 home」开关。缺的只是包装脚本与**拒绝逻辑**。

**价值.** 这直接缓解本项目最紧的约束：TUI/REPL 改动「无法真机验证」。在隔离 home + 临时 cwd 下跑一次真实二进制，即便不能断言屏幕像素，也能验证「进程不崩、配置不漏读、不写脏用户目录」——这正是上一轮拒绝的那类改动最需要的那一层证据。

**注意.** jcode 的完整形态依赖常驻 daemon（`AGENTS.md:34-39` 解释了为何「不重指符号链接就测的是旧代码」）。heartflow 无 daemon，**这一层不适用**；可移植的只是「隔离 + 拒绝 + 私有权限」三条。

---

### A6 · 把编译期失效边界当作架构度量 ★ 与 cli 拆薄直接相关

jcode 的两份文档给出了这个领域少见的**量化**表述：

**`CRATE_OWNERSHIP_BOUNDARIES.md:129-139` 的实测基线（mtime-touch 法）：**

| 场景 | 观察耗时 | 解读 |
|---|---|---|
| 触碰根 crate 行为模块 `src/usage.rs` | **~6.25s** | 依赖已建时，根内改动可以很便宜 |
| 触碰 `crates/jcode-core/src/usage_types.rs` | **~65.35s** | **改 core 会作废广泛下游** |

结论（`:139`）：「编译速度目标**不是**简单地把东西搬出根 crate」——把**高 churn** 的领域 DTO 塞进高扇出的 `jcode-core` 会适得其反，应放进 `jcode-usage-types` 这类**叶子** crate（`:137`、`:141-143`）。

**`COMPILE_TIME_ISOLATION_REFACTOR.md` 的诊断方法.** 它先用 Cargo timing 报告证明瓶颈**不是链接器也不是第三方冷编译**，而是「少数巨型 crate 串成一条链」的前端串行化（`:26-41`：`jcode-base → app-core → tui → lib → bin` 串行栈跨度 14.72s / 前端 11.99s）。目标因此被表述为「**加宽依赖 DAG、缩小串行前端单元**」（`:9`），而不是「更多 crate」。

配套的**反目标**（`:198-203`）尤其值得抄：
- 不要为「文件看起来更整齐」而拆；拆不出失效边界或并行度就不拆；
- 不要把高 churn 行为搬进低层类型/协议 crate；
- 不要一次性大重写——每个阶段都要能构建、能被测量。

**对 heartflow 的映射.** 本项目正在做 cli 拆薄（S1–S7），但**目前没有任何编译耗时数据**。可移植的最小行动：对 `cli/src/main.rs`、`cli/src/tui.rs`、`runtime/src/conversation.rs` 各做一次 mtime-touch 基准，确认拆薄是否真的加宽了串行单元。jcode 还提供了「把 16 种 crate 归为 4 层」的分层规则（`MODULAR_ARCHITECTURE_RFC.md:225-292`）与 10 条依赖方向铁律（`:666-738`），其中 3 条对本项目直接可用：

- **类型 crate 只依赖 serde/chrono 等轻量库**，不依赖 tokio/reqwest/ratatui（`:308-333`）；
- **反方向禁令**：契约 crate 不得依赖运行时 crate；TUI crate 不得依赖具体实现内部（`:433-439`）；
- **禁止 `jcode-common` 式 mega crate**：「它会变成新的根 crate，作废一切」（`:458`）。

**必须诚实标注的差异.** heartflow 只有 8 个 crate、35k 行；jcode 是 101 crate。**本项目不应照搬 crate 数量**——jcode 自己也反对「一目录一 crate」（`:459`）。真正可移植的是**判据**（失效边界 + 所有权 + 依赖重量）与**测量习惯**，不是切分粒度。

---

### A7 · 大对象分解的增量纪律 ★ 与「并行 agent 改动」场景高度吻合

`TUISTATE_TRAIT_DECOMPOSITION.md` 分析一个 **114 方法**的 trait，是「先审计再动手」的范本。两处反直觉结论：

1. **拆成子 trait 不是编译解耦收益**（`:20-44`）：因为 `App` 无论如何都要实现整个表面，且 `&dyn TuiState` 无法组合（Rust 没有稳定的 `&dyn (A + B)`），中央渲染器只能继续持有完整 supertrait。收益是**可读性与叶模块的窄化**，不是解耦。文档明确写出这一点，而不是把「拆了」当成功。
2. **有测量支撑的行动范围**（`:36-40`）：28 个渲染模块中只有 **2** 个是跨类别，其余 26 个各用单域——所以子 trait 拆分对多数模块确有收窄价值，但「头部宽接口」不会变窄。

以及最贴合本项目处境的一条（`:132-147`）：迁移**按每次一个叶 trait、每次一个提交**推进，「每步都是行为保持的、可独立编译的，因此**可以穿插在别的 agent 的工作之间合并，而不会形成大爆炸式冲突**」。

**对 heartflow 的映射.** 本条正好补上 `list.md` 里「S4 暂不抽」的决策方法论：不是「抽 or 不抽」的二元选择，而是**先量化收益属于哪一类**（编译解耦 / 可读性 / 可测试性），再决定值不值。若收益只是可读性，那就该按「可穿插合并」的粒度做，而不是动最不可验证的中枢。

---

### A8 · 缺陷文档模板：把「什么不起作用」也写下来 ★

`TUI_TEST_FLAKINESS.md`（78 行）把一次并行 flaky 定位写成可复核的报告，结构如下：

| 段 | 内容 | 为什么值钱 |
|---|---|---|
| Evidence | `--test-threads=1` 下 2006/2006 通过；默认线程数下失败集合每次不同；单独跑必过（`:6-10`） | 用三个事实把「竞态」与「逻辑错」分开 |
| Root cause | 定位到 `create_test_app()` 调 `clear_test_render_state_for_tests()` 清**进程全局**渲染状态，且**未持锁**（`:17-31`） | 指出共享可变状态是根因 |
| Bisected proof | 二分出触发者 `test_tui_login_providers_have_real_tui_handlers`（`:39-45`） | 可复现的归因 |
| **What does not work** | 加锁 → 套件从 12s 变 **10 分钟以上**，实测后回退；断言下界 / 手动清理 → 5 次全失败，**回退而非作为 churn 提交**（`:47-56`） | **负结果与正结果同等记录**，避免后人重走 |
| Suggested direction | 首选 thread-local（消除共享状态），而非给共享状态加协调（`:58-71`） | 给出方向而非补丁 |
| Scope note | 说明与另三个提交无关，用 stash 复现过（`:73-77`） | 隔离责任范围 |

**为什么对本项目特别值.** `list.md` 已有这个气质——「已裁定」栏、`#3`/`#36`/`#46` 被证伪后如实改档、变异测试证明用例判别力。**本条是把这种气质固化成模板**：一个 `.md` 骨架，以后再遇到「修不动/不该修」的问题直接填，而不是让结论散落在对话里。

---

### A9 · 交互层的两个小机制

**(1) 能力探测可注入.** `jcode-tui-style` 用 `AtomicU8` 承载颜色能力覆盖（`crates/jcode-tui-style/src/color.rs:15-41`），并提供 `pin_truecolor_for_tests`。**理由**：终端能力探测读环境（`COLORTERM` 等），若直接 `env::var`，测试就依赖运行环境；切成原子可写覆盖后，测试能确定性地断言降级分支。
**heartflow 落点**：`cli/src/theme.rs`、`cli/src/viewport_term.rs` 的能力探测若读环境变量，同样应可注入。（本仓已有 `theme.rs`/`keymap.rs`/`settings.rs` 的三面分层，缺的是探测值的可注入性。）

**(2) 按键冲突探测作为独立纯函数.** jcode 把冲突检测写成「纯解析 + 薄壳」，可脱离终端测试（`docs/KEYMAP_CONFLICTS.md`）。
**heartflow 落点**：`cli/src/keymap.rs`（898 行）已有两层覆盖与动作映射，可增设「绑定表自检」：同一键被两个动作占用时在启动期报告，而不是等用户按键才发现。

---

### A10 · schema 集中注册表（只借一半）

heartflow 的 `crates/runtime/src/schema.rs` 已有：`normalize_tool_schema`（对象补 `properties`、数组补 `items`、单元素 `type` 联合降为裸串，`:77-116`）+ 按 canonical JSON 缓存的编译后校验器（`:19-36`）+ 「宽松/畸形 schema 永不拦截调用」的契约（`:41-60`）。

jcode 的 `jcode-schema-dialect` 是三段式：**allow-list 校验 → 单一递归 walk 归一 → provider 400 文本恢复 → 持久化 quirks 记忆**。

**建议只借前两段，不借后两段。**
- 可借：**allow-list 而非 denylist**（未知关键字永不导致拒绝——heartflow 已有等价测试 `unknown_keywords_are_ignored`）、单一递归 walk 配深度上限（`:81-85`，heartflow 已有 32 层上限）。
- 不借：**provider 400 文本恢复 + quirks 持久化**。这需要多 provider 方言矩阵与远程 quirks 库；heartflow 是单一 provider 抽象，引入只会增加一条无法验证的失败路径。

**与既有条目的关系.** `list.md` 的 **#5（校验器缓存键改按工具名）** 与本条同落点：现状 `schema.rs:28` 每次工具调用都 `serde_json::to_string(schema)` 生成缓存键，而 schema 在会话内不变。jcode 的注册表把「按 schema 内容寻址」换成「按工具身份寻址」，正是 #5 的方向——**两条应合并实施**。

---

## B 档：jcode 有，heartflow 已有等价物（不再投入）

| 能力 | jcode 实现 | heartflow 等价物 |
|---|---|---|
| 原子写 + 崩溃恢复 | JSON temp+rename + `.bak` 回填 | `session.rs` temp+rename；JSON 权威 + SQLite 事务 |
| 持久化与并发读 | `O_APPEND` 整行追加、`active_pids` 存活判定 | SQLite WAL + `busy_timeout`；`store/src/lib.rs` 显式事务 |
| 存储调优 | — | `lib.rs:110-120` pragma 已吃满（见 `list.md`「已具备」） |
| 外部工具探测缓存 | `OnceLock` | `doc_search.rs:124` `rga_available()` 同款 |
| 全局分配器 | 系统默认 | `main.rs:22` mimalloc |
| 网络重试纪律 | — | `api/retry.rs` CONNECT/READ 分层超时、幂等感知重试 |
| redirect 安全 | — | `web.rs:68` 上限 5 + 逐跳 SSRF 复查 |
| 压缩/回放/免疫 | 有 | `compact.rs` 就地压缩、`conversation.rs:245` 回放投影、`pinned` 免疫 |
| 帧缓冲双缓冲 | 有 | `viewport_term.rs:47` 双 `Buffer` |
| 主题 / 键位双层覆盖 | 有 | `theme.rs` / `keymap.rs` / `settings.rs` 用户层 + 项目层 |
| 通配符 re-export 治理 | 有预算脚本 | 实测 **0 处**，无需治理 |

---

## C 档：明确不该抄（附理由）

| jcode 能力 | 不抄的理由 |
|---|---|
| 单 server / 多 client + socket 生命周期（`SERVER_ARCHITECTURE.md`、`MULTI_SESSION_CLIENT_ARCHITECTURE.md`） | heartflow 是单进程终端程序，**无对应载体**；引入即重写产品形态 |
| swarm 协调与任务图（`SWARM_ARCHITECTURE.md`、`SWARM_TASK_GRAPH.md`） | 依赖常驻 server 与跨会话共享状态 |
| ambient 无人值守 + safety review queue + 邮件/SMS/webhook 通知（`SAFETY_SYSTEM.md`） | 该文档状态是 **Design**（`:3`），未实现；且核心价值（离开本地沙箱的动作需人类批准）以「无人在场」为前提，heartflow 是交互式的，**前提不成立** |
| 遥测上报（`TELEMETRY.md`，29k 字；`jcode-telemetry-core`） | 需要服务端与隐私合规面；本项目无此产品决策 |
| embedding / ONNX 记忆栈（`jcode-embedding`）+ 远程 Jev 记忆决策模型 | heartflow 的历史只有 MB 级，且 `list.md` 已判定 HNSW/向量召回**无场景** |
| 五格式历史导入（`jcode-import-core`） | 一次性迁移需求，成本高、无持续收益 |
| self-dev 自构建 / 热重载（`jcode-selfdev-types`、`daemon` 重指） | 依赖常驻进程与构建产物频道管理 |
| desktop / ios / gateway / harness-api-server / SDK | 均为产品面扩张，非本项目目标 |
| **101 crate 的切分粒度本身** | jcode 自己反对「一目录一 crate」（`MODULAR_ARCHITECTURE_RFC.md:459`）；本项目 8 crate / 35k 行应移植**判据**而非粒度 |
| 命令风险的 stage-2 反思门 | 需额外模型回合预算；heartflow 的 `Confirm` 可由现有权限确认通道承载 |

---

## 第二批来源：rig / ADK-Rust / Pica / Morphz

> 判据与第一批相同：**先核实 heartflow 现状，再判断缺什么**。本节所有「heartflow 实测」句均可复现（命令附在证据索引）。
> 一句话总览：**四个来源里，rig 与 Morphz 各给出一条真缺口（D1 回放、D2 命名诚实性），ADK-Rust 给出两条（D3 特性分层、D4 评估层），Pica 可借鉴项最少。**

### D 档前的更正：一条先入之见被实测推翻

调研前我预期「heartflow 没有 provider 级 mock，所以该抄 rig 的 `MockCompletionModel`」。**实测推翻了它**，如实记录，避免日后照着错误前提重复讨论：

| 我原本以为 | 实测 |
|---|---|
| 没有 mock | `crates/runtime/src/conversation.rs` 测试模块里有 **15 个 `impl ApiClient`**（`ScriptedApiClient:1200`、`SingleCallApiClient:1341`、`TruncatedApiClient:1533`、`StreamingApiClient:1564`、`OneToolClient:1663`、`EagerClient:1900`、`TwoStepClient:1951`、`ToolBatchClient:2224` …）与 **5 个 `impl ToolExecutor`**（`StaticToolExecutor:1073`、`SpecExecutor:1596`、`SchemaExecutor:1640`、`PlanExecutor:1750`、`ProbeExecutor:2199`） |
| 测试靠真网 | `crates/api/tests/client_integration.rs`（445 行）**自建 `TcpListener` 假服务器**，捕获请求体 + 回放固定 SSE/JSON——保真度高于 trait 级 mock（连 reqwest 与 SSE 解析一起测） |
| 无法端到端 | `crates/cli/tests/e2e.rs`（156 行）用 `Command` **拉起真实 `hf` 二进制**，驱 `hf --resume <file> --run <slash>` 的离线路径（该路径"never touches a provider"） |
| 压缩无法量测 | `crates/runtime/tests/calibration_measurement.rs` 是 `#[ignore]` 的人工仪器，**且已有记录基线**：2026-09-21，28 会话 / 451 轮，原始启发式误差中位 **62.4%**／偏差 **−57.5%**；校准后 **8.9%**／**−1.5%**；learned density ≈**2.23**、固定开销 ≈**11.6k** tokens |

**命名坑（我踩了）：本项目测试替身叫 `ScriptedApiClient` / `SimpleApi` / `*Client`，不叫 `Mock*`。** 用 `grep -i mock` 会得到零结果并误判为「没有 mock」。教训与 A1 的口径教训同型：**先确认命名约定，再下结论**。

### D 档：真缺、且成本可控

#### D1 · 效果日志与回放（rig `EffectLog` / `rig-cassette`）★ 最高性价比

- rig 把全部副作用建模为可序列化效果，经 `EffectLog` 记录，`LogHeader{format, run_spec, handlers, signature}` 让**整轮运行可重放**，重放时校验签名族匹配。
- heartflow 的缺口很**精确**：`conversation.rs` 的脚本化 client 是**进程内**的——只能断言「给定这个脚本，循环这么做」；**无法把一次真实回合（真实 provider 流）录下来再离线重放**。这正是「只有真机冒烟能验」的根因。
- **接缝已经现成**：`crates/runtime/src/conversation.rs:110` 的 `pub trait ApiClient: Send { fn stream(&mut self, request: ApiRequest) -> Result<TurnStream, RuntimeError>; }` 是整条链路的**唯一咽喉**。一个装饰器式 `RecordingApiClient`（录）＋ `ReplayApiClient`（放）即可覆盖，**不碰循环本体**。
- 与 A5（隔离影子环境）、A4（同机差分）互补：A5 管「在哪跑」，D1 管「跑完能不能重复跑」。
- **待验证前提**：`TurnStream` 能否在 `runtime` crate 外构造。若不能，录制层要么放进 `runtime` 内部，要么加 `#[doc(hidden)]` 构造器——**先验证这一点再动手**。

#### D2 · `dangerously_disable_sandbox` 命名过度承诺（Morphz 的真沙箱作对照）★ 置信度最高、成本最低

- Morphz 有**真**沙箱：macOS/Linux/Windows 各有原生实现；Linux 的 `workspace-write` 依赖 Bubblewrap + 非特权 user namespace；Windows 安全声明依赖 helper bundle；**安装器还会在下载前把这一边界报告给用户**。
- heartflow 实测：**全树不存在任何 OS 级隔离**——`seccomp` / `seatbelt` / `bubblewrap` / `namespaces` / `CreateRestrictedToken` 零命中。`crates/runtime/src/bash.rs:132-134` 的 `if !input.dangerously_disable_sandbox.unwrap_or(false) { scrub_credential_env(&mut spawn); }` 里，该开关的**唯一作用**是跳过 `scrub_credential_env`（`:383-389`，仅移除名字命中凭据规则的环境变量）。
- 所以这是**过度承诺的公开参数**：名字读作「关掉 OS 沙箱」，实际是「让子进程继承凭据环境」。两条路选一条：①**改名为 `dangerously_inherit_credentials`**（诚实、零风险、纯重命名）；②真做隔离（大工程，Windows 上无低成本方案）。
- 建议先做 ①——与 `list.md` 里「斜杠派发不盲改」同一取向：**先让名字说真话，再讨论要不要真做。**

#### D3 · 特性分层（ADK-Rust 的 feature tier）

- ADK-Rust 用 Cargo feature 把编译面切四层（默认 `minimal` = 仅 Gemini + Agent + Session）。
- heartflow 实测：**全仓零个 `[features]` 段**（`^\[features\]` 仅命中 `archive/jcode`）。每个构建都要吃下 `store` 的 `rusqlite bundled`（含 C 代码）、`cli` 的 `syntect 5`、`runtime` 的 `simd-json 0.18`、`tools/web` 的 `reqwest blocking`。
- 收益是**构建时间与依赖面**，不是正确性。代价要认：`AGENTS.md` 规定改公共契约须同步 README/`docs`/`llms.txt`/`bucket/heartflow.json`，feature 矩阵属于公共契约的一部分，**引入即新增一条同步义务**。建议**只切一刀**（`mcp` 与 `tools/web` 可关），不做四层。

#### D4 · 轨道评估层（ADK-Rust `adk-eval`）

- ADK 的 eval 提供：**轨迹评估**（工具调用序列 精确 / 子集 / 顺序无关 三种匹配）、文本相似度（Jaccard/Levenshtein/ROUGE）、LLM-as-judge、rubric 加权评分。
- heartflow 的脚本化 client 已能断言「循环行为」，但缺**对真实运行的打分**：例如「这一轮的工具调用序列是否是该任务的合理序列」「压缩后是否丢了关键事实」。
- 与 D1 是同一件事的两半：**D1 提供可重放的输入，D4 提供判据**。故 **D1 先行、D4 后置**——没有可重放输入的打分器只能测人工脚本，价值有限。
- 可沿用既有形态：`calibration_measurement.rs` 已经示范了「`#[ignore]` 人工仪器 + 记录基线 + 写明怎么跑」这一套。

### E 档：heartflow 已有等价物（勿重复投入）

| 借鉴点 | 来源 | heartflow 等价物（已核实） |
|---|---|---|
| 工具白名单 / 按 agent 限权 | Pica `availableTools`、ADK RBAC | `crates/cli/src/permissions.rs:38-85` 的 `PermissionPolicy`：逐工具 `Allow`/`Prompt`/`Deny` + `with_prompt_gate`（`plan` 模式挂 `BlockPrompter` 硬闸）；`read-only`/`plan` 两模式额外并入 MCP 只读工具名（`:79-84`）。**比 Pica 的静态白名单更细**（多一个 `Prompt` 中间态） |
| 系统提示由工具目录生成 | Pica `generateSystemPrompt()` | `crates/runtime/src/prompt.rs` 负责组装；`-- system-prompt` 子命令可直接打印（见 `AGENTS.md` 冒烟清单） |
| 脚本化模型 | rig `MockCompletionModel.script()` | 15 个 `impl ApiClient`（见上表） |
| 人机协同中断 / 恢复 | rig `OutcomeSink::detach()`、ADK HITL checkpointer | 已有三个 trait：`PermissionPrompter`（`runtime/src/permissions.rs:24`）、`UserQuestioner`（`cli/src/interact.rs:91`）、`EscalationHandler`（`cli/src/plan.rs:124`） |
| 无头 UI 测试 | （两者均无） | heartflow 领先：`TestBackend`（`cli/src/tui.rs:2863,2957`、`viewport_term.rs:251+`） |
| 结构化上下文替代 transcript | Morphz cognitive Frame | `runtime/src/compact.rs` 的压缩模型（取径不同，见 F 档） |

### F 档：不该抄（附理由）

- **rig 的 effect-bus**（单通道 + `Handle<F>` 类型化视图 + `Bus::reopen`）。它是**规模驱动**的答案——rig 要面对 Bevy/ECS/WASM 与多运行时托管。heartflow 是单进程 CLI、8 个 crate。引入总线等于把**编译期可查的调用关系**换成**运行期 handler 查表**，与 A6「把编译期失效边界当度量」的既定取向**正好相反**。理由同 `list.md` 对斜杠派发的处置：**爆炸半径与收益不成比例**。
- **ADK-Rust 的 39-crate 分层 + `GraphAgent`/`PregelExecutor`（BSP 并行 + checkpoint）**。heartflow 是单 agent 交互式终端，不是 DAG 编排引擎。其中**唯一值得留意的是 checkpoint 崩溃恢复**，但会话已落 SQLite、`--resume` 可用，收益撑不起一个执行器。
- **Pica 的 OneTool（100+ 托管集成）+ AuthKit**。这是**托管服务的产品面**（Gmail/Slack/Salesforce 的 OAuth 由它代管），不是可移植架构；heartflow 无账号体系，其 MCP 已覆盖「接外部工具」。**此来源可借鉴项最少，如实记录。**
- **Morphz 的 S-expression 认知机 / Yao 语言 / Mind Frame Exchange**。它主张「把结构化上下文而非不断增长的 transcript 作为模型直接评估的对象」，与 heartflow 的 transcript + 压缩是**范式级分歧**；且其自述为 Developer Preview（0.1），明确「不声称生产级多租户」。**读其 preprint，不照搬。**
- **MCP Elicitation**（ADK-Rust 提到的协议能力）：heartflow 的 `mcp` crate 实现了 `initialize`/`tools/list`/`tools/call`；对 **server 发起的请求**只在 `ping` 时回空结果，其余一律以 `-32601 "method not supported by heartflow"` 拒绝（`mcp/src/client.rs:141-152`）——`sampling/createMessage` 因此被挡在门外，`elicit` 全树亦零命中。已有自研 `ask_user` 工具承担同类职责，暂不引入协议级 elicitation。

### G 档：工程纪律（来自 Morphz 仓库布局，与刚做完的归属工作直接相关）

- **许可分层的形状值得记一笔**：Morphz 是 `LICENSE` + **`LICENSE_SCOPE.md`**（明确哪部分适用哪个许可）+ `TRADEMARKS.md` + `PATENTS.md`，且中英各一份。
- 对照 heartflow：只有 `LICENSE` + `NOTICE`（本轮刚补上 jcode 的 MIT 归属）。**当前无需扩建**；但若将来出现「vendored 第三方代码」或「分许可发布」（例如把某些 crate 单独再许可），`LICENSE_SCOPE` 这个形状是现成模板。
- **另一条：依赖审计门**。Morphz 的 CI 审计每个已提交的 lockfile；heartflow 无 `deny.toml`、无 `.cargo/`、workflows 里无 `cargo-deny`/`cargo-audit`（仅 `release.yml`/`docs.yml`，`ci.yml.bak` 刻意停用）。考虑到依赖面含 `rusqlite bundled`（C 代码）、`syntect`、`reqwest`，**一个 advisory 门有价值——但前提是先决定 CI 是否恢复**，否则又是一个「只在提交前手动跑」的脚本。

**第二批落地顺序**：**D2**（改名，零风险）→ **D1**（回放，先验证 `TurnStream` 构造可达性）→ **D3**（只切一刀 feature）→ **D4**（评估层，依赖 D1）。G 档两项彼此独立，可随时决定。

---

## 落地顺序建议（按「先验前提、再谈改造」）

结合 `list.md` 已确立的测试纪律（先测前提 → 确定性指标优先 → 报分位数 → 负结果同等记录）：

| 序 | 动作 | 类型 | 成本 | 前置 |
|---|---|---|---|---|
| 1 | **A1** 建 3 个棘轮脚本（代码体积 / panic / 警告），跑一次取基线 | 工具 | 低 | 无 |
| 2 | **A2(2)** 为「压缩切断 tool 配对」埋点，取真实频率 | 观测 | 低 | 无（**只观测，先不改**） |
| 3 | **A2(1)** 加 0.95 临界兜底 | 代码 | 极低 | 无 |
| 4 | **A4** 把 #35 的差分定标固化为可重复验收程序 | 方法 | 中 | 无 |
| 5 | **A5** 隔离影子环境脚本（`HEARTFLOW_CONFIG_HOME` + 拒绝逻辑） | 工具 | 低 | 无 |
| 6 | **A3** 命令风险分级（分词 + wrapper 解包 + 路径分类，**不含反思门**） | 代码 | 中 | 建议先有 A4 的差分台架 |
| 7 | **A6** 对 3 个热点做 mtime-touch 编译基准 | 测量 | 低 | 无 |
| 8 | **A9** 能力探测可注入 + 绑定表自检 | 代码 | 低 | 无 |
| 9 | **A10 + #5** 校验器按工具身份寻址（合并实施） | 代码 | 低 | 无 |
| 10 | **A7 / A8** 分解纪律与缺陷模板入档 | 方法 | 低 | 无 |

**本轮不改任何代码。** 上述条目建议以「新增章节」形式并入 `list.md`，并复用其现有分档（主清单 / 已具备 / 明确不做 / 猜想与测试）。

---

## 附录：证据索引

**jcode（`archive/jcode/`）** —— jcode **v0.86.0**，MIT License，Copyright (c) 2025 Jeremy Huang，上游 <https://github.com/1jehuang/jcode>。本仓 `archive/` 已列入 `.gitignore`，故该副本**不随仓库分发**；其中唯一被衍生进主树的是 `scripts/check_panic_budget.py`（归属与许可见根 `README.md` §致谢与第三方代码、根 `NOTICE`）。下表行号均对应存档副本 v0.86.0。

| 主题 | 路径 | 关键行 |
|---|---|---|
| 预算棘轮（体积） | `scripts/check_code_size_budget.py` | `:7-13` 策略、`:26` 阈值 1200 |
| 预算棘轮（panic） | `scripts/check_panic_budget.py` | `:28` 模式、`:79-105` 剔除内联测试 |
| 预算棘轮（吞错） | `scripts/check_swallowed_error_budget.py` | `:4-16` |
| 命令风险分级 | `crates/jcode-command-risk/src/lib.rs` | `:20-25` 设计取舍、`:44-57` 四级、`:151-199` 词表、`:227-249` 分段与解包 |
| 命令风险（分词/路径） | `.../src/tokenize.rs`、`.../src/paths.rs` | 323 行 / 321 行 |
| 压缩两档阈值 | `crates/jcode-compaction-core/src/lib.rs` | `:9` `COMPACTION_THRESHOLD=0.80`、`:13` `CRITICAL_THRESHOLD=0.95`、`:19` `RECENT_TURNS_TO_KEEP=10` |
| 压缩配对保护 | `crates/jcode-compaction-core/src/lib.rs` | `:238-264` `safe_compaction_cutoff` |
| 配对保护的调用点与意图 | `crates/jcode-base/src/compaction.rs` | `:903-904`「do not split tool call/result pairs」、`:966-1016` 临界硬压缩 |
| 迁移验收方法论 | `docs/RENDER_PARITY_ACCEPTANCE_CRITERIA.md` | `:8-18` L1-L4、`:24-39` rule of three、`:62-73` 可复现、`:84-102` 覆盖清单 |
| 隔离影子环境 | `scripts/refactor_shadow.sh` | `:62-65` 拒绝生产路径、`:90-102` 只删自有 socket、`:5` umask |
| 编译失效边界 | `docs/CRATE_OWNERSHIP_BOUNDARIES.md` | `:129-139` 实测基线、`:141-143` 扇出审计 |
| 编译隔离目标 | `docs/COMPILE_TIME_ISOLATION_REFACTOR.md` | `:9` 目标表述、`:26-41` 串行栈、`:198-203` 反目标 |
| 分层与依赖铁律 | `docs/MODULAR_ARCHITECTURE_RFC.md` | `:225-292` 四层、`:444-452` 拆分就绪清单、`:458-463` 反面模式、`:666-738` 规则 1-10 |
| 大对象分解纪律 | `docs/TUISTATE_TRAIT_DECOMPOSITION.md` | `:20-44` 非解耦收益、`:132-147` 可穿插合并 |
| 缺陷文档模板 | `docs/TUI_TEST_FLAKINESS.md` | `:6-10` 证据、`:17-31` 根因、`:47-56` 什么不起作用、`:58-71` 方向 |
| 重构非协商规则 | `docs/REFACTORING.md` | `:16-36` 五条规则、`:40-67` 阶段 |
| 能力探测可注入 | `crates/jcode-tui-style/src/color.rs` | `:15-41` |
| schema 方言 | `crates/jcode-schema-dialect/src/` | `lib.rs` 596 行、`quirks.rs` 243 行 |

**第二批来源（本节 D/E/F/G 档的依据）**

| 来源 | 出处 | 本轮取用的具体主张 |
|---|---|---|
| **rig** | `https://github.com/0xPlaygrounds/rig`（= `rig.rs`） | effect-bus（单通道 + `Handle<F>` 类型化视图 + `Bus::reopen`）；`EffectLog` / `LogHeader{format,run_spec,handlers,signature}` 可重放；`rig-cassette` 记录/回放；`const _: () = {…}` 编译期尺寸预算（`Dispatcher` 32B / `Pending` 64B）；`ContextValue{const KEY}` 声明式键；单点擦除守卫（只允许 `bus/handler.rs` 出现 `dyn Handler`）；`OutcomeSink::detach()` 外部应答；`MockCompletionModel.script()`；loom 并发模型检验；provider 别名由 rustdoc 生成（114 个） |
| **ADK-Rust** | `https://github.com/zavora-ai/adk-rust`（v1.0.0，Apache-2.0，39 crate，17+ provider，130K+ 下载 / 6 个月） | 五个核心 trait（`Agent`/`Llm`/`Tool`/`Session`/`Toolset`）；`#[tool]` 宏从 doc comment 提描述 + 由 args 类型推导 JSON Schema；`GraphAgent` + `PregelExecutor`(BSP) + checkpoint 崩溃恢复 + HITL 中断恢复；`adk-eval`（轨迹 精确/子集/顺序无关 三态匹配 + Jaccard/Levenshtein/ROUGE + LLM-as-judge + rubric）；RBAC/SSO/Guardrail（PII 脱敏、内容过滤、JSON Schema 校验）+ JSONL 审计日志；**feature tier**（minimal/standard/enterprise/full）；MCP Elicitation |
| **Pica** | `https://github.com/picahq/pica`（经 `https://juejin.cn/post/7463802171998994466`） | OneTool 统一 SDK 接 100+ 平台；`availableTools` 按 agent 限权；`generateSystemPrompt()` 按可用工具自动生成系统提示；AuthKit 托管 OAuth。**注**：该来源为产品面，可移植架构成分最少 |
| **Morphz** | `https://github.com/morphz-ai/morphz` | 「从 chat completion 到结构化上下文评估」：模型只负责非确定语义，**确定性事务内核**持有事实/授权/状态/执行/恢复；Agent 拥有独立于 session 的**版本化 cognitive Frame**；并发具因果结构（Objectives/Threads/Activations/dependencies）；**原生沙箱**（macOS/Linux/Windows；Linux `workspace-write` 需 Bubblewrap + 非特权 userns）；`LICENSE_SCOPE.md` + `TRADEMARKS.md` + `PATENTS.md`（中英各一份）；CI 审计每个已提交 lockfile；`update status/update/rollback` + SHA-256 校验 |

**heartflow（本仓）**

| 事实 | 路径:行 |
|---|---|
| 危险命令判定（bool 启发式） | `crates/runtime/src/bash.rs:639` |
| 压缩配置（保留 4 条 / verbatim 尾 12） | `crates/runtime/src/compact.rs:184-208` |
| 压缩触发（单一半窗阈值） | `crates/runtime/src/compact.rs:252-256` |
| 压缩按条数切分（配对风险点） | `crates/runtime/src/compact.rs:319-321` |
| 回放投影（只 stub，不剔孤儿） | `crates/runtime/src/conversation.rs:245-323`（`:292-297`） |
| 无头 TUI 测试后端已在使用 | `crates/cli/src/tui.rs:2863,2957`；`crates/cli/src/viewport_term.rs:251+` |
| 隔离配置根 | `crates/runtime/src/config.rs:69` |
| schema 归一化与校验器缓存 | `crates/runtime/src/schema.rs:19-36,77-116` |
| TUI 斜杠派发分叉点（本轮已入档决策） | `crates/cli/src/main.rs:728,779,950` |
| panic 预算棘轮（已落地，含 `panic-ok:` 第三类） | `scripts/check_panic_budget.py`；基线 `scripts/panic_budget.json` |
| 合理 hack 的 8 处标注 | `crates/runtime/src/redact.rs:40,50,63,74`；`crates/api/src/retry.rs:23,147`；`crates/runtime/src/bash.rs:481,483` |
| 可无痛消除的 2 处（已审计：不可达，非缺陷） | `crates/cli/src/main.rs:398,414` |
| **回放接缝（D1 落点，唯一咽喉）** | `crates/runtime/src/conversation.rs:110`（`trait ApiClient`）、`:118`（`trait ToolExecutor`） |
| 脚本化测试替身：15 个 `impl ApiClient` | `crates/runtime/src/conversation.rs:1200,1341,1392,1431,1474,1533,1564,1612,1663,1765,1781,1900,1951,2010,2224` |
| 脚本化测试替身：5 个 `impl ToolExecutor` | `crates/runtime/src/conversation.rs:1073,1596,1640,1750,2199` |
| socket 级假 HTTP 服务器（捕获请求 + 回放 SSE） | `crates/api/tests/client_integration.rs`（445 行） |
| 真实二进制 e2e（仅离线路径） | `crates/cli/tests/e2e.rs`（156 行） |
| 压缩估算器人工仪器 + 记录基线 | `crates/runtime/tests/calibration_measurement.rs`（451 轮：原始 62.4% / 校准 8.9%） |
| **无 OS 级沙箱**（该开关只控凭据擦除，D2 落点） | `crates/runtime/src/bash.rs:132-134`；`scrub_credential_env` `:383-389` |
| 全仓零 `[features]` 段（D3 落点） | `grep '^\[features\]'` 仅命中 `archive/jcode` |
| 全仓零编译期尺寸断言（对照 rig 的 `const _`） | `grep 'const _: ()'` 零命中 |
| 无依赖审计门（G 档） | 无 `deny.toml`、无 `.cargo/`；workflows 仅 `release.yml`/`docs.yml`（`ci.yml.bak` 刻意停用） |
| MCP 方法面：无 elicitation，server 请求除 ping 外一律 `-32601` 拒绝 | 我方方法 `crates/mcp/src/client.rs:63,86,98`；拒绝点 `:141-152`；测试 `:277` |
