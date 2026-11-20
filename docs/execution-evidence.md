# 工程验收与性能记录

[文档中心](README.md) › [内核与开发](README.md#内核与开发) · [检查与演示](setup.md#检查与演示) · [开发路线图](roadmap.md)

按版本查看平台验收、进程测试和性能测量。运行完整检查的命令见 [检查与演示](setup.md#检查与演示)。

| 要查什么 | 记录 |
| --- | --- |
| **发布验收** | [0.0.2 Windows 与 Linux 双平台验收](#002-windows-与-linux-双平台验收) |
| **性能数据** | [上下文选取](#003-上下文选取按字节预算解码2026-11-20) · [历史分页](#003-历史分页按字节预算解码2026-11-20) · [恢复查询](#003-恢复查询的-sqlite-字段提取2026-11-20) |
| **恢复与重连** | [任务恢复判断](#003-任务恢复判断2026-11-20) · [AG-UI 审批重连](#003-ag-ui-审批重连2026-11-20) |
| **本机命令执行** | [预算和停止顺序](#预算和停止顺序) · [报告字段](#报告字段) · [工程验证](#工程验证2026-11-18) |

<details>
<summary>展开 0.0.3 验收索引</summary>

- **历史与用量**：[会话历史分页](#003-会话历史分页2026-11-19) · [历史搜索与直读](#003-历史搜索与直读2026-11-19) · [模型用量范围分页](#003-模型用量范围分页2026-11-20) · [用量汇总](#003-模型用量汇总2026-11-20) · [历史检索按需解码](#003-历史检索按需解码2026-11-20) · [可调扫描窗口](#003-可调历史扫描窗口2026-11-20)
- **历史续读**：[增量搜索](#003-增量历史搜索2026-11-20) · [会话历史订阅](#003-会话历史订阅2026-11-20) · [按字节预算解码](#003-历史分页按字节预算解码2026-11-20)
- **工作台与任务**：[待审批快照](#003-待审批快照2026-11-20) · [工作台快照](#003-工作台快照2026-11-20) · [工作台实时订阅](#003-工作台实时订阅2026-11-20) · [批量任务摘要](#003-批量任务摘要2026-11-20) · [水位通知合并](#003-水位通知合并2026-11-20) · [任务变动回放](#003-任务变动回放2026-11-20) · [任务变动摘要](#003-任务变动摘要2026-11-20) · [摘要修订回放](#003-任务摘要修订回放2026-11-20) · [批量取消任务](#003-批量取消任务2026-11-20)
- **交付物**：[持久产物摘要](#003-持久产物摘要2026-11-20) · [交付物下载续传](#003-交付物下载续传2026-11-20) · [合并摘要和首块](#003-交付物下载首块2026-11-20) · [交付物目录订阅](#003-交付物目录订阅2026-11-20) · [批量交付物摘要](#003-批量交付物摘要2026-11-20) · [交付物作为任务输入](#003-交付物作为任务输入2026-11-20)
- **任务提交**：[任务摘要重试回执](#003-任务摘要重试回执2026-11-20)
- **会话目录**：[倒序分页与续读](#003-会话目录倒序分页2026-11-20)
- **终端**：[Rust 终端续读](#003-rust-终端续读2026-11-20)
- **AG-UI 与审批**：[内嵌图片输入](#003-ag-ui-内嵌图片输入2026-11-20) · [内嵌 PDF](#003-ag-ui-内嵌-pdf2026-11-20) · [图片请求重试](#003-图片请求重试2026-11-20) · [AG-UI SSE 续传](#003-ag-ui-sse-续传2026-11-19) · [HTTP 宿主演示](#003-ag-ui-http-宿主演示2026-11-19) · [审批中断与续跑](#003-ag-ui-审批中断与续跑2026-11-19) · [批量审批回执](#003-批量审批回执2026-11-20)
- **执行控制**：[每任务模型尝试上限](#003-每任务模型尝试上限2026-11-20) · [AG-UI 宿主模型尝试上限](#003-ag-ui-宿主模型尝试上限2026-11-20) · [审批续跑边界预检](#003-审批续跑边界预检2026-11-20)

</details>

## 预算和停止顺序

`fliuno-exec` 管理一次获批前台命令的原生启动、stdin、stdout/stderr 和进程所有权。它与模型、协议传输、SQLite 状态机分开；任务层持久保存报告，不从日志文本猜测退出原因。跨轮次 PTY 使用独立服务，不复用这套短命令生命周期。

`timeoutMs` 从调用原生启动前开始计算，包含 AppContainer profile 准备、暂存和进程创建。隔离启动由阻塞 worker 执行，执行器等待它交回进程所有权；原生启动返回前不能由此 deadline 抢占。如果返回时已取消或预算已用完，执行器先释放从未轮询的输入 future，再请求终止，随后并发收集输出和根进程退出状态。

运行中的取消、超时或输出/退出观察错误也会请求停止。若输入仍待完成，停止时立即丢弃输入 future，避免继续排队写入。隔离路径与 Windows 受信任宿主路径均使用 reactor 管道，Unix 受信任宿主保留 Tokio 的原生进程管道。报告对所有已轮询后中止的输入保留保守的 `stdinAborted`，不把丢弃 future 等同于 OS 管道句柄已经关闭或全部 pending IO 已完成取消。

根进程自然退出也会请求回收其后代，并完成输入和双输出的收尾。清理阶段有独立的 2 秒预算；超时则报告 `termination=cleanup_failed`，保留最初停止原因。`elapsedMs` 包含启动和清理，不是仅衡量子进程 main 函数的耗时。

该 2 秒是异步排空预算，不能抢占同步的原生 terminate 或 Drop。Windows sandbox 的 kill 另有最多 5 秒根进程等待，底层 OS/文件清理也可能延迟；本接口不承诺 `timeoutMs+2000` 的绝对返回上限。

## 报告字段

以下 `evidence` 对象随正常 `tool_completed` 结果或 `tool_outcome_unknown.output` 一起持久化。旧事件不回填，新字段不改变既有任务或审批身份，也不需要数据库迁移。

| 字段 | 含义 |
| --- | --- |
| `launchMs` | 原生 spawn 返回时的累计毫秒数，是总预算的一部分。 |
| `stopCause` | 首次请求停止的原因：`root_exited`、`deadline`、`cancelled`、`io_error`；后续清理失败不会覆盖它。 |
| `stopRequestSucceeded` | 原生终止请求是否被接受；不是所有后代已经退出的单独证明。 |
| `rootReaped` | 实际观察到了根进程退出状态。 |
| `stdinClosed` | 本地输入 writer 已释放且没有已知未完成的后台写入；不能推断子进程已读取。 |
| `stdinSkipped` | 停止前未轮询过输入，输入 writer 已直接释放。 |
| `stdinAborted` | 已轮询过的输入 future 在停止时被丢弃；后台写入可能仍待完成，不据此声明 `stdinClosed`。 |
| `stdoutClosed` / `stderrClosed` | 相应读取观察到了 EOF 或 BrokenPipe；其他读取错误不会设为 true。 |
| `cleanupExpired` | 清理排空预算已耗尽。 |
| `faults` | 按 operation 去重、最多 6 项的 IO 诊断。 |

每项 fault 只保存 `operation`、Rust `ErrorKind` 的固定名称 `kind` 和可选数值 `osCode`。operation 为 `stdin_write`、`stdin_shutdown`、`stdout_read`、`stderr_read`、`poll_exit` 或 `terminate`。不保存 `io::Error` 的显示文本，避免系统或库错误将主机路径、参数、环境值带进任务记录。

`stdinComplete=true` 要求 `write_all` 和 `shutdown` 都成功，随后释放本地 writer。不能仅靠 `write_all` 成功或忽略 shutdown 错误。即使两者成功，也只证明数据已交付给管道；OS 缓冲可容纳尚未被程序读取的输入。

原隔离路径的 `tokio::fs::File` 会先把 write 接收到中间缓冲，shutdown 才等待 flush；现已改为 `fliuno-sandbox::spawn_async` 统一提供 reactor 管道。Windows 使用私有单实例 named pipe，只有父端开启 overlapped IO、子端保持同步；Linux 使用 Tokio 的非阻塞 `unix::pipe`。Windows Mio 仍可能在 WriteFile 提交时接受数据，而 Tokio named pipe 的默认 flush 不等待它完成。独立 writer 因此等待写就绪并提交零字节探测，取出上一笔写入的完成错误；shutdown 之后才关闭写端。每次提交的数据最多 64 KiB，不使用要求 reader 消费才能返回的 `FlushFileBuffers`。

MCP、ACP 与前台命令复用这套管道；MCP/ACP stderr 用异步任务持续丢弃，在生命周期结束时 abort，不再 join 阻塞 reader。同步 `spawn` 与持久 PTY bootstrap 保持原有接口，reactor 实现只在 `async-io` feature 中启用。Windows 受信任宿主命令和 MCP 通过 `spawn_host_async` 使用同一 reactor，保留显式 cwd/环境和完整 OS 用户权限；它的 Job 只负责生命周期，不附加隔离模式的内存/PID 默认限额。原生启动/终止仍是同步操作，因此这里不承诺所有宿主调用的严格墙钟上限。

stdin 写入或关闭失败并不自动等于根进程结果未知：若根进程和双输出均正常收尾，返回已知 `exited` 结果，但 `success=false`、`stdinComplete=false`，并记录失败阶段。输出读取、退出观察、终止或清理不完整则保留未知结果，不自动重跑。

根进程已回收且 Unix TrustedHost 的 `killpg` 返回 ESRCH 时，记录停止请求失败但允许已知收尾，因为目标进程组已经不存在。其他终止错误一律保留清理失败，不能只凭根进程退出便掩盖 Job/cgroup 回收失败。

`spawn_async` 错误带有独立的启动/worker/管道注册阶段。前台执行器将原生启动失败保留为已知失败；启动 worker 丢失或启动后的管道注册失败进入未知结果，不能把未拿到管道当作程序从未运行的证据。

## 工程验证（2026-11-18）

- 单元测试使用一个接受全部 write 但拒绝 shutdown 的 writer，检查失败阶段、本地释放和诊断不泄露私有错误文本。
- 生命周期驱动以受控时钟和独立进程观察接口验证：启动交回时已超时/取消绝不轮询 stdin；清理超时保留取消原因；自然退出后的挂起输入在清理结束时报告中止；根退出不掩盖其他 terminate 失败。这些测试验证调度和报告，不能代替原生隔离行为探针。
- 真实管道测试启动当前 Rust 测试程序并等待其退出，再用 file-backed async writer 写入已无 reader 的管道；观察 shutdown 阶段的 BrokenPipe，验证缓冲接受不等于交付成功。
- 原生异步管道探针完成约 416 KiB UTF-8 输入往返和 EOF；reader 已退出及 Windows 已提交写入后被强杀都必须返回 BrokenPipe。取消探针保持不读取 stdin 的隔离子进程存活，在读写均挂起后丢弃端点，要求宿主 runtime 在 3 秒内退出，再清理子进程，避免用对端退出掩盖阻塞 IO 泄漏。
- 集成测试覆盖 literal argv、UTF-8 stdin、独立 stdout/stderr、超量输出、二进制、非零退出、deadline、取消、后代回收与审批身份；Windows 真实运行 TrustedHost 和 AppContainer。
- 子孙回收另外运行一个 60 秒存活的真实 descendant，以 30 秒总预算确认 `descendant-ready`、其原生 PID 和 deadline 清理后的不运行状态，避免 200 ms 短超时在程序尚未创建子孙时误算回收成功。自然退出用正常启动预算；独立 200/500 ms deadline 探针保留。
- 并发测试由父线程准备后统一释放各 worker，在 Windows/Linux 上同时运行两个隔离模式各自的 echo/flood，共四个命令；检查输入身份、输出尾部、错误流、回收和诊断各自独立。worker 只等待父线程的有界消息，不使用可能因准备失败而死锁的多方 barrier。
- 真实 app-server 进程测试通过 stdio RPC 检查 deadline 证据，关闭后从 SQLite 重启，要求相同事件原样保留且拒绝恢复重放。

首次 A2A 全量验收中有两项原生命令测试偶发进入未知结果。本轮增强诊断后复现：AppContainer flood 的 `launchMs=8348`、echo 的 `launchMs=8945`，两者均为 `termination=timed_out`、`stopCause=deadline`，超过成功路径 fixture 原先的 5000 ms 总预算。这定位了观察到的失败机制；未查明造成 OS 启动延迟的具体环境原因。成功路径测试改用 30000 ms，独立 200/500 ms 的超时测试保留。

复现记录在 `target/verification/native-input-evidence.log`；后续聚焦与并发记录分别为 `native-evidence-focused.log` 和 `native-evidence-concurrent.log`。Linux 源码和同类探针保留在 CI；当前 Windows 主机的 Linux 交叉编译不能代替 Linux 运行验收。

共享 reactor 管道的原生探针与 MCP/ACP/exec 契约测试记录在 `target/verification/async-stdio-native.log` 和 `async-stdio-contracts.log`。

后续全量并发验收另暴露一个独立缺陷：AppContainer echo 的根退出码为 0、stdin 已完成、双流数据完整，回收也成功，但两条流在 2 秒清理预算内均无 EOF（`native-descendants-verified.log`）。它不是启动超时。确定性测试先创建隔离管道，再从同一宿主启动无关的长命 `std::Command`，释放原子端后稳定复现 EOF 不到，确认可继承子端被另一条启动链路带走。

Windows 管道现全部在宿主创建为不可继承；只有始终挂起的短命中转进程持有三个可继承副本。`PARENT_PROCESS` 选择该句柄表，`HANDLE_LIST` 限定三个子端。中转使用创建时原子分配的 kill-on-close Job，实际子进程在恢复前验证已离开中转 Job；普通 sandbox workload 的资源 Job 也通过 `JOB_LIST` 随创建分配。同步隔离、reactor 隔离和持久 service bootstrap 共用这一边界，不依赖其他库配合全局 spawn 锁。实现与平台限制见 [沙箱边界](sandbox.md)。

回归测试保持无关进程存活，只释放中转和原子端，要求双流在原有 2 秒预算内 EOF；另硬杀中转创建者，要求中转的原生观察句柄在 5 秒内报告退出。修复后的原生记录为 `inheritance-native.log` 和 `inheritance-exec.log`；全工作区记录为 `inheritance-workspace.log`。实际结果以测试退出状态为准。

本轮工作区全量实际通过 220 项（0 失败、0 忽略）；随后新增的外层 Job 保留探针也通过，相关 Windows 单元测试共 5 项通过（`inheritance-nested-jobs.log`）。该探针在独立 fixture 中先加入禁止 breakaway 的外层 Job，再启动真实 AppContainer，直接查询 workload 仍属于原外层 Job，避免把临时 Job 的脱离行为扩大为宿主约束的绕过。fmt、全目标/全 feature Clippy、协议 Schema 核对、12 个离线演示和 Linux 交叉 Clippy 均通过；最后两项日志分别为 `inheritance-demos.log`、`inheritance-linux-clippy.log`。Linux 行为仍未在本机运行。

随后完成 Windows TrustedHost reactor 接入和 native launcher / Job owner 模块化，普通 host 与 AppContainer 均运行相同四项异步 IO 探针。该版本工作区全量实际通过 **221 项，0 失败、0 忽略**（`host-reactor-workspace.log`）；真实进程测试包含 MCP host/隔离契约、exec 四路并发/后代回收，以及 app-server 审批、重启、PTY、ACP/A2A 闭环。全目标/全 feature Clippy 与独立 sandbox 无默认 feature Clippy 均通过，后者也纳入 Windows/Linux CI。Linux 交叉 Clippy 记录为 `host-reactor-linux-clippy.log`；fmt、Schema 和 12 项离线演示记录为 `host-reactor-demos.log`。这些证据不等同于 Linux 运行验收或真实付费模型互操作。

切换共享管道后的首轮全量测试在 MCP 隔离初始化触发原 fixture 的 5 秒 deadline（`async-stdio-workspace.log`）；聚焦测试中同例曾通过，未取得该次超时的独立启动阶段计时，因此不能断言其具体耗时全部来自原生启动。该例改用生产默认的 30 秒初始化/调用预算，延迟调用改为 60 秒，继续验证真实超时关闭和后续调用拒绝，而不移除超时验收。

实现依据包括锁定的 Tokio 源码 `tokio-1.54.1/src/fs/file.rs` 中 `AsyncWrite::poll_shutdown` 对 flush 的委托，以及 Microsoft 的管道读写语义：

- https://docs.rs/tokio/latest/tokio/fs/index.html
- https://learn.microsoft.com/windows/win32/ipc/anonymous-pipe-operations
- https://learn.microsoft.com/windows/win32/api/fileapi/nf-fileapi-writefile
- https://docs.rs/tokio/1.54.1/tokio/net/unix/pipe/index.html
- https://docs.rs/tokio/1.54.1/tokio/net/windows/named_pipe/struct.NamedPipeServer.html
- https://docs.rs/mio/1.2.5/src/mio/sys/windows/named_pipe.rs.html
- https://learn.microsoft.com/windows/win32/ipc/synchronous-and-overlapped-input-and-output
- https://learn.microsoft.com/windows/win32/api/processthreadsapi/nf-processthreadsapi-updateprocthreadattribute
- https://learn.microsoft.com/windows/win32/procthread/inheritance
- https://learn.microsoft.com/windows/win32/procthread/nested-jobs

## 伴随资源清单验收（2026-11-18）

该版本 Windows 全工作区实际通过 **231 项，0 失败、0 忽略**。`scripts/check.ps1` 完整退出成功，包含 fmt、全工作区/独立 sandbox Clippy、Schema 一致性和 **18 个离线演示**，记录为 `target/verification/bundle-full-native.log`。Linux 目标的全工作区、全部 target、全部 feature Clippy 也退出成功（`bundle-linux-clippy.log`）；它发现并修正了 Linux 有界复制新增 `Read::take` 的 import/unused-mut 问题，仍不代表 Linux 行为已运行。

新增探针验证清单路径/大小写/父子冲突、源与复制内容变化、单文件/合计/数量预算、exec 和 MCP 的 SQLite 待批恢复身份，以及官方 ACP SDK Agent 读取独立副本。真实 Node 24.22.1 / CPython 3.12.10 在 AppContainer 中加载多文件 MCP，允许/拒绝/篡改三分支均通过；数据、文件/TCP/secret 正反对照和工具调用次数由演示断言核验。早期 exec 测试误读 `stdout.text` 已改为协议实际字段 `stdout.data`；MCP 资源测试使用旧默认工具列表首项的问题也已修正为明确选择 MCP 工具。失败与修正不能用编译通过替代真实运行验收。

真实 Node 的默认入口 realpath 在容器内失败，通过显式 runtime 参数解决；Python 的相邻 DLL、标准库 ZIP 和 `._pth` 通过固定清单提供。具体依赖及摘要范围见[伴随文件边界](runtime-bundles.md)。该阶段尚缺完整注册绑定，后续实现见[注册身份](authority.md)；本节历史验收不包含后续代码、真实付费模型、外部商业 Agent 或 Linux 内核运行。

## 注册身份绑定验收（2026-11-18）

此阶段 Windows 全工作区实际通过 **247 项，0 失败、0 忽略，45 个测试目标**。`scripts/check.ps1` 完整退出成功，包含 fmt、全工作区/独立 sandbox Clippy、Schema 一致性与 **18 个离线演示**，记录为 `target/verification/authority-final-native.log`。该数字独立于上一阶段的 231 项基线。

新增真实 SQLite 重启测试验证同名工具的 Schema 变化、MCP HTTP 同别名的端点/Schema 变化、空清单 stdio 同别名的参数变化，旧审批均不执行，新审批只调用一次。注册测试验证实际环境/凭据冻结与轮换后的摘要变化；Windows 缺失 SystemRoot 时拒绝注册，避免启动阶段补入新的宿主值。原生 executable pin 探针验证复制字节与摘要不符时拒绝创建进程。

ACP/A2A 排队任务保存创建时的摘要，配置变化和旧 v7 无绑定记录不能启动。真实 app-server 调度测试还验证注册失配必须保存 `task_failed`，不只打印错误留下运行中任务；新配置的新任务仍可完成，失败快照重启后保持一致。初版缺失注册测试误走已有的恢复拒绝路径，已改为直接验证启动准备阶段，修正后的六项 core authority 测试和全量检查均通过。数据库 v8 迁移与自定义 trait 摘要边界见[注册身份](authority.md)。

Linux 全工作区、全部 target、全部 feature 的 cross-Clippy 已退出成功（`authority-linux-clippy.log`），修正了共享 program 模块重构后遗留的 Linux 导入。当前主机没有可运行的 WSL 发行版，未执行 Linux 内核行为探针；本阶段也未使用真实付费模型或商业 Agent。

## 终端批量归档验收（2026-11-18）

加入批量归档后，Windows `scripts/check.ps1` 完整退出成功：**252 项测试，0 失败、0 忽略，45 个测试目标**，以及 fmt、两种 Clippy、Schema 一致性和 **18 个离线演示**。全量日志为 `target/verification/archive-batch-full-native.log`，新增四项 SQLite/文件系统契约及一项真实 PTY/RPC 契约也有独立的 `archive-batch-contracts.log` / `archive-batch-process.log`。

测试验证全列表非法/重复/容量预检不删除第一项输出；混合结果保持请求顺序；其他会话、仍持锁的监督进程和不存在的 ID 各自拒绝；未知文件只阻止清理，不撤销已提交身份。真实 SQLite reader 阻塞 COMMIT 时返回 `outcome_unknown` 并保留 spool/endpoint；单项插入失败不会撤销先前项或跳过后续项。真实原生 PTY 对照证明正在运行的进程未被隐式杀死，显式 release 后可重试归档，重启仍保留原归档快照和创建身份。

## 宿主配置读取验收（2026-11-18）

五类 JSON 配置统一有界读取后，Windows `scripts/check.ps1` 完整退出成功：**257 项测试，0 失败、0 忽略，46 个测试目标**，以及 fmt、两种 Clippy、Schema 一致性和 **18 个离线演示**。全量记录为 `target/verification/startup-full-native.log`；Linux 全工作区、全部 target、全部 feature cross-Clippy 退出成功，记录为 `startup-linux-clippy.log`，不代表 Linux 行为已运行。

新增测试验证增长中的输入流仍最多读取 1 MiB + 1 字节、恰好上限的 JSON 可读取，以及 IO/解析诊断不回显字段、值或配置路径。真实 CLI 分别拒绝 MCP/ACP/A2A/exec/terminal 的超额与损坏配置，且不创建数据库；后续 ACP JSON 无效时，前面的 loopback MCP fixture 没有收到连接请求。所有 JSON 反序列化先于外部初始化，跨 adapter 的语义验证仍不是一个事务。配置边界见[宿主配置](host-configuration.md)。

## Skills 资源与来源绑定验收（2026-11-18）

此阶段 Windows `scripts/check.ps1` 完整退出成功：**263 项测试，0 失败、0 忽略，46 个测试目标**，以及 fmt、全工作区/独立 sandbox Clippy、Schema 一致性和 **21 个离线演示**。全量记录为 `target/verification/skills-resources-full-native.log`。Linux 全工作区、全部 target、全部 feature cross-Clippy 同样退出成功，记录为 `skills-resources-linux-clippy.log`；Unix 链接/FIFO 行为仍待实际 runner。

Skills 的八项原生测试包含真实目录联接不能跳到同库其他技能、中文文本/摘要、UTF-8/容量/目录扫描边界、取消、参数覆盖拒绝，以及 SQLite 重启切换来源使旧资源审批失败。工具名冲突测试检查整个注册表前后相同，避免遗漏内核默认工具。Unix 专用测试还覆盖最终文件链接、FIFO 和替换后的 SKILL.md，但不计入 Windows 的通过数量。

三条新增演示均启动真实 app-server 和本机确定性 HTTP 模型：正常逐步披露、拒绝资源读取、待批时重启切换 Skills 目录。切换来源拒绝旧调用且模型请求数不增长，新任务可完成；全部分支的脚本都只作为文本返回，未执行。脚本和证据边界见[Skills 文档](skills.md)，独立日志为 `skills-resources-demo.log` / `skills-resources-deny.log` / `skills-resources-switch.log`。

## MCP 资源、提示与完成状态验收（2026-11-18）

最终 Windows `scripts/check.ps1` 完整退出成功：**272 项测试，0 失败、0 忽略，48 个测试目标**，以及 fmt、全工作区/独立 sandbox Clippy、Schema 一致性和 **24 个离线演示**。最终日志为 `target/verification/mcp-content-final-native.log`。Linux 全工作区、全部 target、全部 feature cross-Clippy 退出成功，记录为 `mcp-content-linux-clippy.log`；当前主机没有实际运行 Linux 隔离探针。

新增八项 HTTP/SQLite 契约验证资源服务不接收 tools/list、单页审批、opaque URI 不触发宿主文件或独立 HTTP 读取、二进制 blob 保留、覆盖参数/预算拒绝、服务 cache hints 不隐藏第二次请求或失败、集合/游标/wire 大小限制、未知结果不续交互、取消/超时和跨方法排队取消。真实 SQLite 重启同时覆盖资源和提示：拒绝不发请求、换端点旧审批失效、新授权完成、提示角色只在 ToolResult 内，以及事件和最终快照原样保留。

官方 Rust SDK HTTP 服务在 discover/initialize 与 JSON/SSE 四种组合内各执行五类内容方法。另一项独立 SDK stdio 进程测试分别运行 Windows TrustedHost/AppContainer，核对真实进程身份、容器状态、私有 cwd 与关闭后的目录回收。三条新增 app-server 演示依次审批分页、模板、读取、提示列表与获取，分别验收允许、拒绝和待批时重启换端点；最终重启查询不重放。聚焦记录为 `mcp-content-contracts.log`、`mcp-content-stdio.log` 和 `mcp-content-demo.log` / `mcp-content-deny.log` / `mcp-content-switch.log`。

首轮全量虽已通过相同的 272 项与 24 个演示（`mcp-content-full-native.log`），随后复核 SDK untagged 结果又发现部分内容与非完成 resultType 组合可能被当作完成，实际反例记录在 `mcp-result-type-reproduction.log`。修正后显式核验工具发现、工具调用及五类内容结果，拒绝 input_required/task/未知扩展，即使附带最终形状；MCP 工具摘要域更新为 v2。增强测试包含这些组合和原旧版缺省字段兼容，聚焦记录为 `mcp-result-type-contracts.log`，最终全量日志对应修正后的代码。最初一个断言过度要求固定错误类别，已改为检查结果未知、不泄露状态、不重放和共享连接关闭等实际契约。

边界和预算见[MCP 内容通道](mcp-content.md)。本阶段没有使用真实付费模型、外部商业 MCP 服务或远程 CI 运行结果；本机 fixture、官方 SDK 双端互操作与平台原生测试分别记录，不能相互替代。

## Skills 二进制资源验收（2026-11-18）

Windows `scripts/check.ps1` 完整退出成功：**274 项测试，0 失败、0 忽略，48 个测试目标**，包含 fmt、两种 Clippy、Schema 一致性和 **24 个离线演示**，记录为 `target/verification/skills-binary-full-native.log`。Linux 全工作区/全部 target/全部 feature cross-Clippy 同样退出成功（`skills-binary-linux-clippy.log`），不代表 Linux 行为验收。

Skills 的十项原生测试新增显式 base64 读取：覆盖全部 256 种字节、恰好 256 KiB 和超额一字节、空文件、编码错误、UTF-8 模式不静默降级、目录链接拒绝、普通审批前无读取结果、拒绝不返回数据、重启继续同一待批调用，以及最终持久快照与不自动创建产物。摘要和长度针对原始字节，编码后的数据实际还原比对。编译依赖只新增已锁定的 base64 0.22.1 引用。

三条既有 Skills app-server 演示已扩展到二进制资源，并分别实际运行正常/拒绝/换来源分支（`skills-binary-demo.log`、`skills-binary-deny.log`、`skills-binary-switch.log`）。正常与新来源任务通过逐次审批读取原始字节，模型 fixture 实际核验 base64 还原、长度和 SHA-256；脚本 marker 在所有分支均不存在。工具输入契约改变后旧待批读取需重新授权，文档见[Skills](skills.md)。

## 显式历史任务选择验收（2026-11-18）

Windows `scripts/check.ps1` 完整退出成功：**278 项测试，0 失败、0 忽略，48 个测试目标**，包含 fmt、两种 Clippy、Schema 一致性和 **24 个离线演示**，记录为 `target/verification/context-selection-full-native.log`。Linux 全工作区、全部 target、全部 feature cross-Clippy 同样退出成功（`context-selection-linux-clippy.log`），不代表 Linux 行为已运行。

新增四项 core 契约验证显式选择跳过超大最新任务、完成事件顺序、ID 集合换序后的幂等、快照冻结、非法/跨会话/未完成 ID 拒绝，以及完整序列化快照和原始投影的容量限制。选择失败不保存任务/事件、不占用幂等键；指定记录不会为了预算被静默删除。真实磁盘 v8 到 v9 升级保留旧快照形状，重启后返回原选择。聚焦记录为 `context-selection-contracts.log`；首次测试编译曾使用不受 rusqlite 支持的 u64 COUNT 目标，已改为 SQLite 对应的 i64 后运行通过。

增强后的既有上下文演示启动真实 app-server 和确定性 loopback 模型，检查实际 HTTP 请求精确包含指定任务及产物身份，排除最新超大记录；RPC 对跨会话/等待审批/重复/空/超额选择的拒绝不增长任务、事件或模型请求。服务重启后交换 ID 输入顺序仍返回相同任务和冻结快照，不重放模型请求。独立记录为 `context-selection-demo.log`。数据库 v9 只为新冻结输入提升版本号；未使用真实付费模型或远程 CI 结果。

## 工作区文件读取加固验收（2026-11-18）

Windows `scripts/check.ps1` 完整退出成功：**282 项测试，0 失败、0 忽略，48 个测试目标**，包含 fmt、两种 Clippy、Schema 一致性和 **24 个离线演示**，记录为 `target/verification/workspace-file-io-full-native.log`。Linux 全工作区/全部 target/全部 feature cross-Clippy 退出成功（`workspace-file-io-linux-clippy.log`）；单独的 tools Linux 契约编译记录为 `workspace-file-io-linux-contracts.log`。

新增的独立 `tools/file_io` 统一文本读取、补丁基线和全文写入预览的实际字节预算与取消检查。Windows 新增四项测试：恰好 256 KiB/空输入、无限增长 reader 最多消费一额外字节、开始前/返回数据后/EOF 时取消、中断重试与部分读取后的 IO 错误，以及真实文件的容量/按行/摘要/预览边界。tools 原生聚焦共 12 项通过（`workspace-file-io-contracts.log`），最后一个错误探针随后增强为先返回部分数据再失败，最终全量包含增强版本。

Unix 新增独立测试子进程，实际创建没有写端的 FIFO，核验只读/读写打开、工具读取、补丁、全文替换和写入预览均拒绝；父进程在 5 秒后会杀死阻塞探针，防止回归挂住 runner。该测试已交叉编译，但当前 Windows 主机没有运行它，不能计入原生通过数量。实现核对了锁定的 `cap-fs-ext 4.0.3` / `cap-primitives 4.0.3` 非阻塞选项，以及直接读取的 Linux man-pages `open(2)` 说明（HTTP 200 与摘录记录为 `workspace-file-io-research.json`）。O_NONBLOCK 对普通文件不构成硬 deadline；路径、写入基线与结果未知规则见[架构](architecture.md)和[协议](protocol.md)。

## 工具组注册与工作区二进制读取验收（2026-11-18）

Windows `scripts/check.ps1` 完整退出成功：**288 项测试，0 失败、0 忽略，48 条 Cargo 测试结果汇总**，通过 fmt、全 workspace Clippy、独立 sandbox Clippy、Schema 一致性和 **24 个离线演示**。最终记录为 `target/verification/workspace-binary-full-native.log`。Linux 全 workspace、all-targets、all-features cross-Clippy 同样通过，记录为 `target/verification/workspace-binary-linux-clippy.log`；它只证明 Linux 目标编译和静态检查通过，不代表 Linux 原生行为已运行。

core 的 `register_tools` 先暂存并校验整组工具的名称、摘要和与已有注册的冲突，再一次发布；若校验失败，已存在的注册保持不变。本轮接入工作区、Skills、MCP、ACP/Agent、exec 与 terminal 适配器。注册契约记录为 `target/verification/registry-contracts.log`，整组回归包含在最终全量日志中。

工作区新增 `workspace_read_bytes`，每次读取都需要审批，最多返回 256 KiB 原始字节及 base64、原始长度和 SHA-256；不会解码、执行或创建产物。15 项聚焦测试覆盖空文件、全部字节值、精确上限与超限、拒绝不返回数据，以及 SQLite 重启后继续原待批调用，记录为 `target/verification/workspace-binary-contracts.log`。Unix FIFO 探针已交叉编译，但没有在 Linux runner 上原生执行。

## 模型请求状态与大小边界验收（2026-11-18）

模型 provider 错误明确区分本地请求构造失败、发送前取消和可能已经发出的请求。OpenAI/Anthropic 的续接校验失败和超限请求体落为已知 `failed`；发送开始后发生取消、传输、状态或响应解析错误仍落为 `interrupted` / outcome unknown，并保留显式重试同意。旧版自定义 provider 的字符串错误继续按未知结果处理，避免低报风险。

两个 HTTP 适配器在网络 dispatch 前以有界 JSON 序列化器限制请求体为 8 MiB，精确上限接受、超出一字节已知拒绝；此限制只界定序列化的传输体大小，不估算 token 或模型窗口。core 测试覆盖真实 `start_task` 的已知失败/取消状态；provider 测试覆盖精确边界、预发送错误与取消不建连接，以及不完整 SSE 保留未知结果。

变更后的 Windows `scripts/check.ps1` 完整退出成功：**292 项测试，0 失败、0 忽略，48 条 Cargo 测试结果汇总**，通过 fmt、全 workspace Clippy、独立 sandbox Clippy、Schema 一致性及 **24 个离线演示**。记录为 `target/verification/model-request-budget-full-native.log`。Linux 全 workspace、all-targets、all-features cross-Clippy 退出成功，记录为 `target/verification/model-request-budget-linux-clippy.log`；它不代表 Linux 原生行为已运行。

## 0.0.2 Windows 与 Linux 双平台验收

2026 年 11 月 19 日，在 Windows 主机上用便携 QEMU 启动 Debian x86_64 来宾（Linux 6.12.107），运行 Zig 交叉链接的原生测试进程。来宾没有外部网络，启用回环和 cgroup v2 `cpu`、`memory`、`pids` 控制器。测试使用实际 SQLite、子进程和 PTY。

- Rust 内核 72 项、工作区工具 20 项通过，覆盖 FIFO 非阻塞拒绝、符号链接路径、文件元数据和持久审批。
- 工作区事件回放和产物索引各有一个真实 app-server 进程用例通过，覆盖数据库迁移、分页和重启。
- 三项异步管道、五项隔离及对应两项委派 cgroup 探针、一个 sandbox guardian 硬杀探针通过。终端两个专门启用的 cgroup 用例也通过，覆盖 app-server 被强制结束后的重连，以及 supervisor 结束后回收已脱离会话的后代。
- Linux MCP 的四项伴随文件契约和两项隔离进程契约通过。首次真实进程测试发现 stdio fixture 使用了不需要的 Tokio 信号驱动，启动时尝试创建 socket pair。改用只启用定时器的 runtime 后，通过了 SQLite 审批绑定、固定伴随文件读取、超时回收和宿主文件及网络拒绝的验证。Windows 同时复跑了九项 MCP 集成测试。

Windows 工作区在加入 Job 预算后通过 381 项测试、rustfmt 和两种 Clippy。此前的 380 项版本还执行了完整 `scripts/check.ps1`，涵盖协议 Schema 和离线演示。Linux 已完成全工作区测试二进制的交叉编译与链接，上述条目已在来宾运行。外部付费模型和商业 Agent 的验收仍待做。

Windows 的完整检查脚本随后通过 382 项测试、两种 Clippy、Schema 一致性和离线演示。最新的 Debian app-server 并发回归通过 58 项测试，0 失败，2 项 cgroup 专项默认忽略且已在上面的专门运行中通过。首次 `initialize` 测试等待上限从 5 秒增至 15 秒，为来宾并发启动及 SQLite 打开和迁移留出时间。其余请求仍使用 5 秒。调整后 Windows app-server 进程测试也通过 60 项。

同一来宾随后通过工作区工具 21 项、会话 4 项、事件流 5 项，覆盖 `workspace_sha256` 审批、超限、FIFO 和相对路径边界，以及项目目录被移除后重启仍可由 `workspace/get` 取回记录。日志为 `D:/Fliuno-Native-Test/guest-workspace.log`。

Node 24.4.1 和 Debian 系统 CPython 3.13.5 的真实 MCP 演示各通过允许、拒绝和篡改三个分支。CPython 探针现在从创建 socket 开始捕获拒绝。Linux 隔离进程能读取固定的私有伴随文件和写入私有目录，探针验证了文件读写、环境变量及回环连接的隔离。以前的未优化 Debug 构建运行 Node 演示时超过 45 秒测试上限。仅为 SHA-256 依赖启用编译优化后，同一来宾两次允许分支均约 21 秒，这是启动到演示结束的耗时。最终日志为 `D:/Fliuno-Native-Test/guest-runtime-final.log`。

任务级订阅用例在 Windows 和 Debian 来宾都通过。该用例在一批请求中提交同会话的 72 个任务，再取消被订阅的任务，验证同会话其他事件不会占用订阅队列，回放能按切换水位补齐。Debian 的 6 项事件流测试全通过，日志为 `D:/Fliuno-Native-Test/guest-task-events-final.log`。

Windows 完整 `scripts/check.ps1` 通过 385 项测试、格式检查、两种 Clippy、Schema 一致性和 26 个离线演示。日志为 `target/0.0.2-task-event-scope-windows.log`。随后对批量事件夹具的调整重跑了 6 项 Windows 事件流测试。运行时演示排除 Python 自动生成的 `__pycache__` 后，使用已构建 app-server 复跑了 Windows 上的 6 个 Node 和 CPython 分支，暂存清单回到预期的 3 项和 11 项。

## Linux 可信 ACP 续接（2026-11-19）

Debian 来宾的 13 项 ACP app-server 契约通过。另以委派 cgroup v2、256 MiB 内存及 16 个进程或线程名额复跑两项续接测试。一次明确拒绝会释放续接名额，提交结果未知的上一轮在重启后不会重发。原生运行另通过 3 项异步管道探针及 5 项隔离探针。部分隔离探针按委派可用性选择分支。记录为 `D:/Fliuno-Native-Test/guest-acp-trusted.log` 和 `guest-acp-native.log`。

Windows 全量 `scripts/check.ps1` 以退出码 0 完成，包括 385 项测试和 26 个离线演示。Linux 工作区交叉 Clippy 通过。记录分别为 `target/0.0.2-acp-trusted-linux-windows.log` 和 `D:/Fliuno-Native-Test/guest-acp-trusted-linux-clippy.log`。

## 会话历史检索和 MCP 目录监听（2026-11-19）

Windows 的 `scripts/check.ps1` 退出码为 0，包含 388 项测试和 26 个离线演示。日志为 `target/0.0.2-search-mcp-catalog-windows.log`。Debian 复验通过一项真实 app-server 历史检索契约和三项 MCP 官方 SDK HTTP SSE 契约。日志为 `D:/Fliuno-Native-Test/guest-search-catalog.log`。

检索性能试验使用相同 SQLite 数据库和 Windows 调试二进制，在 128 条模拟历史中查找最早一条。两份二进制交替运行，每组 55 次。仅反序列化必要字段的试验版本对纯 ASCII 数据有改善，混合文本的结果随轮次波动且没有稳定收益。此轮保留原有查询实现。对照记录为 `target/0.0.2-history-search-lighter-bench.log`。

## 可信 MCP 与前台命令的原生预算（2026-11-19）

可信 MCP 在 Windows 的两项新增进程测试中验证了宿主目录和显式 Job 进程上限。Debian 通过三项可信 MCP 测试及一项 stdio 内容测试。委派用例确认 MCP 子进程加入 cgroup，读取 `pids.max=16` 和 `memory.max=268435456`，关闭后目录被回收。配置错误的委派目录在进程启动前被拒绝。Windows 完整脚本通过 390 项测试及 26 个离线演示。Linux 全工作区交叉 Clippy 通过。日志为 `target/0.0.2-trusted-mcp-windows.log`、`D:/Fliuno-Native-Test/guest-mcp-trusted.log` 和 `guest-mcp-trusted-clippy.log`。

Debian 另通过前台命令的 12 项执行测试，覆盖管道、超时、取消、进程回收、隔离文件和审批。`pids.max=1` 的探针在受限用例里观察到内核拒绝 fork，未配置限制时额外子进程正常退出。日志为 `D:/Fliuno-Native-Test/guest-exec-final.log`。

## 产物格式筛选（2026-11-19）

Windows 和 Debian 各通过两项产物进程契约。测试覆盖旧版正文迁移、清单回填、跨作用域隔离、精确格式匹配，以及重启后从原分页位置继续。Debian 日志为 `D:/Fliuno-Native-Test/guest-artifact-types.log`。

SQLite 3.49.1 内存数据库写入 10 万条模拟产物。两种查询都返回同一工作区的 51 条稀疏结果。复合索引可以同时定位工作区、类型和游标。61 轮交替采样中，首屏的中位数为 0.36 ms，使用仅工作区索引时为 7.59 ms。翻页查询分别为 0.32 ms 和 7.82 ms。日志为 `target/0.0.2-artifact-media-index-bench.log`。

Windows 随后以 `target/0.0.2-native-exec-artifact-types-windows.log` 完成全量复验，包括 391 项测试和 26 个离线演示。此前一轮的测试已通过，但在重叠的构建中 Cargo 无法替换被占用的 Windows 可执行文件，演示因此中断。串行执行后通过。

## A2A 短时状态观察（2026-11-19）

Windows 全量检查通过 396 项测试和 26 个离线演示，记录在 `target/0.0.2-a2a-watch-windows.log`。Windows 和 Debian 各通过 20 项 app-server A2A 进程测试，验证订阅形成独立的持久核对事件、原运行和产物保持原状、重启可回放核对结果。Debian 的 `D:/Fliuno-Native-Test/guest-a2a-watch.log` 同轮有两个普通 JSON-RPC 用例失败，其中一个报传输错误。普通 fixture 请求时限从 300 ms 调到 3 秒，专门的超时用例仍使用 300 ms。最终 Windows 和 Debian 各通过 16 项 JSON-RPC 用例，Debian 复跑记录在 `D:/Fliuno-Native-Test/guest-a2a-watch-confirm.log`。

## 历史角色筛选和任务快照字节分页（2026-11-19）

Windows 和 Debian 各通过 4 项上下文进程测试和 2 项任务清单进程测试。检索用例验证按角色查找、排除另一角色和重启后的快照游标。任务清单用例覆盖 JSON 转义及 UTF-8 字节数、恰好达到目标、单项超额、数量上限和摘要游标。Debian 记录为 `D:/Fliuno-Native-Test/guest-context-task-pages.log`。

Windows 调试构建用同一批 32 个模拟任务快照交替请求 21 次。默认首屏返回 7,894,655 字节，客户端收到并解析响应的中位时间为 668.08 ms。指定 256 KiB 后首屏返回 1 个任务，共 246,706 字节，中位时间为 46.89 ms。该结果只比较首屏，读取全部任务仍需继续翻页。记录为 `target/0.0.2-task-pages-benchmark.log`。

Windows 的全量 `scripts/check.ps1` 通过 397 项测试和 26 个离线演示，包含上面两项功能，记录为 `target/0.0.2-context-task-pages-windows.log`。另用官方 `a2a-sdk==1.3.1` 验证活动补答任务上的 `SubscribeToTask`，记录为 `target/0.0.2-a2a-watch-sdk.log`。

## SQLite 读取语句复用（2026-11-19）

工作区、会话和任务的存在性检查、事件水位和会话读取使用 SQLite 连接已有的语句缓存。会话 JSON 从查询行借用，省去解析前的一次字符串复制。Windows 内存 SQLite 微基准将会话存在性检查、水位查询和会话内容读取作为一组，交替执行 11 组，每组 2 万轮。中位耗时从 30.23 µs 降至 10.94 µs，覆盖 SQLite 调用和行访问，不包括 JSON 解析或 RPC。基准记录为 `target/0.0.2-read-statement-bench.log`。`fliuno-core` 的 Clippy 和全部 73 项测试通过。

## Agent 注册能力查询（2026-11-19）

Windows 和 Debian 各通过 4 项实际进程用例。覆盖轮询 A2A、流式 A2A、隔离 ACP 和支持跨重启续接的可信 ACP。A2A 测试验证查询没有向对端发送请求，未知 Agent 返回 `-32004`。Debian 记录为 `D:/Fliuno-Native-Test/guest-agent-capabilities.log`。

## 远端核对调度（2026-11-19）

Windows app-server Clippy、15 项库测试和 65 项进程测试通过。Debian 通过 5 项调度库测试和 20 项 A2A 进程测试。并发用例让远端核对停在屏障处，同一任务再次核对被拒绝，普通模型任务同时到达待审批状态。记录在 `target/0.0.2-remote-queue-windows.log` 和 `D:/Fliuno-Native-Test/guest-remote-queue.log`。

## MCP 多资源短时监听（2026-11-19）

Windows 和 Debian 各通过 5 项官方 SDK 订阅测试及 1 项 HTTP 生命周期测试。新增用例验证一次订阅报告第二条 URI 的更新，部分确认返回 `declined`，非法、重复与超额 URI 在发送前被拒绝，两个有效请求结束后服务端都释放流。Debian 记录在 `D:/Fliuno-Native-Test/guest-mcp-watch-many.log`。

首次全量复验发现 stdio fixture 仍预期旧的工具数量。更新为 8 后，同一真实 SDK 进程同时验证单 URI 和多 URI 监听，在 Windows 的可信主机与 AppContainer、Debian 的可信主机与 Landlock 均通过。

## 远端核对证据索引（2026-11-19）

Windows 和 Debian 通过真实 app-server 进程用例，按 ID 读取事件，与常规回放逐字段一致。跨任务查找返回未找到。回退到 v28 结构后，在未配置远端 Agent 的进程中升级并取回原事件。内核故障测试验证索引拒绝重复 ID 时不提交额外事件或发送通知。Debian 记录在 `D:/Fliuno-Native-Test/guest-remote-lookup.log`。

## 产物正文摘要（2026-11-19）

Windows 和 Debian 各通过两项产物进程测试。已知摘要用例包含 108,000 个字节，覆盖多字节 UTF-8、NUL、换行和跨读取缓冲区边界。v20 正文迁移前后摘要一致。旧版空正文返回标准空 SHA-256 值，跨任务读取返回未找到。Debian 记录在 `D:/Fliuno-Native-Test/guest-artifact-digest.log`。

Windows 全量 scripts/check.ps1 退出码为 0，包含 400 项测试、Schema 校验和 26 个离线演示。日志为 target/0.0.2-artifact-digest-windows.log。Windows 的大小边界探针确认 8 MiB 正文可读取摘要，超出 1 字节返回预算错误。

## 字节目标分页扩展（2026-11-19）

任务摘要清单支持紧凑 JSON 字节目标，覆盖会话、工作区和状态筛选，并沿用 task/list 的创建游标。待审批收件箱也支持同一预算范围，可在 workspace/session 范围间沿用各自游标。两项真实 app-server 测试覆盖精确字节边界、状态筛选、重启续读和非法预算拒绝。

Windows 全量 scripts/check.ps1 通过 402 项测试、Schema 一致性和 26 个离线演示。摘要与审批分页进程用例各自通过，核心库 74 项测试通过，Clippy 和 rustfmt 检查通过。完整日志为 target/0.0.2-byte-paging-windows.log。

Debian Linux 6.12.107 来宾通过 3 项 task_status 与 2 项 approval app-server 进程测试。覆盖两种列表的字节边界、原始游标、workspace/session 过滤和 app-server 重启续读。构建与运行日志为 D:/Fliuno-Native-Test/guest-byte-pages-build.log 和 D:/Fliuno-Native-Test/guest-byte-pages.log。

## 会话批量归档（2026-11-19）

新增最多 32 项的原子批量归档/恢复。真实 app-server 进程测试验证请求顺序、重复请求、空/重复/缺失/超额 ID 的拒绝、SQLite 写入故障回滚和重启后的持久状态。Windows 全量 scripts/check.ps1 通过 403 项测试、Schema 一致性和 26 个离线演示；Clippy 与 rustfmt 检查通过。日志为 target/0.0.2-session-archive-batch-windows.log。

Debian Linux 6.12.107 来宾通过 5 项 sessions app-server 真实进程测试，包含同一事务触发器故障回滚探针。日志为 D:/Fliuno-Native-Test/guest-session-batch-build.log 和 D:/Fliuno-Native-Test/guest-session-batch.log。

## MCP Tasks 客户端适配（2026-11-19）

MCP 客户端声明 `io.modelcontextprotocol/tasks`。现代对端声明该扩展后，`tools/call` 创建的远端任务句柄作为本地工具结果持久化；新增受主机注册摘要约束的 `tasks/get`、`tasks/update` 和 `tasks/cancel` 工具。输入响应限制为 16 项和 256 KiB，任务快照限制为 1 MiB。取消回执按协作请求处理，不代表远端任务已结束。

官方 Rust SDK 双端测试通过任务创建、包含待补请求的状态查询、补答和协作取消。Streamable HTTP SDK 矩阵覆盖 2026 现代协议启用 Tasks 工具及 2025 旧协议不启用，并验证 JSON/SSE 两种响应。`scripts/check.ps1` 退出码为 0，涵盖 workspace 测试、Clippy、rustfmt、协议 Schema 与 Windows 离线演示；另有 workspace 全量测试及两组 Clippy 独立通过。本轮未在 Debian Linux 来宾复验。

随后增加独立审批的任务短轮询。Windows `cargo test -p fliuno-mcp --all-features --locked` 通过 60 项测试，同包严格 Clippy 通过。官方 Rust SDK 双端用例验证工作中任务进入待补答或终态、轮询间隔与次数边界，以及错误任务 ID、响应类型或创建句柄导致连接关闭。所有轮询用例核对远端未收到补答或任务取消请求。

## ACP 会话目录 RPC（2026-11-19）

3 项真实 app-server 和 ACP SDK stdio 进程测试通过。覆盖显式注册、拒绝调用方指定目录、跨进程分页、游标原样转交、远端错误净化与 256 KiB 页预算。阻塞对端期间本地查询可继续处理，队列填满后返回冲突，关闭 app-server 会打断读取并丢弃排队工作。重启后旧目录请求 ID 失效，代理保存的会话仍能重新发现。

Windows `scripts/check.ps1` 退出码为 0：421 项 workspace 测试、26 个离线演示、rustfmt、两组严格 Clippy 与生成的 RPC Schema 均通过。此轮未在 Debian Linux 来宾或外部 Agent 服务上复验。

## 关键词筛选近期上下文（2026-11-19）

`match_recent` 复用历史搜索的字面匹配和有界扫描。同一会话的匹配记录按现有字节预算入选；扫描使用精简投影，只在命中且需要完整明细时加载完整版。进程测试覆盖角色筛选、跳过超预算记录、会话边界、预览与实际捕获一致、重启后相同幂等键返回原始快照；内核测试覆盖 128 条扫描上限与非法选项。

Windows `scripts/check.ps1` 退出码为 0：423 项 workspace 测试、26 个离线演示、rustfmt、两组严格 Clippy 与生成的 RPC Schema 均通过。此轮未在 Debian Linux 来宾复验。

## 二进制产物（2026-11-19）

内核新增逐次审批的 `artifact_create_binary`。Windows 进程测试以本机 HTTP 模型替身发起工具调用，验证审批拒绝时无产物，允许后 SQLite 保存 150,003 个原始字节，重启后完整读取、跨块续读与独立计算的 SHA-256 一致，并拒绝用其他任务 ID 读取。内核测试覆盖 256 KiB 边界和无效参数，旧文本产物迁移测试仍通过。

Windows `scripts/check.ps1` 退出码为 0：425 项 workspace 测试、26 个离线演示、rustfmt、两组严格 Clippy 与生成的 RPC Schema 均通过。此轮未在 Debian Linux 来宾复验。

## A2A 内联附件（2026-11-19）

20 项 A2A app-server 进程测试通过：轮询、立即消息及 SSE 追加保留原件 JSON 和 Part 边界，重启后可以用原本地产物 ID 读取字节；URL 没有触发额外请求。内核测试覆盖重复索引、数量、单件与总预算的整批拒绝。Windows 全工作区 425 项测试、两组严格 Clippy、rustfmt 和 Schema 一致性通过。更新离线 A2A 演示的清单断言后，轮询、SSE 及两种取消演示均通过。

## 工作区历史搜索（2026-11-19）

进程测试从 SQLite v32 回填历史，覆盖同工作区跨会话命中、归档会话、隔离其他工作区、新记录入索引和重启后复用旧分页水位。同时保留原 `session/searchHistory` 的进程验收。

Windows 内存 SQLite 3.49.1 微基准使用 100,000 条模拟历史，其中 200 条属于目标工作区。首屏探针最多取 129 行（含判断是否还有下一页的一行），两种查询的首屏及后续页结果一致。61 轮交替采样下，按工作区与完成序号查索引的首屏中位数为 0.433 ms，按全局完成序号扫描并过滤工作区的中位数为 25.842 ms。此测量只覆盖 SQLite 查询和取行，不含 Rust JSON 解析、匹配或 RPC。日志为 `target/0.0.2-workspace-history-benchmark.log`。

包含工作区历史搜索及 A2A 附件的 Windows `scripts/check.ps1` 退出码为 0：426 项测试、26 个离线演示、rustfmt、两组严格 Clippy 和生成的 RPC Schema 均通过。日志为 `target/0.0.2-workspace-history-windows.log`。本轮未在 Debian Linux 来宾复验。

## A2A 增量附件组装（2026-11-19）

Windows 21 项 A2A 进程测试通过。合并超限的用例先收到合法分片，再因合并后的大小超限中断；只保存首个分片事件，无正文入库或远端重发。单元测试覆盖 256 KiB 精确边界、转义与元数据替换、256 个 Part 限制及失败后的原件完整性。

Windows 调试构建使用 96 段、197,697 字节的最终 JSON，先校验输入分片，再对比增量合并与每次完整序列化并重新验证的旧流程。21 轮交替采样中位数分别为 5.61 ms 和 699.19 ms，最终 JSON 相同。该探针只测 Rust 内存组装，不含 HTTP 或入库。日志为 `target/0.0.2-a2a-assembly-benchmark.log`。

## 降低任务恢复的内存开销（2026-11-19）

恢复时先从 SQLite 的运行索引取得最多 64 个行号，再逐条读取并提交快照。5 项内核恢复测试通过，包括跨页积压和中途写入失败后再次恢复。

Windows 调试版 app-server 在同一份种子库上交替执行旧版和新版，每版 3 次。库中有 80 个待恢复任务，每份快照约 1 MiB。进程从启动到返回 `initialize` 的峰值工作集中位数从 79.89 MiB 降至 16.00 MiB。每轮均恢复全部 80 条任务，每项只记录一次中断和模型结果未知，无产物写入。该测量仅代表这一组队列和本机环境。日志为 `target/0.0.2-recovery-memory-benchmark.log`。

包含 A2A 组装和任务恢复改动的 Windows `scripts/check.ps1` 退出码为 0：429 项测试、26 个离线演示、rustfmt、两组严格 Clippy 与 RPC Schema 均通过。日志为 `target/0.0.2-stream-assembly-recovery-windows.log`。本轮未在 Debian Linux 来宾复验。

## MCP 资源导入（2026-11-19）

HTTP 契约测试覆盖无效 Base64、无效返回 URI、数量限制、256 KiB 单项边界和 512 KiB 合计边界。超限或无效内容返回失败，后续正常读取仍复用连接。SQLite 进程内验收覆盖换源后旧审批失效、拒绝时零次读取、允许时一次读取，以及重启后从本地取回文本和二进制字节。

真实 app-server 离线演示验证 MCP SDK 附带标准 `_meta` 时，业务参数只有获批的 URI；导入标题不发送给服务。模型续轮只收到产物引用，重启后分段读取和独立 SHA-256 一致。Windows `scripts/check.ps1` 退出码为 0：430 项测试、26 个离线演示、rustfmt、两组严格 Clippy 和 RPC Schema 均通过。日志为 `target/0.0.2-mcp-resource-import-final-windows.log`。本轮未在 Debian Linux 来宾或外部 MCP 服务复验。

## Gemini Interactions 与 MCP 只读失败（2026-11-19）

Gemini 以 `store:false` 提交无状态交互。回环测试覆盖函数调用进入 Fliuno 审批、原始模型步骤和工具结果续轮、SSE 文本增量、缺少结束帧时撤销预览，以及输出上限。app-server 进程测试验证环境配置和所用 API 修订，并确认不支持的供应商计数选项在联网与建库前拒绝。MCP 回归覆盖远端 JSON-RPC 只读拒绝脱敏、原连接复用，以及传输错误仍关闭连接。

Windows `scripts/check.ps1` 通过 415 项测试、26 个离线演示、Schema 一致性、Clippy 和 rustfmt。日志为 `target/0.0.2-gemini-mcp-windows.log`。本轮未使用真实 Gemini 凭据，也未在 Debian Linux 来宾复验。

## 产物目录快照（2026-11-19）

app-server 进程测试覆盖空目录、翻页间新增、重启后的旧水位续读、精确媒体类型筛选及无效水位拒绝。`artifacts/listSnapshot` 复用现有目录索引，无 SQLite 迁移。

Windows 内存 SQLite 3.49.1 微基准使用 100,000 条目录记录，在序号 70,000 以内取同样的 14 条匹配项。无查询规划统计时，61 轮交替采样中，SQLite 自选工作区索引的查询与取行中位数为 10.2127 ms，显式使用既有工作区与媒体类型复合索引后为 0.1511 ms。该测量只覆盖 SQLite 查询及取行。日志为 `target/0.0.2-artifact-snapshot-query-plan.log`。

Windows `scripts/check.ps1` 退出码为 0。431 项测试、26 个离线演示、rustfmt、两组严格 Clippy 和 RPC Schema 均通过。日志为 `target/0.0.2-artifact-catalog-snapshot-windows.log`。本轮未在 Debian Linux 来宾复验。

## 工作区文本定位与 Debian 复验（2026-11-19）

app-server 进程用例覆盖路径越界在审批前被拒绝，拒绝时不向模型返回文件结果，以及允许后读取当前字节。测试核对中文 UTF-8 和 CRLF 的位置、同一行只返回首个命中、跨文件结果上限、二进制和不可读文件。模型收到的位置结果不含原文。

Windows `scripts/check.ps1` 退出码为 0。432 项测试、26 个离线演示、rustfmt、两组严格 Clippy 和 RPC Schema 均通过。日志为 `target/0.0.2-workspace-search-windows.log`。

Debian Linux 6.12.107 来宾通过 4 项产物进程测试和 1 项文本定位进程测试，包含产物快照跨重启续读。日志为 `D:/Fliuno-Native-Test/guest-artifact-search.log`。

## 事件回放字节预算（2026-11-19）

回放页达到字节目标后仍探测下一行是否存在。该行留在原有游标之后，直到客户端请求下一页时再解析正文。对照 Windows 调试版 app-server 的两个构建，页面、`hasMore`、精确目标边界、包含式 `through` 和下一页正文均一致。

使用两份相同的 SQLite 测试库，目标为 4 KiB，首个事件形成 24,688 字节的页面，下一事件含 1,048,576 字节正文。61 轮交替测得本机 stdio JSON-RPC 延迟中位数由 63.38 ms 降至 4.97 ms。该结果针对下一条事件较大且当前页已达到目标的场景。日志为 `target/0.0.2-event-replay-benchmark.log`。

Windows `scripts/check.ps1` 退出码为 0。432 项测试、26 个离线演示、rustfmt、两组严格 Clippy 和 RPC Schema 均通过。Debian Linux 6.12.107 来宾的 6 项事件回放及订阅进程测试均通过。日志分别为 `target/0.0.2-event-replay-windows.log` 和 `D:/Fliuno-Native-Test/guest-event-replay.log`。

## 审批决定查询（2026-11-19）

模型工具的允许和拒绝、ACP 权限的允许均可按审批 ID 取回原事件。进程测试覆盖提交前返回未找到、跨任务隔离、重启续读和 SQLite v33 回填。决定事件与任务状态及索引在同一事务提交。

Windows 内存 SQLite 3.49.1 使用同一任务的 100,000 条模拟事件，其中 400 条是决定。61 轮交替采样的命中查询中位数为全任务扫描 115.46 ms、主键索引 0.023 ms，结果逐字段一致。一次 v34 回填耗时 86.78 ms。该对照测量 SQLite 查询与取行。日志为 `target/0.0.2-approval-decision-query.log`。

Windows 通过 433 项测试、26 个离线演示、rustfmt、两组严格 Clippy 和 RPC Schema。Debian Linux 6.12.107 来宾通过 3 项审批与 1 项 ACP 进程测试。日志分别为 `target/0.0.2-approval-decision-windows.log` 和 `D:/Fliuno-Native-Test/guest-approval-decision.log`。

## 工作区任务摘要倒序分页（2026-11-19）

进程测试跨会话和工作区创建任务，检查按状态筛选与紧凑 JSON 字节目标，最后重启并新增任务，确认旧倒序游标继续向更早任务读取。默认正序的结果和显式 `oldest_first` 一致。

Windows 内存 SQLite 3.49.1 在 100,000 条模拟任务上使用既有工作区索引倒扫，带状态筛选时使用工作区与状态复合索引。两者都无临时排序。取 51 行含翻页探针的 61 轮查询中位数分别为 0.0588 ms（筛选）和 0.0797 ms（不筛选）。此测量只计 SQL 查询和取行。日志为 `target/0.0.2-newest-task-summaries-query.log`。

Windows 通过 434 项测试、26 个离线演示、rustfmt、两组严格 Clippy 和 RPC Schema。Debian Linux 6.12.107 来宾通过 4 项任务摘要及完整任务分页进程测试。日志为 `target/0.0.2-newest-task-summaries-windows.log` 和 `D:/Fliuno-Native-Test/guest-newest-task-summaries.log`。

## Rust stdio 客户端（2026-11-19）

新 crate 通过真实 app-server 验证握手、能力发现、合法 RPC 错误后的继续调用、任务通知和已提交审批决定。定向单元测试覆盖请求超限时不写入也不消耗 ID、响应 ID 错位时停用连接和响应超限。Debian 首跑发现关闭两个管道可能撞上仍在发送的通知。修正测试为先关闭写端，再读到 EOF，随后 Debian Linux 6.12.107 进程测试通过，Windows 同一关闭流程也通过。

Windows 全量检查通过 438 项测试、26 个离线演示、rustfmt、两组严格 Clippy 和 RPC Schema。关闭流程修正后，app-server 和客户端的严格 Clippy 与真实进程复验通过。日志分别为 `target/0.0.2-rust-client-windows.log`、`target/0.0.2-rust-client-final-windows.log` 和 `D:/Fliuno-Native-Test/guest-rust-client-final.log`。

## Rust 客户端事件补读（2026-11-19）

同一真实进程用例在 `events/setScope` 后按任务范围每页读取一条事件，核对审批决定已包含在固定水位内、序号递增、最终游标到达水位。客户端单元用例验证无进展的下一页被拒绝，随后可从原游标补读稀疏范围。

Windows 客户端 4 项单元测试、app-server 真实进程用例、两包严格 Clippy 和全仓格式检查通过。Debian Linux 6.12.107 来宾的真实进程用例通过。日志为 `target/0.0.2-client-event-replay-targeted.log`、`target/0.0.2-client-event-replay-clippy.log` 和 `D:/Fliuno-Native-Test/guest-client-event-replay.log`。

## 工作区文件导入产物（2026-11-19）

真实 app-server 进程测试验证拒绝及超限都无产物，恰好 256 KiB 的文件可以持久保存，模型仅收到四个引用与校验字段。删除原文件并重启后，从目录快照找到产物，分四段下载并与原始字节核对，独立 SHA-256 相同。

Windows `scripts/check.ps1` 退出码为 0。440 项测试、26 个离线演示、rustfmt、两组严格 Clippy 和 RPC Schema 通过。Debian Linux 6.12.107 来宾复验通过。日志为 `target/0.0.2-workspace-import-windows.log` 和 `D:/Fliuno-Native-Test/guest-workspace-import.log`。

## Rust 客户端直接解码应答（2026-11-19）

客户端借用行缓冲区内的 JSON-RPC 字段，直接将 `result` 解码为调用方请求的类型。回归测试覆盖合法的 `result:null`、同时携带 `result` 和 `error` 时拒绝，以及错误后的连接状态。

Windows `scripts/check.ps1` 退出码为 0。441 项测试、26 个离线演示、rustfmt、两组严格 Clippy 和 RPC Schema 校验通过。Debian 来宾的真实 stdio 客户端进程测试通过。日志分别为 `target/0.0.2-client-raw-reply-windows.log` 和 `D:/Fliuno-Native-Test/guest-client-raw-reply.log`。

Windows Release 内存帧基准按新旧构建各交替测量七轮，每轮 61 帧。123,470 字节的 100 项事件页从 1.063 ms 降至 0.4728 ms，87,488 字节的产物应答从 0.1866 ms 降至 0.1048 ms，均为各轮中位数的中位数。测量覆盖客户端帧处理，不含子进程 I/O。日志为 `target/0.0.2-raw-reply-benchmark.log`。

## Rust 分块下载与工作区显示名称（2026-11-19）

真实 stdio 进程测试分两段下载 81,000 字节的产物，原始 UTF-8 字节和 SHA-256 一致。客户端测试覆盖非法 Base64、长度和偏移、空产物、摘要不匹配以及错误后继续使用连接。调用方可以选择从已保存的摘要校验。

工作区进程测试覆盖从 SQLite v34 迁移、同名目录的独立名称、重复更新、重启后沿用清单游标，以及原目录不可用时读取和清除名称。原工作区对象在更新前后相同。

Windows `scripts/check.ps1` 退出码为 0，444 项测试、26 个离线演示、rustfmt、两组严格 Clippy 和 RPC Schema 校验通过。Debian Linux 6.12.107 来宾通过上述两项进程测试。日志为 `target/0.0.2-workspace-profiles-windows.log` 和 `D:/Fliuno-Native-Test/guest-workspace-profiles.log`。

## 工作区批量元数据（2026-11-19）

内核验收覆盖 16 项和 8 KiB 路径上限、遍历路径拒绝、拒绝审批没有结果、一次允许后按请求顺序报告文件、目录和不可用项。单路径查询沿用同一元数据读取函数。

Windows `scripts/check.ps1` 退出码为 0，445 项测试、26 个离线演示、rustfmt、两组严格 Clippy 和 RPC Schema 校验通过。Debian 来宾通过单路径及批量元数据两项测试。日志为 `target/0.0.2-batch-stat-windows.log` 和 `D:/Fliuno-Native-Test/guest-batch-stat.log`。


## 模型步骤用量记录（2026-11-19）

OpenAI Responses、Anthropic Messages 与 Gemini Interactions 的 JSON 和 SSE 响应可生成 `model_usage_reported` 持久事件。用量与对应的模型步骤结果同事务提交，工具调用随后被拒时仍可回放。Anthropic 合并累计用量及缓存输入，Gemini 使用其报告的输入和输出总量。供应商未报告用量或计数无效时，任务继续执行。SQLite v36 增加事件类型投影。

Windows `scripts/check.ps1` 退出码为 0，446 项测试、26 个离线演示、rustfmt、两组严格 Clippy 和 RPC Schema 校验通过。Debian 来宾通过 17 项模型 HTTP 契约测试和 1 项 app-server 用量持久回放进程测试。模型使用本地模拟服务，真实供应商验收待做。日志为 `target/0.0.2-provider-usage-windows.log`、`D:/Fliuno-Native-Test/guest-provider-usage.log` 和 `D:/Fliuno-Native-Test/guest-model-usage.log`。


## 0.0.2 发布前复验（2026-11-19）

清理 Cargo 的 Windows 开发、发布与 Linux 交叉构建产物后，在 Windows 重新运行 `scripts/check.ps1`。脚本以退出码 0 完成，446 项测试、26 个离线演示、rustfmt、两组严格 Clippy 和生成的 RPC Schema 一致性均通过。日志为 `target/0.0.2-release-check-windows.log`。


## 事件订阅交接与 v0.0.2 验收（2026-11-19）

真实 app-server 进程测试先建立会话订阅，再提交超出数据库水位的游标尝试切换到单任务。服务返回冲突并保留会话范围。切换请求之后新提交的任务通过实时通知送达，回放只读到建立订阅时的水位。

Windows `scripts/check.ps1` 以退出码 0 完成。446 项测试、26 个离线演示、rustfmt、两组严格 Clippy 和 RPC Schema 一致性通过。日志为 `target/0.0.2-checked-subscription-windows.log`。


## 0.0.3 模型用量分页（2026-11-19）

真实 app-server 进程测试覆盖同一任务的两步用量、限条数翻页、包含式上界、无有效用量的空结果，以及从 SQLite v36 回填后沿用原序号和游标。

SQLite 3.49.1 内存库中，使用 100,000 条模拟事件，其中 400 条报告用量。读取固定窗口中的 50 条及翻页探针，61 轮交替样本的查询中位数从逐条解析 JSON 扫描的 49.21 ms 降至索引查询的 0.259 ms。两种结果一致，索引路线只读命中的事件正文。一次回填用时 347.19 ms。该对照只计 SQLite 查询和取行。日志为 `target/0.0.3-usage-replay-bench.log`。

Windows `scripts/check.ps1` 以退出码 0 完成。446 项测试、26 个离线演示、rustfmt、两组严格 Clippy 和 RPC Schema 一致性通过。日志为 `target/0.0.3-usage-replay-windows.log`。


## 0.0.3 AG-UI 持久事件转换（2026-11-19）

真实 app-server 进程测试通过 Rust 客户端分页回放带审批和产物的已完成任务，验证运行开始、审批引用、产物引用、提交文本和运行完成依序生成。单元测试覆盖中断、显式恢复、稳定 ID、乱序及跨任务输入。与 Rust 序列化结果逐项相等的八个事件样例通过 `@ag-ui/core@1.0.5` 的运行时 Schema 检查。SDK 验证日志为 `target/0.0.3-ag-ui-sdk-contract.log`。

Windows `scripts/check.ps1` 以退出码 0 完成。448 项测试、26 个离线演示、rustfmt、两组严格 Clippy 和 RPC Schema 一致性通过。日志为 `target/0.0.3-ag-ui-windows.log`。

## 0.0.3 会话历史分页（2026-11-19）

真实 app-server 进程测试覆盖任务创建和完成顺序不同、会话隔离、后续完成后冻结的分页水位、重启后继续翻页和无效游标。字节预算测试包含多字节文本和 JSON 转义，验证超预算引用、延后完整记录和扩大预算后的重读。SQLite 3.49.1 的查询计划使用 `session_turns_cursor` 范围扫描和 `conversation_turns` 主键查找。

Windows `scripts/check.ps1` 以退出码 0 完成。450 项测试、26 个离线演示、rustfmt、两组严格 Clippy 和 RPC Schema 一致性通过。日志为 `target/fliuno-0.0.3-history-check.log`。

Rust 客户端的续页测试先保存下一页参数，再模拟水位变化和重复完成序号，确认两次失败均不推进游标。真实 stdio 进程测试读回包含多字节和转义字符的完整对话，并核对产物引用。客户端和 app-server 的严格 Clippy、25 项库测试、stdio 进程测试和 RPC Schema 一致性均通过。

## 0.0.3 AG-UI SSE 续传（2026-11-19）

真实 app-server 进程测试从任务审批前的历史回放接到实时通知。任务完成后重启服务，使用该事件的第二条 SSE 消息 ID 续传，剩余两条消息与原流逐字节相同。短写故障注入在下一条消息中间断开，验证恢复后没有跳过或重复完整消息。测试也覆盖跨任务、无效编号和未找到的续传位置。

Node EventSource 通过本机 HTTP 测试重连，自行带回最后一条完整消息的 `Last-Event-ID`。接收的五条 Rust 输出全部通过 `@ag-ui/core@1.0.5` 的运行时 Schema，并保留多字节和换行文本。日志为 `target/0.0.3-ag-ui-sse-sdk-contract.log`。

Windows `scripts/check.ps1` 以退出码 0 完成。454 项测试、26 个离线演示、rustfmt、两组严格 Clippy 和 RPC Schema 一致性通过。日志为 `target/0.0.3-ag-ui-sse-windows.log`。

## 0.0.3 Rust 客户端事件补读（2026-11-19）

真实 stdio 进程测试在同一连接批量提交任务，使服务端的通知队列实际溢出。`EventFeed` 保留会话范围并自动重订阅，补读结果逐项等于同水位的持久回放。测试确认同一工作区内的另一会话未混入结果。单元测试覆盖通知合并、乱序水位、补读期间出现缺口和旧订阅通知的交接。两 crate 的严格 Clippy、28 项库测试和真实进程测试通过。

## 0.0.3 紧凑事件水位（2026-11-19）

`watermarkOnly` 订阅让内核按匹配任务的每次提交发送一个序号。内核测试用构造计数确认，只有水位订阅时不会构造事件正文。真实 stdio 测试对照同一个包含 128 KiB 文本的审批事件。完整通知的序列化大小为 131,577 字节，水位通知实际为 89 字节，正文仍由回放完整读取。测试覆盖无效游标保留现有订阅、切回完整通知，以及紧凑队列溢出后的会话内补读。

Windows `scripts/check.ps1` 以退出码 0 完成。457 项测试、26 个离线演示、rustfmt、两组严格 Clippy 和 RPC Schema 一致性通过。日志为 `target/0.0.3-event-watermarks-windows.log`。

## 0.0.3 AG-UI 文本任务与重连（2026-11-19）

真实 stdio 进程测试先完成一条历史任务，再用 AG-UI 文本请求创建新任务。重复提交绑定同一任务，跨会话查找请求键失败，外部传入的对话正文未进入冻结上下文。关闭并重启应用服务后，用原运行 ID 找回任务，确认 SSE 输出保留首次执行的运行 ID。客户端单元测试验证不支持的输入在发出 RPC 前被拒绝。

Windows `scripts/check.ps1` 以退出码 0 完成。459 项测试、26 个离线演示、rustfmt、两组严格 Clippy 和 RPC Schema 一致性通过。接入文档的 JSON 示例通过 `@ag-ui/core@1.0.5` 的 `RunAgentInputSchema` 验证。日志为 `target/0.0.3-ag-ui-run-windows.log`。

## 0.0.3 AG-UI SSE 订阅（2026-11-19）

真实 stdio 进程测试使用同一条连接回放待审批事件、处理审批并追上持久完成事件。重启应用服务后，从完成文本帧的 ID 续传，剩余帧逐字节一致。测试还覆盖超出订阅水位的游标，以及写入端失效后必须重建 feed 的行为。

Windows `scripts/check.ps1` 通过 460 项测试、26 个离线演示、rustfmt、两组严格 Clippy 和应用协议 Schema 一致性检查。日志为 `target/0.0.3-ag-ui-sse-feed-windows.log`。

## 0.0.3 AG-UI 执行段结束与取消（2026-11-19）

真实 stdio 进程测试回放任务启动和待审批事件，再通过应用协议取消任务。SSE 以原运行 ID 输出带 `cancelled` 结果的 `RUN_FINISHED`，订阅报告已结束。从取消帧的 ID 续传时，执行段状态仍恢复到已结束，输出不会重复。`@ag-ui/core@1.0.5` 的事件 Schema 保留该取消结果。

Windows `scripts/check.ps1` 通过 461 项测试、26 个离线演示、rustfmt、两组严格 Clippy 和应用协议 Schema 一致性检查。日志为 `target/0.0.3-ag-ui-lifecycle-windows.log`。

## 0.0.3 AG-UI 共享连接回放（2026-11-19）

真实 stdio 进程测试在同一连接上交错回放两个任务，并保留第一个任务的实时订阅。推进共享水位后，独立回放得到的字节与该任务的实时输出一致。第二个读取方只收到自己任务的帧。

Windows `scripts/check.ps1` 通过 461 项测试、26 个离线演示、rustfmt、两组严格 Clippy 和应用协议 Schema 一致性检查。日志为 `target/0.0.3-ag-ui-shared-replay-windows.log`。

## 0.0.3 AG-UI HTTP 路由（2026-11-19）

回环 HTTP 进程测试在真实 stdio 服务上并发提交两个文本任务，由宿主使用共享连接处理审批。测试确认不同授权会话无法读取运行或提交到其他会话，两个响应使用各自的运行 ID。`Last-Event-ID` 指向完成文本帧时，重连只收到剩余的两条消息。不存在的帧位置返回 HTTP 400。

Windows `scripts/check.ps1` 通过 462 项测试、26 个离线演示、rustfmt、两组严格 Clippy 和应用协议 Schema 一致性检查。日志为 `target/0.0.3-ag-ui-http-windows.log`。

## 0.0.3 AG-UI HTTP 合并水位查询（2026-11-19）

回环测试记录发给真实 stdio 服务的 `events/getWatermark` 请求。两个 HTTP 响应在待审批阶段保持打开，连续四次空闲查询的间隔均至少为 145 毫秒。审批后两个响应分别结束，帧级续传仍返回相同字节。

Windows `scripts/check.ps1` 通过 462 项测试、26 个离线演示、rustfmt、两组严格 Clippy 和应用协议 Schema 一致性检查。日志为 `target/0.0.3-ag-ui-http-coalescing-windows.log`。

Debian Linux 6.12.107 来宾的 5 项 AG-UI 进程测试通过，覆盖 HTTP 授权、并发 SSE 与水位合并、跨重启重新附接、取消和帧级续传。日志为 `D:/Fliuno-Native-Test/guest-ag-ui-http-0.0.3-linux.log`。

## 0.0.3 AG-UI HTTP 审批（2026-11-19）

回环测试在两个 SSE 响应保持打开时分别读取并批准工具请求。提交者身份无法审批，其他会话的审批者和同会话其他任务的审批 ID 均无法处理。两次相同决定返回同一份已提交事件，相反决定返回 HTTP 409。

Windows `scripts/check.ps1` 通过 462 项测试、26 个离线演示、rustfmt、两组严格 Clippy 和应用协议 Schema 一致性检查。Debian Linux 6.12.107 来宾通过同一 HTTP 进程测试。日志为 `target/0.0.3-ag-ui-http-approvals-windows.log` 和 `D:/Fliuno-Native-Test/guest-ag-ui-http-approvals.log`。

## 0.0.3 审批提交与决定事件（2026-11-19）

真实 app-server 进程测试验证 `approval/respondDecision` 与事件查询返回相同的决定。重复提交相同决定或尝试提交相反决定都不会推进事件水位。重启并从 v33 回填索引后，旧审批仍可按原决定重试。HTTP 审批继续通过会话和任务绑定检查。

Windows `scripts/check.ps1` 通过 462 项测试、26 个离线演示、rustfmt、两组严格 Clippy 和应用协议 Schema 一致性检查。Debian Linux 6.12.107 来宾通过审批决定和 HTTP 路由两项进程测试。日志为 `target/0.0.3-approval-respond-decision-windows.log` 和 `D:/Fliuno-Native-Test/guest-approval-respond-decision.log`。

## 0.0.3 AG-UI HTTP 取消（2026-11-19）

真实回环测试通过 `DELETE /runs/{run_id}` 取消任务。未登录和其他会话的调用失败。授权用户连续取消两次都收到 HTTP 204，SSE 只输出一次 `cancelled` 结果。已完成任务返回 HTTP 409。

Windows `scripts/check.ps1` 通过 462 项测试、26 个离线演示、rustfmt、两组严格 Clippy 和应用协议 Schema 一致性检查。日志为 `target/0.0.3-ag-ui-http-cancel-windows.log`。

## 0.0.3 AG-UI HTTP 宿主演示（2026-11-19）

`python examples/ag_ui_http_demo.py` 启动独立 Rust 宿主及 app-server，在临时工作区完成任务提交、独立身份审批和 SSE 帧级续传。Windows `scripts/check.ps1` 通过 462 项测试和 27 个离线演示，日志为 `target/0.0.3-ag-ui-http-demo-windows.log`。


## 0.0.3 AG-UI 审批中断与续跑（2026-11-19）

真实 stdio 测试将带待审批任务的数据库从 v37 升级到 v38。审批决定与续跑键绑定同一个事件；跨重启重试保持序号不变，工具执行和产物创建各发生一次。其他会话、错误的父运行、变更后的决定以及父运行取消子运行均被拒绝。

真实 HTTP 测试通过本机回环连接验证父流在 `RUN_FINISHED.outcome.type=interrupt` 后断开。审批者可读取详情并用新 ID 续跑，提交者无此权限。重连父流不混入子运行，跨执行段游标被拒绝；重启后子运行的 SSE 可逐帧重放。原有审批路由的测试与离线演示继续通过。

Windows `scripts/check.ps1` 通过 464 项测试、28 个离线演示、rustfmt、两组严格 Clippy 及应用协议 Schema 检查。`@ag-ui/core@1.0.5` 验证了文档示例、两次实际 HTTP 输入和父子运行的 8 条 SSE 消息。日志为 `target/0.0.3-ag-ui-interrupt-windows.log` 与 `target/0.0.3-ag-ui-interrupt-sdk.log`。


## 0.0.3 AG-UI 按游标定位事件（2026-11-19）

真实 HTTP 进程测试记录宿主发送的 `events/list` 请求。子运行的每个有效帧重连都只读取一个事件页，首个读取游标为请求序号减一；流内多帧事件仍按序号与帧编号精确续传。跨执行段游标不读取事件页，超出合法帧编号的游标返回 `400`。真实 stdio 测试从 v37 和 v38 迁移数据库，验证父、子运行的边界与原事件保持一致。

另外用 60,004 条事件的合成数据库核对 SQL 查询计划：结束索引只存两条边界记录，查找按任务和序号使用主键范围。查询计划未扫描或排序事件历史。该探针验证查询路径，未测量 HTTP 延迟。日志：`target/0.0.3-ag-ui-indexed-replay-sqlite-plan.log`。

Windows `scripts/check.ps1` 通过 464 项测试、28 个离线演示、rustfmt、两组严格 Clippy 和协议 Schema 检查。日志：`target/0.0.3-ag-ui-indexed-replay-windows.log`。


## 0.0.3 AG-UI 连续两次审批（2026-11-19）

真实 app-server 接入本机模型对端，连续请求两次产物工具调用。HTTP 测试验证已解决审批不可读取，父运行 ID 既不能读取后续工具参数，也不能替子运行处理第二个审批。用正确的子运行拒绝第二次调用，新的 SSE 段以 `RUN_ERROR` 结束。重复拒绝回放同一执行段；数据库中只记录一次工具启动和一次拒绝决定。

Windows `scripts/check.ps1` 通过 465 项测试、28 个离线演示、rustfmt、两组严格 Clippy 和协议 Schema 检查。日志：`target/0.0.3-ag-ui-approval-scope-windows.log`。

Debian Linux 6.12.107 来宾使用 `4a600db` 构建的原生 app-server 和进程测试二进制，四项定向测试全部通过：v37/v38 迁移与重启去重、连续审批与拒绝、按帧重连及旧 HTTP 审批模式。交叉构建使用 Zig 0.15.2 与 cargo-zigbuild 0.24.1；QEMU 本机回环测试以 `FLIUNO_RESULT=0` 结束。日志：`D:/Fliuno-Native-Test/guest-ag-ui-approval-scope-build.log` 与 `D:/Fliuno-Native-Test/guest-ag-ui-approval-scope.log`。

## 0.0.3 历史搜索与直读（2026-11-19）

通过实际 app-server 进程创建并完成任务，再保存会话、工作区两种搜索的游标。新任务完成后重启进程，原游标仍读取旧快照；重新搜索可以看到新任务。对工作区搜索命中直接读取对话，完成序号和产物引用与命中对应。独立验证覆盖超限引用和增大预算后重读，以及拒绝其他会话、未完成、缺失或快照之后的任务。Rust 客户端还用受控响应校验空页续读和无效响应后的原游标重试。

Windows `scripts/check.ps1` 以退出码 0 完成，467 项测试、28 个离线演示、rustfmt、两组严格 Clippy 及应用协议 Schema 一致性检查全部通过。日志：`target/0.0.3-history-lookup-windows.log`。

## 0.0.3 模型用量范围分页（2026-11-20）

用本机 HTTP 模型 fixture 和真实 app-server 进程在两个工作区交错创建任务。按会话读取时隔离同工作区的其他会话；按工作区读取时包含归档会话，且不会读到其他工作区的用量。沿用已捕获的水位翻页时晚到的报告不进入快照。进程测试覆盖从 v36 的事件日志回填，以及从 v39 的任务用量索引升级后续读。Rust 客户端测试验证误收到其他事件类型时游标不前移。

Windows 的 SQLite 3.49.1 内存库对照：150,000 条事件分布在 3 个工作区，每 157 条插入一条用量报告，共 955 条。对同一范围和游标读取 32 条报告及一条续页探针，逐行比较结果一致。各测 300 次，工作区全事件索引加 JSON 条件的查询中位数为 1,648.60 µs；使用用量范围索引为 34.70 µs。查询计划确认通过 `model_usage_workspace` 覆盖索引定位，再按事件主键读取报告。此测量只包含内存查询和结果读取，不包含迁移、写入或 stdio。

Windows `scripts/check.ps1` 以退出码 0 完成，469 项测试、28 个离线演示、rustfmt、两组严格 Clippy 和协议 Schema 一致性检查全部通过。日志：`target/0.0.3-usage-scopes-windows.log`。

## 0.0.3 待审批快照（2026-11-20）

真实 app-server 进程用两个工作区和多个会话验证 v40→v41 升级前后的现有审批游标不变。捕获快照后拒绝一项、取消一个任务、再新增一项审批；重启后在同一水位重放旧页，关闭项仍位于原顺序，新审批只在新快照中出现。通过同范围的事件回放读取捕获后的三项变化，并验证已经关闭的审批不再返回当前详情。内核测试覆盖 Agent 权限在恢复时关闭，以及在固定时间点和到期边界读取历史摘要。Rust 客户端测试覆盖保存续页请求及水位或时间不一致时保留游标。

SQLite 3.49.1 的 `EXPLAIN QUERY PLAN` 确认工作区的当前页只走 `pending_approvals_workspace`；旧页对关闭项从 `closed_approvals_workspace` 的关闭水位范围开始，再同仍打开的项按原游标合并。两条输入都只保留续页所需的摘要，未读入审批工具参数。

Windows `scripts/check.ps1` 以退出码 0 完成，471 项测试、28 个离线演示、rustfmt、两组严格 Clippy 和协议 Schema 一致性检查全部通过。日志：`target/0.0.3-approval-inbox-windows.log`。

## 0.0.3 工作台快照（2026-11-20）

真实 app-server 使用两个工作区、归档会话、离线模型和本机 HTTP fixture 覆盖六种任务状态。升级 v41→v42 时保留已有任务并回填计数；保持一个模型请求未返回，重启后验证运行中任务转为中断。Rust 客户端从首屏续读审批，期间关闭一项审批后仍能读到旧快照中的原记录，新的工作台快照显示更新后的计数，同范围事件回放可读取审批决定。

SQLite 3.49.1 内存对照：250,000 条任务状态，分属 5 个工作区，查询其中 50,000 条任务的相同计数。各测 300 次，直接按状态聚合的中位数为 5,523.25 µs，按范围读取一行计数为 5.50 µs。另对同一任务交替更新两种状态，各测 600 次，单条更新的中位数从 5.40 µs 增至 15.00 µs。前者只测计数查询，后者只测语句执行，两项均不含 stdio、审批读取或事务提交。

Windows `scripts/check.ps1` 以退出码 0 完成，472 项测试、28 个离线演示、rustfmt、两组严格 Clippy 和协议 Schema 一致性检查全部通过。日志：`target/0.0.3-work-overview-windows.log`。

## 0.0.3 工作台实时订阅（2026-11-20）

真实 app-server 在同一批请求内先捕获会话快照、再提交两项取消。测试确认响应写出前已提交的范围内事件在响应之后收到水位通知，范围外的取消不送达；同批无效范围和字节预算请求不影响新订阅。另测 SQLite v41 升级与恢复后的工作区快照，Rust 同时按原水位继续读审批，并从首屏水位收到新的审批决定。

通过新接口建立会话订阅后，用 72 个同范围任务和一个范围外任务填满实时队列，检查缺口通知，并将 Rust 逐页补读结果与 SQLite 的同范围回放逐项比较。建立快照和订阅从两次 stdio RPC 合并为一次，旧接口保留。

Windows `scripts/check.ps1` 以退出码 0 完成，473 项测试、28 个离线演示、rustfmt、两组严格 Clippy 和协议 Schema 校验全部通过。Debian Linux 6.12.107 来宾也通过上述 3 项 app-server 进程测试。日志：`target/0.0.3-work-subscription-windows.log` 与 `D:/Fliuno-Native-Test/guest-work-subscription.log`。

Windows 本机 stdio 对照：同一调试构建和进程中预置 8 个待审批任务，读取 4 项的首页。分别测量 `work/getSnapshot` 加 `events/setScope` 和 `work/subscribe`，交替执行各 500 次，逐次核对计数、首页内容和水位一致。中位数分别为 952.45 µs 和 680.40 µs，约减少 29%；该测量包含请求编解码和本机管道 I/O，不包含界面刷新或后续事件页。日志：`target/0.0.3-work-subscription-benchmark.log`。

## 0.0.3 批量任务摘要（2026-11-20）

真实 app-server 重启后，用工作台订阅接上任务状态变化，按 ID 读取取消后的摘要。验收覆盖空数据库、跨工作区排除、按会话筛选、已归档会话、缺失项、重复 ID、顺序、64 项边界、无效范围和 ID。Rust 客户端拒绝长度不一致、ID 错位或会话不符的服务响应。

查询从有界输入数组按主键定位摘要与作用域，先筛范围再读取摘要。返回水位和摘要在同一 SQL 语句内读取；无需扫描任务、反序列化对话或进行数据库迁移。

Windows `scripts/check.ps1` 以退出码 0 完成，475 项测试、28 个离线演示、rustfmt、两组严格 Clippy 和协议 Schema 校验全部通过。Debian Linux 6.12.107 来宾通过批量摘要和工作台的 3 项真实进程测试。日志：`target/0.0.3-task-summary-batch-windows.log` 与 `D:/Fliuno-Native-Test/guest-task-summary-batch.log`。

Windows 本机 stdio 对照：同一个调试构建读取同一会话的 32 个待审批任务，比较包含 32 条 `task/getSummary` 的 JSON-RPC 批次和一次带会话范围的 `task/getSummaries`。两种方式交替各执行 300 次，逐次核对摘要和顺序。耗时中位数从 3,386.40 µs 降至 1,841.65 µs，约减少 46%；请求与响应的合计字节从 14,042 降至 10,663，约减少 24%。测量含本机管道 I/O 和 JSON 编解码。SQLite 3.49.1 查询计划确认作用域与摘要均走主键，无任务表扫描或临时排序。日志：`target/0.0.3-task-summary-batch-benchmark.log`。

## 0.0.3 水位通知合并（2026-11-20）

每个水位订阅使用一个唤醒槽和一个原子序号。生产方有待处理唤醒时只更新序号，读取方消费最新值后重新开启下一次唤醒；完整事件队列保持原样。单元测试覆盖等待中和已取出唤醒、丢弃未读唤醒、关闭接收端和两线程并发推进 20,000 次后的最终水位。

真实 app-server 一批取消 72 个同会话任务，并夹入 1 个范围外任务。客户端只收到 1 条水位提醒，没有缺口；用 Rust 客户端分页读回全部 72 条范围内事件，与直接从同一范围回放的结果逐项一致。批次之后再取消一个任务，验证发送端继续发出新水位。事件正文、事件序号和协议 Schema 未改变。

Windows `scripts/check.ps1` 以退出码 0 完成，477 项测试、28 个离线演示、rustfmt、两组严格 Clippy 和协议 Schema 校验全部通过。额外的范围过滤探针确认：更晚但不匹配的提交不会覆盖待发水位。Debian Linux 6.12.107 来宾通过 10 项真实进程测试，覆盖工作台、合并水位以及完整事件队列的缺口恢复。日志：`target/0.0.3-event-watermark-coalescing-windows.log`、`target/0.0.3-event-watermark-scope-rerun.log` 与 `D:/Fliuno-Native-Test/guest-event-watermark-coalescing.log`。

## 0.0.3 任务变动回放（2026-11-20）

从现有事件和范围索引只选择任务 ID 与事件序号，最多扫描 1,000 条匹配记录，再按任务保留本页最后序号。不需要迁移数据库。测试用真实 app-server 验证跨工作区隔离、归档会话、去重、重启后在原水位续页，以及没有匹配事件时推进游标。工作台订阅用实时水位读取新的取消事件，再用批量摘要核对最新状态。Rust 客户端拒绝重复 ID 或水位错误的响应，并保留原游标。

Windows 本机 stdio 对照：24 个待审批任务，共 72 条事件，每个审批携带约 48 KiB 的正文。两条接口交替各测 120 次，用完整回放计算每个任务的最后序号并逐次核对。完整回放的响应中位数为 1,195,549 字节，轻量回放为 1,755 字节；耗时中位数分别为 76.71 ms 和 0.94 ms。此结果针对含大正文的窗口，计入本机管道和 JSON 编解码，仅比较找出变动任务所需的回放，不含后续摘要查询。SQLite 3.49.1 查询计划确认先按工作区主键定位范围，再按事件主键取任务 ID，无临时排序。日志：`target/0.0.3-task-change-replay-benchmark.log`。

Debian Linux 6.12.107 来宾通过任务变动回放、工作台订阅和水位合并的 4 项真实进程测试。Windows 首轮全量检查发现既有 ACP/A2A 注册身份测试并发创建同一临时目录；为夹具加入进程内唯一序号，两个测试连续并行运行 10 轮均通过。日志：`D:/Fliuno-Native-Test/guest-task-change-replay.log`、`target/0.0.3-task-change-replay-windows.log` 与 `target/0.0.3-authority-fixture-windows.log`。

修复夹具后，Windows `scripts/check.ps1` 完整复跑以退出码 0 结束：479 项测试、28 个离线演示、rustfmt、两组严格 Clippy 和协议 Schema 检查全部通过。22 页 Markdown 的 333 个本地引用校验通过。日志：`target/0.0.3-task-change-replay-windows-rerun.log`。

## 0.0.3 任务变动摘要（2026-11-20）

`events/listTaskSummaryChanges` 在同一 SQLite 读事务内复用变动页，再按主键读取当前摘要投影。无需数据库迁移。真实进程测试在固定事件窗口后取消任务，确认历史页返回较新的摘要，但 Rust 客户端只提交已消费游标；重启后续读归档会话中的任务，再接上工作台实时变动。用例覆盖工作区、会话、任务和全局范围、空尾页、边界错误。模拟服务返回错误水位、重复任务、越界序号或不符的会话时，客户端保留原游标。

Debian Linux 6.12.107 来宾通过回放、摘要、工作台订阅和水位合并的 5 项真实进程测试，`FLIUNO_RESULT=0`。日志：`D:/Fliuno-Native-Test/guest-task-summary-change-replay.log`。

Windows `scripts/check.ps1` 以退出码 0 完成：481 项测试、28 个离线演示、rustfmt、两组严格 Clippy 和生成的协议 Schema 全部通过。日志：`target/0.0.3-task-summary-change-replay-windows.log`。

Windows 本机 stdio 对照：同一个调试构建，24 个待审批任务、72 条事件，比较固定窗口首屏扫描 64 条事件后取得的 22 个摘要。预热 40 对后交替测量 200 对，逐次核对两条路径的摘要、顺序、游标和两个水位。两次调用 `events/listTaskChanges` + `task/getSummaries` 的耗时中位数为 1,425.30 µs，请求与响应合计 9,520 字节；一次 `events/listTaskSummaryChanges` 为 1,086.35 µs 和 7,430 字节，分别少约 24% 和 22%。结果包括本机管道与 JSON 编解码，仅代表该负载。日志：`target/0.0.3-task-summary-change-replay-benchmark.log`。

## 0.0.3 持久产物摘要（2026-11-20）

新产物把正文长度、SHA-256 与正文一起提交。旧产物第一次查询时按原 8 MiB 总限制、64 KiB 缓冲分块读取，再持久保存摘要。升级无需迁移或扫描正文；补算不会新增事件、改变目录水位或公开额外元数据。重复查询只访问元数据，与下载的分块读取分开。

真实进程测试覆盖新产物、旧格式补算、超限拒绝、跨任务查询拒绝，以及重启后下载时发现同长度的正文损坏。Debian Linux 6.12.107 来宾通过 6 项相关进程测试，也覆盖 v20 升级、二进制字节保留、目录续页和工作区文件导入。日志：`D:/Fliuno-Native-Test/guest-stored-artifact-digest.log`。

Windows 本机 stdio 对照：对同一批字节分别用 `5d03bd8` 和新代码的调试构建调用 `artifacts/digest`，交替采样并与 Python hashlib 结果逐次比对。256 KiB 重复查询测量 200 对，中位数由 603.10 µs 降至 190.20 µs；8 MiB 测量 30 对，由 13,061.25 µs 降至 259.15 µs。每种大小另有 10 对预热。旧格式的首次补算仍需完整读取并写元数据，本次分别为 3,578.60 µs 和 13,180.20 µs。这一测量包括本机管道和 JSON 编解码，只比较摘要请求，不衡量产物提交或正文下载。日志：`target/0.0.3-stored-artifact-digest-benchmark.log`。

Windows 首轮全量检查发现既有工作台恢复测试在 HTTP 夹具读完请求前终止子进程。`ae7b45e` 让测试确认模型请求已到达后再触发重启，定向连续通过 10 次。修复后 `scripts/check.ps1` 全量通过 482 项测试、28 个离线演示、格式、Clippy 和协议 Schema 检查。日志：`target/0.0.3-stored-artifact-digest-windows.log` 与 `target/0.0.3-stored-artifact-digest-windows-rerun.log`。

## 0.0.3 交付物下载续传（2026-11-20）

用 `Client::resume_artifact_download` 按持久暂存文件的实际字节长度续传。客户端用同一个有界缓冲区重算本机前缀的 SHA-256、解码后续分块并校验整份文件。超长暂存文件在发出 RPC 前拒绝，已完整文件直接本地验证，损坏的已有前缀即使收到正确的后缀也不能通过最终摘要。

在真实 app-server 测试中下载 216,000 字节的产物，第一块写入后断开管道，并模拟暂存文件末尾仍有两字节未落盘。服务重启后重新打开文件，真实管道只发出 `after=65534,131070,196606` 三次分块请求；起点在 UTF-8 多字节字符内部。测试核对了所有原始字节，并再次验证完整文件不会增加 RPC。与从头下载相比，该次恢复无需重传已有的 65,534 字节正文。

Debian Linux 6.12.107 来宾的 6 项产物进程测试全部通过，涵盖下载续传、持久摘要、历史数据库升级、二进制产物和目录快照。日志：`D:/Fliuno-Native-Test/guest-artifact-download-resume.log`。

Windows `scripts/check.ps1` 以退出码 0 完成，484 项测试、28 个离线演示、格式、Clippy 和协议 Schema 检查全部通过。日志：`target/0.0.3-artifact-download-resume-windows.log`。

## 0.0.3 交付物目录订阅（2026-11-20）

`artifacts/subscribe` 在同一个内核锁作用域中取得目录首页、当前事件水位、同范围水位订阅。不读取交付物正文，也不改变 SQLite 表。目录和事件分别保存游标；媒体类型只作用于目录页。Rust SDK 验证页大小、相邻游标关系、固定上界、重复 ID 和媒体类型，拒绝错误结果时保留续页参数。

进程测试固定一份会话目录，首屏之后提交两份产物，只有当前会话的产物进入订阅事件流。失败的订阅请求不会把原会话扩大到工作区。服务重启后，用保存的目录请求读完旧窗口，再从目录水位读取新产物；同时用单独的事件水位重放 `artifact_created`。测试还覆盖最后一条匹配结果早于全局目录水位，以及没有新增项的下一窗口。

Debian Linux 6.12.107 来宾的 6 项交付物进程测试全部通过，包含目录订阅、下载续传、目录快照、数据库迁移与二进制产物。日志：`D:/Fliuno-Native-Test/guest-artifact-catalog-subscription.log`。

Windows `scripts/check.ps1` 以退出码 0 完成，486 项测试、28 个离线演示、格式、Clippy 和协议 Schema 检查全部通过。日志：`target/0.0.3-artifact-catalog-subscription-windows.log`。

使用同一个 Windows 调试服务，创建 24 份产物，每次返回首屏 16 条。预热 25 对后交替测量 200 对，每次核对目录结果和两个水位。分开调用 `events/setScope` 与 `artifacts/listSnapshot` 的中位耗时为 751.25 µs，请求和响应共 3563 B；单次 `artifacts/subscribe` 为 599.60 µs、3390 B。这是本机 stdio 场景的测量。日志：`target/0.0.3-artifact-catalog-subscription-benchmark.log`。

## 0.0.3 批量交付物摘要（2026-11-20）

`artifacts/getDigests` 通过 SQLite 的产物 ID 索引读取已保存的摘要，不打开正文，也不补算缺失的摘要。真实进程测试覆盖最大批量、非法批次、重复项、跨任务身份、未记录的旧产物、单份补算后跨重启复用，以及正文损坏后下载校验拒绝。同一批查询前后事件水位保持不变。

Windows `scripts/check.ps1` 以退出码 0 完成，486 项测试、28 个离线演示、格式、Clippy 和协议 Schema 检查全部通过。Debian Linux 6.12.107 来宾中 7 项交付物进程测试通过，日志分别在 `target/0.0.3-artifact-digest-batch-windows.log` 和 `D:/Fliuno-Native-Test/guest-artifact-digest-batch.log`。

用同一个 Windows 调试服务经 stdio 查询 24 份不同的已保存摘要。每种方式预热 20 次后交替测量 150 次；逐次比对全部摘要，统计包含完整请求和响应。结果只代表这次本机测量，不包含正文下载。

| 调用方式 | 中位耗时 | 请求与响应 |
| --- | ---: | ---: |
| 24 次 `artifacts/digest` | 4.543 ms | 7536 B |
| 一帧 JSON-RPC batch 内 24 次 `artifacts/digest` | 1.590 ms | 7540 B |
| 一次 `artifacts/getDigests` | 0.944 ms | 5894 B |

测量日志：`target/0.0.3-artifact-digest-batch-benchmark.log`。

## 0.0.3 模型用量汇总（2026-11-20）

`modelUsage/getSummary` 使用范围主键和事件水位读取固定数量的数据。新报告及任务、会话、工作区计数同事务提交。SQLite v43 只回填有索引的报告；SQLite 查询计划检查确认汇总按主键查找，运行时写入的三个计数也分别按主键定位。

真实进程通过本机 OpenAI 协议 fixture 记录报告，覆盖空范围、任务和会话隔离、同工作区归档会话、外部工作区不计入、显式 `0` 与缺失可选值、合计超过 `u64`、v42 升级后累加与跨重启一致。用快照水位接续原有用量回放，只看到快照后的报告。

Windows `scripts/check.ps1` 以退出码 0 完成，487 项测试、28 个离线演示、格式、Clippy 和协议 Schema 检查全部通过。Debian Linux 6.12.107 来宾 3 项用量进程测试全部通过。日志：`target/0.0.3-model-usage-summary-windows.log` 和 `D:/Fliuno-Native-Test/guest-model-usage-summary.log`。

在同一个 Windows 调试服务中，由 400 个真实任务经本机 fixture 记录用量。每种方式预热 20 次后交替测量 120 对，每次比对完整累计值和水位。每页上限 100 条时，先取水位再分页回放并求和的中位耗时为 19.498 ms，合计 84114 B 请求与响应；一次汇总为 0.288 ms、422 B。这是本机 stdio 读取对比，不计模型调用、数据库写入或迁移耗时。日志：`target/0.0.3-model-usage-summary-benchmark.log`。

## 0.0.3 批量取消任务（2026-11-20）

`task/cancelBatch` 在一个 SQLite 事务中检查归属、关闭审批、提交取消及更新任务摘要和计数。全部提交后才发布事件和通知后台执行器。单项取消也采用先提交后通知的顺序；API 返回的是本地持久状态，执行器结果仍通过原有事件路径观察。

真实进程测试通过本地模型 fixture 创建待审批、已完成及在途任务。范围错误和非法批次不写入任何事件；通过测试数据库触发器拒绝第二次写入，确认第一项任务、审批、计数和水位一并回滚。取消成功后重放得到两条有序事件，范围外任务未变，已取消任务重启后重试未产生新事件。

Windows `scripts/check.ps1` 以退出码 0 完成，488 项测试、28 个离线演示、格式、Clippy 和协议 Schema 检查全部通过。Debian Linux 6.12.107 来宾通过 14 项相关进程测试，覆盖批量取消、工作台快照、审批、模型 HTTP 在途取消、A2A 和本机命令。日志：`target/0.0.3-task-cancel-batch-windows.log` 与 `D:/Fliuno-Native-Test/guest-task-cancel-batch.log`。

在同一个 Windows 调试服务上，每个样本取消 24 个不同的待审批任务。每种方式预热 3 次，交替测量 15 次，逐批核对任务顺序、审批清除和新增取消事件数。只计取消请求及响应，不计任务准备和外部清理。

| 调用方式 | 中位耗时 | 请求与响应 |
| --- | ---: | ---: |
| 24 次 `task/cancelSummary` | 42.518 ms | 10368 B |
| JSON-RPC batch 内 24 次 `task/cancelSummary` | 39.755 ms | 10372 B |
| 一次 `task/cancelBatch` | 11.139 ms | 8427 B |

日志：`target/0.0.3-task-cancel-batch-benchmark.log`。

## 0.0.3 Rust 终端续读（2026-11-20）

`fliuno-client` 使用既有 `terminal/tail`、`terminal/output` 与订阅通知。客户端校验会话、终端 ID、保留起点、缺口和字节长度，再交付原始 PTY 字节。应用处理后保存字节游标；下一条连接可以从该位置建立新的订阅。应用协议 Schema 与 SQLite 版本不变。用法见 [Rust 终端客户端](terminal-subscriptions.md#rust-客户端)。

进程用例启动真实 PTY，通过 Rust 客户端比对末尾与指定范围，重启应用服务后从保存的位置订阅，按 7 字节预算重组退出输出，并核对结束游标。单元用例验证其他订阅可继续路由，错误会话、损坏的 base64 和不匹配的结束游标不会推进位置。

Windows `scripts/check.ps1` 退出码为 0：490 项测试、28 个离线演示、格式、Clippy 和 Schema 检查通过。Debian Linux 6.12.107 来宾有 12 项终端进程测试通过，包括新 Rust 用例；另外 2 项要求委派 cgroup v2，在这次来宾运行中按测试标记跳过。日志：`target/0.0.3-terminal-sdk-windows.log` 和 `D:/Fliuno-Native-Test/guest-terminal-sdk.log`。

## 0.0.3 会话目录倒序分页（2026-11-20）

`session/list` 可选择 `newest_first`，保留原参数、默认正序、归档过滤和行游标。内部复用工作区索引和归档表达式索引逆向定位，无新增索引或数据库迁移。Rust 的 `SessionCatalog` 校验工作区、归档状态、页内重复项和游标方向；收到不合契约的页时保留原续读位置。见 [会话目录](protocol.md#会话目录)。

进程测试验证正序兼容、按工作区隔离、归档过滤、无效排序与游标、Rust 续页请求序列化，并在应用服务重启期间创建新会话、重命名旧会话。原倒序页从保存位置继续，看到旧会话的新名称；重拉首页才看到较新创建的会话。

Windows `scripts/check.ps1` 以退出码 0 完成，492 项测试、28 个离线演示、格式、Clippy 和协议 Schema 检查通过。Debian Linux 6.12.107 来宾的 6 项会话进程测试通过。日志：`target/0.0.3-session-catalog-windows.log` 与 `D:/Fliuno-Native-Test/guest-session-catalog.log`。

SQLite 3.49.1 的内存基准使用 24 万条会话，分配到 4 个工作区，一半归档。过滤和未过滤的倒序查询计划都以工作区和 `rowid` 上界走索引；过滤场景同时使用归档表达式等值条件。以 20 条页（加一条判定是否有更多）对比第 8,000 条之后的等价 `OFFSET` 查询。20 次预热后交替测量 200 对，结果逐行相同。SQL 中位耗时为 47.10 µs 对 603.60 µs；不含 stdio、序列化或真实磁盘耗时。日志：`target/0.0.3-session-catalog-sqlite.log`。

## 0.0.3 任务摘要重试回执（2026-11-20）

SQLite v44 只新增回执表，不扫描旧任务。普通带键提交把输入 SHA-256、后端和规范化后的上下文选项与任务、摘要、键和 `task_started` 一起提交。`task/startSummary` 重试时读取回执和当前摘要；已存在的任务不重新调度。旧键、审批续跑和远端续接仍走原身份校验。协议参数及 JSON Schema 不变。见 [任务提交与重试](protocol.md#任务提交与重试)。

真实进程测试从 v43 重启并再次读取旧键；模拟回执插入失败后确认任务、键、回执和事件数量均不变。批准新任务并重启后，测试临时替换该已完成的任务正文，证明摘要重试能从紧凑记录恢复相同任务，而 `task/get` 无法读取损坏的夹具。还验证了变更输入、后端或上下文会冲突，以及只有一个 `task_started`。Debian Linux 6.12.107 来宾通过 26 项相关进程测试，覆盖回执、上下文、审批、AG-UI 和 A2A 续接。日志：`D:/Fliuno-Native-Test/guest-task-start-receipt.log`。

Windows `scripts/check.ps1` 以退出码 0 完成：493 项测试、28 个离线演示、格式、Clippy 和协议 Schema 检查通过。日志：`target/0.0.3-task-start-receipt-windows.log`。

Windows 本机 stdio 调试服务基准使用同一项已完成任务。提交输入较短，冻结历史使任务快照达到 126,969 B；回执的 SHA-256 与选项净载荷合计 101 B。用两个指向同一任务的键分别走回执和旧路径，逐次比对相同摘要。预热 40 对后交替测量 400 对，中位往返为 301.05 µs 对 1,060.45 µs；不计模型执行或首次写入的成本。日志：`target/0.0.3-task-start-receipt-benchmark.log`。

## 0.0.3 工作台最近任务（2026-11-20）

`work/getSnapshot` 与 `work/subscribe` 可请求 1 至 50 个最近创建的 `TaskSummary`。读取事务固定状态计数、待审批首页、摘要和全局事件水位；订阅在内核提交锁内接上该水位。应用协议仍为 `0.1`，用 `workRecentTasks` 发现功能。未请求时不查询摘要；Rust 客户端连接旧服务时省略该参数。见 [工作台快照](protocol.md#工作台快照)。

真实进程用例覆盖全局、工作区和会话范围、归档会话、空范围及无效上限。批请求先订阅再取消任务，验证返回的快照仍显示取消前的任务与待审批记录，订阅随后从下一水位收到取消；刷新快照后任务与计数一同更新。SDK 用例验证旧服务兼容，以及拒绝缺项、重复 ID、越界条数和错误会话。Debian Linux 6.12.107 来宾的 6 项相关进程测试通过。日志：`D:/Fliuno-Native-Test/guest-work-preview.log`。

在应用服务创建的 SQLite v44 库上，`EXPLAIN QUERY PLAN` 确认会话使用 `tasks_session`，工作区使用 `task_summary_workspace_cursor`，全局逆向访问任务行；三种查询都按主键取紧凑摘要，无临时排序和数据库迁移。

Windows `scripts/check.ps1` 以退出码 0 完成：495 项测试、28 个离线演示、格式、Clippy 和协议 Schema 检查通过。日志：`D:/Fliuno-Native-Test/windows-work-preview-check.log`。

## 0.0.3 会话任务目录与 Rust 续页（2026-11-20）

`task/listSummaries` 增加 `order`，默认保持创建正序，倒序以任务行位置向前定位。服务以 `sessionTaskSummaryOrdering` 声明功能；已有的工作区排序能力和响应契约不变。Rust `TaskSummaryCatalog` 统一浏览两种范围，把状态、顺序、预算和游标一起保存；无效页或 RPC 错误不会推进位置。见 [任务目录](protocol.md#任务目录)。

真实进程测试保存一个受字节预算限制的倒序检查点，在服务重启后创建更新的任务并取消一个尚未展示的任务。续页跳过新创建及不再符合筛选的任务，刷新首页能看到新任务。另验证默认正序、两种范围的成员隔离及无效排序和游标。Debian Linux 6.12.107 来宾的 8 项相关进程测试通过，覆盖任务目录、旧任务摘要、会话目录和工作台。日志：`D:/Fliuno-Native-Test/guest-task-catalog.log`。

SQLite 3.49.1 使用实际 v44 结构的内存副本，填充 16 万条任务分到 4 条会话。筛选时使用 `task_summary_status_cursor`，未筛选时使用 `tasks_session`；均直接定位游标并按主键读紧凑摘要，无临时排序或迁移。对比倒序第 8,000 条之后同样的 20 条，先核对结果一致，预热 20 对后交替测量 200 对。筛选中位耗时为 95.40 µs 对 `OFFSET` 的 5,211.45 µs；未筛选为 94.60 µs 对 6,281.90 µs。此合成基准只测本机 SQL，不含服务和磁盘开销。日志：`D:/Fliuno-Native-Test/task-catalog-query-plan.log`。

Windows `scripts/check.ps1` 以退出码 0 完成：497 项测试、28 个离线演示、格式、Clippy 和协议 Schema 检查通过。日志：`D:/Fliuno-Native-Test/windows-task-catalog-check.log`。

## 0.0.3 AG-UI 空闲流任务水位（2026-11-20）

`events/getTaskWatermarks` 按输入顺序返回最多 64 个任务的最新持久事件序号，缺少事件的任务为 `null`。SQLite 3.49.1 的查询计划使用已有 `events_task_sequence` 覆盖索引，VM 取得索引末端后直接完成 `MAX(sequence)`，无需读事件正文或新增迁移。应用协议 `0.1` 增加方法和结果类型，原契约不变。日志：`D:/Fliuno-Native-Test/task-watermarks-query-plan.log`。

AG-UI HTTP 仍用全局水位校验起始回放和重连位置。空闲时，每个响应登记正在观察的任务；同一任务的多个响应共享批量水位样本，无关提交不会推动其回放窗口。不声明 `taskEventWatermarks` 的服务沿用全局查询。见 [HTTP 接入](ag-ui.md#http-接入)。

真实进程测试让三条 SSE 响应观察两个任务，向另一个会话提交四组任务启动与取消。记录的 stdio 请求显示，每个空闲样本只查询这两个任务，没有全局轮询，也没有 `events/list` 调用；审批后两条运行仍正确结束，指定 SSE 帧可续传。协议测试验证请求顺序、重复和缺失 ID、边界拒绝、仅匹配事件推进及跨重启结果一致。Debian Linux 6.12.107 来宾的 7 项进程测试通过，覆盖工作台、事件流、AG-UI HTTP 和审批中断。日志：`D:/Fliuno-Native-Test/guest-task-watermarks.log`。

Windows `scripts/check.ps1` 以退出码 0 完成：498 项测试、28 个离线演示、格式、Clippy 和协议 Schema 检查通过。日志：`D:/Fliuno-Native-Test/windows-task-watermarks-check.log`。

## 0.0.3 批量模型用量汇总（2026-11-20）

`modelUsage/getSummaries` 从同一 SQLite 读视图返回最多 64 个混合范围的累计值及全局事件水位。重复范围只读取一次，响应保留各个位置；未知范围为 `null`，存在且没有报告的范围返回零计数。复用 v43 累计表和既有主键，不改变数据库 v44，也不加载任务或事件正文。见 [批量用量汇总](protocol.md#批量用量汇总)。

进程测试用回环 HTTP 模型夹具生成超过 64 位的累计值，混合工作区、会话和任务范围，区分未知、空范围、重复和另一个工作区。验证 64 项上限、非法参数整体拒绝、v42 回填后结果相同及重启后新报告可见。Rust 客户端测试验证回包长度、水位及重复项不一致会被拒绝，下一次调用仍可成功。Debian Linux 6.12.107 来宾的 7 项用量、工作台和任务回放进程测试通过。日志：`D:/Fliuno-Native-Test/guest-usage-summary-batch.log`。

SQLite 3.49.1 查询计划确认三类范围均走主键索引，无扫描。Windows 本机调试服务用 32 个不同范围，预填固定累计记录，对比一次批量 RPC 与 32 次顺序 RPC。先核对计数及水位一致，预热 20 对后交替测量 200 对，往返中位耗时为 3.48 ms 对 10.59 ms。结果包含本机 stdio 和序列化开销，不含模型请求。日志：`D:/Fliuno-Native-Test/usage-summary-batch-benchmark.log`。

Windows `scripts/check.ps1` 以退出码 0 完成：499 项测试、28 个离线演示、格式、Clippy 和协议 Schema 检查通过。日志：`D:/Fliuno-Native-Test/windows-usage-summary-batch-check.log`。

## 0.0.3 任务摘要修订回放（2026-11-20）

SQLite v45 保存升级时的事件水位，无需回填历史。在更新任务摘要的事务内，只为投影实际变化写入一条修订索引记录。应用协议 `events/listTaskSummaryRevisions` 在旧水位前保守回放任务事件，之后仅查修订记录，输出当前摘要和独立的消费游标。见 [任务摘要修订回放](protocol.md#任务摘要修订回放)。

SQLite 3.49.1 的查询计划确认，全局读取走序号主键范围，任务、会话、工作区读取各走覆盖索引。新段无需查询事件表或解码工具输出。日志：`D:/Fliuno-Native-Test/task-revisions-query-plan.log`。

真实进程测试从 SQLite v44 升级，覆盖带预算的跨边界分页、四种范围、归档、跳过没有修改摘要的工具事件，以及服务重启后从历史回放接上实时订阅。Debian Linux 6.12.107 来宾共通过 12 项相关进程测试，另覆盖启动回执、审批、取消和工作台。日志：`D:/Fliuno-Native-Test/guest-task-revisions.log`。

Windows `scripts/check.ps1` 以退出码 0 完成：500 项测试、28 个离线演示、格式、Clippy 和协议 Schema 检查通过。日志：`D:/Fliuno-Native-Test/windows-task-revisions-check.log`。

## 0.0.3 批量审批回执（2026-11-20）

`approval/getDecisions` 在同一读事务内取得事件水位和最多 64 个回执。复用 SQLite v34 的任务与审批联合主键索引，不改变数据库 v45。重复键只查询一次；未记录决定的项保留为空位。Agent 回执是本地已提交决定的证据，不能证明对端已收到。见 [批量读取审批决定](protocol.md#批量读取审批决定)。

真实服务测试覆盖允许、拒绝、待审批、取消后无决定、跨任务错配、重复位置、边界和重启，验证查询不推进事件水位或重复产生交付物。Rust 客户端拒绝绑定、水位或重复项不一致的回包。Windows 的定向检查和 Debian Linux 6.12.107 来宾的 7 项对应进程测试通过，包括真实隔离 ACP 进程的回执验证。Linux 日志：`D:/Fliuno-Native-Test/guest-approval-decision-batch.log`。

SQLite 3.49.1 的查询计划按联合主键定位决定，再按事件序号直读事件表。Windows 本机调试服务对 32 条不同的真实持久回执比较一次批量 RPC 和 32 次顺序调用，先核对投影字段完全相等。预热 20 对后交替测量 200 对，中位耗时分别为 1.12 ms 和 8.20 ms；结果包含本机 stdio 与编解码开销。日志：`D:/Fliuno-Native-Test/approval-decision-batch-benchmark.log`。

Windows `scripts/check.ps1` 以退出码 0 完成：502 项测试、28 个离线演示、格式、Clippy 和协议 Schema 检查通过。日志：`D:/Fliuno-Native-Test/windows-approval-decision-batch-check.log`。

## 0.0.3 历史检索按需解码（2026-11-20）

`session/searchHistory`、`workspace/searchHistory` 和 `match_recent` 共享按完成序号扫描的路径。借用 SQLite 行中的 JSON、任务 ID 和会话 ID，未转义的消息直接借用，有转义时仍解码为原文。搜索无需构造完整历史对象；上下文选择仅对命中项解码完整投影。仍使用既有作用域索引、Unicode 小写匹配、默认 128 条扫描上限、128 KiB 单条搜索上限和原游标。无数据库迁移或协议变动。

Windows 对照使用同一数据库的 SQLite 快照、两个本机调试服务和 128 条历史；目标在最早一条，每次请求都比较结果完全相等。10 对预热后交替采样 40 对。每条约 55 KiB 的 ASCII 历史搜索中位数由 107.54 降至 100.05 ms，混合中文与英文由 168.38 降至 155.48 ms。混合文本选入上下文由 170.33 降至 162.29 ms，短历史搜索由 2.50 降至 1.66 ms；含大量引号和控制字符转义的样本为 145.31 到 148.21 ms，未见提速。新匹配循环曾造成大文本回退，已经撤回，生产代码沿用原匹配逻辑。日志：`D:/Fliuno-Native-Test/history-scan-benchmark.log`。

Debian Linux 6.12.107 来宾通过 11 项相关进程测试，覆盖会话和工作区搜索、跨重启分页、归档、关键词上下文、历史字节预算及工作台订阅。核心单测另验证 希腊语词尾 sigma、Unicode 展开、转义控制字符、文本角色、消息边界以及产物标题和工具输出不参与匹配。Linux 日志：`D:/Fliuno-Native-Test/guest-history-scan.log`。

Windows `scripts/check.ps1` 以退出码 0 完成：503 项测试、28 个离线演示、格式、Clippy 和协议 Schema 检查通过。日志：`D:/Fliuno-Native-Test/windows-history-scan-check.log`。

## 0.0.3 可调历史扫描窗口（2026-11-20）

会话和工作区历史搜索可显式传 `scanLimit`，接受 1～512，默认仍为 128。两个停止条件独立：扫描条数或命中数先达到对应上限，就返回本页和最后扫描位置。旧请求及旧游标可以续用；`match_recent` 不读取这项参数，仍固定扫描最近最多 128 条。见 [查找历史任务](protocol.md#查找历史任务)。

真实服务进程通过 Rust 客户端获取小预算空页并保存检查点，重启后补读同一水位，即使其间已有新的匹配历史。测试验证扩大窗口可以一次返回相同命中、拒绝 0 和 513，并覆盖两个搜索范围。核心测试将命中放到第 129 条，验证大预算可跨过旧默认窗口、跳过超限记录；客户端测试验证缓存检查点不因旧服务或超预算响应而前进。

Windows 本机调试服务使用 257 条实际完成的任务，把唯一命中放在最早一条。默认窗口分三次读 `128 + 128 + 1` 条，`scanLimit:512` 一次读完，命中和水位相同。预热 25 对后交替测量 100 对，本轮同机有验证任务运行，中位耗时分别为 7.00 和 5.54 ms。更大的窗口会集中更多读取工作，实测不代表其他数据规模或机器上的收益。日志：`D:/Fliuno-Native-Test/history-scan-limit-benchmark.log`。

Debian Linux 6.12.107 来宾通过 11 项相关进程测试，覆盖历史搜索、分页和直接读取、上下文冻结以及工作台快照与订阅。日志：`D:/Fliuno-Native-Test/guest-history-scan-limit.log`。

Windows `scripts/check.ps1` 以退出码 0 完成：504 项测试、28 个离线演示、格式、Clippy 和协议 Schema 检查通过。日志：`D:/Fliuno-Native-Test/windows-history-scan-limit-check.log`。

## 0.0.3 每任务模型尝试上限（2026-11-20）

模型任务可设置 `maxModelSteps`，只降低现有的 8 次默认上限。默认任务和旧两项式重试回执形状不变；SQLite v46 只更新版本水位，无需扫描历史。Windows 上用实际 v45 服务创建任务，再用 v46 服务按同一请求 ID 找回旧任务，另建限额任务；v45 服务在启动时明确拒绝再次打开升级后的数据库。见 [模型尝试次数](protocol.md#模型尝试次数)。

真实进程测试跨重启核对两种提交入口、默认等价性和非法值。最后一次响应提议的工具获批后仅生成一份产物；下一次模型调用被阻止。环回供应商收到一次请求并返回不可用响应时，续跑拒绝且原任务、结果不明事件和事件水位保持不变。

Debian Linux 6.12.107 来宾通过 15 项进程测试，涵盖模型预算、启动回执和审批。日志：`D:/Fliuno-Native-Test/guest-model-step-limit.log`。

Windows `scripts/check.ps1` 以退出码 0 完成：507 项测试、28 个离线演示、格式、Clippy 和协议 Schema 检查通过。日志：`D:/Fliuno-Native-Test/windows-model-step-limit-check.log`。

## 0.0.3 AG-UI 宿主模型尝试上限（2026-11-20）

`fliuno-ag-ui-http::RouterBuilder` 可给新任务设置限额，沿用 SQLite v46 的任务参数与重试规则。现有构建函数继续使用默认上限。配置了限额时，Rust 客户端先检查握手能力，不支持就不发送提交。请求正文不能覆盖宿主设置；同一执行段的 SSE 重连和审批续跑均不重新应用宿主配置。见 [宿主模型尝试上限](ag-ui.md#宿主模型尝试上限)。

进程测试通过真实 HTTP 启动连续流和审批中断两种任务，拒绝请求体中的限额覆盖。收到审批后，在默认策略下重启 app-server 与 HTTP 路由。原根 ID 重新提交返回冲突；通过原审批继续后只产生一份交付物，随后 SSE 报告尝试用尽。重连可读取完整结果，重复续跑不会重复交付。Debian Linux 6.12.107 来宾通过 8 项进程测试，涵盖两种新场景和既有 SSE 重连、审批及任务绑定。日志：`D:/Fliuno-Native-Test/guest-ag-ui-host-limit.log`。

Windows `scripts/check.ps1` 以退出码 0 完成：510 项测试、28 个离线演示、格式、Clippy 和协议 Schema 检查通过。日志：`D:/Fliuno-Native-Test/windows-ag-ui-host-limit-check.log`。

## 0.0.3 任务恢复判断（2026-11-20）

`task/getResumeStatus` 只读返回任务状态、恢复判断和模型剩余尝试次数。真正的 `task/resume` 使用同一判断，并在准备启动 Agent 时仍校验注册身份。查询不写入事件、不保留调度名额，与既有任务和数据库 v46 兼容。见 [任务恢复判断](protocol.md#任务恢复判断)。

真实回环供应商收到一次请求并返回不可用响应后，查询与未显式同意的恢复均不新增事件；同意后恰好再发一次并成功。用尽上限、启动过的 ACP Agent、A2A 远端续接及本机工具超时均返回对应阻止原因。Windows ACP 测试另外验证注册改变后恢复直接拒绝，原中断快照跨重启保持；按新配置创建的任务仍可运行。Debian Linux 6.12.107 来宾的 12 项进程测试通过。日志：`D:/Fliuno-Native-Test/guest-resume-status.log`。

Windows `scripts/check.ps1` 以退出码 0 完成：512 项测试、28 个离线演示、格式、Clippy 和协议 Schema 检查通过。日志：`D:/Fliuno-Native-Test/windows-task-resume-status-passed-check.log`。

## 0.0.3 AG-UI 审批重连（2026-11-20）

`approval/getRunReplay` 一次读取运行绑定、执行段范围和已提交水位。SQLite 查询计划使用请求绑定、执行段结束索引和 `events_task_sequence` 查找，没有扫描事件表。协议仍兼容旧服务；客户端测试核对旧请求顺序，并确认从指定帧之后输出。见 [审批中断与续跑](ag-ui.md#审批中断与续跑)。

真实 HTTP 进程在已结束的父运行和子运行中各只发送一次 `approval/getRunReplay` 和一页 `events/list`。该页以各自的最后事件为上限；跨执行段的游标返回 `400`，不发送 `events/list`。环回流式供应商测试验证进行中的执行段取得任务最新水位，完成后以本段结束事件为上限。Debian Linux 6.12.107 来宾通过 15 项进程测试，覆盖这些重连行为及审批读写和迁移。日志：`D:/Fliuno-Native-Test/guest-ag-ui-run-replay.log`。

Windows `scripts/check.ps1` 以退出码 0 完成：514 项测试、28 个离线演示、格式、Clippy 和协议 Schema 检查通过。日志：`D:/Fliuno-Native-Test/windows-ag-ui-run-replay-check.log`。

## 0.0.3 恢复状态按需解码（2026-11-20）

`task/getResumeStatus` 从 SQLite 行借用任务 JSON，只解码状态、运行标志、注册身份和模型次数。消息、上下文和工具参数仍需扫描以跳过 JSON，但不再构造这些对象。真正恢复仍读完整任务，与只读查询共用同一判断。旧快照中省略的可选字段沿用原默认值，无数据库迁移。[任务恢复判断](protocol.md#任务恢复判断)

Windows 调试构建的本机 stdio 测量使用同一数据库的副本，先预热 25 对再交替执行 200 对只读请求，逐次核对返回相同。短输入产生 977 字节快照，中位数从 0.287 降到 0.244 ms；约 250 KiB 的带转义输入产生约 876 KiB 快照，中位数从 25.90 降到 15.01 ms，p95 从 34.14 降到 20.92 ms。测量包含进程间通信，不能直接外推到 release 构建。日志：`D:/Fliuno-Native-Test/resume-projection-benchmark.log`。

真实服务进程保存长输入、中断并重启；恢复查询不改变任务、事件或请求次数，只有明确同意后才再次请求模型。Debian Linux 6.12.107 来宾通过 12 项相关进程测试，另覆盖审批、外部工具超时和远端提交结果不明。日志：`D:/Fliuno-Native-Test/guest-resume-projection.log`。

Windows `scripts/check.ps1` 以退出码 0 完成：515 项测试、28 个离线演示、格式、Clippy 和协议 Schema 检查通过。日志：`D:/Fliuno-Native-Test/windows-resume-projection-check.log`。

## 0.0.3 审批续跑边界预检（2026-11-20）

`approval/resumeRun` 先通过 `pending_approvals`、过期时间和 `task_run_ends` 的边界事件完成只读核对，再构造审批执行所需的完整任务。普通 `approval/respond` 处理第一项工具后，旧父运行 ID 不能提交同一任务后续执行段的审批；跨重启仍保持原事件水位和模型请求次数。Windows 审批进程测试 7 项通过，包含该边界场景；核心 81 项单测和目标 crate Clippy 通过。

Debian Linux 6.12.107 来宾通过 7 项审批进程测试。Windows `scripts/check.ps1` 以退出码 0 完成：516 项测试、28 个离线演示、格式、Clippy 和协议 Schema 检查通过。日志：`D:/Fliuno-Native-Test/guest-approval-boundary.log`、`D:/Fliuno-Native-Test/windows-approval-boundary-check.log`。

## 0.0.3 恢复查询的 SQLite 字段提取（2026-11-20）

SQLite 直接返回 `task/getResumeStatus` 所需的字段：状态、后端、注册身份、模型尝试次数和操作是否存在。SQLite 仍需读任务 JSON，Rust 只解码这几个字段；`task/resume` 仍读取完整快照，沿用相同的恢复规则。旧快照中的省略字段和显式 `null` 用例通过，无数据库迁移。[任务恢复判断](protocol.md#任务恢复判断)

Windows 调试构建对比上一版 Serde 字段筛选实现。使用同一数据库的两个副本，预热 25 对后交替测量 200 对 stdio 请求，逐次核对结果相同。977 字节快照的中位耗时为 0.222 → 0.220 ms；896,921 字节快照为 18.50 → 2.03 ms，p95 为 23.10 → 3.21 ms。测量含进程间通信，使用本机离线模型产生的两个待审批快照。日志：`D:/Fliuno-Native-Test/resume-sql-final-benchmark.log`。

Debian Linux 6.12.107 来宾通过 12 项进程测试，覆盖带转义长输入跨重启恢复、尝试次数用尽、审批与外部结果不明。Windows `scripts/check.ps1` 以退出码 0 完成：516 项测试、28 个离线演示、格式、Clippy 和协议 Schema 检查通过。日志：`D:/Fliuno-Native-Test/guest-resume-sql.log`、`D:/Fliuno-Native-Test/windows-resume-sql-check.log`。

## 0.0.3 增量历史搜索（2026-11-20）

两种搜索都以 `afterSequence` 限定本次检查的最旧完成序号。SQLite 3.49.1 对等表结构的查询计划使用 `session_turns_cursor` 和 `workspace_turn_index` 的主键，同时应用完成序号上下界，无需临时排序。见[搜索范围](protocol.md#只搜索新完成的任务)。

真实进程测试保存一次没有命中的搜索页，重启后完成一项新任务，再恢复原查询。续页返回窗口内的命中，窗口外的新旧记录均不计入扫描数。测试还核对空窗口和倒置范围；Rust 客户端在缺少能力时不发送请求，拒绝越界响应后也保留原游标。Debian Linux 6.12.107 来宾通过 12 项相关进程测试。日志：`D:/Fliuno-Native-Test/guest-history-window.log`。

Windows `scripts/check.ps1` 以退出码 0 完成：518 项测试、28 个离线演示、格式、Clippy 和协议 Schema 检查通过。22 页 Markdown 的 485 处本地引用校验通过。日志：`D:/Fliuno-Native-Test/windows-history-window-check.log`。

## 0.0.3 会话历史订阅（2026-11-20）

`session/subscribeHistory` 在提交锁下读取历史首页，并从同一事件水位挂接会话订阅。增量下界随历史续页保留，数据库无需迁移。[订阅与续页](context.md#订阅会话历史)

进程测试在一帧 JSON-RPC 批处理中先建立订阅、再尝试无效切换、最后提交两个会话的取消操作。历史响应先于水位通知，通知只对应原订阅会话。另一项测试在首页后完成任务，实时读取完成事件，再重启续读冻结历史，并以旧水位打开增量窗口；新窗口返回相同的完成序号。Debian Linux 6.12.107 来宾通过 14 项相关进程测试。日志：`D:/Fliuno-Native-Test/guest-history-subscription.log`。

Windows `scripts/check.ps1` 以退出码 0 完成：520 项测试、28 个离线演示、格式、Clippy 和协议 Schema 检查通过。22 页 Markdown 的 491 处本地引用校验通过。日志：`D:/Fliuno-Native-Test/windows-history-subscription-check.log`。

## 0.0.3 交付物下载首块（2026-11-20）

`artifacts/openDownload` 返回摘要和前 64 KiB。单块正文的客户端测试核对首轮从 `artifacts/digest` 加 `artifacts/read` 两次 RPC 减为一次；旧服务仍发两次。首块范围无效时不调用摘要保存回调，保存失败也不写入正文。见[下载协议](protocol.md#一次取得摘要和首块)。

真实进程在首块后断开，使用此前保存的摘要和截到 UTF-8 字符中间的暂存文件重启续传，从暂存长度补读并校验完整正文。旧摘要补算与普通读取结果一致；错误任务、缺失产物和超摘要预算仍按原规则拒绝。Debian Linux 6.12.107 来宾通过 6 项相关进程测试。日志：`D:/Fliuno-Native-Test/guest-artifact-download-start.log`。

Windows `scripts/check.ps1` 以退出码 0 完成：522 项测试、28 个离线演示、格式、Clippy 和协议 Schema 检查通过。22 页 Markdown 的 494 处本地引用校验通过。日志：`D:/Fliuno-Native-Test/windows-artifact-download-start-check.log`。

## 0.0.3 历史分页按字节预算解码（2026-11-20）

历史页先按库中紧凑 JSON 的字节数加上响应封装计量。放不进整页的正文由 SQLite 跳过，能放进整页但超过本页剩余预算的记录留待下页，无需预先解码。真实进程用多字节字符和转义字符串验证单条读取和历史页在刚好够用及少一字节时的返回值与游标。协议和数据库版本不变。

Windows 调试构建使用相同数据库的副本，经本机 stdio 预热 25 对后交替测量 200 对请求，逐次核对响应一致。单条封装后的记录为 148,868 字节时，分页返回第一条并延后第二条的中位数为 24.32 → 11.28 ms，p95 为 32.20 → 15.37 ms；直读中位数为 14.64 → 10.57 ms。381 字节记录的分页中位数为 0.753 → 0.735 ms。测量包含进程间通信，仅代表该夹具和本机调试构建。日志：`D:/Fliuno-Native-Test/history-byte-budget-benchmark.log`。

Debian Linux 6.12.107 来宾通过 14 项相关进程测试。日志：`D:/Fliuno-Native-Test/guest-history-byte-budget.log`。

Windows `scripts/check.ps1` 以退出码 0 完成：522 项测试、28 个离线演示、格式、Clippy 和协议 Schema 检查通过。22 页 Markdown 的 496 处本地引用校验通过。日志：`D:/Fliuno-Native-Test/windows-history-byte-budget-check.log`。

## 0.0.3 可调上下文扫描范围（2026-11-20）

`fit_recent` 和 `match_recent` 可在 1 至 512 条之间设置扫描范围，默认 128 条。核心测试验证超预算及未命中的记录也占用额度，并检查扩大窗口后仍按完成顺序返回。进程测试检查参数边界、v46 数据库升级、显式默认值与旧回执的等价性，以及重启后复用同一键时保留原快照、变更范围时返回冲突。详见[上下文扫描范围](context.md#调整上下文扫描范围)。

Windows 调试构建通过 stdio 创建 129 份已完成记录，每份用户输入约 8 KiB。唯一匹配位于第 129 条，扩大窗口才选中它。另用同一水位比较默认窗口和 `scanLimit=1`，两者结果均为空并报告相同的省略数。预热后交替测量 100 对调用，中位数分别为 18.812 ms 和 0.764 ms。较小窗口的耗时收益来自减少扫描，与原窗口有不同的历史覆盖范围。原始日志见 `D:/Fliuno-Native-Test/context-scan-limit-benchmark.log`。

Debian Linux 6.12.107 来宾完成 14 项相关进程测试，退出码为 0。原始日志见 `D:/Fliuno-Native-Test/guest-context-scan-limit.log`。

Windows `scripts/check.ps1` 以退出码 0 完成。522 项测试、28 个离线演示、格式、Clippy 和协议 Schema 检查通过。22 页 Markdown 的 501 处本地引用校验通过。原始日志见 `target/context-scan-limit-windows-final.log`。

## 0.0.3 工作区批量 SHA-256（2026-11-20）

`workspace_sha256_batch` 通过一次普通工作区读取审批计算一组文件摘要。真实进程测试验证越界路径在审批前被拒绝、拒绝审批时没有工具结果、重启后审批仍对应原路径清单，以及允许后计算的是实际读取的字节。结果按批准的顺序返回，接受空文件和恰好 8 MiB 的文件，超限一字节、缺失、目录各自标记为不可用。进程测试还验证已完成结果在再次重启后保持一致。见[工具契约](protocol.md#内置工具与作用域)。

Windows `scripts/check.ps1` 退出码为 0，包含 523 项测试、28 个离线演示、格式、Clippy 和 Schema 检查。Debian Linux 6.12.107 来宾通过批量摘要和工作区搜索两项进程测试，覆盖真实 FIFO 和末级符号链接。日志见 `target/workspace-hash-batch-windows-check.log` 和 `D:/Fliuno-Native-Test/guest-workspace-hash-batch-final.log`。

## 0.0.3 上下文选取按字节预算解码（2026-11-20）

内核从 SQLite 借用候选正文。精简对话先按已存的紧凑 JSON 字节数判断是否能纳入，完整工具值继续按重新序列化后的大小计量。核心测试覆盖多字节字符、转义、刚好够用、少一字节、遗漏计数位数变化，以及工具浮点值解码后长度变化的情况。

Windows 调试构建在同一数据库的两个副本上经 stdio 读上下文。夹具含 18 条完成记录，32 KiB 预算保留最新一条和最早一条，中间 16 条无法放入剩余预算。预热后交替测量每个策略 100 对调用，逐次核对快照一致。`fit_recent` 的中位数从 29.410 ms 降至 4.988 ms，`match_recent` 从 37.223 ms 降至 15.977 ms。日志见 `D:/Fliuno-Native-Test/context-fit-sizing-benchmark.log`。

Windows `scripts/check.ps1` 以退出码 0 完成，包含 524 项测试、28 个离线演示、格式、Clippy 和 Schema 检查。Debian Linux 6.12.107 来宾通过 14 项相关进程测试。22 页 Markdown 的 504 处本地引用校验通过。日志见 `target/context-fit-sizing-windows-final.log` 和 `D:/Fliuno-Native-Test/guest-context-fit-sizing.log`。

## 0.0.3 AG-UI 内嵌图片输入（2026-11-20）

图片片段按 `@ag-ui/core@1.0.5` 的 `image` 和 `source.type: "data"` 接入，在内核中保留有序输入。真实进程测试覆盖 HTTP 会话授权、拒绝 URL 和文件句柄、Base64 与 MIME 校验、审批时关闭原 SSE 流、重启后用同一运行 ID 去重，以及替换图片后返回请求冲突。完成后的会话历史保留文字和 `[image]`，图片字节可从原始任务读取。OpenAI、Anthropic 和 Gemini 的回环请求分别核对图片字段和片段顺序。[图片接入](ag-ui.md#提交内嵌图片)

Windows `scripts/check.ps1` 退出码为 0，包含 526 项测试、28 个离线演示、格式、Clippy 和 Schema 检查。Debian Linux 6.12.107 来宾通过 9 项 AG-UI 进程测试，QEMU 与 `FLIUNO_RESULT` 均为 0。日志见 `D:/Fliuno-Native-Test/guest-ag-ui-image-input.log`。

## 0.0.3 图片请求重试（2026-11-20）

`task/startSummary` 在入参通过大小和数量检查后查找回执。回执匹配同一输入时直接返回当前摘要，省去 Base64 解码。首次提交、旧绑定和已占用请求键的不同输入仍检查图片数据。进程测试覆盖图片任务重启后的重试、格式错误和替换图片时的冲突。

Windows 调试构建在相同数据库的两个副本上测量同一条 stdio 请求。夹具使用 500,040 字节 Base64 编码的 PNG，预热后换序交替运行 100 对调用，逐次核对结果相同。中位数从 9.309 ms 降至 5.131 ms，100 对调用均更快。脚本和日志见 `D:/Fliuno-Native-Test/image-retry-benchmark.py` 与 `D:/Fliuno-Native-Test/image-retry-benchmark.log`。

Windows `scripts/check.ps1` 以退出码 0 完成，包含 527 项测试、28 个离线演示、格式、Clippy 和 Schema 检查。Debian Linux 6.12.107 来宾通过 8 项 AG-UI 和回执进程测试。QEMU 与 `FLIUNO_RESULT` 均为 0。日志见 `D:/Fliuno-Native-Test/guest-image-retry.log`。

## 0.0.3 AG-UI 内嵌 PDF（2026-11-20）

HTTP 进程测试提交混合文本、图片和 PDF，检查授权、MIME 不匹配时的拒绝、重启后的原始内容及请求去重，并核对会话历史只包含文字和占位符。三种模型接口的回环测试验证同一消息内的片段顺序和各自的 PDF 字段。[接入文档](ag-ui.md#提交内嵌-pdf)

Windows `scripts/check.ps1` 退出码为 0，包含 527 项测试、28 个离线演示、格式、Clippy 和 Schema 检查。Debian Linux 6.12.107 来宾通过 8 项 AG-UI 和回执进程测试，QEMU 与 `FLIUNO_RESULT` 均为 0。日志见 `D:/Fliuno-Native-Test/guest-ag-ui-pdf-input.log`。

## 0.0.3 任务输入提前校验片段数（2026-11-20）

应用协议解码时按 JSON 形状区分字符串和数组。读取数组片段数后会尽早验证 32 项上限。进程测试提交恰好 32 项并取得原始任务摘要，第 33 项返回 `-32602`。

Windows 调试构建通过 stdio 使用相同数据库副本，对每种输入预热后换序交替运行 100 对调用。50 万字节的有效图片请求重试逐次返回同一摘要，中位数从 4.866 ms 降至 4.365 ms。30,000 个短片段的超限数组逐次返回同一错误码，中位数从 206.147 ms 降至 138.426 ms。测量数据见 `D:/Fliuno-Native-Test/input-decode-benchmark.log`。

Windows `scripts/check.ps1` 退出码为 0，包含 527 项测试、29 个离线演示、格式、Clippy 和 Schema 检查。Debian Linux 6.12.107 来宾通过 8 项 AG-UI 和回执进程测试。QEMU 与 `FLIUNO_RESULT` 均为 0。日志见 `D:/Fliuno-Native-Test/guest-task-input-decode.log`。

## 0.0.3 AG-UI 历史消息内存（2026-11-20）

AG-UI 解析器逐条校验来信，只保留提交任务所需的最后一条消息。它仍会检查已丢弃的历史条目是否包含不支持的加密字段或工具调用。进程测试验证历史中的工具调用依然导致拒绝，本地已提交历史仍是模型的上下文来源。

Windows 用同一份 724,402 字节的请求分别运行两种解析器。请求包含 8 条带内嵌图片的历史记录和一条新文本。每个进程同时保留 128 个解析结果，测量 3 次的私有内存中位数从 90.3 MiB 降至 1.54 MiB。这是批量持有已解析请求的压力测试。测量见 `D:/Fliuno-Native-Test/ag-ui-retention-measurements.log`。

Windows `scripts/check.ps1` 退出码为 0，包含 527 项测试、29 个离线演示、格式、Clippy 和 Schema 检查。Debian Linux 6.12.107 来宾通过 9 项 AG-UI 进程测试。QEMU 与 `FLIUNO_RESULT` 均为 0。日志见 `D:/Fliuno-Native-Test/guest-ag-ui-retention.log`。

## 0.0.3 交付物作为任务输入（2026-11-20）

本机 `task/start` 和 `task/startSummary` 可以引用本会话已经提交的图片或 PDF。进程测试通过一次真实审批创建二进制交付物，随后检查回环模型请求的内容和顺序、跨会话拒绝、输入总预算，以及进程重启后的请求去重。[协议约定](protocol.md#将交付物用作任务输入)

Windows 调试构建通过 stdio 交替执行 100 对摘要重试，逐次核对摘要一致，同时检查两种方式保存了相同的输入内容。引用同一个已保存的 PNG 时，JSON 参数从 318,233 字节降至 260 字节，重试中位数从 3.801 ms 降至 0.415 ms。脚本和日志见 `D:/Fliuno-Native-Test/artifact-input-benchmark.py` 与 `D:/Fliuno-Native-Test/artifact-input-benchmark.log`。

Windows `scripts/check.ps1` 退出码为 0，包含 528 项测试、29 个离线演示、格式、Clippy 和 Schema 检查。Debian Linux 6.12.107 来宾通过 6 项交付物、AG-UI 和回执进程测试，QEMU 与 `FLIUNO_RESULT` 均为 0。日志见 `D:/Fliuno-Native-Test/guest-artifact-input.log`。
