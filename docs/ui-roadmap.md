# heartflow UI 路线图与落地计划

> 状态：草案 v1 · 范围：阻塞式 REPL + 全屏 shell 双后端
> 基准：commit 当前主线（含 ratatui 全屏 shell 实验后端、FollowUpQueue 骨架、build_guide 三段组装）

---

## 一、待解决问题（先止血）

### 1.1 错误消息可理解性

**现象**：Turn failed 后输出 `✘ api request failed after 3 attempts: http error: error sending request for url (https://api.deepseek.com/v1/chat/completions)`

**根因**：`crates/api/src/error.rs:102` `RetriesExhausted` 直接透传 `last_error`（reqwest `Error` 的 Display），后者只说"error sending request for url"，不告诉你到底是 DNS 失败、TLS 握手失败、连接超时、还是代理问题。

**修复方案**：
- `ApiError::RetriesExhausted` 增加错误分类：在重试循环里记录最后一次失败的「类型」（connect / timeout / tls / status_code / body_decode / sse_frame），Display 时先输出分类再输出原始错误。
- 例：`✘ api request failed after 3 attempts (connection refused): error sending request for url (...)`
- 同时把 `last_error.source()` 链也打出来（一级即可，不递归），方便定位代理/证书等深层原因。
- 不改变 error 类型结构，只加一个 `last_error_kind: &'static str` 字段。

**落点**：`crates/api/src/retry.rs`（重试循环处打标）+ `crates/api/src/error.rs`（Display 输出分类）

### 1.2 失败时输出行保留

**现象**：Turn failed 时 spinner 行被替换成 "Turn failed"，流式过程中已输出的文本停在原处，但用户关心的是——"已产生的所有输出行必须永久留在终端里，不能被回退覆盖"，这是队列功能和多 section 的前置条件。

**当前状态核实**：
- 阻塞式 REPL：TurnRenderer 用 `\r` 回车覆写 spinner 行（spinner 的 tick/finish/fail 都在同一行走），这是唯一的"行被覆盖"的地方。
- 流式文本增量：直接 `print!` 追加，不回退。
- MessageStop 后的重渲染（整段 assistant text 重排 markdown）：这会用光标上移 + 清行来替换"裸 markdown"为格式化版本，这也是一种"行被替换"。

**修复原则**：
- spinner 行：失败后保留 spinner 的最终状态行（"Turn failed" / "Interrupted"），不清除——当前已满足。
- 流式重渲染（T1 做掉之后）：最后一个未闭合块的重绘范围严格限制在块内，已闭合的块永久不动。
- 错误详情行：追加打印，不覆写任何已有输出。
- 新增：失败时把「已产生的部分 assistant 文本」也保留并标记为不完整（muted 色 + "incomplete" 标记），而不是像现在这样 spinner fail 后用户看不到已流式输出了什么。

**当前代码缺口**：`turn.rs:240-271` 的 Err 分支里，`turn.spinner.fail()` 之后直接打错误行，`turn` 里累积的流式文本（`assistant_buf` / `thinking_buf`）没有被输出为"已产生但未完成"的状态。需要补一个 `turn.render_incomplete()` 方法，在失败路径上把已收到的内容格式化输出。

---

## 二、Readline 快捷键增强（P0 · 纯 editor.rs 内改动）

全部在 `crates/cli/src/editor.rs` 内实现，不触及 runtime，不影响现有键位。

### 2.1 Ctrl+Left / Ctrl+Right 单词跳转

**行为定义**：
- 英文：按单词边界跳转，支持代码子词（snake_case、kebab-case、camelCase、PascalCase、数字字母过渡）
- 中文：一次跳 2 个汉字
- 标点：连续标点作为一个词
- 行首/行尾：跳到边界即停，不跨行

**实现**：
- 新增 `fn word_break_prev(line: &str, byte_col: usize) -> usize` 和 `word_break_next`
- 字符级扫描，返回字节索引（与 tui-textarea cursor() 返回的 col 对齐）
- 跳转通过循环调用 `textarea.input(KeyEvent::new(KeyCode::Left, NONE))` 实现，次数 = 当前字符位置 - 目标字符位置（注意：字节索引 → 字符索引转换）
- 或者更干净：直接操作 `textarea` 内部 cursor——查 tui-textarea 有没有直接设置光标列的 API；如果没有，循环 Left/Right 是稳妥方案

