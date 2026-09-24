<!-- 仓库无每-push CI 门,以下检查在本地完成后勾选。 -->

## 概述

<!-- 动机、改动面、验证方式。涉及公共契约时注明同步了哪些文档。 -->

## 提交前检查

- [ ] `cargo fmt --all -- --check` 通过
- [ ] `cargo clippy --workspace --all-targets -- -D warnings -A clippy::pedantic` 通过（all 零告警为硬门）
- [ ] `cargo test --workspace` 通过
- [ ] `python3 scripts/check_panic_budget.py` 通过，未为通过检查而新增 `panic-ok` 标注或刷新基线
- [ ] 提交信息遵循 Conventional Commits（feat/fix/!，scope 用 crate 名）
- [ ] 改动涉及公共契约时，README、`docs/src`、`llms.txt`/`llms-full.txt`（`bash scripts/gen-llms.sh`）已同步
