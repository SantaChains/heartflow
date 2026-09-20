# Agent 资产

仓库指令文件以 `AGENTS.md`（含 `AGENTS.local.md`、逐级向上到仓库根）为主注入系统提示词；仅当同目录没有 `AGENTS.md` 时才回退读取旧名 `CLAUDE.md`（排他回退，避免双注入）；`hf init` / REPL `/init` 生成中性的 `AGENTS.md` 骨架。规则与技能分用户层与项目层，项目同名覆盖用户。

```text
AGENTS.md / CLAUDE.md    仓库指令（项目上下文，逐级向上聚合）
~/.agent/rules/*.md      全量注入（按名排序，上限 32 个，单个截断 32KB）
~/.agent/skills/*/SKILL.md  仅注入 name/description 元数据，正文按需读取
.agent/...               项目层同构
```

## 自迭代记忆

`~/.heartflow/MEMORY.md`（或项目 `.heartflow/MEMORY.md`）作为跨会话的坑/决策/偏好记录，以极小 token（截断 4KB）注入系统提示词的 Memory 段；`/remember` 手动追加（去重），任务环遇硬坑自动记录（非向量嵌入）。user 与 project 两层各自按行公平夹紧，避免一层饿死另一层。
