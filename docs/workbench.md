# 使用 Fliuno 终端工作台

[产品首页](../README.md) · [文档中心](README.md) · [0.0.9 设计](plan-0.0.9.md)

在终端里运行 `fliuno --demo`。工作台和 `fliuno-app-server` 需在同一目录。也可通过 `--server PATH` 指定服务程序。离线演示使用固定模型夹具，审批后把提交的文字保存为交付物。

| 操作 | 按键 |
| --- | --- |
| 命令搜索 | 在工作台按 `F1` 或 `/`，输入关键字筛选当前可用操作，上下键选择，`Enter` 执行。 |
| 工作区、会话 | `W` 打开工作区列表，`O` 输入新路径；`S` 打开会话列表，列表里的 `N` 新建会话。上下键选择，`Enter` 打开，`PageDown` 读取下一页。 |
| 工作区任务中心 | `Shift+I` 打开跨会话收件箱；`1` 待处理、`2` 运行中、`3` 最近结束。选中任务按 `Enter` 回到原会话；待审批项会打开原审批详情。审批有更多页时，移到列表末尾继续读取当前快照。实时更新会刷新列表。`Esc` 返回当前会话。 |
| 历史查找 | `Shift+H` 搜索当前工作区已完成历史；结果按固定水位分页，`PageDown` 读更多，`U` 只搜用户输入，`A` 只搜助手回复，`B` 搜两者。选中结果按 `Enter` 返回任务。匹配是字面子串；列表会显示扫描量和因单条上限而跳过的记录数。 |
| 提交或协作 | `N` 新任务，`I` 补充指令。任务草稿中 `Enter` 换行，`Ctrl+S` 提交；其他表单用 `Enter` 提交，`Esc` 返回。 |
| 查看任务 | 上下键选任务，`Tab` 切换任务列表和详情，详情用上下键或 `PageUp` / `PageDown` 滚动。 |
| 处理待办 | `Enter` 或 `A` 打开审批或澄清。审批用 `Y` 允许一次、`N` 拒绝。对于内核只允许拒绝的 Agent 请求，界面只显示拒绝选项。`X` 后按 `Y` 确认取消。 |
| 查看交付 | `V` 打开目录，`Enter` 查看内容与摘要，`D` 输入目标路径保存。文本预览中 `U` 将该版用作新任务输入，`P` 打开上一版（如有）。 |
| 恢复或退出 | `R` 重新连接或显式恢复可安全重试的任务。`Q` 或 `Ctrl+C` 退出。 |

草稿支持方向键、`Home` / `End` 和组合字符删除。粘贴文字后仍需 `Ctrl+S` 提交。切换会话时，当前运行最多暂存 8 个其他会话的草稿及附件；关闭后未提交的草稿不会恢复。窄窗口按 `Tab` 切换任务队列与详情。

使用 `--theme auto`（默认）跟随终端配色，`--theme dark` 或 `--theme light` 指定配色，`--theme mono` 不使用颜色。任务状态和审批选项都有文字标签。

关闭工作台会结束它启动的本机服务；重开后可从数据库找回任务和待办。结果未知的外部操作不会自动重发。下载会校验 SHA-256，拒绝覆盖已有文件。文本预览上限 64 KiB，下载上限 32 MiB。

## 接入模型

| 变量 | 取值 |
| --- | --- |
| `FLIUNO_PROVIDER` | `openai`、`anthropic` 或 `gemini` |
| `FLIUNO_MODEL` | 账户有权使用的模型 ID |
| 对应密钥 | `OPENAI_API_KEY`、`ANTHROPIC_API_KEY` 或 `GEMINI_API_KEY` |

工作台从启动它的终端读取环境。API 密钥应由本机凭据管理方式注入，避免放到任务输入或工作区文件中。模型和接入端点的更多选项见[服务配置](setup.md)。

```sh
fliuno --workspace ./project
fliuno --workspace ./project -- --mcp-config ./mcp.json
```

`--db PATH` 选择 SQLite 文件。Windows 默认位于 `%LOCALAPPDATA%\fliuno\workbench.db`，Linux 默认位于 `$XDG_STATE_HOME/fliuno/workbench.db`（未设置时使用 `~/.local/state`）。`--demo` 默认使用同目录下独立的 `demo.db`。

Linux 启动包面向 x86-64、glibc 2.34 及以上。发布页的 `SHA256SUMS.txt` 可用 `sha256sum -c SHA256SUMS.txt` 校验已下载的文件；只下载其中一个包时可用 `sha256sum -c --ignore-missing SHA256SUMS.txt`。Windows 用 `Get-FileHash` 计算 `.zip` 的 SHA-256 并与清单对照。
