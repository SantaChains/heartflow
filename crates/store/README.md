# heartflow-store

heartflow 的会话持久化：每会话一份 JSON 快照为权威转录（原子 temp+rename 覆盖），best-effort 镜像到系统级 SQLite 历史库（FTS5 trigram 全文检索、用量聚合）。

heartflow 家族的库组件，终端工具与使用文档见 [heartflow](https://crates.io/crates/heartflow)。
