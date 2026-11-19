# 原生进程隔离边界

当前实现与本机验证日期：2026-11-18。`fliuno-sandbox` 不依赖任务内核、模型或外部 Agent 协议；`fliuno-mcp/launch.rs` 和 `fliuno-acp/transport.rs` 分别接到官方 SDK 的 stdio 传输。MCP 主机配置必须明确选择 `trusted_host`、`windows_app_container` 或 `linux_landlock`；ACP 的 `connect_isolated` 使用当前平台后端，通用 transport 由调用方管理权限和生命周期。隔离启动不存在失败后转宿主运行的路径。

## Windows 私有目录模式

每次启动创建随机 UUID 命名的 AppContainer profile，不复用身份。将主机指定的单个原生 `.exe` 通过新建文件写入私有 `LocalState/fliuno`，不使用会携带源文件 ACL 的 Windows 文件复制操作。工作目录、用户目录与临时目录环境变量指向此私有目录。没有宿主目录授权、网络能力或 loopback exemption；不改宿主工作区或祖先目录的 ACL。

回收 profile 时，`DeleteAppContainerProfile` 若返回文件共享冲突，则最多追加四次尝试，等待合计 300 ms。持续占用或其他错误仍写入诊断。原生测试持有一个禁止删除的文件句柄，验证释放后私有目录会被清除。

此模式支持自包含原生工具，以及通过固定摘要 `assets` 清单明确暂存依赖的解释器；Windows 已验收真实 Node/CPython 的多文件 MCP。清单不开放宿主目录；工作区访问和网络代理仍需独立接口，见[伴随文件边界](runtime-bundles.md)。Windows 的已有共享资源 ACL（例如授予所有 AppContainer 的系统文件）仍然生效，不能将此后端描述为严格的路径白名单、文件系统命名空间或虚拟机。它也没有阻止在私有目录写入并执行程序。

## Linux Landlock 模式

主机配置选择 `linux_landlock` 后，启动器将单个程序复制到新建的 0700 私有目录，清空继承环境，并将 `HOME`、`TMPDIR`、`TEMP` 指向该目录。`PATH` 固定为 `/usr/bin:/bin`，额外环境只来自主机明确配置的 allowlist。工作目录设为私有目录；当前不授予 workspace 或其他宿主数据路径。

规则集硬性要求 Landlock ABI 6：文件系统访问控制、TCP bind/connect 控制，以及抽象 Unix socket 和信号作用域。允许私有目录内的文件操作；系统运行所需的 `/usr/lib`、`/lib`、`/lib64`、`/usr/bin`、`/bin`、`/usr/sbin`、`/sbin`（存在时）只开放读取/枚举/执行，另允许读取 `/etc/ld.so.cache`。其余路径默认拒绝。seccomp 在 exec 前安装，并拒绝 socket/socketpair、连接与消息调用及 `io_uring`，以覆盖 crate 尚未表达的 UDP 和其他 socket 类型；syscall ABI 不匹配时直接终止子进程。Landlock、`no_new_privs`、seccomp 或 `close_range(CLOEXEC)` 任一环节失败，启动都会失败。

这是按当前系统目录布局提供运行依赖的最小 scratch 模式，不是 mount/user namespace 或虚拟机。系统运行目录只读但仍可被枚举和执行；workspace 文件代理与 GPU/设备代理尚未提供；依赖伴随文件可由 `assets` 明确暂存，Linux 真实解释器行为仍待 runner 验收。Linux 启动器会探测 unified cgroup v2：有可写委派时，在 exec 前将子进程放入专属 cgroup，并用 `cgroup.kill` 回收整棵子树，包括已调用 `setsid` 的后代；没有委派时使用进程组回收并公开 `Child::uses_cgroup_v2()` 供宿主诊断。

