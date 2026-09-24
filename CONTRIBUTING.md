# 贡献指南

感谢关注 heartflow。本文是贡献者的入口，与 agent 指令文件 [AGENTS.md](AGENTS.md) 同源，两者冲突时以 AGENTS.md 为准。

## 开发环境

- Rust 工具链由 `rust-toolchain.toml` 钉定（1.93.1 + rustfmt/clippy），rustup 会自动按需安装；MSRV 为 1.88（下限来自依赖，声明在根 `Cargo.toml` 的 `[workspace.package] rust-version`）
- 本仓库无每-push CI 质量门，全部检查在本地完成后再提交

```bash
cargo build --release          # target/release/hf(.exe)
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings -A clippy::pedantic
cargo test --workspace
```

`clippy::all` 零告警是硬门；pedantic 为提示级，不强求清零。

冒烟验证：

```bash
cargo run -p heartflow -- --help
cargo run -p heartflow -- doctor
cargo run -p heartflow -- system-prompt
```

注意 `doctor` 与 `system-prompt` 是子命令，不是 flag；写成 `--doctor` 会被参数解析器以 exit 2 拒绝。

## panic 预算棘轮

提交前手动执行 `python3 scripts/check_panic_budget.py`（可用 `--list` 查看、`--update` 刷新基线）：

- 只统计 `crates/*/src/**/*.rs` 的生产行（剔除测试、注释与字符串）
- 基线在 `scripts/panic_budget.json`，是本机棘轮，不入库；新增 panic 一律失败，`debt` 只许变小
- 刻意保留的 panic 必须在命中行行尾或紧邻上一行标注 `// panic-ok: <理由>`，理由不少于 8 字符
- 纪律：不要为了让检查变绿而标注或 `--update`

## 提交规范

提交信息遵循 Conventional Commits，它直接驱动发版链（git-cliff），不是风格偏好：

- `feat:` 触发 minor，`fix:` 触发 patch，`!` 或 `BREAKING CHANGE:` 触发 major
- `chore` / `docs` / `test` / `ci` / `style` / `build` 不触发发版
- 版本号唯一维护点是根 `Cargo.toml` 的 `[workspace.package]`，不要手动改各 crate 版本

## Pull Request 流程

1. Fork 后建分支，一个 PR 聚焦一件事
2. 提交前跑完上面的质量门与 panic 棘轮
3. PR 描述说明动机、改动面与验证方式；Issue 用仓库提供的五种模板（Bug / 功能 / 咨询 / 性能 / 文档），先搜索是否已有重复
4. 改动涉及公共契约（见下节）时，PR 描述里注明同步了哪些文档

## 接口面变更同步清单

修改任一公共契约时，以下文档必须同一 PR 内同步：

| 变更对象 | 同步目标 |
| --- | --- |
| CLI 子命令、flag | README、`docs/src`、`llms.txt` / `llms-full.txt`（`bash scripts/gen-llms.sh`） |
| 配置文件格式 | 同上 + `docs/src` 对应章节 |
| 模型目录（provider.toml 种子） | `bucket/heartflow.json` 的 notes |
| 其他公共契约 | `.devin/wiki.json` |

## 架构边界

分层不可破，PR 触及边界会被要求拆分：

- 依赖方向：`cli` → `{provider, runtime, api, tools, mcp, store, commands}`；`api`（仅 reqwest/serde/tokio）→ `provider`（协议桥接）→ `runtime`（会话循环）
- `runtime` 不感知传输细节，不得引用具体协议类型
- 身份不变量 `FRONTIER_MODEL_NAME = "heartflow"`，改提示词时必须保留

## 第三方代码

任何衍生或移植自外部项目的代码（脚本、片段、算法），须在根 `NOTICE` 追加版权声明与许可证全文，并在 README 致谢部分点名来源。

## 性能验证

涉及热路径（流式渲染、工具调度、压缩、检索）的改动，建议用 `scripts/bench-gate.sh --save` 建立本机基线后对比；超过默认 30% 的回退会使脚本以非零码退出。基线是机器本地的，不要提交进仓库。
