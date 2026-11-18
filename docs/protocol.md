# Fliuno Application Protocol 0.1

## 传输

本机 stdio，每行一个 UTF-8 JSON-RPC 2.0 值；支持 batch 和 notification。所有业务方法使用命名对象参数；未知参数拒绝。单个输入帧最多 1 MiB。 单批最多 128 项，包含通知。超限时返回 `-32600`，整批不执行。客户端可从 `capabilities.maxBatchItems` 读取限制。进程不监听 TCP，不把本机调用接口当作已认证的远程 API。

连接先 `initialize`，版本必须匹配 `0.1`。返回 `capabilities` 供客户端决定交互，不应由客户端猜测支持情况。初始化状态属于连接；工作区、会话、任务、审批和产物属于数据库。

`capabilities.registrationAuthority:true` 表示工具审批以 `Approval.authority`、外部 Agent 任务以 `Task.backendAuthority` 绑定具体注册摘要；配置变化或旧记录缺少摘要时拒绝执行，需要新建任务。摘要由宿主生成，客户端不得以传入摘要的方式选择或刷新授权，详见[注册身份与 v8 迁移](authority.md)。

宿主命令行指定的 MCP/ACP/A2A/exec/terminal JSON 各最多 1 MiB，全部先经过有界读取与反序列化，再打开数据库和注册适配器。加载诊断只返回种类和 JSON 行列位置，不回显配置内容；语义校验仍由各适配器负责，见[启动配置边界](host-configuration.md)。

## 方法