可选 `resources` 使用 `memoryMaxBytes`（16 MiB..1 TiB）、`pidsMax`（1..4096）、`cpuQuotaMicros`（Linux 上 1,000..100,000,000 微秒）配置内存、进程数与 CPU 上限。Linux 分别写入并回读 `memory.max`、`pids.max`、`cpu.max`；CPU quota 的周期固定为 100 ms。资源预算要求 cgroup v2 父目录已委派相应控制器；宿主通过 `linuxCgroupParent` 指定一个绝对路径、可写、类型为 domain 且 `cgroup.subtree_control` 已启用所需控制器的目录。未提供该项时尝试使用当前进程的 cgroup；无法写入全部所请求的限制时启动失败，不降级到无预算运行。Windows 使用 Job Object，`pidsMax` 覆盖默认 16 个活动进程，`memoryMaxBytes` 覆盖默认 512 MiB Job commit 上限；Windows 不接受 CPU quota。Linux 的 `memory.max` 与 Windows Job commit 上限采用各自内核的内存记账语义，并非跨平台相同的 RSS 指标。

每个接受的 cgroup 在 workload 启动前创建独立 guardian，运行在该 workload 的 cgroup 外。普通 sandbox 通过 pidfd 绑定宿主进程整体：创建它的线程退出不结束任务，宿主进程退出或所有权关闭则触发 `cgroup.kill`。持久 service 在 exec 前以 SCM_RIGHTS 传递自己的 pidfd，收到确认才继续 exec；交接后 guardian 跟随 worker，app-server 退出不终止终端。guardian 不依赖 PID 数字、app-server reaper 或 PDEATHSIG。创建/交接失败拒绝 exec，不静默保留只有 cgroup 而没有监控的树。

Guardian 在 fork 前预开 cgroup kill/events 句柄，屏蔽生成线程的信号；子分支只使用预构造数据、栈缓冲和系统调用，关闭除四个必要句柄外的全部描述符并脱离原会话，重置继承的 signal handlers 后再解除屏蔽，不保留 RPC 管道、数据库锁或日志句柄。监控目标结束后持续尝试 kill、确认 `populated 0` 并删除空 cgroup，清理预算约 5 秒。无法确认时保留目录，活着的宿主会收到 guardian 失败诊断。主机不再存活时不能承诺日志投递；离线 terminal 快照仍是未知结果，不从回收推断命令成功。

此边界依赖 guardian 本身和受信任的委派目录仍可工作，不覆盖整机故障、外部强杀 guardian 或宿主撤销委派。宿主硬崩溃后的 sandbox scratch 目录当前不会自动删除。Linux guardian 的真实硬杀/重连/后代回收探针已接入 Ubuntu CI；当前 Windows 开发机只验证交叉编译，运行结果仍待 runner 确认。

Windows 启动流程：

1. 校验可执行文件、参数与显式环境；可执行文件最大 128 MiB，参数最多 256 项、合计 32,000 UTF-8 字节，环境最多 256 KiB；环境名称限制 ASCII 并拒绝大小写重复。
2. 创建私有 profile、暂存原生程序与逐文件固定摘要的清单资源。没有来自父进程的隐式环境，`SystemRoot` 是运行系统组件所需的例外；显式配置的凭据仍然会传给服务器。
3. 创建带低完整性标签的管道；宿主中全部端点均不可继承。创建一个始终挂起的当前 executable 副本，将三个子端以可继承形式复制到其句柄表，通过 `PARENT_PROCESS` 与 `HANDLE_LIST` 只传递这三个句柄。宿主中其他库并发执行 `std::Command` 也不能带走这些管道。
4. 预先配置 workload 的 `KILL_ON_JOB_CLOSE` Job，默认限制 16 个活动进程和 512 MiB Job commit；`resources.pidsMax` 和 `resources.memoryMaxBytes` 可分别覆盖。通过 `JOB_LIST` 随隔离进程创建原子分配 Job，并同时指定零能力 `SECURITY_CAPABILITIES`。在初始线程仍挂起时核验其位于 workload Job、位于中转 Job 外，终止中转进程后才恢复初始线程。准备失败终止暂停进程。
5. 传输持有进程；MCP 关闭、超时、取消、初始化失败或所有者 Drop 都触发进程树回收。先终止 Job，再关闭/刷新管道，避免服务器不读取 stdin 导致关闭挂起；stderr 持续丢弃，不记录服务器可能输出的凭据。
6. 进程退出后删除 profile 与私有目录。清理 API 失败会在 stderr 报告，当前尚无持久化的孤立 profile 清理队列。

