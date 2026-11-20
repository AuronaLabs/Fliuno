<p align="center">
  <picture>
    <source media="(max-width: 600px) and (prefers-color-scheme: light)" srcset="docs/assets/fliuno-cover-compact-light.svg">
    <source media="(max-width: 600px)" srcset="docs/assets/fliuno-cover-compact.svg">
    <source media="(prefers-color-scheme: light)" srcset="docs/assets/fliuno-cover-light.svg">
    <img src="docs/assets/fliuno-cover.svg" alt="本机任务轨迹：提交任务、等待审批、保存交付物" width="1120">
  </picture>
</p>

# Fliuno

**Fliuno 是给桌面工作台和其他宿主应用使用的本机 Rust Agent 内核。** 应用可以提交任务、展示待审批操作和收取交付物。内核用 SQLite 保存进度与结果，客户端重连后可从上次的游标继续读取。

**[运行离线演示](#快速开始)** · **[文档中心](docs/README.md)** · [内核架构](docs/architecture.md)

开发中：[`0.0.3`](docs/roadmap.md#003-开发中) · 已发布：[`0.0.2`](CHANGELOG.md#002)

## 快速开始

### 跑一次离线任务

在仓库根目录运行。需要 [Rust 1.89+、C/C++ 构建工具和 Python](docs/setup.md#快速开始)，无需 API Key。脚本会使用临时数据库，自动提交任务并批准一次 `artifact_create`。

```sh
python examples/demo.py
```

输出包含以下四行，随后会打印事件回放：

```text
Task: waiting_approval
Proposed tool: artifact_create
Result: completed
Artifacts: 1
```

脚本也会检查审批前没有交付物。运行 `python examples/demo.py --deny` 可验证拒绝路径：任务结束为 `failed`，交付物为 `0`。[查看演示源码](examples/demo.py)

### 接入自己的客户端

把应用服务作为子进程启动，通过 **stdin** 逐行发送 JSON-RPC，通过 **stdout** 读取响应和通知。

```sh
cargo run -p fliuno-app-server -- --db .fliuno/kernel.db
```

`initialize` 握手后，依次调用 `workspace/open` → `session/create` → `task/start`。

[完整接入步骤](docs/setup.md#再启动自己的服务) · [工作台接入顺序](docs/README.md#工作台要展示什么) · [AG-UI HTTP / SSE](docs/ag-ui.md#http-接入)

## 内核提供什么

| 能力 | 文档 |
| --- | --- |
| **任务、审批与交付**：恢复会话和任务，回放事件，下载交付物 | [工作台接入](docs/README.md#工作台要展示什么) |
| **模型、工具与 Agent**：配置 OpenAI、Anthropic 或 Gemini，接入 MCP、ACP 与 A2A | [集成方式](docs/README.md#模型工具与-agent) |
| **本机命令与终端**：由宿主注册能力，按平台配置进程隔离 | [Windows 与 Linux 执行](docs/README.md#本机执行与隔离) |

## 开发与验证

```powershell
pwsh -NoProfile -File scripts/check.ps1
```

这条命令检查格式、Clippy、工作区测试、应用协议 Schema 和离线演示。

[验收与性能记录](docs/execution-evidence.md) · [更新记录](CHANGELOG.md)
