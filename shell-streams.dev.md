# shell 三家（bash / pwsh / nushell）流设计与 heartflow 结合调研

> 读法：给 AI 记忆用。事实块 = 可验证陈述（带来源）；结合点 = W 编号（可执行项，含落点 file:line）；否决块 = 明确不做 + 理由。末节给优先级序。本仓证据全部本机核实过；外部事实标注官方文档 URL。

## 0. 一句话结论

三家的核心分歧是「管道里流的是什么」：bash 流字节文本（组合性来自约定），pwsh 流 .NET 对象（组合性来自类型），nushell 流带 span 元数据的结构化值（组合性来自协议）。heartflow 的 bash 工具是「代理的眼睛」，三家教训映射为三件事：**stderr/编码语义必须包装层统一治理**（pwsh 是重灾区）、**大输出落盘引用而非内联**（已落地）、**错误带定位标签**（nushell LabeledError 模式）。

## 1. 设计哲学对照

| 维度 | bash | pwsh 7.4 | nushell 0.1xx |
|---|---|---|---|
| 管道载荷 | 字节（文本是约定） | .NET 对象（文本只是渲染） | 结构化 Value + metadata |
| 流数量 | 1(stdout)+2(stderr)+fd3+退出码 | **6 条**:Output/Error/Warning/Verbose/Debug/Progress(stream 6) | 1 条结构化 + 进度走 Progress 事件 |
| 错误模型 | exit code 约定(0/1/2/126/127/130) | ErrorRecord 对象 + $LASTEXITCODE 双轨 | ShellError/PluginError 带 span 标签 |
| 编码 | 字节透传,locale 决定解码 | OutputEncoding/代码页陷阱带 | 内部 UTF-8,边界显式 |
| 进程边界 | fork/exec,管道即字节流 | native 命令是「对象世界外来者」,进出要做序列化 | native 命令输出降级为 ByteStream(类型 Unknown) |
| 可组合性来源 | 约定(每程序读文本写文本) | 类型系统(Get-xxx→Where-Object→…) | 协议(Value schema + cell-path) |

教训浓缩：pwsh 证明「对象管道」跨进程边界时退化为文本（native 命令），pwsh 自己都在 7.3/7.4 连续打补丁（参数传递/错误升级），说明**跨进程统一语义极难，代理层必须在包装处收口**。nushell 证明「结构化 + span」可行且错误定位体验最佳，但其插件协议本质是「自家 IPC」——对 heartflow 而言可借鉴的是协议形态而非直接采用。

## 2. bash：事实块

- fd0/1/2 独立缓冲；`2>&1` 合流后**字节交错顺序不定**（两个独立 pipe 被 reader 轮询），合流输出对「按帧解析」的消费者不可靠。代理工具应分开捕获再决定合并展示。
- SIGPIPE：管道下游退出后上游写 fd1 得 SIGPIPE 默认死，exit code 141；`set -o pipefail` 才让管道中间环节失败可见——模型写 `big | head` 时 141 是常态不是故障。
- 无 NUL 兼容：bash 变量不能存 NUL；`xargs -0`/`find -print0` 是规避约定的补丁。
- exit code 语义弱：只有约定无类型；`curl` 用 22 表 --fail，`robocopy` 用非零表进度——**exit code 必须按程序解释，不能全局映射成败**。
- 子 shell/进程替换 `<(cmd)` 提供 fd 而非文件，代理沙箱若限 fd 会踩。

## 3. pwsh 7.4：事实块（来源 learn.microsoft.com about_Preference_Variables / about_Parsing）

