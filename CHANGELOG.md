
# heartflow 1.1.0 - 2026-09-20

## 性能优化

- Vectorized ASCII fast path for token estimation (`82593c2`)

- Compact the session in place without full rescans (`fd6d541`)

- Glob_search honors .gitignore via ignore+globset, add tool benchmark (`6da8811`)

- Switch hf to the mimalloc global allocator (`a5bbfa1`)

- Cache syntect globally and parallelize MCP HTTP requests (`c1d1d26`)

- **runtime**: Amortized-O(1) session token estimation (`22b988f`)

- **runtime**: Cache compiled JSON-Schema validators (`2eabe38`)

- **runtime,api**: Parallel grep crawler, atomic writes, wire compression (`e9e12a6`)

- **runtime**: Serialize mutating tools; run read-only batches in parallel (`712ae6c`)

- **cli**: Mirror history incrementally under a stable conversation key (`7070b57`)

- **store**: Reuse prepared INSERTs in save_session hot loop (`2888c6f`)


## 新功能

- Geometric heart companion and incremental redaction on save (`43ff2ad`)

- Reshape attachments down to the vision grid (`71f13d7`)

- Restore sessions from snapshot plus append-only segment (`326daad`)

- Add performance regression gate script (`31a719c`)

- Suggest the closest snippet when old_string misses (`1b8f2b1`)

- Cap execute_bash output streams at 512 KB (`b6f7cf9`)

- Fold oversized diffs returned by edit tools (`f2f315b`)

- Background bash tasks log to a file; extend the tool benchmark (`dce5dad`)

- Glob_search accepts multiple globs in one crawl (globset) (`8d2de7f`)

- Export rga_available for conditional tool registration (`954219e`)

- Register search_documents only when rga exists, update search guidance (`f4f6f72`)

- Add search_documents tool backed by external rga (`ebca401`)

- **api,provider,runtime,tools,cli**: Image attachments end to end (`a984f4f`)

- **cli,runtime**: Half-block mascot, Kurosawa theme, transcript redaction (`ef7d5ac`)

- **tools,cli**: Add web_search, structure-preserving web_fetch, geometry golden tests (`a932933`)

- **cli**: Render mascot as braille dot-matrix blob, faster tick (`e8ef72f`)

- **cli**: Add ASCII/geometric mascot companion (Route A, spring-driven) (`a449b9a`)

- **store**: Add append_messages incremental tail-write fast path (`1b0d7fc`)

- **cli**: Make terminal palette overridable via theme.toml (`a8eb4c9`)

- **cli**: Unify terminal colors/layout into a single Theme source (`682c840`)

- **cli**: Replace reedline with a ratatui + tui-textarea inline input editor (`94fed8c`)

- **cli**: Add ! shell pass-through running the shared bash tool off the async reactor (`64b3dfb`)

- **cli**: Add session core state layer with follow-up queue and local guide assembly (`26c165c`)


## 缺陷修复

- Doc_search timeout-thread drain and honest truncation flag (`2e8dcd9`)

- Retry idempotent file reads once on transient OS errors (`a315239`)

- **cli**: Grok-bot mascot redesign, cursor flicker and anchor drift (`54d2979`)

- **cli**: Embed windows version resource in hf.exe (`15a3e9c`)


## 重构

- **workspace**: Extract heartflow-provider crate (`5e0c1b4`)


# heartflow 1.0.1 - 2026-09-19

## 缺陷修复

- **scoop**: Hash extraction field is find, not regex (`bb45a6b`)


# heartflow 1.0.0 - 2026-09-19

## 其他

- Harden memory clamp + replay view found in review (`69dbb52`)

- Make Ctrl+C a reliable, discoverable turn interrupt (`b464c75`)

- Make permission, todo-threshold and Design-ranking lines actionable (`f2a9da8`)

- Redact secret-named config values before injecting into the system prompt (`2b72c1e`)

- Tunable replay tail + Ctrl+D saves session and prints resume hint (`fa29cdc`)

- Lean replay view + fair memory clamping on resume (`bdc4bc3`)


## 新功能

- **cli**: Document interaction contract, exit codes, add explicit chat alias (`d60d9fc`)


## 缺陷修复

- **release**: Touch CHANGELOG before git-cliff prepend (missing on first release) (`c69a443`)

- **release**: Git-cliff prepend rejects same-file output, drop -o from CHANGELOG (`599a47e`)

- **scoop**: Checkver github expects user/repo, not full url (`a21fe06`)

- **cli**: Make --resume enter the REPL with the restored session; add -v version (`99e7760`)

- **prompt**: Align tool names to specs and advertise openmemory L79 tools (`a575181`)


## 重构

- **prompt**: Rewrite system prompt in layered natural language, drop markdown scaffolding (`6f1bdc3`)

- **cli**: Collapse resume positionals into --resume[=PATH] plus --run (`e2dd5eb`)

- Replace bounded schema checker with jsonschema crate; dedup helpers (`43192b2`)

