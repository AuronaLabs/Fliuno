<p align="center">
  <picture>
    <source media="(max-width: 600px) and (prefers-color-scheme: light)" srcset="docs/assets/fliuno-cover-compact-light.svg">
    <source media="(max-width: 600px)" srcset="docs/assets/fliuno-cover-compact.svg">
    <source media="(prefers-color-scheme: light)" srcset="docs/assets/fliuno-cover-light.svg">
    <img src="docs/assets/fliuno-cover.svg" alt="任务流程：提交、审批、交付" width="960">
  </picture>
</p>

# Fliuno

**面向桌面工作台和宿主应用的 Rust Agent 内核。** 提交任务、审批操作、收取交付物。执行进度与结果保存在本机，客户端重连后可按游标继续读取。

**[快速开始](#快速开始)** · **[文档中心](docs/README.md)** · [内核架构](docs/architecture.md)

`0.0.3` 开发中 · [已发布 0.0.2](CHANGELOG.md#002) · [开发路线图](docs/roadmap.md)

## 快速开始

### 跑一次离线任务

在仓库根目录运行。需要 [Rust 1.89+、C/C++ 构建工具和 Python](docs/setup.md#快速开始)，无需 API Key。

```sh
python examples/demo.py
```

演示先停在待审批状态，允许一次 `artifact_create` 后完成任务并保留一个交付物。

<details>
<summary>预期输出与拒绝审批的演示</summary>

```text
Task: waiting_approval
Proposed tool: artifact_create
Result: completed
Artifacts: 1
```

脚本会验证审批前没有交付物。加上 `--deny` 可测试拒绝路径：任务结束为 `failed`，产物为 `0`。

[示例源码](examples/demo.py)

</details>

### 接入自己的客户端

把应用服务作为子进程启动，通过 **stdin** 逐行发送 JSON-RPC，通过 **stdout** 读取响应和通知。

```sh
cargo run -p fliuno-app-server -- --db .fliuno/kernel.db
```

`initialize` 握手后，依次调用 `workspace/open` → `session/create` → `task/start`。宿主可用同一套接口展示待审批操作、订阅执行进度和取回交付物。

[完整接入步骤](docs/setup.md#再启动自己的服务) · [工作台接入顺序](docs/README.md#工作台要展示什么) · [AG-UI HTTP / SSE](docs/ag-ui.md#http-接入)

## 能力一览

- **任务与交付**：在会话中提交与恢复任务，展示审批，跟踪交付物。[工作台接入](docs/README.md#工作台要展示什么)
- **模型和工具**：连接 OpenAI、Anthropic、Gemini，或通过 MCP、ACP、A2A 接入外部能力。[集成索引](docs/README.md#模型工具与-agent)
- **本机执行**：由宿主注册命令和终端，并按平台配置进程隔离。[Windows / Linux 执行](docs/README.md#本机执行与隔离)

## 开发与验证

```powershell
pwsh -NoProfile -File scripts/check.ps1
```

这条命令检查格式、Clippy、工作区测试、应用协议 Schema 和离线演示。

[验收与性能记录](docs/execution-evidence.md) · [更新记录](CHANGELOG.md)
