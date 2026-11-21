# Skills 与按需资源

[文档中心](README.md) › [模型、工具与 Agent](README.md#模型工具与-agent) · [运行与配置](setup.md#文件与-skills) · [MCP 资源、提示与技能](mcp-content.md)

宿主用 `--skills-dir ABSOLUTE_PATH` 配置 Agent Skills 目录。发现只读取各子目录的 `SKILL.md` 元数据，模型随后按需请求指令和资源；目录内容不会自动进入上下文，也不会注册进程或获得工作区权限。

| 工具 | 参数 | 结果 |
| --- | --- | --- |
| `skills_list` | `{}` | 已发现技能的 frontmatter 元数据，不包含正文或资源内容 |
| `skills_read` | `{name}` | `{instructions,grantsPermissions:false}` |
| `skills_list_resources` | `{name,path?:""}` | 单层目录的 `{name,path,entries,grantsPermissions:false}` |
| `skills_read_resource` | `{name,path,encoding?:"utf8"}` | `{name,path,content,bytes,sha256,grantsPermissions:false}` |
| `skills_read_resource` | `{name,path,encoding:"base64"}` | `{name,path,encoding:"base64",data,bytes,sha256,grantsPermissions:false}` |

四个工具均使用内核普通逐次审批，`allowed-tools` frontmatter 作为数据保留，不修改策略。默认资源读取支持 UTF-8 文本，包括 `references/` 的说明、`assets/` 的文本模板及 `scripts/` 的源码。读取脚本不会执行它；真正执行必须走主机明确注册的命令、参数、环境、隔离与审批。需要二进制模板、图片或文档时，显式指定 `encoding:"base64"` 返回不透明数据，不尝试解码、解析 MIME、生成预览、执行或创建产物。资源列举不接受 encoding 字段。

## 目录能力与预算

发现时为每个技能打开独立目录句柄并保留，随后从该句柄逐级解析资源，不能从技能 A 借相对路径或目录链接跳到技能 B。每级目录使用 `open_dir_nofollow`，最终文件不跟随链接，并复核实际打开的是普通文件。Unix 文件打开还带 `O_NONBLOCK`，文件变成 FIFO 时也不会无限等待写端。`SKILL.md` 同样使用这些文件读取约束。

资源路径以 `/` 分隔，相对于单个技能根；最多 512 UTF-8 字节、16 个组件，每组件最多 128 字节。只接受 Unicode 字母/数字以及 `. _ -`，不接受空组件、`.`/`..`、绝对路径、反斜杠、ADS、尾点和 Windows 设备名，包括带扩展名或特殊数字的设备名。空路径仅用于列出技能根。含空格或其他标点的文件名不在当前子集中。

技能库根最多扫描 1024 项、发现最多 256 个技能，单份 `SKILL.md` 最多 128 KiB。每次资源列举最多扫描 256 项，不递归遍历；超额时返回错误而不伪装成完整列表。列举按名称排序，只返回支持路径子集内的普通文件和目录，不读取文件内容。单份资源最多 256 KiB，读取循环独立检查实际字节数，返回所选表示与同一批原始字节的 SHA-256。字节预算不等于整个进程内存或任意磁盘 IO 的 deadline。

`encoding:"base64"` 复用相同的无链接路径解析与普通文件读取，原始字节仍最多 256 KiB，编码后最多 349528 个字符。`bytes` 和 `sha256` 始终针对原始字节，不能对 base64 字符串重新计算来比较。空文件可返回空 data。默认 UTF-8 模式遇到非法 UTF-8 继续失败，不静默切换编码。base64 允许读取任意符合路径规则的技能资源，不仅限于名为 assets 的子目录；它不会修改 SKILL.md 的发现/指令规则。

## 注册绑定

四个 Skills 工具的注册摘要包含规范化的宿主目录路径、发现时的元数据和公开工具契约。相同工具名和技能名换到另一个目录后，旧待批调用不能授权新目录；摘要之外不把绝对来源路径加入 Task 或审批。连接仍通过 `tools/list` 发现实际注册的能力。

加入显式编码选项改变 `skills_read_resource` 的公开契约，因此升级前该工具的待批调用不能继续允许；新建任务重新授权。读取编码与路径都是持久审批的一部分，批准时不能换成另一种表示或另一个文件。

这不是技能发布者认证或内容 pin。目录句柄防止本次注册后的路径替换把读取重定向到另一个目录，资源内容仍在每次获批时读取，可能已被宿主编辑。相同路径与元数据下替换资源内容不要求摘要变化；返回的 `sha256` 用于描述本次观测，不授予执行或信任权限。修改注册或重启重新发现后，元数据变化会改变摘要。通用绑定与旧记录规则见[注册身份](authority.md)。

## 执行离线文本 Skill

宿主同时提供 `--skills-dir` 和 `--exec-config`，在后者的 `programs` 中为一个已发现的 Skill 登记解释器和脚本。以下 Linux 示例用原生 `/usr/bin/dash` 将纯文本的每一行变成 Markdown 列表项：

```text
/srv/fliuno/skills/text-list/SKILL.md
/srv/fliuno/skills/text-list/scripts/to-list.sh
```

`SKILL.md` 的 frontmatter 使用 `name: text-list` 和 `description: Convert text lines into a Markdown list.`。`to-list.sh` 的内容只用 shell 内建命令：

```sh
while IFS= read -r line || [ -n "$line" ]; do
    printf '%s\n' "- $line"
done
```

`--exec-config` 指向以下 JSON。将 `SCRIPT_SHA256` 换成 `sha256sum /srv/fliuno/skills/text-list/scripts/to-list.sh` 输出的 64 位小写十六进制摘要，并按部署填写绝对路径：

```json
{
  "programs": [{
    "id": "text-list",
    "command": "/usr/bin/dash",
    "isolation": "linux_landlock",
    "args": ["scripts/to-list.sh"],
    "envAllow": [],
    "timeoutMs": 5000,
    "outputBytes": 16384,
    "skill": {"script": "scripts/to-list.sh", "artifactTitle": "Markdown list"},
    "assets": [{
      "path": "scripts/to-list.sh",
      "source": "/srv/fliuno/skills/text-list/scripts/to-list.sh",
      "sha256": "SCRIPT_SHA256"
    }]
  }]
}
```

宿主用 `tools/list` 查看 `skill_text-list_…` 工具名，再用真实模型或本地模型夹具发起任务。解释器和参数由宿主固定；工具只接受 `stdin`，上限为 16 KiB UTF-8。Windows 使用 `windows_app_container` 和可以在 AppContainer 中运行的原生解释器 `.exe`；依赖文件需要显式登记为带摘要的暂存文件。两种模式都没有工作区访问或网络授权，`envAllow` 必须为空。注册时核对发现的脚本路径与摘要，启动时再核对实际暂存的脚本和解释器。

宿主通过 `approval/get` 的 `approval.executionPlan` 审阅解释器和脚本的摘要、固定参数、暂存文件、隔离与 IO 预算。完整计划最多 8 KiB；超限的注册在启动时拒绝。获批后，仅成功退出且未被截断的 UTF-8 stdout 会作为文本产物与工具结果一并提交。`artifacts/get` 读取正文，模型收到产物 ID。超时、取消和中断不会自动重试；有界的已捕获结果可在事件中检查，强制重启时结果可能未知。

## 离线验证

Windows AppContainer 和 Debian 6.12.107 来宾分别通过下列三组进程测试。原生夹具解释器执行固定文本转换，验证脚本已启动之后再取消或终止 app-server；重启后同一提交键不会再次执行。

```sh
cargo test -p fliuno-app-server --features test-fixtures --locked --test process skill_execution
```

旧的纯读取流程可用以下示例验证：

```powershell
python examples/skills_demo.py
python examples/skills_demo.py --deny
python examples/skills_demo.py --switch-source
```

演示使用本机确定性 HTTP 模型 fixture、真实 app-server、SQLite 和逐次审批，按元数据、指令、目录、参考资料、脚本文本、二进制资源的顺序读取。二进制分支实际还原原始字节并核验长度/摘要。拒绝分支不产生资源读取结果；切换目录分支拒绝旧审批且不增加模型请求，新任务可以正常完成。脚本包含可检测写入，但三种流程结束后该文件都不存在。

单元/集成测试另覆盖中文文件名、摘要、UTF-8 与大小预算、路径攻击、目录扫描预算、普通文件/目录类型、Windows 真实目录联接、SQLite 重启和取消。二进制测试包含全部 256 种字节、恰好 256 KiB/超出一字节、空文件、错误编码和逐次审批后的持久工具结果，不自动生成产物。Unix 文件链接及 FIFO 探针需在 Linux runner 实际运行；Windows 交叉检查不代替运行证据。

格式依据： https://agentskills.io/specification