`async-io` feature 提供 `spawn_async`，供 exec、MCP、ACP 共用。隔离启动在阻塞 worker 中准备，交回进程所有权后将父端注册到调用方的 IO runtime。Windows 使用带相同 SID/低完整性 ACL 的私有单实例 named pipe，拒绝远程客户端；只有父端是 overlapped handle，子端仍按同步 stdio 继承。Linux 将匿名 pipe 通过 Tokio `unix::pipe` 设为非阻塞。同步 `spawn` 和独立服务 bootstrap 不改用 overlapped handle，因此现有同步调用方不会对其误用 `std::fs::File` IO。

异步管道不会在 Tokio 文件线程池保留阻塞读写。Windows 独立 writer 补足默认 named-pipe flush 不等待 pending WriteFile 完成的差异，确认完成或错误后才 shutdown，并限制每笔缓冲 64 KiB。MCP/ACP stderr 由可 abort 的异步任务丢弃；结束时不在协议线程 join 阻塞读端。字段语义与原生取消探针见 [执行证据](execution-evidence.md)。

Windows 的 exec、MCP 和 ACP 可信启动复用 `spawn_host_async(HostLaunchSpec)`，使用绝对可执行路径、绝对 cwd 和显式 Unicode 环境。宿主进程保留当前用户权限，使用独立 Job 管理生命周期。MCP 和 ACP 可显式配置 `resources.pidsMax` 和 `resources.memoryMaxBytes`。Windows 要求配置 `.exe`，运行脚本时由宿主选定解释器。

Linux 的可信 MCP 和 ACP 使用同一 API。启动时加入独立进程组，并在委派可用时接入 cgroup v2 与 guardian。配置了 `linuxCgroupParent` 或资源上限时，无法应用配置就拒绝启动。可信 MCP 继续使用配置的宿主 cwd。Linux 可信 exec 目前使用 Tokio 进程与进程组。共享管道探针覆盖大块往返、取消、已退出 reader 和写入错误。

句柄中转进程没有执行自己的入口、加载器线程或 Rust 初始化代码；启动时即以 `JOB_LIST` 放入独立的 kill-on-close Job，宿主硬崩溃也会回收它。中转 Job 的 silent breakaway 允许真实子进程离开这个临时 Job，外层禁止脱离的 Job 约束仍适用；恢复线程前另用 `IsProcessInJob` 验证实际归属。持久 service bootstrap 复用句柄中转，但实际 service 的 Job 仍由 worker 自己持有，以支持 app-server 重启。此启动路径要求支持 `PROC_THREAD_ATTRIBUTE_JOB_LIST` 的 Windows 10 / Server 2016 或更新系统，不支持时拒绝启动。

中转方式会增加一次原生进程创建和回收，均计入 `launchMs`，不改变执行器的总启动预算。中转 native cleanup 也有最多 5 秒的同步等待，因此异步清理预算不能作为严格的调用返回上限。宿主崩溃后的 AppContainer profile/scratch 仍可能残留，独立清理队列尚未实现。

## 测试证据

Windows/Linux CI 都运行 `cargo test --workspace --all-features --locked`，平台相关探针在对应 runner 上执行：

