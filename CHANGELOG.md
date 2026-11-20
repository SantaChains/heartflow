
# heartflow 1.6.0 - 2026-11-20

## 新功能

- **provider**: Builtin 表扩至 7 家(zai 智谱国际端点入列) + {NAME}_BASE_URL 基址重定向环境变量族(显式 config > env 重定向 > 内置默认) + AGENTS/docs/bucket/wiki/llms 接口面同步 (`0cce2bf`)

- **st-3/w5/d4**: Mission resume 自动重启 loop 驱动(ST-3) + ToolError hint 字段与政策拒绝 hint(W5) + doctor 含密文件/inline key 检查(D4/D8) (`69c8af5`)

- **provider**: Builtin 表扩至 6 家(zhipu/kimi/qwen/openai 入列,/provider 可切+两段补全) + deepseek 默认模型换代 deepseek-flash + REPL /models 在线刷新模型列表进 catalog + roadmap 收编 DeepSeek 全家桶/Harness 接线 (`8aec613`)

- **repl**: /provider [NAME] [MODEL] 命令——列出/切换 provider(读 config.toml+env,轻量同 /model 契约) + 两段式补全(第一段补 provider 名,选定后第二段按该 provider 的 catalog 补 model 名) + editor 双参数 site 机制 (`cd62328`)

- **w1-w4**: Shell 工具面小批——Windows shell stderr/进度治理前缀(W1) + 工具 description 退出码/2>&1 指导(W2/W3) + nushell 一等 shell 支持(W4) (`efc38f5`)

- **st-2**: 步级传输重试(≤2线性退避,可禁用) + 压缩两级降级(overflow 二连夹具)——传输类流故障回合内恢复,溢出残留两级递进 (`c6f47bc`)

- **st-1**: 流 idle timeout(S1) + 流错误已收 blocks 落 transcript(S7)——夹具先行纪律兑现 (`147fb41`)


## 缺陷修复

- **security**: D9 收口——runtime::private_fs 权限加固(Unix 0600/0700, Windows no-op)钉满六类 save 写点(会话快照/SQLite/影子 git/jobs/mission/cassette) + 诊断子进程 cargo --color=never 防 ANSI 污染解析 (`aee76c6`)

- **security**: D2 收口——jobs 持久化面(events.jsonl/status.json)接 redaction——set_job_redaction_secrets 全局集,spawn 源头 label/prompt_head 脱敏,worker 写盘前过滤,测试钉死 (`336ad46`)

- **security**: 代理凭据脱敏进日志/doctor(D1) + MCP stdio 子进程凭据清洗(D3) + image key 注册(D5) + 4xx body 截断(D10)；security.dev.md 安全面审计 (`0ed240f`)

- **cassette**: Replay 匹配按 DYNAMIC_BOUNDARY 切分——静态段严格、动态段只比块数；.gitignore 忽略 fixtures/；新增动态漂移/静态漂移两测试 (`be28cd7`)

- 稳定性批次——错误分级 R2/退避 S2/压缩护栏 R4/循环熔断 S6/热重载保态 S4/steer 落录 S5/锁污染收口 D4/SlotGuard J1/ReplPlanState 聚合 D1/桥接统一 D3；调研文档 agents/ai-rust-hygiene/stability 收编 (`f1a9391`)


# heartflow 1.5.0 - 2026-10-26

## 其他

- **toolchain**: 撤回 rust-analyzer 组件——用户裁决不装非必需/旧工具，IDE 警告属外观性质接受 (`d6aca29`)


## 新功能

- 新增 spec 模式并实现迭代限制软终止 (`3c62a8d`)

- **tui**: C-1 下相——cycle_mode(Shift+Tab)/toggle_web(Ctrl+O) 键位 + 联网三态共享原子即时生效（roadmap C-1） (`49cf7fd`)

- **cli**: C-1 模式系统核心落地——spec/loop 权限档 + /mode 六值 + /web 三态 + /effort + 模式提示段（roadmap C-1） (`491b0db`)

- **api,provider**: Tool Choice + Response Format 贯通（采样战役两补遗收口） (`8cf0276`)

- **provider**: 采样参数全链贯通收尾——SamplingParams 六键 config→profile→三协议 adapter（790 测试） (`d7a1b97`)

- **cli**: /bootstrap DeepSeek 特殊优化——flash 自动选择 + 余额请求 + 上下文自动填入（真机验证） (`7b65d04`)

- **cli**: 模型自举落地——/bootstrap <provider> [model] [--reorganize]（用户点名，roadmap §C） (`1034d80`)