| 方法 | params | result |
| --- | --- | --- |
| `initialize` | `{protocolVersion, clientName}` | 服务身份与能力 |
| `workspace/list` | `{after?:0,limit?:100}` | Workspace ObjectPage |
| `session/list` | `{workspaceId,archived?,after?:0,limit?:100}` | Session ObjectPage，可按归档状态筛选 |
| `task/list` | `{sessionId,after?:0,limit?:100}` | Task ObjectPage |
| `task/listSummaries` | `{sessionId,after?:0,limit?:100}` | TaskSummary ObjectPage。与完整列表共用创建顺序和游标 |
| `workspace/open` | `{root}` | Workspace；根目录必须存在 |
| `session/create` | `{workspaceId,title?,clientRequestId?}` | Session |
| `session/get` | `{sessionId}` | 单个 Session |
| `session/rename` | `{sessionId,title}` | 持久化标题并返回 Session |
| `session/setArchived` | `{sessionId,archived}` | 设置持久归档标记并返回 Session |
| `session/context` | `{sessionId,options?:{mode?,maxTurns?,maxBytes?,selection?,taskIds?}}` | TaskContext；预览当前历史选择 |
| `task/start` | `{sessionId, input, clientRequestId?, backend?, context?:{mode?,maxTurns?,maxBytes?,selection?,taskIds?}}` | Task；默认模型；可选 `{type:"external_agent",agentId}` |
| `task/get` | `{taskId}` | Task |
| `task/checkRemote` | `{taskId}` | `{checkId}`；后台只读核对，结果经事件返回，见 [A2A](a2a.md#显式核对远端状态) |
| `task/continueRemote` | `{taskId,input,clientRequestId}` | 独立的本机 Task；后台复核后向原远端任务或上下文提交，见 [A2A](a2a.md#显式续接远端任务) |
| `task/cancel` | `{taskId}` | Task；重复取消可安全调用 |
| `task/resume` | `{taskId,retryUncertainModelStep?:false}` | Task；恢复后排队；外部未知结果不能重试；模型结果不确定时须显式传 true |
| `approval/listPending` | `{workspaceId?,sessionId?,after?:0,limit?:100}` | `{items,nextAfter,hasMore}`，只包含当前待审批的摘要 |
| `approval/get` | `{taskId,approvalId}` | `{kind,approval}`，包含指定待审批项的完整请求 |
| `approval/respond` | `{taskId, approvalId, decision}` | Task；工具执行后排队继续模型步骤；decision 为 `allow_once` 或 `deny` |
| `events/list` | `{taskId, after?:0, limit?:100, targetBytes?}` | `{events, nextAfter, hasMore}` |
| `events/listAll` | `{after?:0, through?, limit?:100, targetBytes?}` | 全任务共享全局序号的事件页 |
| `events/listSession` | `{sessionId, after?:0, through?, limit?:100, targetBytes?}` | 单个会话的持久事件页，沿用全局序号 |
| `events/resubscribe` | `{}` | 在队列溢出后恢复实时通知，返回 `{throughSequence}` |
| `artifacts/list` | `{taskId}` | Artifact 数组 |
| `artifacts/listSummaries` | `{taskId,after?,limit?}` | 按保存顺序分页，返回 `{items,nextAfter,hasMore}` |
| `artifacts/get` | `{taskId,artifactId}` | 单份完整 Artifact |
| `artifacts/read` | `{taskId,artifactId,after?,limit?}` | 返回 `{offset,nextOffset,contentBytes,dataBase64}` |
| `tools/list` | `{}` | 工具名称、描述、效果类型、输入 Schema |
| `agents/list` | `{}` | 注册 Agent 的 ID、协议、隔离类型和 `workspaceAccess`，不返回启动命令 |
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

`initialize.capabilities.sessionCreateIdempotency` 表示 `session/create` 接受 `clientRequestId`。键由 1 到 128 个可打印且不含空格的 ASCII 字符组成，在工作区内唯一。连接中断后用原键和原始标题重试会返回同一个会话的当前信息。复用键并更改原始标题会返回 `-32009`。创建记录和键同事务保存，重启后仍可重试。

`initialize.capabilities.sessionArchiving` 表示支持会话归档。`session/list` 省略 `archived` 时列出全部会话，传 `true` 或 `false` 可按状态筛选。会话对象省略 `archived` 字段时视为 `false`。`session/setArchived` 可以重试；它只更新显示状态，任务、历史和会话 ID 都可照常使用。翻页期间应沿用相同筛选条件。归档状态变动后，从 `after:0` 重拉列表即可反映当前状态。

客户端可通过 `initialize.capabilities.artifactSummaries` 探测轻量产物接口。清单项包含 `id`、`taskId`、`title`、`mediaType` 和正文的 UTF-8 字节数 `contentBytes`。首次请求使用 `after:0`，之后以同一任务的 `nextAfter` 翻页。每页最多 100 项。使用 `artifacts/get` 按任务和产物 ID 读取正文。

客户端可通过 `artifactByteReads` 和 `maxArtifactReadBytes` 探测分块读取。从 `after:0` 开始，每次以返回的 `nextOffset` 续读。默认读取 32 KiB，每块最多 64 KiB。对 `dataBase64` 做标准 Base64 解码并拼接字节后，再按 UTF-8 解码正文，因为块边界可能切开一个字符。`nextOffset` 等于 `contentBytes` 时已经读完。处于末尾的请求返回空块，超过末尾的偏移返回 `-32602`，产物不属于指定任务时返回 `-32004`。同一产物的偏移在服务重启后仍可使用。

`initialize.capabilities.pendingApprovalInbox` 表示客户端可以读取待审批列表。省略两个范围参数时列出整个数据库的待审批项，传 `workspaceId` 或 `sessionId` 可缩小范围，两者只能选一个。每页最多 100 项，按加入列表的顺序返回。可沿用同一范围的 `nextAfter` 翻页或在重连后读取新项。从 `after:0` 读取可以重新核对整份当前列表。

每项包含 `id`、`taskId`、`sessionId`、`kind`、`allowOnceAvailable` 和 `expiresAtUnixMs`。`kind` 为 `tool_call`、`agent_permission` 或 `host_tool`。工具审批没有到期时间。若 `initialize.capabilities.pendingApprovalDetails` 为 `true`，客户端可通过 `approval/get` 查看单项原请求，再用 `approval/respond` 提交决定。已解决、过期或属于其他任务的项返回 `-32004`。提交时仍以当时的任务状态、审批期限、工具注册和策略为准。已撤销、解决或过期的审批会从列表移除。服务重启后保留模型工具审批，已丢失的 Agent 运行结束时清除相应审批。

审批不提供“允许全部”模式。未来可新增策略接口，但不得把模型或 MCP 声明的工具注解当作授权来源。

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

模型可提供额外 argv（最多 256 项、与固定前缀合计最多 32,000 UTF-8 字节）、UTF-8 stdin（最多 256 KiB）、较小的 timeout（配置 100..300000 ms）、较小的输出上限（配置 1024..65536 bytes/stream）。输入交付后关闭 stdin 写端，让等待 EOF 的程序继续运行；停止时中止未完成输入。stdout/stderr 并发排空，每条仅保留末尾有界原始字节；有效 UTF-8 返回文本，否则返回 base64，并报告总见字节、保留量和截断标记。非零退出是已知结果，报告 exitCode 和 success=false。

超时、取消、输出读取/退出观察失败或清理结果不完整会记录 `tool_outcome_unknown` 及有界部分报告，任务进入 interrupted 并保留 `inFlightTool`，不能通过 task/resume 重跑。取消先提交而 worker 后完成时，任务仍为 cancelled，但匹配的部分结果作为只追加审计事件持久保存。`stdinComplete` 要求全部写入及异步 shutdown/flush 成功，且本地写端已释放；不表示子进程实际读取。stdin 失败但退出和输出完整时，仍可返回已知结果，`success=false`，并保留失败阶段。一次审批授权整条注册程序运行；`trusted_host` 具有配置宿主的完整 OS 权限，因此不能把它当作低权限沙箱。

进程报告的 `evidence` 包含原生启动耗时、首次停止原因、停止请求结果、根进程回收、各管道关闭和有界 IO 错误。总 deadline 从原生启动之前开始；若同步启动返回时预算已耗尽或已经取消，先释放尚未轮询的 stdin 再清理，不继续写入。清理另有最多 2 秒的排空预算，因此 `elapsedMs` 可以超过 `timeoutMs`；同步原生启动本身不能由此定时器抢占。字段定义与真实探针见 [执行证据](execution-evidence.md)。

`process_execution` 是前台有界命令工具；模型不会经此工具获得跨轮次 terminal ID。独立 PTY 生命周期、主机配置和存储契约见下节。MCP/ACP Agent 没有拿到通用 shell 或终端能力；macOS OS sandbox 与用户可调文件/网络 profile 仍待建设；Linux 有委派 cgroup 时提供独立 guardian，无委派时仍只有进程组的尽力清理。

### 跨轮次终端

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

每个终端由独立于 app-server 的 supervisor OS 进程拥有。主机原子发布完整的创建预留记录后才启动 supervisor；PTY child 尝试前的明确错误能安全失败，进入不确定阶段的错误会保留 terminal ID 和审计结果。Windows Job 在子命令创建前挂到 supervisor，杀掉或异常结束 supervisor 时 OS 回收 PTY 进程树；重启 app-server 后可通过私有 named-pipe IPC 查询状态或在同一 session 创建新的获批操作，不会 relaunch 旧进程。Linux 有可写委派 cgroup v2 时，独立 guardian 在 worker 退出后回收包含 setsid 后代的进程树并应用 preset 资源预算；它在 app-server 退出后仍持续监控 worker。新的 Linux hard-crash 行为探针仍待 Ubuntu CI 执行确认。无 cgroup 委派且未配置预算时，Linux 使用进程组的尽力清理；macOS 目前仅有进程组行为。IPC 是版本 1 的本地长度前缀 JSON frame，每帧最多 128 KiB，有随机 capability token 和 session ID 检查；token 与 endpoint 保存在受限 state directory，PID 只是诊断信息而不是信任依据。

PTY 输出按字节追加到 SQLite，游标是单调绝对 byte offset；默认每条最多保留 1 MiB，允许 64 KiB..8 MiB。读取每次最多 32 KiB，`dataBase64` 保留 ANSI 控制码、非法 UTF-8 和被截断的 UTF-8 边界。`from`、`next` 指示本页区间，`gap:true` 表示请求游标之前的输出已经按容量删除；客户端应以 `next` 继续读取。20 毫秒生命周期 actor 和有界 native IO 队列让读写不阻塞控制通道，洪泛时仍受 spool 和 channel 容量约束。最多 16 个 IPC 请求并发处理。达到绝对游标上限、spool 磁盘故障或 supervisor 不可达时会显式返回错误/未知状态，不自动恢复或重放副作用。

`terminal/list` 遍历托管根中最多 128 项以找到最多 64 个 terminal record；根目录是 Fliuno 私有服务状态，不应混放用户文件。离线快照不会通过 PID 或锁文件推断进程仍活着，未明确结束的记录显示 `outcome_unknown`，并保留最后写入的 spool。Windows hard-crash 可由 Job 验证后代回收；Linux/macOS 当前无法仅从此离线快照证明进程已经停止。

初始化能力 `sessionContext:true`、`contextTaskSelection:true`、`maxContextTurns:32`、`maxContextBytes:262144` 表示支持跨任务历史与显式任务选择。`context`/`options` 默认 `{mode:"session",maxTurns:16,maxBytes:131072}`；有效范围 1..32 条和 1024..262144 UTF-8 字节，非法值返回 -32602。`mode:"isolated"` 使本任务不读取历史，完成后仍可成为后续任务的来源；另建会话可隔离工作范围。

`Task.context` 保存 `{options,throughSequence,turns,omittedTurns}`；turns 包含来源任务 ID、完成事件序号、后端、中性消息、产物元数据与宿主操作。默认选择同会话已知完成工作的连续最近后缀，超额按完整任务省略。预算包含序列化快照元数据，历史提示前缀另计。任务创建时冻结；预览不预留输入，提交返回的快照才是实际输入。重试/恢复/重启不重新选择历史；`Task.messages` 仍仅为当前任务。旧记录 context 为 null。详见 [上下文设计与预算](context.md)。

可选 `taskIds` 显式指定 1..maxTurns 个不同、同会话已完成记录 ID，每个为 1..128 可打印 ASCII 字节且无空格，只能用于 session 模式。输入 ID 按字典序规范化，历史仍按完成顺序；交换同一集合的输入顺序不改变幂等身份。不存在、其他会话和未完成记录统一返回 -32602；完整快照超预算也整体拒绝，不丢弃指定记录、不保存失败创建的任务/事件/幂等键。`omittedTurns` 为该会话已完成但未选中总数。数据库 v9 引入此冻结输入字段，v8 升级只提升版本号并保留旧快照。

历史作为 JSON 文本数据交给后端，不继承审批、执行身份或供应商签名续接，也不递归加入来源 context；新的模型或 ACP 操作仍独立授权。当前实现是有界选择，没有摘要压缩或模型 token 窗口自动管理。

外部 Agent 通过 `--acp-config` 注册，初始化能力 `externalAgents` 表示注册情况。任务的 `backend` 和 `agentRun` 持久化；外部权限在 `agentRun.pendingApprovals` 数组中，最多同时 16 项，与内置工具的 `pendingApproval` 分开。`approval/respond` 同时支持两种审批；拒绝外部权限交回 Agent，不直接终止任务。`agent_started`、`agent_session_opened`、`agent_update`、`agent_approval_requested/resolved/closed`、`agent_finished` 均进入同一事务事件日志。决定先持久化再交付，resolved 仅表示决定已记录，不承诺远端收到。Agent 任务与模型/工具共用执行名额，等待权限仍占名额。

`AgentApproval.source` 区分 `agent_permission`（Agent 自身权限，旧记录的默认值）与 `host_tool`（Fliuno 实际执行的宿主工具）。两者共用审批接口和 16 项容量；Agent 权限允许不能替代宿主工具审批。`host_tool.request` 为 `{call,preview}`，读请求的 preview 为 null，写请求包含 `{path,diff,createsFile}`。客户端展示已保存的参数与 diff，仍只提交审批 ID 和决策。

`agentRun.hostTools` 保存宿主操作的 ID、不可替换的 call、status、可选 output/error；旧快照默认空数组。状态为 `awaiting_approval`、`approved`、`running`、`succeeded`、`denied`、`failed`、`outcome_unknown` 或 `closed`。`agent_host_tool_requested`、`agent_host_tool_started`、`agent_host_tool_completed` 写入同一事件日志。允许决定先提交，执行前另行提交 started；结果提交成功后才向 Agent 返回文件内容或写入成功。运行中操作在断线/重启时保留未知结果，Agent 的正常结束不能将其覆盖。

主机配置 `workspaceAccess` 默认 false；开启时仅提供虚拟工作区内的文本读写代理，terminal 仍关闭。每个文件操作独立审批，单次文件/内容最大 256 KiB；每运行最多 8 个未完成文件请求、64 个累计宿主操作。写提案读取基线供客户端生成 diff，执行时复核；`baseSha256:null` 仅创建新文件。相关兼容字段在 v6 引入，当前数据库版本为 v10；可读取 v3 中缺少 `source`/`hostTools`、v4 中缺少 `context` 和 v5 中缺少后续事件兼容字段的记录，并事务回填已完成任务的历史索引；不改写旧输入和事件，旧二进制应拒绝新版本。

已启动的 Agent 运行不支持 `task/resume`，即使结果已知或传入模型重试同意也不能重放。服务退出或重启使待审权限失效，活动 Agent 任务进入 interrupted；取消任务保持 cancelled。运行 ID、`outcomeUnknown`、`finished` 和 `stopReason` 区分结果未知、执行器结束和已知但不完整的响应。配置、预算及完整状态映射见 [ACP 文档](acp.md)。

模型步骤和工具执行由 4 个固定 worker 并发处理，单个任务同时最多只有一个在途步骤；队列容量为 32，因此最多接受 36 个活动或排队的步骤。初始化能力会报告 `concurrentExecution: true` 和 `maxConcurrentExecutions: 4`。`backgroundTasks` 和 `taskCancellation` 为 true。`task/start` 和 `task/resume` 返回持久化后的任务快照并调度模型步骤；`approval/respond` 持久化审批决定和工具执行身份后即返回，工具执行完成后会自动排入下一模型步骤。客户端应通过 `task/get` 或事件通知观察后续状态；容量耗尽时任务以 `failed` 及原因结束，原幂等键仍绑定该任务。

模型和工具调用期间数据库锁已释放，因此查询和取消可以继续处理。取消会立即将任务置为 `cancelled`，并通知当前 provider 或工具；provider HTTP 等待和 MCP 调用会响应取消，迟到结果会被忽略。取消不保证阻止供应商计费或撤销已发生的外部副作用。工具调用身份在执行前持久化，重启后结果未知的调用不能自动重试。stdin 关闭后停止启动排队步骤并取消当前执行，再把未完成任务置为 `interrupted`。

支持 `sessionEventReplay` 的客户端可用 `events/listSession` 回放某一会话内各任务的持久事件。它沿用全局的 `sequence`，所以相邻记录的序号可能不连续。使用 `nextAfter` 翻页，并可传入包含式 `through` 固定回放上界。有固定上界时，最后一页即使没有事件，也代表该会话已回放到上界。实时通知继续共用全局事件订阅。

实时 provider 可在模型响应完成前发送 `notifications/modelProgress`，参数为 `{progress}`。`progress` 是带 `type` 的联合类型：`text_delta` 包含 `taskId`、`step` 和文本片段，`reset` 清除该步骤尚未确认的预览。它不写入 SQLite、没有持久序号，且可能与 `notifications/event` 相对乱序。客户端应按 task/step 暂存增量；收到该步骤的审批请求或任务完成、失败、取消、中断等持久状态后，忽略迟到增量并以任务快照和持久事件为准。取消、错误、工具调用会清除预览。`initialize.capabilities.modelProgressNotifications` 表示当前 provider 是否支持此通知。慢订阅者被分离后会收到 `notifications/modelProgressGap`（参数为空）；预览不回放，客户端应清除预览并查询活动任务状态。如果 `capabilities.modelProgressResubscribe` 为 true，收到缺口通知后可用 `modelProgress/resubscribe({})` 在同一连接恢复，成功返回 `{subscribed:true}`。新订阅只接收注册之后开始的模型步骤。正在执行的步骤可通过持久事件或 `task/get` 查看结果。

初始化后，服务会发送 `notifications/event`，参数为 `{event}`；每个事件已先提交 SQLite，再进入容量为 64 的有界内存订阅队列。实时通知只覆盖本次进程连接期间的新事件，不补发历史。客户端重连时用 `events/listAll` 从持久游标回放，并按全局 `sequence` 去重，以弥合订阅和回放之间的竞态。

若客户端消费过慢导致订阅队列溢出，服务先排空已排队通知，再发 `notifications/eventGap`。参数 `{afterSequence}` 是本连接最后成功写出的序号。客户端从自身已提交的游标调用 `events/listAll` 补齐。SQLite 日志是事实来源，通知可能丢失。事件按 `sequence` 排序，`after` 为严格大于且须在非负 i64 范围，`limit` 在 1..1000。客户端保存 `nextAfter`、处理 `hasMore` 并按序号去重。

`initialize.capabilities.eventReplayTargetBytes` 表示两个回放方法支持的最大字节目标。客户端可设置 `targetBytes` 为 1024..8388608，按紧凑 JSON 的 `events` 数组大小分页。单个事件已超过目标时，该页完整返回该事件，游标仍会前进。

支持 `eventResubscribe` 的连接收到 `notifications/eventGap` 后，可用 `events/resubscribe` 重新订阅。先暂存恢复后的实时通知，再从本地游标调用 `events/listAll`，传入返回的 `throughSequence` 作为包含式 `through`。回放结束后按序号处理暂存通知。服务在同一内核锁下采集水位并建立订阅，实时通知从该水位之后开始。若实时队列再次溢出，可重新建立订阅并从已保存游标补齐。订阅仍有效时重复调用返回 -32009。

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

## A2A 远程 Agent 配置

远程 Agent 通过独立的 `--a2a-config` 注册，选择方式沿用 `task/start.backend` 的 `external_agent`。`agentRun.remoteTask` 保存稳定的远程 id、contextId 与中性 state；状态变化以 `agent_remote_task_updated` 事件回放。`agentRun.artifacts` 记录远端 sourceId 与本地 artifactId，内容仍通过 `artifacts/list` 查询。新增 `failed` / `rejected` / `input_required` / `auth_required` 结束原因；详细状态映射、配置、取消取证和 v7 迁移见 [A2A 任务与产物](a2a.md)。这些字段默认空，现有 ACP 任务语义不变。

## MCP 主机配置

stdio 服务器必须配置 `isolation`；HTTP 服务器不使用该字段，使用下文的 `streamable_http` 配置。同一份 servers 列表可混合两种传输。

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

`timeoutMs` 可选，默认 30000，取值范围 100..300000 毫秒，分别限制初始化、发现与每次调用；调用预算包含等待同一主机前序调用的时间。排队时的取消或等待超时返回已知失败，不向服务器发送请求，也不关闭正被其他任务使用的主机。请求发送后超时、取消或协议失败会关闭该 MCP 主机并将外部结果标记为未知；重启 Fliuno 后才会重新连接，内核不会自动重放该调用。子进程在 Unix 上放入独立进程组，在 Windows 上放入 Job Object，关闭时回收其进程树。工具别名稳定且限定为模型 API 可接受的 ASCII 名称，原始名称由适配器保存。stdio 生命周期、版本协商与消息格式由官方 `rmcp` 实现；HTTP 配置、生命周期、预算和支持边界见 [MCP 传输文档](mcp.md)。当前不向服务器提供模型采样或用户交互能力。

stdio 的 `isolation` 必填。`trusted_host` 使用宿主用户权限，必须指定 `cwd`。Windows 可选择 `windows_app_container`，Linux 可选择 `linux_landlock`；隔离模式必须省略 `cwd`。两者都在新建私有目录中启动单个程序，不授予 workspace 路径，并在隔离设置失败时拒绝启动。Linux 要求 Landlock ABI 6，系统运行目录只读开放，seccomp 禁止 socket 操作；当前 cgroup v2 有可写委派时，sandbox 将进程放入私有 cgroup，并用 `cgroup.kill` 回收含 `setsid` 后代的整树。资源预算依赖已委派的 domain cgroup 和 `cpu`/`memory`/`pids` 控制器；通过 `linuxCgroupParent` 指定时，Fliuno 在该目录下创建私有子 cgroup 并设置所请求的 `memory.max`、`pids.max`、`cpu.max`，任何一项不可用都会拒绝启动。Windows 默认 Job 上限为 16 个活动进程和 512 MiB commit，可由 `resources` 覆盖；CPU quota 仅支持 Linux。Linux sandbox 的独立 guardian 通过 pidfd 监控宿主进程整体，宿主退出或 cgroup 所有者 Drop 后回收；创建它的线程退出不会终止任务。子进程在 exec 前完成 cgroup 加入和 pidfd 交接确认，guardian 不可用时拒绝执行。Windows 系统共享资源仍受既有 ACL 控制。解释器伴随文件通过宿主固定摘要的 `assets` 清单提供；MCP workspace 代理尚未实现，见[伴随文件](runtime-bundles.md)。其他平台请求不支持的隔离模式会明确失败，绝不退回宿主权限。

```json
{"servers":[{"id":"isolated","isolation":"windows_app_container","command":"C:/absolute/path/to/native-mcp.exe","timeoutMs":30000}]}
```

Linux 示例（路径须由宿主预先委派，并在 `cgroup.subtree_control` 启用这些控制器）：`{"servers":[{"id":"isolated","isolation":"linux_landlock","command":"/usr/bin/example-agent","resources":{"memoryMaxBytes":536870912,"pidsMax":64,"cpuQuotaMicros":50000},"linuxCgroupParent":"/sys/fs/cgroup/fliuno-delegated","timeoutMs":30000}]}`。ACP `agents` 配置和 `--exec-config` 的每个 `programs` 条目接受相同的 `resources` 与 `linuxCgroupParent` 字段；宿主配置固定这些权限，模型请求无法覆盖。

隔离进程先暂停创建，加入 Job Object 后再恢复；Job 限制为 16 个进程、总提交内存 512 MiB。继承句柄仅允许 stdin/stdout/stderr 三条管道。关闭、初始化失败、超时与取消都会回收该进程树与私有目录。审批仍绑定每次工具调用，OS 隔离不替代审批策略。

## 内置工具与作用域

`tools/list` 的效果分类包括 `artifact_write`、`workspace_read`、`workspace_write`、`skill_read`、`process_execution`、`terminal_control`、`external`。分类由主机工具实现提供；默认所有调用都请求一次审批。客户端审批只提交审批 ID 和决策，不提交新的工具参数或工作区。

- `workspace_list({path})`：工作区内目录，根目录用 `.`；返回最多 256 项及 truncated。
- `workspace_list_page({path,after?,limit?:128})` 按文件名的 UTF-8 字节顺序返回 `{entries,nextAfter,hasMore,omittedNames}`，每页 1..256 项。目录不变时，按 `nextAfter` 续页可浏览全部可寻址条目。文件名无法作为工作区相对路径片段时，计入 `omittedNames`。每次扫描最多 16,384 项，超限返回错误。分页期间目录可以变化，游标只表示名称边界。每页按工作区读取工具走审批。
- `workspace_read_text({path,line?,limit?})`：读取最多 256 KiB UTF-8 文件；line 从 1 开始，limit 是最大行数，均须为正整数。按行截取保留原换行，SHA-256 始终针对完整文件；超出末行返回空文本。
- `workspace_read_bytes({path})`：读取最多 256 KiB 原始字节并返回 `{path,encoding:"base64",data,bytes,sha256,grantsPermissions:false}`。长度和摘要针对原始字节；不检测 MIME、不解码格式、不执行，也不自动创建 artifact。该读取和所有其他工具调用一样，每次需要审批；拒绝审批不会返回文件数据。
- `workspace_create_text({path,content})`：创建新文件，父目录须存在，目标须不存在。
- `workspace_patch_text({path,baseSha256,patch})`：对已存在文本文件应用一个 unified diff。diff 的 `a/` 与 `b/` 路径必须与 `path` 一致，且必须是单文件修改；`baseSha256` 必须等于最近读取的内容哈希。审批记录包含完整 diff，批准前不修改文件。执行时通过打开的文件句柄复核基线，以不跟随符号链接方式打开最终文件，拒绝多硬链接目标。本进程内的新建和补丁会串行化；其他进程不受此互斥锁约束，因此外部并发编辑不具备事务性保证。写入错误或同步错误标记为结果未知，中断任务且不会自动重试。
- `workspace_replace_text({path,content,baseSha256})`：全文替换或创建；baseSha256 必须提供，既有文件须匹配小写 SHA-256，null 仅允许新建。与补丁共享最终文件的符号链接/硬链接检查以及写入锁；保留权限的原位更新不构成针对外部进程的原子 CAS。

读取、补丁基线和全文写入预览共用普通文件检查、实际字节预算和逐块取消；不能通过按行参数读取超额文件的局部内容来绕过 256 KiB 限制。Unix 以 O_NONBLOCK 打开已有文件后检查描述符，拒绝 FIFO 等特殊文件；该标志不保证普通存储 IO 可在固定时间内中止。读取失败或取消不返回部分成功；尚未开始写入时不会报告已修改。
- `skills_list({})` / `skills_read({name})`：需主机通过 `--skills-dir` 配置技能目录。
- `skills_list_resources({name,path?:""})` / `skills_read_resource({name,path,encoding?:"utf8"|"base64"})`：单技能能力内的单层列举和有界文本/原始二进制编码；完整边界见[Skills](skills.md)。

路径使用相对的正斜杠形式。绝对路径、`..`、反斜杠、ADS、Windows 设备名等被拒绝，目录能力继续约束实际路径解析。

对象分页返回 `{items,nextAfter,hasMore}`，对象列表 limit 在 1..100；after 是数据库局部游标，不应解析其数值意义。`clientRequestId` 为会话内 1..128 可打印 ASCII 字节；重试必须使用相同输入、后端与上下文选项。新任务省略 context 和显式默认选项等价；升级前 context=null 的旧任务仅接受未显式指定 context 的原样重试。没有提供幂等键的 task/start 每次都创建新任务。
