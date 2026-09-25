#!/usr/bin/env bash
# 按 llms.txt 规范 v2(llmstxt.org)生成 llms.txt(索引)与 llms-full.txt(全站拼接)。
# 规范要点:H1 站名(唯一必填) → blockquote 摘要 → 非标题的 details 段 → 若干 H2 文件列表;
# 链接指向 LLM 友好的 markdown 版本(此处用 GitHub raw .md,当下即可达);次要页归入 "## Optional"。
# 用法: bash scripts/gen-llms.sh [OUT_DIR] [SRC_DIR]
#   OUT_DIR 默认 "." → 产出可提交的仓库根 llms.txt/llms-full.txt;Pages 工作流传 docs/book。
set -euo pipefail

OUT_DIR="${1:-.}"
SRC_DIR="${2:-docs/src}"
RAW_BASE="${RAW_BASE:-https://raw.githubusercontent.com/SantaChains/heartflow/main/docs/src}"
SITE_URL="${SITE_URL:-https://santachains.github.io/heartflow}"
# 归入 ## Optional 的文件(agent 可按需跳过的次要页),空格分隔。
LLM_TXT_OPTIONAL="${LLM_TXT_OPTIONAL:-roadmap.md contributing.md}"

[ -d "$OUT_DIR" ] || { echo "gen-llms: no output dir '$OUT_DIR'" >&2; exit 1; }
SUMMARY="$SRC_DIR/SUMMARY.md"
[ -f "$SUMMARY" ] || { echo "gen-llms: no $SUMMARY" >&2; exit 1; }

# 按 SUMMARY 顺序抽取 .md 链接(去重保序),跳过 SUMMARY 自身。
mapfile -t pages < <(grep -oE '\]\([^)]+\.md\)' "$SUMMARY" | sed -E 's/\]\(([^)]+)\)/\1/' | awk '!seen[$0]++')

is_optional() { case " $LLM_TXT_OPTIONAL " in *" $1 "*) return 0 ;; *) return 1 ;; esac; }

full="$OUT_DIR/llms-full.txt"
idx="$OUT_DIR/llms.txt"

# 生成单条列表项: - [title](url): desc(无 desc 则省略冒号)
emit_item() { # file
  local f="$SRC_DIR/$1" title desc
  title="$(grep -m1 -E '^# ' "$f" | sed -E 's/^# +//')"; [ -n "$title" ] || title="${1%.md}"
  # 描述取 H1 之后首个正文行(跳过围栏代码块/引用/列表/表格),不做字节截断以免切断多字节字符
  desc="$(awk '!seen&&/^# /{seen=1;next} seen{if(/^```/){f=!f;next} if(f)next; if(NF&&$0!~/^([`>|*-]|#{1,6} )/){print;exit}}' "$f")"
  if [ -n "$desc" ]; then printf -- '- [%s](%s/%s): %s\n' "$title" "$RAW_BASE" "$1" "$desc"
  else printf -- '- [%s](%s/%s)\n' "$title" "$RAW_BASE" "$1"; fi
}

# ---- llms.txt ----
{
  printf '# heartflow\n\n'
  printf '> Rust 实现的终端 AI agent,命令名 hf:流式多轮协作,三方言(Anthropic/OpenAI Chat/OpenAI Responses)、原生工具、MCP 客户端、任务环、SQLite FTS5 会话检索、上下文压缩。\n\n'
  printf '安装: `scoop bucket add heartflow https://github.com/SantaChains/heartflow && scoop install heartflow`(或 `cargo install heartflow`)。\n'
  printf '首次运行前设 `DEEPSEEK_API_KEY` 或 `ANTHROPIC_API_KEY`,`hf --provider deepseek` 进入 REPL。文档源在 `docs/src`,以 mdBook 构建;本文件由 `scripts/gen-llms.sh` 生成,链接均指向原始 markdown。\n'
  printf '\n## Documentation\n\n'
} > "$idx"
for p in "${pages[@]}"; do is_optional "$p" || emit_item "$p" >> "$idx"; done
if [ -n "${LLM_TXT_OPTIONAL// /}" ]; then
  printf '\n## Optional\n\n' >> "$idx"
  for p in "${pages[@]}"; do is_optional "$p" && emit_item "$p" >> "$idx"; done
fi

# ---- llms-full.txt(全站内容拼接,自包含) ----
{
  printf '# heartflow\n\n'
  printf '> Rust 终端 AI agent(命令名 hf)。本文件为文档站各章节全文拼接,供 AI 一次性读入。\n'
  printf '> 源码 https://github.com/SantaChains/heartflow · 站点 %s\n' "$SITE_URL"
} > "$full"
for p in "${pages[@]}"; do
  f="$SRC_DIR/$p"; [ -f "$f" ] || continue
  printf '\n\n---\n\n<!-- file: %s -->\n\n' "$p" >> "$full"
  cat "$f" >> "$full"
done

echo "gen-llms: wrote $idx (${#pages[@]} pages) 和 $full" >&2