- **runtime**: 基准套件第一片——零依赖 hot-path harness（真实夹具输入，roadmap §9） (`eecc72b`)

- **cli**: 启动横幅加「心狭」中文像素 wordmark——同源 half-block 管线 (`f127bfc`)

- **runtime**: Compaction 改造收尾——Phase 1b 硬触发 + Phase 3b 摘要模式（roadmap §4 D4.2/D4.5） (`0989631`)

- **tui**: Per-section 泳道递送轮转落地（roadmap §11.4）——跨 section 公平轮转 + 逐条递送 + 回合中导航解禁 (`1303294`)

- **tui**: P-C 三件套落地——队列编辑面板 + guide 两栏可编辑 + mode 分流计数（roadmap §11.1） (`e98e98e`)

- **runtime**: D1.3 mission 续跑驱动器——每回合提醒注入 + 单活跃自动记账（roadmap §11.3） (`bde7a43`)

- **tui**: P-D Steer 落地——在飞回合迭代边界注入（roadmap §11.2） (`e9d63cf`)


## 缺陷修复

- **tui,jobs**: 审计修复——轮转守卫臂条目丢失 + 对账写锁纪律（复查 6156912/1303294/3294c42） (`f0e8ece`)

- **jobs**: 跨进程 job 互踩修复——pid 命名空间 + 探活对账（roadmap §4 遗留/§11.5） (`3294c42`)

- **tui**: KI-001/002——多行续行键对齐 REPL + 打断提示状态派生持久化 (`b9d2f9a`)

- **runtime**: 修复 compact 统计损坏测试的并行竞态 + clippy 未使用绑定 (`0f3ecf5`)

- **ci**: 修复发布流水线因二进制 git 依赖导致的空壳 Release (`5554faa`)


## 重构

- **cli**: 质量审计——/compact 语义修正 + 死代码清除 (`5ce7335`)

- **tui**: 三相事件循环重构(roadmap §11.0 根因锁) (`4b4e67c`)


# heartflow 1.4.0 - 2026-10-21

## 新功能

- Thiserror 2 错误类型迁移——11 枚举/2 结构体样板消除 + 跨边界 stringly 错误修复 (`43b8c9a`)

- 依赖治理——终端栈收敛至 ratatui 0.30 / crossterm 单拷贝 + dependabot 裁决 (`9ea2636`)

- 跨进程文件锁——sidecar <data>.lock + OS 建议性锁（路线图 §4 批次） (`1aeffe3`)

- 停机安全 + 长期运行稳定性（路线图 §4 批次） (`5d6f4c3`)

- Mission 账本 + scheduled-runs + 队列命名泳道（路线图 §1/§4 三项落地） (`3069a7d`)


## 重构

- 全项目代码考究审计 + u64 边界混沌测试（§4/§9 纪律落地） (`844f165`)


# heartflow 1.3.0 - 2026-10-21

## 新功能

- 落实路线图 §4/§6/§1 三批次并新增 §8 bash job 监控调研批次 (`b31983d`)


# heartflow 1.2.0 - 2026-09-25

## 新功能

- 新增聚焦模式、错误提示优化及多工具步骤展示 (`fda2515`)

- Md 渲染收窄到基本能力子集，修掉会改写命令的智能标点 (`4f61264`)

- 移除代码块语法高亮并补表格投影测试，syntect 依赖退场 (`1cb47d5`)

- 落地写入路径约束与权限门理由，修复六处静默失败 (`fa1e4e9`)

- 新增 -p 短命令与离线回放夹具，修复锁污染降级并加固质量门 (`a3227ee`)

- 新增模型目录与补全系统，重构配置与会话流程 (`197acf4`)

- 落地panic预算棘轮机制，新增脚本与8处合理panic标注 (`9573bc9`)

- 新增多配置面导出、热重载拆分与token校准能力 (`e4bfeee`)


## 重构

- 重构路径规范化逻辑以处理跨符号链接的父目录跳转 (`2e4dae6`)


# heartflow 1.1.0 - 2026-09-20

## 性能优化

- Vectorized ASCII fast path for token estimation (`82593c2`)

- Compact the session in place without full rescans (`fd6d541`)

- Glob_search honors .gitignore via ignore+globset, add tool benchmark (`6da8811`)

- Switch hf to the mimalloc global allocator (`a5bbfa1`)

- Cache syntect globally and parallelize MCP HTTP requests (`c1d1d26`)

