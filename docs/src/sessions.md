# 会话与历史库

对话以 JSONL 为权威存储写入 `~/.heartflow/sessions`；同时 best-effort 镜像进系统级 SQLite 库 `~/.heartflow/heartflow.db`（镜像失败绝不阻断保存）。库采用 WAL、外键级联与 `user_version` 迁移，历史消息存于 `messages`、`messages_fts`（FTS5 trigram）供检索。中文、空格、Windows 全路径均按 UTF-8 正确处理。REPL 内 `/search <Q>` 与非交互 `hf search <Q>` 共用同一检索引擎；查询 ≥ 3 码点走 trigram 索引，更短的词回退到转义后的 `LIKE`。

镜像以每会话稳定的键写入（进程级，与 JSON 文件命名解耦），故一次对话在库中是一行、随回合**增量追加**新消息（`append_messages` 仅写尾部），而非每回合重刷一份全量快照。当回合使转录缩短（compact）或原地改动已镜像行（`/pin` 翻标志、任务环重置换种子）时自动回退全量重写。JSON 始终权威，被拒的追加从不丢失或重复历史。

因为库是从 JSON 派生的缓存，`hf doctor` 会对其跑一次结构体检（只读、非破坏）：默认用完整的 `PRAGMA integrity_check`，仅当库超 64 MiB 才降级为 `quick_check`。检出损坏时不会静默改数据，而是提示"JSON 转录仍为权威，删除 `heartflow.db` 即会在下次保存时重建检索索引"。

## 恢复会话

```bash
hf --resume                 # 进选择器挑历史会话
hf --resume=a.json          # 直接打开指定转录
hf --resume --run /compact  # 恢复后立即执行 slash 命令
```

`/exit` 退出时会自动保存并打印可直接粘贴的 resume 命令。
