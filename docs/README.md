<p align="center">
  <img src="assets/fliuno-docs.svg" alt="Fliuno Field Guide" width="960">
</p>

# Fliuno 文档中心

[项目首页](../README.md) · [更新记录](../CHANGELOG.md)

1. **第一次运行**：[跑离线任务](../README.md#跑一次离线任务) → [配置运行环境](setup.md) → [接入真实模型](setup.md#真实模型)
2. **开发客户端或宿主**：[stdio 握手](protocol.md#传输) → [Rust 客户端](protocol.md#rust-调用方) 或 [AG-UI HTTP](ag-ui.md#http-接入)
3. **理解或修改内核**：[内核架构](architecture.md) → [注册身份与授权](authority.md) → [验收记录](execution-evidence.md)

[接入客户端](#接入客户端) · [模型、工具与 Agent](#模型工具与-agent) · [本机执行与隔离](#本机执行与隔离) · [内核与开发](#内核与开发)

## 接入客户端

从 `initialize` 读取服务能力，再选用本机 stdio、Rust 客户端或宿主 HTTP 接口。

| 文档 | 查什么 |
| --- | --- |
| [应用协议 0.1](protocol.md) · [JSON Schema](../protocol/fliuno.schema.json) | 握手、任务、审批、事件和产物的接口定义 |
| [Rust 客户端](protocol.md#rust-调用方) | 类型化调用、订阅与回放交接、分段下载和摘要校验 |
| [待审批收件箱](protocol.md#待审批快照) | 分页读取同一份待审批快照，并从对应的事件水位接上更新 |
| [模型用量](protocol.md#模型用量) | 按任务、会话或工作区分页读取供应商报告的 token 用量 |
| [AG-UI](ag-ui.md) · [本机 HTTP 演示](../examples/ag_ui_http_demo.py) | 提交文本任务、审批后续跑、通过 SSE 断线续传 |
| [会话历史与检索](context.md#会话历史分页) · [搜索接口](protocol.md#查找历史任务) | 用固定水位读取已完成对话，按文本定位历史任务 |

## 模型、工具与 Agent

先选任务需要的模型，再按需接入外部工具或把任务交给 Agent。

| 接入方式 | 查什么 |
| --- | --- |
| [模型配置](setup.md#真实模型) | OpenAI、Anthropic、Gemini 的流式输出与请求预算 |
| [MCP](mcp.md) · [资源、提示与技能](mcp-content.md) | 连接外部工具，读取资源与提示，监听变更 |
| [ACP](acp.md) | 启动本机 Agent、加载会话历史、代理受控文件访问 |
| [A2A](a2a.md) | 提交远端任务、跟踪状态、显式续接和导入附件 |
| [Agent Skills](skills.md) | 发现本机技能，按需读取指令和资源 |

## 本机执行与隔离

| 文档 | 查什么 |
| --- | --- |
| [宿主配置](host-configuration.md) | 读取配置文件、注册执行能力和配置大小限制 |
| [原生隔离](sandbox.md) · [运行时伴随文件](runtime-bundles.md) | Windows、Linux 的进程隔离、资源预算和依赖文件 |
| [注册身份与持久授权](authority.md) | 注册配置变化后如何处理旧审批与排队任务 |
| [终端订阅与恢复](terminal-subscriptions.md) · [终端归档](terminal-archive.md) | PTY 字节游标、断线续读、输出回收和创建身份 |

## 内核与开发

| 文档 | 查什么 |
| --- | --- |
| [内核架构](architecture.md) | Rust 模块、任务状态机、事务与权限边界 |
| [会话上下文](context.md) | 历史选择、冻结快照与模型请求预算 |
| [路线图](roadmap.md) · [更新记录](../CHANGELOG.md) | 开发中的能力、后续计划与已发布版本 |
| [验收与性能记录](execution-evidence.md) · [检查与演示](setup.md#检查与演示) | Windows 与 Debian 实测、性能测量和复现命令 |
| [协议调研](research.md) | 模型、工具、Agent 和系统隔离的选型依据 |

[返回文档顶部](#fliuno-文档中心) · [返回项目首页](../README.md)
