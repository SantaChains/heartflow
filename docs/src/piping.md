# 管道与脚本

hf 遵循 Unix 过滤工具约定：当 stdin 被管道或重定向（非交互终端）时自动读入全文，与命令行指令拼接为一个提示词（`指令\n\n<stdin>`）；`--quiet` 只输出回答正文，`--json` 输出 `{text, usage, session_id}`，便于与 `jq`、`Select-String`、`git` 等串联。每回合自动镜像进 SQLite 历史库，`hf search` 因此可在管道里非交互地检索历史。

```bash
git diff HEAD | hf prompt --quiet "评审这次改动，只列风险点"
git diff HEAD | hf -p "评审这次改动，只列风险点"    # -p 是 prompt 的短写法，同一个过滤器
hf prompt --json "用三句话总结上面这段日志" | jq -r .text
hf search 中文笔记 --json | jq -r '.[].snippet'   # ≥ 3 码点走 FTS5 trigram，短词/中文回退转义 LIKE
```
