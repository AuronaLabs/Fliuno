<p align="center">
  <picture>
    <source media="(max-width: 600px) and (prefers-color-scheme: light)" srcset="docs/assets/fliuno-cover-compact-light.svg">
    <source media="(max-width: 600px)" srcset="docs/assets/fliuno-cover-compact.svg">
    <source media="(prefers-color-scheme: light)" srcset="docs/assets/fliuno-cover-light.svg">
    <img src="docs/assets/fliuno-cover.svg" alt="Fliuno：提交请求、审阅操作、查看交付结果。" width="1120">
  </picture>
</p>

# Fliuno

**在项目目录里，用终端和 AI 一起做事。** Fliuno 是本机优先的 Rust Agent 工作台。打开项目，描述要完成的改动，审阅操作与交付结果。会话、审批记录和交付物保存在本机；模型请求发往你选择的服务。

`Windows` · `Linux` · `Rust TUI`

**[开始真实工作](#开始真实工作)** · [使用指南](docs/workbench.md) · [文档中心](docs/README.md)

[下载安装包](https://github.com/AuronaLabs/Fliuno/releases/latest) · [更新记录](CHANGELOG.md) · [0.0.21 开发记录](docs/plan-0.0.21.md)

> **稳定版 v0.0.21** 已提供 Windows x64 MSI、Debian 12 amd64 `.deb` 和两套便携包。[下载](https://github.com/AuronaLabs/Fliuno/releases/tag/v0.0.21) · [安装包验收](docs/execution-evidence.md#0021-安装包验收)

## 开始真实工作

在要工作的文件夹运行 `fliuno`。首次启动会引导你选择模型、填写可选的 Base URL，并粘贴 API 密钥或选择已有密钥文件。按 `Enter` 发送，按 `Ctrl+N` 换行，在输入框键入 `/` 查找命令。`/resume` 可续接项目对话，`/model` 可切换已配置的模型。配置、会话、审批和交付物保存在本机。模型请求会发往你配置的服务。

Windows x64 可以安装 MSI，Debian amd64 可以安装 `.deb`；也可解压便携包，把 `fliuno` 和 `fliuno-app-server` 放在同一目录。[安装与校验](docs/workbench.md#安装) · [升级前备份](docs/workbench.md#从旧版升级) · [接入模型](docs/workbench.md#接入模型)

### 项目工作流

在项目目录运行 `fliuno`，例如输入：「找到订单折扣的实现，修复边界值错误，运行已配置的检查，并告诉我改了哪个文件。」Agent 可以从未知路径搜索源码、读取文件，并提交可审阅的小范围替换或补丁。写入前会停下来等你批准；审批先展示目标和改动，`Tab` 可查看工具参数。需要运行项目检查时，先由你在宿主配置登记程序，再审阅实际命令和输入文件。

完成后用 `/changes` 查看本轮确认写入的文件、补丁和检查输出。仅需一段文字时可以直接在对话里交付；只有需要下载或写文件时，才走对应的交付流程。[项目工作流](docs/workbench.md#在项目目录里工作) · [如何配置检查](docs/setup.md#在隔离环境里运行项目检查0021)

### 多供应商设置与简体中文

在输入框敲 `/settings` 后按 `Tab`，或按 `F1` 搜索 `settings`。先在 **Suppliers** 中配置 Base URL 和凭据，再切到 **Models**，给每个档案选供应商和协议，按 `F4` 查询并选取模型，或手填模型 ID，再设置思考强度。例如两个档案可以共用 `lab` 的连接，也可以各自连不同的服务。保存到 `~/.fliuno/config.toml` 后，用 `/model` 应用所选档案。旧回复仍显示当时的 `供应商/模型`。[设置步骤和配置示例](docs/workbench.md#命名供应商)

用 `/language` 或 `F1` 切换简体中文。项目主屏、命令、供应商设置、内置操作审阅和改动回执支持中文界面，语言偏好保存在本机，模型回复保留原文。[切换步骤与覆盖范围](docs/workbench.md#切换界面语言)

## 快速体验

想在配置账号前试一遍终端流程，可以选择离线演示。演示无需 API Key：

```sh
./fliuno --demo
```

Windows PowerShell 可用 `.\fliuno.exe --demo`。创建一个会话，按 `N` 写任务、`Ctrl+S` 提交；等到审批出现后，按 `Enter` 查看本次操作，选择允许一次或拒绝。完成后按 `V` 打开交付物目录。[全部快捷键和数据库位置](docs/workbench.md)

按 `Shift+I` 查看当前工作区各会话的待办，按 `Shift+H` 搜索已完成的历史并跳回原任务。按 `F1` 或 `/` 搜索当前可用的命令。主界面显示当前会话消息，按 `Tab` 展开单个任务的详情。按 `F7` 查看工作区交付物，按 `Shift+L` 浏览完整任务列表。主题可在配置文件中选择。

| 工作流 | 快捷键 |
| --- | --- |
| 浏览当前会话的任务 | `Shift+L` 打开完整列表，支持按状态筛选及翻页。 |
| 审阅计划后开始实施 | `F2` 切换计划模式，任务完成后 `Shift+P` 创建实施草稿。 |
| 安排后续工作 | `Ctrl+E` 入队，`Shift+Q` 查看并启动下一项。 |
| 探索另一条思路 | `B` 从当前会话分支，`Shift+B` 以所选的已完成任务为边界。 |
| 查看工具和预算 | `T` 打开时间线，`C` 查看上下文和记忆用量。 |

## 工程能力

| 能力 | 说明 |
| --- | --- |
| **任务协作与审批** | 模型可以发布计划和请求澄清。客户端可中途补充要求、为精确的本机只读操作设置限时授权，并在重连后查看下一步。[应用协议](docs/protocol.md#任务计划与澄清) |
| **任务与 Agent 调度** | SQLite 持久化任务、审批、交付物和 Run 依赖图，支持并发调度、期限和断线续读。[Run 协议](docs/protocol.md#持久-run-dag) |
| **模型与上下文** | OpenAI、Anthropic、Gemini 支持 Token 预检和自动上下文压缩。会话可检索工作区长期记忆。[会话上下文](docs/context.md) · [工作区记忆](docs/memory.md) |
| **工具与外部 Agent** | 兼容两代 MCP，包括 Tasks、OAuth 和资源监听，并接入 ACP 和 A2A。[MCP 兼容与配置](docs/mcp.md) |
| **本机执行** | 由宿主注册工作区文件、命令和终端能力；支持的平台可启用进程隔离。 |
| **恢复与诊断** | 提供 SQLite 一致性备份、独立路径还原、JSONL 追踪和可选的 OTLP 导出。[备份](docs/backup.md) · [执行诊断](docs/setup.md#执行诊断) |
| **客户端协议** | 提供 stdio JSON-RPC、Rust 客户端及可嵌入的 AG-UI HTTP / SSE 路由。 |

## 开发与验证

```powershell
cargo build --locked -p fliuno-tui -p fliuno-app-server
./target/debug/fliuno --demo
pwsh -NoProfile -File scripts/check.ps1
```

工程检查覆盖格式、Clippy、工作区测试、应用协议 Schema 和离线演示。调试、发布或验收可复用同一个专用构建缓存；在空间紧张时按[构建缓存说明](docs/setup.md#检查与演示)清理。实际终端验收和安装包门槛见[当前迭代计划](docs/plan-0.0.21.md#验收与发布门槛)。

[架构与模块](docs/architecture.md) · [验收记录](docs/execution-evidence.md) · [数据库升级验证](docs/setup.md#版本升级验证) · [开发路线图](docs/roadmap.md)
