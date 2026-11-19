<p align="center">
  <img src="assets/fliuno-docs.svg" alt="Fliuno 文档：上手、应用接入与内核开发" width="960">
</p>

# Fliuno 文档中心

[项目首页](../README.md)　·　[更新记录](../CHANGELOG.md)　·　[开发路线图](roadmap.md)

## 从这里开始

1. **先跑起来**<br>
   [离线任务演示](../README.md#跑一次离线任务) → [准备环境](setup.md#快速开始) → [接入真实模型](setup.md#真实模型)
2. **接入应用**<br>
   [stdio 握手](protocol.md#传输) → [Rust 客户端](protocol.md#rust-调用方) 或 [AG-UI HTTP](ag-ui.md#http-接入)
3. **修改内核**<br>
   [模块与依赖](architecture.md#领域与依赖) → [持久授权](authority.md) → [本地验证](setup.md#检查与演示)

[接入客户端](#接入客户端)　·　[模型、工具与 Agent](#模型工具与-agent)　·　[本机执行与隔离](#本机执行与隔离)　·　[内核与开发](#内核与开发)

## 接入客户端

用应用协议提交工作、展示进度、处理审批和收取交付物。

| 要实现 | 查阅 |
| --- | --- |
| **建立连接** | [应用协议 0.1](protocol.md) · [JSON Schema](../protocol/fliuno.schema.json) |
| **展示工作台与待审批** | [工作台实时订阅](protocol.md#工作台实时订阅) · [待审批快照与续页](protocol.md#待审批快照) |
| **刷新变动任务** | [任务变动回放](protocol.md#任务变动回放) · [批量读取任务摘要](protocol.md#批量读取任务摘要) |
| **订阅进度与断线恢复** | [Rust 客户端](protocol.md#rust-调用方) · [AG-UI SSE 续传](ag-ui.md#sse-输出与续传) |
| **走 HTTP 提交、审批与续跑** | [挂载 AG-UI 路由](ag-ui.md#http-接入) · [HTTP 演示](../examples/ag_ui_http_demo.py) |
| **浏览会话或定位任务** | [历史分页](context.md#会话历史分页) · [搜索接口](protocol.md#查找历史任务) |
| **下载交付物与统计用量** | [产物读取与校验](protocol.md#审批事件与产物) · [模型用量分页](protocol.md#模型用量) |

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
