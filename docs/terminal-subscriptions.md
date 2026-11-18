# 终端输出订阅与恢复

`terminal/subscribe` 观察已有终端，不创建或控制进程，也不产生新的执行审批。必须先完成 `initialize`，并检查 `capabilities.terminalOutputSubscriptions`。订阅 ID 只属于当前 app-server 连接；工作会话和 terminal ID 仍由持久记录确定。它不构成远程用户认证。

```json
{"jsonrpc":"2.0","id":10,"method":"terminal/subscribe","params":{"sessionId":"SESSION","terminalId":"TERMINAL","after":0,"limit":32768}}
```

响应为 `{subscriptionId,sessionId,terminalId,after}`。初始 spool 回放、原始字节增量以及状态变更都通过 `notifications/terminalOutput` 发送，params 为 `{subscriptionId,output}`。`output` 与 `terminal/output` 结果完全同型：`snapshot`、`from`、`next`、`gap` 和 `dataBase64`。

stdio 先写出订阅响应，再写通知。客户端按 subscription ID 路由，对每个订阅按到达顺序消费。`after` 是已经消费的绝对字节数；省略表示从 0 开始。`limit` 是单页原始字节预算，范围 1..32768，默认 32768；base64 字符数不计入此预算。未来游标、错误 session 和无效预算在建立订阅前拒绝。

PTY 可以包含任意字节、ANSI 转义及被切开的 UTF-8 序列。应先 base64 解码，再按连续字节交给终端 emulator 或流式解码器；逐帧独立解码字符串会破坏中文。纯状态更新可以没有字节，`from == next`。每帧快照中的 `endOffset` 可能大于该帧 `next`，表示仍有已提交数据等待后续页。

客户端正常消费时，下一帧 `from` 等于上一帧 `next`。spool 保留窗口已丢弃旧数据时，通知明确设置 `gap:true`，并使 `from` 跳到 `snapshot.startOffset`。客户端应报告缺口并重置受影响的终端渲染状态；无法从已经删除的字节重建完整屏幕。消费完一帧后保存其 `next`，重新订阅时作为 `after`，避免依赖 PID 或旧订阅 ID。

`notifications/terminalSubscriptionEnded` 的 params 为 `{subscriptionId,sessionId,terminalId,after,reason}`。`after` 是本连接已写出的最后一帧字节游标；尚未写出的数据不计入。原因包括：

| reason | 含义与后续处理 |
| --- | --- |
| `closed` | 已观察到终态和 PTY 输出关闭，并读到当时的末尾；订阅自动释放 |
| `supervisor_unavailable` | IPC 断开或无法连接；最后读取可用的持久页，并给出保守离线快照后结束。运行状态可能为 `outcome_unknown`，不代表允许重跑命令 |
| `overflow` | 客户端消费过慢导致队列用尽；已排队页先送达，再结束。可用最后消费的 `next` 补读或重新订阅，仍须处理保留窗口缺口 |
| `failed` | 观察出现协议、存储或其他错误；可用 `terminal/get` / `terminal/output` 检查并显式恢复 |
| `archived` | 已显式归档，保留窗口为空；最后一帧报告原末尾游标与归档时间，旧输出不会再恢复 |

`closed` 不只依赖 root 退出码：`outputClosed` 也必须为 true，且已经读取末尾。无法确认完整排空时报告可用证据，不伪造关闭完成。监督进程断开后不进行轮询或隐式重连；再次观察由客户端显式发起。终端继续持久保存输出。

使用 `terminal/unsubscribe({subscriptionId})` 取消观察，重复取消返回 `removed:false`。成功响应之前已经写到 stdout 的通知仍可能到达；成功响应之后不再输出此 ID 的通知，也不补发 ended。取消、客户端断开或 app-server 关闭仅关闭观察 socket，持久终端的生命周期仍由 preset、退出、kill/release 和 OS supervisor 决定。

单 app-server 连接最多 8 个活动订阅，每个订阅最多排队 8 页；单页最多 32 KiB 原始字节。通知按订阅轮转发送，结束标志有独立状态槽，队列满不会丢掉结束原因。stdout 阻塞时后台观察器不会等待它，溢出会停止该订阅读取，PTY actor 持续写入有界 spool。队列内存与 spool 保留量分别受限；它们不提供无限输出历史。

监督进程自身最多接受 8 个长连接观察者，跨 app-server 共享该预算。已接受请求的全局连接预算为 16，握手和普通操作有超时；长连接写入单帧最多等待 5 秒。初始订阅握手最多等待 4 秒，不使用状态查询的 150 ms 观察预算。读缓冲、JSON 帧和命令队列都有上限。

实现分为三层：actor 只提交 spool 和发布合并后的变更提示；`fliuno-terminal/subscription` 通过认证长连接从提交后的字节游标读取，不定时轮询；app-server 的连接订阅管理器负责预算、取消、公平队列和通知。注册变更观察先于首次 spool 读取，避免回放切换实时流时漏掉提交。提示可合并，游标和 spool 是事实来源。
