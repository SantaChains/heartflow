# 优化清单：可落地项与可移植范式

> 针对 heartflow（Rust 终端 AI agent）的优化候选与跨项目可移植范式总表。
> 状态：条目均已对照源码核验，附证据行号；未实施。

## 排序规则

三个评分字段，各 1–5 分：

| 字段 | 含义 | 5 分代表 |
|---|---|---|
| 底层度 | 距离硬件/OS/数据结构的深度 | 直抵 CPU 缓存、SIMD、页管理、哈希探测 |
| 重要性 | 对当前真实瓶颈的缓解程度 | 已证实的热点，改完有可见差异 |
| 实践速度 | 落地所需改动量（越大越快） | 单文件级改动，半天可验证 |

排序：**先按「底层度 + 重要性」降序；同分按「实践速度」降序。**

核验口径：只认源码里能指到行号的证据；依赖是否已在 `Cargo.lock` 中也会影响实践速度（已在依赖图中 = 不新增编译单元）。

---

## 实施进度（2026-09-21）

> 只收录**已改代码并跑过测试**的条目，证据为改动后的真实落点。
> 约束：`crates/cli` 正被并行任务改动，且本机 `cargo test` 在其 `build.rs`（winresource 找 `rc.exe`）处阻塞，故本轮改动全部落在 `runtime` / `store` / `api` 三个**无内部依赖**的 crate 内——它们可独立构建与验证，不与拆薄任务抢编译单元。

| # | 条目 | 改动落点 | 测试结果 |
|---|---|---|---|
| 61 | apply_patch 写阶段补 pre-image 补偿 | `runtime/src/file_ops.rs` 写循环：任一文件失败即按逆序回写已写文件，并把回滚结果写进错误消息。**`PreparedFile.original` 本已保存原文**，无需新增读取 | `cargo test -p heartflow-runtime --lib file_ops` → **20 passed**（含 2 条新增回滚用例：多文件失败回滚、回滚删除本批新建文件） |
| 59 | read_file 编码降级链 | `runtime/src/file_ops.rs`：`fs::read` + chardetng 探测 + encoding_rs 解码，替换遇非 UTF-8 直接 Err 的 `fs::read_to_string`；`chardetng`/`encoding_rs` 已在依赖中 | 同上 **20 passed**（含新增非 UTF-8 解码用例） |
| 1 | glob_search 并行化并补齐剪枝 | `runtime/src/file_ops.rs`：`collect_single_glob` / `collect_multi_glob` 改走 `build_search_walker`（`WalkParallel` + `SKIP_DIRS` + `require_git(false)`） | 同上 **20 passed**（含新增 `glob_skips_build_cache_directories`） |
| 4 | `nearest_snippet` 消除每窗口分配 | `runtime/src/file_ops.rs:1156`：`span == 1` 走 `Cow::Borrowed` 零分配快路径，算法不变（仅分配数） | 同上 **20 passed** |
| 21 | grep 每文件分配的削减 | `runtime/src/file_ops.rs` `scan_file`：`files_with_matches`（默认模式）改为单趟 `.count()`，不再为每个匹配建一个 `usize` 索引；`content` 模式仍建索引（上下文窗口需要） | 同上 **20 passed** |
| 43 | 原子写 rename 的 AV 瞬时锁重试 | `runtime/src/session.rs`：新增 `rename_with_transient_retry`，**仅对 `PermissionDenied`** 重试 3 次 × 50ms，其余错误首次即返 | `cargo test -p heartflow-runtime --lib session` → **23 passed**（含 2 条新增用例：正常搬运、真实错误不快重试） |
| 25 | 删除冗余索引 `idx_messages_session` | `store/src/lib.rs`：`SCHEMA_VERSION` 2→3；建表批次内 `DROP INDEX IF EXISTS`（新库不再创建）；**并把 v1 的 ALTER 守卫由 `>= 1` 收紧为 `== 1`**——否则 v2 库会重复加 `pinned` 列而失败 | `cargo test -p heartflow-store` → **9 + 28 + 4 passed**（新增：新库无该索引、v1 迁移后索引消失） |
| 28 | 尊重 `Retry-After` 退避 | `api/src/retry.rs`：新增 `parse_retry_after`（仅 delta-seconds，HTTP-date 显式不支持→回落本地退避）与 `MAX_HONOURED_RETRY_AFTER = 60s`（超出则不睡、直接收敛为错误）；`ApiError::Api` 增 `retry_after` 字段与 `retry_after()` 方法；三处 `expect_success`（client / openai / responses）**在消费 body 前**取头 | `cargo test -p heartflow-api` → **29 + 4 passed**（含 3 条新增用例） |
| 32 | bash 子进程整树收尸 + 防闪窗 | `runtime/src/bash.rs`：超时分支在 `child` 被 drop **之前**调 `kill_process_tree`（Windows `taskkill /F /T /PID`）——`timeout` 的 future 只**借用** `child`，到期被丢弃并不触发 `kill_on_drop`，故进超时分支时父进程仍活着，`/T` 才枚举得到后代；两条 spawn 路径均加 `CREATE_NO_WINDOW`（`0x0800_0000`），无控制台启动时不再闪窗抢焦点 | `cargo test -p heartflow-runtime --lib bash` → **19 passed**。含新增 `timeout_sweeps_descendants`，并用**变异测试**证明其判别力：临时注掉收尸调用后该用例如实失败（`descendant 3972 survived the timeout sweep`），说明泄漏真实存在、测试能捕获 |
| 33 | 子进程环境凭据洗刷 | `runtime/src/bash.rs`：新增 `is_credential_env_name`（后缀形状表 + 精确名，**刻意不含裸 `_KEY`**，以免误伤 `SSH_KEY`/`GPG_KEY` 这类标识符）与 `scrub_credential_env`；两条 spawn 路径均按名剔除。`dangerously_disable_sandbox` 复用为**洗刷的显式退出开关**，因此 `gh pr create` 这类真需要 `GITHUB_TOKEN` 的工作流仍可达 | 同上 **19 passed**（新增：判定表正反用例、端到端「子进程拿不到凭据」、opt-out 保留用例） |
| 35 | token 估算器在线自校准 | `runtime/src/compact.rs` 新增 `TokenCalibration`（仿射最小二乘 + 滚动窗口 16 + 首样本播种截距 + 双向钳位）；`runtime/src/usage.rs` 新增 `TokenUsage::context_input_tokens`（`input_tokens` 不含缓存命中，须加 `cache_creation`/`cache_read`）；`conversation.rs` 在 `record` 点喂入真值：`estimated_tokens` 返回校准值，`raw_estimated_tokens` 供拟合与缓存，二者分离以防因子自我复合 | `-p heartflow-runtime` → **151 passed**（含 8 条新增用例）。**精度已用真实会话实测**，见下节 |
| 64 | 后台任务状态收敛 | `runtime/src/bash.rs`：新增 `BackgroundTaskStatus` / `BackgroundTaskState` 侧写文件（`<log>.status.json`）——spawn 时先落 `running`，再由**两平台共用的一个 detached reaper** 在 `wait()` 返回后覆写 `exited` + `exit_code`/`success`（原 Windows 分支是 `drop(child)`，句柄一关退出码就永远拿不到，这正是缺口）；`BashCommandOutput` 增 `background_status_path` 字段，消息里直接给出该路径；`tempfile_log` 增加 3 天保留期清理（`prune_background_logs`，**只动 `bg-` 前缀自家产物**，外来文件不碰） | `-p heartflow-runtime --lib bash` → **21 passed**（新增：侧写如实报告 exit code 7、保留期清理只删自家产物且放过外来文件） |

跨 crate 回归：`-p heartflow-runtime` → **153 passed / 1 ignored**（ignored 者为新加的手工测量仪器 `tests/calibration_measurement.rs`）；`-p heartflow-provider` → **38 passed**；clippy（`--all-targets -D warnings -A clippy::pedantic`）对 runtime / store / api / provider **全绿**；`cargo fmt -p heartflow-runtime -- --check` 清零；下游 `-p heartflow-tools` `cargo check` 通过（新增字段是纯增量，`BashCommandOutput` 的构造点全仓仍只有 `bash.rs`）。

### 实测：压缩阈值一直在低估真实上下文（#35 的执行期发现）

`list.md` 原把 #35 写成「误差 ±30% → ±5%」。实际拿本机 93 个存档会话（451 个有效轮次、28 个可用会话）实测后，**原始启发式的真实误差是中位 62.4%、系统偏差 −57.5%**——远不止 30%，且方向固定为低估。

拆开看是两个**量级不同、性质不同**的误差源，把它们混成一个乘性因子是初版实现的错误：

| 来源 | 性质 | 实测值 |
|---|---|---|
| 系统提示 + 工具 schema + 逐消息框架 | **加性**（估算器只数 `session.messages`） | 每会话仿射截距中位 **12,329** token，走前式实测 11,632 |
| 字符启发式 vs 真实 BPE | **乘性**（转写本以 JSON/代码为主，4 字符/token 过于乐观） | 斜率中位 **2.23** |

改纯乘性时，因子被迫把固定开销吞进斜率，随提示增长而过度校正并**顶到钳位上限**（实测中位会话在第一个样本就把因子钉到 2.0）。改为仿射 `actual ≈ overhead + density × predicted` 后：

| 方案 | 中位误差 | 平均 | p90 | 偏差 |
|---|---|---|---|---|
| 原始启发式 | 62.4% | 57.5% | 83.4% | **−57.5%** |
| 纯乘性（初版，已废弃） | 31.8% | 41.9% | 75.4% | −16.0% |
| **仿射（已实现）** | **8.9%** | **18.7%** | **35.6%** | **−1.5%** |

**这条为什么是缺陷而非优化**：门限是 `context_window / 2`。偏差 −57.5% 意味着门限在真实提示**已越过整个窗口**之后才会触发——那不是保守的余量，是实打实的越界。修好后偏差降到 −1.5%，门限才回到它声称的位置。

实现细节两处值得记：**首样本播种**把截距一次置为 `actual − predicted`（一个残差无法分离斜率与截距，而截距是更大的那一项），实测把 p90 从 84.4% 压到 53.1%，且是「剔除前三轮」才能达到的水平——即预热尾部基本消失；**双向钳位**（density ∈ [0.5, 3.0]、overhead ∈ [0, 200k]）保证一个坏报告最坏退化为「不比未校准更差」，而非把压缩关掉。