- **runtime**: Amortized-O(1) session token estimation (`22b988f`)

- **runtime**: Cache compiled JSON-Schema validators (`2eabe38`)

- **runtime,api**: Parallel grep crawler, atomic writes, wire compression (`e9e12a6`)

- **runtime**: Serialize mutating tools; run read-only batches in parallel (`712ae6c`)

- **cli**: Mirror history incrementally under a stable conversation key (`7070b57`)

- **store**: Reuse prepared INSERTs in save_session hot loop (`2888c6f`)


## 新功能

- Geometric heart companion and incremental redaction on save (`43ff2ad`)

- Reshape attachments down to the vision grid (`71f13d7`)

- Restore sessions from snapshot plus append-only segment (`326daad`)

- Add performance regression gate script (`31a719c`)

- Suggest the closest snippet when old_string misses (`1b8f2b1`)

- Cap execute_bash output streams at 512 KB (`b6f7cf9`)

- Fold oversized diffs returned by edit tools (`f2f315b`)

- Background bash tasks log to a file; extend the tool benchmark (`dce5dad`)

- Glob_search accepts multiple globs in one crawl (globset) (`8d2de7f`)

- Export rga_available for conditional tool registration (`954219e`)

- Register search_documents only when rga exists, update search guidance (`f4f6f72`)

- Add search_documents tool backed by external rga (`ebca401`)

- **api,provider,runtime,tools,cli**: Image attachments end to end (`a984f4f`)

- **cli,runtime**: Half-block mascot, Kurosawa theme, transcript redaction (`ef7d5ac`)

- **tools,cli**: Add web_search, structure-preserving web_fetch, geometry golden tests (`a932933`)

- **cli**: Render mascot as braille dot-matrix blob, faster tick (`e8ef72f`)

- **cli**: Add ASCII/geometric mascot companion (Route A, spring-driven) (`a449b9a`)

- **store**: Add append_messages incremental tail-write fast path (`1b0d7fc`)

- **cli**: Make terminal palette overridable via theme.toml (`a8eb4c9`)

- **cli**: Unify terminal colors/layout into a single Theme source (`682c840`)

- **cli**: Replace reedline with a ratatui + tui-textarea inline input editor (`94fed8c`)

- **cli**: Add ! shell pass-through running the shared bash tool off the async reactor (`64b3dfb`)

- **cli**: Add session core state layer with follow-up queue and local guide assembly (`26c165c`)


## 缺陷修复

- Doc_search timeout-thread drain and honest truncation flag (`2e8dcd9`)

- Retry idempotent file reads once on transient OS errors (`a315239`)

- **cli**: Grok-bot mascot redesign, cursor flicker and anchor drift (`54d2979`)

- **cli**: Embed windows version resource in hf.exe (`15a3e9c`)


## 重构

- **workspace**: Extract heartflow-provider crate (`5e0c1b4`)


# heartflow 1.0.1 - 2026-09-19

## 缺陷修复

- **scoop**: Hash extraction field is find, not regex (`bb45a6b`)


# heartflow 1.0.0 - 2026-09-19

## 其他

- Harden memory clamp + replay view found in review (`69dbb52`)

- Make Ctrl+C a reliable, discoverable turn interrupt (`b464c75`)

- Make permission, todo-threshold and Design-ranking lines actionable (`f2a9da8`)

- Redact secret-named config values before injecting into the system prompt (`2b72c1e`)

- Tunable replay tail + Ctrl+D saves session and prints resume hint (`fa29cdc`)

- Lean replay view + fair memory clamping on resume (`bdc4bc3`)


## 新功能

- **cli**: Document interaction contract, exit codes, add explicit chat alias (`d60d9fc`)


## 缺陷修复

- **release**: Touch CHANGELOG before git-cliff prepend (missing on first release) (`c69a443`)

- **release**: Git-cliff prepend rejects same-file output, drop -o from CHANGELOG (`599a47e`)

- **scoop**: Checkver github expects user/repo, not full url (`a21fe06`)

- **cli**: Make --resume enter the REPL with the restored session; add -v version (`99e7760`)

- **prompt**: Align tool names to specs and advertise openmemory L79 tools (`a575181`)


## 重构

- **prompt**: Rewrite system prompt in layered natural language, drop markdown scaffolding (`6f1bdc3`)

- **cli**: Collapse resume positionals into --resume[=PATH] plus --run (`e2dd5eb`)

- Replace bounded schema checker with jsonschema crate; dedup helpers (`43192b2`)

