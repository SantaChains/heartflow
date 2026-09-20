# 贡献与质量门

日常迭代按效率优先：提交前保证下列四条绿。pedantic 级风格 clippy 仅作提示不阻断，但 `clippy::all` 正确性 lint 仍为阻断门。

```bash
cargo fmt --all -- --check
cargo test --workspace
cargo build --release
cargo clippy --workspace --all-targets -- -D warnings -A clippy::pedantic   # 阻断正确性；pedantic 仅提示
```

## 提交规范

版本语义由 git-cliff 从提交信息推导：`feat:` → minor，`fix:` → patch，`!` 或 `BREAKING CHANGE:` → major；`chore/docs/test/ci/style/build` 不触发发版。scope 用 crate 名，如 `feat(tools): ...`。

## 文档

改 README 里列出的 CLI/REPL 接口时，同步更新 `docs/src/` 对应章节与 `--help`。文档站构建见 [AI 文档集成](ai-integration.md)。

## License

本项目以 [Apache License 2.0](https://github.com/SantaChains/heartflow/blob/main/LICENSE) 开源。
