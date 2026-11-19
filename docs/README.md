<p align="center">
  <picture>
    <source media="(max-width: 540px)" srcset="assets/fliuno-docs-compact.svg">
    <source media="(prefers-color-scheme: light)" srcset="assets/fliuno-docs-light.svg">
    <img src="assets/fliuno-docs.svg" alt="Fliuno 文档：上手、应用接入与内核开发" width="960">
  </picture>
</p>

# Fliuno 文档中心

[项目首页](../README.md) · [更新记录](../CHANGELOG.md) · [开发路线图](roadmap.md)

## 从这里开始

按你现在想做的事选择入口。下面的索引可以直接跳到具体主题。

### 01 · 跑一次任务

先用离线演示走完审批和交付，再按需切换到自己的模型。

**[运行离线演示](../README.md#跑一次离线任务)** · [准备环境](setup.md#快速开始) · [配置模型](setup.md#真实模型)

### 02 · 接入应用

让自己的应用提交任务、显示审批，并在重连后接上进度。

**[完成 stdio 握手](protocol.md#传输)** · [Rust 客户端](protocol.md#rust-调用方) · [AG-UI HTTP](ag-ui.md#http-接入)

### 03 · 开发内核

看清模块分工和授权边界，再用本地测试和演示验证改动。

**[阅读内核架构](architecture.md#领域与依赖)** · [持久授权](authority.md) · [本地验证](setup.md#检查与演示)

---

**按主题查阅：** [接入客户端](#接入客户端) · [模型、工具与 Agent](#模型工具与-agent) · [本机执行与隔离](#本机执行与隔离) · [内核与开发](#内核与开发)

## 接入客户端

用应用协议提交工作、展示进度、处理审批和收取交付物。

### 连接方式

| 从哪里接入 | 查阅 |
| --- | --- |
| **stdio / JSON-RPC** | [握手与消息格式](protocol.md#传输) · [JSON Schema](../protocol/fliuno.schema.json) |
| **Rust** | [类型化客户端与事件补读](protocol.md#rust-调用方) |
| **HTTP / SSE** | [挂载 AG-UI 路由](ag-ui.md#http-接入) · [SSE 续传](ag-ui.md#sse-输出与续传) · [HTTP 演示](../examples/ag_ui_http_demo.py) |

### 工作台要展示什么

| 要实现 | 查阅 |
| --- | --- |
| **展示工作台与待审批** | [工作台实时订阅](protocol.md#工作台实时订阅) · [待审批快照与续页](protocol.md#待审批快照) |
| **停止选中的任务** | [批量取消任务](protocol.md#批量取消任务) |
| **刷新变动任务** | [一次读取变动摘要](protocol.md#任务变动摘要) · [仅回放任务 ID](protocol.md#任务变动回放) · [按 ID 读取摘要](protocol.md#批量读取任务摘要) |
| **浏览会话或定位任务** | [历史分页](context.md#会话历史分页) · [搜索接口](protocol.md#查找历史任务) |
| **浏览并下载交付物** | [目录订阅](protocol.md#交付物目录订阅) · [批量校验摘要](protocol.md#批量读取交付物摘要) · [分段读取](protocol.md#审批事件与产物) |
| **查看模型用量** | [累计用量](protocol.md#用量汇总) · [逐次报告](protocol.md#回放用量报告) |

## 模型、工具与 Agent

按宿主需要选择模型、工具和执行 Agent 的连接方式。

| 接入方式 | 用途 |
| --- | --- |
| **[模型配置](setup.md#真实模型)** | OpenAI、Anthropic、Gemini 的流式输出与请求预算 |
| **[MCP](mcp.md)** · [资源、提示与技能](mcp-content.md) | 调用外部工具，读取资源与提示，监听变更 |
| **[ACP](acp.md)** | 启动本机 Agent、加载会话历史和代理受控文件访问 |
| **[A2A](a2a.md)** | 提交远端任务、核对状态、显式续接和导入附件 |
| **[Agent Skills](skills.md)** | 发现本机技能，按需读取指令和资源 |

## 本机执行与隔离

查看宿主如何授予本机能力，以及进程和输出怎样受控。

| 主题 | 查阅 |
| --- | --- |
| **宿主启动** | [配置读取](host-configuration.md) · [注册命令](setup.md#主机注册的命令) |
| **进程与资源限制** | [Windows / Linux 隔离](sandbox.md) · [运行时伴随文件](runtime-bundles.md) |
| **审批的有效范围** | [注册身份与持久授权](authority.md) |
| **跨轮次终端** | [配置终端](setup.md#跨轮次终端) · [订阅与恢复](terminal-subscriptions.md) · [终端归档](terminal-archive.md) |

## 内核与开发

查模块职责、实现决策，以及如何复现验证结果。

| 文档 | 内容 |
| --- | --- |
| **[内核架构](architecture.md)** | Rust 模块、任务状态机、事务与权限边界 |
| **[会话上下文](context.md)** | 历史选择、冻结快照和模型请求预算 |
| **[开发路线图](roadmap.md)** · [更新记录](../CHANGELOG.md) | 计划、开发中的能力和已发布版本 |
| **[验收与性能记录](execution-evidence.md)** · [检查与演示](setup.md#检查与演示) | 可复现的命令、双平台验收和性能测量 |
| **[内核调研](research.md)** | 模型、工具、Agent 和系统隔离的选型依据 |

[回到阅读入口](#从这里开始) · [返回项目首页](../README.md)