口径差异须知：校准后的 `estimated_tokens` 语义是「预测提供商实际计费的提示大小」，已含固定开销，与 `estimate_session_tokens`（只数消息）不再是同一个量。`cli/tui.rs:1255` 的 `context_budget` 手写了同一个求和式（input + cache_read + cache_creation），可在拆薄收口后改用新的 `TokenUsage::context_input_tokens` 去重。

实施过程中的两处核验修正见「修订说明」末段（#3 目标被证伪、#15 实施点不在本批）。

### 第四批实施（2026-09-21）——cli/tools 编译阻塞解除，#41 + #29 落地

前几批的约束「本机 `cargo test` 在 `crates/cli/build.rs`（winresource 找 `rc.exe`）处阻塞，故只动 runtime/store/api」**已解除**：本轮 `cargo test --workspace` 全绿（cli bin 202 passed），cli 与 tools 两 crate 现可独立构建与验证。据此把可安全落地、确定性强、有真实价值的两条补上——一条在 cli（TUI 渲染质量），一条在 tools（web_fetch cookie 安全语义）。

| # | 条目 | 改动落点 | 测试结果 |
|---|---|---|---|
| 41 | DEC 2026 同步输出 | `cli/src/tui.rs`：新增 `draw_synced<W,F>` helper（`:1634`），用 `queue!(BeginSynchronizedUpdate/EndSynchronizedUpdate)` 把 `terminal.draw` 整帧括起来；`event_loop` 与 `run_one_turn` 两处生产 draw 站点改走它。crossterm 0.28 有这两条命令，ratatui 0.29 的 `CrosstermBackend` 不自动发射（已核） | `-p heartflow` TUI 渲染相关单测通过；新增 `draw_synced_brackets_the_frame_with_dec_2026`：helper 对 writer 泛型，故可用内存 `Vec<u8>` 后端断言 `\e[?2026h` 在 `\e[?2026l` 之前发射（绕开 ratatui unstable 的 `writer()` 访问器）|
| 29 | curl 级 cookie 语义 | `tools/src/web.rs`：`CookieJar` 值类型 `String`→`StoredCookie{value,secure,expires,path}`（自定义 `Deserialize` 用 untagged enum 兼容旧裸串 jar，原地升级不丢）；`parse_set_cookie(raw,now)` 遍历属性段，捕获 `Secure`/`Path`、把 `Max-Age` delta 折成绝对 Unix 过期；`build_cookie_header(jar,host,path,is_secure,now)` 回放前三重过滤；新增 `path_matches`（RFC 6265 `/` 边界前缀）与 `unix_now`（时钟异常 saturating 不 panic）。`Expires` HTTP-date 刻意不解析→按会话 cookie fail-open | `-p heartflow-tools` → **46 passed**（6 条新增：`secure_cookie_withheld_over_plain_http`、`expired_cookie_is_dropped_on_replay`、`path_attribute_gates_replay`、`path_matches_slash_boundary`、`max_age_folds_to_absolute_expiry`、`legacy_bare_string_cookie_upgrades_on_load`）|

同批核验处置（对照源码，非改动）：**#30**（`web.rs:67 .timeout()` 已是整请求总上限）、**#38**（`search.rs:47` <3 码点走 LIKE 回退，CJK 两字查询已正确匹配）判为已具备；**#37** 的终端恢复 panic hook 已做（`tui.rs:1610`）、回合级 `catch_unwind` 未做，标为部分；**#5**（非热点 + 按名缓存陈旧 validator 的正确性风险）、**#55**（rga 缺失时不下发必然失败的工具是刻意设计）判为不做；**#34**（cache_control 属传输层，须真机冒烟）延后。各条备注已在主清单就地更新。

### 第五批实施（2026-09-21）——#58 陈旧图片折叠（含 #42 去重意图）+ 核验 #42/#65 已具备

延续「对照项目实际」核验，本批落地一条、核实两条已具备。

| # | 条目 | 改动落点 | 测试结果 |
|---|---|---|---|
| 58 | Image block 纳入统一裁剪 | `runtime/src/conversation.rs build_replay_messages`：投影原本只折叠超尾的 `ToolResult` 正文，用户内联图片（`ContentBlock::Image`）从不下裁剪——每回合把幸存图片整体重传，正是 `image.rs:59` 自陈的最大重传成本。改为把超尾、未 pin 的图片块折成 `ContentBlock::Text` 占位（「re-attach the file if it is still needed」），与 tool_result stubbing 同构；磁盘转录仍存全字节，pin 消息与 verbatim 尾内的近期图片逐字存活。守卫谓词由「含 ToolResult」扩为「含 ToolResult 或 Image」 | 新增 `replay_folds_old_image_attachments_but_keeps_recent_and_pinned`：断言旧图折成 <200 字符占位、块数守恒（1 图→1 文本）、pin 图与近期图字节不变、原转录不被改动 |

**核实为已具备（不重复投入）.**
- **#42 图片入站降采样**：`tools/src/image.rs:96 shrink_to_vision_grid` 早已把长边 >1568px 的 png/jpeg 用 Lanczos3 缩到网格、JPEG q85 重编码，并保留「原图 vs 重编码」中更小者（`VISION_MAX_EDGE=1568`、`JPEG_QUALITY=85`，比清单设想的 q80 更高），另含 EXIF 方向纠正（`decode_oriented`）。gif/webp 可能带动画，刻意透传不重编码。**降采样部分已完整落地**；「同图去重」的真实形态是「陈旧图片不再每回合重传」——已由本批 #58 的 replay 折叠实现（API 无法「引用」上一轮的图，同轮内重复附件才是唯一可去重面，价值边际）。
- **#65 写类工具回传副作用清单**：成功路径已内建——`apply_patch` 返回 `ApplyPatchOutput{files_changed, results:[{file_path, kind, structured_patch}]}`（每个落盘文件的路径 + create/update + 真实 diff hunk），`write_file`/`edit_file` 各自返回 `file_path`；失败路径由 #61 的回滚错误消息报「rolled back N previously written file(s)」。信息面已闭合，无需另立实现。

**本批核验后仍延后的落地项（理由）.** #20（FTS5 external content：镜像库可删重建、非权威热路径，且属 schema 迁移，无真机跑 store 测试下不宜盲改）、#31（wire 日志：属 api 传输层，AGENTS.md 硬约束改传输须端到端冒烟）、#45（TTFT 可观测：TTFT 半段在流式路径，同需真机）、#44（release.sh 404 重试：发布脚本须本地 dry-run 验证）、#6/#14（bytes 零拷贝 / compact_str SSO：跨多文件的机械改造，收益偏软，不跑构建下编译 churn 风险高）、#60（search_documents 限额/参数面：降 4MB→2MB 无实测可能误伤大归档检索，加 -C 等参数须同步 README/docs/llms.txt/bucket/wiki 文档面）、#56（grep 参数收敛：同属文档面 + 近似破坏性 API 改动）。

---

## 主清单

