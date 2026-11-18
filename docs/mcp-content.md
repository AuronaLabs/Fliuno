# MCP 资源、提示与技能

主机照常通过 `--mcp-config` 注册固定的 stdio 或 Streamable HTTP 服务。握手声明 `resources` 时注册资源列表、模板列表与读取。声明 `prompts` 时注册提示列表与获取。声明 `completions` 时注册参数补全。声明 `io.modelcontextprotocol/skills` 扩展时注册技能列表和元数据查询。没有 `tools` 能力的服务不会被发送 `tools/list`。连接时不枚举资源、提示或技能，不把它们自动加入上下文。

以下名称的 `<id>` 来自主机配置。每次调用均为 `External` 工具效果，先持久审批和开始事件，再发出请求。服务的只读声明不跳过审批。

| 工具 | MCP 方法 | 参数 |
| --- | --- | --- |
| `mcp_<id>_resources_list` | `resources/list` | `{cursor?:string}` |
| `mcp_<id>_resources_templates` | `resources/templates/list` | `{cursor?:string}` |
| `mcp_<id>_resources_read` | `resources/read` | `{uri:string}` |
| `mcp_<id>_prompts_list` | `prompts/list` | `{cursor?:string}` |
| `mcp_<id>_prompts_get` | `prompts/get` | `{name:string,arguments?:{key:string}}` |
| `mcp_<id>_complete` | `completion/complete` | `{ref,argument:{name,value},context?:{arguments}}` |
| `mcp_<id>_skills_list` | `skills/list` | `{cursor?:string}` |
| `mcp_<id>_skills_get` | `skills/get` | `{uri:string}` |
| `mcp_<id>_resources_directory` | `resources/directory/read` | `{uri:string,cursor?:string}` |

补全的 `ref` 可以是 `{"type":"ref/prompt","name":"draft"}` 或 `{"type":"ref/resource","uri":"fixture:report/{id}"}`。`argument.value` 是已输入的前缀，可为空字符串。`context.arguments` 是可选的已知参数值，遵循提示参数的字符串与字节预算。

补全返回 `result.completion.values` 中的最多 100 项建议，并保留服务返回的 `total` 和 `hasMore`。取用这些建议来读取资源或获取提示时，需要分别提出调用并获得审批。

技能列表每页最多 256 个条目。`skills/get` 的 `uri` 必须指向技能的 `SKILL.md`，返回项须匹配请求的 URI。内核检查技能名称和清单结构，每份清单最多 256 个文件引用。结果是元数据。客户端要读取技能内容，可以针对已注册资源工具发起新的审批请求。

技能扩展声明 `directoryRead:true` 时，目录工具可以读取一个页面的直接子项。目录项和 `nextCursor` 作为数据返回，内核不会递归读取，分页仍需逐次审批。

## 返回数据与权限

结果统一为 `{serverId,method,result,grantsPermissions:false}`。`result` 是 SDK 解码后的协议结果，保留资源内容、提示消息、模板、分页和可选元数据；不是原始 wire 字节。资源文本和二进制 `blob` 作为数据保留，内核不解码二进制、不创建产物、不执行脚本、不展开 URI 模板，也不获取结果中附带的 URL。

资源 URI 必须带合法 ASCII scheme，至多 4096 UTF-8 字节，不含空白或控制字符。`file:`、`http:` 和自定义 scheme 都只作为相应方法的 `uri` 发给该 MCP 服务；它们不会触发内核的文件读取或独立网络访问。远程服务实际能够读取什么由服务自身权限决定，stdio 隔离进程继续受平台策略约束。

`prompts/get` 返回的 `user` / `assistant` 角色位于 `Message::ToolResult` 的 JSON 内，不会转成任务消息、系统指令或额外授权。内容仍可能包含不可信指令，客户端应保留来源与工具结果边界。`grantsPermissions:false` 描述内核权限语义，不是对内容正确性的背书。

## 单次资源监听

协商版本至少为 `2026-07-28` 且服务声明 `resources.subscribe` 时，注册 `mcp_<id>_resources_watch({uri,waitMs})`。每次调用独立审批，监听一条 URI，等待时间为 1 毫秒到 5 秒且最多占配置超时的一半。排队、握手和关闭监听都计入整个调用的超时。

结果只包含 `uri` 和 `status`。`updated` 表示收到更新，`elapsed` 表示等待期结束，`declined` 表示服务未接受该 URI，`ended` 表示服务正常结束监听。需要资源正文时，可另行审批 `resources_read`。监听结束后释放订阅。发出请求后取消、超时或连接异常会按现有未知结果规则关闭该 MCP 连接。

互操作验证覆盖官方 SDK 的 HTTP SSE、Windows 本机 stdio 与 AppContainer stdio。旧版协议即使声明了 `resources.subscribe` 也不注册这个工具。

