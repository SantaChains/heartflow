# heartflow 会话安全分析

> 格式约定：事实带 file:line；风险评级高/中/低；【已修】= 本批落地，【缓修】= 已列入待办。本文件供 AI 复读，规则密集、少叙事。

## 一、模型：三层防线现状

```
L1 进程边界   API key 只在 env/config → 内存 ProviderProfile，永不出 header
L2 落盘咽喉   save_session 唯一写点：redact_for_save → JSON + SQLite mirror
L3 旁路落盘   jobs / cassette / checkpoint / 后台日志 ← 本批前无防线
```

| 层 | 控制点 | 状态 |
|---|---|---|
| L1 | key 只进 header（x-api-key / Authorization），永在 URL 之外；rustls 默认链验证，无 danger_accept_invalid_certs；显式代理（proxy crate）而非 reqwest 隐式读 env | ✅ |
| L2 | `redact_for_save`（storage.rs:199）：字面量注册集（secrets ≥4 字符）+ 4 条结构规则；SQLite mirror 收已脱敏 clone；compact 变短触发 force_full 重扫（storage.rs:371-384） | ✅ |
| L3 | 四条旁路各自为政 | ⚠️ 本批收口两条 |

## 二、redaction 规则面（redact.rs:33-95）

字面量先于结构规则，统一替换 `[REDACTED]`：
1. `scheme://user:pass@` → userinfo 抹除
2. `Authorization: Bearer|Basic <tok>`（含 `=` 形态）
3. 标签赋值：`api_key|apikey|secret_key|access_token|refresh_token|auth_token|client_secret|password|passwd` 后跟值 ≥6 字符（`token` 单词故意排除防 `max_tokens`）
4. 前缀 key：`sk-[A-Za-z0-9_-]{16,}` / `gh[pousr]_…{20,}` / `AKIA[0-9A-Z]{16}`

**已知盲区**（结构性，不建议硬修）：zhipu `id.secret` 无前缀形态靠字面量注册兜底；URL path/query token；Digest/token scheme。

## 三、子进程环境面

- **bash 工具**：spawn 前按名字形状清洗（`*_API_KEY|*_APIKEY|*_ACCESS_KEY|*_SECRET*|*_TOKEN|*_PASSWORD|*_PASSWD|*_CREDENTIAL*` 后缀 + `TOKEN|API_KEY|APIKEY|SECRET|PASSWORD` 全名，bash.rs:430-466），前景/后台两条路都应用。`dangerouslyDisableSandbox=true` 是显式 opt-out（模型可触发、需权限门批准）。
- **MCP stdio【已修 D3】**：transport.rs 原全量继承父 env（provider key 可达，进程生命周期长于单次工具调用）。现与 bash 共用 `runtime::scrub_credential_env`，config `env` 增量在 scrub 之后追加（显式配置仍可达自己的 server）。新依赖 mcp→runtime，无环。
- **测试**：bash.rs:967-1035 已验证 ANTHROPIC_API_KEY/DEEPSEEK_API_KEY/GITHUB_TOKEN/AWS_* 不可达；MCP 侧沿用同一实现，不做重复测试。

## 四、代理面（crates/proxy）

- 优先级：config `[proxy]` > `HEARTFLOW_PROXY*` > `HTTPS_PROXY→ALL_PROXY→HTTP_PROXY` + `NO_PROXY` > WinINET 注册表 > direct；`direct` 模式压倒一切；loopback/LAN 永远直连。
- socks4/4a/5/5h 全支持（reqwest socks feature）。
- **凭据泄露点【已修 D1】**：三处曾打原文——探测失败 warn 打完整 Url（lib.rs:544）、env 解析失败 warning 带 `{name}={raw:?}`（env.rs:61）、config address 解析失败带 `{raw:?}`（lib.rs:250）、`hf doctor` 的 describe() 打完整地址（lib.rs:437）。现全部经 `display_redacted`（userinfo → `[REDACTED]`）或省略 raw 值；测试钉死 `describe_never_echoes_proxy_userinfo`。
- 代理 URL 凭据不进 system prompt、不进 session JSON（proxy 地址不参与消息内容）；config 导出回写时 `redact_proxy_address` 抹 userinfo（provider/config.rs:356-390）。
- **残余信任边界**：HTTPS 经代理 CONNECT 时代理方可见 SNI 与元数据，内容仍 TLS 保护——代理 = 信任决策，文档级提示即可，不做代码。