| 验证 | 证据 |
| --- | --- |
| 文件与 TCP | 原生对照先证明可读/写宿主临时私有文件和连接本机监听端口；AppContainer 中相同请求失败，外部文件保持原内容；私有文件写入成功 |
| 令牌 | 子进程直接查询 `TokenIsAppContainer`，不是依据环境或命令行判断 |
| 环境 | 原生父进程明确注入测试 secret，然后从该父进程启动隔离子进程；子进程看不到该变量 |
| 管道和参数 | UTF-8/中文、空字符串、引号、反斜杠与尾反斜杠通过真实进程往返 |
| 异步管道取消 | reader 保持存活且不消费输入，父端读写挂起后丢弃端点；runtime 在有界时间内退出，之后再回收子进程。另验证完整输入/EOF、已退出 reader 和 Windows pending-write 失败 |
| Windows 并发句柄继承 | 保持中转句柄存活时从同一宿主启动无关 `std::Command`；释放原端点和中转后双输出须在 2 秒内 EOF。硬杀创建者后中转退出；独立 fixture 另验证实际 AppContainer 保留宿主外层禁止 breakaway 的 Job |
| 子孙进程 | 根进程再创建一个持有 stdout 的子进程；Drop 后管道 EOF 在有界时间内到达，私有目录已删除 |
| MCP | 官方 rmcp 测试服务器经真实 stdio 完成发现/调用；相同文件/TCP 探针拒绝；超时关闭主机并清理 profile，重复调用被拒绝 |
| ACP | 官方 ACP SDK Agent 经真实 stdio 创建会话、提示和更新；宿主对照成功，容器中同样的文件/TCP 请求被拒绝，私有写入成功；提示超时和最后客户端 Drop 清理 profile |
| 不支持的平台 | 请求隔离启动会返回 Unsupported，不运行宿主后备进程 |
| Linux 行为 | 原生对照成功；Landlock/seccomp 子进程不能读写宿主临时文件或连接 loopback TCP，可写私有 cwd，且报告 seccomp 与 no_new_privs 已生效 |
| Linux delegated cgroup | CI 创建启用 cpu/memory/pids 的临时委派目录；要求 sandbox 进入子 cgroup、证明 `setsid` 后代回收，并用 `pids.max=1` 验证子进程创建被内核拒绝；无委派的普通测试显式走进程组回收路径 |

Linux 集成测试覆盖同样的原生对照与隔离探针：宿主临时文件读写和 loopback TCP 先在普通进程中成功，再在 Landlock/seccomp 子进程中失败；私有 cwd 写入成功、环境 secret 不继承、外部文件内容不变。Ubuntu CI 另用 sudo 配置可写的临时委派 cgroup v2，强制验证 `setsid` 后代回收；本地开发机是否有委派不影响普通测试。Windows 本机探针单独验证 AppContainer token 和 Job 回收。当前开发主机为 Windows；Linux 内核行为需由 Ubuntu CI 执行确认。

文件/TCP 探针不等于穷尽 Windows 的所有 IPC、网络协议和内核攻击面。Linux Landlock/seccomp 和 delegated cgroup 探针已加入 CI；当前尚未在本机 Linux 内核执行，实际通过情况以 Ubuntu runner 为准。macOS 后端与任意敌对二进制认证均未实现。

## unsafe 审查范围

其他 Fliuno crate 继承 workspace 的 `unsafe_code = forbid`。该 crate 默认 `deny`，仅 `windows` 和 `linux` 模块允许平台 FFI，并启用 `clippy::undocumented_unsafe_blocks = deny`：

- `windows/profile.rs`：AppContainer profile/SID 分配与释放、API 返回字符串，SID 复制到 DWORD 对齐的 Rust 所有权缓冲区。
- `windows/stdio.rs`：SDDL 与管道创建；所有成功返回的句柄立即交给 `OwnedHandle`，安全描述符由 LocalFree 释放。
- `windows/async_io.rs`：仅将内部 `FILE_FLAG_OVERLAPPED` 父端句柄一次性转移给 Tokio；其他模块不能传入任意同步句柄。
- `windows/inheritance.rs`：挂起中转进程、原子 Job 分配与远端句柄复制；远端句柄由中转句柄表关闭，不能在宿主用 `CloseHandle` 误关相同数值的其他句柄。
- `windows/job.rs`：原生 workload Job 的预先创建、资源预算及 kill-on-close 配置。
- `windows/launch.rs`：共用的原生创建边界；编码参数/环境、借用 AppContainer/Job、选择中转句柄表、恢复前验证归属。所有属性值存活至属性列表释放。
- `windows/process.rs`：共享 Job 所有权、根进程等待与终止；隔离模式在此 owner 回收之后才释放 profile。
- `windows/host.rs`：受信任宿主策略，只选择显式 cwd/环境、未设资源预算的 Job 和 reactor 管道。
- `windows/mod.rs`：对齐的属性列表存储、隔离策略与令牌查询；只有初始化成功的属性列表才会进入 Drop。
- `linux.rs`：Landlock 规则集在父进程构造；`pre_exec` 只执行 close-range、cgroup/guardian 交接、prctl 与 Landlock syscalls，seccomp BPF 在 fork 前预先构造；进程组信号使用仍由 waitid 保留的根 PID。
- `linux/guardian.rs`：fork 期间屏蔽信号，子分支不调用分配器、日志或析构；pidfd/SCM_RIGHTS 固定身份，close_range 关闭继承句柄；poll、预开 kill/events 文件、rmdir 与 `_exit` 完成独立回收。

