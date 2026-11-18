# ACP 客户端适配器

实现与验证基准：2026-11-18，官方 Rust SDK `agent-client-protocol 5.3.0`，稳定 ACP v1。`fliuno-acp` 是独立库，不依赖 `fliuno-core`、SQLite、模型 provider 或 UI。它使 Fliuno 可以作为 ACP 客户端连接另一个 Agent；没有将 Fliuno 伪装成 ACP 服务端。

## 当前接口

| 接口 | 行为 |
| --- | --- |
| `Client::connect` | 连接主机提供的官方 SDK transport；调用方负责该 transport 背后的进程权限和生命周期 |
| `Client::connect_io` | 对 Tokio 读写流应用逐行 JSON-RPC；解析前限制单行 1 MiB |
| `Client::connect_isolated` | 通过 `fliuno-sandbox` 在 Windows AppContainer 或 Linux Landlock/seccomp 中启动单个程序，驱动持有隔离进程和 stdio；不支持的平台拒绝启动 |
| `initialized` | 查看协商结果、Agent 能力和身份；只接受协议版本 1 |
| `new_session` | 创建独立会话；需要绝对且存在的 cwd；隔离模式仅接受规范化后位于私有根内的现存目录 |
| `prompt_text` | 发送单轮文本，返回官方 `PromptResponse`；流式消息、思考、工具、计划等通过 `Events` 的类型化通知返回 |
| `respond_permission` | 按本地 permission ID 和会话 ID 响应允许一次或拒绝；不能更换原始请求 |
| `respond_host_request` | 按本地文件请求 ID 和会话 ID 返回匹配的读/写结果、拒绝或失败；不执行 IO |
| `host_request_remaining` | 复核会话活动轮次和单调时钟到期时间；执行器据此固定截止时间 |
| `cancel` | 请求取消该会话的当前轮次，唤醒权限等待并发送 `session/cancel` |
| `close` | 停止连接，等待自有进程和私有 profile 回收 |

会话通知可以在提示轮次之外到达。`session/new` 响应之前到达的通知先有界暂存，获得并核验 session ID 后按顺序释放。未知会话、空 ID、复用 ID 和错误会话的早到通知会关闭连接。

默认初始化声明 `fs.readTextFile=false`、`fs.writeTextFile=false`、`terminal=false`；未声明的能力请求返回 Method Not Found。独立客户端可显式设 `ClientOptions.filesystem=true`，此时仅将文本文件请求转为 `Event::HostRequest`，不自行打开路径。持久后端的主机配置 `workspaceAccess` 决定是否启用这一能力；虽然后台内核现在有单独的 `fliuno-terminal` 服务，该服务面向 Fliuno 自身经审批的终端工具，并未挂接到 ACP adapter。ACP terminal 始终关闭，不向 Agent 注入 MCP 配置。

本实现锁定正式 v1；官方 v2 仍为草案，且移除了客户端文件/终端 API，建议以 MCP 提供宿主工具。文件能力实现、持久操作状态与审批位于独立模块，未来可复用到其他协议；当前没有声明 v2 兼容。

## 权限与取消

权限请求只接受当前活动轮次、当前连接中的会话；本地生成随机 permission ID。最多同时等待 16 项，容量在 SDK 分发阶段预留，随后才启动等待任务。等待发生在 SDK 并发任务中，因此不会堵住后续通知、其他会话或取消。

允许一次仅匹配选项的 `kind=allow_once`，不依据顺序、显示名称或模型文本。Agent 只提供 `allow_always` 时，允许一次返回输入错误，不会扩大授权；拒绝优先匹配 `reject_once`，没有该选项时返回 `cancelled`。选项 ID 为空或重复、超出上限、没有活动轮次等请求直接得到 `cancelled`。

待定请求有绝对到期时间；超时、取消、轮次结束、连接关闭都会使其失效。跨会话、重复和迟到响应返回 Not Found。取消时所有尚未返回的权限结果使用 `cancelled`；不持久记忆允许或拒绝。取消后的最终 `PromptResponse` 仍由 Agent 返回，调用方应检查 `stopReason`；已经发生的外部效果不会由协议取消回滚。

Agent 在宽限期内不回应、提示超时、请求发送后丢弃 future、会话创建结果丢失、传输中断等情况会关闭连接，不自动重发。错误使用 `OutcomeUnknown` 表示需要核对外部结果。结构化远端错误只保留数字 code，不暴露 message/data。连接关闭不等于撤销 Agent 已完成的操作。

## 资源与生命周期

