# REPL 命令

```text
/help     帮助              /model [NAME] 显示或切换模型
/mode     [NAME] 显示或切换权限模式（read-only/workspace-write/full）
/status   会话状态          /compact      手动强制压缩会话历史（忽略阈值）
/pin      切换末条消息的永不压缩标记（跨 /compact 逐字存活）
/save     立即持久化        /clear        开启新会话
/sessions 列出已存会话      /open N       跳回第 N 个已存会话（同 /sessions 编号）
/remember T 追加一条长期记忆到 ~/.heartflow/MEMORY.md（自动去重）
/mcp      列出 MCP 服务器与工具
/search Q 全文检索历史      /exit         退出（自动保存并打印 resume 命令；/quit 为别名）
/init     生成 AGENTS.md 骨架  /expand [ID]  展开上次折叠的工具输出
/guide T  本地零 token 组装“前情/现状/下一步”三段引导草稿，供编辑后发送
/queue [pop|clear]  查看/撤回回合运行期间排队的后续消息（入队注入随 TUI 事件循环上线）
!CMD  不经模型直接跑一条 shell 命令取输出（Windows 走 pwsh，危险命令先确认，输出同工具一样可折叠/展开）
/plan GOAL 规划先行（写仅门禁到 plans/）  /plan approve 逐任务新鲜上下文执行+收尾复盘  /plan end 退出规划
/restart 以全新的配置与 MCP 装载重启进程（链式重启受深度上限约束）
```

## 权限模式

`read-only` / `workspace-write` / `full` 三档，工具级覆盖，REPL 内 `/mode` 热切换（`auto` 归一为 `full`；`HEARTFLOW_PERMISSION_MODE` 另接受 `plan`）。`/plan` 规划模式是硬门禁：仅可写 `.heartflow/plans/*.md`，其余写/bash 一律拒绝；审批后进入 Hermes 任务环逐任务在新鲜上下文执行，收尾把复盘写入 `.heartflow/reflections/`。`read-only` 与 `plan` 两档自动放行标注为只读的 MCP 工具。