**子词边界判定规则**（从左往右扫描 next，prev 对称）：
```
当前字符分类 C，下一个字符分类 N：
  C=lower_ascii_alpha, N=upper_ascii_alpha → 边界（camelCase）
  C=ascii_alnum,  N='_' or N='-'          → 边界（snake/kebab）
  C='_' or C='-',  N=ascii_alnum           → 边界
  C=digit,        N=alpha                 → 边界
  C=alpha,        N=digit                 → 边界
  C=cjk,          N=cjk                   → 每 2 个汉字一个边界
  C=punct,        N=punct                 → 不边界（连续标点算一词）
  C=whitespace,   N=whitespace            → 不边界（连续空白算一词）
  其他任何类型不一致 → 边界
```

### 2.2 Ctrl+U 清空整行

**行为**：静默清空当前输入行，不输出提示文字，清空补全和历史搜索状态。

**实现**：一行替换 `*textarea = blank_textarea()`，加状态清理。

### 2.3 Ctrl+[ / Ctrl+] 前缀历史搜索 + 行内灰色 hint

**行为定义**：
- Ctrl+[ ：向后（更旧的历史）循环滚动
- Ctrl+] ：向前（更新的历史）循环滚动
- 匹配规则：当前输入内容作为前缀，从最近历史开始向前找，最近优先
- 显示方式：仅把「前缀之后的后缀部分」作为 muted/dim 灰色 inline hint 放在光标后面，前缀保持不变
- 确认：按 →（Right 箭头）把后缀补入输入行，其他任意按键丢弃预览、重置滚动状态
- 输入修改（增删字符）：立刻清空候选列表和滚动索引，下次 Ctrl+[ 重新查找
- 无弹窗，仅行内 hint

**新增状态**（`edit_loop` 函数内 `PrefixSearchState`）：
```rust
struct PrefixSearchState {
    prefix: String,     // 触发时的输入快照
    candidates: Vec<usize>, // 命中的 history 索引（从新到旧）
    cursor: usize,      // 当前滚动位置（candidates 下标）
    active: bool,
}
```

**渲染实现**：
- `draw_frame` 中 textarea 渲染后，计算光标像素位置，在光标后手动写入灰色后缀文本
- 用 ratatui Buffer 的 cell_mut 直接写，不经过 textarea
- 后缀超出终端右边界时截断，末尾加 `…`

**按键分发改动**：
- `handle_key` 的 CONTROL 分支新增 `Char('[')` 和 `Char(']')`
- Right 键：如果 `prefix_search.active`，应用完整历史条目并清除搜索状态；否则走原有逻辑
- 文本修改类按键：在 `edit_loop` 顶部比较当前输入与 `prefix_search.prefix`，不一致则重置搜索状态（比在每个按键分支里写清除更可靠，覆盖粘贴/撤销等所有修改路径）

**测试**：
- 词边界单测：中英文混排、camelCase、snake_case、kebab-case、数字过渡、行首行尾、空行
- 前缀搜索单测：空前缀、无匹配、单匹配、多匹配循环、文本修改后重置、Right 确认、其他键取消

---

## 三、队列功能（P1 · 回合内输入 + FollowUpQueue 接键盘）

### 3.1 现状

- `FollowUpQueue` 结构已在 `core.rs` 落地，单 FIFO，有 `push` / `cancel_last` / `drain_injection` / `len`
- `drain_injection` 语义：回合边界取出一条注入下一轮
- `/queue pop` 和 `/queue clear` 命令已实现
- 注释："队列恒空，直到 P4-c 接键盘轮询"

### 3.2 阻塞式 REPL 下的接入

阻塞式 REPL 的问题：`run_turn_interactive` 是同步阻塞的，回合进行中键盘事件无法被 REPL 主循环读到。所以"回合内入队"在阻塞式 REPL 下做不了——用户按 Enter 的输入会被系统缓冲，等回合结束后 REPL 才读到。

**可行方案**：
- 回合进行中，用户输入的字符被终端行缓冲积攒；回合结束后，REPL 读这些积攒的输入，把第一行当次回合输入，后续行入队。
- 这是「被动入队」：不是用户刻意按回车入队，而是回合进行中用户打了多条消息，自动变成队列。
- 体验上差一些（没有实时反馈、看不到队列深度），但零架构改动，阻塞式 REPL 就能用。

**实现要点**：
- 回合结束后，读取 stdin 缓冲区中所有待处理行（用 `poll` + `read_line` 非阻塞读）
- 第一行作为下一回合输入直接执行
- 其余行 push 进 `FollowUpQueue`
- 回合开始前显示队列深度：`· N queued`

### 3.3 全屏 shell 下的接入（T5）

全屏 shell 有独立的事件循环，键盘事件与 AI 生成并行，真正的"回合内入队"才能实现。

**行为**：
- RUNNING 状态下按 Enter：当前输入框内容入队，输入框清空
- 状态栏实时显示 `[N queued]`
- 回合边界自动 drain 队列，取出第一条执行
- `/queue pop` 取消最后入队的一条
- `/queue clear` 清空队列

**多队列语义**（远期）：
- 默认单 FIFO，等上一个完成再开下一个
- 扩展：支持"注入当前回合"（工具补充参数）vs "作为下一个任务"（独立 follow-up）两种语义
- 当前 `drain_injection` 只处理注入，下一个任务语义需要新增 `drain_next_task`

---

## 四、引导功能（P1 · Ctrl+G overlay + 草稿确认）

### 4.1 现状

- `/guide <TASK>` 命令已实现
- `build_guide` 三段组装（prior work + state + task）零 token 本地生成
- 但没有可视化 overlay，也没有确认即发送的交互

### 4.2 阻塞式 REPL 下的降级

阻塞式 REPL 没有 overlay 能力，Ctrl+G 降级为：
- 输出 guide 草稿全文到终端
- 提示 "Enter to send, Esc to cancel"
- 阻塞等待一行输入（Enter 发送 / Esc 取消）

### 4.3 全屏 shell 下的完整实现（T6）

- Ctrl+G 弹出半屏 overlay
- 两栏布局：左侧 guide 草稿（可滚动只读），右侧操作说明
- Enter 确认发送，Esc 取消
- 草稿内容可编辑？暂不做，先只读确认

---

## 五、多 Section（P2 · TabBar + SessionActor 化）

### 5.1 现状

- 完全未开始
- 全屏 shell 已有 tab bar 预留（`HEARTFLOW_TUI=1` 下 sections > 1 时显示）
- 但 section 切换逻辑、每 section 独立 runtime、MCP 共享都没做

### 5.2 设计目标

- 每个 section = 独立会话 + 独立 runtime + 独立队列 + 独立 JSONL
- MCP 连接共享（同进程内复用，不重复起进程）
- 切换 section 不 abort 当前运行中的任务（后台继续）
- Ctrl+T 新建 section，Ctrl+PageUp/PageDown 切换
- Tab 栏显示 section 标题（从第一条用户消息提取，或默认 "new")
- 运行中的 section 前缀 `*` 标记