- 每连接最多 64 个会话、8 个并行提示，每会话一个活动轮次；会话创建同时最多一个，容量不足直接返回 Busy。
- 当前输入文本最大 256 KiB；可选历史文本块单独最大 257 KiB。两个块编码后的 PromptRequest 最多 1 MiB 减 1024 字节，控制字符转义也计入；超额在占用活动轮次和发送前拒绝。单条序列化会话更新/权限请求最大 256 KiB，早到更新最多 16 条。
- 文件请求最多同时 8 个，容量在 SDK handler 中预留后才生成等待任务；编码请求最大 1 MiB，文件内容最大 256 KiB，路径最大 4096 UTF-8 字节。整个请求共用权限超时预算，单调时钟负责最终有效性检查。
- `Events` 默认容量 128，可设为 1..1024；积压或丢弃消费者关闭连接。该队列是实时交付，不提供持久回放。
- 请求、提示、权限等待、取消宽限都有独立上限，可配置为大于零且不超过一小时；默认分别为 30/300/120/2 秒。
- 最后一个 `Client` 被释放时驱动停止；`Events` 持有者不会延长 Agent 进程生命。`close().await` 可等待确定的资源回收。
- 隔离进程结束前先终止 Job，再释放管道和私有目录；stderr 持续丢弃。清理在 blocking worker 执行，不阻塞协议运行线程。

这些限制覆盖适配层会话、权限、事件和线长；官方 SDK 内部仍有自身的缓冲和调度机制，不能由此宣称整个进程对任意恶意流量都有固定总内存界限。AppContainer 的权限及局限见 [沙箱边界](sandbox.md)。通用 transport API 不提供 OS 隔离，协议审批也不能阻止一个宿主权限 Agent 绕过协议直接访问系统。

## 运行与验收

```powershell
cargo test -p fliuno-acp --all-features --locked
cargo build -p fliuno-acp --features test-fixtures --bin fliuno-acp-fixture --locked
cargo run -p fliuno-acp --example isolated -- E:/Projects/Fliuno/target/debug/fliuno-acp-fixture.exe "你好，Fliuno"
```

最后一条演示仅适用于 Windows；按实际检出路径替换 exe 绝对路径。测试 Agent 是确定性 fixture，不访问模型或产生付费请求；普通构建不包含该 binary。示例没有审批 UI，会明确拒绝权限请求。

契约测试使用官方 SDK Agent 作为对端，验证中文更新、早到通知、默认能力拒绝、权限绑定/过期、单次与持久选项区分、其他会话并发、取消、忽略取消后的关闭、丢弃 future、事件积压、超大无换行输入、版本拒绝、初始化超时和远端错误脱敏。Windows 与 Linux 另启动真实 stdio Agent：宿主文件/TCP 对照先成功，再检查各自的 AppContainer 或 Landlock/seccomp 请求被拒绝、私有写入成功、超时与最后 owner Drop 清理私有目录。

## 持久任务与审批

`fliuno-agents` 已将独立客户端接入 core 的 `AgentBackend`，app-server 用 `--acp-config` 注册后端。外部 Agent 与模型 provider 共用 4 个固定 worker 和容量 32 的队列；等待权限时占用一个执行名额，查询、审批和取消继续由 RPC 主循环处理。core 不依赖 ACP SDK，其他 Agent 协议可以实现相同的执行边界。

每个任务启动一个新的隔离进程和 ACP 会话；运行 ID 在创建进程、发送提示之前事务提交。不会跨任务复用进程，也不把 Fliuno 工作区路径授予 Agent。`native_scratch` 在 Windows 使用零 capabilities 的 AppContainer，在 Linux 使用 Landlock ABI 6 与 seccomp；两者都使用单个程序和私有 cwd，可选择通过逐操作审批的宿主文件代理访问工作区。主机可通过 `assets` 暂存固定摘要的脚本、DLL 和数据；官方 SDK 对端已验证读取副本。网络代理及真实商业 Agent 互操作仍未完成，不能从伴随文件支持推断其已可运行。清单与解释器验收范围见[伴随文件](runtime-bundles.md)。

配置示例（`command` 换成实际绝对路径）：

```json
{"agents":[{"id":"native","command":"C:/absolute/native-agent.exe","args":[],"isolation":"native_scratch","envAllow":[],"resources":{"memoryMaxBytes":536870912,"pidsMax":16},"workspaceAccess":false,"requestTimeoutMs":30000,"promptTimeoutMs":300000,"permissionTimeoutMs":120000}]}
```

三个超时参数可省略，默认值如上，范围为 100..3600000 毫秒。`workspaceAccess` 可省略，默认 false。命令、参数、环境变量来自主机配置；`agents/list` 只返回 ID、协议、隔离类型和 workspaceAccess。普通 RPC 不能创建新命令或开启主机文件代理。启动任务时选择：

