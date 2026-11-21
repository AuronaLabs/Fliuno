# MCP 传输与边界

[文档中心](README.md) › [模型、工具与 Agent](README.md#模型工具与-agent) · [运行与配置](setup.md#mcp) · [资源与提示](mcp-content.md)

`fliuno-mcp` 使用官方 Rust `rmcp` SDK，将主机配置的 MCP 工具接入 Fliuno 工具注册表。工具发现时生成稳定的模型侧别名；请求仍经过参数 Schema 校验、持久化审批、执行身份记录和结果提交。MCP 工具标记为外部效果，工具注解和服务器能力不会授予权限。

[传输配置](#传输配置) · [出站策略](#出站策略) · [生命周期与协商](#生命周期和版本) · [工具参数与结构化结果](#工具参数和结构化结果) · [预算与失败](#预算和失败语义) · [MCP Tasks](#mcp-tasks)

## 传输配置

每个服务器显式选择传输。stdio 保留原有配置格式，也可写 `"transport":"stdio"`。HTTP 示例：

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

服务器 ID 是 1..16 个 ASCII 字母、数字或下划线；单份配置最多 32 个服务器。HTTP URL 最多 4096 字节，只接受 HTTPS，或主机名为字面 IP 回环地址的 HTTP。拒绝 userinfo、fragment、普通明文远端 HTTP。客户端不跟随 HTTP 重定向，不使用系统代理环境变量，也不自动重试请求。

`bearerTokenEnv` 指向只含裸 token 的环境变量，内核添加 `Bearer` 前缀。`headersEnv` 将自定义头名映射到环境变量名，可用于服务商 API key。头名和值在送入 reqwest 前校验；传输控制头、重复头、空值、超长值和超过 16 KiB 的总凭据头被拒绝。凭据不进入配置持久化、Task、Event、Artifact 或错误正文。缺失凭据会在网络请求前失败。需要用户授权的 Streamable HTTP 服务可使用下文的 OAuth 接口。

可信 stdio 使用宿主配置的绝对 `cwd`，保留宿主用户的文件和网络权限。Windows 可用 `resources.pidsMax` 和 `resources.memoryMaxBytes` 限制 Job。Linux 可指定 `linuxCgroupParent` 并配置进程或线程数、内存和 CPU 预算。Linux 无法应用指定 cgroup 或预算时拒绝启动。详细范围见[进程启动](sandbox.md)。

### 出站策略

HTTP 服务器可在自己的配置内指定 `egress`；适用于两代 Streamable HTTP、现代短时订阅和旧 HTTP+SSE。下例使用文档示例地址，实际使用时换成宿主确认的服务地址：

```json
{
  "id": "remote",
  "transport": "streamable_http",
  "url": "https://mcp.example.test/mcp",
  "egress": {
    "routes": [
      {"host": "mcp.example.test", "addresses": ["192.0.2.10", "2001:db8::10"], "ports": [443]}
    ]
  }
}
```

启用后，每个需要连接的已配置主机都必须有路由，并明确授权每个目标端口。最多 16 个主机；每个最多 8 个地址和 8 个端口。主机名使用 URL 规范形式（小写 ASCII；IPv6 字面地址带方括号），不支持通配符。字面 IP 只能绑定自身。域名连接只尝试列出的地址，不查询系统 DNS，也不会在失败后切到其他地址。请求仍使用 URL 的端口和域名，HTTPS 正常校验证书。配置的主机和端口不会扩大 URL、同源检查或 MCP 方法允许的范围。省略 `egress` 的既有配置继续使用系统 DNS。

OAuth 沿用同一策略，须为资源、授权发行方、额外授权端点来源和已配置的客户端元数据 URL 补齐路由。宿主打开的登录浏览器独立运行，Fliuno 不改变其连接策略。出站配置属于工具审批和加密授权记录的绑定身份；更改后，旧的待批调用不能运行，旧授权也不会接到新策略下。

此通道由父进程的 MCP 适配器发起；工具参数不能指定任意网络目标。隔离 stdio 不接受此配置，不继承套接字或创建通用代理；`trusted_host` 的网络权限仍由宿主系统决定。

## 生命周期和版本

Streamable HTTP 和 stdio 的 `lifecycle` 默认均为 `auto`，先用 `2026-07-28` 的 `server/discover` 探测。Streamable HTTP 兼容 `2025-03-26`、`2025-06-18` 和 `2025-11-25` 的 initialize 生命周期。收到 HTTP 400 时先检查正文：普通旧版拒绝可触发一次回退，默认提出 `2025-11-25`；对端明确只支持共同的旧版时，使用其中最新的共同版本握手。对端声明支持现代版本时仍按现代协议协商，缺失必要能力或标准头校验失败会报错。认证失败、限流、服务器错误、超时或连接断开不触发回退。详见[官方传输兼容规则](https://modelcontextprotocol.io/specification/2026-07-28/basic/transports/streamable-http#backward-compatibility)。

已弃用的 [`2024-11-05` HTTP+SSE](https://modelcontextprotocol.io/specification/2024-11-05/basic/transports#http-with-sse) 需要显式设置 `"transport":"http_sse"`，并将 `url` 指向 SSE GET 地址。它从 `endpoint` 事件取得同源 POST 地址，再用 `initialize` 握手；`lifecycle` 可用 `auto` 或 `legacy`。对端不能更新 POST 地址或把请求和凭据重定向到别处。断开流会结束连接，已发送的工具请求按结果未知处理；不会自动重连、重发或回退到此传输。

stdio 的 `auto` 可以在收到旧端错误响应后回退；若探测后旧进程已关闭，会用原命令、环境和隔离配置最多重启一次，再发送 `initialize`。`legacy` 跳过探测，`discover` 要求现代端。上述行为只用于启动握手；工具请求发出后不会因协商或重连被重放。

现代 HTTP POST 携带 `MCP-Protocol-Version`、`Mcp-Method` 和 JSON-RPC 请求元数据；工具调用还携带 `Mcp-Name`。若远端工具输入 Schema 使用 `x-mcp-header` 注解，SDK 会按 MCP 标准将对应原始参数值编码为 `Mcp-Param-*` 头。标准头由 `rmcp` 从请求生成，配置不可覆盖。客户端接受 JSON 与 SSE 响应。当前锁定 `rmcp 3.11.0`，其 SDK 负责协议编码和生命周期协商。

新版工具、提示、资源和资源模板列表及普通资源读取可按 `ttlMs` 和 `cacheScope` 在当前连接内复用。缓存最多保留 8 项，存活期上限 5 分钟；未设 TTL 或 TTL 为零时直接请求远端。新连接不继承缓存，包括标记为 `public` 的响应。收到目录或资源更新通知后，相应缓存项失效；刷新失败时不返回过期内容。读取仍需逐次审批，资源导入和工具执行直接请求远端。

## 工具参数和结构化结果

在现代连接上，工具 Schema 未声明 `$schema` 时使用 JSON Schema 2020-12；也接受显式声明的 Draft 4、6、7 和 2019-09。输入 Schema 根须声明 `type: "object"`。支持组合、条件、`$defs` 和本地片段引用；不访问网络或文件来解析引用。校验拒绝递归和动态引用，并使用不依赖回溯的正则引擎。单个 Schema 最大 64 KiB，Schema 和校验数据最大深度 32；还限制节点数和引用展开次数。每次编译或校验最多等待 500 毫秒，队列满时立即返回错误。已经开始的校验可以在受限线程中结束，超时后的结果不会被采用。

参数不符合 Schema 时不会发送工具请求。若工具声明了 `outputSchema`，成功返回的 `structuredContent` 必须存在并符合声明。结构化输出可以是数组、标量或显式 `null`；只有不含结构化输出的 `isError: true` 响应可以省略它。输出契约纳入审批身份。已派发的工具返回不合规数据时，任务保留结果未知证据，不保存无效正文或自动重发调用。旧版连接保持原有输入校验和输出兼容行为。现代 Tasks 的 `get` 和 `wait` 使用发起工具的同一输出契约校验最终结果。

## 预算和失败语义

- 每服务器最多一个并行 tools/call；最多排队 16 个 HTTP 传输消息。
- `timeoutMs` 范围为 100..300000，默认 30000。连接超时最多 10 秒；SDK 控制请求最多 5 秒；初始化、发现和工具调用遵循主机预算。
- 一条 SSE event 和一条完整 JSON 响应最多 1 MiB；工具输出序列化后也最多 1 MiB。工具发现最多 32 页、256 个工具及 4 MiB 累计序列化页面数据；工具描述最多 4 KiB，单个 Schema 最多 64 KiB。每页及 SDK 已解析对象仍另有短暂分配，因此累计发现预算不是总进程 RSS 上限。
- 排队期间取消或超时在发送前返回已知失败，不关闭正在处理其他调用的主机。
- 请求发出后的网络错误、超时、取消、协议失败和 HTTP 超限响应都按结果未知处理，并关闭共享主机。内核保留 `inFlightTool`，重启不会重新发送该调用。协议层完整结果在序列化后超过工具输出预算也保留未知标记。
- MCP `tools/call` 返回 MRTR `input_required` 时，内核只接受受限的表单 elicitation 请求。任务进入 `waiting_input`，宿主展示请求并显式提交 `task/respondToolInput`；内核校验每个响应后，将原工具参数、响应及原 `requestState` 交回同一 MCP 服务。用户取消或拒绝表单由远端工具按协议处理。
- sampling、roots 和 URL elicitation 请求不会自动执行或显示为受支持的补答表单；不支持或超预算的请求按结果未知处理，不会自动续接。每个工具调用最多续接 10 轮，请求与响应各最多 256 KiB。
- `isError: true` 是一个完整的 MCP 工具结果，并不代表 HTTP/JSON-RPC 传输失败。

> **审批边界**：用户输入只继续同一个已审批工具调用，不创建新的模型工具调用，也不会重复审批或改写原始参数。续接令牌只保存在内核 SQLite 私有表，不进入 Task、Event 或 RPC 响应；响应幂等摘要也只保存在该表中。

## MCP Tasks

客户端声明 `io.modelcontextprotocol/tasks` 能力。仅当远端在 `2026-07-28` 或更新协议中声明同一扩展时，才注册 Tasks 工具。`tools/call` 返回 `resultType:"task"` 时，Fliuno 将其任务句柄作为该工具的已知结果写入本地任务记录；不支持的对端仍按未知结果处理。

| 工具 | MCP 方法 | 参数 |
| --- | --- | --- |
| `mcp_<id>_tasks_get` | `tasks/get` | `{taskId:string}` |
| `mcp_<id>_tasks_wait` | `tasks/get`（短轮询） | `{taskId:string,waitMs:integer}` |
| `mcp_<id>_tasks_watch`（现代 Streamable HTTP、Windows/Linux 原生 stdio） | `subscriptions/listen`，前后用 `tasks/get` 核对 | `{taskId:string,waitMs:integer}` |
| `mcp_<id>_tasks_update` | `tasks/update` | `{taskId:string,inputResponses:object}` |
| `mcp_<id>_tasks_cancel` | `tasks/cancel` | `{taskId:string}` |

这些操作均绑定远端扩展能力和服务器注册摘要，且每次独立审批。现代 HTTP 对 `tasks/get`、`tasks/update`、`tasks/cancel` 发送与正文任务 ID 匹配的 `Mcp-Name`，并随请求声明 Tasks 客户端能力。

收到句柄后，Fliuno 保存任务 ID、创建时间、工作区、会话、发起调用及原工具输出契约。app-server 使用 SQLite v61 持久化绑定，直接调用适配器或使用内存内核时仍只保存在当前连接。注册配置、已解析凭据、协议版本和对端所报告的实现身份一致时，新连接可按原工作区与会话恢复绑定，并继续用原 Schema 验证结果。每个注册身份最多保存 4096 个句柄；远端重用 ID 或超过上限时按结果未知处理。`get` 和 `wait` 发现最终结果不符合原契约，或缺少本地绑定时，只保留任务 ID 和状态作为未知证据。

`tasks/update` 在同一连接上先读当前任务快照，再提交属于当前待补表单且符合表单 Schema 的响应。Fliuno 不自动执行 `roots/list`、采样或 URL elicitation。每次最多提交 16 个响应，JSON 合计最多 256 KiB；同一任务最多登记 64 个不同的输入请求 ID。app-server 在派发前以事务登记整批 ID，不保存答案；即使进程在派发期间退出或远端仍返回旧表单，重启也不会再次提交已登记的 ID。这一边界覆盖同一数据库的重启；从历史备份启动时，无法推断备份之后的外部执行结果。`tasks/cancel` 也先核对远端状态和原句柄的创建时间；服务器拒绝查询、句柄不匹配或任务已终止时不向远端发送取消。成功结果只表示服务器接受了协作取消请求，需再调用 `tasks/get` 核实最终状态。任务 ID 最多 512 字节且不得包含控制字符，读取结果最多 1 MiB。

`tasks_wait` 立即读取一次，之后遵循服务端建议的 `pollIntervalMs`（至少 100 毫秒，未提供时为 500 毫秒），总计最多 8 次。`waitMs` 限制在 1 毫秒至主机超时的一半，且最多 5 秒；该窗口限制发起下一次查询的时间，已发出的查询仍由主机总超时约束。等待期间该连接继续串行处理请求。返回 `{reason,polls,snapshot}`，其中原因是 `terminal`、`ttl_elapsed`、`input_required`、`wait_budget` 或 `poll_limit`。等待不会补答或请求取消远端任务。

`tasks_watch` 通过 [Tasks 扩展](https://github.com/modelcontextprotocol/ext-tasks/blob/main/specification/2026-07-28/tasks.md) 的 `notifications.taskIds` 订阅当前会话已绑定的一项任务。HTTP 沿用同一端点、凭据和响应预算；stdio 沿用原子进程，并用 `notifications/cancelled` 结束该订阅请求。两种传输均核对订阅 ID、服务端确认的过滤条件、任务 ID 和创建时间。监听结束后如果连接仍可用，重新执行 `tasks/get`，只返回查询快照和 `stream` 状态；stdio 管道断开时报告结果未知。通知正文不进入工具结果。每次等待预算同 `tasks_wait`；结束监听不取消远端任务、不自动重连，也不重放通知。

`tasks/get` 收到远端 JSON-RPC 拒绝时作为已知读取失败返回，并保留连接；发送后的超时、取消或传输错误仍属于结果未知，会关闭该主机，避免后续请求复用不确定的会话状态。

资源、提示、Skills 元数据和 completion 查询使用相同的只读错误语义。

模型或调用方显式选择单次读取、短时观察、补答或协作取消；内核不会在后台隐式轮询，也不会在重启后自动重发 Tasks 请求。读取结果必须匹配请求的任务 ID，且是完整快照。两代 Tasks 都用最新快照的创建时间加 TTL 判断是否停止等待、打开旧版表单或派发补答和取消。服务端仍保有的终态结果可读取。过期绑定保留用于核对 ID，自动清理仍未实现。MRTR 仅支持工具调用内的表单 elicitation 续接，不覆盖其他 server-to-client 请求类型。

[`2025-11-25` 的实验性 Tasks](https://modelcontextprotocol.io/specification/2025-11-25/basic/utilities/tasks) 使用独立的线协议。Windows 和 Linux 的 stdio 以及 Streamable HTTP 支持通过 `initialize` 的 `capabilities.tasks` 和 `tools/list` 的 `execution.taskSupport` 协商。`required` 工具发送 `params.task`，`optional` 工具保留普通调用并增加一个 `_task` 后缀的独立审批入口，`forbidden` 工具始终普通调用。服务必须显式声明 `capabilities.tasks.requests.tools.call`；仅声明空的 `tools` 对象不授权任务化调用。旧 SDK 若省略空的 `ttl`，内核接受该句柄并将该字段规范为 `null`，不推断到期时间。

旧版 `tasks_get` 只查询任务状态，`tasks_result` 可取回终态结果或打开 `input_required` 任务的补答通道，`tasks_cancel` 只在服务声明取消能力时注册。结果和取消要求原工作区与会话的句柄，app-server 可从 SQLite 恢复该绑定；结果须匹配原任务 ID 和原工具输出契约。已过期任务的状态被远端拒绝时，不重发原调用，也不继续发送取消。

旧版补答通过 `tasks/result` 打开的同一请求接收 `elicitation/create`，校验 `io.modelcontextprotocol/related-task` 的任务 ID，再交给宿主通过 `task/respondToolInput` 答复。表单、原工作区和会话、发起任务及调用都必须匹配；每连接最多保留一个补答流程，最多接收 10 个不同的服务端请求 ID。等待最长为配置的 `timeoutMs` 与 120 秒中的较小值。失去原连接后，内核保留已保存的输入记录，并把恢复结果记为未知，不重新请求或发送旧答案。通过官方 Python SDK 1.25.0 验证了 stdio 和 SSE；该 SDK 的 HTTP JSON 模式未送出服务端表单。旧版不使用现代 `tasks/update`。

旧版 `tasks_wait` 同样逐次审批，每次最多查询 8 次，只等待当前会话已绑定的任务。它读取旧版 `pollInterval`，并监听 `notifications/tasks/status`。与原任务 ID 和创建时间都匹配的通知可提前唤醒一次查询；重复和无关通知不会触发额外查询。返回状态只取自 `tasks/get`，通知缺失时继续按查询间隔等待。`notificationReceived` 表示这次等待被匹配通知唤醒，不是远端状态的凭证。断线后不会补放通知或重新发起原工具调用。

## 当前范围

隔离 stdio server 可配置 `assets`，在启动前把宿主列举的脚本、DLL 和数据复制到私有根并检查 SHA-256。工具审批另存完整注册摘要，绑定传输、端点、原生程序内容、参数、实际环境/凭据、预算与远端 Schema；即使展示别名不变，重启后的配置变化也不能复用旧审批。环境和凭据在注册时冻结，传输使用同一份值，见[注册身份](authority.md)。Windows 和 Debian Linux 来宾都已通过真实 Node/CPython 多文件 MCP 的允许、拒绝、篡改流程，命令与局限见[解释器清单](runtime-bundles.md)。MCP 不借此获得工作区代理或网络能力。

除 MCP tools/list 和 tools/call 外，内核也按服务声明接入资源、资源模板、提示和 Tasks 控制，详见[内容通道](mcp-content.md)。这些操作和短时目录与 URI 监听均逐次审批。工具调用内的表单 elicitation 可以通过显式用户输入续接原调用；内容读取不会自动续接。工具执行和内容操作共享排队、取消与失败关闭逻辑。长期订阅、sampling、roots、独立 elicitation、热重连和外部真实服务验收仍在计划中。stdio Windows AppContainer/Linux Landlock 隔离不适用于 HTTP 网络服务器；远程服务器访问由宿主网络身份和凭据决定。

## 宿主 OAuth 接口

Rust 宿主用 `OAuthClient::begin` 准备授权，并在处理浏览器回调后取得 `OAuthGrant`。通过 `register_config_with_oauth` 按服务 ID 注册，或用 `connect_oauth` 建立独立连接。两代 Streamable HTTP 都可使用；同一配置不能混用静态凭据，旧 HTTP+SSE 继续使用原有配置。授权只能发往原 MCP URL；握手、工具和任务请求以及现代任务短时订阅在派发前获取有效凭据。一次授权的注册标识随刷新保持不变，每次新授权产生新标识，已有任务和审批不会因重新登录而交给另一份授权。

app-server 可同时传入 `--mcp-config PATH --mcp-oauth-config PATH`。OAuth 配置单独按 MCP 服务 ID 绑定授权方、回调及客户端策略，不包含令牌：

```json
{
  "servers": {
    "remote": {
      "issuer": "https://login.example.com/tenant",
      "redirectUri": "http://127.0.0.1:8022/callback",
      "clientId": "registered-public-client"
    }
  }
}
```

宿主控制浏览器并接收回调。它在 `initialize` 前经[启动授权 RPC](protocol.md#mcp-启动授权)取得授权链接、交回回调，然后等待 MCP 注册完成。也可配置 `clientMetadataUrl` 使用 CIMD；只有服务未声明 CIMD 支持且配置显式允许 `allowDynamicRegistration` 时才会申请动态注册。`issuer` 及元数据声明的授权和令牌端点受 HTTPS 与来源限制；HTTP 回调只允许字面回环地址。OAuth 配置不能附加到旧 HTTP+SSE、stdio 或带静态认证头的服务。

HTTP 401 会结束本次操作。已经发出的工具调用保留未知结果，宿主可以重启并重新授权，不会隐式重发原调用。

默认授权只在当前进程内有效。可选的 `--mcp-oauth-vault PATH` 使用与业务库分离的 AES-256-GCM 加密凭据库。宿主应在私有目录中配置凭据库文件，并在系统或应用密钥库中保管专用随机密钥。配置、授权方、资源和客户端身份共同决定授权可用于哪个服务；重新登录会替换该服务的授权身份。业务数据库备份不包含凭据库，不应把旧凭据库副本恢复到当前路径，以免复用已消耗的刷新令牌。

同时启动的进程使用条件事务占用刷新凭据：先将旧记录标记为不可用，再联系令牌端点。刷新成功才保存新状态；请求结果未知时要求重新授权。两种情况下都不会重新派发原工具调用。

协议版本矩阵、凭据处理、JSON/SSE、取消、不确定结果、审批/拒绝与重启行为由单元测试、官方 SDK HTTP server 互操作测试和本机 app-server 进程测试覆盖。运行本机端到端演示：

```powershell
python examples/mcp_http_demo.py
python examples/mcp_http_demo.py --sse
python examples/mcp_http_demo.py --legacy --sse
python examples/mcp_http_demo.py --deny
```
