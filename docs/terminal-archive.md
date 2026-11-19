# 终端归档与身份保留

[文档首页](README.md) · [终端订阅与恢复](terminal-subscriptions.md) · [应用协议](protocol.md#跨轮次终端)

终端输出和输入操作缓存可以回收，创建身份不能复用。Fliuno 将小型 tombstone 账本保存在状态根的 `archive.db`，与每个 terminal ID 的输出目录分开；Tauri 或其他客户端通过相同应用协议管理它，不接触 SQLite 路径、IPC 凭据或 PID。

`initialize.capabilities.terminalArchiving:true` 表示支持此功能。`terminal/archive({sessionId,terminalId})` 是受信本机客户端显式发起的保留策略操作，返回 `{snapshot,cleanupPending}`；不会隐式终止仍运行的监督进程。模型通过单独的 `terminal_archive_*({terminalId})` 工具使用它，走既有逐次审批，session 始终从任务关系解析。拒绝审批不删除输出。需要先 release/kill 或等待自然退出，再等监督进程真正释放所有权锁。

归档过程按以下顺序执行：

1. 持有根目录的 `registry.lock`，检查持久 session 归属，尝试获取该记录的 `owner.lock`。监督进程持锁时拒绝；PID、状态文本和 endpoint 是否存在都不能替代此检查。
2. 持锁重读最后持久快照。曾处于 starting/running 的离线记录继续保持 `outcome_unknown`，归档不将未知执行结果变成成功或终止证明。
3. 在独立 SQLite 事务中提交 tombstone，保存 session、terminal ID、最后状态、退出与输出关闭证据、字节计数和 `archivedAtUnixMs`。采用 DELETE journal 与 synchronous=EXTRA。旧输出全部失效，快照 `startOffset` 提升到原 `endOffset`；后者不重置为 0。
4. 事务成功后，尝试删除 spool、操作缓存、endpoint、socket 与所有权锁。只处理明确列出的内部文件，不递归删除目录；发现未知文件、符号链接或 Windows reparse point 时保留待清理状态。文件暂被其他读者打开或删除失败时也保留 `cleanupPending:true`。
5. 完成删除后再更新清理标志。归档快照和原创建身份始终留在账本中。

主机在接收相同创建身份时检查账本，已归档 ID 永不重新预留。监督进程也在取得自己的锁之后、创建 PTY 之前检查账本：即使主机已交付的 bootstrap 被延迟执行，归档仍能阻止它启动命令。此前已运行的监督进程持有所有权锁，归档不能越过它。当前不支持混合版本的监督进程迁移或交接。

`terminal/archive` 可显式重复调用同一 ID；重复调用保持原归档时间与快照，只重试未完成的清理。提交结果无法确认时，工具保存含 terminal/session ID 和 archive 阶段的未知结果证据，客户端应查询记录。重试清理不会恢复旧命令。服务启动和下次创建预留前，各尝试最多 8 个已提交的待清理记录；失败项轮转到队尾，避免少数难以删除的文件阻塞其他记录。自动归档由宿主保留策略控制。

根目录还有 `archive.version` 初始化标记。标记已存在而账本丢失、重置为空库或版本未知时，服务拒绝启动，避免把历史身份当作从未使用。账本和标记属于同一持久状态，应一起备份与恢复；当前不提供删除 tombstone 的接口。

归档后 `terminal/get` 仍返回该 session 的最后快照，`archivedAtUnixMs` 非 null，`supervisorConnected:false`。`terminal/output` 返回空数据：游标落后于原末尾时 `gap:true`，`from == next == endOffset`；超过末尾仍拒绝。客户端不能将这段空数据解释为没有产生过输出。`terminal/subscribe` 返回同样的归档投影，随后以 `reason:"archived"` 结束。原先的 `outputClosed` 证据保留，归档本身不伪造 PTY 已排空。

`terminal/list` 只列出未归档的记录。使用 `terminal/archives({sessionId,after?:0,limit?:100})` 分页查询归档历史，结果为 `{items,nextAfter,hasMore}`，每项为 `{snapshot,cleanupPending}`。limit 范围 1..100，游标为不透明的账本序号；其他 session 的记录不会被返回。

当前每根仍最多有 64 个物理终端记录、8 个活动或不确定记录。归档清理完成会释放物理名额；仍待删除的目录保守占用 64 项容量。归档账本最多保留 100,000 个 tombstone，到限后拒绝新增归档，不淘汰身份来换取空间；查询和重试既有清理仍可进行。账本仅保存有界快照元数据，不保存原始输出、输入、环境值或 IPC token。其他平台的进程树回收证据仍服从平台能力，获取 owner 锁仅证明该监督进程不再持有记录。

## 宿主保留策略

在宿主传给 `--terminal-config` 的 JSON 顶层增加 `"retention":{"archiveAfterExpiryDays":7}`，可自动归档到期的终端输出。省略 `retention` 时保留历史，仍可手动归档。天数取 1 到 365，从持久快照的计划到期时间 `expiresAtUnixMs` 开始计算。提前退出的终端也会保留至这个期限。

配置注册成功后执行一次检查。若新的终端预留遇到 64 项物理容量上限，也会检查一次。每次最多扫描 128 个目录条目并归档 8 项。检查须能取得监督进程的所有权锁，并确认记录身份和时间一致。正常结束的终端还需有输出已关闭的证据。明确在 PTY 创建前失败的记录也符合条件。运行中、结果不明和输出未关闭的记录保留以供检查。

自动归档复用手动归档账本，现有查询与游标恢复语义相同。若账本写入失败，从原记录继续查询。已入账而文件清理未完成时，后续启动或预留会继续执行有界清理，也可用 `terminal/archive` 重试指定项。空闲期间不定时扫描。

## 客户端批量归档

`initialize.capabilities.terminalBatchArchiving:true` 表示本连接配置了批量归档能力，`maxTerminalArchiveBatch` 为 32。受信本机客户端通过 `terminal/archiveBatch({sessionId,terminalIds})` 明确选择 1..32 个不同 ID。完整列表先校验，非法或重复 ID、空列表、超额列表会在第一项归档前整体拒绝；不会按年龄自动挑选或推断保留策略。模型继续使用单 ID 归档工具的逐次审批。

有效列表按请求顺序串行处理，每项独立提交身份并清理输出，返回 `{items:[...]}`。客户端按每项的 `terminalId` 和 `status` 展示结果：

| status | 其余字段 | 含义 |
| --- | --- | --- |
| `archived` | `archive:{snapshot,cleanupPending}` | 身份已归档；`cleanupPending:true` 表示部分输出仍待清理 |
| `failed` | `message` | 此项遇到已知拒绝，例如监督进程仍持锁、会话不符、记录不存在或提交前失败 |
| `outcome_unknown` | `message` | tombstone 提交结果无法确认，保留 ID 供查询；不描述为归档成功或确定失败 |

某项失败或不确定时继续尝试其他不同 ID。批量操作不提供跨项事务：已完成项不会因为后续失败而恢复输出。批量响应丢失时，客户端应查询 `terminal/get` 或分页 `terminal/archives` 核对各 ID，再显式重试；已归档项沿用原快照和归档时间，仅重试待完成的清理。所有请求继续检查 session 归属，仍运行的监督进程不会被隐式终止。

实现位于独立 `archive/batch` 模块，复用单 ID 的锁、tombstone 和保守清理路径，不引入后台任务或第二套回收逻辑。测试以真实 SQLite reader 锁阻塞 COMMIT，验证返回不确定项而不删除证据；还覆盖单项插入失败后的后续项、重启重试、未知文件和真实 PTY 持锁拒绝。

验证分层包括：真实 SQLite 的归档/重开、分页、容量释放、插入失败不删证据、账本缺失拒绝重置；未知文件阻止清理与有界公平重试；真实 app-server 的逐次审批、归档读取/订阅、重启补清理；以及同一 bootstrap 构造器的原生 PTY 成功对照和归档后延迟启动拒绝探针。Linux 编译检查不替代 Linux 运行测试。