- **6 条流**：Write-Output(1)/Write-Error(2)/Warning(3)/Verbose(4)/Debug(5)/Progress(6)。代理捕获 stdout 时 3-6 默认不进 stdout；`$ProgressPreference='SilentlyContinue'` 是脚本提速标配（Invoke-WebRequest 进度条可拖慢 10 倍量级）。
- **native stderr 双轨史**：7.2 起 native 命令重定向 `2>&1` 产生的记录**不进 $Error、不受 $ErrorActionPreference 管**；7.4 新增 `$PSNativeCommandUseErrorActionPreference`（默认 $false），设 true 才把非零退出码按 EAP 升级。**结论：pwsh 下模型写 `cmd 2>&1` 得到的 stderr 会带 `NativeCommandError` 包装头（旧版行为）或纯文本（7.2+），跨版本语义漂移，代理不能靠解析它。**
- **参数传递三模式** `$PSNativeCommandArgumentPassing`：Legacy（整行拼接）/ Standard（数组化转义）/ Windows（Standard 但对 cmd.exe/.bat/.cmd/find.exe/sqlcmd 等回退 Legacy）。默认 Windows。**传参含空格引号给 .bat 时 Legacy 模式是注入面**——代理 wrap 层不该让模型直接拼 .bat 调用。
- **编码**：`[Console]::OutputEncoding` 决定解码 native 输出；`$OutputEncoding` 决定管道给 native 的输入编码；Windows PowerShell 5 默认 ANSI 代码页，pwsh 7 默认 UTF-8 但 native 仍是 OEM 代码页——中文环境乱码重灾区。包装层显式设两者为 UTF-8 是唯一稳妥解。
- **$LASTEXITCODE 只由最后一个 native 命令设置**；cmdlet 不设也不清零——`Get-ChildItem; git status` 后 $LASTEXITCODE 可能是上一次的残留。代理层判断成败只能看子进程真实 ExitStatus，不能信 shell 内变量。
- `-Command` 接字符串存在转义二义性；`-EncodedCommand`（base64 UTF-16LE）无歧义但不可读。heartflow 用 `-Command` + 前缀注入可接受，但要意识到引号嵌套是模型出错高发区。

## 4. nushell：事实块（来源 nushell.sh/contributor-book/plugins）

