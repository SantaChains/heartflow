# AI 文档集成

本页记录 heartflow 面向"文档可被人和 AI 工具消费"的整套适配：文档站点（web/wiki）、API 文档（rustdoc）、`llms.txt`、Context7、DeepWiki 与 GitHub Wiki。地基是 `docs/` 下的 mdBook——其余各项都从中派生。

## 文档站（web / wiki）

- **引擎**：mdBook（纯 Rust，零 Node，与发布流水线取向一致）。源在 `docs/src/`，目录见 `docs/src/SUMMARY.md`，输出到 `docs/book/`（已 gitignore）。
- **本地预览**：`cargo install mdbook && mdbook serve docs -p 3000 --open`（mdBook 0.5+；book 根为 `docs`，输出默认 `docs/book`）。
- **部署**：Pages 工作流是 `.github/workflows/docs.yml`，在 push 到 main 且 `docs/**`、`scripts/gen-llms.sh` 或该 workflow 本身有变动时触发，也可 `workflow_dispatch` 手动跑。唯一的一次性前提：仓库 Settings → Pages → Source 选 "GitHub Actions"，否则 deploy 那步会失败。上自定义域名时，除把 `docs/book.toml` 的 `site-url` 改成 `/` 并填 `cname`，还要改 workflow 里 gen-llms 步骤的 `SITE_URL`（该变量名固定，写成别的名字不报错但会被静默忽略）。项目页默认子路径为 `/heartflow/`，站点 <https://santachains.github.io/heartflow/>。

## API 文档（rustdoc）

同一 workflow 跑 `cargo doc --workspace --no-deps`，把产物挂到站点 `/api` 路径，与用户手册并列。用户手册讲"怎么用"，rustdoc 讲"每个公开项的签名"。

## llms.txt

`scripts/gen-llms.sh` 按 **llms.txt 规范 v2**（<https://llmstxt.org>）生成两份供 AI 读取的产物，**默认落在仓库根并已随仓库提交**（不必等 Pages 上线，raw.githubusercontent 当下即可取用）：

- `llms.txt`：H1 站名 + 摘要 blockquote + details 段，随后 `## Documentation` / `## Optional` 两个文件列表，每项链接到**对应章节的原始 markdown**（v2 要求链接 LLM 友好版而非 HTML），次要页（路线图/贡献）归入 `## Optional` 供 agent 按需跳过。
- `llms-full.txt`：全部章节按 SUMMARY 顺序拼接成自包含单文件，供一次性读入。

改文档后重跑 `bash scripts/gen-llms.sh` 刷新根目录两份文件（注意本机 PATH 上的 `bash` 若不是 GNU bash，脚本会因缺 `mapfile` 而失败，用 Git 的 bash）。Pages 工作流跑的是 `bash scripts/gen-llms.sh docs/book docs/src`，另生成一份到站根。Cursor `#Docs` 与各家 MCP 摄取器直接添加 `https://raw.githubusercontent.com/SantaChains/heartflow/main/llms.txt`（或 `<site>/llms.txt`）即可。mdBook 另有 `mdbook-llms-txt` 后端可作原生替代，当前用零依赖脚本自足。

## Context7

Context7 把仓库文档解析→抽取代码片段→向量索引，供 AI 助手实时检索。**要求仓库含 md/mdx/txt/rst 格式的实际文档**——这正是 `docs/src/` 提供的。两步：

1. **提交收录**（自助，无需所有者身份）：到 <https://context7.com/add-library> 选 GitHub 标签，粘贴 `https://github.com/SantaChains/heartflow`，提交后得到库 ID `/santachains/heartflow`。入库前该地址返回 404。
2. **约束解析范围**：仓库根 `context7.json`（Context7 的 robots.txt 式约定，只在解析时读取）把索引范围收在 `docs/src`，排除生成物 `docs/book` 与 `docs/wiki`，并把安装口径与 `hf` 命令名作为 rules 直接投喂给调用方 agent。字段以官方 <https://context7.com/docs/library-owners> 为准。

收录后，用户提问时加 `use context7` 即可命中最新文档。README 已挂 Context7 徽章链接。文档每次 push 后由 Context7 按热度自动刷新，不必额外接线。

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

两个一次性前提（仓库设置，文件里配不了）：Settings → Features 勾上 Wikis；再到 wiki 页面点一次 Create the first page 保存。第二点不能省——首次建页之前 GitHub 尚未创建 wiki 的 git 仓库，`clone .../heartflow.wiki.git` 会报 Repository not found。

建议保持精简：`Home.md` 做导航并指向本文档站，避免与 mdBook 双份长文漂移。
