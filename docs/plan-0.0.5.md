# 0.0.5 迭代计划与 0.0.6 TUI 起步

[文档中心](README.md) › [开发路线图](roadmap.md) · [TUI 首版](#006第一版-tui) · [调研依据](#调研依据)

状态：**开发中，已接入本机备份管理、A2A 身份配置和任务混合记忆检索**。调研核对日期：2026-11-21。

0.0.5 补齐首版 TUI 的内核依赖：真实 Agent 接入、可解释的记忆与预算、隔离脚本执行、可操作的备份恢复。继续使用 Rust、SQLite 和本机应用协议。0.0.6 开始做 TUI；GUI 和原生 macOS 沙箱另行排期。

| 方向 | 0.0.5 范围 | 后续评估 |
| --- | --- | --- |
| ACP | v1 认证、隔离会话续接、受限终端、真实对端 | v2 Preview 独立试验；服务端、通用二进制文件代理与附加根目录后置 |
| A2A | 对照 v1.0.1，做认证、远端租户绑定与扩展协商，验证真实对端 | HTTP+JSON、gRPC、Push 和本地多租户按接入需求排序 |
| 长期记忆 | 可选 Embedding、融合检索与自动注入、审阅与修订 | 自动冲突合并、通用时间衰减和独立向量服务 |
| Sandbox 与 Skills | Windows/Linux 原生验收；一项离线 Skill 从审批到交付 | 通用网络代理、依赖自动安装、市场 |
| 备份与上下文 | 本机管理入口、恢复演练、预算归因与可控缩减 | 覆盖现有库、跨设备同步、自动裁剪当前输入 |

## ACP：先把 v1 的连接和续接做完整

- [ ] 按能力区分协议登录与用户发起的终端登录；宿主决定可启动程序与可用凭据，客户端可见登录、拒绝和取消状态，业务事件不存凭据。
- [ ] 隔离 Agent 的私有会话状态按工作区和注册身份分开。按能力区分 `session/load` 历史回放和 `session/resume` 续接，回放不触发工具或审批。身份改变时拒绝续接，结果未知的 prompt 不自动重发。[现有可信宿主续接](acp.md#跨重启续接-agent-会话)
- [ ] 受限终端复用现有隔离、审批与回收机制；验证创建、输出与终止。选定 Agent 如需联网，验证一条绑定注册身份与宿主策略的受限出口；可信宿主运行与隔离验收分开记录。
- [ ] 固定一个真实 Agent 版本，验证登录、审批、取消和重启后的新一轮。OpenCode 是候选；按对端实际声明确定测试范围，未覆盖的可选行为另做官方 SDK 契约测试。

ACP v2 于 **2026-11-20** 从 Draft 进入 Preview。单独试验 prompt 接收与回合结束分离、按 ID 更新、子会话、后台任务和回放完成标记怎样接入持久事件。Preview 互操作不作为发布门槛。

## A2A：先验证身份和真实任务

- [x] 对照 v1.0.1 的 JSON-RPC binding，保持线路版本 `1.0`。保留静态 Bearer，新增 OAuth 客户端凭据与过期重取。测试覆盖获取失败、令牌轮换和 401 不重发。[配置](a2a.md#配置与调用)
- [x] 将端点、binding、远端 `tenant` 和认证配置纳入持久注册绑定；统一为每项 RPC 写入租户。协商宿主启用的可选扩展，派发前拒绝不支持的必需扩展，并报告固定原因。
- [ ] 用真实 v1.0 对端验证拒绝未授权、一次提交、状态核对、终态产物、重启不重发；按能力验证取消。Foundry 入站可作为 Entra/JSON-RPC 候选，采用前核对凭据、媒体与流式限制。自建夹具只记为契约测试。远端租户路由不要求 Fliuno 自身变成多租户服务。[当前支持范围](a2a.md#固定版本与支持范围)

## 长期记忆：把语义检索接进任务创建

- [x] 保留 SQLite 原始记忆、来源和 revision；向量是可重建索引。增加可选 Embedding 提供方和有界后台索引，按工作区启用。[数据传输与配置](memory.md#自动索引与任务检索)说明发送范围。未启用、缺索引、超时或失败时回退词法；隔离模式不检索。网络查询不占用 SQLite 写事务，提交前复核选中项 revision。
- [x] 在直接启动的模型任务中融合向量和词法排名，保留精确 key 和中文双字匹配。低于相似度阈值的向量结果不注入；沿用 8 条、16 KiB 上限，并服从总预算。[当前记忆设计](memory.md#任务上下文)
- [x] 冻结实际注入项的来源、修订号、命中与回退原因，允许单次关闭注入。显式更新或删除后旧向量失效，历史快照保持原样。
- [ ] 用中英样本验证 Rust 偏好与高性能 OS 内核的改写关联、精确短词、跨语言、事实更新、无关问题、删除和索引故障。对照 0.0.4 的命中率、误注入率与额外 p50/p95 延迟；测完基线再固定数值阈值，用独立样本验收。失效或越范围记录不得注入。

## Sandbox 与 Skills：从注册到交付

- [ ] Windows AppContainer/Job 和 Linux Landlock/seccomp/cgroup v2 分别记录启动 p50/p95、取消到回收、硬退出残留和实际限额。Linux 区分有无 cgroup 委派。用同环境同负载证明改进，保留现有权限边界；把实际隔离与无法满足的限制报告给客户端。[执行边界](sandbox.md)
- [ ] 首版选一个离线文档或数据处理 Skill，只执行宿主登记的脚本。审阅计划包含脚本摘要、解释器、参数、输入输出范围、网络策略和预算。脚本或权限变更需重新审批；读取 `SKILL.md` 不触发执行，Skill 声明不授予权限。[当前 Skills](skills.md)
- [ ] 验收发现 → 审阅计划 → 批准一次 → 隔离执行 → 读取有界输出与收取产物。覆盖拒绝、脚本变更、超时、取消和重启；未知结果保留证据，不自动重试。

## 备份与恢复：接入本机管理流程

- [x] 提供本机 `backup verify`、`backup restore` 和 `backup verify-restored`，成功输出 JSON，失败报告原因。恢复只发布到新路径，离线命令不会启动任务或 Provider。[当前备份命令](backup.md#校验与还原)
- [x] 接入 `backup create`、`backup list`、`backup rotate` 和进度报告。Windows 测试覆盖持库时创建、复制期间提交写入、发布前超时清理、快照损坏和轮换范围。[创建与保留](backup.md#创建与保留)
- [x] 在 Windows 用 `v0.0.4` 标签的源码生成 schema 64 快照，执行校验、恢复到新路径并复核。保留了 3 条任务（已完成、待审批和未派发各一条）、工作区记忆与交付物正文；事件水位仍为 13。
- [x] 用实际 app-server 进程显式启动 schema 64 恢复副本。未派发任务标为中断，待审批与现有交付物保留。
- [x] 通过当前 app-server 恢复并校验 `v0.0.4` 快照，启动恢复副本后由 schema 64 迁移至 65。原库维持 64；副本保留记忆、交付物和已完成/待审批状态，未派发任务标记为中断。
- [ ] 带真实外部状态的切换演练。
- [x] 说明备份是否包含数据库外的工作区文件、凭据、Agent 私有会话和外部状态。恢复数据库不会撤销备份后已发生的外部操作。

## 上下文：讲清哪里超限、能怎样调整

- [x] 生成派发前确认超限时，在剩余尝试额度内先省略可选记忆，保留原始记忆快照与事件，再对请求完整预检。进程夹具确认输入在阈值之上时不会派发，移除记忆后恰好达到上限才派发一次。
- [ ] 分别报告输入窗口、输出预留、序列化与传输上限。列出历史、当前输入与媒体、工具定义与结果、记忆可归因的开销，标明估算、供应商计数、投影或不可单独计量。
- [ ] 可重读的大体积旧工具结果用摘要或产物引用，必要时再压缩历史。保护当前输入、活跃工具状态和必须原样回传的 Provider 片段。单个附件或工具定义仍超限时返回可操作原因；完整请求通过最终预检才派发。[当前预算规则](context.md#模型请求预算)
- [ ] 用三类 Provider 离线契约覆盖刚好够用、超过上限、中文、图片/PDF、工具 Schema、大工具结果、摘要失败和取消。明确区分尚未派发和结果未知，验证自动调整不重放已派发的调用。

## 交付顺序与验收

1. **固定基线和客户端契约。** 基于 0.0.4 确定对照数据、候选对端及可用凭据。对照[工作台快照和订阅](protocol.md#工作台实时订阅)，梳理任务、审批、认证、取消、预算拒绝和恢复所需的状态，按能力声明兼容旧客户端。
2. **按依赖跑通各条链路。** 先建立隔离执行与恢复边界，再接 ACP 终端和 Skills；A2A 身份、记忆检索与预算可独立验收。分别记录夹具、官方 SDK、真实对端和原生系统证据，常规回归无需真实凭据。
3. **发布前用无界面客户端走完一条工作流。** 提交 → 查看进度 → 处理审批或认证 → 断线后按游标续读 → 打开交付物。验证通知缺口、重复请求和权限不可用，另做恢复演练与 Windows/Linux 原生验收。后置项不阻挡 TUI 起步。

## 0.0.6：第一版 TUI

- [ ] 首个里程碑用离线 Provider 做本机工作台：选择工作区和会话、提交和取消任务、查看状态、处理审批、打开交付物。真实 Agent 和登录入口按能力逐步开放；完整 Run 图、记忆编辑和备份设置后续推进。
- [ ] 复用 `fliuno-client` 和应用协议。用 `work/subscribe` 获取快照和水位，收到缺口就从持久游标补读；模型流式预览可以丢弃。键盘和渲染不等待 RPC。区分连接断开与服务重启；如果退出 TUI 会关闭 app-server，则不承诺任务继续后台运行。独立服务托管留待首版验证后决定。
- [ ] Ratatui/Crossterm 是候选，开工时再定。验收覆盖 Windows 和 Linux 终端、非 TTY、中文宽字符、窄屏、尺寸变化、慢客户端和正常退出或异常后的终端模式恢复。

## 调研依据

- [ACP v1 认证](https://agentclientprotocol.com/protocol/v1/authentication)、[v1 会话](https://agentclientprotocol.com/protocol/v1/session-setup)和 [v2 Preview 公告](https://agentclientprotocol.com/announcements/acp-v2-preview)：认证、回放与续接分别按能力验收；v2 的回合与回放另做试验。
- [A2A v1.0.1 规范](https://a2a-protocol.org/v1.0.1/specification/)与 [Foundry 入站端点](https://learn.microsoft.com/en-us/azure/foundry/agents/how-to/enable-agent-to-agent-endpoint)：以真实对端需要的身份、绑定和流式能力排序。
- [OpenClaw 内置记忆](https://docs.openclaw.ai/concepts/memory-builtin)、[SQLite FTS5](https://www.sqlite.org/fts5.html#the_trigram_tokenizer)和 [LongMemEval](https://arxiv.org/abs/2410.10813)：融合词法和向量，保留短词基线；评估改写、更新与无关查询。
- [Claude Code 隔离](https://code.claude.com/docs/en/sandboxing#network-isolation)与 [Agent Skills 脚本指南](https://agentskills.io/skill-creation/using-scripts)：执行隔离、联网策略与技能发现分别处理，先验证已注册的离线脚本。
- [SQLite Online Backup](https://sqlite.org/backup.html)与 [Anthropic 上下文工程](https://www.anthropic.com/engineering/effective-context-engineering-for-ai-agents)：快照要有实际恢复流程；清理旧工具结果与压缩历史分开验证。
- [OpenCode ACP](https://opencode.ai/docs/cli/acp/)、[Ratatui 测试](https://ratatui.rs/recipes/testing/)与[终端异常恢复](https://ratatui.rs/recipes/apps/panic-hooks/)：区分终端登录与协议登录，把输入、渲染和终端模式恢复列入验收。