| # | 项目 | 类型 | 底层度 | 重要性 | 速度 | 落点 / 证据 | 备注 |
|---|---|---|---|---|---|---|---|
| 1 | `glob_search` 并行化并补齐剪枝 | 落地 | 4 | 5 | 4 | `file_ops.rs:591`、`:635` 走串行 `WalkBuilder`；对比 `:941` grep 已用 `WalkParallel` | 且缺 `SKIP_DIRS`(`:154`) 剪枝与 `MAX_SEARCH_FILES`(`:178`) 上限；本仓库 `target/` 有 6.8 万文件 |
| 2 | 自适应双层存储（阈值升级） | 范式 | 5 | 4 | 3 | 记忆条目、会话列表：小数据用连续数组，超阈值升级哈希/跳表 | Redis listpack / intset |
| 3 | `rustc-hash` 替代 SipHash | **待证（目标被证伪）** | 5 | 3 | 5 | ~~`conversation.rs:946` 工具分发表用 `BTreeMap`~~ —— 生产分发是 `tools/src/lib.rs:362` 的 `match name`（编译期跳转表）；那个 `BTreeMap` 属 `StaticToolExecutor`，仅测试使用 | 已暂缓。见修订说明；若要复活需先找到真实热点 |
| 4 | `nearest_snippet` 消除每窗口分配 | 落地 | 5 | 3 | 5 | `file_ops.rs:1051` 的 `window_at` 每次 `join("\n")` 新建 String，最多 5 万窗口 × 2 轮 | 算法已是 coarse-to-fine，问题在分配而非复杂度 |
| 5 | 校验器缓存键改按工具名 | **不做（核验否决）** | 4 | 4 | 5 | `schema.rs:27 compiled_validator` 每次调用 `serde_json::to_string(schema)` 作缓存键；`validate_tool_input` 无 tool_name 参数（调用点 `conversation.rs:683` 有 name 可得） | 否决：① 非真热点（工具调用 IO/网络绑定，键计算淹没在往返里）；② 按名做键有**正确性风险**——MCP 同名工具跨会话 schema 可能不同，按名缓存会命中陈旧 validator。收益软、风险实 |
| 6 | 零拷贝传递 `tool_result` | 落地 | 4 | 4 | 4 | `conversation.rs` 中 `output.clone()`、`message.clone()` 等多处跨 crate 复制 | **`bytes 1.11.1` 已在 `Cargo.lock`** |
| 7 | Windows 目录遍历特化 | 落地 | 5 | 3 | 3 | `FindFirstFileExW` + `FIND_FIRST_EX_LARGE_FETCH`、跳过 reparse point | `ignore` crate 未暴露该 flag；Everything MFT/USN 思路 |
| 8 | 缓存友好布局与数据局部性 | 范式 | 5 | 3 | 3 | 热路径结构体转 SoA、去掉指针跳转 | **需先 profiling 证实**；当前程序偏 IO/网络绑定，勿盲改 |
| 9 | FST 有限状态转换器字典压缩 | 范式 | 5 | 3 | 2 | term 字典比哈希省内存，支持前缀遍历 | 若上 #12 则由 tantivy 内部自带，无须自建 |
| 10 | delta + bitpacking 分块编码 | 范式 | 5 | 3 | 2 | 有序整数序列压缩 | 同 #9，落点依赖 #12 |
| 11 | 代价模型驱动的决策 | 范式 | 5 | 3 | 2 | `compact.rs:13` 压缩触发是固定的「半窗」比例，非成本估算 | 当前启发式已够用，改造成本模型属过度工程 |
| 12 | tantivy 作可选检索后端 | 落地 | 4 | 4 | 2 | 倒排 + BM25 打分，补 FTS5 trigram 无排序的短板 | 需新增依赖 + feature gate；注意与「JSON 权威 + SQLite 镜像」模型冲突 |
| 13 | 会话压缩策略分级 | 落地 | 4 | 4 | 2 | `compact.rs:186` 已按轮次加权分配字符预算 | Lucene TieredMergePolicy：同量级才合并 + 合并预算 |
| 14 | `compact_str` 短字符串 SSO | 落地 | 4 | 3 | 5 | 会话 JSON 中 `role`/`type`/`tool_name` 的短字符串 | **`compact_str 0.8.2` 已在 `Cargo.lock`**，零新依赖 |
| 15 | 工具目录缓存化 | **待 cli 拆薄收口** | 3 | 4 | 5 | `conversation.rs` 与 `cli/tool_exec.rs` 每回合多次重建 spec 并复制 name/description | 实施点属 `cli`（`tool_exec.rs:264` 追加 MCP spec），runtime 无权改装配顺序；能否 `OnceLock` 固化取决于 MCP 连接时序 |
| 16 | `mmap` / `madvise` 策略细化 | 落地 | 5 | 2 | 4 | `lib.rs:113` 已设 `mmap_size=256MB`，但未区分访问模式 | 只读段可用 `MADV_RANDOM` / `MADV_WILLNEED` |
| 17 | 跳表概率平衡 | 范式 | 5 | 2 | 3 | 内存有序结构：范围查询与 rank 场景 | **当前无落点**，仅作范式储备 |
| 18 | 布隆 / ribbon 过滤器 | 范式 | 5 | 2 | 3 | 检索前做「肯定不存在」剪枝 | **当前无落点** |
| 19 | zstd 字典压缩会话快照 | 范式 | 5 | 2 | 3 | 会话 JSON 快照瘦身 | 需新增依赖；磁盘不紧张，收益偏软 |
| 20 | FTS5 改 external content | 落地 | 4 | 3 | 3 | `lib.rs:529` 与 `:539`：`search_text` 在 `messages` 与 `messages_fts` 各存一份 | 消除双份文本存储与写放大；需补齐同步逻辑 |
| 21 | grep 每文件分配的削减 | 落地 | 4 | 3 | 3 | `file_ops.rs:869` 整文件 `fs::read`；`:901` `text.lines().collect()` 建全文件行切片 Vec；`:924` 每条匹配行 `format!` | 且 `files_with_matches` 模式下也全量读，未早退 |
| 22 | 后台子进程剥离重活 | 范式 | 4 | 3 | 2 | 镜像写库、索引重建从回合关键路径移出 | Redis fork RDB / RocksDB WriteStall 思路 |
| 23 | HNSW 近似最近邻 | 范式 | 5 | 2 | 1 | 语义召回 | **当前无场景**，仅备档 |
| 24 | `mimalloc` 运行时参数调优 | 落地 | 4 | 2 | 5 | `main.rs:22` 已是全局分配器 | 仅剩 purge 延迟 / eager commit 等运行时参数 |
| 25 | 删除冗余索引 `idx_messages_session` | 落地 | 3 | 3 | 5 | `lib.rs:538` 的 `session_row` 单列索引，被 `:536` 的 `UNIQUE(session_row, seq)` 隐式索引完全覆盖 | **净删除**：省一棵 B-tree 的每次写维护与缓存占用 |
| 26 | `serde_json::RawValue` 惰性解析 | 落地 | 3 | 3 | 4 | 工具入参在 `converted to Value` 前后被完整解析 | 延迟到真正需要时再解析 |
| 27 | arena / bump 分配 | 落地 | 4 | 2 | 3 | 会话快照序列化路径 | 生命周期同构对象批量释放 |
| 28 | 尊重 `Retry-After` 退避 | 落地 | 2 | 4 | 5 | `retry.rs:61` `backoff_for_attempt` 纯指数(200ms→2s 封顶)、`:112` 按其 sleep，全程不读响应头；`:29` 429/503 已在重试集 | curl `--retry` 会读 `Retry-After`；LLM 限流时服务器已给等待秒数，忽略=重试太早浪费配额或盲等 |
| 29 | curl 级 cookie 语义(Secure/Expires/Path) | **已实施** | 2 | 3 | 3 | `web.rs` `StoredCookie{value,secure,expires,path}` + 自定义 Deserialize（旧裸串 jar 原地升级）；`parse_set_cookie(raw,now)` 折 `Max-Age`→绝对 Unix 过期、捕获 `Secure`/`Path`；`build_cookie_header(jar,host,path,is_secure,now)` 三重过滤（Secure 走 http 不回放、过期不回放、Path 按 `/` 边界匹配）；`Expires` HTTP-date 刻意不解析→会话 cookie fail-open（同 `retry.rs` 拒手搓日期数学） | `-p heartflow-tools` → **46 passed**（6 条新增：Secure/过期/Path 边界/Max-Age 折算/旧格式反序列化/path_matches）|
| 30 | web_fetch 非流式总时长上限 | **已具备** | 2 | 2 | 5 | `web.rs:67 .timeout(REQUEST_TIMEOUT=30s)` —— reqwest blocking 的 `.timeout()` 是**整请求总时长**上限（含 body 读），正是本条所求；非流式路径已封顶 | 原备注「无总 cap」是误读：`.timeout()` 即 `--max-time` 等价物。SSE 走 api crate 不设总上限是对的(长流合法) |
| 31 | curl `-v` 式 HTTP wire 日志 | 落地 | 1 | 2 | 4 | `retry.rs:17` `build_http` 与 `web.rs:66` client 均无请求级 trace；现仅 `HEARTFLOW_LOG` 粗粒度 | 排障 API/MCP 故障免抓包；只打请求行/状态/耗时，不打 body 与凭据 |
| 32 | bash 子进程整树收尸 + 防闪窗 | **已实施（改走安全路径）** | 4 | 5 | 4 | `bash.rs` 仅 `kill_on_drop(true)`，`pwsh -Command` 的孙进程杀不掉存活 | ~~`CreateJobObjectW`~~ **被 `unsafe_code = "forbid"` 否决**（forbid 无法被局部 allow 覆盖）。已落地：`CREATE_NO_WINDOW` + 超时瞬间 `taskkill /F /T`。性质差别已在实施进度章标注 |
| 33 | 子进程环境凭据洗刷 | **已实施** | 3 | 5 | 5 | 全仓 `crates/` grep 无 `env_remove`；bash 子进程继承 `ANTHROPIC_API_KEY`，`printenv` 即明文外发——save 期 redact 护磁盘不护出口 | ~~与 `SECRETS` 注册表同源~~ 不可行：registry 在 cli、`runtime` 在其下层看不见。改用**名字形状表**（后缀 + 精确名），并以 `dangerously_disable_sandbox` 作显式退出开关 |
| 34 | Anthropic cache_control 前缀缓存 | 落地（**延后**） | 2 | 5 | 3 | system+工具规格+最早稳定前缀打断点；现有 replay 投影已天然保证前缀稳定 | 全项目最大单点金钱收益，但属**传输层**改动，AGENTS.md 硬约束「改流式/工具往返必须真机冒烟」——SSE/请求重排的真实行为只有真服务器能暴露。当前无真机运行条件，延后至可端到端冒烟时做 |
| 35 | token 估算器在线自校准 | **已实施** | 2 | 4 | 4 | `compact.rs` 码点启发式(ASCII/4)；每回合回传的 `usage` 未被利用 | ~~EMA 喂分母，误差 ±30%→±5%~~ 实测后改写：真实误差中位 **62.4%**、偏差 **−57.5%**，且主项是**加性**固定开销（截距中位 12.3k）而非乘性密度。改为**仿射最小二乘**，实测中位降至 **8.9%**、偏差 **−1.5%** |
| 36 | Image block 计入 token 估算 | **已具备** | 2 | 3 | 5 | ~~`estimate_tokens_from` 只扫文本，`ContentBlock::Image` 零计费~~ —— 误读。`compact.rs:6` 与 `:305` 早已按 1,500 计费 | 仅剩「固定 1500 未按像素推导」的精度微调，不立条目 |
| 37 | 回合级 catch_unwind + 终端恢复 panic hook | **部分已具备** | 3 | 4 | 4 | panic hook 已做：`tui.rs:1610-1615` `take_hook`+`set_hook`，panic 时 `disable_raw_mode`+`LeaveAlternateScreen` 后再转原 hook——终端不卡在 raw/alt-screen。**回合级 `catch_unwind`→报 Error 回合→继续 未做**（全仓 grep 无 `catch_unwind`） | 终端恢复面已闭合；剩余「捕获后不杀 REPL、继续下一回合」需真机验证交互行为，延后 |
| 38 | CJK bigram 影子列（FTS 短词） | **已具备（LIKE 回退）** | 3 | 4 | 3 | `store/src/search.rs:47 choose_method`：<3 码点走 `LIKE`，≥3 走 FTS trigram；`:80` 注释「so short Chinese terms still match」 | 中文两字查询已由 LIKE 回退正确覆盖（无空结果）。bigram 影子列只是 MB 级历史上的**性能**优化，非正确性缺口，不做 |
| 39 | redact 触发字节预筛 | 落地 | 3 | 2 | 5 | `redact.rs` 每次 save 全量 Text ×4 regex 扫描 | 先 memchr 触发字节(`:` `/` `@` `s` `g` `A`)短路；诚实注：regex 自带前缀预筛，实测后再定 |
| 40 | syntect bincode dump 预编译 | 落地 | 2 | 3 | 4 | render.rs 的 syntect 从目录加载(.pack plist 解析) | `dump_to_binary` 加载快 10–100×，或首帧后异步预热 |
| 41 | DEC 2026 同步输出 | **已实施** | 2 | 3 | 5 | `tui.rs:1634 draw_synced`：`queue!(BeginSynchronizedUpdate)` → `terminal.draw` → `queue!(EndSynchronizedUpdate)` → `flush`；两处生产 draw 站点（event_loop / run_one_turn）改走它。ratatui 0.29 `CrosstermBackend` 不自发该序列（已核） | helper 对 writer 泛型故字节序列可单测：`draw_synced_brackets_the_frame_with_dec_2026` 断言 `\e[?2026h` 在 `\e[?2026l` 之前。不支持 DEC 2026 的终端忽略该对转义（安全 no-op）|
| 42 | 图片入站降采样 + 同图去重 | **已具备（降采样）+ #58（去重意图）** | 2 | 3 | 4 | `image.rs:96 shrink_to_vision_grid`：长边 >1568px 的 png/jpeg 用 Lanczos3 缩到 `VISION_MAX_EDGE=1568` + JPEG q85，保留原图/重编码中更小者，含 EXIF 方向纠正；gif/webp 透传（可能带动画）。「同图去重」真实形态＝陈旧图不再每回合重传 | 降采样已完整落地（比清单设想的 q80 更高）；重传成本由 #58 的 replay 折叠消除。同轮内重复附件是唯一可去重面，价值边际 |
| 43 | 原子写 rename 的 AV 瞬时锁重试 | 落地 | 2 | 3 | 5 | `session.rs save_to_path` temp+rename | Defender 实时扫描可令 rename 瞬时 ACCESS_DENIED；3 次 ×50ms |
| 44 | crates.io 传播 404 重试 | 落地 | 1 | 3 | 5 | `scripts/release.sh` publish 循环 | 依赖刚发布即发依赖方会 "no package named"(非 429)，单独 sleep+retry 分支 |
| 45 | 启动与 TTFT 可观测性 | 落地 | 1 | 2 | 4 | tracing 无请求级计时 | `HF_PROFILE` 打印 started-in-Xms + 每回合 TTFT 入 usage；防启动回归 |
| 46 | 流式重试只在首字节前 | **已具备** | 2 | 4 | 5 | 核验（2026-09-21）：`api/retry.rs:95-100` 的 `check` 只判状态码即返回 `Ok(response)`，**body 不在重试闭包内被消费**；`client.rs:87` 只包住建连；`provider/adapter.rs:246` 收到内容后 `keep what arrived`，仅在「无内容到达」时回退非流式 | 中途断流不会重发整请求，「重试导致内容重复」的隐患不存在，无须实施 |
| 47 | file_ops symlink 逃逸专项审计 | 审计 | 3 | 4 | 3 | workspace-write 下 read/write/edit/apply_patch 的路径解析链（未核实现状） | canonicalize 后须仍在 cwd 前缀内；符号链接指向仓外即越权，需专项逐工具验 |
| 51 | 加权 / Damerau-Levenshtein 编辑匹配 | 范式 | 4 | 3 | 4 | `file_ops.rs:1051` → `:1062` 精算用普通 normalized-levenshtein：无加权、无换位 | STOC 2025 加权有界编辑距离最优算法（Gorbachev & Kociumaka）。`fn`↔`nf` 换位当前计 2 次编辑，Damerau 计 1 次。**先去分配（#4）再换距离函数** |
| 53 | 前缀复用点由执行语义预测 | 落地 | 2 | 5 | 3 | 与 #34 同落点：`api` 请求组装处的 `cache_control` 断点位置 | CacheScout 范式。#34 只取「静态最稳前缀」，本条让断点随工具调用分布 / 轮次动态选取。**前置：#34 须先落地** |
| 50 | 扫描预算按选择率自适应 | 落地 | 2 | 3 | 3 | `file_ops.rs:178` `MAX_SEARCH_FILES` 为固定常数；`:154` `SKIP_DIRS` 为固定集合 | learned pruning（HELMSON, OSDI'26；lakehouse arXiv 2608.05441「剪枝前置」）：先由模式选择率估应扫规模，再决定是否截断，替换固定上限。**与 #1 同文件，宜合并实施** |
| 48 | 学习增强式压缩策略（预测 + 鲁棒回退双保证） | 范式 | 2 | 3 | 2 | `compact.rs:13` 压缩触发阈值为固定半窗比例，不含任何预测 | learning-augmented（arXiv 2606.01342 相对预测预算，达成 H_k + O(1) 双保证）；预测源可用每回合回传的 `usage`（与 #35 同源）。与 #11 的区别：**不建代价模型，只加可验证预测 + 退化闸** |
| 52 | AI 自优化回路（确定性 bench + 参数进化） | 范式 | 1 | 4 | 1 | 无现成 harness；候选参数见 #1 并发批大小、#11 半窗比例、#50 扫描上限 | AlphaEvolve 方法论（种群 → LLM 语义级变异 → 确定性评估器打分 → 择优作父本）；开源同谱系 OpenEvolve / ShinkaEvolve / DeepEvolve。**前置是 bench，不是 LLM** |
| 49 | 缓存替换策略避开 LRU | 范式 | 2 | 2 | 3 | 当前无缓存层 —— **属预先避坑，非修复** | SOLAR（arXiv 2607.00394）：在 agent 记忆缓冲上 LRU/LFU/ARC 一致输给朴素 FIFO，因语义负载既无时间局部性也无频率集中；其解为 regret 累积 + 贝叶斯在线学习决定替换谁，竞争比 ≤3 |
| 61 | apply_patch 写阶段补 pre-image 补偿 | 落地 | 4 | 4 | 3 | `file_ops.rs:370-418` 先在内存逐条校验全部 `old_string`，通过后才按序写（`:420-440`）；`:427` 任一文件失败直接 Err 且**已写文件不回滚**；单文件 temp+rename 原子（`:461`） | 最坏形态＝前 N 个已改、第 N+1 个失败。模型收到 Err 但**不知道哪些已落盘**，会基于错误前提重试（`old_string` 对已改文件已不成立）。正解：改前记 pre-image，失败按逆序回写——saga 缺失的标准症状，全批次最高优先 |
| 59 | read_file 编码降级链 | 落地 | 3 | 4 | 4 | `file_ops.rs:231` `fs::read_to_string` 遇非 UTF-8 直接 Err，无 UTF-16/GBK 探测 | `bash.rs:299-316` 已用 chardetng 做同一件事，实现可直接搬。中文 Windows 下 GBK 文件与 PowerShell 的 UTF-16 输出是日常输入，当前等于完全读不到 |
| 54 | 工具 schema 选择性按需加载（deferred） | 落地 | 3 | 3 | 2 | `cli/src/main.rs:3424` `ToolExecutor::specs`；15 条 description 合计 3,784 字符 + schema ~4.3k ≈ 8k 字符（2,000–2,600 tokens/回合，且每回合重复） | 只对低频四工具（verify_graphics / generate_image / todo_write / search_documents）deferred。**全量 deferred 会让高频工具多一轮往返，净亏**。理论：Anthropic Tool Search Tool 最高 85% 削减；Cloudflare Code Mode 把 2,500+ endpoint 压成 2 tools |
| 58 | Image block 纳入统一裁剪 | **已实施** | 2 | 3 | 4 | `conversation.rs build_replay_messages`：把超 verbatim 尾、未 pin 的 `ContentBlock::Image` 折成 `Text` 占位，与 tool_result stubbing 同构；守卫谓词扩为「含 ToolResult 或 Image」；磁盘存全字节、pin 与近期图逐字存活 | 消除「每回合重传所有幸存图片」这一全仓唯一无界的注入路径。新增 `replay_folds_old_image_attachments_but_keeps_recent_and_pinned` 覆盖折叠/pin/近期/块数守恒/原转录不变 |
| 62 | 工具级审计通道 | 落地 | 2 | 3 | 3 | `tools/` 与 `conversation.rs` 均无 tracing，仅 `usage_tracker.record`（`conversation.rs:462`）；`session.rs:36-37/371` 的 JSONL 是**会话通道**——会被压缩、会被 stub 化、随会话删除，不能替代审计 | 最小字段集：tool_name / redacted_args_digest / duration_ms / status / side_effect_class / bytes_out，走 `HEARTFLOW_LOG` 不进上下文。**脱敏须与 #33 的 SECRETS 注册表同源**，否则审计通道自身成为泄漏面 |
| 63 | side_effect_class 三分级 | 落地 | 2 | 3 | 3 | `main.rs:3462` `is_concurrent_safe` 只分「只读可并行 / 写类串行」二元 | 生产级工具 API 标准为 read / write / destructive 三类。现 bash 的一条 `rm -rf` 与 `write_file` 同档，`main.rs:3728 permission_policy_for_mode` 无法按动作性质区分，只能拦工具名 |
| 64 | 后台任务状态收敛 | **已实施** | 3 | 2 | 3 | 原状：`bash.rs:56` → `:110-172` 只返回 pid + 临时日志路径，靠 `read_file` 读日志、`bash kill` 停；**无「是否结束 / 退出码」查询**，`tempfile_log()` 无清理策略 | 已落地：终止状态侧写 `<log>.status.json`（`running` → `exited` + exit code）+ 3 天保留期清理。关键点：Windows 分支原本 `drop(child)`，句柄一关退出码即丢失，故**必须**保留 reaper——两平台因此合并成同一份 `wait()` 线程 |
| 65 | 写类工具回传副作用清单 | **已具备** | 2 | 3 | 3 | 成功路径：`apply_patch` 返回 `results:[{file_path,kind,structured_patch}]`、`write_file`/`edit_file` 各返回 `file_path`；失败路径：#61 回滚错误消息报「rolled back N previously written file(s)」 | 信息面已内建于结构化输出，无需另立实现。与 #61（补偿面）合起来即「重试前能确认前提」 |
| 66 | 纯函数契约与 panic 自由性验证 | 审计 | 2 | 3 | 2 | 落点：`file_ops.rs` 窗口索引（`nearest_snippet` 边界）、`bash.rs` 字节计数、`compact.rs` 字符预算——off-by-one 高发区，且均为**纯计算** | Kani 0.65 `--prove-safety-only` / 0.66 循环不变式；AWS Autoharness 在 MIR 层自动生成 harness（16,748 个 / 11,970 通过，约 15 个月人工成果的 10 倍）；contracts 已成为 Rust 实验性语言特性。**本仓 `unsafe_code = "forbid"`，Kani 主打的 unsafe 检查用不上**；可用的是 panic 自由性与函数契约。瓶颈：std 建模不全（碰 `HashMap`/`Mutex`/文件 I/O 即卡求解），只能覆盖切出来的纯函数，不是整个 crate |
| 55 | search_documents 常驻化 + rga 缺失降级 | **刻意不做** | 1 | 3 | 5 | `tool_exec.rs:110` 仅当 `rga_available()`（`doc_search.rs:124`）为真才注册 | 现状是**刻意设计**：无 rga 时不下发一个必然失败的工具，避免污染工具表与误导模型。原条目主张「常驻 + 结构化缺失提示」，但那会把一个不可用工具塞进每回合 schema（token 成本）；权衡后维持条件下发 |
| 57 | 搜索类四工具边界审计 | 审计 | 1 | 2 | 5 | glob_search（路径通配）/ search_files（nucleo 模糊路径）/ grep_search（内容正则）/ search_documents（归档内文本）职责实为清晰四分，**非功能冗余** | 「重叠工具是推理税」：description 须显式写出「何时用 A 而非 B」。纯文档改动，零风险 |
| 60 | search_documents 限额与其他工具对齐 | 落地 | 1 | 2 | 5 | `doc_search.rs:19-23`：4MB 输出 / 30s 超时；对比 `read_file` 2MB、`web_fetch` 文本仅 2 万字符 | 结果质量最低的工具反而拿到最大额度（无上下文行、无排序）。另其参数面（`lib.rs:234`）缺 `-B/-A/-C`、`type`、`-n`，而归档内检索恰最需要上下文行 |
| 56 | grep_search 参数收敛至 ≤8 | 落地 | 1 | 2 | 4 | `lib.rs:199` 起共 13 个参数；其中 `-B/-A/-C` 短名与 `context` 长名语义重叠，属同义参数并存 | AWS Prescriptive Guidance 建议工具参数 ≤8。`output_mode` 已用 enum 消歧是对的方向，应延续并择一保留上下文参数 |

