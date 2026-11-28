# 运行与配置

[项目首页](../README.md) · [文档中心](README.md) › [从这里开始](README.md#从这里开始) · [应用协议](protocol.md)

下文的命令从仓库根目录运行。先运行离线示例，再按需配置模型或外部工具。应用服务使用本机 stdio，宿主通过启动参数和用户配置文件提供设置。

| 要做什么 | 从这里开始 |
| --- | --- |
| **从零启动** | [准备环境](#快速开始) → [离线演示](#先运行离线演示) → [启动服务并握手](#再启动自己的服务) |
| **配置模型或外部服务** | [真实模型](#真实模型) · [MCP](#mcp) · [ACP 客户端](#acp-客户端) · [A2A 远程任务](#a2a-远程任务) |
| **启用本机能力** | [文件与 Skills](#文件与-skills) · [注册命令](#主机注册的命令) · [隔离项目检查 · 0.0.21](#在隔离环境里运行项目检查0021) · [跨轮次终端](#跨轮次终端) |
| **验证工作区** | [执行诊断](#执行诊断) · [工程检查和离线示例](#检查与演示) · [版本升级验证](#版本升级验证) |

## 快速开始

| 用途 | 准备 |
| --- | --- |
| **构建内核** | Rust 1.89+ 和平台的 C/C++ 构建工具。SQLite 和 TLS 依赖需要原生构建。 |
| **运行离线演示** | Python。离线内置模型不需要 API Key。 |
| **启用本机隔离** | Windows 原生启动需要 Windows 10 / Server 2016 或更新版本；Linux 隔离需要 Landlock ABI 6。见 [平台与隔离边界](sandbox.md)。 |

### 先运行离线演示

> [!TIP]
> 离线演示无需 API Key。它会构建应用服务，在临时数据库提交任务、批准一次交付物写入，然后回放事件。

```sh
python examples/demo.py
```

[预期输出](../README.md#快速体验) · [换用真实模型](#真实模型) · [运行工程检查](#检查与演示)

### 再启动自己的服务

让宿主把应用服务作为子进程启动。以下命令将任务数据库存放在仓库的 `.fliuno` 目录：

```sh
cargo run -p fliuno-app-server -- --db .fliuno/kernel.db
```

通过 **stdin** 发送下面的握手请求，以换行结尾。通过 **stdout** 逐行读取 JSON-RPC 响应和通知，**stderr** 留给诊断日志。

```json
{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"0.1","clientName":"my-client"}}
```

| 客户端步骤 | 对应调用 |
| --- | --- |
| **创建任务** | `workspace/open` → `session/create` → `task/start`。用 `task/get` 查询状态。 |
| **展示审批** | 任务进入 `waiting_approval` 后展示 `pendingApproval.call`。用户允许一次或拒绝时调用 `approval/respond`。 |
| **收取交付物** | 用 `artifacts/list` 查看产物，用 `artifacts/read` 分段读取正文。 |
| **订阅和重连** | 用 `events/setScope` 订阅任务、会话或工作区。重连或收到 `notifications/eventGap` 时，按范围和持久游标补读。 |

[工作台接入顺序](README.md#工作台要展示什么) · [应用协议参考](protocol.md#方法) · [Rust 客户端](protocol.md#rust-调用方) · [AG-UI HTTP](ag-ui.md#http-接入)

### 导出协议 Schema

```powershell
cargo run -p fliuno-app-server -- --schema
```

上面的命令从 Rust 真实类型导出协议 JSON Schema，避免维护另一份手写类型。

同一会话的新任务会保存已有历史的有界快照，供重试和重启继续使用。要预览、缩小或关闭历史范围，见 [会话上下文](context.md)。

## 执行诊断

应用服务默认关闭追踪输出。用 `--trace-log PATH` 把诊断追加到 JSONL 文件。监督进程将自己的追踪写入同目录的 `*.worker-*.jsonl` 文件，并沿用父工具调用的 trace ID。

```powershell
cargo run -p fliuno-app-server -- --db .fliuno/kernel.db --trace-log .fliuno/trace.jsonl
```

用 `--otlp-endpoint URL` 显式开启 OTLP/HTTP JSON 导出，传入接收方的完整端点，例如本机的 `http://127.0.0.1:4318/v1/traces`。远端接收方须使用 HTTPS。只设置 `OTEL_EXPORTER_OTLP_ENDPOINT` 不会自动启用导出。诊断记录固定的操作、关联标识、状态和数值。任务与审批事件仍通过业务协议读取。队列满或导出失败时可能丢弃诊断记录，适合定位问题和测量开销。

## ACP 离线演示

Windows 可直接运行完整 ACP 离线闭环；脚本构建确定性测试 Agent，在真实 AppContainer 中运行，并展示持久审批和完成事件：

```powershell
python examples/agent_demo.py
python examples/agent_demo.py --deny
python examples/agent_demo.py --workspace
python examples/agent_demo.py --workspace --deny
```

`--workspace` 展示写入 diff，分别审批宿主写入与按行读取；Agent 直接读取其虚拟工作区路径仍取不到宿主文件。演示只修改临时目录，不调用模型。外部 Agent 的配置、任务选择和当前隔离限制见 [ACP 文档](acp.md)。

## 真实模型

[工作台配置](workbench.md#接入模型)可直接交给 stdio 应用服务。由宿主启动时传入配置文件和数据库路径。0.0.21 开发版可以用 [`/settings`](workbench.md#命名供应商0021-开发版) 维护 `[providers]` 和档案绑定。供应商决定连到哪个 Base URL、用哪套凭据；档案的 `protocol` 决定请求格式。下面的内联配置仍适用。

```sh
fliuno-app-server --config ~/.fliuno/config.toml --db .fliuno/kernel.db
```

模型端点可通过 `[model].base_url` 或 `[models.<id>].base_url` 指定。地址填写 API 根路径。例如 `https://gateway.example/v1/`。`provider` 决定请求协议，`openai` 使用 Responses，`anthropic` 使用 Messages，`gemini` 使用 Interactions。配置自定义地址时，网关需要支持所选协议。 给只提供 `/chat/completions` 的服务时，在 `provider = "openai"` 的配置旁设置 `protocol = "chat_completions"`。省略 `protocol` 时会使用上述原生协议。Chat Completions 使用相同的 `/models` 目录，本机服务可在回环地址上用 HTTP。

```toml
[models.local]
provider = "openai"
protocol = "chat_completions"
base_url = "http://127.0.0.1:8080/v1/"
name = "local-model-id"
api_key_file = "credentials/local.key"
```

本机兼容服务不要求 Key 时，可将该档案的 `api_key_file` 替换为 `auth = "none"`。此选项只接受显式的 Chat Completions 回环地址（`localhost`、`127.0.0.1` 或 `[::1]`），目录和生成会直接连接，不发送认证头。`models/check` 会把凭据状态标记为 `not_required`。未设置 `auth` 的旧档案仍需要凭据。

Chat Completions 支持文本、图像和每步一次函数调用，具体能力取决于服务器和模型。预算可配置数值窗口与 `max_output_tokens`，`provider_count` 须省略或为 `false`。使用同一模型 ID 的 Responses 和 Chat Completions 仍有不同的内核身份，以保留持久 Run 和续接的协议范围。

在填写 `name` 前可以用已配置的 Provider、Base URL 和凭据拉取目录。命令输出 JSON，支持将返回的 `nextCursor` 传给下一次调用。运行中的可信本地 stdio 客户端也可以调用 `models/list`，可选 `profile` 和 `cursor`。 设置 `baseProfile: true` 可显式查询 `[model]`。编辑器可携带 `revision`，防止意外查询已被外部修改的配置。

```sh
fliuno-app-server --list-models --config ~/.fliuno/config.toml --model-profile lab
```

可信本地客户端可调用 `modelProfiles/getEdit` 取得指定档案的保存值和修订号；`profile: null` 指 `[model]`。`modelProfiles/saveEdit` 使用相同的档案和修订号改动 `model` 与 `reasoningEffort`（`null` 恢复提供方默认值）。修订过期时重新读档案再编辑。保存不会替换运行中的模型。确认使用已更新的活动档案时，调用 `modelProfiles/select` 并传 `reload: true`；待审批、待恢复或仍在进行的模型工作会阻止切换。

`--check-model` 返回本地配置、凭据读取、目录访问和所选模型是否列出的独立状态。它最多检查 4 页，网络查询总时限为 5 秒。`incomplete` 表示搜索未走完，`not_listed` 表示已走完当前可见目录。目录访问成功只能确认本次目录请求通过。只提供生成端点的网关仍可手填模型 ID。命令不会发送生成请求或打开数据库。可信本地 stdio 客户端可调用 `models/check`，可选 `profile`。

```sh
fliuno-app-server --check-model --config ~/.fliuno/config.toml --model-profile lab
```

目录中的 `maxInputTokens`、`maxOutputTokens` 和 `supportsThinking` 来自服务方返回的模型元数据。Anthropic 还会提供明确支持的 `reasoningEfforts`。字段缺省表示目录未提供这项信息。模型目录所报的上限供客户端和用户参考，实际请求仍使用配置中的上下文预算。

`reasoning_effort` 可写在 `[model]` 或某个 `[models.<id>]` 下。省略时使用提供方默认值。具体模型支持的档位由服务端决定。

`--check-model` 和 `models/check` 可返回 `reasoning`，分别报告已配置的档位、目录声明的档位和模型是否声明支持思考。`advertised` 表示目录明确列出该档，`not_advertised` 表示目录提供了档位信息但未列出该档，`unknown` 表示拿不到档位证据。没有配置档位时使用提供方默认值（`provider_default`）。目录证据不等于生成验收；不会据此自动修改配置。

| Provider | 配置接受的档位 |
| --- | --- |
| `openai` | `none`、`minimal`、`low`、`medium`、`high`、`xhigh`、`max` |
| `anthropic` | `low`、`medium`、`high`、`xhigh`、`max` |
| `gemini` | `minimal`、`low`、`medium`、`high`、`xhigh` |

```toml
[models.lab]
provider = "openai"
base_url = "https://gateway.example/v1/"
name = "model-id"
api_key_file = "credentials/lab.key"
reasoning_effort = "medium"
```

可信本地 stdio 客户端可使用 `modelProfiles/getConnection` 读取档案连接与配置修订号，`modelProfiles/previewConnection` 查询未保存连接的模型目录，`modelProfiles/saveConnection` 新增或更新档案。客户端应通过 `initialize.capabilities.modelProfileConnections` 检查支持。三种目标分别是 `{"type":"base"}`、`{"type":"existing","profile":"lab"}` 和 `{"type":"new","profile":"lab"}`。预览和保存均须提供当前修订号。

凭据选项为 `keep`（保持当前凭据）、`managed`（传入待保存的 Key）、`file`（使用现有文件）和 `none`（本机无 Key）。改变协议或 API 根地址时不能使用 `keep`。预览不会保存配置或凭据，保存也不会切换当前模型或修改启动默认值。响应只返回凭据状态，不返回凭据引用或内容；此接口在附加连接上不可用。

需要限制模型输入时，可设置以下配置。

```toml
[model.budget]
window = 131072
max_output_tokens = 4096
provider_count = true
```

`window` 是总窗口，`max_output_tokens` 为输出预留。使用 Anthropic 时，`window = "provider"` 会向服务查询窗口。`provider_count` 启用 Provider 计数。Gemini 使用计数投影，完整归因见[上下文文档](context.md)。Embeddings JSON 可设置 `apiKeyFile`，路径相对于该 JSON 文件。

旧版无界面脚本可通过已有的 Provider 环境变量继续启动，显式 `--config` 时以文件设置为准。

## MCP

```powershell
cargo run -p fliuno-app-server -- --mcp-config <配置路径>
```

配置结构见 [协议文档](protocol.md) 和 [MCP 传输边界](mcp.md)。stdio 进程的 `isolation` 必填：`trusted_host` 需要绝对路径的命令和工作目录，按宿主权限运行；Windows 使用 `windows_app_container`，Linux 使用 `linux_landlock`，均在私有目录启动且不授予 workspace/network。Linux 要求 Landlock ABI 6，并只开放系统运行目录读取/执行；隔离模式不能指定宿主 `cwd`。Streamable HTTP 服务器不启动本地进程，也不使用 `isolation`；仅允许 HTTPS 或字面 IP 回环 HTTP。隔离进程可通过宿主固定 SHA-256 的 `assets` 清单加载脚本、DLL 和数据，Windows 和 Debian Linux 来宾均已验收真实 Node/CPython 多文件 MCP；MCP 工作区代理仍未实现；不支持的平台会拒绝隔离启动。详见 [沙箱边界](sandbox.md)。

stdio 默认不继承 API key 等环境变量，额外变量通过 `envAllow` 明确指定。HTTP 凭据只从宿主环境变量读取，绝不写入任务数据。Streamable HTTP 接受 `application/json` 与 `text/event-stream`，支持现代无会话发现及受约束的旧版初始化协商；MCP 工具仍逐次审批。`timeoutMs` 默认为 30000，包含同一主机的排队等待；排队取消不发送请求，也不关闭主机。请求发出后的超时或取消会关闭 MCP 主机并保留“外部结果未知”状态，避免在不确定时自动重放。工具调用返回 MRTR `input_required` 时，内核支持经用户确认后的表单 elicitation 续接；采样、roots 和 URL elicitation 不受支持。声明 `io.modelcontextprotocol/tasks` 扩展的现代服务可返回远端任务句柄，并提供受审批的 `tasks_get`、`tasks_update`、`tasks_cancel` 操作；Fliuno 不自动轮询或宣称取消已完成。OAuth 尚未实现。普通构建不包含测试服务器；`--all-features` 会启用 MCP/ACP fixture，用于真实 stdio 进程、HTTP 协议与隔离契约测试。

声明 `resources` / `prompts` 的服务还提供 `mcp_<id>_resources_list`、`resources_templates`、`resources_read`、`prompts_list` 和 `prompts_get` 对应工具，每次调用独立审批。列表一次只取一页；资源 URI 只交给已注册服务处理；返回的提示角色保留在工具结果内。只有资源或提示的 MCP 服务也可接入。预算、恢复和原始数据边界见 [MCP 内容通道](mcp-content.md)。

可在本机启动真实 app-server 与临时 HTTP MCP fixture 验证发现、审批、凭据隔离及调用：

```powershell
python examples/mcp_http_demo.py
python examples/mcp_http_demo.py --sse
python examples/mcp_http_demo.py --legacy --sse
python examples/mcp_http_demo.py --deny
```

旧版 Tasks 的 SDK 互操作验收需在独立 Python 环境安装 `mcp==1.25.0`。运行 `python examples/mcp_legacy_tasks_sdk.py`，可依次测试 stdio、Streamable HTTP JSON 和 Streamable HTTP SSE。加 `--form` 验证 stdio 和 HTTP SSE 的补答，或用 `--restart-form` 验证重启后的旧补答不会重发。应用进程与 SDK 服务只使用临时目录及回环地址，无需真实模型凭据。见 [验收记录](execution-evidence.md#004-mcp-tasks-验收2026-11-21)。

## ACP 客户端

`fliuno-acp` 使用官方 Rust SDK 实现 ACP v1 会话、文本提示、流式通知、单次权限和取消，可通过 Windows AppContainer 或 Linux Landlock/seccomp 启动原生 Agent。`fliuno-agents` 将它接入持久任务与 app-server 的 `--acp-config`；用 `task/start.backend` 选择注册的 Agent。默认关闭文件和终端能力；主机可用 `workspaceAccess: true` 开启逐操作审批的文件代理，仍不向 Agent 授予宿主路径或终端。代理会记录执行开始和结果，重启不重放旧授权。接口与验收范围见 [ACP 文档](acp.md)。

## A2A 远程任务

主机还可通过 `--a2a-config` 注册远程 Agent，沿用 `task/start.backend`、`agents/list` 和任务事件。A2A 通道固定 1.0 JSON-RPC，提供一次提交、轮询、取消证据和最终产物；远程文件链接作为数据保存。配置、状态映射、恢复与兼容范围见 [A2A 文档](a2a.md)。本机离线演示：`python examples/a2a_demo.py`，或加 `--cancel` 验证取消。

## 文件与 Skills

内置文件工具包括目录列举、按行读取、显式 base64 原始字节读取、新建、单文件补丁和 `workspace_replace_text` 全文替换，默认每次调用均需审批。文件 IO 使用 `cap-std` 目录能力；单文件读写限制为 256 KiB，字节读取不解码或执行内容。创建使用 `create_new`；修改要求基线 SHA-256 一致。全文写入提案可读取旧文件生成供客户端审阅的 diff，批准前不修改目标，也不向外部 Agent 返回旧内容。审批后的外部并发修改不具有文件系统事务保证；写入或同步失败保留未知结果。命令执行只调用 `--exec-config` 中由主机预注册的程序，每次执行单独审批；不开放模型构造 shell 命令或临时 executable 的通用 shell 工具。

## 主机注册的命令

配置命令执行器时传入主机控制的 JSON 配置。模型只能提供参数和有限 stdin；程序路径、固定参数、环境变量、隔离方式、timeout 与输出预算由主机确定。见 [命令执行协议与边界](protocol.md)。

执行报告保存启动耗时、首次停止原因、输入是否跳过/中止、管道关闭和有界 IO 诊断；超时包含原生启动，未知结果不会自动重放。字段语义与真实进程测试见 [执行证据](execution-evidence.md)。

```powershell
cargo run -p fliuno-app-server -- --exec-config C:/absolute/path/to/execution.json
```

## 在隔离环境里运行项目检查（0.0.21）

此功能属于开发中的 0.0.21。先在 `~/.fliuno/config.toml` 引用你控制的执行配置，路径相对于此 TOML，而非项目目录：

```toml
[integrations]
execution = "execution.json"
```

下面是 `execution.json` 的检查配置示例；将 `command` 换成你安装的原生检查程序的绝对路径，`args` 换成该程序接受的固定参数。Windows 使用 `windows_app_container`；Linux 使用 `linux_landlock`。

```json
{
  "programs": [{
    "id": "check-format",
    "command": "C:/Tools/formatter.exe",
    "isolation": "windows_app_container",
    "args": ["--check", "src/main.rs"],
    "projectCheck": {
      "label": "检查代码格式",
      "description": "检查 src/main.rs 是否符合格式要求",
      "inputs": ["src/main.rs"]
    }
  }]
}
```

项目检查只接收配置中的输入文件及其 SHA-256，不允许模型改命令、参数或环境。批准后最多复制 16 个文件、合计 8 MiB；文件内容变化则须重读并重新审批。检查在私有目录里运行，写入不会回到工作区。程序需要的其他资源可显式配置为固定摘要的 `assets`；不会自动加载整个仓库、工具链缓存或外部依赖。对话会显示命令和退出码；被取消或结果未知的检查不会显示为通过。用户侧配置是执行授权来源，仓库文件不能自行登记检查。

## 跨轮次终端

`--terminal-config` 注册由主机固定的原生程序 preset。示例使用独立的 Windows PowerShell 进程；将绝对路径替换为目标平台真实存在且可信的原生可执行文件。`trusted_host` 以当前用户权限运行，支持 cwd 下已有的用户工作内容，但不是文件/网络沙箱。普通 `process_execution` 仍是有 timeout 的前台命令，两种工具的权限范围不同。

```json
{
  "stateDirectory": "C:/absolute/path/to/fliuno/terminals",
  "presets": [{
    "id": "shell",
    "command": "C:/Windows/System32/WindowsPowerShell/v1.0/powershell.exe",
    "isolation": "trusted_host",
    "args": ["-NoLogo", "-NoProfile", "-NoExit"],
    "envAllow": ["PATH"],
    "lifetimeMs": 900000,
    "spoolBytes": 1048576
  }]
}
```

```powershell
cargo run -p fliuno-app-server -- --db .fliuno/kernel.db --terminal-config C:/absolute/path/to/terminal.json
```

工具通过普通持久任务逐次审批，终端输入、调整尺寸、停止也各自审批；只读的 `terminal/list`、`terminal/get`、`terminal/output` RPC 不启动进程。PTY supervisor 是独立服务进程，随 app-server 退出仍可运行。输出是带游标和 gap 标志的 base64 原始字节；默认最多保留 1 MiB，保留 15 分钟。启动配置路径、容量、恢复语义和操作系统差异见 [终端协议](protocol.md#跨轮次终端) 与 [沙箱边界](sandbox.md#持久终端)。

Linux preset 可配置 `resources` 与 `linuxCgroupParent` 为 worker 及 PTY 子孙设置 cgroup v2 预算；Windows 的 `resources` 可覆盖 worker Job 的默认进程数和内存上限。Linux 启用 cgroup 时由独立 guardian 跟随 worker 生命周期，app-server 退出不终止终端，worker 退出或崩溃后回收剩余后代。Linux guardian 行为探针已接入 Ubuntu CI，当前仍待 runner 验证。

```powershell
cargo run -p fliuno-app-server -- --skills-dir C:/absolute/path/to/skills
```

目录结构为 `<skills-dir>/<skill-name>/SKILL.md`。启用后先用 `skills_list` / `skills_read` 发现元数据和指令，再通过 `skills_list_resources` / `skills_read_resource` 按需读取参考资料、文本模板和脚本源码；显式传 `encoding:"base64"` 可读取最多 256 KiB 的二进制资源。每个工具均逐次审批，资源不能通过目录链接跳到其他技能；读取不会执行脚本、解码文档或创建产物。技能的 `allowed-tools` 作为元数据保留，不授予执行权限。路径、预算、来源绑定与离线演示见 [Skills 文档](skills.md)。

## 检查与演示

```powershell
./scripts/check.ps1
python examples/demo.py
python examples/demo.py --deny
python examples/context_demo.py
python examples/ag_ui_http_demo.py
python examples/ag_ui_http_demo.py --interrupts --inline-media
```

`check.ps1` 运行格式、Clippy、测试、生成契约一致性检查和离线进程演示；解释器演示需要本机 Node 24，Windows 另需 CPython 3.12。GitHub Actions 在 Windows、Linux 和 macOS runner 上执行工作区构建与回归检查。macOS runner 目前只验证跨平台编译和不支持隔离时的拒绝行为，不代表 macOS 沙箱已实现；本地验证结果也不代表远程 CI 已经运行。

开发和测试构建保留行号堆栈、关闭增量缓存，以控制 Windows 调试产物占用；需要调试局部变量时可通过本地 Cargo 配置覆盖 `profile.dev.debug=2`。

如果工作区所在磁盘空间紧张，可在当前 PowerShell 会话把编译产物放到独立目录；检查脚本也会从该目录读取二进制：

```powershell
$env:CARGO_TARGET_DIR = 'D:/BuildCache/Fliuno'
./scripts/check.ps1
```

停止构建和测试后，可用 `cargo clean --target-dir 'D:/BuildCache/Fliuno'` 清理这个专用缓存。验收日志、数据库样本和运行数据应放在缓存目录之外，因为 `cargo clean` 会清理整个指定目录。

## 版本升级验证

验证从 `v0.0.2` 创建的 SQLite 数据库升级到当前开发版。将标签检出到独立目录构建旧服务，再构建当前服务。以下命令在仓库根目录运行。

```powershell
git worktree add --detach ../Fliuno-v0.0.2 v0.0.2
cargo build --locked --manifest-path ../Fliuno-v0.0.2/Cargo.toml -p fliuno-app-server
cargo build --locked -p fliuno-app-server
python scripts/check_v002_upgrade.py `
  ../Fliuno-v0.0.2/target/debug/fliuno-app-server.exe `
  target/debug/fliuno-app-server.exe
```

Linux 上去掉两个可执行文件路径末尾的 `.exe`。如果配置了自定义 Cargo 构建目录，传入实际的可执行文件路径。

脚本核对旧服务的 Schema，使用离线模型在临时目录创建数据库。升级后会检查归档会话、任务请求键、审批详情、交付物和旧事件，再允许原先待审批的任务继续执行。

[实测结果](execution-evidence.md#从-v002-升级数据库) · [验证脚本](../scripts/check_v002_upgrade.py)

---

[文档中心 ↑](README.md) · 接下来：[应用协议](protocol.md) · [宿主配置](host-configuration.md) · [内核架构](architecture.md)
