# AG-UI 接入

[文档首页](README.md) · [应用协议](protocol.md#rust-调用方)

宿主可以通过 Rust 客户端提交文本任务，并把已提交的任务事件转换成 AG-UI 消息。宿主管理 HTTP 路由和会话授权，审批通过 Fliuno 应用协议处理。

[提交文本任务](#提交文本任务) · [投影内核事件](#投影内核事件) · [SSE 输出与续传](#sse-输出与续传)

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
| 任务失败、中断或取消 | `RUN_ERROR`。先中断再取消的任务已结束前一个执行段，取消另发 `fliuno.task_cancelled`。 |
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

### 帧格式和底层接口

`fliuno_client::ag_ui::AgUiSseStream` 可以把单个持久事件的投影写入实现 `Write` 的响应体。由宿主提供 HTTP 路由和连接，响应使用 `AG_UI_SSE_CONTENT_TYPE`（`text/event-stream`）。每条消息有一行 `id` 和一行 `data`。`data` 是 AG-UI 事件 JSON，`id` 由任务 ID、内核事件序号和该事件内从 1 开始的消息编号组成。

重连时，读取客户端发送的 `Last-Event-ID`。通过 AG-UI 提交的任务用 `run.sse(last_event_id)` 创建输出流。直接投影内核任务时，使用 `AgUiSseStream::new(&summary, last_event_id)`。将上文回放循环中的 `projection.project(event)` 换成 `stream.write_event(event, &mut response_body)`。和首次连接一样从 `TaskStarted` 回放，以重建执行段状态。输出从指定消息之后开始，即使它位于一个任务事件产生的多条消息中间。

历史补读完成后，调用 `stream.finish_replay()` 确认找到了续传位置，再把暂存的实时事件按序写入同一个流。找不到 ID、任务不匹配或写入中断会返回错误。写入中断后，消费端用最后一条完整收到的 SSE ID 建立新流。SSE 流输出已提交事件。

事件字段对照了 AG-UI 官方规范和官方 TypeScript Schema，固定在提交 `3416b86da4b3288e87f827c7f2759a7fa18b5231`。[事件规范](https://github.com/ag-ui-protocol/ag-ui/blob/3416b86da4b3288e87f827c7f2759a7fa18b5231/docs/concepts/events.mdx) · [SDK Schema](https://github.com/ag-ui-protocol/ag-ui/blob/3416b86da4b3288e87f827c7f2759a7fa18b5231/sdks/typescript/packages/core/src/generated/schemas.ts)