---

## 已具备，勿重复投入

以下曾被列为优化项，核验后确认已实现：

| 项目 | 证据 | 结论 |
|---|---|---|
| grep / 模糊检索的并行遍历 | `file_ops.rs:941` `build_search_walker` 返回 `ignore::WalkParallel`；`:759` 整条「过滤→读→嗅探→正则」在 worker 线程跑，仅每文件触碰一次共享 sink；`:958` 模糊检索同样并行 | 已是 ripgrep 同款架构，不必再"引入并行遍历" |
| store 写入批事务 | `lib.rs:148`、`:194`、`:335` 均为显式 `conn.transaction()`；`:460` 复用 prepared statement，`:498` FTS 二次通过 | 不存在逐条 fsync 问题 |
| 全局分配器 | `main.rs:22` `#[global_allocator] mimalloc::MiMalloc` | 已是 mimalloc |
| SQLite 调优 | `lib.rs:110-120`：`cache_size=-8000`、`mmap_size=256MB`、`temp_store=MEMORY`、WAL + `synchronous=NORMAL`、`busy_timeout=5000`；`:367` 关闭前 `PRAGMA optimize` + `wal_checkpoint(TRUNCATE)` | pragma 层已吃满，继续深挖边际收益极低 |
| 外部工具探测缓存 | `doc_search.rs:124` `rga_available()` 用 `OnceLock` 进程级缓存 | 无每回合子进程探测 |
| 编辑距离提示的两段式 | `file_ops.rs:1053` 粗筛 jaro-winkler → `:1062` 前 10 名精算 normalized-levenshtein，且 `:1039` 有 5 万窗口上限 | 已是「粗筛 + 精验」，无须替换算法 |
| 崩溃恢复幂等 | JSON 权威 + 原子 temp+rename；SQLite 事务 | 已满足 |
| 全局单遍扫描 | 压缩、token 估算、grep 均为单趟 | 已满足 |
| curl 式 redirect 上限 + 每跳 SSRF 复查 | `web.rs:68` `redirect::Policy::limited(5)`(≈`--max-redirs`)；`:90` redirect 后复查最终 URL 防落到内网 | 已是 curl 逐跳重验证纪律，勿再“引入 redirect 安全” |
| curl 式分层超时 | `retry.rs:6` CONNECT 15s + `:8` READ 300s(≈`--connect-timeout`+读上限) | 已具备 |
| curl 式 gzip 解压 | `api/Cargo.toml:19` reqwest `gzip` feature(≈`--compressed`) | 已具备 |
| curl 式幂等感知重试 | MCP 握手幂等重试 3 次、工具调用非幂等不重试(与 curl 默认只重试安全方法同哲学) | 已对齐，勿改成无脑全重试 |
| curl 式 cookie jar 持久 | `web.rs:75/165/172` opt-in 落盘 jar(≈`-b/-c`)；#29 已补齐 Secure/Path/Max-Age 语义 | 思路已具备，语义严谨度差距已由 #29 闭合 |
| redact 增量半路 | `redact.rs` `redact_messages`(切片级) | 已实现；剩预筛见 #39 |
| 就地压缩 / replay 投影 / pin 免疫 | `compact.rs compact_session_in_place`、conversation.rs 投影、`pinned` | 上下文域已近顶级，勿再堆活 |
| 发布链：SHA256SUMS + SLSA attest + 工具链单一源 | `release.yml` windows-zip（attest-build-provenance@v2、`rustup show` 认 rust-toolchain.toml） | 已落地 |
| SSE 每 chunk idle timeout | `retry.rs:8` READ 300s 本就是 per-read(reqwest read_timeout 语义) | 「防长 thinking 误杀」调研项到场即满足 |
| Image block 计入 token 估算（原 #36） | `compact.rs:6` `IMAGE_TOKEN_ESTIMATE = 1_500`；`:305` `ContentBlock::Image { .. } => IMAGE_TOKEN_ESTIMATE`，注释还写明「按解码位图计费、不用 base64 长度」，`git show HEAD:` 与 `git diff` 双重确认它早于本轮存在 | 原条目「Image 零计费」是误读。真实缺口仅剩「固定 1500 未按像素推导」，属精度微调，不立条目 |
| 流式中途断流不会重发整请求（原 #46） | `api/retry.rs:95-100` `check` 只判状态码即返回 `Ok(response)`，body 不在重试闭包内；`client.rs:87` 只包住建连；`provider/adapter.rs:246` 收到内容后保留已收内容 | 「重试导致内容重复」前提不成立，无须实施 |

