# 内核调研

[文档中心](README.md) › [内核与开发](README.md#内核与开发) · [内核架构](architecture.md) · [开发路线图](roadmap.md)

[参考系统](#参考系统) · [协议分层](#协议不是同一个层次) · [选型](#选型判断) · [平台隔离](#操作系统隔离)

核对日期：2026-11-18。以下是公开文档提供的事实及 Fliuno 的设计判断，不能视为其他产品未公开的内部架构。

OpenCode 源码于核对日检出 `anomalyco/opencode` 提交 `a9a35b4b4ce35573d71207e246e770cfa26ff0e1`。其 core bus 将持久事件日志与 volatile subscribe 分开，server event feed 为慢订阅者设置有界队列并在溢出时断开；Fliuno 采用同样的可靠性分层，但用 SQLite 全局 sequence 与 `events/listAll` 恢复，不复制其实现。

## 参考系统

| 系统 | 可验证的公开机制 | 对 Fliuno 的影响 |
| --- | --- | --- |
| OpenAI Codex App Server | 线程、轮次、执行项；双向 JSON-RPC、流式事件、轮次中断、审批 | 内核作为独立服务；交互与展示由客户端负责。Fliuno 自有协议借鉴生命周期能力但保持独立：事件通知与持久游标回放分层，取消会中断本地 provider HTTP 等待，但不保证撤销对端已接收请求 |
| OpenCode | 独立 server、API/SDK、权限、MCP 接入 | UI 和执行逻辑分离；模型与工具适配器保持可替换 |
| Claude Cowork | 多步任务、指定文件与工具、进度引导、可审阅成果；产品能力正并入 Claude | 领域对象围绕工作区、任务、工具和产物，而非仅围绕代码仓库和终端 |
| ChatGPT Work | 面向完成工作任务和交付物的产品交互 | 产物应当有独立身份、可读取内容和可追踪来源；产品行为不等于可复用的内核协议 |

来源：

- https://developers.openai.com/codex/app-server/
- https://learn.chatgpt.com/docs/app-server
- https://opencode.ai/docs/server/
- https://opencode.ai/docs/permissions/
- https://opencode.ai/docs/mcp-servers/
- https://claude.com/blog/cowork-research-preview
- https://learn.chatgpt.com/docs/working-with-agents
- https://learn.chatgpt.com/docs/enterprise/chatgpt-work-overview
- https://learn.chatgpt.com/docs/app-server

OpenCode 源码仅用于核对模块边界和慢订阅者处理，不复制第三方产品源码。当前实现依据公开协议、官方 SDK 源码和独立设计。

### 命令与终端生命周期

OpenCode 官方工具文档将 `shell` 定义为在宿主用户 shell 中运行命令：前台默认 timeout 为两分钟，后台命令可持续运行并在完成时通知会话；大输出会缩短模型返回文本，同时由托管文件保留完整输出。命令权限按扫描得到的命令资源评估，也检查外部工作目录；文档明确 shell 仍具有宿主用户的文件、进程和网络权限。对 Fliuno 的启示是把命令授权、执行器隔离和可见进度当成三个独立问题；有进程树回收不代表权限隔离。

OpenCode `packages/process-daemon` 的公开 README 描述一个独立 PTY 服务：以 `portable-pty` 管理进程，用 `libghostty-vt` 维护终端状态并回答终端查询；native terminal IO 与 actor 并行。它通过认证的本机 IPC 发现，并用单一 owner connection 管理 daemon 所有权；owner 断开会终止 daemon，只有显式、限期、原子消费的一次性 handoff ticket 才能移交 owner。Fliuno 复用了可替换 native PTY crate、actor 和本机认证 IPC 这些可验证的工程概念，但生命周期不同：此内核需要 app-server 重启后仍可观察已审批终端，所以 supervisor 独立持有进程资源，并以宿主配置的生命周期结束；当前没有 owner/handoff 会话语义、terminal emulator，也未将输入画面转为权威终端状态。`portable-pty` 的运行时可用性本身不等于完整终端 emulator。

ACP v1 将 terminal 定义为 Client 提供的可选能力，由 Agent 通过 `terminal/create` 启动命令，再通过 `terminal/output`、`terminal/wait_for_exit`、`terminal/kill` 管理，并在不再使用时 `terminal/release`。创建参数包括 command、args、env、绝对 cwd 和 `outputByteLimit`；它返回 terminal ID 后命令可继续在后台运行，输出响应包含截断状态及可用的退出状态。这个协议描述的是可管理的终端对象生命周期，实际授权与 OS 隔离仍由 Client 实现。ACP v2 目前仍标为 draft，移除了 Client 文件与终端方法，并建议 Client 通过 MCP servers 暴露宿主工具。

Fliuno 现在并行提供前台 `process_execution` 与独立 `fliuno-terminal`。后者定义 host preset、审批后确定 terminal ID、独立 worker、持久 spool 和有界输入去重；Windows owner Job 在启动 PTY 前接管 worker，可回收其后代。Linux 在可写 delegated cgroup v2 下加入 worker 并由 app-server reaper 在 worker 退出后清理 PTY 子孙，包括调用 `setsid` 的后代。RPC 中断/重启保留 terminal 记录和 spool，未知情况不重启程序。新增 Linux guardian 按 worker pidfd 延续监控，app-server hard crash 后已启动终端继续运行；其实际内核行为仍待 Ubuntu CI 确认。该实现仍未提供终端屏幕状态机、所有权交接 UI 或 ACP terminal 能力。

来源：

- https://opencode.ai/docs/tools/
- https://opencode.ai/docs/permissions/
- https://github.com/anomalyco/opencode/blob/dev/packages/process-daemon/README.md
- https://agentclientprotocol.com/protocol/v1/terminals
- https://agentclientprotocol.com/protocol/v2/migration
- https://learn.microsoft.com/en-us/windows/console/creating-a-pseudoconsole-session
- https://docs.rs/portable-pty/0.9.0/portable_pty/trait.MasterPty.html

## 协议不是同一个层次

### 模型响应流

OpenAI Responses、Anthropic Messages 与 Gemini Interactions 都提供 SSE 流式响应，但事件形状不同。OpenAI 的 `response.output_text.delta` 可用于实时文本预览，`response.completed` 中的完整响应仍是最终解析与工具调用判定的依据。Anthropic 通过 `content_block_start`、`content_block_delta`、`content_block_stop` 和 `message_stop` 表达块生命周期；工具输入的 `partial_json` 必须在块结束后合并解析，thinking 与 signature block 则需保留为原生续接数据。Gemini 使用 `step.start`、`step.delta`、`step.stop` 和 `interaction.completed`；函数参数增量在步骤结束后解析，思考签名保留给无状态续接，不进入文本预览。

Fliuno 将这些 provider 事件映射为 `notifications/modelProgress` 的易失文本增量；它没有持久游标，不参与审批、任务完成或上下文构造。工具调用、流错误和取消会发出 `reset`，完整 provider 响应通过现有状态机校验后才会形成持久任务结果。SSE decoder 限制总响应为 4 MiB、单事件为 1 MiB；JSON 兼容端点保留整包解析路径。

来源：

- https://platform.openai.com/docs/guides/streaming-responses
- https://docs.anthropic.com/en/api/messages-streaming
- https://ai.google.dev/gemini-api/docs/interactions
- https://ai.google.dev/gemini-api/docs/interactions-breaking-changes-may-2026
- https://ai.google.dev/gemini-api/docs/streaming
- https://ai.google.dev/gemini-api/docs/function-calling

| 协议 / 约定 | 边界 | Fliuno 的处理方式 |
| --- | --- | --- |
| Fliuno Application Protocol | 桌面 / CLI ↔ Fliuno 内核 | 自有、版本化 JSON-RPC；Schema 从 Rust 类型生成 |
| OpenAI Responses / Anthropic Messages / Gemini Interactions | 内核 ↔ 模型供应商 | 统一模型决策，保留供应商要求的推理与工具调用上下文；Gemini 关闭服务端存储并本地重放模型步骤 |
| MCP | Agent ↔ 工具、资源、提示服务 | 官方 `rmcp` 客户端；外部工具先注册、验参、审批再执行 |
| ACP（Agent Client Protocol） | 编辑器 / 客户端 ↔ Agent | 独立适配层；不与自有应用协议混称 |
| A2A | Agent 服务 ↔ Agent 服务 | 独立 1.0 JSON-RPC 通道；身份、Task、Artifact、轮询与取消证据 |
| Agent Skills | 技能目录、SKILL.md 与渐进加载 | 文件格式与发现机制，不是获得系统权限的凭证 |
| AG-UI | Agent ↔ 前端事件 | 可选事件投影；不作为数据库真相源 |
| JSON-RPC / JSON Schema | 信封、错误、类型与校验 | 使用 JSON-RPC 2.0，类型与数据契约独立维护 |

来源：

- https://developers.openai.com/api/docs/guides/function-calling
- https://platform.claude.com/docs/en/agents-and-tools/tool-use/implement-tool-use
- https://modelcontextprotocol.io/specification/
- https://modelcontextprotocol.io/specification/2026-07-28/changelog
- https://github.com/modelcontextprotocol/rust-sdk
- https://docs.rs/rmcp/3.11.0/rmcp/
- https://modelcontextprotocol.io/specification/2026-07-28/basic/transports/streamable-http
- https://github.com/modelcontextprotocol/rust-sdk/blob/main/CHANGELOG.md
- https://agentclientprotocol.com/protocol/overview
- https://a2a-protocol.org/v1.0.1/specification/
- https://agentskills.io/specification
- https://docs.ag-ui.com/introduction
- https://www.jsonrpc.org/specification

## MCP 资源、提示与结果状态（2026-11-18）

直接读取 2026-07-28 官方资源与提示规范，核对 `resources/list`、`resources/templates/list`、`resources/read`、`prompts/list` 与 `prompts/get`；本机 HTTP 读取状态和方法匹配记录在 `target/verification/mcp-content-research.json`。Fliuno 把这些协议操作投影为逐次审批的工具，目录列表一页一批，不自动将资源或提示加载进上下文。URI 只发给已注册服务，返回的提示角色保留为工具结果数据。

锁定的 `rmcp 3.11.0` 源码显示便利方法会维护缓存，部分错误可能返回过期缓存；因此内容工具使用 SDK 原始单次请求，并通过实际 HTTP 测试要求每次批准均产生新请求、第二次错误不能被旧结果隐藏。共享 host 层负责排队、取消和失败关闭，不自行驱动 MRTR。

另通过 wire fixture 复现 SDK untagged 结果的边界：同时带最终 payload 和非完成 `resultType` 时，结果枚举可能先匹配普通类型。Fliuno 因此显式核验所有内容操作、tools/list 和 tools/call 的状态，只接受 `complete` 或旧版缺省；拒绝 `input_required`、`task` 和未知扩展，保留结果未知且不自动续交互。复现日志为 `mcp-result-type-reproduction.log`，修正后的聚焦记录为 `mcp-result-type-contracts.log`。工具摘要语义域更新为 v2，完整验收见[执行证据](execution-evidence.md)。

来源：

- https://modelcontextprotocol.io/specification/2026-07-28/server/resources
- https://modelcontextprotocol.io/specification/2026-07-28/server/prompts
- 锁定依赖 `rmcp 3.11.0` 的 `src/service/client.rs`、`src/model.rs`、`src/model/mrtr.rs` 与 `src/transport/async_rw.rs`。

MCP 规范和 SDK 在持续变化。适配层通过锁定 Cargo.lock 固定依赖，最终可协商的协议版本由官方 SDK 决定，不凭一份旧教程宣称兼容所有 MCP 版本。当前实现覆盖 stdio 与 Streamable HTTP 工具发现和调用；未实现完整资源、提示、OAuth、采样和远程审批交互。

截至核对日，MCP 最新稳定版为 `2026-07-28`：它移除 `initialize` 握手与会话 ID，改为每个请求携带版本和客户端能力，并提供 `server/discover`；同时加入多轮请求、Tasks 扩展和传输无关订阅。Streamable HTTP 接受 JSON 与 SSE；现代协议使用版本及客户端能力元数据，并通过 `Mcp-Method`、`Mcp-Name` 和 `x-mcp-header` 注解头支持中间层路由。官方 Rust `rmcp 3.11.0` 于 2026-11-17 发布，保留 SEP-2243 实现，并为 HTTP JSON 响应提供可配置的读取上限；其最低 Rust 版本为 1.88。

Fliuno 的 Streamable HTTP 适配仅开放 `tools/list` 与一次性 `tools/call`，覆盖 JSON/SSE、新版 discover 和旧版 initialize。`auto` 先协商现代协议；SDK 明确表示无共同版本且远端广告已知旧版时，才通过新连接启动旧生命周期。HTTP 集成测试由官方 SDK 服务端配合本机原始 wire fixture，验证 JSON/SSE × 现代/旧版矩阵、路由头、逐项审批、环境凭据隔离和未知副作用不得重放。每个 JSON 或 SSE event 在传输解码前最多 1 MiB，工具发现还有分页与累计页面预算。多轮交互暂时返回未知结果并关停主机，不在没有数据库状态机时自动推进。

ACP 官方文档当前将 v1 标为 latest、v2 标为 draft。它基于双向 JSON-RPC，会话提示通过 `session/prompt` 传入，进度通过通知返回，并定义权限请求与取消。A2A v1.0.1 区分交互 Message、带生命周期状态的 Task 和任务输出 Artifact；Agent Card 提供能力发现。Fliuno 的持久 Task、Event 和 Artifact 可以映射到这些概念，但审批语义、持久回放与取消保证仍由 Fliuno 自己定义，不能把传输映射当作授权。

2026-11-18 进一步核对官方 ACP Rust SDK 发布源码：当前接入版本为 `agent-client-protocol 5.3.0`，协议类型包为 `agent-client-protocol-schema 1.18.0`，两者声明最低 Rust 1.88，满足 Fliuno 的 1.89 基线。只启用稳定 v1，不启用 draft v2 或不稳定扩展。SDK 的 `block_task` 只适合连接前台 future 或并发任务，在消息 handler 中等待会堵住分发；权限代理因此先在 handler 中预留有界容量，再交给 SDK 并发任务等待 UI。

官方取消语义要求发送 `session/cancel` 后继续处理进度，所有尚未完成的权限请求返回 `cancelled`，最终由 Agent 返回 `PromptResponse`。会话通知也可能在没有活动 prompt 时到达，包括 `session/new` 响应之前。Fliuno 的适配层按这些规则维护连接状态，但没有将协议允许与 OS 权限混淆；Agent 自有工具的文件/TCP 边界由 AppContainer 控制。持久运行、独立宿主审批及文件代理的实现边界见 [ACP 设计](acp.md)。

同日核对 v1 File System 与 Session Setup：Agent 必须检查客户端文件能力；文本读写使用绝对路径和 sessionId，读取可指定从 1 开始的 line/limit，写入不存在文件须创建，成功返回空结果。客户端应自行验证路径和授权，Agent 自身的 permission 不能代替客户端检查。Fliuno 用私有虚拟 cwd 映射到工作区能力工具，逐操作审批；写提案在宿主生成基线 diff，执行前复核，结果持久化后才回复。它未实现附加 session roots。

官方 v2 Migration 仍标为 draft，v1 仍是 latest；v2 移除客户端文件系统及终端 API，建议通过 MCP 提供宿主工具。因此当前显式锁定 v1，并将实际文件能力、操作审计和协议代理分层；后续 MCP 接入可以复用同一宿主工具边界，不能把本轮 v1 验收描述为 v2 兼容。

来源：

- https://github.com/agentclientprotocol/rust-sdk
- https://docs.rs/agent-client-protocol/5.3.0/agent_client_protocol/v1/
- https://agentclientprotocol.com/protocol/v1/file-system
- https://agentclientprotocol.com/protocol/v1/session-setup
- https://agentclientprotocol.com/protocol/v2/migration
- https://agentclientprotocol.com/protocol/v1/initialization
- https://agentclientprotocol.com/protocol/v1/prompt-turn

## 选型判断

本次会话上下文实现采用 Fliuno 自持久化的中性完成记录：供应商的原生思考/工具续接只在当前任务保留，新任务将历史作为明确标注的 JSON 文本输入。这样新任务可切换供应商或 ACP 后端，仍使用独立执行与审批；不会把旧 call ID、旧授权或来源任务已继承的历史再次接入执行。此处是 Fliuno 的设计选择，不依赖托管 conversation ID，也未实现模型摘要压缩。通过本机 HTTP 契约和 Windows AppContainer 对端检验实际输入，细节见 [上下文设计](context.md)。

1. Rust 的所有权与类型系统适合约束长生命周期任务与进程资源；以 workspace 分 crate 保持依赖方向。
2. 先做本机内核和协议，不绑定 Tauri，避免 UI 的事件循环成为业务状态机。
3. 快照用于加载任务，事务事件日志用于审计与回放；当前不是“从事件重建所有状态”的纯事件溯源。
4. 内部产物可以与任务、事件在同一事务提交；外部副作用不能套用这项保证。
5. 不自研模型 HTTP 通用框架或完整 MCP 栈；把供应商差异收在适配器中。

## 操作系统隔离

进程组和 Windows Job Object 用于回收进程树，不限制子进程能访问的文件、网络或凭据。`cap-std` 目录能力只约束内置文件工具，也不会限制 MCP 服务器。宿主模式的 MCP 继续继承宿主用户权限；隔离模式的权限必须由 OS 令牌与子进程启动路径实际约束。

跨平台隔离需要平台后端和一套共同探针，而不是仅在启动器增加一个标志：Windows 评估 AppContainer 能力与文件 ACL；Linux 使用 Landlock 与 seccomp；macOS 仍需评估应用沙箱配置及其对子进程的继承。后端不可用或策略无法完整安装时应拒绝启动，不允许静默退回宿主权限。必须分别验证工作区读写、工作区外文件拒绝、网络拒绝、写后执行边界与进程树回收。

调研了 `heel 0.10.1` 作为现成 Rust 候选。它公开了 Linux Landlock/seccomp、Windows AppContainer、macOS sandbox-exec 的后端及文件/网络策略；其 README 同时声明它面向模型生成代码，不是任意不可信代码的通用沙箱。Cargo 包未声明 `rust-version`，Linux 的受监管进程模式还要求匹配版本的 `heel` 可执行文件。

2026-11-18 在本机运行其发布包的 `isolation_windows` 测试：4 个通过、19 个失败。失败主要发生在给工作目录祖先设置 ACL 的准备阶段，分别使用用户临时目录和 `E:/Projects` 都得到访问拒绝；这不能证明其隔离被突破，但表明该部署路径在当前普通权限环境不可用。Fliuno 没有接入此依赖，也没有通过扩大父目录 ACL 来绕过失败。

还审阅了 `sandboxrs-windows 0.1.1` 与 `rappct 0.13.3`，未引入依赖。前者的路径授权会递归修改目录 ACL，并对部分子项失败作忽略处理，不能直接用作 Fliuno 的最小权限路径边界；后者更接近底层机制，但进程生命周期与 stdio 仍需自行审查。

Windows 后端独立实现私有目录，避免修改宿主文件/目录 ACL。使用标准 AppContainer 零能力令牌、三条受限继承管道和启动前 Job 分配，并由自身探针验证原生对照成功、容器令牌、宿主私有文件/TCP 拒绝、显式环境、中文/转义参数与子孙进程回收。另用官方 rmcp 服务器通过真实 stdio 发现和调用，并验证超时清理。OS 后端的范围是单程序与私有存储；ACP 工作区文本访问经另一个受控宿主代理完成，未开放路径 ACL。解释器资源和 macOS 隔离尚未完成。Windows 系统共享资源仍受既有 ACL 控制。

2026-11-18 增加 Linux Landlock/seccomp 后端。Rust `landlock` 0.4.7 的 API 当前表达至 ABI 9；Fliuno 固定要求 ABI 6（Linux 6.12 起支持），以硬性启用文件/TCP 访问规则和 abstract-socket/signal scope。seccomp 额外拒绝所有 socket 操作、io_uring、命名空间创建/切换与挂载入口，并清空继承 capabilities；这样 UDP 和 ABI 未覆盖能力不会落回默认允许。仅为系统运行目录授予读取/执行，工作目录与 HOME/TMPDIR 指向私有目录。真实探针已接入 Linux CI 矩阵，但当前 Windows 开发机只完成 Linux 目标编译，实际内核行为仍待 Ubuntu CI 确认。

2026-11-18 复核 OpenCode `dev` 架构：core session service 位于 `packages/core/src/session.ts`，独立 server handler 与 route composition 位于 `packages/server/src/handlers/session.ts`、`packages/server/src/routes.ts`。这与 Fliuno 的协议无关 core + app-server adapter 分层方向一致；只借鉴责任边界，不照搬其 TypeScript/Effect package layout。

增加 Linux delegated cgroup v2 的可选 sandbox process-tree owner 与资源预算。启用 domain controllers 受 no-internal-process 约束，因此资源控制通过主机配置的空委派 `linuxCgroupParent` 接入，不假设当前进程 cgroup 可安全重配；每个启动创建私有 cgroup 并设置 `memory.max`、`pids.max`、`cpu.max`，回读失败或控制器缺失时 fail closed。`cgroup.kill` 会对 cgroup 与全部后代发送 SIGKILL，并处理并发 fork 和迁移；它提供 supervisor 主动发起的整树回收，不会在 supervisor 自身崩溃时自动执行。Ubuntu CI 探针分别验证 `setsid` 后代回收和 `pids.max` 强制限制；本机 Windows 只能验证 Windows Job，Linux 行为以 Ubuntu runner 为准。下文新增独立 guardian，使 cgroup 清理不依赖 app-server 仍存活。

2026-11-18：terminal worker 配置现将 `resources` 与 `linuxCgroupParent` 纳入 preset identity 和私有 bootstrap。Linux worker 在 exec 前加入独立 cgroup，app-server reaper 在 worker 退出后清理 PTY 子孙并记录清理失败；Ubuntu CI 新增 detached setsid 后代与 memory/pids/cpu 配置探针。这是独立 guardian 接入前的 reaper 路径；当前 hard-crash 监控方案见下文。

`cap-std` 与 `cap-fs-ext` 的依赖下限均为 `4.0.3`。该版本修复符号链接目录路径带尾斜杠时的越界问题；内置文件能力测试包含对应回归路径。通过 `cap-fs-ext` 读取打开句柄的硬链接计数，在 Windows 和 Unix 上拒绝多硬链接修改，并用同一扩展在最终打开时不跟随符号链接。修改工具的基线哈希可发现审批前的改动，但不是针对任意外部写入者的文件系统 CAS；Fliuno 只串行化本进程内的新建和补丁写入，保留权限的原位更新仍可能在外部并发写入时发生竞争。

来源：

- https://docs.kernel.org/userspace-api/landlock.html
- https://docs.rs/landlock/0.4.7/landlock/enum.ABI.html
- https://github.com/anomalyco/opencode/blob/dev/packages/core/src/session.ts
- https://github.com/anomalyco/opencode/blob/dev/packages/server/src/handlers/session.ts
- https://github.com/anomalyco/opencode/blob/dev/packages/server/src/routes.ts
- https://docs.kernel.org/admin-guide/cgroup-v2.html

## 终端归档与创建身份（2026-11-18）

本轮直接读取 SQLite 的 Atomic Commit 与 synchronous pragma 文档，以及 Rust `std::fs::File` 文档。SQLite 的事务提交和文件系统删除需要明确排序；DELETE journal 下 EXTRA 在 FULL 之外同步回滚 journal 删除后的目录。Fliuno 因此先独立提交 tombstone，再删除输出文件，将无法完成的清理作为可重试状态，而不把删除目录当作遗忘创建身份。

Rust 文件锁的句柄生命周期用于区分仍被 supervisor 持有的记录与可归档记录；保存的 PID 不参与授权或清理。这个锁检查还不够处理已交付但延迟启动的 bootstrap，因此主机和 worker 在各自的执行边界分别检查账本。它们是 Fliuno 的设计选择，通过保留原 Starting spool 的真实延迟 worker 探针与可正常启动 PTY 的成功对照验证，完整契约见 [终端归档](terminal-archive.md)。

来源：

- https://sqlite.org/atomiccommit.html
- https://sqlite.org/pragma.html#pragma_synchronous
- https://doc.rust-lang.org/std/fs/struct.File.html#method.try_lock
- https://learn.microsoft.com/windows/win32/api/winnt/ns-winnt-jobobject_extended_limit_information
- https://learn.microsoft.com/windows/security/application-security/application-isolation/windows-defender-application-control/appcontainer/appcontainer-isolation
- https://developer.apple.com/documentation/security/app_sandbox
- https://docs.rs/heel/0.10.1/heel/
- https://github.com/May-Agent/heel/tree/v0.10.1
- https://docs.rs/sandboxrs-windows/0.1.1/sandboxrs_windows/
- https://docs.rs/rappct/0.13.3/rappct/
- https://docs.rs/cap-fs-ext/4.0.3/cap_fs_ext/trait.MetadataExt.html
- https://github.com/sunfishcode/cap-std/security/advisories/GHSA-hp8f-xmx4-4qrg

## Linux 独立 guardian 的生命周期

2026-11-18 再次核对 Linux man-pages 与 cgroup v2 文档：`PR_SET_PDEATHSIG` 跟随创建者线程的退出，且 fork 子进程不会继承该设置，不能直接承担线程池宿主与独立 PTY 树的生命周期。`pidfd_open(getpid(), 0)` 得到宿主进程整体的身份；pidfd 可用 poll 等待退出，避免从可重用的 PID 数字推断所有权。cgroup.kill 仍需由活着的执行者写入，故在 workload cgroup 外建立独立的 guardian。

Fliuno 普通 sandbox 的 guardian 监控宿主 pidfd；terminal service 在 exec 前通过 SCM_RIGHTS 交接自己的 pidfd，并收到确认后才执行。终端交接后不再跟随 app-server，因此主服务重启不终止 PTY，但 supervisor 退出会触发整树回收。创建时屏蔽 fork 线程的信号，guardian 子分支只使用系统调用与栈数据；关闭不必要描述符、重置继承 handler 后才解除信号屏蔽。它不会保留 app-server 的 stdio 或 SQLite 文件锁。清理失败保留目录，离线任务仍保持未知结果。

这些是 Fliuno 的实现选择。CI 脚本新增两组真实行为探针：隔离 owner 的创建线程退出后任务继续运行，再 SIGKILL owner 并等待 setsid 后代 stdout EOF；持久终端在 app-server SIGKILL 后重连继续输入，再关闭所有 app-server 并 SIGKILL supervisor，要求独立 guardian 删除 cgroup、后代结束且 spool 可查而不重启终端。当前 Windows 主机无法执行这些 Linux 内核探针，实际验收以 Ubuntu runner 为准。

来源：

- https://man7.org/linux/man-pages/man2/pidfd_open.2.html
- https://man7.org/linux/man-pages/man2/PR_SET_PDEATHSIG.2const.html
- https://man7.org/linux/man-pages/man7/unix.7.html
- https://man7.org/linux/man-pages/man7/signal-safety.7.html
- https://docs.kernel.org/admin-guide/cgroup-v2.html


## A2A 1.0 任务通道（2026-11-18）

核对 [A2A v1.0.0 specification](https://a2a-protocol.org/v1.0.0/specification/) 的核心模型、Part oneof、状态枚举与 JSON-RPC binding，固定 `A2A-Version: 1.0`、`SendMessage` / `GetTask` / `CancelTask`。1.0 wire 与 0.3 的方法、角色、状态及文件表示不同，不能把旧样例作为 1.0 一致性依据。

Fliuno 首批实现选择独立 transport crate 与中性 Agent adapter，以一次提交、有界轮询和取消取证覆盖任务生命周期。最终 Artifact 原 JSON 入库，保留文本/结构化数据/base64/URL；URL 不触发下载。卡片只验证主机固定接口，不产生新端点或凭据权限。暂停状态与远端失败有单独 stopReason，启动后断线/重启不隐式重发。

审阅了 [官方 Rust SDK 仓库](https://github.com/a2aproject/a2a-rs) 的公开入口，但本机克隆受网络连接失败阻断，未完成 SDK 构建或互操作验证。当前测试证据是按固定规范编写的独立 HTTP 契约 fixture、实际 app-server 进程与 SQLite 重启测试；不能等同于官方 SDK 或外部 Agent 互操作。SSE、认证、扩展与多轮继续留有单独的后续验收项，见 [A2A 边界](a2a.md)。

## 隔离进程的异步管道（2026-11-18）

直接审阅 Cargo.lock 固定的 Tokio 1.54.1、Mio 1.2.5 发布源码。Tokio fs 文档明确提示 special file 可能拖住 runtime shutdown；其文件 write 可在后台完成，丢弃 future 不取消底层操作。Tokio Linux `unix::pipe::from_file` 校验 pipe/访问模式并设置 nonblocking。Windows named pipe 虽使用 IOCP，但默认 `poll_flush` 直接成功；Mio 在提交 overlapped WriteFile 时接收数据，前一笔失败由下一笔 write 返回。因此不能仅替换类型便宣称 stdin 已完成。

Fliuno 增加 sandbox 内部 reactor 适配层，父端与子端分别创建为 overlapped/synchronous，只交付内部受控句柄。Windows writer 以写就绪和零字节 probe 等待上一笔完成并取出错误；不调用需要 reader 消费缓冲的 FlushFileBuffers。大块往返、已退出 reader、pending-write 后强杀及对端仍存活时取消读写的真实探针验证该实现；MCP/ACP 各保留官方 SDK stdio 契约。随后 Windows TrustedHost 的命令/MCP 也接入同一 native launcher / reactor，并以完整宿主权限、显式 cwd/环境和无隐式资源限额区分策略；Unix host 保留 Tokio process。Linux 行为须由实际 runner 验收，见 [执行证据](execution-evidence.md)。

来源：

- https://docs.rs/tokio/1.54.1/tokio/fs/index.html
- https://docs.rs/tokio/1.54.1/tokio/net/unix/pipe/index.html
- https://docs.rs/tokio/1.54.1/tokio/net/windows/named_pipe/struct.NamedPipeServer.html
- https://docs.rs/mio/1.2.5/src/mio/sys/windows/named_pipe.rs.html
- https://learn.microsoft.com/windows/win32/ipc/synchronous-and-overlapped-input-and-output
- https://learn.microsoft.com/windows/win32/api/fileapi/nf-fileapi-writefile

## Windows 并发继承与创建时 Job（2026-11-18）

直接读取 Microsoft 的继承、UpdateProcThreadAttribute 与 nested jobs 文档。`HANDLE_LIST` 要求句柄本身可继承，只限定当前这一次创建；不能阻止同宿主另一线程的无 allowlist `CreateProcess(TRUE)` 复制它们。Fliuno 的确定性单元探针已复现：创建私有管道后启动无关的 std::Command，即使释放原有子端，两条输出仍没有 EOF。它与较早的 5 秒启动预算超时是不同的故障机制。

实现把宿主中全部管道设为不可继承，只在始终挂起的短命当前 executable 副本中保存三个可继承副本。`PARENT_PROCESS` 从该进程继承句柄，`HANDLE_LIST` 限定三个值。中转创建时以 `JOB_LIST` 加入 kill-on-close / silent-breakaway Job，实际子进程在恢复前检查不属于该临时 Job；普通隔离 workload 的资源 Job 也随创建原子分配。这样既不要求宿主其他库配合锁，也不在创建后分配 Job 的空隙遗留挂起进程。

无关进程保持存活时的双流 EOF、创建者硬杀后的中转退出，以及原有 AppContainer 文件/TCP/输入/后代探针共同验收。中转增加一次进程创建，不代表降低启动延迟；Windows 10 / Server 2016 是该路径的最低 API 要求。完整边界与日志见 [沙箱](sandbox.md) 和 [执行证据](execution-evidence.md)。

来源：

- https://learn.microsoft.com/windows/win32/api/processthreadsapi/nf-processthreadsapi-updateprocthreadattribute
- https://learn.microsoft.com/windows/win32/procthread/inheritance
- https://learn.microsoft.com/windows/win32/procthread/nested-jobs
- https://learn.microsoft.com/windows/win32/procthread/job-objects

## 解释器伴随文件（2026-11-18）

直接读取 Node CLI、CPython 3.12 Windows 使用与 Microsoft DLL 搜索顺序的官方文档。Node 明确区分依赖模块的 `--preserve-symlinks` 和入口的 `--preserve-symlinks-main`，后者不隐含前者。实际 AppContainer 内原始 Node 启动查询磁盘根的 realpath，返回 `EPERM: lstat C:\\`；两个参数由演示宿主显式配置后，真实多文件 CommonJS MCP 初始化与调用成功，未扩大文件 ACL。

CPython 文档说明 embeddable distribution 的标准库 ZIP 与相邻 DLL 布局，以及 `._pth` 对注册表、环境模块路径与 site 的控制。Fliuno 没有下载解释器发行版，演示从现有 CPython 3.12 明确构建 fixture 所需的标准库 ZIP、DLL/扩展文件及 `python312._pth`，再逐项固定摘要。实际 AppContainer 运行成功并通过同样的宿主文件/TCP/环境正反对照。它是已验收的特定运行依赖布局，不是任意 Python 应用的自动依赖收集器。

实现为独立 sandbox 资源模块：源文件与目标路径明确列举，完整预检、能力目录下新建复制、复制字节的流式 SHA-256、单文件/合计/数量预算，启动后仍遵循现有 scratch 权限。exec 配置身份与带资源 MCP 别名包含 pins，真实 SQLite 重启测试验证新摘要不能复用旧审批。此阶段尚未覆盖完整 MCP/Agent 注册字段，后续已以独立[注册摘要](authority.md)绑定配置及实际使用值。Linux 资源路径已接入探针，当前 Windows 主机不能确认 Linux 内核运行结果，见[伴随文件](runtime-bundles.md)。

来源：

- https://nodejs.org/api/cli.html#--preserve-symlinks-main
- https://nodejs.org/api/cli.html#--preserve-symlinks
- https://docs.python.org/3.12/using/windows.html#the-embeddable-package
- https://docs.python.org/3.12/using/windows.html#finding-modules
- https://learn.microsoft.com/en-us/windows/win32/dlls/dynamic-link-library-search-order

## 有界批量归档（2026-11-18）

直接读取 SQLite 事务与 JSON-RPC 2.0 的官方规范。JSON-RPC batch 允许服务器任意顺序与并发宽度处理请求，响应顺序也不保证与输入一致；因此 Fliuno 用独立的应用方法 `terminal/archiveBatch` 明确规定完整 ID 列表预检、按序逐项处理与结果关联，而不从传输 batch 推导业务原子性。

SQLite 的 COMMIT 可能因读事务持有锁而返回 SQLITE_BUSY。单终端归档已有保守的提交不确定边界，批量模块保留该结果分类，不把它转成普通失败。实际测试用第二条 SQLite 连接持有 DELETE-journal 读事务，使插入之后的 COMMIT 超时，确认原 spool 和 endpoint 保留，再查询并显式重试。不同 ID 继续独立提交，批量操作不承诺跨项回滚。

来源：

- https://www.sqlite.org/lang_transaction.html
- https://www.jsonrpc.org/specification
