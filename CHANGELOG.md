
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

