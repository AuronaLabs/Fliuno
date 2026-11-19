# MCP 传输与边界

`fliuno-mcp` 使用官方 Rust `rmcp` SDK，将主机配置的 MCP 工具接入 Fliuno 工具注册表。工具发现时生成稳定的模型侧别名；请求仍经过参数 Schema 校验、持久化审批、执行身份记录和结果提交。MCP 工具标记为外部效果，工具注解和服务器能力不会授予权限。

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

`bearerTokenEnv` 指向只含裸 token 的环境变量，内核添加 `Bearer` 前缀。`headersEnv` 将自定义头名映射到环境变量名，可用于服务商 API key。头名和值在送入 reqwest 前校验；传输控制头、重复头、空值、超长值和超过 16 KiB 的总凭据头被拒绝。凭据不进入配置持久化、Task、Event、Artifact 或错误正文。缺失凭据会在网络请求前失败。OAuth 发现、浏览器授权、刷新与凭据存储尚未实现。

可信 stdio 使用宿主配置的绝对 `cwd`，保留宿主用户的文件和网络权限。Windows 可用 `resources.pidsMax` 和 `resources.memoryMaxBytes` 限制 Job。Linux 可指定 `linuxCgroupParent` 并配置进程或线程数、内存和 CPU 预算。Linux 无法应用指定 cgroup 或预算时拒绝启动。详细范围见[进程启动](sandbox.md)。

## 生命周期和版本

HTTP `lifecycle` 默认 `auto`：首先使用 `2026-07-28` 的 `server/discover` 无会话生命周期。SDK 在对端明确不支持 discover 方法时回退旧版 initialize；Fliuno 还处理对端在发现前拒绝现代版本的情形：仅当 SDK 明确报告无共同版本，且对端明确列出双方认识的 `2025-03-26` 或更新的 initialize 版本时，新建一次 transport 并尝试旧版 initialize。认证失败、连接错误、未知版本和显式 `discover` 不触发此额外回退。`legacy` 直接使用旧 initialize 生命周期。stdio 默认保留原来的 `legacy` 行为。

现代 HTTP POST 携带 `MCP-Protocol-Version`、`Mcp-Method` 和 JSON-RPC 请求元数据；工具调用还携带 `Mcp-Name`。若远端工具输入 Schema 使用 `x-mcp-header` 注解，SDK 会按 MCP 标准将对应原始参数值编码为 `Mcp-Param-*` 头。标准头由 `rmcp` 从请求生成，配置不可覆盖。客户端接受 JSON 与 SSE 响应。当前锁定 `rmcp 3.11.0`，其 SDK 负责协议编码和生命周期协商。

## 预算和失败语义

- 每服务器最多一个并行 tools/call；最多排队 16 个 HTTP 传输消息。
- `timeoutMs` 范围为 100..300000，默认 30000。连接超时最多 10 秒；SDK 控制请求最多 5 秒；初始化、发现和工具调用遵循主机预算。
- 一条 SSE event 和一条完整 JSON 响应最多 1 MiB；工具输出序列化后也最多 1 MiB。工具发现最多 32 页、256 个工具及 4 MiB 累计序列化页面数据；工具描述最多 4 KiB，单个 Schema 最多 64 KiB。每页及 SDK 已解析对象仍另有短暂分配，因此累计发现预算不是总进程 RSS 上限。
- 排队期间取消或超时在发送前返回已知失败，不关闭正在处理其他调用的主机。
- 请求发出后的网络错误、超时、取消、协议失败、HTTP 超限响应及目前不支持的 MRTR 多轮 continuation 都按结果未知处理，并关闭共享主机。内核保留 `inFlightTool`，重启不会重新发送该调用。协议层完整结果在序列化后超过工具输出预算也保留未知标记。
- `isError: true` 是一个完整的 MCP 工具结果，并不代表 HTTP/JSON-RPC 传输失败。

## MCP Tasks

客户端声明 `io.modelcontextprotocol/tasks` 能力。仅当远端在 `2026-07-28` 或更新协议中声明同一扩展时，才注册 Tasks 工具。`tools/call` 返回 `resultType:"task"` 时，Fliuno 将其任务句柄作为该工具的已知结果写入本地任务记录；不支持的对端仍按未知结果处理。

