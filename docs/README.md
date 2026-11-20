<p align="center">
  <picture>
    <source media="(max-width: 600px) and (prefers-color-scheme: light)" srcset="assets/fliuno-docs-compact-light.svg">
    <source media="(max-width: 600px)" srcset="assets/fliuno-docs-compact.svg">
    <source media="(prefers-color-scheme: light)" srcset="assets/fliuno-docs-light.svg">
    <img src="assets/fliuno-docs.svg" alt="" width="960">
  </picture>
</p>

# Fliuno 文档中心

[项目首页](../README.md) › 文档中心

## 从这里开始

1. **第一次运行**：先[跑一次离线任务](../README.md#跑一次离线任务)，再按需[配置模型](setup.md#真实模型)。
2. **把 Fliuno 接进应用**：[启动服务并握手](setup.md#再启动自己的服务)，再[选择连接方式](#连接方式)。
3. **修改内核**：[看模块依赖](architecture.md#领域与依赖)，了解[授权边界](authority.md)，然后[运行工程检查](setup.md#检查与演示)。

**按主题浏览：** [接入客户端](#接入客户端) · [模型、工具与 Agent](#模型工具与-agent) · [本机执行与隔离](#本机执行与隔离) · [内核与开发](#内核与开发)

## 接入客户端

### 连接方式

| 宿主形态 | 接入文档 |
| --- | --- |
| **任意语言子进程** | [stdio / JSON-RPC](protocol.md#传输) · [JSON Schema](../protocol/fliuno.schema.json) |
| **Rust 应用** | [类型化客户端与事件补读](protocol.md#rust-调用方) |
| **HTTP 宿主** | [挂载 AG-UI 路由](ag-ui.md#http-接入) · [SSE 续传](ag-ui.md#sse-输出与续传) · [离线示例](../examples/ag_ui_http_demo.py) |

### 工作台要展示什么

1. **加载工作台**：[握手](setup.md#再启动自己的服务)读取服务能力，再用[工作台实时订阅](protocol.md#工作台实时订阅)取得初始快照，接上后续变动。
2. **提交任务**：[提交并保存任务 ID](protocol.md#任务提交与重试)。如果提交后断线、还没收到任务 ID，用同一个 `clientRequestId` 重试。
3. **处理审批**：展示[待审批操作](protocol.md#待审批快照)，待用户[允许或拒绝](protocol.md#决定与运行)。
4. **展示交付物**：[订阅交付物目录](protocol.md#交付物目录订阅)，按需[分段下载](protocol.md#审批事件与产物)。

**重新连接时**，从上次已处理的游标[补读工作台变动](protocol.md#任务变动摘要)。采用 HTTP / SSE 的宿主，用原运行 ID 和 `Last-Event-ID` [续传消息](ag-ui.md#sse-输出与续传)。

<details>
<summary>进阶：重连、批量操作、历史和用量</summary>

- **工作台刷新**：[独立读取快照](protocol.md#工作台快照) · [按修订回放](protocol.md#任务摘要修订回放)
- **分开读取变动和详情**：[回放任务 ID](protocol.md#任务变动回放)，再[批量取摘要](protocol.md#批量读取任务摘要)。
- **恢复与批量操作**：[判断能否恢复](protocol.md#任务恢复判断) · [批量取消任务](protocol.md#批量取消任务) · [核对审批结果](protocol.md#批量读取审批决定) · [校验交付物摘要](protocol.md#批量读取交付物摘要)
- **目录与历史**：[会话目录](protocol.md#会话目录) · [任务目录](protocol.md#任务目录) · [历史分页](context.md#会话历史分页) · [历史搜索](protocol.md#查找历史任务) · [只搜新完成的任务](protocol.md#只搜索新完成的任务)
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
| **[内核调研](research.md)** | 模型、工具、Agent 和系统隔离的选型依据 |

**版本与进度**：[更新记录](../CHANGELOG.md) · [开发路线图](roadmap.md)

[回到阅读入口](#从这里开始) · [返回项目首页](../README.md)
