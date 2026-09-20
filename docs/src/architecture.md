# 架构

Workspace 结构（`crates/*`，依赖单向 cli → {runtime, api, tools, mcp, store, commands}）：

```text
crates/
├── api       传输层：Anthropic/OpenAI 客户端、SSE 解析、重试
├── runtime   会话循环：流消费、工具调度、compact、系统提示词
├── tools     原生工具实现与注册
├── mcp       MCP 客户端（stdio + Streamable-HTTP/SSE JSON-RPC 2.0）
├── commands  请求/响应数据结构
├── store     系统级 SQLite 历史库：FTS5 全文检索、用量聚合、事务写入、best-effort 镜像
└── cli       hf 入口：REPL、配置、渲染、输入编辑
```

## 上下文注入纪律

磁盘转录 ≠ 喂给模型的注入视图。每次构造请求时，`conversation.rs::build_replay_messages` 会把超出最近 `replay_verbatim_tail`（默认 12）条的旧 `tool_result` 正文折成占位，落盘 session 仍存全文；`tool_use`/`tool_result` 配对与顺序不可破，`/pin` 的消息不改写。Memory 段按 user/project 两层公平夹紧，防避坑条目被饿死。

## 健壮性

connect/read 双超时、子进程 `kill_on_drop`、全链路 UTF-8（BOM 剥除、非 UTF-8 字节经 `chardetng` 嗅探 + `encoding_rs` 解码 GBK 等遗留码页、CJK 宽度对齐）、工具输出 32K 截断、缓存目录剪枝；工具入参执行前用 `jsonschema` crate 做完整 JSON-Schema 校验。