```json
{"jsonrpc":"2.0","id":4,"method":"task/start","params":{"sessionId":"<session-id>","input":"任务内容","clientRequestId":"submission-1","backend":{"type":"external_agent","agentId":"native"}}}
```

`requestTimeoutMs` 分别约束隔离进程启动、协议 initialize 和 session/new 等请求，并非所有阶段共享一个总计时器。Windows profile 创建与可执行文件暂存计入进程启动阶段。启动、初始化或会话设置失败会向 stderr 输出阶段及已脱敏的本地错误；远端错误仍只保留数字 code，不输出 Agent 的 message/data、提示或凭据。进程端到端测试使用生产默认启动预算；协议超时与取消由受控对端单独验证。

省略 `backend` 仍使用模型流程。幂等键同时绑定输入、后端与上下文选项；更换其中任何一个都会冲突。外部运行保存在 `Task.agentRun`，初始化能力以 `externalAgents` 表示是否有注册后端。

宿主可以用 `Client::supports_session_listing()` 探测已连接 Agent 的 `sessionCapabilities.list`，然后用 `Client::list_sessions(cwd, cursor)` 读取一页。隔离进程的筛选路径限制在其私有工作目录，省略筛选时使用该目录。每页最多 128 条，同一连接同时处理一条列表请求。调用方显式使用 `next_cursor` 翻页。列出的 ID 只用于展示或选择，仍需独立打开会话才能发送 prompt。列表目前通过 Rust 适配器调用。

声明 `loadSession` 的 Agent 支持宿主在当前 ACP 连接中显式调用 `Client::load_session(id, cwd)`。调用方同时消费 `Event::SessionHistory`，并在加载成功后接收这些历史。历史最多 1024 条、合计 16 MiB，沿用每条 256 KiB 和有界事件队列。加载结束前，目标 ID 不能接收新 prompt，也不能触发宿主文件操作或权限请求。加载成功后，宿主可以为该 ID 发起新一轮 prompt。隔离进程继续使用私有工作目录。

新任务可通过 Fliuno 的冻结 Task.context 延续同会话已完成工作，模型与 ACP 共用中性历史索引。有历史时 `prompt_text_with_context` 在当前输入前单独发送一个 TextContent JSON 数据块；没有历史或选择 isolated 时只发当前输入。历史不包含旧审批、供应商签名或递归 context，不重用 ACP 进程/会话；新的宿主文件操作仍逐次审批。这条 app-server 路径仍为每项任务创建 ACP 会话，具体选择和字节预算见 [上下文设计](context.md)。

流式通知先写入 `agent_update` 事件；文本消息另外汇总到 `agentRun.text`。计划、思考、工具更新等保持原始只读 JSON，仅文本消息进入最终输出。权限记录位于 `agentRun.pendingApprovals`，最多同时 16 项，各有独立审批 ID、运行 ID、连接内 permission ID、原始请求、单次允许选项可用性和到期时间。客户端显示请求后使用既有 `approval/respond` 的 `allow_once` 或 `deny`，不能提交替换参数。拒绝交回 Agent，不直接判定任务失败。

审批决定与快照先原子提交，再通过容量 16 的内存通道交付运行后端；通道交付不做 IO。崩溃可能留下“决定已记录、交付未知”的状态；绝不据此重放授权。ACP 仍在交付时按会话、活动轮次和单调时钟复核有效性。到期、轮次结束、取消、连接丢失和重启都会关闭尚待审批的记录；跨任务、重复和旧审批不能授权。

每运行最多 4096 条更新、64 项累计 Agent 权限、64 个累计宿主操作；两种来源共用最多 16 项待审容量。普通更新/Agent 权限请求最多 256 KiB；宿主提案（工具调用与审阅 preview）最多 2 MiB，宿主结果最多 1 MiB。记录的更新、请求、结果及汇总文本合计最多 8 MiB，汇总文本最多 1 MiB。这是应用层预算，不包含 SDK 内部全部缓冲、SQLite 页、快照/事件中的重复数据和固定元数据；超额停止连接，保留未知结果。

最终响应的 `end_turn` / `refusal` 记为 completed；`max_tokens` / `max_turn_requests` 记为 interrupted，但 `outcomeUnknown=false`；`cancelled` 记为 cancelled。超时、断线或观测失败记为 interrupted 和未知结果。用户取消立即持久化 cancelled，迟到结果不能改写终态或生成最终输出。`agentRun.finished` 表示运行记录已终结；正常执行先等待进程清理，再记录结果；授权通道交付失败时会先终结记录并通知执行器停止，因此不能把这个字段当作所有情况下的 OS 进程退出证明。