- 管道值 = `[Value, metadata]` 二元组；**每个 Value 带 span(start/end)**，错误渲染用 span 下划线定位源码位置——这是三家错误体验的天花板。
- 流类型：单值 Value / `ListStream`（无限值流）/ `ByteStream`（外部命令原始输出，类型 String/Binary/**Unknown**）。native 命令输出默认 Unknown——「结构化世界」显式标记降级区，不假装能解析。
- **插件协议**（对 heartflow MCP/jobs 最有参照价值）：
  - 握手：启动时输出「长度字节 + 编码器名（json/msgpack）」再发 `Hello{protocol,version,features}`，版本 minor 必须匹配否则插件退出。
  - 每条 Call 带**自增 ID**，响应必须 `CallResponse:[id,…]` 配对——乱序/丢失可检测。
  - stdio 模式 stdin/stdout 专用协议，stderr 继承给人看——**人与机器不同通道**，代理工具输出同理（人读渲染版，模型读结构版）。
  - `Goodbye` 优雅退出 + GC 机制（流式响应期间引擎等待调用完成才 goodbye）；输入流生命周期受输出消费节制（0.117+：输出被丢弃后再读输入得 Interrupted 而非 EOF）。
  - 错误 = `LabeledError{msg, labels:[{text,span}], help, code, url, inner}`。
- 环境是值不是进程属性（$env 表可闭包携带）——代理快照/恢复会话状态时的好参照。

## 5. 文件传输格式横向（代理把文件/大块数据搬进上下文的方式）

| 方式 | 代表 | 优点 | 缺点 |
|---|---|---|---|
| 内联全文 | 早期 copilot/read 工具 | 简单 | 烧 context,大文件不可行 |
| 有界截断+注记 | heartflow bash.rs read_stream_bounded | 便宜 | 截断点语义不保证 |
| 大件落盘+路径引用 | heartflow persisted_output_path / 后台 tee 日志 | context 恒定,可二次精确读取 | 需要二次工具调用 |
| diff/patch 格式 | aider(u-diff),codex apply_patch | token 密度高,可审 | 模型生成 patch 失败率高,需回退梯子 |
| 结构化信封 | nushell 插件 msgpack,MCP content 数组 | 机器可验证 | 人不可读,调试要解码器 |
| base64 | 二进制唯一正道 | 无损 | 4/3 膨胀 |

本仓已落地的正确决策：**双通道**（模型读 stdout 有界版 + rawOutputPath 全量落盘）+ **后台 sidecar**（bg-*.log 管内容、status JSON 管终态，bash.rs:216-247）。这正是 nushell「人机分通道」与「大件不进上下文」的收敛。

## 6. 与 heartflow 结合点（W 系列，按证据落点）

现状盘点（本机已核实）：
- `crates/runtime/src/bash.rs:89` current_shell → Windows pwsh 优先(853)，`-NoLogo -NoProfile -Command`(858)；`HEARTFLOW_SHELL` 可覆盖。
- `bash.rs:830` wrap_command_for_encoding：pwsh 命令前缀注入 `[Console]::OutputEncoding=UTF8; $OutputEncoding=UTF8`。
- `bash.rs:156` read_stream_bounded：stdout/stderr **分开捕获**各 512KiB 上限，超限保留计数+省略注记(181)。
- `bash.rs:117` arm_idle_watchdog：前台双流静默超时杀进程（HEARTFLOW_BASH_IDLE_TIMEOUT_MS，默认关），背景任务豁免。
- `bash.rs:26` BashCommandOutput：stdout/stderr 分字段 + interrupted + rawOutputPath + background sidecar。
- `crates/api/src/client.rs:263` 流读 chunk().await 无 idle 超时（S1，本批 ST-1 修）。

结合点：
- **W1（做，随 ST-1 批次顺手）pwsh stderr 包装治理**：wrap 前缀追加 `$ErrorActionPreference='Continue'; $PSNativeCommandUseErrorActionPreference=$false;`——防模型写的命令在 7.4 特定配置下 stderr 升级为终止错误中断捕获；同时 `$ProgressPreference='SilentlyContinue'` 提速。落点 bash.rs:833 prefix 字符串。代价：3 个变量注入，无行为回归风险（显式续行本来就是默认值，防的是用户 profile 改过——但我们已经 -NoProfile，此项为防御纵深，可与 W2 合并评估后择一）。
- **W2（做，小）退出码诚实化**：BashCommandOutput 已有 exit_code 字段（bash.rs:329 经 return_code_interpretation bash.rs:699 生成 `exit_code:N` 注记）。补一条约定文档进工具 description：**141/143 类信号退出码提示为「管道下游关闭/超时信号」而非命令逻辑失败**，防模型误判重试。落点 tools/src/lib.rs:141 description。
- **W3（做，小）`2>&1` 指导**：pwsh 7.2+ 下 `2>&1` 语义漂移（不进 $Error）。工具 description 增一句「Windows 下避免依赖 2>&1 解析错误，输出已分字段返回」——heartflow 已分字段捕获（bash.rs:156 双流），模型根本不需要 2>&1。落点同 W2。
- **W4（做，中）nushell 作为一等 shell**：resolve_shell(850) 已支持 HEARTFLOW_SHELL=nu，但参数走 `-lc` 分支（nu 实际用 `-c`）+ 编码包装只认 -Command。补：`lowered.contains("nu")` → `["-c"]`；nu 自带 UTF-8 无需前缀。测试钉死三 shell 的 args 选择。落点 bash.rs:850-864。
- **W5（做，中）LabeledError 式工具错误**：nushell labels[{text,span}] 映射到 heartflow 工具失败输出——失败时附「定位线索」（文件+行+最近似代码段）。已有先例：runtime/file_ops 的 failed_edit_hints_at_the_closest_snippet 测试（编辑失败给最近似 snippet）。推广为工具错误输出通用约定：`{error, hint, at:{path,line}}`。渐进做，不强制全工具。
- **W6（只记录）插件协议 Call-ID 配对 ↔ jobs.rs append_event 事件流**：nushell 用自增 ID 配对请求/响应防乱序；heartflow jobs 事件流是追加式 status 序列，无配对校验。若将来做 MCP 并发调用去耦，Call-ID 模式可直接抄。记入 roadmap 工程稳定性节。
- **W7（否决）结构化对象管道**：pwsh/nushell 式「工具间传对象」对 heartflow 是过度工程——LLM 上下文就是文本，工具边界天然是文本+结构注记（BashCommandOutput JSON）。否决理由：加一层对象总线不减少 token 反增复杂度。
- **W8（否决）-EncodedCommand**：可读性损失 > 转义收益；模型引号错误靠错误信息迭代即可。若未来观察到引号错误率数据再翻案。

## 7. 优先级序（结合 ST-1 当前批次）

1. W2+W3（工具 description 一句话级改动，随本批 commit）
2. ST-1（idle timeout + blocks 落 transcript，独立 commit）
3. W1（pwsh 包装治理，一行 prefix + 测试）
4. W4（nu shell 支持 + 三 shell 参数测试）
5. W5（工具错误结构化 hint，渐进）
6. W6（只记录不执行）

## 8. 与既有文档的关系

- stability.md S1（流中挂断）→ 本批 ST-1；bash 侧 idle watchdog 是同思想在子进程域的镜像（bash.rs:154 注释已点名 codexmon）。
- ai-rust-hygiene.md R 系（字符串嗅探/吞错）→ W1/W2 的本质是「跨进程语义漂移必须收口到唯一边界」，与 classify_stream_error 全仓唯一字符串匹配点同构。
- roadmap §15 工具面上下文预算 → 双通道落盘引用（§5 表）已是实现；W5 错误结构化是其错误域延伸。
