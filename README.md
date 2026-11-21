<p align="center">
  <picture>
    <source media="(max-width: 600px) and (prefers-color-scheme: light)" srcset="docs/assets/fliuno-cover-compact-light.svg">
    <source media="(max-width: 600px)" srcset="docs/assets/fliuno-cover-compact.svg">
    <source media="(prefers-color-scheme: light)" srcset="docs/assets/fliuno-cover-light.svg">
    <img src="docs/assets/fliuno-cover.svg" alt="Fliuno 的任务流程：提交任务、审批操作、查看交付物。" width="1120">
  </picture>
</p>

# Fliuno

**面向工作台的本机 Rust Agent 内核。** Fliuno 驱动任务执行、逐次审批与交付物保存，并将状态和事件持久化到 SQLite，供客户端断线后按游标恢复。

`Rust 1.89+` · `JSON-RPC 0.1` · `本机优先`

**[快速体验](#快速体验)** · [接入工作台](#接入工作台) · [文档中心](docs/README.md)

当前版本 **v0.0.4** · [更新记录](CHANGELOG.md#004) · [v0.0.3](CHANGELOG.md#003) · [v0.0.2](CHANGELOG.md#002)

## 快速体验

在仓库根目录运行离线示例。需要 Rust 1.89+、C/C++ 构建工具和 Python；此流程不需要模型 API Key。[环境说明](docs/setup.md#快速开始)

```sh
python examples/demo.py
```

示例使用临时数据库，走完任务提交、审批和读取交付物：

```text
Task: waiting_approval
Proposed tool: artifact_create
Result: completed
Artifacts: 1
```

加上 `--deny` 可以检查拒绝路径。任务以 `failed` 结束，不会产生交付物。[演示源码](examples/demo.py)

## 接入工作台

把服务作为本机子进程启动。客户端通过 **stdin** 逐行发送 JSON-RPC，通过 **stdout** 接收响应与通知。

```sh
cargo run -p fliuno-app-server -- --db .fliuno/kernel.db
```

调用 `initialize` 握手后，按工作区、会话、任务的顺序进入执行流程：

`workspace/open` → `session/create` → `task/start` → `approval/respond` → `artifacts/list`

[启动与握手](docs/setup.md#再启动自己的服务) · [Rust 客户端和 HTTP / SSE 接入](docs/README.md#连接方式) · [应用协议参考](docs/protocol.md)

## 内核能力

| 能力 | 说明 |
| --- | --- |
| **任务与 Agent 调度** | SQLite 持久化任务、审批、交付物和 Run 依赖图，支持并发调度、期限和断线续读。[Run 协议](docs/protocol.md#持久-run-dag) |
| **模型与上下文** | OpenAI、Anthropic、Gemini 支持 Token 预检和自动上下文压缩。会话可检索工作区长期记忆。[会话上下文](docs/context.md) · [工作区记忆](docs/memory.md) |
| **工具与外部 Agent** | 兼容两代 MCP，包括 Tasks、OAuth 和资源监听，并接入 ACP 和 A2A。[MCP 兼容与配置](docs/mcp.md) |
| **本机执行** | 由宿主注册工作区文件、命令和终端能力；支持的平台可启用进程隔离。 |
| **恢复与诊断** | 提供 SQLite 一致性备份、独立路径还原、JSONL 追踪和可选的 OTLP 导出。[备份](docs/backup.md) · [执行诊断](docs/setup.md#执行诊断) |
| **客户端协议** | 提供 stdio JSON-RPC、Rust 客户端及可嵌入的 AG-UI HTTP / SSE 路由。 |

## 开发与验证

```powershell
pwsh -NoProfile -File scripts/check.ps1
```

工程检查覆盖格式、Clippy、工作区测试、应用协议 Schema 和离线演示。

[架构与模块](docs/architecture.md) · [验收记录](docs/execution-evidence.md) · [数据库升级验证](docs/setup.md#版本升级验证) · [开发路线图](docs/roadmap.md)
