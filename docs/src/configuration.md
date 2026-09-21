# 配置

优先级从高到低：CLI 参数 > 项目 `.heartflow/config.toml` > 用户 `~/.heartflow/config.toml` > 内置 provider 表 > 环境变量。同名字段逐项覆盖，坏字段跳过并告警，单条配置不阻断启动。

REPL 运行期间编辑并保存 `config.toml` / `theme.toml` / `keymap.toml` / `settings.toml` 任一，下一回合边界会自动热重载（`config` 重建 provider 并保留当前会话与权限模式，`theme`/`keymap`/`settings` 就地生效；逐面上报，改 theme 不触发 config 重载）。环境异常可用 `hf doctor` 诊断，`hf doctor --fix` 应用安全修复。

```toml
version = 1

[provider.deepseek]
protocol = "openai"          # anthropic | openai(兼容别名 openai-compatible/openai_compat) | openai-responses(别名 responses)
base_url = "https://api.deepseek.com/v1"
api_key_env = "DEEPSEEK_API_KEY"   # 只写环境变量名；也可用 api_key 直接内联明文（导出时永不写出）
auth_token_env = "DEEPSEEK_AUTH_TOKEN"  # 可选：Bearer token 来源变量
model = "deepseek-chat"
max_tokens = 4096              # 缺省 4096
reasoning_effort = "high"      # 思考等级：openai 协议直传，anthropic 协议映射为 extended thinking 预算
context_window = 65536         # 模型上下文窗口 tokens，驱动回合内 >50% 预压缩（等价于 HEARTFLOW_AUTO_COMPACT_TOKENS，后者优先）

[provider.my-proxy]
protocol = "anthropic"
base_url = "https://my-proxy.example.com"
api_key_env = "MY_PROXY_TOKEN"

[mcp.servers.filesystem]
command = "npx"
args = ["-y", "@modelcontextprotocol/server-filesystem", "/tmp"]
env = { "NODE_OPTIONS" = "--max-old-space-size=512" }  # 子进程环境变量

[mcp.servers.remote-search]            # 远程/网络型 MCP：给 url 即走 Streamable-HTTP/SSE（无需 command）
url = "https://mcp.example.com/stream" # 与 command 二选一；两者都缺则跳过并告警
headers = { "X-Tenant" = "acme" }      # 额外请求头；值支持 ${ENV_VAR} 展开
bearer_token_env = "SEARCH_MCP_TOKEN"  # 从此环境变量读 Bearer token（为空则不发）
read_only = true                       # 声明该 server 工具均只读：read-only/plan 模式下也放行
```

`[provider]` 的字段全部可选，未识别字段仅记 debug 日志，坏字段跳过而不阻断启动；`version` 高于当前 schema（1）时按现有 schema 解析并告警。

## 内置 provider

- `deepseek`：OpenAI 协议，`api_key_env = "DEEPSEEK_API_KEY"`，默认模型 `deepseek-v4-flash`。
- `anthropic`：Anthropic 协议，`api_key_env = "ANTHROPIC_API_KEY"`，基址可被 `ANTHROPIC_BASE_URL` 覆盖。
- 任意 `[provider.NAME]` 表项即自定义 provider；都未命中且无内置名时回退到环境变量选路，此时默认模型为 `mimo-v2.5-pro`。

## 终端配色（theme.toml）

配色以单一主题源为基准（内置色板取自黑泽明电影的实物取色，一角色一色相、暖冷协调，流式渲染与输入框共用同一色族），可按语义角色覆盖。文件位于 `~/.heartflow/theme.toml`（用户）与 `.heartflow/theme.toml`（项目，逐项胜出）。每个值取 `#RRGGBB`，坏值跳过并告警、绝不阻断启动；缺省沿用内置色板。改后于下一回合边界热重载生效（进程级缓存，中途换入无需重启）。`hf config export theme` 导出当前生效的完整色板作为可编辑模板。

```toml
[theme]
heading = "#e4613c"      # Markdown 标题（朱漆）
accent = "#5588ee"       # 提示符 / 活动 spinner / 补全高亮（群青）
muted = "#e8b44a"        # 次要文本（增量、空闲提示，金箔）
success = "#8fbf6a"      # 完成（苔）
error = "#e14b63"        # 失败（绯）
# 其余可选：emphasis / strong / inline_code / link / quote
```

## 键位（keymap.toml）

全屏 shell 的按键绑定以语义动作（而非物理键）为中心，可在磁盘上重映射。文件位于 `~/.heartflow/keymap.toml`（用户）与 `.heartflow/keymap.toml`（项目，逐动作胜出）。每个动作映射到一个键串（`"ctrl+c"`）或键串数组；空数组 `[]` 解绑该动作（按键回落到输入编辑器）。修饰键 `ctrl`/`alt`/`super` 参与匹配，`shift` 被忽略（大写字母是否带 shift 因终端而异，故字符大小写不敏感）；未知动作或非法键串跳过并告警，绝不阻断启动。改后于下一回合边界热重载生效。`hf config export keymap` 导出当前生效绑定作为模板。

```toml
[keymap]
submit = "enter"                # 提交输入（回合运行中则排队）
interrupt = "ctrl+c"            # 取消运行中的回合（双击）/ 空闲时退出
escape = "esc"                  # 关闭浮层 / 空闲时退出
toggle_fold = "tab"             # 折叠/展开最近一条过程项（思考或工具输出）
scroll_up = "pageup"            # 向上滚动转录
scroll_down = "pagedown"        # 向下滚动转录
guide = "ctrl+g"                # 打开引导浮层，预览零 token 的下一步任务草稿并发送（回合运行中则排队）
next_section = "ctrl+pagedown"  # 切换到下一个会话分区（标签页）；回合运行中不可切换
prev_section = "ctrl+pageup"    # 切换到上一个会话分区（标签页）
new_section = "ctrl+t"          # 新建一个独立会话分区（自带运行时、会话文件与队列）
help = "f1"                     # 打开/关闭按键参考浮层（列出每个动作的当前绑定键与说明，只读，回合运行中亦可查阅）
```

## Shell 行为（settings.toml）

若干此前为编译期常量的 shell 行为旋钮，可免重编调校。文件位于 `~/.heartflow/settings.toml`（用户）与 `.heartflow/settings.toml`（项目，逐项胜出）。每个字段可选，缺省复现内置行为；类型错或越界的值跳过并告警，绝不阻断启动。改后于下一回合边界热重载生效（`frame_budget_ms` 例外，仅启动时读取）。`hf config export settings` 导出当前生效值作为模板。

```toml
[settings]
scroll_step = 3          # 每次 PageUp/PageDown 滚动的行数
tool_inline_lines = 3    # 工具输出行数 ≤ 此值时默认展开，否则折叠为单行标记（0 = 全部折叠）
fold_thinking = true     # 流式思考是否默认折叠（过程而非产出，故默认折叠）
frame_budget_ms = 80     # 重绘节流窗口（毫秒）；启动时读取，会话中改动下次运行生效
```
