# 0.0.18 开发与发布计划

以 0.0.17 的首次配置、项目对话和原生安装为基线。本版要让用户在对话中完成常见的切换和操作，并能读清楚模型给出的代码与解释。任务、审批、交付物和诊断继续通过命令进入。

## 调研记录

| 一手资料与本地 Git 检出 | 对 Fliuno 的影响 |
| --- | --- |
| [Codex CLI 命令](https://developers.openai.com/codex/cli/slash-commands)，本地 `D:\Fliuno-References\codex` `a1604a8` 的 `bottom_pane/command_popup.rs`、`bottom_pane/chat_composer.rs` | 统一 `/` 和 `F1` 对命令的描述，在输入区内提示可用操作。区分键入命令、继续输入和原样粘贴。 |
| [Claude Code 交互模式](https://code.claude.com/docs/en/interactive-mode)，本地 `D:\Fliuno-References\claude-code` `2771d55` 的 `examples/settings/README.md` | 主要操作留在对话输入区，工作区配置和用户配置的来源明确可查。保留 Fliuno 的 TOML 和已有密钥文件。 |
| [OpenCode TUI](https://opencode.ai/docs/cli/tui/)，本地 `D:\Fliuno-References\opencode` `d284e7c` 的 `packages/tui/src/component/dialog-session-list.tsx`、`dialog-model.tsx` | `/resume` 聚焦当前目录下的可搜索会话。会话与模型切换都要标出当前位置，短暂连接失败时能回到当前对话。 |
| Codex `a1604a8` 的 `markdown_render.rs`、`history_cell/markdown_render_cache.rs` | 让实时与重启后的回答共享排版。缓存已完成消息在当前宽度下的渲染结果，用长历史重绘和窄屏截图检验收益。 |

Windows 凭据使用本机持久范围。核对了 [Windows 凭据规范](https://learn.microsoft.com/en-us/windows/win32/api/wincred/ns-wincred-credentialw) 和本地 Codex `a1604a8` 的 `codex-rs/keyring-store/src/lib.rs`。Debian 安装包面向普通终端与无桌面的机器，所以在当前用户配置目录创建 `0600` 密钥文件。两个平台都先保存独立凭据，再写入只含引用的 TOML。并发配置写入失败时撤回本次凭据，继续兼容 `api_key_file`。

## 开发范围

| 板块 | 从现有功能出发 | 验收 |
| --- | --- | --- |
| 输入与导航 | 复用 F1 的操作和现有 `session/list` 搜索，提供 `/new`、`/resume`、`/model`、`/help` 的可发现入口。命令菜单回到同一个输入框，保留草稿。 | 键入、全量粘贴、多行、未知命令、路由取消、连接失败、中文、窄屏和后台回复同时到达。审批与交付物通过现有能力打开。 |
| 真实配置与模型状态 | 主屏显示已连接模型和配置来源。针对首次启动仍要求预先准备密钥文件的摩擦，先验证受保护的本机密钥保存方案及可恢复写入。使用一个 TOML 兼容现存单模型配置，保留已有密钥文件选项。模型切换用明确范围的文件配置，单独验证服务重连和会话恢复。 | 无有效密钥、取消、已存在 profile、第二实例写入、配置损坏、重启、模型失败恢复。日志、错误、转录、测试截图都不含密钥。模型切换要处理执行中的任务、未发送草稿、历史任务所绑定的能力与权限，以及已选择的工作区。 |
| 阅读模型回复 | 独立组件呈现 Markdown 段落、列表、行内代码和 fenced code，保留代码原始文本，可按现有复制能力导出。对宽字符、截断标记、轻量逐字预览和完成后定型分别验收。 | 实时、历史和重启后一致；长代码、中文、窄屏、终端控制字符、未闭合围栏、长历史翻页和调整窗口宽度不破坏滚动锚点。 |
| 安装与验收 | 复用 0.0.17 的 Windows MSI 与 Debian 包验证脚本。用上一正式安装包做升级和卸载保留夹具，从同一候选提交取得安装包及便携包。 | 格式、Clippy、全工作区测试、schema 和示例过关。平台安装/升级/卸载/重装和真实终端用例过关。在 110、80、48 列保存并复核配置、对话、命令、会话切换及代码回复截图。记录版本、SHA、资产及签名状态。 |

计划先按首个可验证交付拆成独立提交。完成输入与导航后用真实终端确认动线，再据实确定模型切换的存储边界，最后完善渲染和打包。记录锁定版本下的首屏时间、长历史重绘时间与目录隔离。遇到权限或服务切换的不确定结果，以可恢复的错误路径结束当前操作。

## 模型控制与发现

配置和执行放在 `fliuno-config`、`fliuno-providers` 与本地应用服务。TUI 复用这些能力，通过 `/model` 选择已配置的 profile。对照本地 Codex `a1604a8` 的 `codex-rs/config/src/config_toml.rs` 与 OpenCode `d284e7c` 的 TUI 模型选择器。Provider 的协议、端点、模型和凭据分别配置。思考强度映射到各自原生字段。未配置时沿用服务端默认值。

- [OpenAI Models API](https://developers.openai.com/api/reference/resources/models/methods/list) 与 [Responses 推理参数](https://developers.openai.com/api/docs/guides/reasoning) 用于核对模型列表与 `reasoning.effort`。
- [Anthropic Models API](https://platform.claude.com/docs/en/api/models/list) 与 [Effort](https://platform.claude.com/docs/en/build-with-claude/effort) 用于核对分页、停止服务的模型与 `output_config.effort`。
- [Gemini Models API](https://ai.google.dev/api/models) 与 [Interactions API](https://ai.google.dev/gemini-api/docs/interactions) 用于核对列表过滤、分页与 `generation_config.thinking_level`。

通过本地 HTTP 夹具验收三个原生协议、Base URL、凭据、分页、错误脱敏、请求参数以及对持久 Run 绑定的影响。模型目录供无界面命令和本地 JSON-RPC 使用，允许在填写模型 ID 前查询。手工输入模型 ID 的路径继续保留，兼容只实现生成接口的网关。

模型目录保留服务方明确返回的能力信息。Anthropic 返回输入上限、输出上限、思考能力和档位，Gemini 返回输入输出上限与思考能力，OpenAI 的公开列表保留基础标识。参考本地 Codex `a1604a8` 的 `codex-rs/protocol/src/openai_models.rs` 和 OpenCode `d284e7c` 的 `packages/tui/src/mini/catalog.shared.ts`，客户端从结构化元数据读取能力。手填模型 ID 和显式预算继续适用于兼容网关。