| 工具 | MCP 方法 | 参数 |
| --- | --- | --- |
| `mcp_<id>_tasks_get` | `tasks/get` | `{taskId:string}` |
| `mcp_<id>_tasks_wait` | `tasks/get`（短轮询） | `{taskId:string,waitMs:integer}` |
| `mcp_<id>_tasks_update` | `tasks/update` | `{taskId:string,inputResponses:object}` |
| `mcp_<id>_tasks_cancel` | `tasks/cancel` | `{taskId:string}` |

四种操作均绑定远端扩展能力和服务器注册摘要，且每次独立审批。`tasks/get` 保留状态、待补请求或最终结果；`tasks/update` 只转交调用者明确提供的响应，不自动执行 `roots/list`、采样或 elicitation；`tasks/cancel` 的成功结果只表示服务器接受了协作取消请求，需再调用 `tasks/get` 核实最终状态。任务 ID 最多 512 字节且不得包含控制字符；每次最多 16 个响应、响应 JSON 合计最多 256 KiB，读取结果最多 1 MiB。

`tasks_wait` 立即读取一次，之后遵循服务端建议的 `pollIntervalMs`（至少 100 毫秒，未提供时为 500 毫秒），总计最多 8 次。`waitMs` 限制在 1 毫秒至主机超时的一半，且最多 5 秒；该窗口限制发起下一次查询的时间，已发出的查询仍由主机总超时约束。等待期间该连接继续串行处理请求。返回 `{reason,polls,snapshot}`，其中原因是 `terminal`、`input_required`、`wait_budget` 或 `poll_limit`。等待不会补答或请求取消远端任务。

`tasks/get` 收到远端 JSON-RPC 拒绝时作为已知读取失败返回，并保留连接；发送后的超时、取消或传输错误仍属于结果未知，会关闭该主机，避免后续请求复用不确定的会话状态。

资源、提示、Skills 元数据和 completion 查询使用相同的只读错误语义。

Fliuno 将任务句柄持久化在正常工具结果中，由模型或调用方显式选择单次读取、短轮询、补答或协作取消；它不会在后台隐式轮询，也不会在重启后自动重发任何 Tasks 请求。读取结果必须匹配请求的任务 ID，且是完整快照。任务状态通知、自动续接、过期清理和 MRTR 续交互仍未实现。

## 当前范围

隔离 stdio server 可配置 `assets`，在启动前把宿主列举的脚本、DLL 和数据复制到私有根并检查 SHA-256。工具审批另存完整注册摘要，绑定传输、端点、原生程序内容、参数、实际环境/凭据、预算与远端 Schema；即使展示别名不变，重启后的配置变化也不能复用旧审批。环境和凭据在注册时冻结，传输使用同一份值，见[注册身份](authority.md)。Windows 和 Debian Linux 来宾都已通过真实 Node/CPython 多文件 MCP 的允许、拒绝、篡改流程，命令与局限见[解释器清单](runtime-bundles.md)。MCP 不借此获得工作区代理或网络能力。

除 MCP tools/list 和 tools/call 外，内核也按服务声明接入资源、资源模板、提示和 Tasks 控制，详见[内容通道](mcp-content.md)。这些操作均逐次审批，支持逐次审批的短时目录与 URI 监听。MCP server-to-client sampling、elicitation 和长期订阅仍待接入。工具继续使用 SDK 的一次性 `call_tool_once`，内容操作用原始 SDK 请求绕过自动 MRTR 和旧缓存回退；二者共享排队、取消与失败关闭逻辑。Tasks 当前由显式控制工具驱动；MRTR 续交互、任务状态通知、OAuth、旧版独立 HTTP+SSE 传输、热重连和外部真实服务验收都仍在后续计划中。stdio Windows AppContainer/Linux Landlock 隔离不适用于 HTTP 网络服务器；远程服务器访问由宿主网络身份和凭据决定。

协议版本矩阵、凭据处理、JSON/SSE、取消、不确定结果、审批/拒绝与重启行为由单元测试、官方 SDK HTTP server 互操作测试和本机 app-server 进程测试覆盖。运行本机端到端演示：

```powershell
python examples/mcp_http_demo.py
python examples/mcp_http_demo.py --sse
python examples/mcp_http_demo.py --legacy --sse
python examples/mcp_http_demo.py --deny
```
