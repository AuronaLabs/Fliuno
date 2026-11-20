<p align="center">
  <picture>
    <source media="(max-width: 600px) and (prefers-color-scheme: light)" srcset="assets/fliuno-docs-compact-light.svg">
    <source media="(max-width: 600px)" srcset="assets/fliuno-docs-compact.svg">
    <source media="(prefers-color-scheme: light)" srcset="assets/fliuno-docs-light.svg">
    <img src="assets/fliuno-docs.svg" alt="" width="1120">
  </picture>
</p>

# Fliuno 文档中心

[项目首页](../README.md) · [更新记录](../CHANGELOG.md) · [开发路线图](roadmap.md)

## 从这里开始

**先体验**　用离线模型跑通任务、审批和交付物，再换成真实模型。\
[准备环境](setup.md#快速开始) · [运行离线演示](../README.md#跑一次离线任务) · [配置模型](setup.md#真实模型)

**接入应用**　选好传输方式，再按任务流程接上工作台。\
[选择连接方式](#连接方式) · [启动并握手](setup.md#再启动自己的服务) · [接入工作台](#工作台要展示什么)

**开发内核**　找到对应的 Rust crate，查看跨平台验证方式。\
[模块与依赖](architecture.md#rust-工作区模块) · [运行工程检查](setup.md#检查与演示) · [查阅验收结果](execution-evidence.md)

[客户端](#接入客户端) · [模型与工具](#模型工具与-agent) · [本机执行](#本机执行与隔离) · [内核开发](#内核与开发)

## 接入客户端

### 连接方式

| 入口 | 接入文档 |
| --- | --- |
| **stdio** | 适用于任意语言。把内核作为子进程，以 [stdio JSON-RPC](protocol.md#传输) 连接。[JSON Schema](../protocol/fliuno.schema.json) |
| **Rust** | 通过 [Rust 客户端](protocol.md#rust-调用方)发出类型化请求，订阅和补读事件。 |
| **HTTP** | 挂载 [AG-UI 路由](ag-ui.md#http-接入)，用 SSE 接收任务进度。[离线示例](../examples/ag_ui_http_demo.py) |

### 工作台要展示什么

1. **加载首屏。** 握手后按服务返回的能力获取任务计数、最近任务和待审批项。服务支持 `work/subscribe` 时，一次调用即可取得快照并订阅变动。[快照与实时订阅](protocol.md#工作台实时订阅)
2. **发起任务。** 提交前保存 `clientRequestId`，拿到响应后保存任务 ID。如果请求断线且未收到响应，用原参数和请求键重试。[任务提交与重试](protocol.md#任务提交与重试)
3. **处理审批。** 向用户展示操作详情，提交允许或拒绝的决定，再读取任务的执行结果。[待审批项](protocol.md#审批收件箱与决定) · [决定与运行](protocol.md#决定与运行)
4. **收取交付物。** 订阅目录，按需分段下载，也可以把同一会话的图片或 PDF 交给下一项任务。[交付物目录](protocol.md#交付物目录订阅) · [下载](protocol.md#审批事件与产物) · [复用](protocol.md#将交付物用作任务输入)

> [!TIP]
> 重连时从最后已处理的事件游标补读。只需更新任务列表时，可按固定窗口合并同一任务的多次修订。AG-UI 则用原运行 ID 和 `Last-Event-ID` 续传。
>
> [按窗口合并修订](protocol.md#按窗口合并摘要修订) · [续传 SSE 消息](ag-ui.md#sse-输出与续传)

<details>
<summary>进阶：重连、批量操作、历史和用量</summary>

- **工作台刷新**：[独立读取快照](protocol.md#工作台快照) · [按窗口合并修订](protocol.md#按窗口合并摘要修订) · [按修订回放](protocol.md#任务摘要修订回放)
- **分开读取变动和详情**：[回放任务 ID](protocol.md#任务变动回放)，再[批量取摘要](protocol.md#批量读取任务摘要)。
- **恢复与批量操作**：[判断能否恢复](protocol.md#任务恢复判断) · [批量查询恢复状态](protocol.md#批量查询恢复状态) · [批量取消任务](protocol.md#批量取消任务) · [核对审批结果](protocol.md#批量读取审批决定) · [校验交付物摘要](protocol.md#批量读取交付物摘要)
- **目录与历史**：[会话目录](protocol.md#会话目录) · [任务目录](protocol.md#任务目录) · [历史分页](context.md#会话历史分页) · [订阅会话历史](context.md#订阅会话历史) · [历史搜索](protocol.md#查找历史任务) · [只搜新完成的任务](protocol.md#只搜索新完成的任务)
- **模型用量**：[累计用量](protocol.md#用量汇总) · [批量汇总](protocol.md#批量用量汇总) · [逐次报告](protocol.md#回放用量报告)

</details>

## 模型、工具与 Agent

| 连接 | 用途 |
| --- | --- |
| **[模型配置](setup.md#真实模型)** | OpenAI、Anthropic、Gemini 的流式输出与请求预算 |
| **[MCP](mcp.md)** | 调用外部工具，读取[资源、提示和技能](mcp-content.md)，监听变更 |
| **[ACP](acp.md)** | 启动本机 Agent、加载会话历史和代理受控文件访问 |
| **[A2A](a2a.md)** | 提交远端任务、核对状态、显式续接和导入附件 |
| **[Agent Skills](skills.md)** | 发现本机技能，按需读取指令和资源 |

## 本机执行与隔离

| 主题 | 对应文档 |
| --- | --- |
| **配置与命令** | [启动配置读取](host-configuration.md) · [宿主注册命令](setup.md#主机注册的命令) |
| **平台隔离** | [Windows / Linux 隔离与资源限制](sandbox.md) · [运行时伴随文件](runtime-bundles.md) |
| **审批范围** | [注册身份与持久授权](authority.md) |
| **跨轮次终端** | [配置终端](setup.md#跨轮次终端) · [订阅与恢复](terminal-subscriptions.md) · [终端归档](terminal-archive.md) |

## 内核与开发

| 文档 | 内容 |
| --- | --- |
| **[内核架构](architecture.md)** | 模块依赖、任务状态机、事务和权限边界 |
| **[会话上下文](context.md)** | 历史选择、冻结快照和模型请求预算 |
| **[验收与性能记录](execution-evidence.md)** · [检查与演示](setup.md#检查与演示) | 可复现的命令、双平台验收和性能测量 |
| **[迭代路线图](roadmap.md)** · [更新记录](../CHANGELOG.md) | 已完成的能力、待办事项和版本变更 |
| **[内核调研](research.md)** | 模型、工具、Agent 和系统隔离的选型依据 |

[回到阅读入口](#从这里开始) · [返回项目首页](../README.md)
