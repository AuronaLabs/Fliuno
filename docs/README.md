# Fliuno 文档

[项目首页](../README.md) · [版本更新记录](../CHANGELOG.md)

| 我想做什么 | 从这里开始 |
| --- | --- |
| 跑一个离线任务或配置真实模型 | [运行与配置](setup.md) |
| 接入桌面端、CLI 或 Rust 客户端 | [应用协议](protocol.md) · [JSON Schema](../protocol/fliuno.schema.json) |
| 看懂任务、审批和重启恢复 | [内核架构](architecture.md) · [会话上下文](context.md) |
| 了解本机执行与文件权限 | [沙箱](sandbox.md) · [注册身份](authority.md) |

[AG-UI 事件接入](ag-ui.md)说明如何把持久任务、审批和交付物接到上层客户端。

## 接入模型和工具

| 文档 | 内容 |
| --- | --- |
| [模型配置](setup.md#真实模型) | OpenAI、Anthropic、Gemini，以及文本流和请求预算 |
| [MCP](mcp.md) · [资源、提示和监听](mcp-content.md) | stdio、Streamable HTTP、工具目录及资源读取 |
| [ACP](acp.md) | 启动本机 Agent、加载历史、代理文件操作和续接会话 |
| [A2A](a2a.md) | 提交和跟踪远端任务、流式状态、续接与导入附件 |
| [Agent Skills](skills.md) | 技能发现、指令和资源读取 |

## 本机运行与恢复

| 文档 | 内容 |
| --- | --- |
| [宿主配置](host-configuration.md) | 配置文件读取、大小限制和启动顺序 |
| [沙箱与原生进程](sandbox.md) · [运行时伴随文件](runtime-bundles.md) | Windows 与 Linux 的隔离、资源预算、固定摘要和回收 |
| [终端订阅](terminal-subscriptions.md) | PTY 原始输出、字节游标、断线续读和保留窗口 |
| [注册身份绑定](authority.md) | 配置变化时如何处理旧审批和排队任务 |
| [会话上下文](context.md) | 选择历史、冻结快照和设置模型预算 |

## 开发与版本

[路线图](roadmap.md)记录已完成工作和后续计划。[验收记录](execution-evidence.md)包含 Windows 与 Debian 的测试范围、性能测量和日志位置。[协议调研](research.md)记录各协议的取舍与来源。
