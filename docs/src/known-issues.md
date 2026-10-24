# 已知问题登记（待修复清单）

本文件登记 heartflow 已观察到的**缺陷与体验问题**。按用户指令（2026-10-22）建立的登记纪律：条目登记时一律先**只登记、不修复**（状态「登记未修复」）；修复须先真机复现并核实根因，再回填根因与 commit（状态「已修复」）。每个条目含：现象、环境·复现、影响、根因核实（登记时为「疑点·待查位置」假设，修复时回填实证）、登记日期、状态。

新增条目请沿用 `KI-` 编号并追加到文末；修复后把状态改为「已修复（commit <sha>）」并从清单移除或留痕。

---

## KI-001 · Alt+Enter 在 pwsh 下触发全屏、无法进行多行续行

- **现象**：在交互式命令（实际执行 PowerShell，见 `error.md` 记录：`bash` 工具在 Windows 上跑的是 PowerShell）中输入多行命令时，按 **Alt+Enter** 不会像在普通终端里那样把光标移到下一行继续输入（续行），而是触发**全屏**表现（程序进入/切换全屏），换行意图落空。
- **环境·复现**：Windows + heartflow TUI（crossterm 原始模式 + 备用屏幕 `EnterAlternateScreen`）。在 TUI 内启动交互式/长运行命令（如 `bash` 工具跑 PowerShell）后，于输入态按 Alt+Enter。
- **影响**：无法在 TUI 内输入多行草稿；Alt+Enter 被「吃掉」或上抛到全屏语义，破坏正常编辑流。
- **根因核实（2026-10-23，代码实证）**：
  - 原「疑点 A：TUI 按键拦截层吃掉 Alt+Enter」**不成立**——keymap 无任何绑定认领 Alt+Enter（`submit` 只绑裸 Enter），该键一贯落到编辑器，tui-textarea 对带任意修饰键的 `Key::Enter` 执行 `insert_newline`，即 heartflow 层 Alt+Enter 本就插入换行。
  - 「全屏」表现来自**宿主终端**：Windows Terminal 默认键绑定 `alt+enter → toggleFullscreen` 在按键经 ConPTY 送达应用**之前**就被终端自身消费。heartflow 无法拦截宿主层语义。
  - heartflow 侧的真缺陷是**两条输入面不一致**：REPL 编辑器是「裸 Enter 发送；Shift/Alt+Enter 换行」（`editor.rs`），而 TUI 里 Shift+Enter 会**提交**（keymap 匹配刻意掩盖 SHIFT，导致 Shift+Enter 等同裸 Enter 命中 submit）——在 Alt+Enter 被宿主吃掉的 Windows Terminal 上，用户没有任何可发现的多行键。
  - 「透传 PTY」方向无机制可挂：全仓无 PTY（`!CMD` 与 bash 工具的子进程 stdin 均为 `Stdio::null()`），不存在「交互态透传」通道；该假设按无机制处理，不在本条修复范围。
- **修复（2026-10-23）**：TUI 对齐 REPL 约定——`on_key` 在 keymap 解析前拦截 Shift/Alt+Enter 插入换行（`tui.rs`），输入栏标题提示补 `shift+enter newline`。Windows Terminal 用户用 Shift+Enter（宿主不拦截）；要让 Alt+Enter 到达应用须自行在终端设置解绑其全屏动作。
- **登记日期**：2026-10-22
- **状态**：已修复（commit b9d2f9a，真机冒烟待用户陪测）

---

## KI-002 · Ctrl+C 提示文字显示时间过短，看不清消息

