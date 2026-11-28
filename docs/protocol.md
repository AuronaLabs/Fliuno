# Fliuno Application Protocol 0.1

[文档中心](README.md) › [接入客户端](README.md#接入客户端) · [运行与配置](setup.md) · [JSON Schema](../protocol/fliuno.schema.json)

应用协议连接本机客户端与 Fliuno 内核。先握手并读取服务能力，再发起任务、处理审批或订阅持久事件。

| 要实现什么 | 查阅 |
| --- | --- |
| **建立连接** | [启动服务](setup.md#再启动自己的服务) · [stdio 传输](#传输) · [Rust 调用方](#rust-调用方) · [AG-UI HTTP / SSE](ag-ui.md#http-接入) |
| **提交并交付** | [创建任务与重试](#任务提交与重试) · [持久 Run DAG](#持久-run-dag) · [固定上下文预览范围](context.md#提交任务时固定预览范围) · [审批与决定](#审批收件箱与决定) · [读取交付物](#审批事件与产物) · [复用交付物](#将交付物用作任务输入) |
| **工作区记忆** | [记忆接口与访问审计](#工作区长期记忆) · [任务上下文中的记忆](memory.md) |
| **同步工作台** | [首屏与订阅](#工作台实时订阅) · [会话任务计数](#批量读取会话任务计数) · [合并摘要修订](#按窗口合并摘要修订) |
| **恢复与历史** | [批量找回已提交任务](#按请求键批查任务) · [判断能否恢复](#任务恢复判断) · [批量查询恢复状态](#批量查询恢复状态) · [查找历史任务](#查找历史任务) |
| **排查调用失败** | [错误码](#错误) · [内置工具与作用域](#内置工具与作用域) |

<details>
<summary>展开完整主题索引</summary>

- **任务与工作台**：[工作区、会话与任务](#工作区会话与任务) · [会话目录](#会话目录) · [任务目录](#任务目录) · [任务提交与重试](#任务提交与重试) · [模型尝试次数](#模型尝试次数) · [工作台快照](#工作台快照) · [实时订阅](#工作台实时订阅) · [待审批快照](#待审批快照) · [批量取消任务](#批量取消任务)
- **恢复与查询**：[变动摘要](#任务变动摘要) · [摘要修订](#任务摘要修订回放) · [轻量回放](#任务变动回放) · [批量读取任务摘要](#批量读取任务摘要) · [搜索历史](#查找历史任务) · [模型用量](#模型用量)
- **事件与交付物**：[审批、事件与产物](#审批事件与产物) · [交付物目录订阅](#交付物目录订阅) · [批量校验摘要](#批量读取交付物摘要)
- **编排与工具**：[持久 Run DAG](#持久-run-dag) · [工具、Agent 与终端](#工具agent-与终端) · [A2A](#a2a-远程-agent-配置) · [MCP](#mcp-主机配置) · [内置工具](#内置工具与作用域)

</details>

## 传输

本机 stdio，每行一个 UTF-8 JSON-RPC 2.0 值；支持 batch 和 notification。所有业务方法使用命名对象参数；未知参数拒绝。单个输入帧最多 1 MiB。 单批最多 128 项，包含通知。超限时返回 `-32600`，整批不执行。客户端可从 `capabilities.maxBatchItems` 读取限制。进程不监听 TCP，不把本机调用接口当作已认证的远程 API。

连接先 `initialize`，版本必须匹配 `0.1`。返回 `capabilities` 供客户端决定交互，不应由客户端猜测支持情况。初始化状态属于连接；工作区、会话、任务、审批和产物属于数据库。

`capabilities.registrationAuthority:true` 表示工具审批以 `Approval.authority`、外部 Agent 任务以 `Task.backendAuthority` 绑定具体注册摘要；配置变化或旧记录缺少摘要时拒绝执行，需要新建任务。摘要由宿主生成，客户端不得以传入摘要的方式选择或刷新授权，详见[注册身份与 v8 迁移](authority.md)。

宿主命令行指定的 MCP/ACP/A2A/exec/terminal JSON 各最多 1 MiB，全部先经过有界读取与反序列化，再打开数据库和注册适配器。加载诊断只返回种类和 JSON 行列位置，不回显配置内容；语义校验仍由各适配器负责，见[启动配置边界](host-configuration.md)。

## MCP 启动授权

仅当宿主显式传入 `--mcp-oauth-config` 时，app-server 在普通 `initialize` 前进入此阶段。它接受同一 stdin/stdout 上的单条 JSON-RPC 2.0 请求，每行一个对象，ID 为整数或至多 128 字节的字符串。启动授权阶段不接受批次或通知，也不会打开 SQLite 或启动工具。宿主应保持这个管道私有，且不得记录授权 URL 或完整回调。

| 方法 | 参数 | 结果 |
| --- | --- | --- |
| `mcp/oauth/pending` | `{}` | `{serverIds: string[], vaultLocked: boolean}` |
| `mcp/oauth/unlock` | `{key, reauthorizeServerIds?}` | `{remaining}`；仅在指定独立凭据库时使用 |
| `mcp/oauth/begin` | `{serverId, wwwAuthenticate?}` | `{attemptId, authorizationUrl}`；同一待答尝试可重复读取 |
| `mcp/oauth/finish` | `{serverId, attemptId, callbackUrl}` | `{serverId, remaining}`；尝试匹配后只消费一次，失败后须重新 `begin` |

宿主跟随 `authorizationUrl` 打开浏览器，在配置好的回调地址接收返回值，并将完整 URL 交给 `finish`。`wwwAuthenticate` 可传入宿主从该 MCP 端点获得的认证挑战；省略时按规范的 well-known 地址发现。`remaining` 为零时进入普通 `initialize`，后者成功才表示工具已注册。授权阶段总时限 10 分钟；重启或关闭管道会丢弃旧尝试。`-32060` 表示等待授权，`-32061` 表示发现或客户端注册失败，`-32062` 表示回调验证或换取令牌失败。错误只包含固定描述；授权码和令牌不出现在业务 RPC、事件和备份里。

配置 `--mcp-oauth-vault PATH` 时，宿主先通过 `unlock` 提供经无填充 Base64URL 编码的 32 字节随机密钥。`-32063` 表示凭据库待解锁，`-32064` 表示无法打开或认证凭据库。丢失或已消耗的授权仍出现在解锁后的 `pending`；有效授权自动恢复。传 `reauthorizeServerIds` 可让指定服务重新登录，且不会改动其他服务。密钥只在私有启动管道传输，不写入参数文件、日志或事件。

## Rust 调用方

`fliuno-client::Client` 接收调用方持有的 stdio 读写端，建立连接时执行 `initialize` 并校验协议版本。`info()` 返回服务版本和能力。`call()` 可直接使用 `fliuno-protocol` 的参数和结果类型。每条连接一次等待一个请求，期间到达的通知通过回调交给调用方，空闲时可用 `read_notification()` 阻塞读取一条通知。

```rust
use fliuno_client::Client;
use fliuno_protocol::{Event, EventGetParams};

let mut client = Client::connect(reader, writer, "workspace-client", |_| {})?;
let event: Event = client.call("events/get", &EventGetParams { sequence: 1 }, |notice| {
    handle_notification(notice);
})?;
```

请求与服务端共用 1 MiB 帧上限，客户端响应帧上限为 16 MiB，均包含换行。合法的 RPC 错误保留连接。读写失败、响应 ID 错位或无效帧会关闭客户端对该连接的使用权限。调用方管理子进程和阻塞 I/O 的生命周期。正常退出时可用 `into_parts()` 取回管道，关闭写端并把读端读到 EOF，再等待进程退出。重连后可用审批决定查询和事件游标核对状态。

`EventReplay` 接收作用域、本地序号和包含式服务水位，按对应方法逐页读取。每页检查任务归属（任务范围）、序号、游标和页大小。`cursor()` 可随已消费事件一起持久化。范围内没有更多事件时，游标直接推进到服务水位。补读期间到达的实时通知仍交给调用方暂存。

`Client::subscribe_events(scope, after, callback)` 建立实时订阅，并用同一范围和服务端返回的水位构造 `EventReplay`。首次连接、重连或收到队列缺口后，都可以从该范围已消费的序号开始。订阅响应之前的持久通知仍归前一个订阅。得到响应后，先完成回放至 `through()`，再处理回放期间暂存的新通知。支持 `eventSubscriptionCursor` 的服务会在切换之前拒绝过新的本地序号。连接旧服务时，客户端改在收到水位后检查。

`HistorySearch` 可以按会话或工作区搜索已完成的对话。将 `HistorySearchRequest` 交给 `HistorySearch::new`，每次 `next_page` 读取一个搜索窗口。空窗口可能仍可继续；只有 `next_page` 返回 `None` 时才无需再发请求。随已消费结果保存可序列化的 `next_request()`；请求失败或返回不一致时保留原游标。`Client::get_history_turn` 可按命中的会话和任务 ID 直接读取一条对话。

`Client::artifact_digest` 读取产物的正文长度和 SHA-256，调用方也可直接提供已保存的摘要。`Client::download_artifact` 按 64 KiB 分段读取，检查每段的长度和偏移，将原始字节写入调用方提供的 `Write`，返回前校验完整 SHA-256。需要在验证完成后才对外可见时，先写到临时文件，再由调用方发布。失败时输出中可能已有部分字节。

首次下载且尚无摘要时，可用 `Client::download_artifact_with_digest`。客户端通过 `artifacts/openDownload` 取得摘要和首块，验证首块格式后，在任何正文写入前调用 `record_digest`。宿主可以在这里将摘要与产物 ID 一起持久保存；回调失败就停止写入。下载完成前仍校验完整 SHA-256。连接旧服务时，客户端自动改用 `artifacts/digest` 和 `artifacts/read`。

重连后，可用 `Client::resume_artifact_download` 继续写入原暂存文件。保持原 `taskId`、`artifactId`、`contentBytes` 和 `sha256`，独占打开暂存文件供读写，不截断已有内容：

```rust
let mut staged = std::fs::OpenOptions::new()
    .read(true).write(true).open(part_path)?;
client.resume_artifact_download(&artifact, &expected, &mut staged, |_| {})?;
staged.sync_all()?;
// 验证完毕，由调用方关闭文件并发布。
```

客户端从文件长度取得字节偏移，先在本机重算已有前缀的哈希，再请求剩余字节。偏移可落在 UTF-8 字符中间；前缀哈希和分块解码共用约 64 KiB 的字节缓冲区。已完整的文件直接校验，无需向服务发请求。失败可能留下部分写入的暂存内容；最终摘要不符时，应丢弃暂存内容。旧服务只要支持 `artifacts/read` 即可使用续传，无需新的协议能力。

## 方法

### 工作区、会话与任务

| 方法 | params | result |
| --- | --- | --- |
| `initialize` | `{protocolVersion, clientName}` | 服务身份与能力 |
| `workspace/list` | `{after?:0,limit?:100}` | Workspace ObjectPage |
| `session/list` | `{workspaceId,archived?,order?,after?:0,limit?:100}` | Session ObjectPage，可按归档状态筛选及按创建顺序倒序分页 |
| `task/list` | `{sessionId,after?:0,limit?:100,targetBytes?}` | Task ObjectPage |
| `task/listSummaries` | `{sessionId,status?,order?,after?:0,limit?:100,targetBytes?}` | 会话内的 TaskSummary ObjectPage；游标来自完整任务列表使用的创建行位置 |
| `task/listWorkspaceSummaries` | `{workspaceId,status?,order?,after?,limit?,targetBytes?}` | 跨会话分页读取同一工作区的任务摘要，可按创建顺序正序或倒序返回 |
| `work/getSessionCounts` | `{workspaceId,sessionIds:[...]}` | [批量读取会话任务计数](#批量读取会话任务计数) |
| `work/getSnapshot` | `{workspaceId?,sessionId?,approvalLimit?,approvalTargetBytes?,recentTaskLimit?}` | 同一水位的任务状态计数、待审批首页和可选的最近任务 |
| `work/subscribe` | 同 `work/getSnapshot` | 同一快照，并将本连接切换为该范围的事件水位订阅 |
| `workspace/open` | `{root}` | Workspace；根目录必须存在 |
| `workspace/get` | `{workspaceId}` | 按 ID 读取已保存的 Workspace。查看 `initialize.capabilities.workspaceById` |
| `workspace/getProfile` | `{workspaceId}` | 当前 WorkspaceProfile，包含原工作区和可选的显示名称 |
| `workspace/listProfiles` | `{after?:0,limit?:100}` | WorkspaceProfile ObjectPage，与 `workspace/list` 的游标相同 |
| `workspace/setTitle` | `{workspaceId,title}` | 持久化显示名称并返回 WorkspaceProfile，`title:""` 清除名称 |
| `workspace/memory/put` | `{workspaceId,key,content,sourceTaskId?,expectedRevision?}` | 新建或按 revision 更新/恢复一条工作区记忆 |
| `workspace/memory/get` | `{workspaceId,key,actorTaskId?}` | 读取一条活动记忆并记录访问审计 |
| `workspace/memory/list` | `{workspaceId,after?,limit?,includeDeleted?,actorTaskId?}` | 分页列出记忆；可显式包含软删除项 |
| `workspace/memory/search` | `{workspaceId,query,after?,limit?,actorTaskId?}` | 对 key 和正文进行字面子串检索，已删除项不参与 |
| `workspace/memory/delete` | `{workspaceId,key,expectedRevision,actorTaskId?}` | 按 revision 软删除并返回 tombstone |
| `workspace/memory/listAudit` | `{workspaceId,after?,limit?}` | 分页读取该工作区记忆的写入及访问审计 |
| `workspace/searchHistory` | `{workspaceId,query,role?,throughSequence?,beforeSequence?,limit?,scanLimit?}` | SessionHistorySearchPage，跨会话查找已完成任务，命中包含 `sessionId` |
| `session/create` | `{workspaceId,title?,clientRequestId?}` | Session |
| `session/get` | `{sessionId}` | 单个 Session |
| `session/rename` | `{sessionId,title}` | 持久化标题并返回 Session |
| `session/setArchived` | `{sessionId,archived}` | 设置持久归档标记并返回 Session |
| `session/setArchivedBatch` | `{sessionIds,archived}` | 原子更新 1..32 个会话，并按请求顺序返回 Session |
| `session/context` | `{sessionId,throughSequence?,options?:ContextOptions}` | `TaskContext`，按[选择策略和事件范围](context.md#协议与使用)预览历史 |
| `session/listHistory` | `{sessionId,afterSequence?,throughSequence?,beforeSequence?,limit?,maxBytes?}` | `SessionHistoryListPage`，按完成序号倒序分页读取精简对话。见[会话历史分页](context.md#会话历史分页)。 |
| `session/subscribeHistory` | `{sessionId,afterSequence?,limit?,maxBytes?}` | `SessionHistoryListPage`，返回历史首页并切换到同会话的事件水位订阅。见[订阅会话历史](context.md#订阅会话历史)。 |
| `session/getHistoryTurn` | `{sessionId,taskId,throughSequence?,maxBytes?}` | `SessionHistoryEntry`，直接读一条对话或返回超限引用。见[打开搜索命中](context.md#打开一条历史记录)。 |
| `session/searchHistory` | `{sessionId,query,role?,throughSequence?,beforeSequence?,limit?,scanLimit?}` | SessionHistorySearchPage，按完成序号查找历史任务 |
| `task/start` | `{sessionId, input, clientRequestId?, backend?, maxModelSteps?, context?:{mode?,maxTurns?,maxBytes?,selection?,taskIds?}}` | Task；默认模型；可选 `{type:"external_agent",agentId}` |
| `task/startSummary` | 与 `task/start` 相同 | 使用同一任务创建和调度流程，返回 `TaskSummary` |
| `run/start` | `{sessionId,clientRequestId,maxConcurrency?,maxModelSteps?,timeoutMs?,context?,nodes:[{nodeId,input,dependsOn?}]}` | 持久 Run 与当前节点/任务状态 |
| `run/get` | `{runId}` | 更新调度状态后读取 Run |
| `run/cancel` | `{runId}` | 取消所有未结束节点，并返回 Run |
| `run/events` | `{runId,after?:0,limit?:64}` | RunEventsPage，按游标续读持久 Run 事件 |
| `task/getSummaryByRequest` | `{sessionId,clientRequestId}` | 通过会话和请求键读取当前 `TaskSummary` |
| `task/getSummariesByRequest` | `{sessionId,clientRequestIds}` | [按请求键批查任务](#按请求键批查任务)，返回同一水位的 `TaskSummaryBatch` |
| `task/get` | `{taskId}` | Task |
| `task/respondToolInput` | `{taskId,callId,requestId,responses}` | Task；提交补答并继续同一获批工具调用，能力标记为 `taskRespondToolInput` |
| `task/getSummary` | `{taskId}` | 单条 `TaskSummary`，可以轻量轮询任务状态 |
| `task/getSummaries` | `{taskIds,workspaceId?,sessionId?}` | 从同一查询快照按请求顺序返回摘要或 `null`，附上事件水位 |
| `task/checkRemote` | `{taskId,waitMs?}` | `{checkId}`；后台只读核对，结果经事件返回，见 [A2A](a2a.md#显式核对远端状态) |
| `task/getRemoteCheck` | `{taskId,checkId}` | 指定核对的持久 `Event`。见 `initialize.capabilities.remoteTaskCheckLookup` |
| `task/continueRemote` | `{taskId,input,clientRequestId}` | 独立的本机 Task；后台复核后向原远端任务或上下文提交，见 [A2A](a2a.md#显式续接远端任务) |
| `task/continueAgentSession` | `{taskId,input,clientRequestId}` | 独立的本机 Task，在原 ACP 会话中发送新一轮，见 [ACP](acp.md#跨重启续接-agent-会话) |
| `task/cancel` | `{taskId}` | Task；重复取消可安全调用 |
| `task/cancelSummary` | `{taskId}` | 复用取消流程，返回 `TaskSummary`。能力标记为 `taskCancelSummary`。 |
| `task/cancelBatch` | `{scope,taskIds}` | 原子提交选中任务的取消，返回 `{items:[{task,changed}],throughSequence}` |
| `task/getResumeStatus` | `{taskId}` | [`TaskResumeStatus`](#任务恢复判断)，查询当前能否恢复及原因 |
| `task/getResumeStatuses` | `{taskIds,workspaceId?,sessionId?}` | [`TaskResumeStatusBatch`](#批量查询恢复状态)，按入参顺序返回同一读取时点的恢复状态 |
| `task/resume` | `{taskId,retryUncertainModelStep?:false}` | Task；恢复后排队；外部未知结果不能重试；模型结果不确定时须显式传 true |

### 审批、事件与产物

| 方法 | params | result |
| --- | --- | --- |
| `approval/listPending` | `{workspaceId?,sessionId?,after?:0,limit?:100,targetBytes?}` | `{items,nextAfter,hasMore}`，只包含当前待审批的摘要 |
| `approval/listPendingSnapshot` | `{page:{workspaceId?,sessionId?,after?,limit?,targetBytes?},snapshot?}` | `{snapshot,page}`，在固定事件水位和到期判断时间读取待审批摘要 |
| `approval/get` | `{taskId,approvalId}` | `{kind,approval}`，包含指定待审批项的完整请求 |
| `approval/getBatch` | `{approvals:[{taskId,approvalId},...]}` | [批量读取仍待处理的审批详情](#批量读取待审批详情) |
| `approval/getDecision` | `{taskId,approvalId}` | 已提交决定对应的 `Event`，用于重连核对 |
| `approval/getDecisions` | `{approvals:[{taskId,approvalId},...]}` | [按请求顺序读取持久回执](#批量读取审批决定) |
| `policy/readTools` | `{}` | 返回支持[只读规则](authority.md#可撤销的本机只读规则)的注册项。 |
| `policy/createReadRule` | [规则参数](authority.md#可撤销的本机只读规则) | 创建绑定工具摘要与精确参数的只读规则。 |
| `policy/list` | `{workspaceId}` | 返回当前有效规则。 |
| `policy/revoke` | `{workspaceId,ruleId}` | 撤销规则并返回记录。 |
| `approval/respond` | `{taskId, approvalId, decision}` | Task；工具执行后排队继续模型步骤；decision 为 `allow_once` 或 `deny` |
| `approval/respondDecision` | 与 `approval/respond` 相同 | 提交决定并返回已持久化的 `Event`。 |
| `approval/resumeRun` | `{sessionId,parentRequestId,clientRequestId,approvalId,decision}` | `ApprovalRun`；提交决定并创建同一任务的子运行。 |
| `approval/getRun` | `{sessionId,clientRequestId}` | `ApprovalRun`；通过创建或续跑请求键查找当前任务摘要与执行段起点。 |
| `approval/getRunWindow` | `{sessionId,clientRequestId}` | `ApprovalRunWindow`；返回 `taskId` 和执行段的起止事件序号。 |
| `approval/getRunReplay` | `{sessionId,clientRequestId}` | `ApprovalRunReplay`；在同一快照读取 `run`、`window` 和 `throughSequence`。 |
| `approval/getRunPending` | `{sessionId,clientRequestId,approvalId}` | `ApprovalDetails`；读取结束该执行段且仍未处理的审批。 |
| `approval/cancelRun` | `{sessionId,clientRequestId}` | `TaskSummary`；仅允许当前运行键控制任务。已有新运行接管时返回 `-32009`。 |
| `events/list` | `{taskId, after?:0, through?, limit?:100, targetBytes?}` | `{events, nextAfter, hasMore}` |
| `events/listModelUsage` | `{taskId, after?:0, through?, limit?:100, targetBytes?}` | 仅返回该任务的模型用量事件，沿用全局序号 |
| `events/listSessionModelUsage` | `{sessionId,after?,through?,limit?,targetBytes?}` | 分页读取会话下各任务的模型用量 |
| `events/listWorkspaceModelUsage` | `{workspaceId,after?,through?,limit?,targetBytes?}` | 分页读取工作区各会话的模型用量，包含已归档会话 |
| `modelUsage/getSummary` | `{scope:{kind:"task",taskId}}`、`{scope:{kind:"session",sessionId}}` 或 `{scope:{kind:"workspace",workspaceId}}` | `{throughSequence,totals}`，读取该范围内已保存报告的累计值 |
| `modelUsage/getSummaries` | `{scopes:[scope,...]}` | `{throughSequence,items}`，按输入顺序返回同一读视图中的累计值 |
| `modelUsage/summarizeRange` | `{scope,after?:0,through?,limit?:256}` | [按事件范围汇总](#按事件范围汇总)，返回此页的精确用量和续页游标 |
| `events/listTaskChanges` | `{taskId?,sessionId?,workspaceId?,after?,through?,limit?}` | 固定水位并回放发生变动的任务 ID，每页去重，不返回事件正文 |
| `events/listLatestTaskSummaryRevisions` | `{taskId?,sessionId?,workspaceId?,after?,through?,limit?}` | [按窗口合并摘要修订](#按窗口合并摘要修订)，整个窗口内每个任务只返回一次 |
| `events/getWatermark` | `{}` | 当前已提交的全局 `throughSequence`，供多个回放读取方共用 |
| `events/getTaskWatermarks` | `{taskIds}` | 按输入顺序返回每个任务的最新事件序号；没有持久事件的项为 `null` |
| `events/listAll` | `{after?:0, through?, limit?:100, targetBytes?}` | 全任务共享全局序号的事件页 |
| `events/get` | `{sequence}` | 按全局序号读取一条持久事件 |
| `events/listSession` | `{sessionId, after?:0, through?, limit?:100, targetBytes?}` | 单个会话的持久事件页，沿用全局序号 |
| `events/listWorkspace` | `{workspaceId,after?,through?,limit?,targetBytes?}` | 按全局游标回放工作区各会话的持久事件 |
| `events/setScope` | `{taskId?,sessionId?,workspaceId?,after?,watermarkOnly?}` | 将实时持久事件限定到一个任务、会话或工作区，也可恢复全局范围，返回 `{throughSequence}` |
| `events/resubscribe` | `{}` | 在队列溢出后恢复实时通知，返回 `{throughSequence}` |
| `artifacts/list` | `{taskId}` | Artifact 数组 |
| `artifacts/listSummaries` | `{taskId,after?,limit?}` | 按保存顺序分页，返回 `{items,nextAfter,hasMore}` |
| `artifacts/listScopedSummaries` | `{scope:{kind:"session",sessionId} 或 {kind:"workspace",workspaceId},after?,limit?,mediaType?}` | 按保存顺序查询指定会话或工作区的产物元数据 |
| `artifacts/listSnapshot` | `{scope,after?,limit?,mediaType?,throughSequence?}` | 返回目录水位和该水位以内的元数据页 |
| `artifacts/subscribe` | `{scope,after?,limit?,mediaType?}` | 一次返回当前目录首页和事件水位，并替换连接的实时订阅 |
| `artifacts/getSummary` | `{taskId,artifactId}` | 单份 ArtifactSummary |
| `artifacts/get` | `{taskId,artifactId}` | 单份完整 Artifact |
| `artifacts/digest` | `{taskId,artifactId}` | 正文的 `contentBytes` 与小写十六进制 `sha256` |
| `artifacts/openDownload` | `{taskId,artifactId}` | `{digest,firstChunk}`，一次取得摘要和首块 |
| `artifacts/openDownloads` | `{artifacts:[{taskId,artifactId},…],limit?:16384}` | [批量下载首块](#批量下载首块)，按输入顺序返回首块和摘要状态 |
| `artifacts/getDigests` | `{artifacts:[{taskId,artifactId},…]}` | 同顺序的 `{items}`，只读取已保存的摘要 |
| `artifacts/read` | `{taskId,artifactId,after?,limit?}` | 返回 `{offset,nextOffset,contentBytes,dataBase64}` |

### 工具、Agent 与终端

| 方法 | params | result |
| --- | --- | --- |
| `tools/list` | `{}` | 工具名称、描述、效果类型、输入 Schema |
| `agents/list` | `{}` | 注册 Agent 的 ID、协议、隔离类型和 `workspaceAccess` |
| `agents/getCapabilities` | `{agentId}` | 单个注册项的 `AgentCapabilities`。见 `initialize.capabilities.agentCapabilitiesById` |
| `agents/startSessionList` | `{agentId,cursor?}` | `{requestId}`，排队读取已启用目录的 Agent 的一页会话 |
| `agents/getSessionList` | `{requestId}` | `AgentSessionListState`，读取进程内请求状态 |
| `terminal/list` | `{sessionId}` | 此 session 的持久终端快照 |
| `terminal/get` | `{sessionId,terminalId}` | 单个终端快照，独立于输出读取。live IPC 无响应时按最后持久状态返回 |
| `terminal/output` | `{sessionId,terminalId,after?:0,limit?:32768}` | base64 原始字节、绝对游标、gap 标志与快照 |
| `terminal/tail` | `{sessionId,terminalId,limit?:32768}` | 最近保留的原始字节和末尾游标，可用 `next` 续读 |
| `terminal/subscribe` | `{sessionId,terminalId,after?:0,limit?:32768}` | `{subscriptionId,sessionId,terminalId,after}`；随后推送输出与状态 |
| `terminal/unsubscribe` | `{subscriptionId}` | `{removed}`；取消此连接内的观察 |
| `terminal/archive` | `{sessionId,terminalId}` | `{snapshot,cleanupPending}`；归档已停止监督器的记录，保留创建身份 |
| `terminal/archiveBatch` | `{sessionId,terminalIds}` | `{items}`；1..32 个不同 ID，全列表预检、按序逐项归档，独立返回 archived/failed/outcome_unknown |
| `terminal/archives` | `{sessionId,after?:0,limit?:100}` | 归档历史 ObjectPage，每项为 TerminalArchive |

`initialize.capabilities.sessionTitles` 表示会话支持标题。标题最多 120 个 Unicode 标量值，去除首尾空白后须至少保留一个字符，且不能包含控制字符。未命名的会话省略 `title`。改名保留会话 ID、所属工作区、分页顺序和已有任务。SQLite v15 保留既有会话的快照，新客户端读取旧数据时会看到未命名会话。

`initialize.capabilities.workspaceProfiles` 表示支持工作区显示名称。`title` 最多 `maxWorkspaceTitleChars` 个 Unicode 字符。服务去掉首尾空白，拒绝控制字符和只有空白的名称。明确传 `""` 可清除名称。名称保存在 SQLite v35 的独立表里。工作区 ID、规范化根目录、清单顺序以及 `workspace/open`、`workspace/get`、`workspace/list` 的响应格式沿用原协议。原目录暂时不可访问时仍可读取和修改名称。

### 工作区长期记忆

`initialize.capabilities.workspaceMemorySemanticIndex` 表示支持 `workspace/memory/indexVectors` 和 `workspace/memory/semanticSearch`。两者分别写入由宿主生成的修订绑定向量、查询同工作区的派生索引。参数、批量修订检查和关键词回退见[语义索引](memory.md#重建与查询语义索引)。

`initialize.capabilities.workspaceMemory` 与 `workspaceMemoryAudit` 表示支持工作区范围的长期记忆和审计；`maxWorkspaceMemories` 是单工作区总记录上限（含 tombstone），`maxWorkspaceMemoryContentBytes` 是单条 UTF-8 正文上限。`key` 在同一工作区唯一，长度为 1..128 个 Unicode 字符；正文为 1..16384 字节且不能含 NUL。`workspace/memory/put` 不带 `expectedRevision` 时创建，已存在的键只有正文和来源完全相同时才作为幂等重试返回；实际更新或恢复必须提供当前 `expectedRevision`。来源任务必须属于该工作区且已完成。删除也必须提供当前 revision；删除会递增 revision 并设置 `deletedAtMs`，不会清除审计或内容，恢复通过 `put` 携带 tombstone 的 revision 完成。

`workspace/memory/get`、`list` 和 `search` 的 `actorTaskId`（若提供）必须属于该工作区，并作为访问者写入审计。查询结果和正文不会写入审计；写入类动作记录正文 SHA-256。列表与搜索会记录一次聚合访问，不为每条结果写一条审计；`listAudit` 本身不再递归产生日志。`search` 是区分大小写的字面子串匹配，query 为 1..256 个非控制字符；这是稳定的关键词路径，不承诺语义相似度。游标以记录创建顺序分页，软删除记录默认不出现在 list/search 中。SQLite v55 持久化记忆与审计；v56 为模型 Token 预检观测事件建立兼容边界。新任务可冻结有界关键词检索结果作为上下文；可重建的语义索引仍未实现。

`initialize.capabilities.sessionCreateIdempotency` 表示 `session/create` 接受 `clientRequestId`。键由 1 到 128 个可打印且不含空格的 ASCII 字符组成，在工作区内唯一。连接中断后用原键和原始标题重试会返回同一个会话的当前信息。复用键并更改原始标题会返回 `-32009`。创建记录和键同事务保存，重启后仍可重试。

`eventBySequence` 表示支持 `events/get`。`sequence` 取自实时通知或回放，范围为 1 到 SQLite 的最大 64 位有符号整数。序号不存在时返回 `-32004`。

客户端可通过 `initialize.capabilities.artifactSummaries` 探测轻量产物接口。清单项包含 `id`、`taskId`、`title`、`mediaType` 和原始字节数 `contentBytes`。首次请求使用 `after:0`，之后以同一任务的 `nextAfter` 翻页。每页最多 100 项。使用 `artifacts/get` 按任务和产物 ID 读取正文。`artifactSummaryById` 表示可以用同样的参数调用 `artifacts/getSummary` 单独读取元数据。

`initialize.capabilities.binaryArtifacts` 为真时，二进制产物的清单项包含 `binary:true`，完整响应以 `contentBase64` 返回标准 Base64。文本产物沿用 `content`，清单省略 `binary`。SQLite v31 为二进制正文标记版本；旧产物及游标无需迁移。

A2A 导入的二进制附件在 `agentRun.artifacts` 中通过 `sourceId` 和可选的 `partIndex` 对应到原消息或产物。`partIndex` 从 0 开始；省略时表示源容器本身。原件和附件拥有各自的本地产物 ID。

`scopedArtifactSummaries` 表示支持 `artifacts/listScopedSummaries`。每页最多 100 项，包含归档会话中的产物。从 `after:0` 开始，使用同一查询范围的 `nextAfter` 翻页。SQLite v26 为此清单新增索引并回填已有产物。任务级清单的游标独立于此接口。

`artifactMediaTypeFilter` 表示 `artifacts/listScopedSummaries` 支持 `mediaType`。筛选条件和已有产物的值完全匹配，包含大小写和参数。筛选词最多 256 个 UTF-8 字节，须包含可见内容。翻页时沿用相同筛选条件和作用域。SQLite v28 为会话和工作区分别建立类型索引，并保留原有游标。

`artifactCatalogSnapshots` 表示支持 `artifacts/listSnapshot`。接口使用相同的作用域、类型筛选及目录游标，额外返回全局目录水位 `throughSequence`。首次可省略该字段；后续页沿用返回的水位和作用域、筛选条件，把前一页的 `nextAfter` 传为 `after`。序号大于 `after` 且小于等于水位的产物才参与查询，包括归档会话中的产物。该水位重启后仍可使用，新的产物不会混入。`after` 大于水位，或水位超出当前目录，返回 `-32602`。如需读取快照之后新增的产物，可从旧水位续查并省略 `throughSequence`。目录水位独立于事件序号和任务产物游标。

客户端可通过 `artifactByteReads` 和 `maxArtifactReadBytes` 探测分块读取。从 `after:0` 开始，每次以返回的 `nextOffset` 续读。默认读取 32 KiB，每块最多 64 KiB。对 `dataBase64` 做标准 Base64 解码并拼接字节。若产物是文本，再按 UTF-8 解码，因为块边界可能切开一个字符。`nextOffset` 等于 `contentBytes` 时已经读完。处于末尾的请求返回空块，超过末尾的偏移返回 `-32602`，产物不属于指定任务时返回 `-32004`。同一产物的偏移在服务重启后仍可使用。SQLite v21 将已有正文移到独立 BLOB 表，保留产物 ID、清单游标和字节偏移。升级在一个事务内完成，迁移中数据库需要空间写入新正文。

#### 一次取得摘要和首块

服务声明 `artifactDownloadStart: true` 时，可调用 `artifacts/openDownload`。`digest` 与 `artifacts/digest` 的响应相同；`firstChunk` 等同于 `artifacts/read` 使用 `after:0`、`limit:65536` 的结果。若 `nextOffset` 小于 `contentBytes`，继续用普通分块读取；否则首块已包含整个正文。空产物返回空首块。旧产物缺少摘要时仍按需计算并保存，使用相同的 8 MiB 摘要预算和任务归属检查。该入口不产生任务事件，无需数据库迁移。

#### 批量下载首块

`initialize.capabilities.artifactDownloadStartBatch` 声明支持 `artifacts/openDownloads`。`maxArtifactDownloadBatch` 公布项数上限（16），`maxArtifactDownloadBatchBytes` 公布整批首块字节上限（256 KiB）。`limit` 是每项的原始字节数，默认 16 KiB，可设为 1 至 65536。`artifacts.length * limit` 需在整批上限内，重复项也计入预算。两个 ID 各须为 1 至 128 个可见 ASCII 字节。

整批在同一个 SQLite 读事务中打开，`items` 保留顺序和重复位置。`ready` 的 `download` 为 `{digest,firstChunk}`，首块从 0 开始，较小的交付物可在这一次请求中读完。`missing` 表示不存在或不属于指定任务。`unrecorded` 带 `contentBytes`，客户端可对需要下载的旧记录调用 `artifacts/openDownload`，按需计算并保存摘要。

Rust 调用方可用 `Client::open_artifact_downloads`，再把 `ready` 的结果与请求中的同一项传给 `Client::download_artifact_from_start`。客户端在写入任何正文前调用摘要回调，完成时校验 SHA-256。中断后可使用当前暂存长度和保存的摘要续传。

#### 获取交付物摘要

`artifacts/digest` 返回原始字节的 `{contentBytes,sha256}`。`initialize.capabilities.maxArtifactDigestBytes` 公布支持摘要的大小上限，目前为 8 MiB。若服务声明 `storedArtifactDigests:true`，新产物在提交正文时同时保存摘要，后续读取不再打开正文；没有保存摘要的旧产物在第一次查询时分块计算并保存。升级无需启动时扫描正文，不改变目录游标，也不会为补算摘要新增事件。调用形式和返回字段与旧服务相同。

下载方仍需对自己实际收到的字节计算 SHA-256 并比对。摘要描述提交时的内容（旧产物为首次计算时的内容）；它不表示服务刚刚校验了磁盘上的正文。

`initialize.capabilities.taskSummaryById` 表示客户端可以通过任务 ID 查询 `TaskSummary`。内容与 `task/listSummaries` 的单项一致，包括状态、后端、输入预览、待审批数量和结果是否未知。需要正文或历史时可使用 `task/get`。

### 任务提交与重试

提交前为这次操作生成 `clientRequestId`，并保存原始输入、交付物引用、后端、上下文选项和模型尝试上限。若等待 `task/startSummary` 响应时断线，在同一会话内原样重试。已接受的请求会返回同一任务的**当前**摘要，不再次调度模型或工具；已中断的任务仍须按原 `task/resume` 规则显式处理。复用同一键但改变输入、后端、规范化后的上下文或模型尝试上限会返回 `-32009`。如果只剩下会话和请求 ID，可以用 `task/getSummaryByRequest` 找回任务。

SQLite v44 将普通有键提交的输入 SHA-256、后端和规范化后的上下文选项随任务与首个事件提交。新任务的摘要重试只核对这个回执和当前摘要。旧任务仍通过原快照核对，升级不扫描历史正文。同一键空间中的审批续跑和远端续接继续按各自的规则识别。

#### 按请求键批查任务

握手声明 `taskSummaryByRequestBatch` 时，宿主可用 `task/getSummariesByRequest` 核对同一会话中多次提交的结果。一次传入 1 至 64 个 `clientRequestId`，上限也可从 `maxTaskSummaryByRequestBatch` 读取。`items` 与请求键逐项对应，重复键保留重复位置，当前会话中没有绑定的键返回 `null`。

```json
{"jsonrpc":"2.0","id":9,"method":"task/getSummariesByRequest","params":{"sessionId":"<session-id>","clientRequestIds":["draft-report","make-chart"]}}
```

响应中的 `throughSequence` 与摘要来自同一次读取。查询涵盖已有的任务请求绑定，包括旧版本没有紧凑回执的绑定。请求不会创建任务或触发恢复。会话不存在返回 `-32004`，键格式或数量不合法返回 `-32602`。

Rust 宿主可以调用 `Client::get_task_summaries_by_request`，并用返回水位接续事件读取。连到旧服务时，可逐项使用 `task/getSummaryByRequest`。

### 持久 Run DAG

`initialize.capabilities.durableTaskRuns` 为 true 时，`run/start` 可在同一会话提交 1..32 个模型子任务。`nodeId` 必须唯一且由 ASCII 字母、数字、`_` 或 `-` 组成；`dependsOn` 只能引用本次提交的节点，不能重复或形成环。`maxConcurrency` 默认为 2，范围 1..8；它限制同时处于模型执行中的子任务，等待用户审批或补答的节点不占执行槽。`maxModelSteps` 是每个子任务的模型尝试上限。上下文历史水位在 Run 创建时固定，节点依赖以外的新完成历史不会混入后续节点。

`clientRequestId` 在同一会话内幂等。相同键和相同计划返回已保存的 Run 并继续安全调度；相同键对应不同计划返回 `-32009`。每个节点创建独立 Task，沿用 Run 的会话/工作区、上下文选择、模型预算、已注册工具集合和审批策略；Run 保存 Provider/模型、模型预算形状、工具注册摘要及策略身份。重启后这些授权身份变化时，不会为等待中的节点创建新 Task。每个子任务使用自己的任务 ID 和审批 ID，授权不能跨节点复用。

只有 `dependsOn` 明确列出的直接依赖结果会传给节点。结果被封装为 JSON 数据并标记为不可信内容；超过单个 Task 输入上限时节点失败，不静默截断。依赖 Task 的产物仍通过原 `taskId` 访问，不复制或改绑到其他节点。失败、取消或跳过的依赖会跳过其下游节点，独立分支可以继续运行。所有节点结束后，全部成功为 `completed`，仅部分节点成功为 `partially_completed`；已完成的 Task 和产物会保留。`interrupted` 节点不会在重启后自动重放；先按 `task/getResumeStatus` 检查，再由宿主明确调用 `task/resume`。模型或外部结果未知的证据保留在原 Task 上。

`run/get` 会刷新已记录的子任务状态并推进可运行节点。子 Task、节点归属与 `node_started` 事件同事务保存；该事件表示 Task 已创建，模型是否已派发须查 Task 本身。`run/events` 返回持久事件；`nextAfter` 和 `throughSequence` 可用于断线续读，游标只用于回传，不应解释为连续编号。`run/cancel` 在一个事务中标记 Run 与其未完成的子任务，提交后再向执行器发出取消信号。主动取消的 Run 为 `cancelled`，仍保留原 Task 的完成、取消与未知结果记录。能力上限另见 `maxRunNodes` 和 `maxRunConcurrency`。

服务重新取得数据库写锁后，可以继续同一 Task 的首次派发，或从已提交的工具结果、上下文压缩边界发起下一次模型请求。内核同时检查持久任务状态、最后一条任务事件和 Run 的模型、工具及策略身份。未提交且没有独立执行凭证的尝试仍按未知结果处理，不会自动重发。

Windows 和 Linux 上，Run 子任务通过 `--exec-config` 注册的进程工具在逐次批准后可交给独立 supervisor。一次性执行凭证绑定 Task、会话、工作区、原调用与工具注册，和 `tool_started` 同事务提交。supervisor 领取凭证后才运行命令，持续检查租约与 Run 期限，完成后保存有界结果。重启时协调器重验原授权，只从同库读取原凭证的结果，不另启动命令。取消与任务状态同事务撤销凭证，迟到结果不能覆盖终态；supervisor 硬退出或租约过期后，无法确认的副作用记为 `tool_outcome_unknown`。Windows 使用现有 Job 清理进程树。Linux 无委托 cgroup 时，强杀 supervisor 后无法保证进程树回收。普通 Task、MCP 调用及模型请求不使用此进程交接路径。

`initialize.capabilities.maxRunTimeoutMs` 存在时，可以提交 `timeoutMs`（1 至 86,400,000 毫秒）。期限从首次接受计划开始，覆盖排队、等待审批、补答与执行；未指定则无 Run 级期限。返回的 `deadlineAtUnixMs` 为持久 UTC 时间戳，同键重试不会延长，改变超时会触发幂等冲突。服务会在后台收敛到期 Run，重启时也会先处理已到期的 Run。未结束节点标记为 `timed_out`；已创建的相应 Task 标记为 `cancelled`，原已完成的 Task 和产物仍可读取。如果所有节点已结束，仍按已提交结果汇总。超时事件记录原期限。执行器收到取消信号前可能已经向外部发出请求，原 Task 会保留可用的未知结果证据。

### 内嵌图片输入

`initialize.capabilities.taskImageInput` 表示本机 `task/start` 和 `task/startSummary` 的 `input` 可以使用有序数组。普通文本仍可直接传字符串。数组仅用于模型任务，由 `{ "type": "text", "text": "…" }` 和 `{ "type": "image", "mimeType": "image/png", "data": "…" }` 组成。图片数据使用标准 Base64，支持 PNG、JPEG 和 WebP。至少传一个图片或 PDF，共最多 4 个媒体、32 个片段，文本和 Base64 分别最多 256 KiB 和 512 KiB。

同一个请求键按片段顺序、文本、图片格式和数据判定是否重复。图片保留在任务的 `user_parts` 消息，普通任务摘要和新任务的历史上下文只使用文字及 `[image]` 占位符。使用配套的 OpenAI、Anthropic 或 Gemini 适配器时，宿主选择可接收图片的模型。按 JSON 字节数估算输入的本地请求预算也包含图片数据。AG-UI 客户端的输入封装见[提交内嵌图片](ag-ui.md#提交内嵌图片)。

### 内嵌 PDF 输入

声明 `initialize.capabilities.taskPdfInput` 的服务在同一个 `input` 数组中接受 `{ "type": "pdf", "data": "…" }`。`data` 是 PDF 文件的标准 Base64 编码。图片和 PDF 合计最多 4 个，编码总长最多 512 KiB，文字和片段上限沿用图片输入约定。重试保留片段种类和顺序。任务摘要和新任务的历史上下文用 `[document]` 表示 PDF。 [查看 AG-UI 接入](ag-ui.md#提交内嵌-pdf)

### 将交付物用作任务输入

声明 `initialize.capabilities.taskArtifactInput` 的服务允许在本机 `task/start` 和 `task/startSummary` 上设置 `artifactInputs`。每项包含源 `taskId` 和 `artifactId`。例如，使用同一会话里生成的报告作为下一项任务的输入时，可以提交以下参数。

```json
{
  "sessionId": "session_example",
  "input": "检查报告中的数据是否一致",
  "artifactInputs": [{"taskId":"task_source","artifactId":"artifact_report"}],
  "clientRequestId": "review-report"
}
```

可引用目标会话内已提交的 PNG、JPEG、WebP 和 PDF。如果服务也声明 `taskTextArtifactInput`，则可引用 UTF-8 文本交付物，例如 `artifact_create` 保存的报告。服务根据存储时的文本或二进制类型读取交付物，文本作为独立文本片段附在 `input` 后面，引用顺序保持不变。

每次最多提交 4 个引用，和 `input` 合计最多 32 个片段。文本和媒体各自累计，文本最多 256 KiB UTF-8，图片和 PDF 合计最多 4 个、512 KiB Base64。只传交付物时可将 `input` 设为空字符串，解析后须包含非空白文本或媒体。客户端可以通过 [交付物目录](#交付物目录订阅) 取得两个 ID。

任务创建时把已解码的文本或媒体内容写入任务快照。同一 `clientRequestId` 使用同一份输入和有序引用时，重试直接返回原任务。更换引用、改变顺序或改用内嵌字节会产生请求键冲突。不存在或不属于目标会话的交付物返回 `-32004`。格式或大小不符合约定时返回 `-32602`。[通过 AG-UI 引用交付物](ag-ui.md#引用已有交付物)

### 模型尝试次数

`initialize.capabilities.maxTaskModelSteps` 表示 `task/start` 和 `task/startSummary` 接受 `maxModelSteps`，取值为 `1..=32`。省略或显式填 `8` 都使用默认上限，重试时视为同一请求。外部 Agent 任务不接受该字段。显式指定非默认上限时，任务快照保留 `maxModelSteps`；已使用的次数见 `modelSteps`。

内核在开始尝试时记一次 `model_started`，随后再交给供应商。即使本次请求未发出或结果不明，这次尝试也占用上限。最后一次响应若提议了工具，获准的工具仍可执行并保存产物；若接下来还需要调用模型，任务以 `model step budget exceeded` 结束。次数用完且任务已中断时，`task/resume` 返回 `-32009`，不会写入续跑事件、清空 `modelOutcomeUnknown` 或再次发出请求。

SQLite v46 保存自定义上限，也能读取已有任务和两项式回执；迁移不扫描或重写历史任务。升级后的数据库应由支持 v46 的内核打开。尝试次数与[单次模型请求预算](context.md#模型请求预算)分别生效。

### 任务恢复判断

服务声明 `initialize.capabilities.taskResumeStatus` 时，可用 `task/getResumeStatus` 或 Rust 客户端的 `get_task_resume_status` 查询 `{taskId,status,disposition,remainingModelSteps}`。这条调用不写事件、不提交工作，也不授予重试同意。`remainingModelSteps` 是模型任务剩下的尝试次数，外部 Agent 任务返回 `null`。

| `disposition` | 客户端如何处理 |
| --- | --- |
| `ready` | 可以调用 `task/resume`。 |
| `retry_uncertain_model_step` | 仍有剩余尝试。向用户说明前一请求可能已经执行，再根据明确同意传 `retryUncertainModelStep:true`。 |
| `not_interrupted` | 任务未中断。按 `status` 展示运行、审批或终止状态。 |
| `model_step_budget_exhausted` | 次数已用完；即使显式同意也不能恢复。原结果未知的证据保留。 |
| `tool_outcome_unknown` | 先核对外部工具的实际结果；任务不允许重跑该工具。 |
| `agent_unavailable` | 宿主当前没有注册该 Agent。 |
| `agent_registration_changed` | 当前注册与任务绑定不符。恢复会拒绝；核对配置后可按需新建任务。 |
| `agent_already_started` / `remote_continuation` | 任务不允许通过 `task/resume` 重放；先核对已保存的 Agent 或远端执行证据。 |

SQLite v48 将恢复所需字段与任务和事件一起提交。升级只创建新表。旧任务按需从原快照提取字段，下一次正常状态变动后即可读取独立记录。结果只反映读取当时的任务和注册。`task/resume` 会加载完整任务并重新判断；期间的取消、状态变化或调度容量仍可能影响恢复。审批仍走 `approval/respond` 等审批入口。

### 批量查询恢复状态

服务通过 `initialize.capabilities.taskResumeStatusBatch` 声明支持，并在 `maxTaskResumeStatusBatch` 公布单次上限，当前为 64。Rust 宿主可调用 `get_task_resume_statuses`。请求接受 `taskIds`，以及可选的 `sessionId` 或 `workspaceId`，两种范围只能选一种。

响应包含 `items` 和 `throughSequence`。每项使用与单条查询相同的恢复状态。重复 ID 保留原位置，缺失或超出范围的任务填 `null`。事件游标和任务状态来自同一次 SQLite 读取，宿主可从该游标补读后续变动。实际恢复时，`task/resume` 会再次检查任务状态及重试同意。

### 会话目录

`initialize.capabilities.sessionListOrdering` 为真时，可对 `session/list` 传 `order:"newest_first"`，按创建顺序从最近的会话向前翻页。省略 `order` 或传 `"oldest_first"` 保持原先的正序。`limit` 为 1..100。

```json
{"jsonrpc":"2.0","id":4,"method":"session/list","params":{"workspaceId":"WORKSPACE","archived":false,"order":"newest_first","after":0,"limit":20}}
```

首轮传 `after:0`。若 `hasMore` 为真，下页传 `nextAfter`，工作区、排序和归档筛选保持一致。倒序游标是排他的；首轮之后新建的会话不会混入续页。每页读取的是当前标题和归档状态；重命名不会改变位置，归档筛选变动时可从 `after:0` 刷新。现有数据库和索引可直接使用。

Rust 宿主可以用 `fliuno_client::SessionCatalog` 读取并校验页。应用消费页面后保存 `next_request()`，重连或重启后把它传给 `SessionCatalog::new`。如果服务还没有 `sessionListOrdering`，可不传 `order`，沿用旧版正序。

`initialize.capabilities.sessionArchiving` 表示支持会话归档。`session/list` 省略 `archived` 时列出全部会话，传 `true` 或 `false` 可按状态筛选。会话对象省略 `archived` 字段时视为 `false`。`session/setArchived` 可以重试；它只更新显示状态，任务、历史和会话 ID 都可照常使用。`sessionArchiveBatch` 和 `maxSessionArchiveBatch` 表示支持最多 32 个 ID 的原子批量更新；无效 ID、重复 ID 或事务失败不会提交任何更新。翻页期间应沿用相同筛选条件。归档状态变动后，从 `after:0` 重拉列表即可反映当前状态。

### 任务目录

`task/listSummaries` 浏览一条会话的任务；声明 `workspaceTaskSummaries` 时，`task/listWorkspaceSummaries` 可跨会话浏览整个工作区，包括归档会话。两者均读取精简的 `TaskSummary`，每页最多 100 条。

| 查询选项 | 客户端能力 |
| --- | --- |
| 会话状态筛选 `status` | `taskSummaryStatusFilter` |
| 会话创建顺序 `order` | `sessionTaskSummaryOrdering` |
| 工作区创建顺序 `order` | `workspaceTaskSummaryOrdering` |
| 响应字节目标 `targetBytes` | `taskSummaryListTargetBytes`（最大值） |

```json
{"jsonrpc":"2.0","id":7,"method":"task/listSummaries","params":{"sessionId":"<session-id>","order":"newest_first","status":"waiting_approval","limit":20}}
```

始终从 `after:0` 开始，随后用 `nextAfter` 续页。默认 `oldest_first`，倒序 `newest_first` 只读上页游标之前创建的任务。续页须使用同一数据库、范围、状态筛选和顺序。重启不改变创建位置；摘要和筛选取每次查询时的当前状态。要看到首页之后创建的任务，或游标另一侧刚进入筛选的任务，重新拉取首页。

Rust 的 `TaskSummaryCatalog` 接受 `TaskSummaryCatalogRequest::Session` 或 `::Workspace`。每消费一页，可把 `next_request()` 序列化并保存；`None` 表示当前目录已到末尾。客户端校验游标方向、状态、重复 ID 和会话归属，收到无效页时保留原请求。正序请求会省略 `order`，便于兼容旧服务。

### 交付物目录订阅

`artifactCatalogSubscription:true` 表示可用 `artifacts/subscribe`。传入会话或工作区范围；`after` 省略为 0，也可填入同范围、同筛选条件下已经完整消费的目录水位。`limit` 默认 100，范围为 1..100。服务原子读取目录页、记录事件水位并注册同范围的水位通知；校验失败时保留原订阅。每条连接只有一个实时事件订阅；响应前收到的通知仍属于旧订阅。

```json
{"jsonrpc":"2.0","id":15,"method":"artifacts/subscribe","params":{"scope":{"kind":"workspace","workspaceId":"<workspace-id>"},"limit":20,"mediaType":"text/plain"}}
```

响应为 `{catalog,eventThroughSequence}`。`catalog` 是 [固定水位的元数据页](#审批事件与产物)。目录还有下一页时，以它的 `nextAfter` 和 `throughSequence` 继续调用 `artifacts/listSnapshot`。当前窗口读完后，用 `throughSequence` 作为下个目录窗口的 `after`，包括最后一页没有匹配条目的情况。`eventThroughSequence` 是事件流的起点，用于 `events/listSession`、`events/listWorkspace` 或 Rust `EventFeed`，不能拿来翻目录。

媒体类型只筛选目录页。收到匹配事件范围的 `artifact_created` 后，客户端可将其作为刷新目录的信号。Rust 的 `Client::subscribe_artifacts` 同时返回首页、`ArtifactCatalog` 和 `EventFeed`；把日常 RPC 的通知交给 `events.observe`。保存 `ArtifactCatalog::next_request()` 可在重启后继续读固定窗口，窗口读完后保存 `completed_through()`，同一进程内也可调用 `begin_next_window()` 读取新增产物。事件消费进度单独以 `EventFeed::cursor()` 保存。

### 批量读取交付物摘要

从目录拿到若干交付物后，可通过 `artifacts/getDigests` 一次查到用于下载校验的 SHA-256。`initialize.capabilities.artifactDigestBatch` 声明支持，`maxArtifactDigestBatch` 公布上限 64 项。每项的两个 ID 各须为 1 至 128 个可见 ASCII 字节。请求形状错误时整体拒绝。

```json
{"artifacts":[{"taskId":"task-id","artifactId":"artifact-id"}]}
```

| `items` 中的状态 | 含义 |
| --- | --- |
| `ready` | `digest` 包含该产物提交时保存的 `contentBytes` 和 `sha256`，或此前补算后保存的值。 |
| `unrecorded` | 只返回 `contentBytes`。若长度在支持范围内，可用 `artifacts/digest` 单独补算并保存。 |
| `missing` | ID 不存在或不属于所给任务。 |

响应逐项对应请求并保留重复项。不打开正文、不补算摘要、不新增事件，也不需要数据库迁移。Rust 调用方使用 `Client::artifact_digests`，可把 `ready` 的摘要直接交给 `download_artifact` 或 `resume_artifact_download` 校验收到的字节。旧服务可继续逐份使用 `artifacts/digest`。

### 批量取消任务

`initialize.capabilities.taskCancelBatch` 为真时，`task/cancelBatch` 可在一次调用中取消同一会话或工作区里选中的任务。`maxTaskCancelBatch` 公布批次上限，当前为 32。每个 ID 含 1–128 个可见 ASCII 字节，列表不能重复。范围必须显式给出：会话为 `{kind:"session",sessionId}`，工作区为 `{kind:"workspace",workspaceId}`。

```json
{"jsonrpc":"2.0","id":4,"method":"task/cancelBatch","params":{"scope":{"kind":"workspace","workspaceId":"<workspace-id>"},"taskIds":["<task-a>","<task-b>"]}}
```

服务在同一事务内校验每个 ID 的归属，并提交任务、审批收件箱和取消事件。若有任务缺失或不属于范围，整批返回 `-32004`；输入格式错误返回 `-32602`。事务失败不会留下部分取消，也不会向执行器发送停止信号。无需迁移数据库。

`items` 按请求顺序返回，每项的 `task` 为 `TaskSummary`；已完成、已失败或已取消的任务保留原状态，`changed:false`。本次新取消的任务返回 `changed:true`。响应丢失时可原样重试，已取消的任务不会重复写事件。`throughSequence` 是该事务看到的全局事件水位；若还要消费取消事件，可从调用前的游标回放到该水位。

单项与批次都先提交取消，再通知后台执行器。响应表示本地状态已提交，不能据此确认在途工具已退出、远端任务已停止或先前副作用被撤销。仍可通过持久事件观察后续结果。Rust 客户端提供 `Client::cancel_task_batch`。

### 批量读取任务摘要

`initialize.capabilities.taskSummaryBatch` 表示支持 `task/getSummaries`，`maxTaskSummaryBatch` 公布上限（目前 64 项）。宿主可以收集一页事件涉及的任务 ID，再一次查询这些任务的当前摘要。该调用只读摘要和作用域索引，不加载完整任务、对话或工具参数。

```json
{"jsonrpc":"2.0","id":9,"method":"task/getSummaries","params":{"workspaceId":"<workspace-id>","taskIds":["<task-id>","<another-task-id>"]}}
```

传 `workspaceId` 或 `sessionId` 限定范围，均省略时按整个数据库查询。工作区范围包含已归档会话。`taskIds` 不能为空，每个 ID 必须为 1 到 128 个不含空格的可打印 ASCII 字符。两个范围同时传入或参数不合法时返回 `-32602`，范围不存在时返回 `-32004`。

响应为 `{items,throughSequence}`。`items` 与 `taskIds` 一一对应，重复 ID 保留重复位置。不存在和不在请求范围内的任务均为 `null`。`throughSequence` 是与这些摘要同一快照的全局事件水位，可能比触发读取的事件更新。继续回放时沿用事件订阅自己的游标；批量查询的水位不代表其余任务的事件已经处理。

Rust 的 `Client::get_task_summaries` 返回类型化结果，校验项数、ID 和会话范围。与工作台实时订阅搭配时，仍把查询期间收到的通知传给 `EventFeed::observe()`。

### 工作台快照

服务声明 `workSnapshot` 后，可以用 `work/getSnapshot` 在一次读取中得到指定范围的状态计数和待审批首页。传 `workspaceId` 或 `sessionId` 限定范围，两个都省略时查询整个数据库；工作区包含已归档会话。不存在的范围返回 `-32004`，同时传两个 ID 返回 `-32602`。

```json
{"jsonrpc":"2.0","id":8,"method":"work/getSnapshot","params":{"workspaceId":"<workspace-id>","approvalLimit":20,"recentTaskLimit":10}}
```

结果的 `taskCounts` 始终包含 `running`、`waitingApproval`、`interrupted`、`completed`、`cancelled` 和 `failed`。`waitingApproval` 是任务数：一个任务可能同时有多项审批，审批首页也会排除已到期项。`approvals` 是[待审批快照](#待审批快照)，其 `snapshot.throughSequence` 也是计数的事件水位。审批首页的条数和字节限制与原接口相同。

当 `initialize.capabilities.workRecentTasks` 为 `true` 时，两种快照请求都可传 `recentTaskLimit`（`1` 到 `50`）。`recentTasks` 按任务创建顺序从新到旧返回 `TaskSummary`，和计数、审批首页使用相同范围及事件水位。请求了预览但没有任务时返回 `[]`；省略参数时，服务不查询摘要，响应也省略 `recentTasks`。预览用于首屏；浏览更多任务可使用 [任务目录](#任务目录)。

Rust 的 `Client::work_snapshot` 返回 `(WorkSnapshot, ApprovalInbox)`。首页已经在 `WorkSnapshot.approvals` 中，`ApprovalInbox` 从下一页开始；也可以从 `throughSequence` 接上同范围的事件。需要刷新首屏时重新调用 `work/getSnapshot`，旧审批游标仍可继续读取原来的列表。Rust 客户端会向不声明 `workRecentTasks` 的服务省略该参数，此时 `recent_tasks` 为 `None`。

SQLite v42 从已有状态索引回填计数。平时只有创建任务或状态变化才改动对应范围的计数，读取时不扫描任务列表。

#### 批量读取会话任务计数

服务声明 `workSessionCounts:true` 后，可用 `work/getSessionCounts` 为工作台会话列表读取状态计数。一次传入同一 `workspaceId` 下的 1～64 个会话 ID。`maxWorkSessionCountBatch` 公布最大项数，所有 ID 须为 1～128 个可见 ASCII 字符。

```json
{"jsonrpc":"2.0","id":9,"method":"work/getSessionCounts","params":{"workspaceId":"<workspace-id>","sessionIds":["<session-id>"]}}
```

`items` 与请求一一对应，保留顺序和重复位置。匹配的项返回 `{sessionId,taskCounts}`，其中 `taskCounts` 与工作台快照一致。空会话返回全零计数，已归档会话仍可读取。不存在或归属其他工作区的会话返回 `null`，不存在的工作区返回 `-32004`。

`throughSequence` 是计数读事务的全局事件水位，可用于判断这些计数的新旧。需要补读事件时，继续使用已处理的事件游标。Rust 调用方可用 `Client::work_session_counts`。

### 工作台实时订阅

当 `initialize.capabilities.workSubscription` 为 `true` 时，`work/subscribe` 使用相同参数，返回相同的 `WorkSnapshot`，并一次建立同范围的事件订阅。内核在同一提交锁下读取快照、挂接订阅。快照水位以内的状态已经计入首屏；后续只发出序号大于该水位的 `notifications/eventWatermark`，由客户端按相同范围补读正文。多次提交可能合并为一次水位通知。参数或范围无效时，当前订阅保持不变。

响应之前收到的持久通知仍属于旧订阅；通过响应拿到新水位后，再处理新订阅的通知。若使用批请求，同批只发一次切换订阅的调用。之后可通过 `events/setScope` 显式切换范围。遇到 `notifications/eventGap`，从最后已处理的事件游标重新建立同范围订阅并补读。

Rust 的 `Client::subscribe_work` 返回 `WorkSubscription { snapshot, inbox, events }`。先展示 `snapshot`，用 `inbox.next_page()` 续读原审批列表，把后续 RPC 和 `read_notification()` 收到的通知传给 `events.observe()`。`events.next_page()` 从快照水位之后分页；收到缺口时会重新订阅同一范围。界面如果只需要刷新计数，仍可调用 `work/getSnapshot`，该方法不切换现有订阅。

#### 任务变动回放

声明 `taskChangeReplay:true` 后，可以用 `events/listTaskChanges` 补读变动的任务 ID，再用 `task/getSummaries` 刷新摘要。该接口不返回审批参数、工具输出或模型文本。它包括该范围内有任何持久事件的任务，摘要本身可能并未改变。只需要已知的任务 ID 和最新摘要时，适合用这个接口替代完整事件回放。

```json
{"jsonrpc":"2.0","id":10,"method":"events/listTaskChanges","params":{"workspaceId":"<workspace-id>","after":120,"limit":64}}
```

`taskId`、`sessionId`、`workspaceId` 最多选一个，均省略时查询全库。`after` 是不含式起点，省略时为 0；`through` 是含式上界，省略时固定到当前全局水位。游标倒置或超出当前水位返回 `-32602`，不存在的范围返回 `-32004`。`limit` 是去重前扫描的匹配事件数，默认 100，范围 1..1000。设置为 64 时，每页可直接交给一次批量任务摘要请求。

响应的 `changes` 只包含 `{taskId,sequence}`，每个任务保留其在**本页**的最后一个事件序号，按序号递增；同一任务可能在下一页再次出现。`hasMore` 为真时，沿用 `throughSequence` 作为 `through`，将 `nextAfter` 用作下一页 `after`。尾页的 `nextAfter` 直接推进到 `throughSequence`，即使该范围没有匹配项。更新摘要后保存已消费的回放游标；批量摘要返回的较新水位不会跳过其他任务的更新。

Rust 的 `EventReplay::next_task_changes` 可从保存的窗口继续。`work/subscribe` 返回的 `EventFeed` 也可调用 `next_task_changes`，共用水位跟踪和缺口恢复。它与完整事件的 `next_page` 共用同一个游标；若两种视图都需完整处理，应使用独立的回放游标。

#### 任务变动摘要

`taskSummaryChangeReplay:true` 表示可用 `events/listTaskSummaryChanges` 一次获取变动任务及其当前摘要。与 `events/listTaskChanges` 使用相同的范围、窗口和页内去重规则；`limit` 限制扫描的匹配事件数，默认 64，范围 1..64，上限也可从 `maxTaskSummaryChangeEvents` 读取。没有匹配事件时，`changes` 为空且尾页游标仍推进。

```json
{"jsonrpc":"2.0","id":11,"method":"events/listTaskSummaryChanges","params":{"workspaceId":"<workspace-id>","after":120,"through":200,"limit":32}}
```

每个 `changes` 项含 `{sequence,summary}`；`sequence` 是该任务**本页**最后一次事件序号，`summary` 是当前的 `TaskSummary`，同一任务可以在后续页再次出现。SQLite 在同一读快照内选出变动任务并读取摘要投影，不返回事件正文。固定历史窗口只限制哪些任务入选；例如取消发生在 `through` 之后，选入的任务仍可返回 `cancelled`。

| 字段 | 用法 |
| --- | --- |
| `throughSequence` | 事件窗口上界；有续页时作下一次请求的 `through`。 |
| `nextAfter` | 更新本页任务后保存的消费游标；有续页时作下一次请求的 `after`。 |
| `summaryThroughSequence` | 同一读快照的全局事件水位，至少等于 `throughSequence`，固定历史窗口内可能更大。用来判断摘要新旧，不能用于推进回放。 |

Rust 的 `EventReplay::next_task_summary_changes` 与 `EventFeed::next_task_summary_changes` 会验证窗口、序号与重复任务，再推进消费游标。与读取完整事件或纯任务 ID 共用同一游标；需要分别消费时使用独立回放。老服务可用 [任务变动回放](#任务变动回放) 后接 [批量读取摘要](#批量读取任务摘要)。

#### 任务摘要修订回放

服务声明 `taskSummaryRevisionReplay:true` 时，工作台可以调用 `events/listTaskSummaryRevisions`，只刷新摘要实际变化的任务。请求、响应、范围和游标与 [任务变动摘要](#任务变动摘要) 相同。每页默认 64 条，接受 1..64。

| 回放区间 | `limit` 统计的记录 |
| --- | --- |
| **升级到 SQLite v45 前** | 匹配范围的任务事件。该前缀保守地将任务事件视为一次刷新提示。 |
| **升级后** | 已提交的摘要修订。每个修改摘要的任务事务产生一条索引记录，序号取该任务在此次提交中的最后一条事件。仅追加工具明细等事件、摘要未变时，不占用修订页预算。 |

迁移保存当时的全局事件水位，不重扫历史。旧游标可以继续使用；跨越升级边界的一页共用同一预算，并按任务去重。`changes[].sequence` 是该任务在本页最后一条命中记录的序号，`summary` 仍是查询时的当前摘要。即使尾页没有修订，也保存 `nextAfter` 继续；`summaryThroughSequence` 仅用于判断摘要新旧。

Rust 的 `EventReplay::next_task_summary_revisions` 和 `EventFeed::next_task_summary_revisions` 复用原有游标校验和缺口恢复。实时通知仍提供通用事件水位，可能推进到没有摘要变化的位置。旧服务可以继续使用 `events/listTaskSummaryChanges`，其按事件刷新语义保持不变。

#### 按窗口合并摘要修订

服务声明 `latestTaskSummaryRevisionReplay` 时，重连的工作台可用 `events/listLatestTaskSummaryRevisions`。请求和响应沿用摘要修订回放的结构。首次读取时省略 `through` 可捕获当前事件水位。翻页时保留首次返回的 `throughSequence`，把上页的 `nextAfter` 作为新的 `after`。

在整个固定窗口中，每个任务只出现在最后一条命中修订的位置。窗口跨越 SQLite v45 的升级边界时，旧段仍按任务事件判断变动。`limit` 限制每页返回的任务数，默认为 64，范围为 1 至 64。服务每页最多检查 1,000 条记录，因此 `changes` 为空且 `hasMore` 为真时仍需保存 `nextAfter` 并继续。摘要可能包含窗口结束后的变动。保存 `nextAfter` 作为回放进度，使用 `summaryThroughSequence` 判断摘要新旧。

Rust 的 `EventReplay::next_latest_task_summary_revisions` 和 `EventFeed::next_latest_task_summary_revisions` 共享原事件游标。它们会返回带有续页游标的空页。连接旧服务时可按服务能力选择原摘要修订回放。

### 审批收件箱与决定

`initialize.capabilities.pendingApprovalInbox` 表示客户端可以读取待审批列表。省略两个范围参数时列出整个数据库的待审批项，传 `workspaceId` 或 `sessionId` 可缩小范围，两者只能选一个。每页最多 100 项，按加入列表的顺序返回。可沿用同一范围的 `nextAfter` 翻页或在重连后读取新项。从 `after:0` 读取可以重新核对整份当前列表。

每项包含 `id`、`taskId`、`sessionId`、`kind`、`allowOnceAvailable` 和 `expiresAtUnixMs`。`kind` 为 `tool_call`、`agent_permission` 或 `host_tool`。工具审批没有到期时间。若 `initialize.capabilities.pendingApprovalDetails` 为 `true`，客户端可通过 `approval/get` 查看单项原请求，再用 `approval/respond` 提交决定。已解决、过期或属于其他任务的项返回 `-32004`。提交时仍以当时的任务状态、审批期限、工具注册和策略为准。已撤销、解决或过期的审批会从列表移除。服务重启后保留模型工具审批，已丢失的 Agent 运行结束时清除相应审批。

#### 工具输入续接

`initialize.capabilities.taskRespondToolInput` 表示支持 `task/respondToolInput`。任务因工具返回可补答请求而进入 `waiting_input`；宿主展示 `Task.pendingToolInput` 后，使用原 `taskId`、`callId`、新的幂等 `requestId` 和响应对象提交。内核重新核对已批准工具的注册身份、参数和当前策略，只将原参数与响应交回同一工具的续接入口，不再调用模型，也不重复审批。请求状态和响应摘要保存在内核私有 SQLite 表，不出现在 Task、Event 或 RPC 响应中。每轮请求和响应最多 256 KiB，最多 10 轮；任务可取消，重启后待输入状态可继续处理。

```json
{"jsonrpc":"2.0","id":10,"method":"task/respondToolInput","params":{"taskId":"<task-id>","callId":"<tool-call-id>","requestId":"<response-id>","responses":{"<request-id>":{"action":"accept","content":{"code":"1234"}}}}}
```

`initialize.capabilities.pendingApprovalListTargetBytes` 给出审批列表的最大字节目标。`targetBytes` 接受 1024～8388608，按紧凑 JSON 数组 UTF-8 字节分页；首次遇到的单个大条目仍完整返回，`nextAfter` 可在同一 workspace 或 session 范围内续读。

#### 待审批快照

`pendingApprovalInboxSnapshot` 声明 `approval/listPendingSnapshot`。首次传 `page`，省略 `snapshot` 和 `page.after`。结果中的 `snapshot` 包含全局事件水位 `throughSequence` 和到期判断时间 `capturedAtUnixMs`，`page` 沿用待审批列表的 `items`、`nextAfter` 和 `hasMore`。

```json
{"jsonrpc":"2.0","id":8,"method":"approval/listPendingSnapshot","params":{"page":{"workspaceId":"<workspace-id>","limit":20}}}
```

续页保留原范围，带回原 `snapshot`，并将 `page.nextAfter` 作为 `page.after`。该游标是收件箱位置，与全局事件水位不同。翻页期间，新审批不进入快照；在原水位待处理的审批即使随后关闭或到期，仍按原水位和原时间读取。Rust 调用方可以用 `ApprovalInbox::new(params)` 逐页读取，并将 `next_request()` 与已消费的页一起保存。

若要接上实时更新，可在读完快照后用 `events/setScope` 建立相同范围的订阅，从快照的 `throughSequence` 补读至订阅响应中的水位，再处理实时事件。快照只用于展示与对账；`approval/get` 和 `approval/respond` 始终校验当前状态和期限。

SQLite v41 在升级时记录可读取的最早水位，并保留待审批摘要的打开和关闭边界，不复制工具参数。早于该水位、超过当前水位或续页时缺少快照会返回 `-32602`。原 `approval/listPending` 继续返回实时列表。SQLite v52 增加按审批打开水位查找的索引，让历史页跳过快照之后打开的审批。

#### 决定与运行

`initialize.capabilities.approvalDecisionLookup` 表示支持 `approval/getDecision`。SQLite v34 从历史事件回填任务 ID、审批 ID 和事件序号。模型工具审批返回 `approval_resolved`，ACP 权限和宿主工具审批返回 `agent_approval_resolved`。事件中的 `decision` 记录已提交的用户决定。实际执行、投递和结果仍查各自的任务与事件。尚无已提交决定的审批、仅关闭的审批或任务与审批 ID 不匹配时返回 `-32004`。

审批不提供“允许全部”模式。未来可新增策略接口，但不得把模型或 MCP 声明的工具注解当作授权来源。

`initialize.capabilities.approvalRespondDecision` 表示支持 `approval/respondDecision`。第一次调用沿用原审批和调度路径。对同一任务与审批 ID 重试相同决定时，直接返回原事件。若已有相反决定，则返回 `-32009`。该重试规则也适用于通过 `approval/respond` 提交过的决定。

`initialize.capabilities.approvalResumeRun` 表示支持 `approval/resumeRun`、`approval/getRun` 和 `approval/cancelRun`。续跑限于同一会话中模型任务执行段的第一项待审批工具调用。子请求键与审批决定一起提交到 SQLite；`ApprovalRun.startSequence` 为该决定的全局事件序号，初始运行则为 `null`。同一子请求键的相同答复返回原绑定和当前任务摘要；更换父请求键、审批或决定会冲突。`getRun` 可以补读绑定，避免把任务最新状态误当成历史执行段的状态。[AG-UI 审批中断](ag-ui.md#审批中断与续跑)

续跑会先用 `pending_approvals` 的当前身份、过期时间和 `task_run_ends` 的首个边界事件核对审批，再读取任务快照和工具注册。若审批已被普通审批接口处理、已过期，或属于父运行之后的执行段，返回冲突或未找到，不会新增事件。

`initialize.capabilities.approvalRunWindow` 表示可通过 `approval/getRunWindow` 读取执行段的边界。`firstSequence` 对初始运行指向 `task_started`，对子运行指向绑定它的 `approval_resolved`。`lastSequence` 包含首个审批请求、任务中断或任务终态事件；还没结束时为 `null`。这对序号用于定位事件，不能替代对具体帧的校验。SQLite v39 从已有事件回填执行段的结束索引。

`initialize.capabilities.approvalRunReplay` 表示可用 `approval/getRunReplay` 一次取得 `ApprovalRun` 和 `ApprovalRunWindow`。同一 SQLite 语句还返回可传给 `events/list.through` 的 `throughSequence`：执行段已结束时等于 `window.lastSequence`，否则是该任务最新提交的事件序号。`run.task` 仍是当前任务摘要，可反映后续执行段的状态；回放范围由 `window` 和水位决定。调用不写入状态，也不改变订阅。旧 RPC 保持原响应形状，不需要数据库迁移。

`initialize.capabilities.approvalRunPending` 表示可以用 `approval/getRunPending` 按运行读取待审批工具。只有在该审批是运行的结束事件且尚未处理时才返回详情；同一任务后续执行段的审批不会通过旧请求键返回。

#### 批量读取待审批详情

服务声明 `pendingApprovalDetailsBatch:true` 后，可以用 `approval/getBatch` 从同一读事务和同一个到期判断时间读取 1～32 项详情。每个 `taskId` 或 `approvalId` 接受 1～128 个可见 ASCII 字符。`maxApprovalDetailsBatch` 和 `maxApprovalDetailsBatchBytes` 公布项数和字节上限。

```json
{"jsonrpc":"2.0","id":20,"method":"approval/getBatch","params":{"approvals":[{"taskId":"<task-id>","approvalId":"<approval-id>"}]}}
```

`items` 与请求逐项对应，保留输入顺序和重复位置。仍待处理的项与 `approval/get` 相同，返回 `{kind,approval}`。找不到、已过期、已关闭或归属任务不匹配的项返回 `null`。整个 `items` 数组的紧凑 JSON 上限是 256 KiB，包括括号、逗号和每个重复项。超限时返回 `-32602`，客户端可以拆小批次，用 `approval/get` 读取单项超大的详情。

Rust 调用方可用 `Client::get_pending_approval_details`。用户做出决定后仍调用 `approval/respond`，由服务按提交时的状态和期限校验。

#### 批量读取审批决定

服务声明 `approvalDecisionBatch:true` 时，可用 `approval/getDecisions` 一次核对 1～64 对任务和审批 ID，客户端重连后无需逐项请求。`maxApprovalDecisionBatch` 提供服务端上限。每个 ID 接受 1～128 个可见 ASCII 字符，任一键无效会拒绝整个请求。

```json
{"jsonrpc":"2.0","id":21,"method":"approval/getDecisions","params":{"approvals":[{"taskId":"<task-id>","approvalId":"<approval-id>"}]}}
```

`items` 与请求一一对应，保留顺序和重复项。回执有 `taskId`、`approvalId`、`decision` 与全局事件 `sequence`；Agent 的审批回执另有 `runId`，表示本地已保存决定，无法证明对端已收到。仍待审批、关闭时未留下决定、键不匹配或从未存在的项均返回 `null`。没有回执不授予任何权限，也不能据此判断某项工作是否已执行。

响应的 `throughSequence` 来自同一 SQLite 读快照，只表示查询水位。读取不产生事件，不应据此推进已消费的事件游标。Rust 使用 `Client::get_approval_decisions`；旧服务可以逐项调用 `approval/getDecision`。需要完整的持久事件时，用返回的 `sequence` 调用 `events/get`。

### 主机注册的命令执行

`--exec-config PATH` 可选加载最多 1 MiB 的主机配置。只允许主机选择程序、固定 argv、allowlisted 环境变量、隔离类型、资源上限与默认 timeout/output budget；RPC 和模型参数不能传 executable、cwd、env、isolation 或资源预算。配置在启动时整体校验再注册，不允许运行期增删 authority。

```json
{
  "programs": [{
    "id": "tests",
    "command": "C:/absolute/path/to/runner.exe",
    "isolation": "windows_app_container",
    "resources": {"memoryMaxBytes": 536870912, "pidsMax": 16},
    "args": [],
    "envAllow": [],
    "timeoutMs": 30000,
    "outputBytes": 32768
  }]
}
```

`isolation` 必填为 `trusted_host`、`windows_app_container` 或 `linux_landlock`。前者使用 task workspace 为 cwd、显式空环境和当前操作系统用户权限；Windows Job / Unix process group 用于限制命令生命周期，但 trusted mode 不限制文件或网络访问。Windows AppContainer 与 Linux Landlock/seccomp 分别仅在对应平台可用，都会暂存单个程序并使用私有 cwd，不获得 workspace 或网络访问；Linux 还要求 Landlock ABI 6，并在当前 cgroup v2 可写委派时启用整树回收。`resources.memoryMaxBytes`、`resources.pidsMax` 和 Linux `resources.cpuQuotaMicros` 是主机配置的硬上限；Linux 可用 `linuxCgroupParent` 指向启用了相应控制器的委派 domain cgroup，控制器或权限缺失时拒绝启动。Windows 内存限制使用 Job commit 记账语义且不支持 CPU quota。隔离配置的 `assets` 可列举固定 SHA-256 的伴随文件，模型不能改变资源清单；workspace 代理尚未开放，见[伴随文件](runtime-bundles.md)。

初始化能力 `processExecution` 表示存在注册命令。`tools/list` 只列出不含主机路径、固定参数或环境值的 `process_execution` 工具元数据。每次模型调用走现有 `ApprovalRequested` / `approval/respond`；批准绑定不可变工具名及参数。工具名包含 executable/config/解析环境策略的摘要，换配置后旧待批调用不能重新绑定新命令。执行前再次核对可执行文件内容摘要；不接受 PATH 查找或隐式 shell。

宿主可为已发现的 [离线 Skill](skills.md#执行离线文本-skill) 增加 `programs[].skill` 注册。它沿用 `process_execution` 审批；工具只接受有界 `stdin`。审批的可选 `executionPlan` 由宿主生成并持久保存，完整 JSON 最多 8 KiB。客户端通过 `approval/get` 读取，批准时无须回传。成功且完整的文本输出作为任务产物交付。

模型可提供额外 argv（最多 256 项、与固定前缀合计最多 32,000 UTF-8 字节）、UTF-8 stdin（最多 256 KiB）、较小的 timeout（配置 100..300000 ms）、较小的输出上限（配置 1024..65536 bytes/stream）。输入交付后关闭 stdin 写端，让等待 EOF 的程序继续运行；停止时中止未完成输入。stdout/stderr 并发排空，每条仅保留末尾有界原始字节；有效 UTF-8 返回文本，否则返回 base64，并报告总见字节、保留量和截断标记。非零退出是已知结果，报告 exitCode 和 success=false。

超时、取消、输出读取/退出观察失败或清理结果不完整会记录 `tool_outcome_unknown` 及有界部分报告，任务进入 interrupted 并保留 `inFlightTool`，不能通过 task/resume 重跑。取消先提交而 worker 后完成时，任务仍为 cancelled，但匹配的部分结果作为只追加审计事件持久保存。`stdinComplete` 要求全部写入及异步 shutdown/flush 成功，且本地写端已释放；不表示子进程实际读取。stdin 失败但退出和输出完整时，仍可返回已知结果，`success=false`，并保留失败阶段。一次审批授权整条注册程序运行；`trusted_host` 具有配置宿主的完整 OS 权限，因此不能把它当作低权限沙箱。

进程报告的 `evidence` 包含原生启动耗时、首次停止原因、停止请求结果、根进程回收、各管道关闭和有界 IO 错误。总 deadline 从原生启动之前开始；若同步启动返回时预算已耗尽或已经取消，先释放尚未轮询的 stdin 再清理，不继续写入。清理另有最多 2 秒的排空预算，因此 `elapsedMs` 可以超过 `timeoutMs`；同步原生启动本身不能由此定时器抢占。字段定义与真实探针见 [执行证据](execution-evidence.md)。

`process_execution` 是前台有界命令工具；模型不会经此工具获得跨轮次 terminal ID。独立 PTY 生命周期、主机配置和存储契约见下节。MCP/ACP Agent 没有拿到通用 shell 或终端能力；macOS OS sandbox 与用户可调文件/网络 profile 仍待建设；Linux 有委派 cgroup 时提供独立 guardian，无委派时仍只有进程组的尽力清理。

### 跨轮次终端

Rust 宿主可用 `Client::subscribe_terminal` 按绝对字节游标读取输出。读取末尾、校验通知和断线续读见 [终端订阅 · Rust 客户端](terminal-subscriptions.md#rust-客户端)。

输出订阅的完整通知、恢复与流控契约见 [终端输出订阅](terminal-subscriptions.md)。`terminalOutputSubscriptions` 为 true 时可使用；单连接最多 `maxTerminalSubscriptions`（当前 8）个订阅。订阅建立前检查 session 归属和输出游标，初始回放与后续实时输出共用字节游标。`terminalOutputTail` 为 true 时可用 `terminal/tail` 获取持久末尾并以 `next` 开始订阅。

`--terminal-config PATH` 可选注册 1..16 个主机 preset；配置最多 1 MiB。`stateDirectory` 和每个 `command` 都必须是绝对路径，命令必须为原生可执行文件；Windows 仅接受 `.exe`。未知配置字段被拒绝。程序摘要、固定参数、allowlist 解析出的环境值、生命周期、spool 预算、资源预算、Linux cgroup parent 及状态根共同决定 preset identity；可执行文件在审批执行前和 supervisor 启动时再次核验。每个 preset 提供一个含 identity 摘要的创建工具，另注册 list/status/output/write/resize/kill/release 控制工具。变更设置会改变工具身份，之前未完成的批准不会变成对新设置的授权。

配置了任意 preset 时，`initialize.capabilities.terminalSessions` 为 true；`externalTools` 也包含这些有副作用的 terminal tools。没有配置时，终端工具不注册，terminal RPC 返回未配置错误。只读状态/output RPC 的实时监督器观察最多等待 150 ms，超时后回到已持久化快照并标为离线状态；`terminal/list` 用共享的 150 ms 预算刷新未知记录。

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

Preset 可选 `resources` 与 `linuxCgroupParent`。`resources.memoryMaxBytes`、`resources.pidsMax`、Linux 上的 `resources.cpuQuotaMicros` 固定为 worker 与其 PTY 子孙设置内核预算；Windows 的资源字段覆盖默认 32 个进程和 1 GiB Job memory，Linux 预算要求可写 delegated cgroup v2。`linuxCgroupParent` 指向宿主预先委派的 cgroup v2 domain 目录；缺省时 Linux 尝试使用当前进程 cgroup 的委派。独立 guardian 在 exec 前通过 pidfd 交接绑定 worker；worker 退出后清理剩余 cgroup 进程，包括调用 `setsid` 的 PTY 后代。预算所需控制器缺失或不可写时拒绝启动；无预算且无 cgroup 委派时退回进程组的尽力清理。app-server 退出不影响已交接的终端；即使 app-server 已被硬杀，guardian 仍跟随 worker 并在其退出时清理。guardian 自身启动或交接失败则拒绝 exec。

每个持久任务的创建工具在普通 `approval/respond` 之后运行；一个创建批准启动一个 PTY。创建 cwd 固定为所属 task 的 workspace root。模型只能增加 argv（最多 256 项、总计 32,000 UTF-8 字节）、设置 1..500 的 rows/cols；不能改 executable、cwd、环境、寿命、输出预算或 isolation。支持的 isolation 当前只有明确声明的 `trusted_host`；其子程序继承当前 OS 用户权限，能读取允许该用户读取的文件、访问网络和继承 allowlist 值。Windows supervisor Job 默认限制每个 PTY 32 个活动进程和 1 GiB Job memory，可由 preset `resources` 覆盖；Linux 在可写 delegated cgroup v2 中应用资源预算。macOS process group 仅提供尽力清理，不等价于 Windows Job 或 OS sandbox。ACP 不因此启用 `terminal/*` capability。

模型每次 terminal 工具调用都要单独审批，包括 list/status/output 和非空 UTF-8 write（最多 16 KiB）、resize、kill、release、archive。应用协议的只读 list/get/output 直接读取已存储记录且不创建任务或审批。写入 operation ID 在 native pipe IO 前持久预留；重试相同 ID 且请求相同只取回缓存结果，不同请求或 crash 后没有结果均拒绝重放。输入累计最多 1 MiB；最多 1,024 项普通 mutation reservation，kill/release 另保留两项容量。每个服务最多有 8 个活动或未知的记录和 64 个物理创建记录；显式归档可回收已停止 supervisor 的 spool，并通过独立 tombstone 账本永久保留创建身份。受信客户端另可通过 `terminal/archiveBatch` 明确选择最多 32 个 ID，能力标记为 `terminalBatchArchiving`；每项独立提交，结果按输入顺序返回。归档审批、延迟 worker 拒绝启动、待清理恢复和分页/批量契约见 [终端归档](terminal-archive.md)。

每个终端由独立于 app-server 的 supervisor OS 进程拥有。主机原子发布完整的创建预留记录后才启动 supervisor；PTY child 尝试前的明确错误能安全失败，进入不确定阶段的错误会保留 terminal ID 和审计结果。Windows Job 在子命令创建前挂到 supervisor，杀掉或异常结束 supervisor 时 OS 回收 PTY 进程树；重启 app-server 后可通过私有 named-pipe IPC 查询状态或在同一 session 创建新的获批操作，不会 relaunch 旧进程。Linux 有可写委派 cgroup v2 时，独立 guardian 在 worker 退出后回收包含 setsid 后代的进程树并应用 preset 资源预算；它在 app-server 退出后仍持续监控 worker。Linux 强制结束与重连探针已在 Debian 6.12 来宾通过。无 cgroup 委派且未配置预算时，Linux 使用进程组的尽力清理；macOS 目前仅有进程组行为。IPC 是版本 1 的本地长度前缀 JSON frame，每帧最多 128 KiB，有随机 capability token 和 session ID 检查；token 与 endpoint 保存在受限 state directory，PID 只是诊断信息而不是信任依据。

PTY 输出按字节追加到 SQLite，游标是单调绝对 byte offset；默认每条最多保留 1 MiB，允许 64 KiB..8 MiB。读取每次最多 32 KiB，`dataBase64` 保留 ANSI 控制码、非法 UTF-8 和被截断的 UTF-8 边界。`from`、`next` 指示本页区间，`gap:true` 表示请求游标之前的输出已经按容量删除；客户端应以 `next` 继续读取。20 毫秒生命周期 actor 和有界 native IO 队列让读写不阻塞控制通道，洪泛时仍受 spool 和 channel 容量约束。最多 16 个 IPC 请求并发处理。达到绝对游标上限、spool 磁盘故障或 supervisor 不可达时会显式返回错误/未知状态，不自动恢复或重放副作用。

`terminal/list` 遍历托管根中最多 128 项以找到最多 64 个 terminal record；根目录是 Fliuno 私有服务状态，不应混放用户文件。离线快照不会通过 PID 或锁文件推断进程仍活着，未明确结束的记录显示 `outcome_unknown`，并保留最后写入的 spool。Windows hard-crash 可由 Job 验证后代回收；Linux/macOS 当前无法仅从此离线快照证明进程已经停止。

初始化能力 `sessionContext:true`、`contextTaskSelection:true`、`maxContextTurns:32`、`maxContextBytes:262144` 表示支持跨任务历史与显式任务选择。`context`/`options` 默认 `{mode:"session",maxTurns:16,maxBytes:131072}`；有效范围 1..32 条和 1024..262144 UTF-8 字节，非法值返回 -32602。`mode:"isolated"` 使本任务不读取历史，完成后仍可成为后续任务的来源；另建会话可隔离工作范围。

`Task.context` 保存 `{options,throughSequence,turns,omittedTurns}`；turns 包含来源任务 ID、完成事件序号、后端、中性消息、产物元数据与宿主操作。默认选择同会话已知完成工作的连续最近后缀，超额按完整任务省略。预算包含序列化快照元数据，历史提示前缀另计。任务创建时冻结；预览不预留输入，提交返回的快照才是实际输入。重试/恢复/重启不重新选择历史；`Task.messages` 仍仅为当前任务。旧记录 context 为 null。详见 [上下文设计与预算](context.md)。

可选 `taskIds` 显式指定 1..maxTurns 个不同、同会话已完成记录 ID，每个为 1..128 可打印 ASCII 字节且无空格，只能用于 session 模式。输入 ID 按字典序规范化，历史仍按完成顺序；交换同一集合的输入顺序不改变幂等身份。不存在、其他会话和未完成记录统一返回 -32602；完整快照超预算也整体拒绝，不丢弃指定记录、不保存失败创建的任务/事件/幂等键。`omittedTurns` 为该会话已完成但未选中总数。数据库 v9 引入此冻结输入字段，v8 升级只提升版本号并保留旧快照。

历史作为 JSON 文本数据交给后端，不继承审批、执行身份或供应商签名续接，也不递归加入来源 context；新的模型或 ACP 操作仍独立授权。当前实现是有界选择，没有摘要压缩或模型 token 窗口自动管理。

外部 Agent 通过 `--acp-config` 注册，初始化能力 `externalAgents` 表示注册情况。任务的 `backend` 和 `agentRun` 持久化；外部权限在 `agentRun.pendingApprovals` 数组中，最多同时 16 项，与内置工具的 `pendingApproval` 分开。`approval/respond` 同时支持两种审批；拒绝外部权限交回 Agent，不直接终止任务。`agent_started`、`agent_session_opened`、`agent_update`、`agent_approval_requested/resolved/closed`、`agent_finished` 均进入同一事务事件日志。决定先持久化再交付，resolved 仅表示决定已记录，不承诺远端收到。Agent 任务与模型/工具共用执行名额，等待权限仍占名额。

`AgentApproval.source` 区分 `agent_permission`（Agent 自身权限，旧记录的默认值）与 `host_tool`（Fliuno 实际执行的宿主工具）。两者共用审批接口和 16 项容量；Agent 权限允许不能替代宿主工具审批。`host_tool.request` 为 `{call,preview}`，读请求的 preview 为 null，写请求包含 `{path,diff,createsFile}`。客户端展示已保存的参数与 diff，仍只提交审批 ID 和决策。

`agentRun.hostTools` 保存宿主操作的 ID、不可替换的 call、status、可选 output/error；旧快照默认空数组。状态为 `awaiting_approval`、`approved`、`running`、`succeeded`、`denied`、`failed`、`outcome_unknown` 或 `closed`。`agent_host_tool_requested`、`agent_host_tool_started`、`agent_host_tool_completed` 写入同一事件日志。允许决定先提交，执行前另行提交 started；结果提交成功后才向 Agent 返回文件内容或写入成功。运行中操作在断线/重启时保留未知结果，Agent 的正常结束不能将其覆盖。

主机配置 `workspaceAccess` 默认 false；开启时仅提供虚拟工作区内的文本读写代理，terminal 仍关闭。每个文件操作独立审批，单次文件/内容最大 256 KiB；每运行最多 8 个未完成文件请求、64 个累计宿主操作。写提案读取基线供客户端生成 diff，执行时复核；`baseSha256:null` 仅创建新文件。相关兼容字段最早在 v6 引入；可读取 v3 中缺少 `source`/`hostTools`、v4 中缺少 `context` 和 v5 中缺少后续事件兼容字段的记录，并事务回填已完成任务的历史索引；不改写旧输入和事件，旧二进制应拒绝新版本。

已启动的 Agent 运行不支持 `task/resume`，即使结果已知或传入模型重试同意也不能重放。服务退出或重启使待审权限失效，活动 Agent 任务进入 interrupted；取消任务保持 cancelled。运行 ID、`outcomeUnknown`、`finished` 和 `stopReason` 区分结果未知、执行器结束和已知但不完整的响应。配置、预算及完整状态映射见 [ACP 文档](acp.md)。

模型步骤和工具执行由 4 个固定 worker 并发处理，单个任务同时最多只有一个在途步骤；队列容量为 32，因此最多接受 36 个活动或排队的步骤。初始化能力会报告 `concurrentExecution: true` 和 `maxConcurrentExecutions: 4`。`backgroundTasks` 和 `taskCancellation` 为 true。`task/start` 和 `task/resume` 返回持久化后的任务快照并调度模型步骤；`approval/respond` 持久化审批决定和工具执行身份后即返回，工具执行完成后会自动排入下一模型步骤。客户端应通过 `task/get` 或事件通知观察后续状态；容量耗尽时任务以 `failed` 及原因结束，原幂等键仍绑定该任务。

模型和工具调用期间数据库锁已释放，因此查询和取消可以继续处理。取消会立即将任务置为 `cancelled`，并通知当前 provider 或工具；provider HTTP 等待和 MCP 调用会响应取消，迟到结果会被忽略。取消不保证阻止供应商计费或撤销已发生的外部副作用。工具调用身份在执行前持久化，重启后结果未知的调用不能自动重试。stdin 关闭后停止启动排队步骤并取消当前执行，再把未完成任务置为 `interrupted`。

支持 `sessionEventReplay` 的客户端可用 `events/listSession` 回放某一会话内各任务的持久事件。它沿用全局的 `sequence`，所以相邻记录的序号可能不连续。使用 `nextAfter` 翻页，并可传入包含式 `through` 固定回放上界。有固定上界时，最后一页即使没有事件，也代表该会话已回放到上界。实时通知默认采用全局事件订阅。

`workspaceEventReplay` 表示支持 `events/listWorkspace`。该方法使用相同的序号、包含式 `through` 和字节分页规则，跨会话读取整个工作区的持久事件。SQLite v27 回填工作区索引。

### 模型用量

#### 用量汇总

若 `initialize.capabilities.modelUsageSummary` 为真，可一次读取任务、会话或工作区的累计用量。工作区包含已归档会话。已知范围没有报告时，各项为 `"0"`；未知范围返回 `-32004`。

```json
{"jsonrpc":"2.0","id":6,"method":"modelUsage/getSummary","params":{"scope":{"kind":"workspace","workspaceId":"<workspace-id>"}}}
```

| `totals` 字段 | 含义 |
| --- | --- |
| `reportCount` | 已提交的供应商用量报告数，可能少于模型调用数。 |
| `inputTokens`、`outputTokens` | 报告中输入与输出 token 的累计值。 |
| `cachedInputTokens`、`reasoningOutputTokens` | 已报告的缓存输入和推理输出累计值；它们已经包含在对应总数中。 |
| `cachedInputReportCount`、`reasoningOutputReportCount` | 携带对应可选字段的报告数。显式 `0` 计为已报告，省略的字段不计。 |

所有计数是十进制字符串，避免大整数在 JavaScript 中丢精度。不同供应商的已报告用量会合计，此结果不估计缺失用量或换算费用。`throughSequence` 是该快照同一读视图的全局事件水位，不是最近一条用量事件的序号。若调用方用事件维护增量，可从该水位继续[回放用量报告](#回放用量报告)，不要再次计入快照已覆盖的事件。

SQLite v43 从已有用量索引回填汇总；新报告和三个范围的计数在同一事务中保存。查询时只读范围计数与当前水位。Rust 调用方可用 `Client::model_usage_summary`。首次启动会完成回填后再接受请求；如需退回旧二进制，应保留升级前的数据库备份。

#### 批量用量汇总

`initialize.capabilities.modelUsageSummaryBatch` 声明 `modelUsage/getSummaries` 可用，`maxModelUsageSummaryBatch` 给出支持的上限（当前 64）。一次请求可以混合任务、会话和工作区。`scopes` 至少有一项；ID 长度为 1 到 128 个可见 ASCII 字节。无效参数会拒绝整次请求。

```json
{"jsonrpc":"2.0","id":8,"method":"modelUsage/getSummaries","params":{"scopes":[{"kind":"workspace","workspaceId":"<workspace-id>"},{"kind":"session","sessionId":"<session-id>"}]}}
```

`items` 和请求一一对应，保留重复位置。范围不存在时返回 `null`，范围存在但暂无报告时返回一组十进制字符串 `"0"`。所有项均对应同一个全局 `throughSequence`。单独比较不同层级的用量时，注意同一份报告可能同时计入任务、会话和工作区，不能将重叠范围的值相加。Rust 客户端可用 `Client::model_usage_summaries`；旧服务仍可逐条调用 `Client::model_usage_summary`，每条有独立水位。

#### 按事件范围汇总

`initialize.capabilities.modelUsageRangeSummary` 为真时，客户端可以读取某个检查点之后的用量。`scope` 与 [累计汇总](#用量汇总) 相同，包含任务、会话和工作区三个层级。工作区汇总包括已归档会话。

```json
{"jsonrpc":"2.0","id":9,"method":"modelUsage/summarizeRange","params":{"scope":{"kind":"workspace","workspaceId":"<workspace-id>"},"after":220,"through":320,"limit":256}}
```

服务器读取 `(after, through]` 范围内的报告。`after` 默认是 `0`，省略 `through` 时在同一 SQLite 读事务中取当前全局事件水位。必须满足 `after <= through <= 当前事件水位`，错误范围返回 `-32602`，不存在的范围返回 `-32004`。`limit` 默认是 256，服务通过 `maxModelUsageRangeReports` 公布上限（当前 512）。

- `totals` 只包含本页贡献，七个字段均为十进制字符串。报告中明确给出的 `0` 计入对应的可选字段报告数。
- `hasMore:true` 时，把 `nextAfter` 和 `throughSequence` 分别作为下一页的 `after` 和 `through`。相加每页各字段可得固定窗口的汇总。
- `hasMore:false` 时，`nextAfter` 直接到达 `throughSequence`，即使末尾没有报告也是如此。客户端可以保存它，作为下一次增量汇总的起点。

Rust 客户端可调用 `Client::summarize_model_usage_range`。需要查看单次模型调用时，使用[逐条报告回放](#回放用量报告)。

#### 回放用量报告

`modelUsageEventReplay` 声明 `events/listModelUsage` 可按任务读取。`modelUsageScopedReplay` 为真时，也可以用 `events/listSessionModelUsage` 或 `events/listWorkspaceModelUsage` 按更大的范围读取，包括已归档会话。三种接口都返回原始用量事件及全局序号，按序号正序排列。

上下文压缩会额外产生 `context_compaction_scheduled` 和 `context_compacted` 持久事件。前者表示输入预算预检确认正式生成未派发并已安排无工具摘要；后者带摘要模型步骤及来源任务/完成事件序号。摘要文本、提示版本、预算、计数和用量保存在任务的 `context.summaries` 快照中。重启恢复依赖任务快照和事件，不会重复应用已提交的摘要。失败或取消不会移除原历史或已有摘要。

先调用 `events/getWatermark`，再将返回的 `throughSequence` 作为每页共同的 `through`。以下示例在水位 `320` 处固定工作区视图：

```json
{"jsonrpc":"2.0","id":7,"method":"events/listWorkspaceModelUsage","params":{"workspaceId":"<workspace-id>","through":320,"limit":32}}
```

`hasMore` 为真时沿用当前范围与 `through`，将 `nextAfter` 作为下一页的排他游标。Rust 调用方也可用 `EventReplay::for_model_usage(ModelUsageScope::Workspace(id), after, through)`，消费一页后保存 `cursor()`。翻完一轮后，Rust 客户端将游标推进到该轮的 `through`；下次用更新的水位发起回放即可只读新增报告。

SQLite v40 为任务、会话和工作区建立用量索引。v37–v39 的数据库从旧用量索引迁移，无需重读所有事件正文；新用量和任务结果同事务提交。各范围的分页直接定位报告，不读取其他类型的事件。

`initialize.capabilities.modelUsageEvents` 表示模型用量可作为 `model_usage_reported` 持久事件回放。OpenAI Responses、Anthropic Messages 和 Gemini Interactions 均支持 JSON 与 SSE。事件含 `step`、供应商及 `inputTokens`、`outputTokens`。`cachedInputTokens` 与 `reasoningOutputTokens` 可选，并包含在对应总数内。Anthropic 的输入总量包含创建与命中缓存的 token。Gemini 沿用 Interactions 报告的输入与输出总量，单独报告的思考计数未纳入 `reasoningOutputTokens`。

内核在同一事务提交用量与该步结果。模型响应成功解析后，即使内核拒绝模型发出的工具调用，也会记录该步用量。缺失或无效的计数不影响模型结果。接口只回放供应商已返回且内核已保存的报告；缺少报告不能据此推断该步骤没有用量。SQLite v36 标识新增的事件类型。

### 实时预览与事件通知

实时 provider 可在模型响应完成前发送 `notifications/modelProgress`，参数为 `{progress}`。`progress` 是带 `type` 的联合类型：`text_delta` 包含 `taskId`、`step` 和文本片段，`reset` 清除该步骤尚未确认的预览。它不写入 SQLite、没有持久序号，且可能与 `notifications/event` 相对乱序。客户端应按 task/step 暂存增量；收到该步骤的审批请求或任务完成、失败、取消、中断等持久状态后，忽略迟到增量并以任务快照和持久事件为准。取消、错误、工具调用会清除预览。`initialize.capabilities.modelProgressNotifications` 表示当前 provider 是否支持此通知。慢订阅者被分离后会收到 `notifications/modelProgressGap`（参数为空）；预览不回放，客户端应清除预览并查询活动任务状态。如果 `capabilities.modelProgressResubscribe` 为 true，收到缺口通知后可用 `modelProgress/resubscribe({})` 在同一连接恢复，成功返回 `{subscribed:true}`。新订阅只接收注册之后开始的模型步骤。正在执行的步骤可通过持久事件或 `task/get` 查看结果。

初始化后，服务会发送 `notifications/event`，参数为 `{event}`；每个事件已先提交 SQLite，再进入容量为 64 的有界内存订阅队列。实时通知只覆盖本次进程连接期间的新事件，不补发历史。客户端重连时用 `events/listAll` 从持久游标回放，并按全局 `sequence` 去重，以弥合订阅和回放之间的竞态。

若客户端消费过慢导致订阅队列溢出，服务先排空已排队通知，再发 `notifications/eventGap`。参数 `{afterSequence}` 是本连接最后成功写出的序号。客户端从自身已提交的游标调用 `events/listAll` 补齐。SQLite 日志是事实来源，通知可能丢失。事件按 `sequence` 排序，`after` 为严格大于且须在非负 i64 范围，`limit` 在 1..1000。客户端保存 `nextAfter`、处理 `hasMore` 并按序号去重。

`initialize.capabilities.eventReplayTargetBytes` 表示回放方法支持的最大字节目标。客户端可设置 `targetBytes` 为 1024..8388608，按紧凑 JSON 的 `events` 数组大小分页。单个事件已超过目标时，该页完整返回该事件，游标仍会前进。

服务声明 `eventWatermarkQuery` 时，可用 `events/getWatermark` 读取当前水位。该方法保留连接现有的订阅、通知队列和缺口状态。多个读取方可把同一水位传给各自的 `events/list*` 请求，按各自的游标独立翻页。

服务声明 `taskEventWatermarks` 时，`events/getTaskWatermarks` 可按任务批量读取最新已提交事件。例如 `{"taskIds":["a","b","a"]}` 可返回 `{"throughSequences":[42,null,42]}`。每批 1 到 64 个 ID，单个 ID 为 1 到 128 个可见 ASCII 字节。返回的序号属于同一个全局事件序列，但仅在该任务有新事件时推进。读取使用同一个 SQLite 视图，不读取事件正文，也不替换现有订阅。Rust 接口为 `Client::task_event_watermarks`。

支持 `eventSubscriptionScope` 的 stdio 连接可请求 `events/setScope`。传入现有 `sessionId` 即可订阅该会话的持久事件。如有 `workspaceEventSubscription`，也可传入 `workspaceId` 订阅整个工作区。如有 `taskEventSubscription`，可传入 `taskId` 订阅单个任务，并用 `events/list` 的 `through` 参数限定回放上界。至多指定一个范围。三个范围参数均省略或为 `null` 时恢复全局范围。声明 `eventSubscriptionCursor` 的服务接受可选的 `after`。如果该序号超过切换时的水位，返回 `-32009` 并保留原有订阅。请求成功时服务替换旧队列，并返回切换时的全局水位 `throughSequence`。在响应之后暂存新通知。按当前范围用 `events/list`、`events/listSession`、`events/listWorkspace` 或 `events/listAll` 补齐至这个水位，再按序号处理暂存通知。队列中尚未发送的旧通知可从持久回放读取。实时模型预览和终端通知各自独立。批量请求依次执行，实时队列使用最后一次成功切换的范围。

支持 `eventResubscribe` 的连接收到 `notifications/eventGap` 后，可用 `events/resubscribe` 重新订阅。先暂存恢复后的实时通知，再从本地游标调用当前范围对应的回放方法，传入返回的 `throughSequence` 作为包含式 `through`。回放结束后按序号处理暂存通知。服务在同一内核锁下采集水位并建立订阅，实时通知从该水位之后开始，沿用恢复前的范围。若实时队列再次溢出，可重新建立订阅并从已保存游标补齐。订阅仍有效时重复调用返回 -32009。

服务声明 `eventWatermarkNotifications:true` 时，`events/setScope` 支持 `watermarkOnly:true`。匹配范围内的提交推动 `notifications/eventWatermark`，参数为 `{throughSequence}`。声明 `coalescedEventWatermarks:true` 时，待发提醒只保留最新的匹配事件序号。客户端按序号回放事件，不应把通知数量或序号跳跃视为提交数或缺口。工具输出等正文只在分页回放中读取。收到缺口后，`events/resubscribe` 延续原来的范围与通知模式。省略 `watermarkOnly` 的订阅继续使用完整事件通知。

Rust 调用方可用 `fliuno_client::EventFeed::subscribe` 建立订阅，将其他 RPC 的通知回调和连接空闲时的通知交给 `observe`。`next_page` 从当前水位读取已提交事件，在收到缺口后按同一范围重新订阅并补读。事件通知只更新最高序号，客户端不会缓冲通知正文。调用方可将处理后的页面和 `cursor()` 一起保存，重连时从保存位置继续。`None` 表示已经读到目前收到通知的水位。`EventFeed` 管理当前连接的订阅范围，在服务支持时选用水位通知。模型预览和终端通知照常交给调用方。

## 错误

| code | 语义 |
| --- | --- |
| -32700 | JSON 解析失败 |
| -32600 | 非法 JSON-RPC 信封 |
| -32601 | 未知方法 |
| -32602 | 参数非法 |
| -32603 | 内部错误 |
| -32002 | 尚未初始化 |
| -32004 | 对象不存在 |
| -32009 | 状态冲突、重复审批等 |
| -32010 | 不支持的协议版本 |

`agents/getCapabilities` 直接读取本地注册项，返回 `remoteTaskChecks`、`remoteTaskWatch`、`remoteTaskContinuation` 和 `agentSessionContinuation`，若注册已开启会话目录则额外返回 `agentSessionListing: true`。这些标记表示适配器提供相应路径，调用仍需满足相应的协议条件。不存在的 ID 返回 `-32004`。

`initialize.capabilities.agentSessionListing` 为真时至少有一个注册启用目录。一个查询运行、最多四个排队；未启用或队列已满返回 `-32009`。查询结果状态为 `pending`、`complete`（含 `page.sessions` 和可选 `page.nextCursor`），或 `failed`（含 `unsupported`、`rejected`、`unavailable`、`invalid_page` 之一）。游标最多 4096 字节，按原样提交给同一个 Agent；服务不主动翻页。过期或重启前的请求 ID 返回 `-32004`。目录查询与本地 `session/list` 相互独立。

## A2A 远程 Agent 配置

远程 Agent 通过独立的 `--a2a-config` 注册，选择方式沿用 `task/start.backend` 的 `external_agent`。`agentRun.remoteTask` 保存稳定的远程 id、contextId 与中性 state；状态变化以 `agent_remote_task_updated` 事件回放。`agentRun.artifacts` 记录远端 sourceId 与本地 artifactId，内容仍通过 `artifacts/list` 查询。新增 `failed` / `rejected` / `input_required` / `auth_required` 结束原因；详细状态映射、配置、取消取证和 v7 迁移见 [A2A 任务与产物](a2a.md)。这些字段默认空，现有 ACP 任务语义不变。

## MCP 主机配置

stdio 服务器必须配置 `isolation`；HTTP 服务器不使用该字段，可配置 `streamable_http` 或已弃用的 `http_sse`。同一份 servers 列表可以混合这些传输，见 [MCP 传输配置](mcp.md#传输配置)。

以下为结构示例，请替换成实际可执行文件与目录，且仅配置你信任的服务。

```json
{
  "servers": [
    {
      "id": "local",
      "isolation": "trusted_host",
      "command": "C:/absolute/path/to/server.exe",
      "args": [],
      "cwd": "E:/absolute/workspace",
      "envAllow": ["SERVICE_SPECIFIC_TOKEN"],
      "timeoutMs": 30000
    }
  ]
}
```

HTTP 服务器不启动子进程，使用显式 `transport` 和环境变量名称配置地址与凭据：

```json
{
  "servers": [
    {
      "id": "remote",
      "transport": "streamable_http",
      "url": "https://mcp.example.test/mcp",
      "bearerTokenEnv": "REMOTE_MCP_TOKEN",
      "headersEnv": {"X-Vendor-Key": "REMOTE_MCP_VENDOR_KEY"},
      "timeoutMs": 30000,
      "lifecycle": "auto"
    }
  ]
}
```

`headersEnv` 是 HTTP 头名到环境变量名的映射；值只能从 Fliuno 主机进程环境读取。传输控制头由协议栈管理，不可配置；同一凭据头不可重复声明。

握手声明 `resources` / `prompts` 的 MCP 服务还会注册按服务 ID 命名的资源、模板与提示工具。它们使用普通工具审批和持久结果，不增加另一套应用 RPC；分页、URI、角色数据、容量和恢复边界见[内容通道](mcp-content.md)。只有资源或提示的服务不接收 `tools/list`。

`timeoutMs` 可选，默认 30000，取值范围 100..300000 毫秒，分别限制初始化、发现与每次调用；调用预算包含等待同一主机前序调用的时间。排队时的取消或等待超时返回已知失败，不向服务器发送请求，也不关闭正被其他任务使用的主机。请求发送后超时、取消或协议失败会关闭该 MCP 主机并将外部结果标记为未知；重启 Fliuno 后才会重新连接，内核不会自动重放该调用。工具内 MRTR 仅支持表单 elicitation 的显式用户补答续接；不支持自动 sampling、roots 或 URL elicitation。子进程在 Unix 上放入独立进程组，在 Windows 上放入 Job Object，关闭时回收其进程树。工具别名稳定且限定为模型 API 可接受的 ASCII 名称，原始名称由适配器保存。stdio 生命周期、版本协商与消息格式由官方 `rmcp` 实现；HTTP 配置、生命周期、预算和支持边界见 [MCP 传输文档](mcp.md)。

stdio 的 `isolation` 必填。`trusted_host` 使用宿主用户权限，必须指定 `cwd`。Windows 可选择 `windows_app_container`，Linux 可选择 `linux_landlock`；隔离模式必须省略 `cwd`。两者都在新建私有目录中启动单个程序，不授予 workspace 路径，并在隔离设置失败时拒绝启动。Linux 要求 Landlock ABI 6，系统运行目录只读开放，seccomp 禁止 socket 操作；当前 cgroup v2 有可写委派时，sandbox 将进程放入私有 cgroup，并用 `cgroup.kill` 回收含 `setsid` 后代的整树。资源预算依赖已委派的 domain cgroup 和 `cpu`/`memory`/`pids` 控制器；通过 `linuxCgroupParent` 指定时，Fliuno 在该目录下创建私有子 cgroup 并设置所请求的 `memory.max`、`pids.max`、`cpu.max`，任何一项不可用都会拒绝启动。Windows 默认 Job 上限为 16 个活动进程和 512 MiB commit，可由 `resources` 覆盖；CPU quota 仅支持 Linux。Linux sandbox 的独立 guardian 通过 pidfd 监控宿主进程整体，宿主退出或 cgroup 所有者 Drop 后回收；创建它的线程退出不会终止任务。子进程在 exec 前完成 cgroup 加入和 pidfd 交接确认，guardian 不可用时拒绝执行。Windows 系统共享资源仍受既有 ACL 控制。解释器伴随文件通过宿主固定摘要的 `assets` 清单提供；MCP workspace 代理尚未实现，见[伴随文件](runtime-bundles.md)。其他平台请求不支持的隔离模式会明确失败，绝不退回宿主权限。

```json
{"servers":[{"id":"isolated","isolation":"windows_app_container","command":"C:/absolute/path/to/native-mcp.exe","timeoutMs":30000}]}
```

Linux 示例（路径须由宿主预先委派，并在 `cgroup.subtree_control` 启用这些控制器）：`{"servers":[{"id":"isolated","isolation":"linux_landlock","command":"/usr/bin/example-agent","resources":{"memoryMaxBytes":536870912,"pidsMax":64,"cpuQuotaMicros":50000},"linuxCgroupParent":"/sys/fs/cgroup/fliuno-delegated","timeoutMs":30000}]}`。ACP `agents` 配置和 `--exec-config` 的每个 `programs` 条目接受相同的 `resources` 与 `linuxCgroupParent` 字段；宿主配置固定这些权限，模型请求无法覆盖。

隔离进程先暂停创建，加入 Job Object 后再恢复；Job 限制为 16 个进程、总提交内存 512 MiB。继承句柄仅允许 stdin/stdout/stderr 三条管道。关闭、初始化失败、超时与取消都会回收该进程树与私有目录。审批仍绑定每次工具调用，OS 隔离不替代审批策略。

## 内置工具与作用域

`tools/list` 的效果分类包括 `artifact_write`、`workspace_read`、`workspace_write`、`skill_read`、`process_execution`、`terminal_control`、`external`。分类由主机工具实现提供；默认所有调用都请求一次审批。客户端审批只提交审批 ID 和决策，不提交新的工具参数或工作区。

- `artifact_create_binary({title,mediaType,contentBase64})`：逐次审批后保存二进制产物。媒体类型须是具体的 `type/subtype`，不带参数。输入使用标准有填充的 Base64，按解码后的大小限额 256 KiB（`maxBinaryArtifactBytes`）。只写 SQLite 产物库，不接受文件路径，也不打开或执行内容。工具结果只返回产物 ID、字节数和媒体类型。

- `workspace_list({path})`：工作区内目录，根目录用 `.`；返回最多 256 项及 truncated。
- `workspace_list_page({path,after?,limit?:128})` 按文件名的 UTF-8 字节顺序返回 `{entries,nextAfter,hasMore,omittedNames}`，每页 1..256 项。目录不变时，按 `nextAfter` 续页可浏览全部可寻址条目。文件名无法作为工作区相对路径片段时，计入 `omittedNames`。每次扫描最多 16,384 项，超限返回错误。分页期间目录可以变化，游标只表示名称边界。每页按工作区读取工具走审批。
- `workspace_stat({path})`：查询工作区相对路径，根目录用 `.`。返回 `path`、`kind`（`file`、`directory`、`symlink` 或 `other`）、普通文件的 `bytes` 和可用的 `modifiedUnixMs`。不适用的字段为 `null`。不跟随末级符号链接。每次查询按工作区读取工具走审批。
- `workspace_stat_batch({paths})` 一次审批查询 1 到 16 条不同的工作区相对路径，路径文本合计最多 8 KiB。返回按输入排序的 `entries`。可读取的项包含 `status:"found"` 及 `workspace_stat` 的元数据字段。无法读取的项仅返回 `path` 和 `status:"unavailable"`。整个批次会先验证路径格式。工具不读取文件正文，也不跟随末级符号链接。
- `workspace_sha256({path})`：审批后对至多 64 MiB 的工作区普通文件逐块计算 SHA-256，返回 `path`、实际读取的 `bytes` 和小写十六进制 `sha256`。结果不含正文。并发修改文件时，摘要只描述实际读到的字节流。
- `workspace_sha256_batch({paths})` 用一次审批计算 1 至 8 个不同的工作区文件的 SHA-256，路径文本合计最多 8 KiB，每个文件最多 8 MiB。`entries` 按输入顺序返回。`hashed` 项包含 `path`、`bytes` 和 `sha256`，`unavailable` 项仅包含 `path` 和状态。缺失、超限、不可读、非普通文件和末级符号链接都归为 `unavailable`。取消会结束整个批次。工具只返回摘要和字节数，逐个读取文件，因此并发修改可能影响结果。
- `workspace_read_text({path,line?,limit?})`：读取最多 256 KiB UTF-8 文件；line 从 1 开始，limit 是最大行数，均须为正整数。按行截取保留原换行，SHA-256 始终针对完整文件；超出末行返回空文本。
- `workspace_read_bytes({path})`：读取最多 256 KiB 原始字节并返回 `{path,encoding:"base64",data,bytes,sha256,grantsPermissions:false}`。长度和摘要针对原始字节；不检测 MIME、不解码格式、不执行，也不自动创建 artifact。该读取和所有其他工具调用一样，每次需要审批；拒绝审批不会返回文件数据。
- `workspace_search_text({paths,query,maxMatches?:24})`：一次审批搜索 1 到 8 个明确指定的工作区相对文件路径，每份文件最多 256 KiB。`query` 为 1 到 256 字节的单行 UTF-8 字面文本，区分大小写。按输入的文件顺序和行序，每行只返回首个命中的行号和 UTF-8 字节列，均从 1 开始。`maxMatches` 限制整批返回 1 到 64 个命中。每份文件分别报告 `searched`、`nonText` 或 `unreadable`。超过匹配预算时，相应文件的 `hasMore` 为 true。工具结果仅含位置，不含原文。
- `workspace_import_artifact({path,title})`：一次审批后将指定常规文件按原始字节保存为当前任务的二进制产物，至多 256 KiB。返回 `{artifactId,contentBytes,mediaType,sha256}`，固定类型为 `application/octet-stream`。审批绑定工作区相对路径，读取时不跟随末级符号链接。客户端可通过 `artifacts/read` 下载，原文件之后删除也不影响已提交的产物。
- `workspace_create_text({path,content})`：创建新文件，父目录须存在，目标须不存在。
- `workspace_patch_text({path,baseSha256,patch})`：对已存在文本文件应用一个 unified diff。diff 的 `a/` 与 `b/` 路径必须与 `path` 一致，且必须是单文件修改；`baseSha256` 必须等于最近读取的内容哈希。审批记录包含完整 diff，批准前不修改文件。执行时通过打开的文件句柄复核基线，以不跟随符号链接方式打开最终文件，拒绝多硬链接目标。本进程内的新建和补丁会串行化；其他进程不受此互斥锁约束，因此外部并发编辑不具备事务性保证。写入错误或同步错误标记为结果未知，中断任务且不会自动重试。
- `workspace_replace_text({path,content,baseSha256})`：全文替换或创建；baseSha256 必须提供，既有文件须匹配小写 SHA-256，null 仅允许新建。与补丁共享最终文件的符号链接/硬链接检查以及写入锁；保留权限的原位更新不构成针对外部进程的原子 CAS。

读取、补丁基线和全文写入预览共用普通文件检查、实际字节预算和逐块取消；不能通过按行参数读取超额文件的局部内容来绕过 256 KiB 限制。Unix 以 O_NONBLOCK 打开已有文件后检查描述符，拒绝 FIFO 等特殊文件；该标志不保证普通存储 IO 可在固定时间内中止。读取失败或取消不返回部分成功；尚未开始写入时不会报告已修改。
- `skills_list({})` / `skills_read({name})`：需主机通过 `--skills-dir` 配置技能目录。
- `skills_list_resources({name,path?:""})` / `skills_read_resource({name,path,encoding?:"utf8"|"base64"})`：单技能能力内的单层列举和有界文本/原始二进制编码；完整边界见[Skills](skills.md)。

路径使用相对的正斜杠形式。绝对路径、`..`、反斜杠、ADS、Windows 设备名等被拒绝，目录能力继续约束实际路径解析。

对象分页返回 `{items,nextAfter,hasMore}`，对象列表 limit 在 1..100；after 是数据库局部游标，不应解析其数值意义。`clientRequestId` 为会话内 1..128 可打印 ASCII 字节；重试必须使用相同输入、后端与上下文选项。新任务省略 context 和显式默认选项等价；升级前 context=null 的旧任务仅接受未显式指定 context 的原样重试。没有提供幂等键的 task/start 每次都创建新任务。

`initialize.capabilities.taskStartSummary` 和 `initialize.capabilities.taskSummaryByRequest` 分别表示两个摘要方法可用。`task/startSummary` 与 `task/start` 共用幂等键和重试规则。已有请求会返回原任务的当前摘要。`task/getSummaryByRequest` 只查找已有任务，请求键未知或属于另一会话时返回 `-32004`。

`initialize.capabilities.taskListTargetBytes` 给出 `task/list` 支持的最大字节目标。传入 `targetBytes`（1024～8388608）时，`items` 按紧凑 JSON 的 UTF-8 长度分页。单项已超过目标时仍完整返回该项。字节目标和行数上限共用原始 `nextAfter` 游标，可用 `task/listSummaries` 在相同位置读取摘要。

`initialize.capabilities.taskSummaryListTargetBytes` 给出 `task/listSummaries` 和 `task/listWorkspaceSummaries` 支持的最大字节目标。两种清单的 `targetBytes` 使用相同范围和紧凑 JSON 数组字节计数，配合 [任务目录](#任务目录) 的排序和状态筛选使用；第一项即使超过目标也完整返回。

### 查找历史任务

`session/searchHistory` 只检查指定会话里已完成且有历史投影的用户与助手消息。查询去除首尾空白后须为 1～256 个 UTF-8 字节，按 Unicode 小写转换后的字面子串匹配，无词法或正则语法。`role` 可设为 `user` 或 `assistant`，省略就查两种消息。`limit` 默认为 16，接受 1～32。结果按完成序号从新到旧，包含 `taskId` 与 `completionSequence`。

每页默认最多检查 128 条历史。服务声明 `maxHistorySearchScanLimit` 时，可显式传 `scanLimit`（1～512，不超过声明的上限）。小窗口适合限制一次调用的工作量，大窗口适合少翻几次稀疏结果。`limit` 仍是最多返回的命中数，先达到任一上限就结束本页。超过 128 KiB 的精简对话投影会跳过并计入 `skippedOversizedTurns`；`scannedTurns` 包含这些记录。

只要 `nextBeforeSequence` 有值就还能翻页，即使 `matches` 为空。继续时沿用同一范围、`query` 和 `role`，传入首次返回的 `throughSequence` 和上一页的 `nextBeforeSequence` 作为 `beforeSequence`。可以调整 `scanLimit`，游标仍从已检查的最后一条继续。Rust `HistorySearch` 会保存扫描预算，校验能力和回包后才更新游标。连接旧服务时省略 `scanLimit`，仍按默认窗口读取。调用方可用查到的 `taskId` 配合 `session/context` 的 `options.taskIds` 预览和选择历史。

`initialize.capabilities.workspaceHistorySearch` 为真时，可用 `workspace/searchHistory` 搜索同一工作区的所有会话，包括已归档会话。每个命中同时返回 `sessionId`，可用它打开对应会话。查询规则、扫描上限和分页水位与 `session/searchHistory` 相同。SQLite v33 回填按工作区和完成序号排序的索引，现有会话搜索响应保持原样。搜索不会把其他会话内容自动加入模型上下文。

#### 只搜索新完成的任务

服务声明 `historySearchAfterSequence: true` 时，两种搜索都接受 `afterSequence` 作为不含式下界。省略时从历史起点搜索。`throughSequence` 是含式上界，首请求省略即使用当前水位；`beforeSequence` 若提供，必须大于 `afterSequence` 且不超过 `throughSequence`。下界大于上界返回 `-32602`；相等返回空页。

```json
{"jsonrpc":"2.0","id":12,"method":"workspace/searchHistory","params":{"workspaceId":"<workspace-id>","query":"交付报告","afterSequence":120,"scanLimit":32}}
```

续页时保留 `afterSequence` 和首响应的 `throughSequence`，并传入 `nextBeforeSequence`。范围外的完成记录不计入 `scannedTurns`。同一查询的所有分页处理完毕后，可把 `throughSequence` 用作下一轮搜索的 `afterSequence`。Rust `HistorySearch` 保存下界，收到越界命中或无法继续的游标时保留原请求；旧服务上带下界的搜索在发送前返回不支持。数据库无需迁移。

#### 打开命中或用于模型上下文

要查看某一条命中，检查 `sessionHistoryGet`，将命中的 `taskId`、所属 `sessionId` 及搜索页的 `throughSequence` 传给 `session/getHistoryTurn`。会话搜索沿用原请求的 `sessionId`，工作区搜索则使用该命中的 `sessionId`。[请求示例与错误边界](context.md#打开一条历史记录)。

`initialize.capabilities.contextMatchRecent` 表示上下文选项也支持 `selection: "match_recent"` 和 `historyQuery: {query,role?}`，将近期匹配记录直接冻结到新任务。`maxContextScanLimit` 声明 `fit_recent` 与 `match_recent` 可通过上下文选项中的 `scanLimit` 调整候选窗口，默认仍为 128 条；此选项独立于历史搜索的范围。见[上下文扫描范围](context.md#调整上下文扫描范围)。

搜索从既有的精简历史投影读取匹配字段，命中时返回索引中的任务 ID。构建上下文时才完整解码命中项，并继续应用原有的字节预算。

## 运行时补充指令

`taskSteering:true` 表示可用 `task/getSteering` 和 `task/steer`。先按 `taskId` 读取 `throughSequence`，提交 `{taskId,expectedSequence,clientRequestId,instruction}`。只接受同一个活跃模型任务，水位变化后需刷新再提交。每项 4096 字节，每任务最多 8 项。重复请求键和参数返回原收据。

任务在下一次正式模型派发前按接收顺序把指令追加为用户消息。`task_steering_attached` 和收据的 `attachedStep` 表示指令已写入该步骤的请求快照。期间完成的模型回复先保存为草稿，只要剩余模型步数足够便进入下一步。审批、澄清和上下文摘要仍有原有挂起边界。终止时没有 `attachedStep` 的请求表示任务未使用该指令。

## 任务计划与澄清

服务声明 `taskInteraction:true` 时，在模型任务的 `context` 里传 `enableInteraction:true`。模型可通过 `task_publish_plan` 提交最多 12 个步骤，客户端通过 `task/getPlan` 或 `task_plan_updated` 事件读取。计划用于查看当前工作，后续文件、命令和外部工具仍逐次检查权限。

模型也可通过 `task_ask_user` 暂停并提问。任务进入 `waiting_input`，`pendingToolInput.requests` 为 `{"kind":"clarification","question":"…"}`。使用 `task/respondToolInput` 提交与任务和调用 ID 绑定的 `responses:{"answer":"…"}`。明确答复进入原模型任务。等待中的问题可在重启后读取并继续处理。


`workNextActions:true` 时，给 `work/snapshot` 或 `work/subscribe` 传 `includeActions:true`。返回的 `nextActions` 与快照共用一个读取水位。它从近期任务推导，默认检查最近 25 项。客户端根据动作类型和任务 ID 读取审批或补答详情，再提交原操作。服务会重新校验当前状态。

## 终端协作接口

| 握手能力 | 方法与语义 |
| --- | --- |
| `taskPlanOnly` | 模型任务在 `context.planOnly` 传 `true`。执行绑定包含计划、澄清与宿主明确支持只读规则的 `WorkspaceRead` 工具。新建实施任务时按当前配置重新绑定并评估权限。 |
| `taskQueue` | `taskQueue/enqueue` 使用 `TaskStartParams`，要求模型后端、文本输入和 `clientRequestId`，可携带同会话的产物引用。相同请求键和草稿返回原回执。`taskQueue/list {sessionId}` 按入队顺序返回最多 16 个待执行项及阻塞任务。 |
| `taskQueue` | `taskQueue/startNext {sessionId,queueId}` 原子创建任务并记下回执，要求目标为首个待执行项、会话未归档且无非终态任务。再提交同一 ID 返回原回执。`taskQueue/cancel` 接受同样参数，取消待执行项。已创建的任务使用 `task/cancel`。 |
| `sessionFork` | `session/fork {sessionId,clientRequestId,title?,throughTaskId?,throughSequence?}` 继承固定完成水位以内最近 64 条对话，返回新会话的 `forkOrigin`。两个水位参数最多传一个。继承的文字用于上下文及历史浏览，任务、操作审批、授权和队列属于原会话。 |
| `sessionSearch` | `session/list` 可传 `query`，按标题或 ID 做字面子串匹配，ASCII 忽略大小写。与 `archived`、`order` 和分页游标组合使用。 |
| `taskTimeline` | `task/timeline {taskId,after?,throughSequence?}` 返回最多 32 条事件索引。续页传 `nextAfter` 与首批返回的 `throughSequence`，按需使用 `events/get {sequence}` 读取原事件。 |

未结任务占用所属会话的队列。用户可恢复任务、完成审批或澄清，或取消任务后手动启动下一项。任务详情及原事件会保留结果未知的记录。

## 上下文与项目说明

`workspaceInstructions:true` 时，通过 `workspace/getInstructions` 读取 `{workspaceId,revision,text}`。通过 `workspace/setInstructions` 提交 `{workspaceId,expectedRevision,text}`，首次写入使用修订 `0`。新任务在 `context.instructionsRevision` 中显式选择当前修订，任务保存该修订的文本快照。`taskExplicitMemory:true` 声明任务可选定确切记忆版本，见[工作区记忆](memory.md)。

`taskContextAttribution:true` 时，`task/contextAttribution {taskId}` 返回历史及摘要引用、记忆命中来源和因预算省略状态、说明修订和字节数。`normalizedInputBytes` 是当前可重建的规范化请求估算。`latestPreflight` 和 `latestBudgetRejection` 引用已记录的事件，供应商总量的覆盖范围取自该事件。

## 核对与修订

| 能力 | 方法 | 用途 |
| --- | --- | --- |
| `taskReconciliation` | `task/reconcile {taskId,state,operator,evidence,checkId?}` · `task/getReconciliation {taskId}` | 记录已结束且结果未知的外部 Agent run 的终态证据。可选 `checkId` 须对应同一运行和状态。原任务状态保留，提交结论不会派发远端动作。 |
| `artifactRevisions` | `artifacts/linkRevision {previous,current}` · `artifacts/getRevision {taskId,artifactId}` | 返回两稿引用与摘要。引用形状为 `{taskId,artifactId}`。两稿是同会话、同媒体类型的文本交付物，上一稿必须先于当前稿创建。 |

## Rust 宿主的工具版本与连接

`Kernel::activate_model_tool_version` 以预期的当前 authority 作并发检查。已有模型任务继续使用冻结绑定，新任务读取新绑定。宿主在重启时透过 `retain_model_tool_version` 重新登记历史实现。`revoke_model_tool_version` 持久保存撤销，已开始的工具继续遵循原调用记录。存在运行中的持久 Run 时需先等待该 Run 结束。

`RpcServer::attach_connection` 创建独立的协议连接，和原连接共享同一内核与调度器。连接有自己的握手、订阅和水位。宿主持有运行实例时，关闭单个连接后可附加新连接。服务进程退出后按现有的 SQLite 恢复边界处理。