重启会将运行中和等待外部权限的任务转为 interrupted，使待审记录失效；内置工具的持久等待审批保持原有行为。已启动的外部运行无论结果已知与否，都不能通过 `task/resume` 重放；应核对结果后创建新任务。尚未启动的排队任务可显式恢复，但要求重新注册相同后端。数据库版本升级为 4，使旧二进制拒绝读取新的运行快照；v3 缺少 source/hostTools 的快照及事件仍可读取并按旧权限恢复规则失效。

新增内核测试覆盖事务写入失败时不交付授权、交付失败后的持久审计、恢复/取消/迟到结果、幂等后端绑定和资源预算。Windows 应用进程测试从真实 JSON-RPC 启动真实隔离 ACP Agent，覆盖审批到完成、重开数据库、等待审批时退出服务、16 项并行权限、权限到期、提示超时与取消。

## 受控工作区文件代理

`workspaceAccess:true` 在容器私有目录中创建空的 `workspace` 子目录并用作 ACP cwd。它只充当虚拟路径前缀；不复制宿主文件、不授予宿主目录 ACL。只接受此前缀下的普通路径组件，转为工作区相对路径后再经注册工具、策略与 `cap-std` 目录能力校验；Agent 直接进行 OS 文件读取仍不会得到宿主内容。

每次读写生成独立 `source=host_tool` 审批；`source=agent_permission` 的允许不能代替它。读操作使用 `workspace_read_text`，支持从 1 开始的 line/limit，保留换行。写操作使用 `workspace_replace_text`：提案阶段在宿主读取旧基线并生成客户端 diff，不写入目标、不向 Agent 返回旧内容；执行时核对 SHA-256，新增文件使用 `create_new` 防止覆盖审批后出现的目标。拒绝最终文件的符号链接及多硬链接。与补丁一样，本进程写入串行化，但外部进程并发编辑不具备文件系统事务保证。

`agentRun.hostTools` 保存调用、状态与结果。批准先提交事务，再交付不可替换的执行对象；开始 IO 前提交 `agent_host_tool_started`，结果落盘后才答复 ACP。拒绝、失效、已知失败、成功、未知结果分别保留；运行中的操作在取消/重启时标记未知。Agent 即使返回 end_turn，只要仍有运行中或未知的宿主操作，任务也不会被记成已知完成。

每运行最多 8 个未完成文件请求和阻塞作业；容量耗尽先拒绝，不创建无界等待任务。截止时间覆盖准备、审批和执行；真正进入阻塞 worker 时再次检查。取消会传入工具，但 Tokio 无法强行中断已进入 OS 的阻塞文件 IO；极慢文件系统下，回收没有严格墙钟上限。写入失败可能已产生部分效果，未知结果禁止自动重放。

```powershell
python examples/agent_demo.py --workspace
python examples/agent_demo.py --workspace --deny
```

真实 AppContainer 进程测试覆盖写/读分别审批、中文按行读取、OS 直接读取拿不到代理文件、审批后基线变化、拒绝不写、绝对宿主路径与遍历拒绝、待审写入重启失效。独立 SDK 测试另覆盖会话/种类/一次性绑定、到期、取消及 8 项并发容量。

排队任务的 `backendAuthority` 在创建时绑定完整 ACP 注册，包括程序内容、参数、实际环境、文件代理开关及预算。重启后相同 Agent ID 的配置变化会拒绝启动旧任务；原生程序在暂存复制时还会再次核验内容 pin。旧 v7 任务不补造注册摘要，详见[注册身份](authority.md)。

尚未提供 Agent 认证流程、app-server 层持久 ACP 恢复、模式和模型切换、多模态输入、终端或网络代理、ACP 服务端、draft v2 或与真实商业 Agent 的互操作验收。文件代理只覆盖 UTF-8 文本，不支持二进制、自动创建父目录或附加 session roots。不能将当前契约测试描述为完整 ACP 功能兼容认证。

官方来源：

- https://agentclientprotocol.com/protocol/overview
- https://agentclientprotocol.com/protocol/v1/initialization
- https://agentclientprotocol.com/protocol/v1/prompt-turn
- https://agentclientprotocol.com/protocol/v1/tool-calls
- https://agentclientprotocol.com/protocol/v1/file-system
- https://agentclientprotocol.com/protocol/v1/session-setup
- https://agentclientprotocol.com/protocol/v2/migration
- https://github.com/agentclientprotocol/rust-sdk
- https://docs.rs/agent-client-protocol/5.3.0/agent_client_protocol/v1/