- **现象**：运行中按 **Ctrl+C** 触发打断时，界面给出的提示文字（如「再按一次确认打断」之类）**一闪而过**，来不及看清内容就消失。
- **环境·复现**：heartflow TUI 运行中（agent 回合或交互态），按 Ctrl+C。
- **影响**：用户无法从提示文字确认当前处于「已 armed 待二次确认」还是「已打断」状态，体验含糊，容易误按或不知已触发。
- **根因核实（2026-10-23，代码实证）**：原假设方向正确。武装提示写进**瞬态** `status_note`，而回合在飞时每个 `AgentEvent::ToolUse`（工具启动）都会重写 `status_note` 为 `tool N: \`name\``——工具密集的回合里提示在下一帧就被覆盖。armed 态本身并未丢失（`ctrl_c_armed` 不受流事件影响），丢的只是**显示**。
- **修复（2026-10-23）**：状态栏改为渲染**状态派生**的粘性覆盖——`App::rendered_status_note()` 在 `cancel_requested`（确认打断、直到回合结算）或 `ctrl_c_armed`（武装、直到任意他键解除或回合结算）为真时优先显示打断提示，流事件只重写底下的瞬态 note、盖不掉覆盖层。提示存续期与它描述的状态严格同寿命（状态是纯函数），无需显式超时器。循环级测试 `the_armed_cancel_hint_survives_streaming_tool_events` 锁定（武装提示在工具事件穿流期间持续在帧上可见）。
- **登记日期**：2026-10-22
- **状态**：已修复（commit b9d2f9a，真机冒烟待用户陪测）

---

## KI-003 · 粘贴多行文本被截断执行（回车触发提前提交）

- **现象**：粘贴含回车的多行文本时，hf 未读全就执行了（首行即触发提交）。
- **环境·复现**：用户实机（Windows 终端），TUI/REPL 粘贴多行内容。
- **影响**：粘贴的长指令被截断，首段直接运行，剩余内容丢失或变成新回合。
- **疑点·待查位置（假设，未核实）**：两面均已启用 bracketed paste（editor.rs:680、tui.rs:2175）且有 `Event::Paste` 整块插入处理——代码面看似完备。用户仍见截断，最可能假设：**所用终端未回传粘贴标记**（bracketed paste 需终端支持；终端不支持时 crossterm 的 Enable 静默无效，每行回车以裸 Key事件到达 → 首行 Enter 立即提交）。次级假设：REPL 编辑器整块插入后仍把内部换行当作后续提交点。
- **修复方向（待验证）**：① 粘连启发式——相邻键事件间隔 <10ms 且含 Enter 的突发序列按粘贴整块处理；② 终端能力探测降级提示；③ 「粘贴模式」显式开关。需用户真机复现区分假设。
- **登记日期**：2026-10-24
- **状态**：登记未修复（等待用户实机复现：终端程序与版本 + 复现步骤）

---

## KI-004 · `!eza` 等现代 CLI 工具的输出经 !CMD 不可读

- **现象**：`!eza`（及管道接 bat）的输出无法读取/不可读。
- **疑点·待查位置（假设，未核实）**：eza/bat 按 isatty 决定是否着色——管道下本应纯文本（工具自身规范），但用户的 eza 配置（EZA_CONFIG_OPTIONS）或 PowerShell profile 可能强制 `--color=always`，ANSI 转义序列进入捕获输出后污染渲染（业界通案：Unix SE 10823、bat #3257）；Windows 下还可能混入 VT 序列与代码页双重问题。
- **修复方向（待验证）**：① 捕获输出统一 **ANSI 转义剥离层**（防御性净化，与既有 chardetng 编码嗅探同族——不动原始字节只净呈现）；② 为子进程注入 `NO_COLOR=1` + `TERM=dumb`（no-color.org 规约，eza/bat 均遵守）；③ doctor 显示子进程 env 策略。
- **登记日期**：2026-10-24
- **状态**：登记未修复

---

## 登记纪律

- 登记时只登记、不修复（除非用户当次明确指令）。
- 每个条目必须含可复核的**现象描述**与**环境/复现**；登记时的疑点标注「假设，未核实」，禁止把假设写成结论。
- 修复前必须真机复现并核对代码，再回填根因与 commit。
- 本清单与 `roadmap.md`（未实现方向）分工不同：roadmap 是设计/方向，本文件是已观测到的具体缺陷。
