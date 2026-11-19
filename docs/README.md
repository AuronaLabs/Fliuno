<p align="center">
  <img src="assets/fliuno-docs.svg" alt="Fliuno 文档：运行演示、接入客户端、开发内核" width="960">
</p>

# Fliuno 文档中心

[项目首页](../README.md) · [更新记录](../CHANGELOG.md) · [开发路线图](roadmap.md)

## 从这里开始

| 目标 | 阅读顺序 |
| --- | --- |
| **01 · 运行演示** | [运行离线任务](../README.md#跑一次离线任务) → [配置环境](setup.md) → [换成真实模型](setup.md#真实模型) |
| **02 · 接入应用** | [完成握手](protocol.md#传输) → [Rust 客户端](protocol.md#rust-调用方) 或 [AG-UI HTTP](ag-ui.md#http-接入) |
| **03 · 内核开发** | [读模块图](architecture.md#领域与依赖) → [理解授权](authority.md) → [运行检查](setup.md#检查与演示) |

[接入客户端](#接入客户端) · [模型、工具与 Agent](#模型工具与-agent) · [本机执行与隔离](#本机执行与隔离) · [内核与开发](#内核与开发)

## 接入客户端

| 想实现 | 查阅 |
| --- | --- |
| 建立连接并调用接口 | [应用协议 0.1](protocol.md) · [JSON Schema](../protocol/fliuno.schema.json) |
| 打开工作台并处理审批 | [工作台快照](protocol.md#工作台快照) · [待审批快照与续页](protocol.md#待审批快照) |
| 订阅事件或断线后追上进度 | [Rust 客户端](protocol.md#rust-调用方) · [AG-UI SSE 续传](ag-ui.md#sse-输出与续传) |
| 通过 HTTP 提交任务、审批并续跑 | [AG-UI 宿主接入](ag-ui.md#http-接入) · [本机 HTTP 演示](../examples/ag_ui_http_demo.py) |
| 浏览会话历史和查找任务 | [历史分页](context.md#会话历史分页) · [搜索接口](protocol.md#查找历史任务) |
| 下载交付物或统计模型用量 | [产物读取与校验](protocol.md#审批事件与产物) · [模型用量分页](protocol.md#模型用量) |

## 模型、工具与 Agent

| 接入方式 | 用来做什么 |
| --- | --- |
| [模型配置](setup.md#真实模型) | 使用 OpenAI、Anthropic、Gemini 的流式输出与请求预算 |
| [MCP](mcp.md) · [资源、提示与技能](mcp-content.md) | 调用外部工具，读取资源与提示，监听变更 |
| [ACP](acp.md) | 启动本机 Agent，加载会话历史，代理受控文件访问 |
| [A2A](a2a.md) | 提交远端任务、跟踪状态、显式续接和导入附件 |
| [Agent Skills](skills.md) | 发现本机技能，按需读取指令和资源 |

## 本机执行与隔离

| 文档 | 查什么 |
| --- | --- |
| [宿主配置](host-configuration.md) | 启动时读取配置并注册执行能力 |
| [原生隔离](sandbox.md) · [运行时伴随文件](runtime-bundles.md) | Windows、Linux 的进程隔离、资源预算和依赖文件 |
| [注册身份与持久授权](authority.md) | 配置变化后如何处理旧审批和排队任务 |
| [终端订阅与恢复](terminal-subscriptions.md) · [终端归档](terminal-archive.md) | PTY 字节游标、断线续读、输出回收和创建身份 |

## 内核与开发

| 文档 | 查什么 |
| --- | --- |
| [内核架构](architecture.md) | Rust 模块、任务状态机、事务与权限边界 |
| [会话上下文](context.md) | 历史选择、冻结快照和模型请求预算 |
| [开发路线图](roadmap.md) · [更新记录](../CHANGELOG.md) | 开发中的能力、后续计划和已发布版本 |
| [验收与性能记录](execution-evidence.md) · [检查与演示](setup.md#检查与演示) | Windows 与 Debian 实测、性能测量和复现命令 |
| [协议调研](research.md) | 模型、工具、Agent 和系统隔离的选型依据 |

[回到阅读入口](#从这里开始) · [返回项目首页](../README.md)
