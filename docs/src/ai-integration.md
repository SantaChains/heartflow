# AI 文档集成

本页记录 heartflow 面向"文档可被人和 AI 工具消费"的整套适配：文档站点（web/wiki）、API 文档（rustdoc）、`llms.txt`、Context7、DeepWiki 与 GitHub Wiki。地基是 `docs/` 下的 mdBook——其余各项都从中派生。

## 文档站（web / wiki）

- **引擎**：mdBook（纯 Rust，零 Node，与发布流水线取向一致）。源在 `docs/src/`，目录见 `docs/src/SUMMARY.md`，输出到 `docs/book/`（已 gitignore）。
- **本地预览**：`cargo install mdbook && mdbook serve docs -p 3000 --open`（mdBook 0.5+；book 根为 `docs`，输出默认 `docs/book`）。
- **部署（当前暂未启用）**：Pages 工作流以 `.github/workflows/docs.yml.bak` 形式暂存（与仓库 `ci.yml.bak` 约定一致，GitHub 不识别 `.bak` 故不触发）。启用三步：① 改回 `docs.yml`；② 仓库 Settings → Pages → Source 选 "GitHub Actions"；③ 如需自定义域名，把 `docs/book.toml` 的 `site-url` 改 `/` 并填 `cname`。项目页默认子路径为 `/heartflow/`。

## API 文档（rustdoc）

同一 workflow 跑 `cargo doc --workspace --no-deps`，把产物挂到站点 `/api` 路径，与用户手册并列。用户手册讲"怎么用"，rustdoc 讲"每个公开项的签名"。

## llms.txt

`scripts/gen-llms.sh` 按 **llms.txt 规范 v2**（<https://llmstxt.org>）生成两份供 AI 读取的产物，**默认落在仓库根并已随仓库提交**（Pages 未上线时也可经 raw.githubusercontent 直接取用）：

- `llms.txt`：H1 站名 + 摘要 blockquote + details 段，随后 `## Documentation` / `## Optional` 两个文件列表，每项链接到**对应章节的原始 markdown**（v2 要求链接 LLM 友好版而非 HTML），次要页（路线图/贡献）归入 `## Optional` 供 agent 按需跳过。
- `llms-full.txt`：全部章节按 SUMMARY 顺序拼接成自包含单文件，供一次性读入。

改文档后重跑 `bash scripts/gen-llms.sh` 刷新根目录两份文件；Pages 上线后工作流会以 `bash scripts/gen-llms.sh docs/book docs/src` 另生成到站根。Cursor `#Docs` 与各家 MCP 摄取器直接添加 `https://raw.githubusercontent.com/SantaChains/heartflow/main/llms.txt`（或上线后的 `<site>/llms.txt`）即可。mdBook 另有 `mdbook-llms-txt` 后端可作原生替代，当前用零依赖脚本自足。

## Context7

Context7 把仓库文档解析→抽取代码片段→向量索引，供 AI 助手实时检索。**要求仓库含 md/mdx/txt/rst 格式的实际文档**——这正是 `docs/src/` 提供的。收录两种方式：

1. **Web（推荐，自动处理）**：到 <https://context7.com/add-library> 粘贴仓库或文档站 URL，处理后得到库 ID `/SantaChains/heartflow`。
2. **声明式 PR**：按 `docs/context7.json` 的字段向 Context7 仓库提 PR；合并后即索引。**该 JSON 的键需以 add-library 页面给出的当前 schema 为准**（本仓库里的 `docs/context7.json` 是可直接提交的初稿，字段若被上游调整以官方为准）。

收录后，用户提问时加 `use context7` 即可命中最新文档。README 已挂 Context7 徽章链接。

## DeepWiki

- **零配置浏览**：把任意 heartflow 页面 URL 的 `github.com` 改成 `deepwiki.com`，或在 <https://deepwiki.com> "Add repo" 粘贴 `SantaChains/heartflow`。公开仓库免费，自动出 wiki + 架构图 + RAG 问答。
- **可控化**：仓库根 `.devin/wiki.json` 的 `repo_notes` 引导生成，`pages`（若提供）会跳过自动聚类、严格按指定页面生成，确保关键模块不被漏。内容见该文件；随文档结构稳定后逐步补 `pages`。
- **自托管（可选）**：如需私有部署或自带模型，用 [deepwiki-open](https://github.com/AsyncFuncAI/deepwiki-open)（Docker，支持 PAT 私有仓库）或其 GitHub Action 重生成并发到 Pages。会引入模型 API key 与外呼，按需评估。

## GitHub Wiki

GitHub Wiki 是独立仓库（`heartflow.wiki.git`），不与主仓库共用工作树。种子页在 `docs/wiki/`（`Home.md` 等），发布方式：

```bash
git clone https://github.com/SantaChains/heartflow.wiki.git wiki-checkout
cp docs/wiki/*.md wiki-checkout/
cd wiki-checkout && git add -A && git commit -m "docs: seed wiki" && git push
```

建议保持精简：`Home.md` 做导航并指向本文档站，避免与 mdBook 双份长文漂移。
