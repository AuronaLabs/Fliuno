# A2A 任务与产物

Fliuno 的 A2A 通道用于调用主机预先注册的远程 Agent。`fliuno-a2a` 只负责有界 HTTP 与 wire 校验；`fliuno-agents` 将协议转换为中性 Agent 观察；`fliuno-core` 保存任务、远程身份、结束原因、产物和事件。它与模型 provider、MCP 工具通道、ACP 本地进程后端分别维护。

## 固定版本与支持范围

本实现固定 [A2A v1.0.0 规范](https://a2a-protocol.org/v1.0.0/specification/) 的 JSON-RPC binding；请求发送 `A2A-Version: 1.0`。方法为 `SendMessage`、`GetTask`、`CancelTask`，使用 `ROLE_USER` / `ROLE_AGENT`、`TASK_STATE_*`、`text` / `data` / `raw` / `url` oneof。旧版 `message/send`、小写状态和 `kind/file` Part 不作为兼容协议接受。

默认使用 `returnImmediately: true` 与有界 `GetTask` 轮询，不请求历史。配置 `streaming: true` 时改用 `SendStreamingMessage` 和 SSE，不执行轮询回退；如果配置了 Agent Card，卡片还必须声明 `capabilities.streaming: true`。流按 JSON-RPC envelope 校验，每次调用最多接收配置的 `maxResponseBytes`，消费队列容量为 16，客户端变慢时网络读取随之暂停。首个 Task 固定远端任务/上下文身份，后续状态和产物更新必须匹配；状态回退、错序、超预算、非终态断流均保留未知结果，不重发提交。TaskStatusUpdateEvent 按 A2A 1.0 的状态字段判定结束，不依赖非标准的 `final` 标志。ArtifactUpdate 的 append/lastChunk 会在内存中按 artifactId 有界组装，只有完整产物随终态入库。取消在获得 Task 身份后最多发送一次 CancelTask，并要求终态响应确认。

支持 Task、立即返回的 Message 和可选 SSE 实时流。gRPC、HTTP+JSON binding、push notification、扩展、tenancy、OAuth、认证卡片、签名验证尚未实现；不声明完整 A2A 一致性。卡片只是可选的主机固定接口校验，不能配置或切换 URL、凭据、tenant 和 binding。

## 配置与调用

```json
{
  "agents": [{
    "id": "researcher",
    "endpoint": "https://agent.example.com/rpc",
    "agentCardUrl": "https://agent.example.com/.well-known/agent-card.json",
    "bearerTokenEnv": "FLIUNO_RESEARCHER_TOKEN",
    "requestTimeoutMs": 30000,
    "taskTimeoutMs": 300000,
    "pollIntervalMs": 500,
    "maxResponseBytes": 1048576,
    "streaming": true
  }]
}
```

只有 `id`、`endpoint` 必填。凭据值从主机环境读取，字段填的是变量名；配置变量但未提供值会使启动失败。整个配置先校验，之后才修改共享的 ACP/A2A Agent 注册表，合计最多 64 个，ID 不得重复。

```powershell
cargo run -p fliuno-app-server -- --a2a-config C:/absolute/path/to/agents.json
```

`agents/list` 返回 `protocol: "a2a_v1_jsonrpc"`、`isolation: "remote"`、`workspaceAccess: false`。通过现有 RPC 选择后端：

```json
{"jsonrpc":"2.0","id":3,"method":"task/start","params":{"sessionId":"session_...","input":"生成报告","clientRequestId":"research-1","backend":{"type":"external_agent","agentId":"researcher"}}}
```

选择该后端会将输入与创建时冻结的有界会话历史发送给注册端点。模型不能注册端点，也不能从远端响应获得宿主文件、终端或工具权限。产物只进入 Fliuno 数据库；不产生工作区文件。

## 身份、结束原因与恢复

`AgentStarted` 与运行 ID 在首次网络操作前提交。运行 ID 也是出站 `messageId`，用于审计，不提供重发保证。第一次 Task 的 `id` / `contextId` 在轮询前提交为 `agentRun.remoteTask`，后续响应必须匹配。每次状态变化产生 `agent_remote_task_updated`；重复状态不增加事件。远端终态不能回退或切换。

| 远端状态 | 本地状态 | `stopReason` |
| --- | --- | --- |
| submitted / working | running | 未结束 |
| completed | completed | end_turn |
| failed | failed | failed |
| rejected | failed | rejected |
| canceled | cancelled | cancelled |
| input_required | interrupted | input_required |
| auth_required | interrupted | auth_required |

已知暂停会保留 `outcomeUnknown: false`，表示已获得明确的暂停状态，远端任务仍可能存在。当前不继续该远端上下文，也不把等待认证解释成本地审批。运行结束表示本地适配器停止，不等价于所有远端状态均为终态。

本地取消立即提交，迟到响应不能改为完成。已知远程身份时，适配器最多发送一次 `CancelTask`，用独立的 2 秒窗口收尾；只有匹配的终态才消除结果未知。取消与完成竞争产生 `TaskNotCancelable` 时，窗口内允许一次 `GetTask` 取证。远端仍为 running、身份变化、错误或超时均保留未知。任务总时限或轮询预算耗尽也走同一取消取证路径，若明确取消则本地成为 cancelled。

提交返回前断线，可能无法获得远端 Task ID；后续轮询断线时，最后已知状态也不能证明任务停止。两者都结束为 `interrupted + outcomeUnknown`。不自动重发、重连提交或尝试另一个 URL。app-server 重启只恢复本地记录；已有运行禁止 `task/resume`。相同 `clientRequestId` 返回原任务。

## 显式核对远端状态

`initialize.capabilities.remoteTaskChecks` 为 true 时，客户端可用 `task/checkRemote` 传入 `{"taskId":"<本机任务 ID>"}`。该任务必须有已结束的 Agent 运行及已保存的远端身份，且当前宿主注册须与原任务的绑定一致。客户端不提供远端 ID、URL 或凭据。请求进入共享后台队列并立即返回 `{"checkId":"..."}`；同一任务已有在途核对或队列已满时返回状态冲突。

配置了支持流式的 A2A Agent 时，`initialize.capabilities.remoteTaskWatch` 为 true。调用 `task/checkRemote` 时可加 `waitMs`（1～5000），对已绑定远端任务和上下文 ID、且上次记录尚非终态的已结束本机任务发起一次 `SubscribeToTask`。预算包含可选 Agent Card 检查和订阅建立。首帧必须是身份相符的任务快照。如果状态已变化，立即记录该状态。否则继续等到检测到变化，或到期记录已验证的状态。快照前超时、协议中断或身份不符记为 `unavailable`。观察只追加核对事件，产物和原运行保持原状。

如果配置了卡片，先核对固定接口；之后只发一次 `GetTask`，使用已保存的 Task ID，且不请求远端历史。核对总时限为任务超时和 5 秒中的较小值，同时受单请求超时约束。通过 `notifications/event` 或 `events/list` 查找 `agent_remote_task_checked`，按 `checkId` 关联；事件还携带原 `runId`。`outcome.status` 为 `observed` 时有 `state`；`unavailable` 表示未取得可用响应（包括超时、远端错误或坏响应），`identity_mismatch` 表示远端 ID 或上下文不一致，`state_conflict` 表示响应与原记录的终态冲突或退回 submitted。

核对只追加状态证据，不改写原 `agentRun`、`outcomeUnknown`、本机任务状态或产物；也不取消或继续远端任务。若 app-server 在结果写入前退出，受理的核对可能没有结果事件，重启后不会自动恢复。客户端可显式发起新的只读核对。

## 显式续接远端任务

`initialize.capabilities.remoteTaskContinuation` 为 true 时，可调用 `task/continueRemote`，提交来源本机 `taskId`、`input` 和必填 `clientRequestId`。来源必须有已结束、结果已知的 A2A 运行，保存有上下文 ID，且宿主注册保持一致。续接建立独立的本机任务，不改写来源，也不附带本机会话历史。

- `input_required`：请求带原 `taskId` 和 `contextId`，响应须属于同一远端任务。同一暂停轮次只允许一个在途或可能已经发送的续接。
- `completed`：只带原 `contextId`，接受同上下文的新 Task 或未绑定任务的直接 Message。每个不同幂等键都是显式的新一轮。

两种模式都用新的持久运行 ID 作为消息 ID。后台先用 `GetTask` 对照原身份和状态，再记录 `agent_remote_continuation_sending` 并允许发送。该读取是发送前验证，不能锁定远端状态。首次响应身份通过验证后才会记录状态、导入产物或发起远端取消。终态产物保存为当前轮的独立快照，即使源 artifact ID 曾出现也不会覆盖之前的本地产物。

预检拒绝的运行结束原因为 `remote_preflight_failed`。发送标记前停止可用新键显式尝试；标记后断流或重启按可能已发送处理。原键永远只返回原尝试，`task/resume` 不重发外部 Agent。数据库 v11 在同一事务保存续接任务、幂等键和暂停轮次占位。

## 产物保存

最终 Task 快照中的产物一次性导入：源 `artifactId` 与新生成的本地 Artifact ID 分开保存到 `agentRun.artifacts`，产物、链接、预算及 `artifact_created` 事件在同一事务提交。重复源 ID、重复导入、数量/字节超限会拒绝。运行可能在导入后、完成前中断；已提交的产物保留供检查，任务不会因此被自动标记完成。

本地 Artifact 的 `mediaType` 为 `application/json`，`content` 是完整的 A2A Artifact JSON，保留 Part、文件名、媒体类型、metadata 等字段。`raw` 只校验 ProtoJSON base64，不写入本机文件；`url` 只保存，不下载或打开，即使它指向本机或工作区。文本 Part 也投影到任务输出。立即 Message 以同样方式保留完整消息 JSON，并投影其文本。只有终态导入 Task 产物，暂停态的部分产物不宣称为最终交付。

## 有界传输

- HTTPS 或字面 IP 回环 HTTP；禁止 userinfo、query、fragment、控制字符。卡片与端点必须同源。
- 不使用环境 HTTP 代理，不跟随重定向，不自动重试。请求与响应只接受当前 JSON 模式。
- 每次请求超时 100..3600000 ms；任务总时限同范围，默认 300000 ms。轮询间隔 25..30000 ms，最多 4096 次。
- 请求最多 1 MiB；响应默认 1 MiB，可配置 1024..4194304 字节，头部长度与逐块读取均校验。
- 每个 Task 最多 64 个产物、每个产物 256 KiB、每个内容容器最多 256 个 Part。内核另有 8 MiB 记录、1 MiB 文本、4096 条更新预算。
- 远端错误只保留固定错误类型与数值 code；HTTP/JSON 解析异常不会输出响应体或 URL。凭据由 HTTP 层发送，不作为运行配置或事件存储。

## 验证与演示

```powershell
cargo test -p fliuno-a2a -p fliuno-agents -p fliuno-core --all-features --locked
cargo test -p fliuno-app-server --all-features --locked a2a
python examples/a2a_demo.py
python examples/a2a_demo.py --cancel
python examples/a2a_demo.py --streaming
python examples/a2a_demo.py --streaming --cancel
```

测试覆盖真实 loopback HTTP wire、标准头与方法、响应关联、Part oneof / ProtoJSON bytes、卡片接口与扩展拒绝、重定向隔离、响应预算、超时，以及真实 app-server 的 SQLite 重启、幂等、产物、上下文、失败/暂停、取消和显式核对。远端任务证据在 v7 引入，旧 Agent 记录的新增字段默认空；v8 增加排队任务的注册绑定，v9 增加[显式上下文选择](context.md)，v10 增加核对事件。迁移不改写原远程证据。

`Task.backendAuthority` 在创建时冻结全部 A2A 配置及实际 bearer token 的摘要。相同 Agent ID 更换端点、凭据、卡片地址或执行预算，重启后旧排队任务不能启动；幂等提交仍返回原任务，不刷新摘要。该检查不认证远端代码，也不允许重发结果未知的提交，详见[注册身份](authority.md)。

这些是独立契约 fixture 与本机进程验收。官方 Python SDK `a2a-sdk==1.3.1` 已通过回环验收，覆盖 Agent Card、JSON 轮询和 SSE 的暂停、补答、同一上下文的新任务、幂等与产物入库。真实外部 Agent 验收仍待执行。

A2A 1.0 规定 `SendStreamingMessage` 以 `Task` 或 `Message` 开始。Python SDK 对已有任务补答时可能直接发 `statusUpdate`。Fliuno 仅在 `GetTask` 已核对暂停状态、请求显式携带 `taskId` 和 `contextId`、且首帧两项身份均匹配时兼容这一行为。未收到合法首帧时保留提交结果未知，也不会根据预检取得的旧状态写入本轮身份或产物。新任务和同一上下文的新一轮仍要求标准首帧。

使用 Python 3.12 及隔离环境在 Windows 复现：

```powershell
python -m venv target/a2a-sdk-interop
& ./target/a2a-sdk-interop/Scripts/python.exe -m pip install 'a2a-sdk[http-server]==1.3.1' 'uvicorn==0.48.0'
& ./target/a2a-sdk-interop/Scripts/python.exe examples/a2a_sdk_interop.py
```
离线演示覆盖轮询和 SSE 的成功/取消闭环，并验证分块产物组装及流式模式不会回退轮询。

2026-11-18 本机 Windows 最终全工作区回归：204 passed、0 failed、0 ignored，41 个测试目标；fmt、所有目标/feature 的 Clippy、Schema 一致性、两个离线演示与 Linux 目标交叉 Clippy 通过。交叉检查不能代替 Linux 内核探针的实际运行。

验证中还修正了一处既有 PTY 测试竞态：跨过输出字节阈值不能证明尾部标记已入 spool，测试改为按游标等待标记并检查退出尾部字节。首次全量还出现两项原生命令执行的未知结果；增加执行模式/事件报告后，串行、相同 feature 的并行和最终全量均通过，暂未复现根因。初轮、诊断与最终日志分别保存在 `target/verification/a2a-workspace-tests.log`、`native-exec-diagnostic.log`、`a2a-workspace-tests-final.log`。

同日后续增加原生启动耗时与停止原因后，复现两次 AppContainer 启动分别耗时约 8.35 秒、8.95 秒，超过成功 fixture 的 5 秒预算，停止原因均为 deadline。成功路径改用 30 秒预算，独立短超时探针保留；stdin shutdown 错误和停止后的写入边界也一并修正，详见 [执行证据](execution-evidence.md)。这解释了本轮复现的失败机制，未确定 OS 启动延迟的具体环境成因。
