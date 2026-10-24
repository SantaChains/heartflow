# 发布前 QA 清单

ds4 `QA_BEFORE_RELEASES.md` 纪律的本仓落地（roadmap §4）：每次发版前逐项勾选。分四段——本地硬门禁、离线行为仲裁、冒烟、真机陪测与发版流程。**任何一项不过就停**：发布逻辑靠 `scripts/release.sh` 共用事实源，但质量判断在这里。

本清单本身随踩坑更新：每次发版发现的坑，修完回填一条（写明日期与原因），这是清单的生长方式。

## 1. 本地硬门禁（提交前全部必跑，全绿才许 push）

- [ ] `cargo build --release` 干净（`target/release/hf(.exe)` 产出）。
- [ ] `cargo fmt --all -- --check` 干净。
- [ ] `cargo clippy --workspace --all-targets -- -D warnings -A clippy::pedantic`——`clippy::all` 零告警是硬门；pedantic 提示级不强求清零，但**新增代码不添新 pedantic 告警**。
- [ ] `cargo test --workspace` 全过，**记下总数**（当前基线 785；数量涨跌要能解释——新测试计入，删测试要问为什么）。
- [ ] `uv run scripts/check_panic_budget.py`——`debt=0` 纪律；**绝不为让门禁变绿而 `--update`**（`--update` 只在真的清掉存量债之后跑）。
- [x] 依赖卫生（2026-10-24 起启用，cargo-machete 0.9.2 + cargo-deny 0.20.2 已装）：`cargo machete` + `cargo deny check advisories bans` 全绿（首跑即绿；machete 唯一命中 ratatui-widgets 为特性承载误报，已入 ignore 并注明）。发 crates.io 的 crate 改公共面前跑 `cargo semver-checks`。
- [ ] amend 之后**重跑全部门禁**（历史教训：amend 收尾打磨后忘跑 clippy，存量告警在下一轮才暴露）。

## 2. 离线行为仲裁（cassette 时代起必跑）

- [ ] `cargo test -p heartflow-provider --test fixtures`——两真实夹具复穿真运行时；**提示组装/回放投影/工具广告/消息序的任何漂移在这里点名失败**。
- [ ] 动过流式路径？铁律：**先重录夹具再动**（录制 = `HEARTFLOW_CASSETTE=record:crates/provider/tests/fixtures/<name>.json hf prompt -q "..."`，一行一个夹具）；录完跑秘密扫描 + 人工核验（环境变量**名**可入库，**值**绝不）。
- [ ] 全树 `cargo update` 之类依赖刷新后：更新前后各跑一遍全门禁；终端栈（ratatui/crossterm/tui-textarea git 钉住面）变动必须显式核对。

## 3. 冒烟（构建产物本体）

- [ ] `hf --help` 正常。
- [ ] `hf doctor` 正常（子命令形态——写成 `--doctor` 会被判 usage error 以 exit 2 退出，这是参数解析器的既定行为不是 bug）。
- [ ] `hf system-prompt` 正常；改过系统提示词时，核对 `FRONTIER_MODEL_NAME = "heartflow"` 身份不变量仍在（`crates/runtime/src/prompt.rs`）。
- [ ] 改过配置面？`hf config export <surface>` 导出→重导入 round trip。

## 4. 真机陪测与发版流程

- [ ] **TUI 改动必须有真机冒烟卡**，且逐张勾完才算闭合。陪测卡登记在 `roadmap.md` 对应小节的「真机冒烟待陪测」处（当前累积见 §11.0/§11.1/§11.2/§11.3/§11.4）；用 `target/release/hf.exe` 验，别用 PATH 上的旧版。
- [ ] 跨平台路径（进程树击杀、锁语义等）真机验证过再发（Windows 本机即验；Unix 侧 CI 覆盖编译面）。
- [ ] `bash scripts/release.sh plan`——`released=true`、版本号语义对（`feat:`→minor、`fix:`→patch、`!`/`BREAKING CHANGE:`→major）。
- [ ] `RELEASE_TAG=<tag> RELEASE_BUMP=<bump> RELEASE_DRY_RUN=1 bash scripts/release.sh publish` 只读预演过（需本地 git-cliff；tag/bump 取 plan 输出，脚本不自动继承）。
- [ ] push 后核对三条 CI 链：release.yml（发版）、docs.yml（文档站）、ci.yml（外部 PR 质量门）。
- [ ] Release 资产核对：Windows amd64 zip + `.sha256` + `SHA256SUMS` + SLSA provenance；`bucket/heartflow.json` 的 version/url/hash 由 windows-zip job 确定性回写（CI 是唯一写入者）。
- [ ] publish 中途失败的续发口径：`RELEASE_TAG=<tag> RELEASE_BUMP=<bump> bash scripts/release.sh publish`（重跑整链会因「tag 之后无 feat/fix」短路）。

## 5. 未闭合残留的显式登记

发版可以带残留，但**必须在本节点名**，不许静默：

- [ ] （2026-10-24）全树 `cargo update` 后 brotli 9 的真实网络解压路径不受离线测试覆盖（cassette 绕过传输）——下一次真机会话第一次真实 API 对话即验，异常形态：HTTP 解码错误/乱码正文。
- [ ] （持续）陪测卡全绿前不 push 发版——push 权在用户。

## 历史坑登记（修完的坑回填在此，防再犯）

- 2026-10-23：amend 后没重跑 clippy，存量未使用绑定下一轮才拦下——故第 1 段末条。
- 2026-10-23：compact 统计损坏测试写坏**进程级全局文件**，并行测试写回「治愈」损坏致偶发失败——磁盘断言必须走私有路径，共享全局态的测试模块须 ROOT_LOCK 串行化（jobs/mission 模式）。
- 2026-10-24：jobs 槽位测试固定 120ms sleep 断言在满载并行下饿死——时序断言一律改有界轮询，不变量是状态不是时钟。
- 2026-10-24：全量测试出现瞬时 FAILED 先复跑 3 次再定性；隔离复现通过 = 并行竞态嫌疑，修测试而非回滚代码。
