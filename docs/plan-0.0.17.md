# 0.0.17 开发与发布计划

[开发路线图](roadmap.md) · [工作台使用指南](workbench.md) · [工程验收记录](execution-evidence.md)

v0.0.16 是当前稳定基线。下一版应让用户在 Windows 和 Debian 上安装、配置自己的模型并直接开始对话。日常操作从输入与回复开始，审批、历史和交付物在需要时进入。此前单独排期的交付物和连接性能改进在本版一起验收。按可审查的功能块提交，完成整版验收后再打标签和发版。

## 调研与取舍

| 资料及本地检出 | 对 Fliuno 的具体影响 |
| --- | --- |
| [Codex CLI](https://developers.openai.com/codex/cli)、[认证](https://developers.openai.com/codex/auth)、[app-server 的历史分页](https://developers.openai.com/codex/app-server)与本地 `codex-rs/tui/src/onboarding`、`chatwidget/empty_state_policy.rs`、`thread_transcript.rs` | 将首次配置放在服务启动前。配置后回到当前工作目录的对话和输入区，历史和实时回复使用同一套消息组件。Fliuno 使用自己的界面、模型选择和文件配置。 |
| [Codex 发布流程](https://github.com/openai/codex/blob/a1604a8c0554c09c1be45e5a121bb1889b4d9a50/.github/workflows/rust-release.yml) 与[原生 Windows 构建](https://github.com/openai/codex/blob/a1604a8c0554c09c1be45e5a121bb1889b4d9a50/.github/workflows/rust-release-windows.yml)，本地 `D:\Fliuno-References\codex`，提交 `a1604a8` | 核对发布版本与 Cargo 元数据，按平台构建，把 TUI 和它实际启动的 app-server 一起交付并验收。 |
| [OpenCode 产物核验](https://github.com/anomalyco/opencode/blob/d284e7ce17d0a98c80d0bebd5cc6dce2dbdd5f74/.github/release/verify-artifacts.sh)及[发布流程](https://github.com/anomalyco/opencode/blob/d284e7ce17d0a98c80d0bebd5cc6dce2dbdd5f74/.github/workflows/release.yml)，本地 `D:\Fliuno-References\opencode`，提交 `d284e7c` | 用相同字节执行验收和发布。发布前核对资产清单和 SHA-256，拒绝缺失或额外产物。 |
| [Claude Code 安装文档](https://code.claude.com/docs/en/setup)与[首次使用指引](https://code.claude.com/docs/en/quickstart)、[交互模式](https://code.claude.com/docs/en/interactive-mode)，本地 `D:\Fliuno-References\claude-code`，提交 `2771d55` | 为每个平台提供安装、更新、卸载和诊断入口。首版让 Windows Installer 和 Debian 包管理器处理显式升级，配置继续由用户目录中的文件管理。 |
| [WiX 主版本升级](https://docs.firegiant.com/wix/schema/wxs/majorupgrade/)与 [Debian 二进制包规范](https://www.debian.org/doc/debian-policy/ch-binary.html) | Windows x64 使用带固定 UpgradeCode 的 MSI，安装两个程序并注册 PATH。Debian 12+ amd64 使用 `.deb`，由包管理器记录 `/usr/bin` 中的程序与文档。ZIP 和 tar.gz 保留给便携用户。 |
| [GitHub Actions 构建证明](https://docs.github.com/en/actions/how-tos/secure-your-work/use-artifact-attestations/use-artifact-attestations)与[不可变发布](https://docs.github.com/en/code-security/concepts/supply-chain-security/immutable-releases) | 候选构建与验证先以只读工作流运行。启用可落实的构建证明后，正式发布从已验收的候选制品组装草稿，并核对远端资产摘要再公开。Windows 代码签名与 Actions 构建证明分别记录。 |

## 版本范围

| 板块 | 交付内容 | 退出条件 |
| --- | --- | --- |
| 首次使用与对话 | 裸启动引导选择模型供应商、填写模型 ID 和已有凭据文件。保存后进入当前项目并聚焦输入区。已经配置的工作区直接打开。主屏接续既有会话的多轮消息，审批和单任务详情随当前操作展开。离线演示通过 `--demo` 启动。 | 实际终端验证首次启动、无效路径、取消不留配置、保存后启动、重启读回、已有配置不覆盖、诊断能定位问题、真实回环模型至少完成两轮回复，重启后按原顺序显示历史。 |
| 跨任务交付物 | 工作区目录、媒体类型筛选和快照分页、来源会话及版本预览、保存回执。 | 双平台真实终端验证跨会话隔离、前后版本切换、重启后浏览、多页文件清单、已存在目标的保留和保存字节的 SHA-256。 |
| 终端可靠性与性能 | 测量空闲刷新、长历史打开、活动任务与断线追赶。优先合并重复刷新并减少空闲 RPC，保留任务完成、审批、状态变更的及时显示。 | 同机同种子比较 v0.0.16 和候选版本，记录空闲 60 秒请求数、首屏和重连延迟、峰值内存。测试快照与增量切换时的新事件、流量突发、后台任务、终态与审批。发现退化时修复或明确记录原因与影响。 |
| TUI 布局与回归 | 审阅目录列表、预览、保存输入和提示，与当前会话界面统一。 | 通过 Windows ConPTY 和 Debian PTY 检查约 110、80、48 列，保存真实屏幕截图。核对焦点、主要操作提示、多行路径、中英文和宽字符，输入与滚动时保留正在审阅的位置。 |
| 安装与发布 | Windows x64 MSI、Debian amd64 `.deb`、两种便携归档、SHA-256 清单与验收记录。优先复用现有配置、服务定位和数据库路径。 | 从安装包运行 `fliuno --version`、`fliuno --doctor` 和真实 TUI 工作流。安装、迁移、升级、卸载与重装均通过，最终资产与已验收资产的摘要一致。 |

## 对话与连接验收

主屏以当前会话的消息为阅读单位。复用 `session/listHistory` 的固定水位和按字节分页加载已完成轮次。新一轮的用户输入、流式回复、待审批和最终回复使用同一视图，完成时用持久轮次替换预览。按任务 ID 去重，向上补页时保持当前阅读位置。任务、工作区收件箱、时间线和交付目录通过 `F1` 等入口按需打开。

首屏只加载最近一页，切换会话或工作区后忽略上一个会话的迟到页。后台完成多轮时按固定水位补齐增量。超过 96 轮的显示容量时，回到最近一页，并保留向上翻页的入口。覆盖最早页、超大轮次、提交后断线、重连期间的事件和被跳过的流式预览，核对审批在历史加载期间仍可发现和处理。用同一份数据测量冷开、恢复和静置时的请求数量，再决定轮询和订阅调整的幅度。

### 连接性能的开发验证

Windows x64 同机对照使用发布的 v0.0.16 和候选版 Release 构建，载入包含 300 轮的同源数据库。实际 ConPTY 配合本机 stdio 代理统计 RPC 方法，等待首屏稳定后记录 60 秒。`work/getSnapshot` 从 79 次降到 3 次。交替预热后的两次首屏对照分别为 489 → 371 毫秒、456 → 359 毫秒，首次启动受系统缓存影响，波动更大。首屏计时截至终端实际显示最新一轮。终止测试服务后，终端显示断线提示的时间为 340 → 24 毫秒，按 `r` 后重新加载会话为 232 → 169 毫秒。一次稳态采样中，客户端和服务的峰值工作集分别增加约 0.25 MiB 和 0.32 MiB。

候选版订阅当前会话或工作区的事件水位，合并短时间内的通知，在快照落后时刷新。通知丢失时重新订阅，低频补查覆盖漏报。模型预览可在无 RPC 在途时持续读入。Windows 已跑通 16 项终端用例，最终候选还需核对两平台的重连与安装后行为。

## 安装生命周期

1. 在空白环境中安装，使用新终端通过命令名启动。无源码、Cargo、编译器或旧 PATH 条目时，TUI 也能定位同包 app-server。Windows MSI 管理系统 PATH，Debian 包管理器管理 `/usr/bin`。两个平台均以普通用户启动应用。
2. 用 v0.0.16 的便携版预置一个含会话、历史、审批、记忆和交付物的用户目录。备份后安装候选版，读取相同数据，继续任务，并核对凭据文件和个人配置。测试里只使用本机模型夹具和假的凭据文件。
3. 用两个不同版本的测试包验证 MSI 主版本更新与 Debian 包更新。确认升级后系统只保留新版本注册，卸载不会手动清理原便携版。检查已运行的程序导致升级被阻止时，安装器能给出可操作的提示及日志。
4. 通过系统卸载移除注册和应用文件，验证 `~/.fliuno` 及自定义数据库、备份和导出文件仍存在。重新安装并打开已有会话。
5. 对便携归档执行独立启动与校验，验证包版本及两份二进制版本一致。Debian 12 上实际运行候选资产，避免由较新的构建环境悄悄抬高 glibc 要求。

## 发布门槛

- 先提交计划，分别提交首次启动与界面、交付物、连接性能、安装与验收。最终版本和更新记录跟随候选来源提交，可独立追溯每个板块。
- 锁定依赖，运行格式、Clippy、全工作区测试、协议 schema 校验和现有示例。候选验收从干净提交构建，核对安装包中的程序与通过平台测试的程序一致。
- 安装生命周期与真实 TUI 工作流均通过，记录测试系统、提交 SHA、候选资产 SHA-256、性能对比及终端截图。公开前远端资产必须与验收清单一一匹配。
- GitHub Actions 先运行可手动触发的候选构建和验证。签名状态、构建证明状态和支持的系统在版本说明中如实标出。候选失败时修复后重新构建和验收，稳定版继续使用原发布。

## 安装包构建与候选验收

[WiX 定义](../packaging/windows/Fliuno.wxs)把终端客户端和 app-server 安装到同一目录，PATH 条目随包管理。打包使用 WiX 6.0.2。先在同一检出中构建两个程序，用该检出的 Cargo 版本设定 MSI 版本。以下示例将输出放到现有外部构建目录。

```powershell
$env:CARGO_TARGET_DIR = 'D:\Fliuno-Build-Target'
cargo build --locked --release -p fliuno-tui -p fliuno-app-server --bins
$bin = Join-Path $env:CARGO_TARGET_DIR 'release'
$version = (& "$bin\fliuno.exe" --version) -replace '^fliuno ', ''
wix build -arch x64 -d "FliunoVersion=$version" -d "BinDir=$bin" `
  -out "$env:CARGO_TARGET_DIR\fliuno-v$version-windows-x64.msi" packaging/windows/Fliuno.wxs
```

Debian 在 Bookworm amd64 上构建，使用 `sh packaging/debian/build.sh target/release dist` 生成包。`dpkg-shlibdeps` 从构建产物推导系统依赖。`.deb` 由包管理器维护 `/usr/bin/fliuno` 和 `/usr/bin/fliuno-app-server`。

[候选工作流](../.github/workflows/installer-candidate.yml)手动启动，以同一提交构建并验证两平台的原生包和便携包。验收从稳定版压缩包生成升级夹具，检查安装后从 PATH 启动、升级前后的二进制摘要、卸载保留的用户文件、重装续聊，以及使用已安装二进制完成真实终端用例。候选输出包含来源提交与 SHA-256。发布从已验收的文件中选择资产。
