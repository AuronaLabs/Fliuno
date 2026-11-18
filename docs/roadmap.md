# 开发计划与验收

## 0.0.2：内核增量开发中

- [x] A2A 1.0 JSON-RPC SSE 实时流：有界解析、背压、任务身份绑定、增量状态/产物、取消与断流未知结果；Agent Card capability、HTTP/进程契约测试、流式产物组装及真实 app-server 离线成功/取消演示均已覆盖。
- [x] A2A 远程状态核对：显式查询已绑定身份，结果追加到持久事件，真实 app-server 验证重启回放、冲突与超时。
- [x] A2A 远程多轮继续：补答暂停任务或在原上下文发起新一轮；每轮独立持久化、后台复核身份与状态、发送边界可审计，重启不重发。
- [x] OpenAI/Anthropic 可选本机预算预检：整份请求按 JSON 字节估算，扣除输出预留，超预算在发送前拒绝。
- [x] 最近历史改为逐条精确计量，达到预算时停止读取；Windows 调试构建同库 stdio RPC 对照，40 次/版本，32 条历史中位数 95.6→13.3 ms，16 KiB 预算保留 4 条时 4.63→1.97 ms，返回一致。
- [x] OpenAI/Anthropic 可选供应商输入计数：复用生成输入，限定计数等待和响应大小；错误、取消及超预算不发送生成请求。
- [ ] 模型窗口探测与进一步的上下文选择策略。
- [x] 会话历史可选 `fit_recent` 策略，在最多 128 条近期记录内跳过不适合剩余预算的记录，冻结快照随原幂等键跨重启复用。
- [x] 初始化握手返回已配置的模型预算和计数模式，调用端可根据它显示当前约束。
- [x] 产物清单分页和单份正文读取。清单页只传元数据，可跨重启沿用游标。双产物进程验收中，单条清单页 JSON 小于 512 字节，旧接口的完整列表超过 64 KiB。
- [x] SQLite v12 为工作区会话分页、会话任务分页和启动恢复增加索引。Windows 上用 SQLite 3.49.1 的内存库测试，10 万条任务、1 万条会话，每项 100 次查询的中位数分别为 0.252→0.021、4.700→0.022 和 8.517→0.003 ms，返回一致。此测量覆盖 SQL 查询时间。
- [ ] 跨协议事件一致性、故障注入与热点路径基准，按测量结果优化 Rust 内存与分配。

## 0.1：本机内核基础

- [x] Rust workspace 与协议/内核/模型/MCP/服务模块边界。
- [x] 工作区、会话、任务、审批、产物、持久化事件。
- [x] 事务产物与审批闭环、重启恢复、单库单进程锁。
- [x] 模型接口、离线 demo、OpenAI Responses / Anthropic Messages 适配器。
- [x] OpenAI / Anthropic SSE 增量文本预览；易失进度通知与持久任务事件分离。
- [x] MCP stdio 工具发现与调用、参数校验、未知副作用阻止重放。
- [x] MCP Streamable HTTP，包含 JSON/SSE、SEP-2243 标准头、现代 discover / 旧版初始化矩阵、凭据隔离和未知结果恢复边界。
- [x] 内置工作区文件能力、仅创建新文件的审批写入。
- [x] Skills 目录发现、frontmatter 校验、渐进读取。
- [x] 工作区/会话/任务分页查询与任务提交幂等键。
- [x] 模型请求结果不确定时的显式恢复选择。
- [x] 模型提供方区分本地拒绝、发送前取消与可能已发送的未知结果；只有后者要求显式重试。
- [x] OpenAI/Anthropic 请求体在网络发送前使用 8 MiB 有界序列化；超限按已知未发送处理，不将字节预算当作 token 窗口。
- [x] 子进程端到端测试、HTTP 模型契约测试、MCP 协议测试。
- [x] 工程质量检查、CI 配置、离线 demo 与使用文档。
- [x] 五类宿主 JSON 配置统一有界读取，全部反序列化先于数据库/进程/连接初始化，固定诊断及真实 CLI 拒绝测试。
- [x] core 统一工具组注册、名称/摘要/重复预检、整组发布与失败丢弃；五类工具适配器接入，待批调用和原注册不受失败影响。

验收重点：干净数据库启动；审批前无产物；允许一次仅产生一次效果；重启不重执行；错误输入不会崩溃；测试不需要真实凭据或网络服务。

## 后续内核能力