## 预算与请求生命周期

每个列表调用只发送一页请求，最多 256 项；不自动追取 `nextCursor`，下一页需要另一次审批。请求和响应游标必须非空、至多 4096 UTF-8 字节且不含控制字符。返回重复游标不导致内核循环，服务是否推进分页由调用方审阅。

资源读取最多 64 个 `contents`；提示获取最多 64 个 `messages`。完整工具结果含外层封装最多 1 MiB。HTTP 沿用传输层的 1 MiB JSON/SSE 单事件上限，先检查 wire 响应再解码；Windows host 和 Windows/Linux 隔离 stdio 的 SDK 读取器也限制每行 1 MiB，非 Windows host 使用 SDK 默认的 16 MiB 行上限。stdio 超长行由 SDK 丢弃，不会直接形成内容结果，仍受调用 deadline 约束。内容集合和封装后的总量核验发生在 SDK 解码后；行长度预算不等于完整进程内存隔离。

所有参数合计 JSON 最多 64 KiB。提示名称和参数键非空、至多 256 UTF-8 字节且无控制字符；最多 32 个参数，值只能为字符串且每个至多 8192 UTF-8 字节。JSON Schema 的长度是字符数量，运行时还核验上述字节预算。拒绝额外字段，不能通过参数指定其他端点、凭据、`_meta` 或 MRTR 续交互状态。

内容操作使用官方 SDK 的单次原始请求，绕过便利方法的缓存与过期缓存回退。服务即使返回 `ttlMs`，下一次批准仍发送新请求；暂不实现内容缓存或订阅。工具、资源和提示共享一个连接请求锁，等待锁也计入 `timeoutMs`。等待中的取消/超时不发请求，不关闭正在执行的操作；发出后的取消、超时、协议/传输错误或不支持的 MRTR 则关闭连接并保留结果未知。关闭的优雅清理最多等待 5 秒，不承诺远程服务的实际副作用已经停止。响应集合/输出预算违规同样不能自动重放，已完成的响应预算违规不要求关闭整个连接。

内核还显式核验响应 `resultType`，不能仅依赖 SDK 的结果枚举：同时带 `contents` / `messages` 等最终形状和 `input_required` / `task` / 扩展状态的响应也会拒绝。只有 `complete` 或旧版兼容的缺省字段可作为完成结果；工具发现和 `tools/call` 使用同一规则，不把部分内容当作完成，也不自动继续请求。

## 审批与恢复

每个内容工具的注册摘要包含已冻结的服务注册身份、方法和公开契约。端点、原生程序、参数、环境、凭据或伴随文件配置改变后，同名别名的旧待批调用不能授权新服务；更换 URI 或提示参数也需要新的调用审批。资源和提示内容不是注册时固定的快照，服务在不同获批调用之间可以返回不同内容。协议元数据也不构成内容发布者认证。

已开始的外部请求遵循普通未知结果规则，重启不重放。已完成的结果、事件和任务快照保存在 SQLite，重启读取不再发送请求。通用契约见[注册身份](authority.md)与[MCP 传输](mcp.md)。资源更新订阅、OAuth 和 MRTR/Tasks 持久继续仍未实现。

## 验证

```powershell
cargo test -p fliuno-mcp --all-features --locked
python examples/mcp_content_demo.py
python examples/mcp_content_demo.py --deny
python examples/mcp_content_demo.py --switch-source
```

HTTP wire 测试验证无工具服务、按需分页、主机文件/HTTP URI 不解引用、编码二进制保留、输入覆盖/预算、缓存提示不隐藏请求或失败、输出预算、MRTR、取消/超时和跨方法排队取消。SQLite 测试覆盖拒绝、换端点的旧审批失效、角色保留为工具数据，以及重启不重放。官方 SDK HTTP 服务在现代 discover / 旧版 initialize 与 JSON/SSE 四种组合中验证资源、提示和补全。独立 SDK stdio fixture 在 Windows TrustedHost/AppContainer 实际运行。Linux 同类探针接入 CI，当前主机交叉检查不代替运行。

演示使用真实 app-server、SQLite 与确定性的本机 HTTP 模型，依次审批分页、模板、资源、提示列表和提示获取；拒绝分支不读取内容，切换来源分支拒绝旧审批且不再请求模型，新任务可完成。全部流程重启核对最终快照。

实现依据：

- https://modelcontextprotocol.io/specification/2026-07-28/server/resources
- https://modelcontextprotocol.io/specification/2026-07-28/server/prompts
- 锁定的 `rmcp 3.11.0` 中 `service/client.rs` 的一次性请求、缓存和过期回退，以及 `model.rs` 的请求/结果类型。