### 5.3 前置依赖

- S1 去全局化（AppState 可多实例化）
- T4 全屏承载层
- T5 队列

---

## 六、整体实施顺序（S 系列 = 结构 / T 系列 = 功能）

```
S0  错误消息可理解性 + 失败输出保留        ← 先止血，立即可做
    ↓
S1  去全局化：7 个 static 收进 AppState    ← 一切的地基
    ↓
S2  搬正交模块：storage + shell           ← main.rs 减三分之一
    ↓
S3  切 turns 模块（TurnRenderer + 折叠）  ← T1/T3 的落点
    ↓
T1  增量流式 markdown 渲染                ← 边流边格式化，不跳屏
T2  表格 + OSC 8 链接渲染                 ← render.rs 纯新增，可与 T1 并行
T3  折叠键位化（Tab 切换）                ← 与 T1 同模块
    ↓
S4  切 repl 模块（run_repl + 命令路由）   ← T5/T6 的落点
    ↓
T0  Readline 快捷键                       ← 独立于模块切分，editor.rs 内可随时插做
T5  回合内输入与多队列（全屏 shell）       ← 阻塞 REPL 降级方案先做
T6  Ctrl+G 引导 overlay（全屏 shell）     ← 阻塞 REPL 降级方案先做
    ↓
T4  全屏承载层（P4-c 主墙）                ← 有 AppState + turns + repl 之后做
    ↓
S5  切 hermes 模块
T7  Hermes 任务级 UI + 真机验证           ← 与队列语义统一
    ↓
S6  AppState 多实例化准备（为 T8 铺路）
T8  多 section 并行（SessionActor 化）     ← 最远的一块
```

