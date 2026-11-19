<p align="center">
  <picture>
    <source media="(max-width: 600px) and (prefers-color-scheme: light)" srcset="docs/assets/fliuno-cover-compact-light.svg">
    <source media="(max-width: 600px)" srcset="docs/assets/fliuno-cover-compact.svg">
    <source media="(prefers-color-scheme: light)" srcset="docs/assets/fliuno-cover-light.svg">
    <img src="docs/assets/fliuno-cover.svg" alt="Fliuno — 提交任务、审批操作、收取交付物" width="960">
  </picture>
</p>

# Fliuno · 面向工作任务的 Rust Agent 内核

Fliuno 为桌面工作台和宿主应用运行任务。在执行前向用户请求所需授权，把进度、审批和交付物保存在本机，方便宿主在重连后继续读取。

**[运行离线演示](#跑一次离线任务)** · **[接入客户端](#接入自己的客户端)** · **[浏览文档](docs/README.md)**

`0.0.3` 开发中 · [已发布 0.0.2](CHANGELOG.md#002) · [应用协议 0.1](docs/protocol.md) · [Windows / Linux](docs/sandbox.md)

## 快速开始

### 跑一次离线任务

在仓库根目录运行。需要 **Rust 1.89+**、平台的 C/C++ 构建工具和 Python。首次运行会构建应用服务，演示使用内置模型与临时数据库，无需 API Key。

```sh
python examples/demo.py
```

```text
Task: waiting_approval
Proposed tool: artifact_create
Result: completed
Artifacts: 1
```

脚本先验证任务在 `waiting_approval` 时没有交付物，再允许一次 `artifact_create`，确认任务完成并保存了产物。运行 `python examples/demo.py --deny` 可以验证另一条路径：任务结束为 `failed`，产物为 `0`。

[安装与配置](docs/setup.md#快速开始) · [示例源码](examples/demo.py)

### 接入自己的客户端

把应用服务作为子进程启动，通过 **stdin** 逐行发送 JSON-RPC，通过 **stdout** 读取响应和通知。

```sh
cargo run -p fliuno-app-server -- --db .fliuno/kernel.db
```

```json
{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"0.1","clientName":"my-client"}}
```

读到 `capabilities` 后，用 `workspace/open` → `session/create` → `task/start` 提交第一项工作。

[完整接入步骤](docs/setup.md#再启动自己的服务) · [Rust 客户端](docs/protocol.md#rust-调用方) · [AG-UI HTTP / SSE](docs/ag-ui.md#http-接入)

## 一项任务如何运行

| 阶段 | 内核做什么 |
| --- | --- |
| **01 · 提交** | 在指定工作区和会话中运行任务，可按配置使用模型或外部 Agent。 |
| **02 · 审批** | 操作需要授权时暂停任务。宿主展示提议，由用户允许或拒绝。 |
| **03 · 交付** | 保存结果与执行事件。客户端按游标恢复进度，按目录收取交付物。 |

## 按用途查文档

| 如果你在做… | 建议接着看 |
| --- | --- |
| **桌面工作台** | [任务、审批和交付物的接入顺序](docs/README.md#工作台要展示什么) |
| **模型或工具集成** | [OpenAI、Anthropic、Gemini、MCP、ACP、A2A](docs/README.md#模型工具与-agent) |
| **本机执行** | [命令注册、终端和平台隔离](docs/README.md#本机执行与隔离) |

## 开发与验证

```powershell
pwsh -File scripts/check.ps1
```

这条命令检查格式、Clippy、工作区测试、应用协议 Schema 和离线演示。

[内核架构](docs/architecture.md) · [验收与性能记录](docs/execution-evidence.md) · [开发路线图](docs/roadmap.md) · [更新记录](CHANGELOG.md)
