# CLI 用法

```text
hf [--provider NAME] [--model MODEL]              进入交互 REPL（等价 hf chat）
hf [--provider NAME] [--model MODEL] prompt TEXT  单次提问，流式输出
hf prompt -q|--quiet TEXT                         只打印答案（去掉进度/用量行，脚本友好）
hf prompt --json TEXT                             输出 {text, usage, session_id} 结构化 JSON
echo TEXT | hf prompt "指令"                       stdin 作为上下文与指令拼接（Unix 管道）
hf search QUERY [--limit N] [--json]              跨会话全文检索历史（非交互，可管道）
hf --resume[=SESSION.json] [--run /compact]       恢复会话（省略 PATH 进选择器），--run 恢复后立即执行 slash 命令
hf config export [--output FILE]                  导出配置（不含密钥）
hf config import FILE                             导入配置（自动备份 .bak）
hf doctor [--fix] [--ai]                          诊断环境（含历史库完整性）；--fix 应用安全修复，--ai 请内置模型给修复建议
hf init [--force]                                 在当前目录生成 AGENTS.md 指令骨架（已存在不动，--force 覆盖）
hf --provider NAME models [--balance]             provider 自举：列模型、报当前模型上下文窗口；--balance 才查余额（省额度）
hf system-prompt [--cwd PATH] [--date YYYY-MM-DD] 打印系统提示词
hf -v | -V | --version                            打印版本号
```

`hf prompt` 的 TEXT 省略时从 stdin 读提示词；`hf search` 的 `--limit` 默认 20。

## 交互契约与退出码

- **交互式**：无参数或 `hf chat` 打开 REPL（唯一能弹确认的模式）。
- **恢复**：`hf --resume[=PATH] [--run "/cmd"]` 重开已存会话（省略 PATH 进选择器；值形式必须用 `=`）。
- **非交互**：子命令（prompt/search/…）绝不阻塞等人。因无 tty 应答确认，`prompt` 在 `HEARTFLOW_PERMISSION_MODE` 的权限模式下跑工具，默认 `full`（自动放行）；无人值守只读管道设 `HEARTFLOW_PERMISSION_MODE=read-only`。

| 退出码 | 含义 |
|--------|------|
| 0 | 成功 |
| 1 | 运行时/提供方错误（流、配置解析、失败的回合） |
| 2 | 用法错误（参数非法，由解析器发出） |