**可独立穿插的项**（不依赖主序列，随时可做）：
- T0 Readline 快捷键（editor.rs 纯内部）
- T2 表格 + OSC 8（render.rs 纯新增）
- S0 错误消息 + 输出保留（小改动，高收益）
- T7 真机验证（不改代码，只跑测试）

---

## 七、关键工程纪律

### 7.1 搬迁 commit 零行为改动

S1-S5 的模块切割必须严格区分「搬迁 commit」和「行为改动 commit」。搬迁 commit 只保证编译通过 + 现有测试过，不改任何逻辑。否则 review 无法分辨搬动和改写，回归无法定位。

### 7.2 AppState 多实例设计

S1 第一步就把 AppState 设计成可多实例、不依赖进程单例。哪怕当下只有一个实例。如果 S1 图省事做成新单例，T8 多 section 就要把去全局化重吃一遍。

### 7.3 双后端对称

所有新增 UI 功能（折叠键位、队列显示、引导）都必须同时考虑阻塞式 REPL 和全屏 shell 两个后端。阻塞式 REPL 下可以降级，但不能完全不可用。功能矩阵：

| 功能 | 阻塞式 REPL | 全屏 shell |
|---|---|---|
| 单词跳转 | ✓（直接做） | ✓（继承自 editor） |
| Ctrl+U 清行 | ✓ | ✓ |
| 前缀历史搜索 | ✓（行内 hint） | ✓（行内 hint） |
| 队列 | 降级（被动入队） | ✓（回合内入队 + 状态栏） |
| 引导 Ctrl+G | 降级（输出 + 阻塞确认） | ✓（overlay） |
| 多 section | ✗（不适用） | ✓（TabBar） |
| 增量流式 markdown | ✓（光标上移重绘） | ✓（区块 diff 重绘） |

### 7.4 测试策略

- 词边界算法：纯函数单测，覆盖所有边界 case
- FollowUpQueue：已有单测，扩展多队列语义时补
- 端到端：用 scripted 客户端模拟按键序列，验证输出行数、光标位置、折叠状态
- 双后端：同一组 scripted 测试，分别跑两个后端，行为必须一致（除了明确标注的降级项）

---

## 八、风险与回退

| 风险 | 概率 | 影响 | 应对 |
|---|---|---|---|
| S1 去全局化引入并发 bug | 中 | 高 | 每搬迁一个 static 就跑一次全量测试 + 冒烟 |
| 增量流式渲染与 pulldown_cmark 交互出问题 | 中 | 中 | 先做"块级缓存"最简版，不做细粒度行级 diff |
| 全屏 shell IME 候选词错位 | 低 | 高 | 保留真实光标定位（editor.rs 当前做法），绝不自绘 IME |
| 多 section MCP 共享竞争 | 中 | 中 | MCP 连接层加锁，串行化 tool call 分发 |
| tui-textarea 无直接设光标 API 导致单词跳转性能差 | 低 | 低 | 循环 Left/Right 最多几十次，可接受；真有性能问题再 fork 加方法 |

---

## 九、当前进展核对（对照 openmemory 历史计划）

| 条目 | 状态 | 备注 |
|---|---|---|
| T1 增量流式 markdown | 未开始 | 优先级最高的观感提升 |
| T2 表格 + OSC 8 | 未开始 | render.rs 纯新增，独立可做 |
| T3 折叠键位化 | 未开始 | /expand 命令已在，缺键位入口 |
| T4 全屏承载层 | 部分（实验后端） | `HEARTFLOW_TUI=1` 已有 ratatui 骨架 |
| T5 回合内输入/队列 | 骨架已在 | FollowUpQueue 结构完成，缺键盘接入 |
| T6 Ctrl+G 引导 | 骨架已在 | build_guide 完成，缺 overlay |
| T7 Hermes 任务 UI | 未开始 | 任务环逻辑已完成，缺 UI 展示 |
| T8 多 section | 未开始 | 最远 |
| Readline 快捷键 | 未开始 | 本次新增，editor.rs 内可独立做 |
| 错误消息优化 | 未开始 | 本次新增，小改动高收益 |
| 失败输出保留 | 部分 | spinner 行保留，但流式内容未输出 incomplete 状态 |
