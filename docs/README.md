![Fliuno Documentation](assets/fliuno-docs.svg)

# Fliuno 文档中心

[项目首页](../README.md) · [更新记录](../CHANGELOG.md)

第一次使用 Fliuno，可以先[跑一次离线任务](../README.md#跑一次离线任务)。接入真实模型或配置本机服务时，查阅[运行与配置](setup.md)。

[接入客户端](#接入客户端) · [模型、工具与 Agent](#模型工具与-agent) · [本机执行与隔离](#本机执行与隔离) · [内核与开发](#内核与开发)

## 接入客户端

| 文档 | 内容 |
| --- | --- |
| [应用协议 0.1](protocol.md) · [JSON Schema](../protocol/fliuno.schema.json) | stdio 握手、RPC 方法、审批、产物和事件回放 |
| [会话历史分页](context.md#会话历史分页) | 用固定水位读取已完成对话和交付物引用 |
| [Rust 客户端](protocol.md#rust-调用方) | 类型化调用、订阅和回放交接、分段下载和摘要校验 |
| [AG-UI 事件投影](ag-ui.md) | 把已提交的任务生命周期、完成文本和自定义事件转成 AG-UI 对象 |

## 模型、工具与 Agent

| 文档 | 内容 |
| --- | --- |
| [模型配置](setup.md#真实模型) | 接入 OpenAI、Anthropic、Gemini，启用文本流并配置请求预算 |
| [MCP](mcp.md) · [MCP 资源、提示与技能](mcp-content.md) | 连接工具服务，读取资源和提示，监听目录与资源更新 |
| [ACP](acp.md) | 启动本机 Agent、加载会话历史，并代理受控文件访问 |
| [A2A](a2a.md) | 提交远端任务、跟踪状态、显式续接并导入附件 |
| [Agent Skills](skills.md) | 发现本机技能，按需读取指令和资源 |

## 本机执行与隔离

| 文档 | 内容 |
| --- | --- |
| [宿主配置](host-configuration.md) | 启动参数、配置文件读取和大小限制 |
| [原生隔离](sandbox.md) · [运行时伴随文件](runtime-bundles.md) | Windows 与 Linux 的进程隔离、资源预算和依赖文件 |
| [注册身份与持久授权](authority.md) | 注册配置变化后如何处理旧审批和排队任务 |
| [终端订阅与恢复](terminal-subscriptions.md) · [终端归档](terminal-archive.md) | PTY 字节游标、断线续读、输出回收和创建身份保留 |

## 内核与开发

| 文档 | 内容 |
| --- | --- |
| [内核架构](architecture.md) | Rust 模块、任务状态机、事务与权限边界 |
| [会话上下文](context.md) | 历史选择、冻结快照和模型请求预算 |
| [路线图](roadmap.md) · [更新记录](../CHANGELOG.md) | 正在开发的内核能力、后续计划和已发布版本 |
| [验收与性能记录](execution-evidence.md) · [检查与演示](setup.md#检查与演示) | Windows 与 Debian 的实测、性能测量和复现命令 |
| [协议调研](research.md) | 模型、工具、Agent 和系统隔离的选型依据 |

[返回文档顶部](#fliuno-文档中心)
