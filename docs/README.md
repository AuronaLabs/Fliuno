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

**想先跑起来？** 用离线演示走完任务、审批和交付流程，再选择要接入的模型。[准备环境](setup.md#快速开始) → [运行演示](../README.md#跑一次离线任务) → [配置模型](setup.md#真实模型)

**在做自己的工作台？** 先选传输方式，完成握手后接入任务、审批和事件流。[选择传输方式](#连接方式) → [启动并握手](setup.md#再启动自己的服务) → [工作台接入顺序](#工作台要展示什么)

**在改 Rust 内核？** 从请求路径和 crate 职责定位代码，再运行检查。[内核架构](architecture.md#领域与依赖) → [工作区模块](architecture.md#rust-工作区模块) → [检查与演示](setup.md#检查与演示)

[应用协议](protocol.md) · [模型与工具](#模型工具与-agent) · [本机执行](#本机执行与隔离) · [开发与验收](#内核与开发)

## 接入客户端

### 连接方式

| 入口 | 接入方式 |
| --- | --- |
| **stdio** | 用任意语言把内核作为本机子进程启动。[JSON-RPC](protocol.md#传输) · [JSON Schema](../protocol/fliuno.schema.json) |
| **Rust** | 用类型化请求读取状态，订阅和补读事件。[Rust 客户端](protocol.md#rust-调用方) |
| **HTTP / SSE** | 由宿主挂载 AG-UI 路由，向客户端推送任务进度。[HTTP 接入](ag-ui.md#http-接入) · [离线示例](../examples/ag_ui_http_demo.py) |

### 工作台要展示什么

从加载首屏到收取交付物，可以按下面的顺序接入。

1. **加载首屏。** 握手后检查服务能力。支持 `work/subscribe` 时，一次调用即可拿到任务计数、最近任务、待审批项及后续变动。[快照与实时订阅](protocol.md#工作台实时订阅)
2. **发起任务。** 提交前保存 `clientRequestId`，拿到响应后保存任务 ID。需要让提交使用预览过的会话历史时，传入相同的上下文范围。[任务提交](protocol.md#任务提交与重试) · [固定上下文预览范围](context.md#提交任务时固定预览范围)
3. **处理审批。** 展示操作详情，向内核提交用户的决定，再显示任务结果。[待审批项](protocol.md#审批收件箱与决定) · [批量读取详情](protocol.md#批量读取待审批详情) · [审批决定与运行](protocol.md#决定与运行)
4. **收取交付物。** 订阅目录、分段下载，也可以把同一会话的文本报告、图片或 PDF 交给下一项任务。[交付物目录](protocol.md#交付物目录订阅) · [批量下载首块](protocol.md#批量下载首块) · [复用](protocol.md#将交付物用作任务输入)

### 重连与恢复

保存已消费的事件游标和提交任务时的请求键。连接恢复后，根据中断的位置选择相应接口。

| 遇到的情况 | 如何继续 |
| --- | --- |
| **提交请求断线** | 用原参数和请求键重试。已有多条请求键时，可以批量查询对应的任务摘要。[重试规则](protocol.md#任务提交与重试) · [批量找回](protocol.md#按请求键批查任务) |
| **任务列表落后** | 从最后处理的事件游标补读。只需更新列表时，可以按固定窗口合并同一任务的多次修订。[合并摘要修订](protocol.md#按窗口合并摘要修订) · [逐次修订回放](protocol.md#任务摘要修订回放) |
| **AG-UI 事件流中断** | 用原运行 ID 和 `Last-Event-ID` 续传。[续传 SSE 消息](ag-ui.md#sse-输出与续传) |
| **执行结果待确认** | 查询单项或批量恢复状态，再按返回结果处理。[任务恢复判断](protocol.md#任务恢复判断) · [批量查询](protocol.md#批量查询恢复状态) |

<details>
<summary>更多查询与批量操作</summary>

- **快照与目录**：[独立读取工作台快照](protocol.md#工作台快照) · [会话目录](protocol.md#会话目录) · [任务目录](protocol.md#任务目录)
- **分开同步变动和详情**：[回放变动的任务 ID](protocol.md#任务变动回放) · [批量读取摘要](protocol.md#批量读取任务摘要)
- **批量操作**：[取消任务](protocol.md#批量取消任务) · [核对审批结果](protocol.md#批量读取审批决定) · [校验交付物摘要](protocol.md#批量读取交付物摘要)
- **历史**：[分页读取](context.md#会话历史分页) · [订阅历史](context.md#订阅会话历史) · [查找任务](protocol.md#查找历史任务) · [只搜新完成的任务](protocol.md#只搜索新完成的任务)
- **模型用量**：[累计用量](protocol.md#用量汇总) · [批量汇总](protocol.md#批量用量汇总) · [从检查点汇总](protocol.md#按事件范围汇总) · [逐次报告](protocol.md#回放用量报告)

</details>

## 模型、工具与 Agent

| 连接 | 用途 |
| --- | --- |
| **[模型配置](setup.md#真实模型)** | OpenAI、Anthropic 和 Gemini 的流式输出与请求预算。 |
| **[MCP](mcp.md)** | 调用外部工具，读取[资源、提示和技能](mcp-content.md)，监听变更。 |
| **[ACP](acp.md)** | 启动本机 Agent、加载会话历史和代理受控文件访问。 |
| **[A2A](a2a.md)** | 提交远端任务、核对状态、显式续接和导入附件。 |
| **[Agent Skills](skills.md)** | 发现本机技能，按需读取指令和资源。 |

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
| **[内核架构](architecture.md)** | 模块依赖、任务状态机、事务和权限边界。 |
| **[会话上下文](context.md)** | 历史选择、冻结快照和模型请求预算。 |
| **[验收与性能记录](execution-evidence.md)** · [检查与演示](setup.md#检查与演示) | 验证命令、双平台验收和性能测量。 |
| **[迭代路线图](roadmap.md)** · [更新记录](../CHANGELOG.md) | 已完成的能力、待办事项和版本变更。 |
| **[内核调研](research.md)** | 模型、工具、Agent 和系统隔离的选型依据。 |

[回到阅读入口](#从这里开始) · [返回项目首页](../README.md)
