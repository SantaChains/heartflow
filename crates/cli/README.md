# heartflow

heartflow 的终端外壳:REPL 行编辑、流式渲染、权限模式与 slash 命令都在这一层,底下复用 `heartflow-runtime`(会话与任务循环)、`heartflow-provider`(provider 配置与协议分发)、`heartflow-tools`(原生工具)、`heartflow-mcp`(MCP 客户端)、`heartflow-store`(SQLite 历史)。本 crate 不承载业务语义。

二进制命令是 `hf`,不是 `heartflow`。

## 模块结构

- main.rs 入口与 REPL:参数解析(clap)、一次性模式(`-p` / stdin 管道 / `--json` / `--quiet`)、回合循环装配、权限提示(`CliPermissionPrompter`)、`/mode` 与 `/plan`、`hf doctor` 自检。渲染与执行在此彻底解耦。
- editor.rs 行编辑:括号/引号配对补全、历史、CJK 宽度对齐的光标与重绘(宽字符不裂、IME 锚定真实光标),粘贴突发只重绘一次。
- render.rs / theme.rs markdown 与代码高亮渲染、配色令牌;mascot.rs 是纯几何绘制的终端伴侣(无图片资源、无第三方动画框架)。
- viewport_term.rs 内联输入视口的终端能力探测;config.rs 配置导出/导入与 MCP 段落的 TOML 往返;core.rs 启动横幅与提示条。

## 安装

```bash
cargo install heartflow
```

Windows 也可用便携包与 scoop,通道见仓库 README。

## 快速开始

```bash
export DEEPSEEK_API_KEY=...      # 或 ANTHROPIC_API_KEY,或写入 ~/.heartflow/config.toml
hf                               # 进入 REPL(hf chat 等价)
hf --resume                      # 重开历史会话
hf doctor                        # 诊断配置、目录与 provider
```

完整用法、工具清单与设计取舍见仓库 [README](https://github.com/SantaChains/heartflow) 与 [docs/src](https://github.com/SantaChains/heartflow/tree/main/docs/src)。