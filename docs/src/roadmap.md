# 路线图

以下方向已明确规划但**尚未实现**，仅作路线记录：

- **Browser use / computer use**：驱动浏览器与 Windows 桌面操作（点击、输入、截屏、表单填充）的原生工具。
- **长期记忆**：基于向量/embedding 检索的跨会话持久记忆，区别于当前的 FTS5 全文检索。
- **DeepSeek Responses API + 原生联网搜索**：接入 Responses 协议与 DeepSeek 服务端原生 web search。
- **`hf logs` 子命令**：当前日志仅按 `HEARTFLOW_LOG` 走 stderr、不落盘；要支持一条命令直出历史日志需先引入文件 sink（`tracing-appender`）+ 存储目录 + 轮转策略。
- **全局开关 `--no-confirm` / `--color`**：非交互危险命令的确认策略已在 `--help` 的 INTERACTION CONTRACT 文档化；显式开关会改变安全/渲染语义，按需再评估。