后续应在保持这一边界的前提下增加 macOS 后端与资源代理，不把 OS 细节放入任务状态机。

## 持久终端

PTY 是由 `fliuno-terminal` 独立实现的生命周期服务，不借用 `fliuno-exec` 的前台子进程，也不自动授予 ACP Agent 终端能力。主机为每个 preset 固定绝对 executable、argv 前缀、环境 allowlist、timeout 和输出字节上限；terminal create 工具审批后使用当前 workspace root 作为 cwd。所有 PTY 输入和生命周期修改分别审批。该模块的协议、错误和资源上限见 [应用协议中的持久终端](protocol.md#跨轮次终端)。

每个 terminal 对应独立 OS supervisor，输入通过绝对路径的受限私有目录启动，不把 launch JSON 放入命令行。启动先原子保留 SQLite spool 和 session/preset 身份；worker 只能打开内容完全相同且仍处于 Starting 的预留快照，不能接管或复用旧记录。Supervisor 以随机本地 endpoint 和 capability token 对 IPC 请求鉴权；输出 spool 只允许该独立 worker 写，包含字节、游标、运行状态、有限操作 ID 和结果，不保存秘密环境值或 child stderr。状态目录不能与非托管用户数据混用。进程和 endpoint descriptor 不是认证凭据的替代品。

Windows supervisor 启动时先把自己放入 `KILL_ON_JOB_CLOSE` Job，再创建 PTY 子程序，故 supervisor hard crash 也会关闭 Job 并回收后代。Job 默认最多 32 个进程、1 GiB Job memory；preset 的 `resources.pidsMax` 与 `resources.memoryMaxBytes` 可分别覆盖对应上限。PTY 使用 ConPTY，需要持续排空独立输出管道并答复启动时一次性的光标继承查询；ClosePseudoConsole 放到独立线程，actor 持续排空收尾输出。PTY native read、write 线程使用有界队列，因此客户端控制连接不会与 PTY 写入互相等待。

Linux terminal worker 若当前 cgroup 或 preset 指定的 `linuxCgroupParent` 可写委派，会在 exec 前加入独立 cgroup；PTY 后代自动继承。独立 guardian 通过交接的 worker pidfd 等待退出并使用 `cgroup.kill` 清理，包括调用 `setsid` 的后代；app-server reaper 仍负责正常父子进程回收。`resources` 可为该树设置 memory、pids 和 CPU 上限；预算无法在 delegated cgroup 中完整应用时拒绝启动。没有可用委派且未请求预算时，保留 Unix process group 的尽力清理，不将其描述为等价保证。macOS 仍仅有进程组行为。app-server hard crash 后已交接的 worker 继续运行；worker 随后退出或硬崩溃时 guardian 仍可清理其 cgroup。Linux launcher 要求 `close_range` 可用，有 cgroup 时另要求 pidfd 与 guardian 启动成功。

状态根每服务最多 64 个物理 terminal record、8 个活动/未知记录。显式 archive 在取得 supervisor 所有权锁后，先向独立 SQLite 账本提交 tombstone，再回收输出与 endpoint；主机预留和 worker 创建 PTY 前均检查账本，延迟交付的旧 bootstrap 也不能绕过归档。初始化标记会阻止缺失或重置的账本被当作无历史状态；完整规则见 [终端归档](terminal-archive.md)。归档仅回收记录，不证明平台未能确认的子孙进程已经结束。PTY child startup 前可报告明确失败；一旦 native spawn 尝试开始则采用保守的未知结果状态。Windows CI 真实验证 kill、自然 root exit、硬杀 supervisor 后 job descendant 回收和 spool 恢复；Unix 目标仍待 CI 行为测试，不将 Windows 证据外推为跨平台保证。