1. [x] 有界后台模型与工具调度、请求期间的查询与取消、provider HTTP 请求中断、SQLite 事务后的实时事件通知、全局事件回放与溢出恢复、MCP 调用超时与进程树回收、固定 worker 并发预算与协议能力声明。
2. [ ] 编辑与文件变更审阅、平台执行器与真实沙箱。
   - [x] 有基线哈希校验的单文件补丁；审批保存 unified diff，旧基线拒绝写入。
   - [x] 工作区文件有界读取、超长/取消与 Unix FIFO 拒绝；显式 base64 原始字节读取、原始哈希/长度和持久审批。
   - [x] 独立文件读取模块：普通文件校验、分块字节/取消预算、读取/补丁/预览复用，以及 Unix FIFO 非阻塞打开；真实 FIFO 限时探针待 Linux runner 运行。
   - [x] 主机固定注册的前台命令执行、逐调用审批、有界 stdin/stdout/stderr、取消/超时证据和进程树回收。
   - [x] 定位原生命令执行验收的偶发 unknown 结果：原生启动证据复现 AppContainer 启动耗时 8.35/8.95 秒超过成功 fixture 的 5 秒预算；修正测试预算，保留独立超时/取消探针，stdin shutdown 错误、跳过/中止写入、首次停止原因与重启证据均有单独报告和验收，见 execution-evidence.md。
   - [x] 隔离执行器的共享 reactor 管道：Windows overlapped IO / Linux nonblocking pipe、输入完成错误、EOF 与取消收尾；exec/MCP/ACP 复用，原生对端保持存活的 runtime 退出探针。Linux 运行验收待 runner。
   - [x] Windows TrustedHost 命令/MCP 接入同一 native launcher、创建时 Job 和 reactor；启动策略、句柄继承、原生创建、进程所有权独立分模块，无隐式 scratch 资源上限；同一组真实 pending IO 探针覆盖 host 与隔离两种模式。
   - [x] 修复真实并发下的跨调用句柄继承：宿主仅持不可继承端点、挂起中转选择句柄表、Job 原子归属；无关 std::Command 存活时 EOF 和创建者硬杀回收探针，不扩张清理预算。
   - [x] 独立 Windows AppContainer 原生执行器；MCP 私有目录模式；文件/TCP/环境/子孙进程探针及真实 MCP 超时回收测试。
   - [x] Windows AppContainer 与 Linux Landlock/seccomp MCP 子进程边界；以宿主对照和隔离探针验证文件/TCP 拒绝，策略缺失时强制失败。
   - [x] Windows 隔离 ACP 的宿主文件代理：虚拟路径、逐操作审批、写入 diff/基线核验、按行读取和真实进程探针。
   - [x] exec/MCP/ACP 的伴随文件清单、SHA-256 复制核验、路径/容量边界、资源变更后的待批身份回归；Windows 真实 Node/CPython 多文件 MCP 允许/拒绝/篡改验收。
   - [ ] MCP 宿主工具代理；Linux runner 实际执行伴随文件与真实解释器探针；其他 Python 版本/任意依赖的打包。
   - [x] MCP/Agent 全配置注册摘要：端点/程序/参数/实际环境/凭据/schema 与旧待批/排队任务绑定，v8 不替旧记录补造授权；隔离启动核验实际复制的 executable pin。
   - [x] 跨轮次 PTY preset、独立 supervisor/actor、逐操作审批、字节 spool、幂等输入、app-server 重启观察、Windows Job 预算与硬崩溃进程树回收。
   - [x] 终端输出事件订阅：认证 IPC 长连接、字节游标回放、状态通知、取消、队列溢出与公平调度、离线收尾和 app-server 重连。
   - [ ] supervisor 迁移/交接协议、应用 UI/终端 emulator adapter；受信任主机目录本身的本机 ACL/威胁模型设计。
   - [x] Linux delegated cgroup v2 子树与 `cgroup.kill`；宿主可配置 cgroup parent 与 CPU/内存/PID 硬预算，CI 探针验证进程上限。
   - [x] Linux terminal worker cgroup 所有权与资源预算配置透传；Ubuntu delegated-cgroup 探针覆盖 setsid 后代和预算值，runner 验证待执行。
   - [x] 独立 Linux cgroup guardian：sandbox 绑定宿主 pidfd、持久 terminal 绑定 worker pidfd，exec 前交接与失败拒绝；不依赖创建线程或 app-server reaper。
   - [ ] Ubuntu runner 实际执行 guardian 硬杀/重连探针：创建线程退出、宿主 SIGKILL、app-server 退出后终端继续工作、supervisor SIGKILL 后 setsid 后代与 cgroup 回收。
   - [ ] macOS 隔离后端及 hard-crash containment；任何不支持能力必须显式拒绝或报告。
   - [x] 终端显式归档与持久 tombstone；监督进程所有权锁、延迟 worker 启动屏障、输出回收、分页历史与有界公平补清理，保留原身份阻止重放。
   - [x] 受信客户端有界批量归档：全部 ID 预检、最多 32 项、逐项已知/不确定结果、独立提交与重启重试，真实 SQLite 锁和原生 PTY 契约。
   - [ ] 终端保留策略配置和主机运维恢复；按年龄自动归档仍未开放。
