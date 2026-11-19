# Fliuno

面向工作任务与交付物的 Rust Agent 内核。桌面壳（预计 Tauri）、CLI 和其他客户端通过独立的应用协议接入，内核不依赖 UI 框架。

目前处于 **0.0.2 开发阶段**，`0.0.1` 是已推送的初始开发基线。默认运行可离线验证的 demo provider；真实模型必须显式配置。MCP 与命令执行都由主机显式注册；Windows 提供私有目录 AppContainer，Linux 提供 Landlock/seccomp scratch 隔离，并在 cgroup v2 可委派时使用内核级整树回收。必需隔离能力缺失时拒绝启动。

持久审批和排队 Agent 任务绑定[注册摘要](docs/authority.md)：重启后更换同名 MCP/Agent 的端点、参数、环境、凭据或 Schema，旧授权不能用于新配置。隔离进程的原生程序与伴随文件均在暂存时核验实际复制内容。

## 快速开始

需要 Rust 1.89 或更新版本，以及对应平台的 C/C++ 构建工具（SQLite、TLS 依赖需要）。

Windows 原生启动层要求 Windows 10 / Server 2016 或更新版本，使用创建时 Job 分配与独立句柄中转，避免并发进程启动意外保留另一任务的管道。Linux 隔离要求 Landlock ABI 6；平台实际验收范围见 [沙箱文档](docs/sandbox.md)。

```powershell
cargo test --workspace --all-features --locked
cargo run -p fliuno-app-server -- --db .fliuno/kernel.db
```

进程从 stdin 读取逐行 JSON-RPC 2.0，向 stdout 写入响应；stderr 用于诊断。首先发送：

```json
{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"0.1","clientName":"my-client"}}
```

然后依次调用 `workspace/open`、`session/create`、`task/start`。新任务先返回 `running`，后台步骤通过 `notifications/event` 实时送达，也可用 `task/get` 查询。任务进入 `waiting_approval` 后，客户端展示 `pendingApproval.call`，通过 `approval/respond` 允许一次或拒绝；允许后继续后台模型步骤。产物内容保存在 SQLite，通过 `artifacts/list` 读取。调用 `events/setScope` 可订阅任务、会话或工作区。重连或收到 `notifications/eventGap` 时，按订阅范围和持久游标补齐事件。

```powershell
cargo run -p fliuno-app-server -- --schema
```

上面的命令从 Rust 真实类型导出协议 JSON Schema，避免维护另一份手写类型。

同一会话的新任务默认携带已完成工作的有界历史，包括工具结果、产物身份和 ACP 宿主操作；创建时冻结，重试与重启继续使用原快照。用 `session/context` 预览，用 `task/start.context` 调整数量/字节预算、以 `taskIds` 显式选择已完成任务，或选择 `mode:"isolated"`。显式选择超预算时整体拒绝。历史不继承旧审批或供应商推理续接；新副作用仍独立审批。选择策略与迁移边界见 [会话上下文](docs/context.md)。

## 模块

| crate | 职责 |
| --- | --- |
| `fliuno-protocol` | 版本、请求参数、领域对象、事件与 Schema |
| `fliuno-core` | 执行状态机、审批策略、工具注册、SQLite 事务与恢复 |
| `fliuno-providers` | OpenAI Responses / Anthropic Messages / Gemini Interactions 的 HTTP 与消息转换 |
| `fliuno-mcp` | 官方 Rust SDK 驱动的 MCP stdio / Streamable HTTP、工具与资源/提示适配 |
| `fliuno-acp` | 独立 ACP v1 客户端；会话、流式更新、单次权限、文件请求与取消 |
| `fliuno-a2a` | 独立 A2A 1.0 JSON-RPC 客户端；有界请求、消息/任务/产物校验与取消 |
| `fliuno-agents` | 外部 Agent 后端；ACP 隔离进程/文件代理与 A2A 远程任务/产物投影 |
| `fliuno-exec` | 主机注册的原生命令、逐任务审批、stdin/双流输出预算、超时取消与进程树回收 |
| `fliuno-terminal` | 持久 PTY 监督进程、本机 IPC、逐操作审批、有界字节 spool、输出订阅与保留身份的归档回收 |
| `fliuno-sandbox` | 原生启动与进程所有权；Windows host/AppContainer 共享 Job/reactor、Linux Landlock/seccomp、私有目录与受限句柄继承 |
| `fliuno-tools` | 工作区目录分页、元数据、文本和二进制读取、流式 SHA-256、文件创建和有基线校验的编辑 |
| `fliuno-skills` | Agent Skills 元数据、指令与资源按需读取，单技能目录能力和来源绑定 |
| `fliuno-app-server` | JSON-RPC 分发、stdio 传输、配置组装；不承载业务逻辑 |

