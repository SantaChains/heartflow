# 快速开始

```bash
# 方式一：环境变量（默认 anthropic 协议）
export ANTHROPIC_AUTH_TOKEN=sk-...

# 方式二：DeepSeek（OpenAI 方言，密钥走环境变量名）
export DEEPSEEK_API_KEY=sk-...
hf --provider deepseek
```

无参数启动即进入 REPL。输入基于 ratatui 内联视口（保留原生 scrollback、真实光标供 CJK/IME 候选）：Enter 发送，Shift/Alt+Enter 或 Ctrl+J 换行，输入 `/` 在下方弹出可选命令列表（↑/↓ 选择、高亮项按 Enter 或 Tab 补全、Esc 取消高亮；无高亮时 Enter 原样发送），空闲时 ↑/↓ 翻历史，Ctrl+C 取消当前回合（空闲行则仅清空），`/exit` 退出。

> **进不去 REPL / 输出乱码？** 几乎都是终端环境问题而非程序故障。一是密钥只在别的 shell 会话里设过：在**当前**终端重新 `export`（Windows 用 `setx` 后要重启终端），再 `hf doctor` 复核 provider 与密钥是否解析成功。二是 Windows 控制台默认 GBK 代码页把 UTF-8 显示成乱码（库内字节始终正确）：执行 `chcp 65001` 或 `[Console]::OutputEncoding=[Text.Encoding]::UTF8`，并换用支持中文的等宽字体即可。
