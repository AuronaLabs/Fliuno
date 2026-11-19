# AG-UI 接入

[文档首页](README.md) · [应用协议](protocol.md#rust-调用方)

宿主可以通过 Rust 客户端提交文本任务，并把已提交的任务事件转换成 AG-UI 消息。`fliuno-ag-ui-http` 提供可嵌入的 HTTP 路由，宿主负责监听地址和会话授权。审批可以由宿主通过应用协议处理，也可以挂载独立授权的审批入口。

[提交文本任务](#提交文本任务) · [投影内核事件](#投影内核事件) · [SSE 输出与续传](#sse-输出与续传) · [HTTP 接入](#http-接入)

## 提交文本任务

在宿主选定本机会话后，将其 ID 及请求正文交给 `Client::start_ag_ui_run`。`threadId` 必须等于该会话 ID。服务需支持 `taskStartSummary` 和 `taskSummaryByRequest` 两项应用协议能力。

```json
{
  "threadId": "local-session-id",
  "runId": "request-001",
  "protocolVersion": "1.0",
  "state": {},
  "messages": [{ "id": "msg-1", "role": "user", "content": "整理这次任务的交付物" }],
  "tools": [],
  "context": [],
  "forwardedProps": {}
}
```

| 字段 | 当前支持范围 |
| --- | --- |
| `runId` | 非空且不含控制字符，最多 512 个 UTF-8 字节。同一会话用同一 `runId` 重试相同输入只创建一个任务。 |
| `messages` | 纯文本的 `user` 或 `assistant` 消息。最后一条必须是 `user`，正文可以是字符串或文本片段数组，合计最多 256 KiB。内核使用最后一条消息和本地会话历史。 |
| `protocolVersion` | 可省略。提供时使用 `1.0`。 |
| 其余输入 | JSON 正文最多 1 MiB。`state` 和 `forwardedProps` 可省略、设为 `null` 或空对象。`tools`、`context` 和 `resume` 可省略或使用空数组。当前不支持 `parentRunId`、工具调用、加密消息或非文本内容。 |

用返回值的 `task()` 取得已创建的任务摘要。连接中断后，可以调用 `Client::attach_ag_ui_run(session_id, run_id, on_notification)`，从数据库找回同一个任务。用 `run.subscribe_sse(...)` 输出事件，即可沿用首次执行的 `runId`。后续显式恢复的执行段仍使用各自派生的 ID。

## 投影内核事件

对于已经通过本机协议创建的任务，`ag_ui::AgUiProjector` 可以直接将持久事件转成 AG-UI 事件对象。

获取 `task/getSummary` 返回的 `TaskSummary`，然后用 `Client::subscribe_events` 建立任务订阅和回放窗口。从该任务的 `TaskStarted` 开始按全局序号依次投影。回放至服务水位后，将属于该任务的实时事件交给同一个投影器。重建投影时从任务起点回放，任务、执行段和消息 ID 会与上次一致。

```rust
use fliuno_client::ag_ui::AgUiProjector;
use fliuno_client::ReplayScope;
use fliuno_protocol::{TaskIdParams, TaskSummary};

let summary: TaskSummary = client.call("task/getSummary", &TaskIdParams { task_id }, |_| {})?;
let mut projection = AgUiProjector::new(&summary);
let mut replay = client.subscribe_events(ReplayScope::Task(summary.id.clone()), 0, |_| {})?;
while let Some(page) = replay.next_page(&mut client, 64, Some(64 * 1024), &mut buffer_live)? {
    for event in page {
        for projected in projection.project(event)? {
            consume(projected)?;
        }
    }
}
```

`buffer_live` 接收回放期间到达的通知，回放结束后按序号处理缓冲的持久事件。若通知队列出现缺口，保存已消费的序号，从该序号重新订阅并补读。保持每个任务独立的投影器和游标。

| Fliuno 已提交事件 | AG-UI 对象 |
| --- | --- |
| 任务启动或显式恢复 | `RUN_STARTED`。`threadId` 使用会话 ID，`runId` 由任务 ID 和本段开始序号派生。 |
| 任务完成 | 提交后的文本作为 `TEXT_MESSAGE_START`、`TEXT_MESSAGE_CONTENT`、`TEXT_MESSAGE_END`，随后是 `RUN_FINISHED`。空文本直接结束。 |
| 任务失败或进程中断 | `RUN_ERROR`。本机中断后的显式恢复会开启新的执行段。 |
| 取消活动任务 | `RUN_FINISHED`，`outcome` 为 `{"type":"cancelled"}`。若前一个执行段已中断，取消只发 `fliuno.task_cancelled` 自定义事件。 |
| 审批请求、决定和关闭 | `CUSTOM`，名称分别为 `fliuno.approval_requested`、`fliuno.approval_resolved` 和 `fliuno.approval_closed`。值包含任务 ID 与审批 ID。审批细节可从原 RPC 按 ID 查询。 |
| 产物和模型用量 | `CUSTOM`，名称为 `fliuno.artifact_created` 和 `fliuno.model_usage_reported`。产物内容通过现有产物 RPC 读取。 |

投影涵盖持久的生命周期、完成文本和轻量引用。进度预览、宿主工具参数和远端原始载荷通过各自的协议处理。AG-UI 的中断和恢复输入尚未接入。

## SSE 输出与续传

一条连接可以使用 `run.subscribe_sse` 订阅任务并输出 SSE。`AgUiSseFeed` 按页读取持久事件，接收实时水位通知，发现缺口时自动补读。对于通过应用协议创建的任务，也可以用 `AgUiSseFeed::subscribe` 从任务摘要建立订阅。

```rust
let mut feed = run.subscribe_sse(&mut client, last_event_id, &mut handle_other)?;
while let Some(frames) = feed.write_next_page(&mut client, &mut response_body, &mut handle_other)? {
    if frames > 0 {
        response_body.flush()?;
    }
}
```

`None` 表示已读到当前已知的水位。宿主继续读取通知，将它们交给 `feed.observe(note)`，再调用 `write_next_page`。宿主可以在两次读取之间执行审批等 RPC，并把期间到达的通知走同一个 `observe`。该连接同一时间只能维护一个事件订阅。连接中断或写入失败时，用消费端最后完整收到的事件 ID 建立新 feed。

读完当前可用的页后，宿主可用 `feed.run_phase()` 决定是否结束响应。`Finished` 表示任务已结束，`Interrupted` 表示当前执行段因本机中断而结束。恢复后，新的执行段会回到 `Running`。宿主主动取消时，对已绑定的任务调用 `task/cancel`，然后用同一订阅输出取消结果。

### 在一条连接上交错回放多个任务

宿主需要服务多个读取方时，可以用 `client.event_watermark` 读一次已提交水位，再为每个任务建立 `run.replay_sse(through, last_event_id)`。各个 `AgUiSseReplay` 共用 stdio 连接，但各自保存投影和游标。`next_chunk` 每次读取并编码一页，由宿主释放客户端借用后再将字节写给消费者。

读完窗口后，用 `advance_through` 接入新的已提交水位。宿主可以查询水位，或从自己维护的实时通知中得到水位。每个读取方只读所属任务，回放和水位查询都保留现有实时订阅。

### 帧格式和底层接口

`fliuno_client::ag_ui::AgUiSseStream` 可以把单个持久事件的投影写入实现 `Write` 的响应体。由宿主提供 HTTP 路由和连接，响应使用 `AG_UI_SSE_CONTENT_TYPE`（`text/event-stream`）。每条消息有一行 `id` 和一行 `data`。`data` 是 AG-UI 事件 JSON，`id` 由任务 ID、内核事件序号和该事件内从 1 开始的消息编号组成。

重连时，读取客户端发送的 `Last-Event-ID`。通过 AG-UI 提交的任务用 `run.sse(last_event_id)` 创建输出流。直接投影内核任务时，使用 `AgUiSseStream::new(&summary, last_event_id)`。将上文回放循环中的 `projection.project(event)` 换成 `stream.write_event(event, &mut response_body)`。和首次连接一样从 `TaskStarted` 回放，以重建执行段状态。输出从指定消息之后开始，即使它位于一个任务事件产生的多条消息中间。

历史补读完成后，调用 `stream.finish_replay()` 确认找到了续传位置，再把暂存的实时事件按序写入同一个流。找不到 ID、任务不匹配或写入中断会返回错误。写入中断后，消费端用最后一条完整收到的 SSE ID 建立新流。SSE 流输出已提交事件。

## HTTP 接入

`fliuno_ag_ui_http::router` 返回可挂载到 Axum 的路由。传入 `Arc<tokio::sync::Mutex<Client<_, _>>>` 及授权回调。回调根据请求头返回授权的本机会话 ID，返回 `None` 时请求在调用内核前被拒绝。宿主可以在两次 SSE 页读取之间通过同一个客户端处理任务审批。

| 请求 | 行为 |
| --- | --- |
| `POST /run` | 接收 `application/json` 的文本任务请求，要求 `threadId` 与授权会话一致。返回 `text/event-stream`。 |
| `GET /runs/{run_id}` | 在授权会话中找回运行。可发送 `Last-Event-ID`，从指定帧之后续传。不存在或不属于该会话的运行返回 `404`。 |
| `DELETE /runs/{run_id}` | 取消授权会话中的运行，成功返回 `204`。重复取消仍返回 `204`。已经完成或失败的任务返回 `409`。 |

路由每次读取一页持久事件，释放客户端后再把编码好的页交给响应体。当前限制同时 16 条响应或待解析请求，POST 正文最多 1 MiB，读取超时 10 秒。空闲响应共享约每 150 毫秒一次的水位查询。水位没有推进时不读取事件页，并定期发送 SSE 保活注释。执行段结束后关闭响应。

### HTTP 审批

用 `router_with_approvals` 挂载审批入口。第二个授权回调根据请求头确定审批者及其可审阅的本机会话。通过任务提交授权的身份仍需单独通过审批授权。

| 请求 | 行为 |
| --- | --- |
| `GET /runs/{run_id}/approvals/{approval_id}` | 读取绑定到该任务的待审批工具及参数。 |
| `POST /runs/{run_id}/approvals/{approval_id}` | 提交 `{"decision":"allow_once"}` 或 `{"decision":"deny"}`，返回持久决定事件。 |

审批 ID 来自 SSE 的 `fliuno.approval_requested` 事件。重复提交同一决定返回原事件，提交相反决定返回 `409`。属于其他任务或会话的审批返回 `404`。宿主可以在响应保持打开时处理审批，审批入口有独立的并发名额，并通过 `approval/respondDecision` 提交决定。

事件字段对照了 AG-UI 官方规范和官方 TypeScript Schema，固定在提交 `3416b86da4b3288e87f827c7f2759a7fa18b5231`。[事件规范](https://github.com/ag-ui-protocol/ag-ui/blob/3416b86da4b3288e87f827c7f2759a7fa18b5231/docs/concepts/events.mdx) · [SDK Schema](https://github.com/ag-ui-protocol/ag-ui/blob/3416b86da4b3288e87f827c7f2759a7fa18b5231/sdks/typescript/packages/core/src/generated/schemas.ts)
