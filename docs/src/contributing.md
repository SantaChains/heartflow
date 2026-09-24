# 贡献与质量门

贡献入口以仓库根的两份文件为权威，本文不再重复维护：

- [CONTRIBUTING.md](https://github.com/SantaChains/heartflow/blob/main/CONTRIBUTING.md)：开发环境、本地质量门、panic 预算棘轮、提交规范与发版语义、PR 流程、接口面同步清单、架构边界
- [AGENTS.md](https://github.com/SantaChains/heartflow/blob/main/AGENTS.md)：面向 agent 的完整仓库指令，两者冲突时以 AGENTS.md 为准

一句话版本：提交前保证 `cargo fmt --all -- --check`、`cargo clippy --workspace --all-targets -- -D warnings -A clippy::pedantic`、`cargo test --workspace` 三条全绿，提交信息遵循 Conventional Commits（`feat:` 触发 minor、`fix:` 触发 patch，scope 用 crate 名），Issue 与 PR 使用仓库提供的模板。

## License

本项目以 [Apache License 2.0](https://github.com/SantaChains/heartflow/blob/main/LICENSE) 开源。