3. [ ] Skills 资源/脚本的受控执行；跨任务会话上下文与压缩策略。
   - [x] Skills 资源的按需目录/UTF-8 文本读取、单技能目录能力、无链接解析、内容摘要/预算、目录来源与旧审批绑定，以及允许/拒绝/切换来源的真实 app-server 离线演示。
   - [x] Skills 二进制资源的显式 base64 读取、原始字节预算/摘要、审批和重启保存，不自动解码、执行或创建产物。
   - [x] 已完成任务的中性历史投影、创建时冻结、幂等/恢复不变、完整任务数量/字节预算、省略计数与 v5 迁移。
   - [x] 模型/ACP 共享历史，跨供应商新任务转换；真实 HTTP、app-server 重启与 Windows 隔离进程验收。
   - [x] 显式 taskIds 选择同会话已完成任务、完成顺序、ID 集合规范化幂等、完整快照超预算整体拒绝及 v9 兼容迁移。
   - [ ] 模型摘要压缩、token/窗口预算、检索选择、二进制资源的类型化处理与 Skills 脚本执行边界。
4. [ ] ACP 客户端适配与协议一致性测试。
   - [x] 独立官方 SDK v1 客户端、连接内会话/权限/取消与真实隔离 stdio 契约测试。
   - [x] 独立 Agent 执行接口、持久运行/多项审批、app-server 配置与真实隔离进程闭环。
   - [x] v1 客户端文本文件 API 的受控工作区代理、持久宿主操作与重启失效测试。
   - [ ] 认证、会话恢复、受控网络/终端与真实 Agent 互操作；跟踪 v2 草案中以 MCP 提供宿主工具的变化。
5. [ ] A2A 与后续事件通道。
   - [x] 独立 A2A 1.0 JSON-RPC 客户端、主机固定端点/凭据、可选同源卡片核验、一次提交/轮询/取消、远程状态与最终产物入库、重启不重放及真实 app-server 契约测试。
   - [ ] 官方 SDK/真实 Agent 互操作、远程任务核对与多轮继续、认证/扩展/tenancy；随后考虑 AG-UI 事件投影。
6. [x] MCP Streamable HTTP 工具通道、JSON/SSE、显式生命周期及现代/旧版官方 SDK 互操作矩阵。
   - [x] MCP 资源/模板/提示按能力注册、逐次审批的单页列举/读取/获取、原始内容封装、注册绑定和 stdio/HTTP 官方 SDK 契约。
   - [ ] OAuth 发现/授权/刷新、资源变更订阅/补全、server-to-client 交互和 MRTR/Tasks 持久继续。
   - [ ] 旧版独立 HTTP+SSE transport、真实外部服务验收及断线后的受控恢复。
7. Tauri 仅作为客户端接入，复用相同协议和内核测试。

## 不混淆的测试层次

- 单元测试：状态迁移、审批绑定、预算、输入校验、供应商响应解析。
- 集成测试：真实 SQLite、重启和锁；本机 HTTP fixture 验证请求与工具回传。
- 进程测试：启动真正的 app-server，通过 stdio 完成任务和恢复。
- 协议测试：MCP stdio 与 Streamable HTTP discover/initialize、JSON/SSE、标准头、工具调用和拒绝无效参数；ACP v1 会话、通知、权限、取消及输入边界；A2A 1.0 JSON-RPC 关联、Part/状态/身份、卡片固定接口、取消证据与未知结果恢复。
- 隔离测试：先运行无隔离的成功对照，再检查真实子进程的令牌/过滤器、文件/TCP 访问拒绝、空白环境、管道/参数和平台回收。Windows 与 Linux 都有真实行为探针；当前 Windows 主机不能本机执行 Linux 探针。
- CI：Windows 和 Linux 编译、rustfmt、Clippy、cargo test --all-features --locked，包含显式测试服务器。

真实模型测试必须显式开启，不能在普通测试或 CI 中隐式产生付费请求。尚未用真实模型凭据验收的部分应在交付记录中明确说明。