Windows 可直接运行完整 ACP 离线闭环；脚本构建确定性测试 Agent，在真实 AppContainer 中运行，并展示持久审批和完成事件：

```powershell
python examples/agent_demo.py
python examples/agent_demo.py --deny
python examples/agent_demo.py --workspace
python examples/agent_demo.py --workspace --deny
```

`--workspace` 展示写入 diff，分别审批宿主写入与按行读取；Agent 直接读取其虚拟工作区路径仍取不到宿主文件。演示只修改临时目录，不调用模型。外部 Agent 的配置、任务选择和当前隔离限制见 [ACP 文档](docs/acp.md)。

## 真实模型

API key 仅从进程环境读取，不写入任务数据库。模型 ID 由调用方明确指定。

```powershell
$env:FLIUNO_PROVIDER = 'openai' # 或 anthropic、gemini；默认 demo
$env:FLIUNO_MODEL = '<你的模型 ID>'
$env:OPENAI_API_KEY = '<你的 key>' # anthropic / gemini 分别使用 ANTHROPIC_API_KEY / GEMINI_API_KEY
cargo run -p fliuno-app-server
```

可同时设置 `FLIUNO_CONTEXT_WINDOW_TOKENS` 和 `FLIUNO_MAX_OUTPUT_TOKENS`，检查输入预算并给供应商设置生成上限。默认按 JSON 字节估算。OpenAI 与 Anthropic 可用 `FLIUNO_CONTEXT_COUNT_MODE=provider` 启用供应商输入计数。Anthropic 还支持用 `FLIUNO_CONTEXT_WINDOW_TOKENS=provider` 在启动时探测模型窗口；Gemini 暂用宿主配置的数字窗口和本地估算。[模型请求预算](docs/context.md#模型请求预算)

可用 `OPENAI_BASE_URL`、`ANTHROPIC_BASE_URL` 或 `GEMINI_BASE_URL` 指定 API 基础地址；Gemini 默认使用 `https://generativelanguage.googleapis.com/v1beta/`。仅允许 HTTPS 和本机回环 HTTP；禁止自动重定向。模型请求使用有超时的 SSE 流式响应，也接受完整 JSON 响应；不会自动重试付费请求。Gemini Interactions 使用 `store:false`，由本地持久化步骤续接函数调用；目前支持文本输出和每轮一个工具调用，已通过回环验收。四个固定 worker 最多并发执行四个独立任务步骤，另有容量 32 的有界队列；并发能力会在 `initialize` 中声明。模型与工具执行期间都可查询和取消任务。取消会通知 provider 或工具并丢弃迟到结果，但供应商可能已收到请求并计费，外部副作用也可能已经发生。

支持流式进度的 provider 通过 `notifications/modelProgress` 发送带任务 ID 和步骤号的文本预览。预览不进入 SQLite，完整响应校验后才形成持久任务结果；取消、错误或工具调用会清除预览。客户端需处理 `reset`、`notifications/modelProgressGap` 与迟到增量，具体语义见 [应用协议](docs/protocol.md)。

## MCP

```powershell
cargo run -p fliuno-app-server -- --mcp-config <配置路径>
```

配置结构见 [协议文档](docs/protocol.md) 和 [MCP 传输边界](docs/mcp.md)。stdio 进程的 `isolation` 必填：`trusted_host` 需要绝对路径的命令和工作目录，按宿主权限运行；Windows 使用 `windows_app_container`，Linux 使用 `linux_landlock`，均在私有目录启动且不授予 workspace/network。Linux 要求 Landlock ABI 6，并只开放系统运行目录读取/执行；隔离模式不能指定宿主 `cwd`。Streamable HTTP 服务器不启动本地进程，也不使用 `isolation`；仅允许 HTTPS 或字面 IP 回环 HTTP。隔离进程可通过宿主固定 SHA-256 的 `assets` 清单加载脚本、DLL 和数据，Windows 和 Debian Linux 来宾均已验收真实 Node/CPython 多文件 MCP；MCP 工作区代理仍未实现；不支持的平台会拒绝隔离启动。详见 [沙箱边界](docs/sandbox.md)。

stdio 默认不继承 API key 等环境变量，额外变量通过 `envAllow` 明确指定。HTTP 凭据只从宿主环境变量读取，绝不写入任务数据。Streamable HTTP 接受 `application/json` 与 `text/event-stream`，支持现代无会话发现及受约束的旧版初始化协商；MCP 工具仍逐次审批。`timeoutMs` 默认为 30000，包含同一主机的排队等待；排队取消不发送请求，也不关闭主机。请求发出后的超时或取消会关闭 MCP 主机并保留“外部结果未知”状态，避免在不确定时自动重放。声明 `io.modelcontextprotocol/tasks` 扩展的现代服务可返回远端任务句柄，并提供受审批的 `tasks_get`、`tasks_update`、`tasks_cancel` 操作；Fliuno 不自动轮询或宣称取消已完成。OAuth 和 MRTR 多轮续交互尚未实现。普通构建不包含测试服务器；`--all-features` 会启用 MCP/ACP fixture，用于真实 stdio 进程、HTTP 协议与隔离契约测试。

声明 `resources` / `prompts` 的服务还提供 `mcp_<id>_resources_list`、`resources_templates`、`resources_read`、`prompts_list` 和 `prompts_get` 对应工具，每次调用独立审批。列表一次只取一页；资源 URI 只交给已注册服务处理；返回的提示角色保留在工具结果内。只有资源或提示的 MCP 服务也可接入。预算、恢复和原始数据边界见 [MCP 内容通道](docs/mcp-content.md)。

可在本机启动真实 app-server 与临时 HTTP MCP fixture 验证发现、审批、凭据隔离及调用：

```powershell
python examples/mcp_http_demo.py
python examples/mcp_http_demo.py --sse
python examples/mcp_http_demo.py --legacy --sse
python examples/mcp_http_demo.py --deny
```

## ACP 客户端

`fliuno-acp` 使用官方 Rust SDK 实现 ACP v1 会话、文本提示、流式通知、单次权限和取消，可通过 Windows AppContainer 或 Linux Landlock/seccomp 启动原生 Agent。`fliuno-agents` 将它接入持久任务与 app-server 的 `--acp-config`；用 `task/start.backend` 选择注册的 Agent。默认关闭文件和终端能力；主机可用 `workspaceAccess: true` 开启逐操作审批的文件代理，仍不向 Agent 授予宿主路径或终端。代理会记录执行开始和结果，重启不重放旧授权。接口与验收范围见 [ACP 文档](docs/acp.md)。

## A2A 远程任务

主机还可通过 `--a2a-config` 注册远程 Agent，沿用 `task/start.backend`、`agents/list` 和任务事件。A2A 通道固定 1.0 JSON-RPC，提供一次提交、轮询、取消证据和最终产物；远程文件链接作为数据保存。配置、状态映射、恢复与兼容范围见 [A2A 文档](docs/a2a.md)。本机离线演示：`python examples/a2a_demo.py`，或加 `--cancel` 验证取消。

## 文件与 Skills

内置文件工具包括目录列举、按行读取、显式 base64 原始字节读取、新建、单文件补丁和 `workspace_replace_text` 全文替换，默认每次调用均需审批。文件 IO 使用 `cap-std` 目录能力；单文件读写限制为 256 KiB，字节读取不解码或执行内容。创建使用 `create_new`；修改要求基线 SHA-256 一致。全文写入提案可读取旧文件生成供客户端审阅的 diff，批准前不修改目标，也不向外部 Agent 返回旧内容。审批后的外部并发修改不具有文件系统事务保证；写入或同步失败保留未知结果。命令执行只调用 `--exec-config` 中由主机预注册的程序，每次执行单独审批；不开放模型构造 shell 命令或临时 executable 的通用 shell 工具。

## 主机注册的命令

配置命令执行器时传入主机控制的 JSON 配置。模型只能提供参数和有限 stdin；程序路径、固定参数、环境变量、隔离方式、timeout 与输出预算由主机确定。见 [命令执行协议与边界](docs/protocol.md)。

执行报告保存启动耗时、首次停止原因、输入是否跳过/中止、管道关闭和有界 IO 诊断；超时包含原生启动，未知结果不会自动重放。字段语义与真实进程测试见 [执行证据](docs/execution-evidence.md)。

```powershell
cargo run -p fliuno-app-server -- --exec-config C:/absolute/path/to/execution.json
```

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

工具通过普通持久任务逐次审批，终端输入、调整尺寸、停止也各自审批；只读的 `terminal/list`、`terminal/get`、`terminal/output` RPC 不启动进程。PTY supervisor 是独立服务进程，随 app-server 退出仍可运行。输出是带游标和 gap 标志的 base64 原始字节；默认最多保留 1 MiB，保留 15 分钟。启动配置路径、容量、恢复语义和操作系统差异见 [终端协议](docs/protocol.md#跨轮次终端) 与 [沙箱边界](docs/sandbox.md#持久终端)。

Linux preset 可配置 `resources` 与 `linuxCgroupParent` 为 worker 及 PTY 子孙设置 cgroup v2 预算；Windows 的 `resources` 可覆盖 worker Job 的默认进程数和内存上限。Linux 启用 cgroup 时由独立 guardian 跟随 worker 生命周期，app-server 退出不终止终端，worker 退出或崩溃后回收剩余后代。Linux guardian 行为探针已接入 Ubuntu CI，当前仍待 runner 验证。

```powershell
cargo run -p fliuno-app-server -- --skills-dir C:/absolute/path/to/skills
```

目录结构为 `<skills-dir>/<skill-name>/SKILL.md`。启用后先用 `skills_list` / `skills_read` 发现元数据和指令，再通过 `skills_list_resources` / `skills_read_resource` 按需读取参考资料、文本模板和脚本源码；显式传 `encoding:"base64"` 可读取最多 256 KiB 的二进制资源。每个工具均逐次审批，资源不能通过目录链接跳到其他技能；读取不会执行脚本、解码文档或创建产物。技能的 `allowed-tools` 作为元数据保留，不授予执行权限。路径、预算、来源绑定与离线演示见 [Skills 文档](docs/skills.md)。

## 检查与演示

```powershell
./scripts/check.ps1
python examples/demo.py
python examples/demo.py --deny
python examples/context_demo.py
```

`check.ps1` 运行格式、Clippy、测试、生成契约一致性检查和离线进程演示；解释器演示需要本机 Node 24，Windows 另需 CPython 3.12。GitHub Actions 配置了 Windows/Linux 两个平台；本地验证结果不代表远程 CI 已经运行。

开发和测试构建保留行号堆栈、关闭增量缓存，以控制 Windows 调试产物占用；需要调试局部变量时可通过本地 Cargo 配置覆盖 `profile.dev.debug=2`。

## 文档

- [架构与设计边界](docs/architecture.md)
- [应用协议](docs/protocol.md)
- [跨任务会话上下文](docs/context.md)
- [MCP 传输边界](docs/mcp.md)
- [解释器与伴随文件清单](docs/runtime-bundles.md)
- [ACP 客户端适配](docs/acp.md)
- [A2A 远程任务与产物](docs/a2a.md)
- [原生沙箱边界](docs/sandbox.md)
- [调研与来源](docs/research.md)
- [开发计划与验收](docs/roadmap.md)
