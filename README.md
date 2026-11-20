<p align="center">
  <picture>
    <source media="(max-width: 600px) and (prefers-color-scheme: light)" srcset="docs/assets/fliuno-cover-compact-light.svg">
    <source media="(max-width: 600px)" srcset="docs/assets/fliuno-cover-compact.svg">
    <source media="(prefers-color-scheme: light)" srcset="docs/assets/fliuno-cover-light.svg">
    <img src="docs/assets/fliuno-cover.svg" alt="Fliuno 的任务流程：提交任务、审批操作、查看交付物。" width="1120">
  </picture>
</p>

# Fliuno

**面向桌面工作台的本机 Rust Agent 内核。** 当工作台提交一项任务，Fliuno 负责调用模型和工具、等待用户审批，并保存交付物。任务与事件写入 SQLite，客户端断线后可以从上次的游标继续读。

**[快速开始](#快速开始)** · [接入客户端](#接入自己的客户端) · [文档中心](docs/README.md)

**版本**　[v0.0.2 发布记录](CHANGELOG.md#002) · [v0.0.3 开发中](CHANGELOG.md#003-开发中)

## 快速开始

### 跑一次离线任务

在仓库根目录运行。需要 Rust 1.89+、C/C++ 构建工具和 Python，无需模型 API Key。[准备环境](docs/setup.md#快速开始)

```sh
python examples/demo.py
```

演示使用临时数据库，依次提交任务、批准操作并读取交付物。

```text
Task: waiting_approval
Proposed tool: artifact_create
Result: completed
Artifacts: 1
```

加上 `--deny` 可体验拒绝流程。任务以 `failed` 结束，交付物数量为 `0`。[演示源码](examples/demo.py)

### 接入自己的客户端

将服务作为本机子进程启动。客户端通过 **stdin** 逐行发送 JSON-RPC，通过 **stdout** 读取响应和通知。

```sh
cargo run -p fliuno-app-server -- --db .fliuno/kernel.db
```

先调用 `initialize` 握手，再依次打开工作区、创建会话和提交任务。

`workspace/open` → `session/create` → `task/start`

[启动与握手示例](docs/setup.md#再启动自己的服务) · [Rust 客户端与 AG-UI HTTP / SSE](docs/README.md#连接方式)

## 内核提供什么

| 能力 | 工作台可以做什么 |
| --- | --- |
| **任务与审批** | 展示进度、待审批操作和执行结果。[工作台接入](docs/README.md#工作台要展示什么) |
| **历史与重连** | 补读断线期间的变动，把预览过的历史范围固定到新任务。[恢复同步](docs/README.md#重连与恢复) · [会话上下文](docs/context.md#提交任务时固定预览范围) |
| **交付物** | 浏览和下载文件，把同一会话的文本报告、图片或 PDF 交给下一项任务。[交付物目录](docs/protocol.md#交付物目录订阅) · [复用交付物](docs/protocol.md#将交付物用作任务输入) |
| **模型、工具与 Agent** | 连接 OpenAI、Anthropic 或 Gemini，通过 MCP、ACP 或 A2A 扩展能力。[集成方式](docs/README.md#模型工具与-agent) |
| **本机命令与终端** | 运行宿主注册的命令，按 Windows 或 Linux 的平台条件启用进程隔离。[本机执行与隔离](docs/README.md#本机执行与隔离) |

## 开发与验证

```powershell
pwsh -NoProfile -File scripts/check.ps1
```

这条命令检查格式、Clippy、工作区测试、应用协议 Schema 和离线演示。

[内核架构](docs/architecture.md) · [验收与性能](docs/execution-evidence.md) · [版本升级验证](docs/setup.md#版本升级验证) · [开发路线图](docs/roadmap.md)
