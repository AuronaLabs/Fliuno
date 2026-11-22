# 0.0.5 迭代计划与 TUI 起步

[文档中心](README.md) › [开发路线图](roadmap.md) · [TUI 首版](#007第一版-tui) · [调研依据](#调研依据)

状态：**0.0.5 范围已验收**。调研核对日期：2026-11-21。

0.0.5 补齐首版 TUI 的内核依赖：真实 Agent 接入、可解释的记忆与预算、隔离脚本执行、可操作的备份恢复。继续使用 Rust、SQLite 和本机应用协议。[0.0.6 优先现代化内核](plan-0.0.6.md)，TUI 从 0.0.7 起步。GUI 和原生 macOS 沙箱另行排期。

| 方向 | 0.0.5 范围 | 后续评估 |
| --- | --- | --- |
| ACP | v1 认证、隔离会话续接、受限终端、真实对端 | v2 Preview 独立试验；服务端、通用二进制文件代理与附加根目录后置 |
| A2A | 对照 v1.0.1，做认证、远端租户绑定与扩展协商，验证真实对端 | HTTP+JSON、gRPC、Push 和本地多租户按接入需求排序 |
| 长期记忆 | 可选 Embedding、融合检索与自动注入、审阅与修订 | 自动冲突合并、通用时间衰减和独立向量服务 |
| Sandbox 与 Skills | Windows/Linux 原生验收；一项离线 Skill 从审批到交付 | 通用网络代理、依赖自动安装、市场 |
| 备份与上下文 | 本机管理入口、恢复演练、预算归因与可控缩减 | 覆盖现有库、跨设备同步、自动裁剪当前输入 |

## ACP：先把 v1 的连接和续接做完整

- [x] 宿主指定 ACP v1 协议登录方法；会话启动前按 Agent 声明校验并调用一次。客户端可见认证成功、拒绝、取消及未知状态，业务事件不存凭据。Windows 进程测试覆盖重复方法 ID、终端方法拒绝、硬退出与幂等重试。[认证接口](acp.md#登录)
- [x] 接通用户发起的受控终端登录。宿主决定可启动程序和可用凭据；不转发对端提供的终端参数及环境变量。Windows 和 Debian 来宾用官方 SDK 夹具验证交互、关闭、超时、身份绑定和旧任务不重发。[本机登录](acp.md#本机交互终端登录)
- [x] 隔离 Agent 的私有会话状态按工作区、注册身份与上一轮任务保存。Windows AppContainer 和 Debian cgroup v2 来宾用官方 SDK 夹具验证硬重启续接、身份变化拒绝与未知 prompt 不重发；SDK 回放测试区分 `session/load` 历史与 `session/resume` 续接，旧权限不授权新操作。[隔离续接](acp.md#原生隔离下保存会话)
- [x] 受限终端复用现有隔离、审批与回收机制。Windows AppContainer 和 Debian cgroup v2 来宾使用 SDK 夹具验证一次性批准、拒绝、输出截断、会话绑定、停止、释放和取消。[受限终端](acp.md#受限终端)
- [x] 固定 OpenCode `v1.19.6`，在 Windows `trusted_host` 验证协议登录、单次审批、取消、重启后的新一轮和请求重放。模型走本地夹具；隔离模式、受限出口、交互登录和 Linux 分开验收。[真实对端记录](acp.md#windows-真实对端)

可信宿主运行与隔离验收分开记录；选定的隔离 Agent 如需联网，另验收一条绑定注册身份与宿主策略的受限出口。

ACP v2 于 **2026-11-20** 从 Draft 进入 Preview。单独试验 prompt 接收与回合结束分离、按 ID 更新、子会话、后台任务和回放完成标记怎样接入持久事件。Preview 互操作不作为发布门槛。

## A2A：先验证身份和真实任务

- [x] 对照 v1.0.1 的 JSON-RPC binding，保持线路版本 `1.0`。保留静态 Bearer，新增 OAuth 客户端凭据与过期重取。测试覆盖获取失败、令牌轮换和 401 不重发。[配置](a2a.md#配置与调用)
- [x] 将端点、binding、远端 `tenant` 和认证配置纳入持久注册绑定；统一为每项 RPC 写入租户。协商宿主启用的可选扩展，派发前拒绝不支持的必需扩展，并报告固定原因。
- [x] 本机运行固定提交的 A2A Testbed Task Runner，使用 JSON-RPC 1.0 验证一次提交、状态核对、终态产物、取消和强制重启不重发；本机 Bearer 入口验证未授权不转发。Foundry 与 Entra 留作后续云端互操作。远端租户路由不要求 Fliuno 自身变成多租户服务。[真实对端记录](a2a.md#a2a-testbed-真实对端)

## 长期记忆：把语义检索接进任务创建

- [x] 保留 SQLite 原始记忆、来源和 revision；向量是可重建索引。增加可选 Embedding 提供方和有界后台索引，按工作区启用。[数据传输与配置](memory.md#自动索引与任务检索)说明发送范围。未启用、缺索引、超时或失败时回退词法；隔离模式不检索。网络查询不占用 SQLite 写事务，提交前复核选中项 revision。
- [x] 在直接启动的模型任务中融合向量和词法排名，保留精确 key 和中文双字匹配。低于相似度阈值的向量结果不注入；沿用 8 条、16 KiB 上限，并服从总预算。[当前记忆设计](memory.md#任务上下文)
- [x] 冻结实际注入项的来源、修订号、命中与回退原因，允许单次关闭注入。显式更新或删除后旧向量失效，历史快照保持原样。
- [x] 用中英样本验证 Rust 偏好与高性能 OS 内核的改写关联、精确短词、跨语言和无关问题；更新、删除、索引故障和工作区隔离另由进程测试验证。固定 E5 配置在第二批独立样本上命中 6/7、误注入 0/12，相对 0.0.4 的 p95 增量 14.49 ms，达到预设验收门槛。[数据与复现](memory.md#中英检索评测)

## Sandbox 与 Skills：从注册到交付

- [x] Windows AppContainer/Job 和 Linux Landlock/seccomp/cgroup v2 分别记录启动 p50/p95、取消到回收、硬退出残留和实际限额。Debian 来宾分有无委派实测，有委派时回收 p95 从 13.87 ms 降到 5.13 ms。Windows 硬退出覆盖受监督 Job。进程工具报告实际后端和限额。[测量和复现](sandbox.md#005-启动和回收测量)
- [x] 支持宿主登记离线文本 Skill，附逐行生成列表的示例。审阅计划包含脚本摘要、解释器、参数、输入输出范围、网络策略和预算。脚本或权限变更需重新审批；读取 `SKILL.md` 不触发执行，Skill 声明不授予权限。[配置与审批](skills.md#执行离线文本-skill)
- [x] Windows AppContainer 和 Debian 6.12.107 Landlock/seccomp 来宾各通过三组进程测试，覆盖发现、审阅、逐次批准、隔离脚本、产物、拒绝、脚本变更、输出截断、超时、取消和重启不重发。[复现](skills.md#离线验证)

## 备份与恢复：接入本机管理流程

- [x] 提供本机 `backup verify`、`backup restore` 和 `backup verify-restored`，成功输出 JSON，失败报告原因。恢复只发布到新路径，离线命令不会启动任务或 Provider。[当前备份命令](backup.md#校验与还原)
- [x] 接入 `backup create`、`backup list`、`backup rotate` 和进度报告。Windows 测试覆盖持库时创建、复制期间提交写入、发布前超时清理、快照损坏和轮换范围。[创建与保留](backup.md#创建与保留)
- [x] 在 Windows 用 `v0.0.4` 标签的源码生成 schema 64 快照，执行校验、恢复到新路径并复核。保留了 3 条任务（已完成、待审批和未派发各一条）、工作区记忆与交付物正文；事件水位仍为 13。
- [x] 用实际 app-server 进程显式启动 schema 64 恢复副本。未派发任务标为中断，待审批与现有交付物保留。
- [x] 恢复并校验 `v0.0.4` 快照，显式启动恢复副本后迁移 schema。原库维持 64；副本保留记忆、交付物和已完成/待审批状态，未派发任务标记为中断。
- [x] Windows 上用真实 OpenCode `v1.19.6` 演练外部状态切换：恢复并验证新库，续接同一 ACP 会话，原库 SHA-256 不变。OpenCode 的私有状态保留在原路径。[复现说明](acp.md#windows-真实对端)
- [x] 说明备份是否包含数据库外的工作区文件、凭据、Agent 私有会话和外部状态。恢复数据库不会撤销备份后已发生的外部操作。

## 上下文：讲清哪里超限、能怎样调整

- [x] 生成派发前确认超限时，在剩余尝试额度内先省略可选记忆，保留原始记忆快照与事件，再对请求完整预检。进程夹具确认输入在阈值之上时不会派发，移除记忆后恰好达到上限才派发一次。
- [x] 预算拒绝事件分别报告输入窗口、输出预留、序列化与传输上限。按历史、当前文字与媒体、工具定义与结果、记忆和供应商续接数据估算本地 JSON 字节；区分观测下界、供应商计数、投影和无法单独计量的 Token。[预算事件](context.md#模型请求预算)
- [x] 确认未派发后可把已完成任务的大工具结果投影成短摘录和原任务引用，必要时再压缩历史。离线进程验收确认旧原文仍可读取、当前结果和签名仍完整，重启重放请求不会增加执行次数。
- [x] 附件或工具定义仍超限时返回可操作建议；完整请求通过最终预检才派发。进程测试覆盖超限的 PDF 只发起计数请求，以及带工具定义时的精简提示。[当前预算规则](context.md#模型请求预算)
- [x] 分层离线验收：三家适配器用包含中文、图片、PDF 和工具 Schema 的同一请求检查 JSON 字节上限与超一字节不派发；内核和进程回归覆盖大工具结果、摘要失败与取消，并验证预算调整不重放已派发的调用。真实模型的 Token 计数不属于这组离线证据。

## 交付顺序与验收

1. **固定基线和客户端契约。** 基于 0.0.4 确定对照数据、候选对端及可用凭据。对照[工作台快照和订阅](protocol.md#工作台实时订阅)，梳理任务、审批、认证、取消、预算拒绝和恢复所需的状态，按能力声明兼容旧客户端。
2. **按依赖跑通各条链路。** 先建立隔离执行与恢复边界，再接 ACP 终端和 Skills；A2A 身份、记忆检索与预算可独立验收。分别记录夹具、官方 SDK、真实对端和原生系统证据，常规回归无需真实凭据。
3. **发布前用无界面客户端走完一条工作流。** 提交 → 查看进度 → 处理审批或认证 → 断线后按游标续读 → 打开交付物。验证通知缺口、重复请求和权限不可用，另做恢复演练与 Windows/Linux 原生验收。后置项不阻挡 TUI 起步。

Windows 无界面流程已通过：Rust 客户端订阅工作台，使用离线模型提交任务，重启 app-server 后从旧游标找回同一审批。重复提交保持同一任务，无效审批不推进水位；批准一次后只执行一次。测试丢弃实时水位并注入缺口通知，从 SQLite 补齐事件，再通过客户端校验下载产物的摘要和正文。复现：`cargo test -p fliuno-app-server --all-features --locked --test process headless_workflow`。

## 0.0.7：第一版 TUI

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