## 五、旁路落盘面（本批审计结论）

| 通道 | 内容 | redact？ | 处置 |
|---|---|---|---|
| jobs `events.jsonl` / `status.json` | worker 结果全文 / 前 160 字符 | ❌（jobs.rs:515-526） | 【缓修 D2】需把 secrets 集传入 jobs 写点，动签名面，随下批 |
| cassette record | AgentEvent 层请求+事件 | ❌（cassette.rs:71-74） | 【缓修 D7】用户显式开启，且 replay 校验依赖原文一致性——加 redact 会破坏回放；文档警示即可 |
| checkpoint 影子 git | 工作区快照（尊重 .gitignore，排除 .heartflow/） | ❌ | 【缓修 D4】根源是用户未 gitignore 含密文件；建议 doctor 加一条检查项 |
| 后台 bash 日志 `%TEMP%/heartflow-bg/*.log` | 子进程 stdout/stderr 3 天 | ❌ | 【可接受】子进程 env 已清洗，命令自身回显密钥属用户行为 |
| 4xx body 回显 `ApiError::Display` | 服务器响应原文 → 屏幕 + transcript | —（不含 key） | 【已修 D10】`truncate_body_echo` 2 KiB char-boundary 截断（error.rs:178） |

## 六、凭据注册面

注册点 3 处（main.rs:4428-4440）：`ANTHROPIC_API_KEY`（Env 模式）、`profile.api_key`、`profile.auth_token`。
【已修 D5】追加 `HEARTFLOW_IMAGE_API_KEY`（image 工具独立 key，tools/image.rs:181，此前错误回显无字面量兜底）。
【缓修 D6】base_url 内嵌 token / proxy userinfo 值不注册——出现频率极低，结构性规则已覆盖 `user:pass@` 形态。

## 七、明文存量与权限【缓修】

- **D8**：inline `[provider] api_key` 用户自写明文落 config.toml（config.rs:137-141）。hf 自身导出不回写该字段（318-328，有测试），但存量明文无法由程序消除——文档建议：优先 `api_key_env`。
- **D9**：sessions/SQLite/checkpoint 全默认权限继承（Unix 0644 可他用户读；Windows 用户 profile ACL 内单用户，风险有限）。全仓无 set_permissions；跨平台 0600 需处理 Windows ACL（无 winside 依赖），单列后批。

## 八、会话更安全/稳定/好用的行动清单（优先级序）

1. ✅ D1/D3/D5/D10 本批已修（各带测试或共享实现）
2. 【D2】jobs 写点接 redact：把 `registered_secrets` 传入 jobs worker 初始化，append_event 与 result_head 前过 `redact_text`
3. 【D4】`hf doctor` 增加检查：工作区含 `.env`/`*key*` 等未 gitignore 文件时警告会进 checkpoint 影子库
4. 【D9】save 路径 0600：Windows 侧用现有 ACL（用户目录默认私有，可降级处理），Unix `PermissionsExt::set_mode(0o600)`
5. 【D8】`hf doctor` 警告 inline api_key 明文，提示改 `api_key_env`
6. 稳定面已由 ST-1/ST-2 覆盖：idle timeout、blocks 落 transcript、步级重试、压缩降级（见 stability.md）

## 九、检测面（防泄漏的观测点）

- 无 telemetry（grep 零命中）——不外发任何数据，检测只能本地做。
- 本地可观测点：`hf doctor`（代理/config/凭据形态）、panic 棘轮、`.heartflow` 目录体检（D4/D8 的 doctor 检查项即此思路）。
- 事后检测盲区：jobs/cassette 落盘发生时无任何提示——D2 修复后可加一条「result 含疑似密钥已被脱敏」计数进 compaction-stats.json（仅计数器，无内容，安全）。