---

## 明确不做 / 不适用

| 项目 | 理由 |
|---|---|
| 自建存储引擎替换 rusqlite | B-Tree、页管理、崩溃恢复已调优；自研需 3–6 个月且无收益 |
| 手写 SIMD intrinsic | `regex` crate 内部已对字面量前缀做 memchr / aho-corasick 预筛；`globset` 内部已有 aho-corasick（`file_ops.rs:609` 注释自陈）；`aho-corasick 1.1.4`、`memchr 2.8.0` 已在依赖图中 |
| 引入向量检索（HNSW） | 语义召回是需求驱动而非性能驱动，当前无该场景 |
| false sharing 消除 | 遍历已是线程本地累积，无共享写热点 |
| 分片并行 / 增量哈希 / 字符串驻留 | 无落点；字符串驻留与 #14 的 SSO 重复 |
| HTTP/3 / QUIC、curl `-Z` 并行传输 | 超范围；heartflow 单连接顺序请求足够，并行传输无场景 |
| Netscape cookie 文件格式、curl 进度条 | 互操作/装饰需求不存在；现有 JSON jar 够用(语义加固见 #29) |
| io_uring | Linux 专属；主平台 Windows，tokio + 现有 pragma 即正解 |
| 沙箱(bash 隔离) | 用户明令不做；权限模式约束维持现状 |

### 一处刻意的工程决策：S4 暂不抽

> 语境补充：S1–S7 是 cli 拆薄的分步序列（完整序列见 `openmemory.md:732` 与 `:773`），S4 指其中「切出 repl 模块（`run_repl`、各 `handle_*_command`、`print_repl_help` 与状态打印）」这一步。

按你的标准权衡后，我没有把 run_repl 那块 ~1100 行抽出到 repl.rs。理由：它是纯代码搬迁，对长期收益/性能/稳定零增益（只降 main.rs 行数观感），却是所有抽取里风险最高的一块——交互式 REPL 主循环是最核心、最难测的路径（无 golden 台架、需真终端验证行为），而我当前无法真机运行。在“稳定优先”下，为行数美观去搬动最不可验证的中枢不划算。这不是“没有底层办法才回退”，而是明确的成本/收益判断。若你之后能真机验证，S4 可低风险补做（套路与前六模块一致）。

### 另一处刻意的工程决策：TUI 斜杠派发暂不盲改

> 语境：cli 拆薄把交互面分成双载体——默认阻断式 REPL（已验）与 `HEARTFLOW_TUI=1` 启的全屏 ratatui。二者共用 `run_repl` 入口，但在 `:728` 的门处分叉。**剩余唯一实质缺口是全屏 TUI 不派发斜杠命令**：每条提交都当模型回合跑。

**核实结论（有行号）.** `main.rs:728` 的 `HEARTFLOW_TUI=1` 门在 `:778` 进 `tui::run_shell` 后于 `:779` 直接 `return Ok(false)`，**绕过了 `:836-849` 的 `/` 分支**；`dispatch_slash_command`（`:950`）那 20 个 arm 只在落空路径（默认 REPL）上可达。TUI 侧 `tui.rs:1032` 的注释自陈了这一取舍——「the shell runs every submission as a turn and dispatches none」，`:1926` 每条提交一律走 `runtime.run_turn_with_blocks`。

**判定：这是对主交互面的大规模侵入式重构，不宜在当前无真机验证下盲做。** 三条理由：

1. **爆炸半径大.** `dispatch_slash_command` 的 ~20 个分支（`:950-1063`）输出全部经 `println!` 打到 stdout；而 TUI 是 alt-screen ratatui，每个 handler 的输出汇都要改道进 transcript。
2. **副作用与事件循环模型不兼容.** `/restart` 需 `confirm_restart()` 交互确认并回 `LoopControl::Restart`（`:1009-1016`）；`/plan` 需 `prompter` 交互（`:1050-1054`）；`/exit` 带 resume 提示（`:965-968`）——都与 ratatui 的 actor/事件循环模型冲突。
3. **只有真机冒烟能验.** alt-screen 下的交互行为无法靠单测覆盖，而当前不具备真机运行条件；盲改会在主交互路径上埋回归。

**已有安全兜底（故这是载体选择，不是功能缺口）.**
- 默认 REPL 斜杠支持完整：补全菜单在 `editor.rs:10/38/116`，派发覆盖全量 arm。
- TUI 是 `HEARTFLOW_TUI=1` 的**可选**载体，非默认路径。
- **补全菜单不可脱离派发单独做**：补全出的命令仍会被当回合发给模型（`tui.rs:1926`），只会制造「命令可用」的错觉，比不提供更误导——**无意义**。

**若日后要补（前置：真机可验证）.** 最小侵入路径分两步，不要一次搬完所有 arm：① 给 TUI 输入层加 `/` 前缀识别，只把**纯输出型只读 arm**（`/help`/`/status`/`/sessions`/`/mcp` 等无副作用者）改道进 transcript；② 带交互/副作用的（`/restart`/`/plan`/`/exit`/`/model`/`/mode`/`/clear`）继续留给 REPL，或单独设计 TUI 原生对话框。

---

## 猜想与测试

> **与主清单的区别。** 主清单每条的现状都能指到行号，即「已知」；本模块是**可证伪但尚无测量数据**的命题。
> 每个猜想先给测量设计、再给证伪条件——设计的目的是能杀掉自己的猜想，不是证明它。
> 仪器落点已核到行号；**实现与执行全部未做**（需 cargo，当前与并行任务互斥）。
> 全仓无任何 bench / criterion 台架（已核：各 `Cargo.toml` 无 `bench`/`criterion` 条目），故所有 harness 都要自建；唯一现成的确定性回放钩子是 `conversation.rs:73`（"Replay a fixed event sequence; used by tests and scripted providers"）。

### 测试纪律（适用于以下全部猜想）

1. **先测前提，再谈改造。** 第一阶段只回答「这个问题存在吗」。前提不成立即停止，不写实现。
2. **确定性指标优先于统计指标。** 能用差分测试或不变量断言的，不引入统计 A/B。
3. **本地耗时与网络耗时分开计时。** 涉及模型调用的路径，本地计算段与往返段必须分列，否则测到的是网络抖动。
4. **报分位数不报均值。** 每种输入重复 R ≥ 30，报 p50 / p95；触发型路径额外报尖峰比（p95/p50）。
5. **负结果同等记录。** 被证伪的猜想进文末「已裁定」栏，不许只留正结果。

### 猜想速览

| 编号 | 猜想 | 理论依据 | 校准的主清单条目 | 依赖 | 测试成本 | 执行顺序 |
|---|---|---|---|---|---|---|
| C3 | effect 声明可在装配期捕获权限漏声明 | effect system + capability-based security 的静态变体 | #63 / #61 / #62 | 无 | 低（确定性，无统计） | 1 |
| C2 | 固定扫描上限造成可测量的召回损失 | learned pruning（HELMSON, OSDI'26）；FM-Index backward search | #50 / #1 | 计数器埋点 | 低（可离线） | 2 |
| C5 | apply_patch 半应用状态的实际频率足够高 | —（观察性，无理论依据，只校准评分） | #61（只校准评分） | 计数器埋点 | 低（观察性） | 3 |
| C1 | 压缩摊还成本随会话年龄增长，且触发回合有尖峰 | IVM / DBSP 双线性恒等式 + arrangement | #13 / #11 / #48 | 合成回放台架 | 中 | 4 |
| C4 | 次模最大化优于现预算分配启发式 | 次模函数最大化，(1−1/e) 近似保证 | #13 | **前置未解决** | 高 | 5（阻塞） |

执行顺序按「能杀掉猜想的强度 ÷ 成本」排：C3 给硬结论且无统计成分，先行；C4 的前置（「信息覆盖」代理定义）未解决前不可启动。

### 理论依据与来源

**IVM / DBSP（C1 的出处）.** 双线性恒等式 `Δ(A⋈B) = ΔA⋈B_old + A_old⋈ΔB + ΔA⋈ΔB`，配 arrangement 物化中间状态供探测；理论增量化提速 O(|DB| / |ΔDB|)。

**甄别结论（防白做功）.** 回放投影本来就是增量的（只追加消息而已），**不需要改造**；全量重算只发生在压缩路径。故 DBSP 在本项目的价值集中在压缩（C1）与 P5 共享记忆库两处，不是全域改造。

**FM-Index（C2 第二阶段的出处）.** Infini-gram mini（UW/Ai2，EMNLP 2025 Best Paper）把 83TB 文本索引到 0.44× 语料；Lance 格式 2026-06-01 投票通过纳入 FM-Index（PR #7026），实测 100K 源码文件 / 1.59GB 文本 → 索引 1,513MB（0.95×），medium query 29ms 对 n-gram 的 480ms（约 17×）；Dynatrace 另有开源 `index4j` 可参考。

**甄别结论.** **空间优势本项目用不上**——FTS5 trigram 已覆盖「任意子串匹配 + 无假阳性」，且会话历史只有 MB 级。真正的增量价值在 `O(m)` 计数：backward search 不 locate 就能给出命中总数，**这正是 C2 第二阶段缺的那个原语**。

**Rust 形式化验证（有落点，已入主清单）.** 见 #66。Kani 0.65 加 `--prove-safety-only`，0.66 支持 while-let/for 的循环不变式；AWS Autoharness 在 MIR 层自动生成 proof harness，产出 16,748 个（11,970 验证通过），约为 15 个月人工成果的十倍；Rust 标准库验证竞赛已有 450+ PR、21+ 外部贡献者；**contracts 已成为 Rust 实验性语言特性**。

**纠正一个直觉.** 本仓 `unsafe_code = "forbid"`，**Kani 最主打的 unsafe 检查用不上**；可用的是 panic 自由性与函数契约。瓶颈如实说：std 建模不完整，函数一旦碰 `HashMap` / `Mutex` / 文件 I/O 就会卡住求解，所以只能覆盖被切出来的纯函数，不是整个 crate。

**同批经核验无落点（不入表、不占编号）.** STOC 2026 / SODA 2026 批次——负权最短路 O(mn^0.7193)（Quanrud & Tajkhorshid）、有向边/点连通性几乎线性近似、流式 max dicut 的 (1/2−ε) 紧界、Online Orthogonal Vectors 确定性下界、HDX 上的局部列表可解码码：全是图算法与编码理论，本项目无对应结构。早前批次的同类三条（时空模拟、四色定理、SPAA'26 竞争下原语）见修订说明。

---

### C3 · 工具 effect 类型系统能否替代人肉纪律

**命题.** 给 `ToolSpec` 增 `effects: {Read, Write, Delete, Network, Exec}`，在装配期断言「每个工具至少被一个权限模式覆盖、每个 effect 都有消费者」，可在启动期捕获权限漏声明；当前漏声明静默通过。

**测法：变异测试 + 不变量测试。** 五条里唯一能给出「检出率 = 100% / 0%」硬结论的一条。

| 项 | 内容 |
|---|---|
| 装配点 | `main.rs:3424 ToolExecutor::specs`——15 个工具的组装处，也是断言的唯一落点 |
| 变异集 | 15 个工具 × 2 种篡改（删声明 / 改为错误 effect）= 30 个变异体，逐个跑启动自检 |
| 主判据 | 检出率 **100%**；当前基线 **0%**（无断言，静默通过） |
| 不变量测试（关键） | 由 effect 表推出的「模式 × 工具」允许矩阵，须与 `main.rs:3462 is_concurrent_safe` + `main.rs:3728 permission_policy_for_mode` 的实际行为**逐项一致**。不一致即断言无效，或策略里有未被文档化的分支 |
| 边界测试（必打） | `bash` 的 effect 取决于参数（`ls` 只读，`rm -rf` 破坏性），静态声明只能给出上界。在 read-only 模式下跑 20 条只读命令 + 20 条写命令，期望「只读全放行、写类全拦」 |

**证伪条件.** 若 `bash` 因声明了 `Write` 而拦掉 `ls`，则该设计在 read-only 档不可用，需退化为「参数级 effect 解析」或「命令 allowlist」。**这是该设想唯一的真实弱点，故测试优先打这里，而不是先测检出率。**

**执行前置（环境，非设计）.** 本机 `cargo test` 阻塞在 `crates/cli/build.rs:6` 的 winresource/`rc.exe`（工作日志有记）。变异体跑启动自检属 cli crate，故 C3 的验证需先解决该环境或换机器；在此之前它只是设计。

---

### C2 · 固定扫描上限是否真的造成召回损失

**命题.** `MAX_SEARCH_FILES = 20_000`（`file_ops.rs:178`）对真实仓库构成可测量召回损失（≥ 5% 命中被丢弃）。

**反命题（必须同时接受被证伪）.** 因 `SKIP_DIRS`（`:154`）已剪掉 `target`/`node_modules`/`.git` 等，多数仓库实际文件数低于上限，**上限从不生效**。

**第一阶段：测前提（这一步的结果决定 #50 的存废）.**

| 项 | 内容 |
|---|---|
| 仪器 | 三个上限各埋一个「是否绑定」标记：`MAX_SEARCH_FILES`（`:178`）、`MAX_GREP_CONTENT_LINES`（`:176`）、返回条数 `head_limit`（默认 250）；另记已访问文件数 |
| 自变量 | 仓库形态：本仓库（`target/` 6.8 万文件）、纯源码仓库、大型 monorepo 代理 |
| 因变量 | ① 各上限的绑定比率（触发上限的查询占比）② 被丢弃命中占比 ③ **哪个上限先绑** ④ 同查询重复 5 次的结果集差异率 |
| 样本 | ≥ 50 个真实 pattern，取自会话历史中的实际查询，不用人造 pattern |
| 接受判据 | 绑定比率 ≥ 5% **且** 丢弃占比 ≥ 1% |
| 证伪条件 | 绑定比率 < 5% → #50 是伪需求，应**删掉该条**，而不是继续做自适应预算 |

**第 ④ 项是方法论必须项。** `grep_search` 走 `WalkParallel`（`:941`），walk 顺序不保证；一旦上限生效，「哪 20000 个文件被扫到」在每次运行间会变。所以基准集必须由**一次无上限全扫**建立，再与截断结果做差集——不能由单次截断结果反推。结果集随运行变化这件事本身若成立，就是一条独立的可靠性发现，须单独记。

**第二阶段（仅当第一阶段通过）：估算器精度.** 轻量代理 = 对 pattern 的字面量前缀跑一次 `glob_search`（`file_ops.rs:591`）估量。判据不是计数精确，而是**决策正确性**：对数量级（10 的幂）判断准确率 ≥ 0.9。若轻量代理不足，再考虑 FM-Index 的 backward search（`O(m)` count，不需 locate）——注意其空间优势对本项目无用（会话历史仅 MB 级），唯一可取的就是这个 count 原语。

---

### C5 · apply_patch 半应用状态的实际频率（只校准评分，不产出功能）

**命题.** #61 的「前 N 个已改、第 N+1 个失败」在实际会话中频率足够高，值 8 分。

| 项 | 内容 |
|---|---|
| 仪器 | `file_ops.rs:420-440` 写循环记「已写入文件数」与失败点 |
| 因变量 | 多文件 `apply_patch` 调用中，**written > 0 且返回 Err** 的比例（真正需要补偿的形态） |
| 判据 | ≥ 1% → 维持 #61 的 8 分；< 0.1% → 降为 3+4=7，并把 #65（副作用清单，信息面）提到 #61 之前，因为轻量解已够 |
| 混杂与诚实标注 | 样本来自本机使用，代表性有限，只能作为**经验频率**，不是总体频率；须在记录里写明 |
| 备注 | 本条不产出任何功能。若不愿为观测长期埋点，可改为「等真实故障发生后再补录频率」 |

---

### C1 · 会话压缩的摊还成本与尖峰

**命题（原版）.** `compact_session_in_place`（`compact.rs:130`）的每回合摊还成本随会话年龄线性增长（即「全量重算」），压缩触发回合存在明显尖峰。

**先纠正一个关于现状的事实（本次核验）.** 压缩不是模型摘要，是**纯本地提取式**：`summarize_messages`（`:193`）无任何 provider 调用，每块预算 80–240 字符（`SUMMARY_MIN_CHARS`/`SUMMARY_MAX_CHARS`，`:190-191`）。这带来两个后果：① 质量对照可做**严格差分**而非主观 A/B；② 压缩把历史整体替换为单条 stub（`:176`），于是**每次触发的重算量被「压缩间隔」限定，而不是被会话总长限定**。

**因此本测试的第一个可证伪点就是命题本身：摊还成本可能已经是常数。**

| 项 | 内容 |
|---|---|
| 仪器 | `compact.rs:130` 入口记 `message_count` / `to_summarize.len()` / 总字符；`:161` `summarize_messages` 前后计时；`:69 should_compact_with_estimate` 调用点单独计时；输出走 `HEARTFLOW_LOG`（`main.rs:185`）一行 JSON，不进上下文 |
| 台架 | 合成回放会话，挂在 `conversation.rs:73` 的确定性事件回放上；本仓无 bench 台架，须自建 |
| 自变量 | 会话年龄 N（回合数）∈ {100, 200, 400, 800, 1600, 3200}，每档 R = 30 |
| 因变量 | ① 每回合 `should_compact_with_estimate` 耗时（应已是摊还 O(1)，`:66-67` 注释自陈）② `summarize_messages` 耗时（触发回合才有）③ 触发回合端到端耗时 ④ 尖峰比 p95/p50 |
| **基线预判（写出来才能被杀）** | β ≈ 0、尖峰比偏大。理由即上面那条：重算量被压缩间隔限定 |
| 接受判据 | β ≥ 0.5 **且** 尖峰比 ≥ 3（原命题才成立） |
| 证伪条件 | β ≤ 0.2 → 原命题不成立。C1 改写为**削峰命题**，新判据：触发回合耗时占该回合端到端耗时 < 5%；否则削峰有价值但优先级低于 C2 / C3 |

**混杂（必须分离）.** token 估算已有增量缓存走 `should_compact_with_estimate`（`:66-67` 自陈 amortized-O(1)），与压缩本身是两段。不分开计时，测到的是估算器而不是压缩器。

**质量对照（差分，非统计）. ** 同一会话，比较「一次性压缩」与「模拟逐回合折叠」的产出。因为 `summarize_messages` 是纯函数，这里可以做严格差分：允许的差异必须**先显式定义**（折叠次数不同导致的截断落点差异），定义之外的任何差异都算回归。这比 A/B 主观打分强得多——**这是本轮核验带来的主要收获**。**原构想列出的那条风险（「增量摘要质量可能低于全量重算，需 A/B 对照后续问答表现」）据此不成立**：摘要由本地提取函数产出、不含模型判断，不存在一个「质量更高的重算版本」可比。

---

### C4 · 次模最大化做压缩块选择（阻塞）

**命题.** 固定 token 预算下，次模最大化选块优于 `compact.rs:186` 的按轮次加权字符分配。

**前置（未解决则不可启动，也不可证伪）.** 先定义「信息覆盖」的可测量代理。三候选按成本升序：

| 候选 | 实现成本 | 说明 |
|---|---|---|
| (i) 词袋覆盖 | 低 | 保留块覆盖的实体 / 路径 / 工具名集合大小；用现有字符串匹配即可，零新依赖 |
| (ii) 探测题覆盖 | 中 | 人工标注 20 道题，覆盖早期关键事实 |
| (iii) embedding 覆盖 | 高 | 当前无该能力，**不选** |

**设计（前置解决后）.** 固定预算下比较四者：现启发式（`compact.rs:186`）/ 贪心次模（1−1/e 保证）/ 随机下界 / 人工标注 oracle 上界。

**证伪条件.** 若 oracle 上界与现启发式接近，说明瓶颈不在预算分配，**删该条**，不要再引入次模。若贪心次模的优势 < 5% 覆盖分数且探测题正确率无差异（n=20 的符号检验需 ≥ 15:5），同样拒绝。

**排序说明.** 五条里优先级最低，与 #52 的 bench 前置同一性质：**前置不解决就停在设计阶段**。

---

### 已裁定（正负结果都进此栏）

| 编号 | 裁定 | 日期 | 依据 |
|---|---|---|---|
| — | 尚未执行任何一条 | — | 全部等待埋点实现；C3 另受本机 cargo 环境阻塞 |

**实现这批埋点需要改的源码文件（供评估，未动）**：`crates/runtime/src/compact.rs`（计时）、`crates/runtime/src/file_ops.rs`（计数器）、`crates/cli/src/main.rs`（装配期断言）。全部为**只增不改**的插桩，不改任何现有语义；若某处无法做到只增，则该猜想的测试成本需上调。

---

## 修订说明（相对上一版）

| 上一版结论 | 核验结果 |
|---|---|
| 「并行目录遍历」列为首要项 | **部分错**：grep 与模糊检索已并行；只有 `glob_search` 串行且缺剪枝 → 收窄为 #1 |
| 「复用 grep-searcher 内核替换自写扫描」 | **前提不成立**：现有实现已是 ripgrep 架构（并行 walk + 线程本地累积 + 二进制嗅探）。剩余差距只是每文件分配 → 收窄为 #21 |
| 「SymSpell 替换 strsim 全扫」 | **前提不成立**：已是 jaro-winkler 粗筛 + 前 10 名精算。真实问题是每窗口一次 String 分配 → 改为 #4 |
| 「批量 fsync 事务审计」 | **已具备**：写入已是单事务 + prepared statement |
| 「mimalloc 接入」 | **已具备**：已是全局分配器 → 降级为运行时参数调优 #24 |
| 「FxHash 替换 SipHash」 | **前提部分错**：分发表是 `BTreeMap` 不是 `HashMap`。价值更高（O(log n) → O(1)），且 `rustc-hash` 已在依赖图中 → 升至 #3 |
| 「索引优化（新增索引）」 | **方向反了**：实际存在一条冗余索引应当删除 → #25 |
| 「缓存友好布局」列在第 1 位 | **证据不足**：无 profiling 支撑，程序偏 IO/网络绑定 → 降至 #8 并标注先证伪 |
| 「代价模型驱动决策」列在第 2 位 | **过度工程**：半窗比例是合理启发式 → 降至 #11 |
| 「curl 无借鉴价值」或「curl 全面落后需替换 reqwest」 | **两端都错**：redirect/超时/压缩/幂等重试已吸收(入“已具备”)；真实差距是 Retry-After、cookie 语义、wire 日志、非流式总时长 → 新增 #28–#31 |

新增（核验中新发现）：#5 校验器缓存键、#15 工具目录缓存化、#20 FTS5 重复存储、#25 冗余索引。
新增（curl 借鉴批次，网络协议层）：#28 Retry-After、#29 cookie 语义、#30 非流式总时长、#31 wire 日志；已吸收项与不做项分别入“已具备”“明确不做”。
新增（10 领域调研批次 2026-09-21，进程/协议/上下文/TUI 层）：#32 Job Object（含 CREATE_NO_WINDOW 同落点）、#33 env 凭据洗刷、#34 cache_control、#35 估算器自校准、#36 图像计费、#37 panic 圈栏、#38 CJK bigram、#39 redact 预筛、#40 syntect dump、#41 DEC 2026（双 crossterm 收敛见任务队列 P4-c）、#42 图像降采样、#43 AV rename 重试、#44 publish 传播重试、#45 启动可观测、#46 首字节前重试纪律、#47 symlink 逃逸审计；「SSE 每 chunk 超时」到场即已具备入表；io_uring、沙箱入“明确不做”。
新增（2025–2026 理论突破批次 2026-09-21）：#48 学习增强式压缩、#49 缓存替换避 LRU、#50 扫描预算自适应、#51 加权/Damerau 编辑匹配、#52 AI 自优化回路、#53 cache 断点语义预测。来源为「固有假设被推翻」一族——elastic/funnel hashing 推翻 Yao 1985 均匀探测最优猜想（arXiv 2501.02305）、加权有界编辑距离最优算法（STOC 2025）、learning-augmented 在线算法的双保证（arXiv 2606.01342）、SOLAR 语义缓存反 LRU（arXiv 2607.00394）、HELMSON 剪枝前置（OSDI'26）、AlphaEvolve 程序搜索方法论。**借鉴的是范式而非算法本体**；同批次中时空模拟（arXiv 2502.17779）、四色定理 O(n log n)、SPAA'26 竞争下原语三条经核验在本项目无落点，不入表。
本批次行序按评分降序排列，编号保持批次初始指代，故编号不随行序递增。
新增（工具层三轴盘点批次 2026-09-21）：#54 工具 schema 选择性 deferred、#55 search_documents 常驻化 + rga 降级、#56 grep_search 参数收敛、#57 搜索类四工具边界审计、#58 Image block 纳入统一裁剪、#59 read_file 编码降级链、#60 search_documents 限额对齐、#61 apply_patch 写阶段补偿、#62 工具级审计通道、#63 side_effect_class 三分级、#64 后台任务状态收敛、#65 写类工具副作用清单回传。理论来源：Anthropic Tool Search Tool（按需加载 schema，最高 85% token 削减）、Cloudflare Code Mode（2,500+ endpoint → 2 tools）、AWS《MCP tool design: Practical approaches and tradeoffs》（参数 ≤8、enum+default 消歧、错误消息即引导、默认精简字段 + 按需详细）、Midpoint《AI Agent Reliability Checklist》（九项控制矩阵）、urandom.io（工具四字段提案 idempotency_key / attempt_number / deadline_ms / side_effect_class、补偿动作 saga、轨迹级 eval：重复副作用率与重试风暴率）。编号沿用代号顺序（#54=F1 … #65=R5），行序按评分降序。明确不做：MCP 网关式鉴权（本地单用户）、幂等键持久化（无分布式重试，本地 fs 天然可查）、把 15 工具合并为 Code Mode 式 2 个（其价值在远程 API 面，本地工具直面文件系统，写脚本反多一层）。
新增（猜想与测试模块，2026-09-21）：新建「猜想与测试」章，收入 C1–C5 五个可证伪命题及其测量设计、测试纪律五条、以及「已裁定」栏（当前为空）。C1 会话压缩的摊还与尖峰；C2 固定扫描上限的召回损失；C3 工具 effect 类型系统；C4 次模最大化做压缩选块；**C5 为本次新增**（apply_patch 半应用状态的实际频率，只用于校准 #61 评分前提，不产出功能，可删）。C1–C5 不占主清单编号（**#66 除外**，见下）。同时把「S4 暂不抽」的工程决策记入「明确不做 / 不适用」下的独立小节（S1–S7 语境见 `openmemory.md:732`、`:773`）。

同批补入（同一轮调研的其余结论，此前只落在工作日志、未进本文件）：
1. 章内新增「理论依据与来源」块，收录四个方向的原始依据——IVM/DBSP 双线性恒等式与 arrangement、FM-Index 的 Infini-gram mini / Lance 实测数据、Rust 形式化验证现状、次模 (1−1/e)。
2. 该块显式记录两条**甄别结论**（防白做功）：**回放投影本来就是增量的，不需改造**，DBSP 只对压缩（C1）与 P5 共享记忆库有价值；**FM-Index 的空间优势本项目用不上**（FTS5 trigram 已覆盖任意子串且无假阳性、历史仅 MB 级），唯一可取的是 `O(m)` count。
3. 新增主清单 **#66 纯函数契约与 panic 自由性验证**（类型 审计，2/3/2 = 5 分），行序插在 #65 与 #55 之间以维持降序；分层视图 L4 已收。这是本轮调研中唯一有落点、却尚未入表的条目。
4. **STOC 2026 / SODA 2026 批次经核验无落点**（负权最短路、有向连通性、流式 max dicut、Online Orthogonal Vectors、HDX 码），记入章内「同批经核验无落点」段，不入表、不占编号。

本轮核验带来两处对猜想的修正，须一并记明：
1. **压缩是纯本地提取式**——`compact.rs:193 summarize_messages` 无任何 provider 调用，每块预算 80–240 字符（`:190-191`）。故 C1 的质量对照可做**严格差分**（对比一次性压缩与逐回合折叠的产出），不必退化为主观 A/B。
2. **压缩把历史整体替换为单条 stub**（`compact.rs:176`），故每次触发的重算量被「压缩间隔」限定而非会话总长限定。据此 C1 原命题（摊还成本随会话年龄线性增长）**大概率已被现状否定**，基线预判写为 β ≈ 0，测试的第一个作用就是杀掉该命题、把它改写成「削峰」命题。

另核：全仓无 bench / criterion 台架（各 `Cargo.toml` 无相应条目），故上述 harness 全部自建；现成的确定性回放钩子只有 `conversation.rs:73`（供 tests 与 scripted provider 用的固定事件序列回放）。

**实施中的核验修正（2026-09-21）——两条前提在执行阶段被推翻：**

1. **#3 `rustc-hash` 替代 SipHash：目标被证伪，暂缓。** 真实工具分发是 `crates/tools/src/lib.rs:362` 的 `match name`，编译期跳转表，不是 BTreeMap。清单所指的 `BTreeMap` 是 `crates/runtime/src/conversation.rs:957` 的 `StaticToolExecutor.handlers`——**仅测试使用**（默认 `specs()` 返回空向量，生产装配走 `cli/src/tool_exec.rs`）。剩下 `schema.rs:19` 的 std `HashMap`，每次工具调用只查一次。两条都够不上原评的 8 分，故暂缓而非实施。
2. **#15 工具目录缓存化：实施点不在本轮范围。** `specs()` 的重建发生在 `crates/cli/src/tool_exec.rs:264`（`self.native.specs()` 后追加 MCP spec），属 cli crate，正被并行拆薄任务占用；`runtime` 侧无权改动装配顺序，且 `specs()` 能否安全固化取决于 MCP 连接时序。**待 cli 拆薄收口后合并处理。**

修正后的清单处置：#46 移入「已具备」；#3 保留编号但降级为待证；#15 保留原评并标注实施点。

**第二批实施中的核验修正（2026-09-21）——一条前提不成立、一条实施路径被仓库自身约束否决：**

1. **#36（Image block 计费）前提不成立 → 移入「已具备」。** `compact.rs:305` 本就写着 `ContentBlock::Image { .. } => IMAGE_TOKEN_ESTIMATE`，常量 `IMAGE_TOKEN_ESTIMATE = 1_500` 在 `:6`，且 `git show HEAD:crates/runtime/src/compact.rs` 证明它在提交 `82593c2`（2026-09-20）就已存在、`git diff` 对该文件为空。原条目（「`estimate_tokens_from` 只扫文本，Image 零计费」）是**误读**，据此并入已具备栏。真正的对应缺口不是「不计」，而是「按固定 1500 计、未按解码像素推导」——这是精度微调，不构成独立条目。

2. **#32 的 Job Object 方案被 `unsafe_code = "forbid"` 否决 → 改走安全路径。** `Cargo.toml:28-29` 的 `[workspace.lints.rust] unsafe_code = "forbid"` 是 `forbid` 而非 `deny`，**局部 `#[allow(unsafe_code)]` 无法覆盖**，因此 `CreateJobObjectW` + `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` 那条「真正的围栏」在本仓不可实现（除非改仓库级策略，或引入 `jobobject`/`process-wrap` 这类安全包装 crate——那要新增下载依赖）。已按可验证的安全路径落地：`CREATE_NO_WINDOW`（std 的 `creation_flags`，安全 API）+ 超时瞬间 `taskkill /F /T` 全树清扫。**诚实标注其性质差别**：Job Object 是「围栏」（子进程在容器内，随容器一起死），`taskkill /T` 是「时点清扫」（在超时那一刻枚举后代并终止）；后者对「守规矩的子孙」足够，但理论上存在「清扫枚举后新生的进程逃逸」的窄窗口。非 Windows 平台保留原状（仅直接子进程），原因是该平台子进程刻意与终端同进程组以便 `Ctrl+C` 可达，重新分组会**静默破坏中断投递**——这个取舍写在 `kill_process_tree` 的文档注释里。

3. **#35 的实施模型在实测后被推翻重做**（初版纯乘性 → 仿射），依据见「实施进度」下的实测专节。这一条不是「前提不成立」，而是「前提只对了一半」：低估真实存在且更严重（62.4% vs 原估 30%），但归因错了——主项是加性固定开销，不是乘性密度。

**第三批核验（2026-09-21）——一条工程决策入档：TUI 斜杠派发暂不盲改。**

cli 拆薄已把交互面分成默认 REPL 与 `HEARTFLOW_TUI=1` 可选全屏载体，二者在 `main.rs:728` 的门处分叉（`:779` `return Ok(false)`）。核实发现**剩余唯一实质缺口是 TUI 不派发斜杠命令**（`tui.rs:1032` 注释自陈、`:1926` 每条提交一律当回合跑）；判定为对主交互面的大规模侵入式重构，不宜在无真机验证下盲做，故**不立为编号条目**，而以独立小节记入「明确不做 / 不适用」（与「S4 暂不抽」并列）。同批确认：补全菜单只存在于 REPL 的 `editor.rs`，脱离派发单独为 TUI 补全只会更误导，一并否决。核实行号：`:950-1063`（20 arm，全部 `println!` 输出）、`:1009-1016`（`/restart` 交互确认）、`:1050-1054`（`/plan` 需 prompter）、`:965-968`（`/exit` resume 提示）。

**第三批实施（2026-09-21）——#64 后台任务状态收敛落地。**

安全区（runtime/store/api/provider）内继续按「测试结果与事实」推进，本轮完成 #64。原条目的诊断是「设计自洽，缺的是终止状态侧写」，实施时发现一个更硬的事实：**Windows 分支原本是 `drop(child)`**（`bash.rs` 旧 `#[cfg(not(unix))]` 段），句柄一关退出码就永远观测不到——所以侧写不是「加个文件」，而是要**把 reaper 推广到两个平台**（Unix 本来就有 reaper，只是从不记录结果）。两平台因此合并为同一份 `wait()` 线程：`running` 快照在 reaper 启动前落盘（保证终态写入永远在后、不被覆盖），终态再写 `exit_code`/`success`。

同批补上 `tempfile_log` 缺失的清理策略：`BACKGROUND_LOG_RETENTION`（3 天）+ `prune_background_logs`，**只匹配 `bg-` 前缀**，外来文件一律不碰；mtime 缺失或未来时间一律视为「不过期」（保守方向）。`BashCommandOutput` 新增 `background_status_path: Option<String>`（`skip_serializing_if` 保证旧路径不破），全仓构造点仍只有 `bash.rs` 一处，故为纯增量。测试：`bash` 模块 19 → **21 passed**，runtime 全量 151 → **153 passed / 1 ignored**；新增用例含「侧写如实报告 exit 7」与「清理只删自家产物」。

---

## 附：按底层度分层视图

| 层 | 含义 | 条目 |
|---|---|---|
| L1 硬件层 | CPU 缓存、SIMD、多核、存储介质 | 8, 14 |
| L2 OS 原语层 | 系统调用、内存映射、文件系统、进程、字节编码 | 1, 6, 7, 16, 22, 32, 33, 41, 43, 47, 59, 61, 64 |
| L3 数据结构与算法范式层 | 索引结构、编码、哈希、合并、字符串匹配 | 2, 3, 4, 9, 10, 11, 12, 13, 17, 18, 19, 20, 21, 23, 25, 26, 27, 38, 39, 51 |
| L4 策略与调度层 | 代价估算、缓存目录、任务编排、网络协议语义、schema 装配、审计、形式化验证 | 5, 15, 24, 28, 29, 30, 31, 34, 35, 36, 37, 40, 42, 44, 45, 46, 48, 49, 50, 52, 53, 54, 55, 56, 57, 58, 60, 62, 63, 65, 66 |

---

## 附：结论性提醒

「索引优化算不算算法」：分三层看——B+ 树/LSM/FST/跳表等结构本身是数据结构算法；优化器的索引选择与代价估算是搜索 + 贪心/DP 算法；`cache_size`、`page_size` 这类参数调整只是调优。判据：**收益依赖数据分布与工作负载、且能被代价模型预测的，是算法；收益只来自参数取值、又拿不出模型的，是调优。**

本清单里 #25 是最能说明问题的例子：一个"索引优化"的正确形态是**删掉**一棵被隐式索引完全覆盖的 B-tree——这需要判断复合索引的前缀覆盖关系，而不是往表上继续加索引。
