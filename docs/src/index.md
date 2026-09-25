# heartflow

Rust 实现的终端 AI agent。二进制命令 `hf`，在 REPL 中通过流式输出与模型协作，可执行 shell、读写文件、检索代码、挂载 MCP 工具，并以任务循环自迭代完成多步工作。

仓库：[github.com/SantaChains/heartflow](https://github.com/SantaChains/heartflow)

## 它能做什么

- **真流式**：SSE 增量经 mpsc 通道推送，思考与正文实时渲染，markdown 与代码高亮输出。
- **三方言**：Anthropic 消息协议、OpenAI Chat Completions 与 OpenAI Responses（`/v1/responses`）；内置 DeepSeek，自定义 provider 通过 `[provider.NAME]` 的 `protocol` 选方言，可接任意兼容端点。
- **原生工具**：bash、read/write/edit_file、glob/grep/search_files、apply_patch、todo_write、ask_user、web_fetch（SSRF 防护）等。
- **MCP 支持**：JSON-RPC 2.0 双传输——本地 stdio 与远程 Streamable-HTTP/SSE。
- **任务自迭代**：todo_write 登记计划，未完成任务自动续推；Hermes 任务环逐任务在新鲜上下文里执行。
- **上下文工程**：`>50%` 窗口预压缩（summarize-then-compact）、`/compact` 手动压缩、`/pin` 免疫压缩。
- **会话持久化**：每会话 JSON 快照为权威存储 + SQLite FTS5 镜像，支持 resume 与跨会话检索。
- **自迭代记忆**：`~/.heartflow/MEMORY.md` 以极小 token 注入系统提示词。

完整能力矩阵与逐条实现细节以 [仓库 README 的「特性」节](https://github.com/SantaChains/heartflow#特性) 为准，本文档站聚焦"怎么用"。

## 从这里开始

- 想跑起来：见 [安装](installation.md) 与 [快速开始](quickstart.md)。
- 想让 AI 工具读到最新文档：见 [AI 文档集成](ai-integration.md)。
