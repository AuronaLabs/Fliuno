# 隔离程序的伴随文件

[文档首页](README.md) · [沙箱](sandbox.md) · [宿主配置](host-configuration.md)

`fliuno-sandbox::LaunchSpec.assets` 由宿主明确列举脚本、模块、DLL 和数据文件，在进程启动前复制到私有目录。exec 的 program、MCP 的 stdio server 和 ACP 的 agent 配置均支持同一个 `assets` 数组。省略时为空；`trusted_host` 不接受清单。该接口不从模型、RPC、Skills 文本或远端工具定义接收路径授权。

```json
{
  "assets": [{
    "source": "C:/absolute/approved/server.js",
    "path": "server.js",
    "sha256": "<64 位小写十六进制 SHA-256>",
    "executable": false
  }]
}
```

`source` 是宿主绝对文件路径；`path` 是私有根内使用 `/` 的相对路径。摘要必填，没有自动信任新内容的选项。宿主应在审阅内容后固定摘要；演示中的即时生成只用于临时 fixture，不构成软件供应链验证。源目录不会被挂载或自动递归复制。

目标路径每组件最多 128 ASCII 字符、全路径最多 512 字节，只接受字母、数字、`.`、`_`、`-` 与目录分隔 `/`。拒绝绝对路径、空组件、`.`/`..`、反斜杠、ADS、尾点、Windows 保留设备名、大小写重复、不一致的目录大小写以及文件/父目录冲突。根 `program` 和 `program.exe` 为原生 executable 保留。源必须是普通文件；复制时再次核验打开的文件类型，并拒绝最终源文件的 symlink/reparse point。

清单最多 1024 个文件，每文件最多 128 MiB，伴随文件合计最多 512 MiB；原生 executable 另有 128 MiB 上限。先验证完整清单与源 metadata，再通过 `cap-std::Dir` 在私有根中逐文件 `create_new`。复制循环独立检查实际字节数并计算实际写入内容的 SHA-256，防止源文件增长或审批后的变化绕过预算/摘要。任一失败都不会启动 workload。失败期间创建的临时文件随私有目录所有者回收。

Windows 新文件继承 AppContainer 私有目录 ACL；不修改宿主源或祖先目录 ACL。Linux 初始文件权限为 0400，`executable:true` 改为 0500。工作负载拥有私有目录，因此这些模式不是启动后的不可修改保证；清单固定的是初始复制内容。启动后的输出、磁盘用量和共享系统资源仍遵循[平台沙箱](sandbox.md)的现有边界，清单大小上限不能代替磁盘配额。

exec 的配置身份包含全部清单字段，MCP 在清单非空时以 SHA-256 摘要生成带 128 位后缀的工具别名；修改源路径、目标、内容摘要、顺序或执行位会改变身份。MCP 空清单保留原展示别名，另外使用完整注册摘要绑定传输、原生程序内容、参数、实际环境/凭据与工具 Schema。SQLite 重启后，即使别名不变，旧审批也不能授权变更后的注册。摘要不构成远端服务器代码认证，见[注册身份](authority.md)。

ACP 每任务使用新的隔离进程，已启动运行的权限在重启后失效；清单在启动时重新校验，不新增后台自动续执行。排队 Agent 任务在创建时冻结注册摘要，ACP 的全部配置、实际环境和程序内容变化都会使旧任务拒绝启动。exec、隔离 MCP 与 ACP 还在暂存原生程序时核验实际复制字节的 SHA-256，避免注册后程序变化被执行。

## 真实解释器验收

Windows 本机运行 Node 24 与 CPython 3.12，多文件 MCP 服务经过真实 app-server、官方 rmcp 客户端、SQLite 和逐次审批。模型端是本机确定性 HTTP fixture，无外部模型或包安装：

```powershell
python examples/bundle_demo.py
python examples/bundle_demo.py --deny
python examples/bundle_demo.py --tamper
python examples/bundle_demo.py --runtime python
python examples/bundle_demo.py --runtime python --deny
python examples/bundle_demo.py --runtime python --tamper
```

可用 `--node` / `--python` 指定已有本机解释器。每次先在宿主进程中证明文件读写、TCP、环境和 scratch 写入成功，再要求隔离进程读取清单数据、写私有目录、只调用一次工具，并拒绝同样的宿主文件/TCP/测试 secret。拒绝分支没有工具完成事件；篡改仅修改临时源副本，要求服务在 MCP 初始化前失败。

Node 在 Windows 的默认 CommonJS realpath 会查询沙箱外的磁盘根，实测返回 EPERM。演示由宿主显式配置 `--preserve-symlinks --preserve-symlinks-main`，同时覆盖主模块和依赖解析；内核不自动添加参数或扩大 ACL。此示例无 npm/native addon，不代表任意 Node 应用都能运行。

Windows Python 演示使用已有 CPython 3.12，明确复制对应 DLL、两个扩展模块，以及为 fixture 构建的无压缩标准库 ZIP；`python312._pth` 只列出 ZIP 和私有根，不导入 site，不继承注册表或环境里的模块路径。依赖清单见 `examples/python_runtime.py`，未打包 pip、venv、第三方包或整套安装目录。其他版本与任意 Python 项目的依赖收集尚未实现。

2026 年 11 月 19 日，Debian x86_64 来宾使用 Node 24.4.1 与系统 CPython 3.13.5，分别通过 默认允许流程、`--deny` 和 `--tamper`。使用 `--app-server /path/to/fliuno-app-server` 可直接测试预编译的 app-server。两种运行时都从私有目录加载已固定的脚本、模块和数据。App-server 通过真实 stdio 搭配本机模型夹具处理审批。来宾无外网，探针先在无隔离对照中连接本机端口，再验证隔离进程被拒绝。

Linux 演示把 CPython 标准库和动态扩展放在来宾的系统只读运行库目录。位于其他位置的安装仍需显式提供依赖。Windows 演示对 CPython 3.12 使用上面列出的显式暂存清单。

官方参考：

- https://nodejs.org/api/cli.html#--preserve-symlinks-main
- https://nodejs.org/api/cli.html#--preserve-symlinks
- https://docs.python.org/3.12/using/windows.html#the-embeddable-package
- https://docs.python.org/3.12/using/windows.html#finding-modules
- https://learn.microsoft.com/en-us/windows/win32/dlls/dynamic-link-library-search-order
