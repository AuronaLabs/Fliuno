# AG-UI 事件接入

[文档首页](README.md) · [应用协议](protocol.md#rust-调用方)

Rust 客户端的 `ag_ui::AgUiProjector` 把 Fliuno 单个任务的持久事件转换成 AG-UI 事件对象。调用方可以将对象序列化后交给 AG-UI 消费端。启动任务和处理审批仍通过 Fliuno JSON-RPC 完成。

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

当前适配覆盖持久的生命周期、完成文本和轻量引用。进度预览、宿主工具参数、远端原始载荷、AG-UI 的执行输入与中断恢复契约分别由调用方按各自协议处理。

## SSE 输出与续传

`fliuno_client::ag_ui::AgUiSseStream` 可以把投影写入实现 `Write` 的响应体。由宿主提供 HTTP 路由和连接，响应使用 `AG_UI_SSE_CONTENT_TYPE`（`text/event-stream`）。每条消息有一行 `id` 和一行 `data`。`data` 是 AG-UI 事件 JSON，`id` 由任务 ID、内核事件序号和该事件内从 1 开始的消息编号组成。

重连时，用 `AgUiSseStream::new(&summary, last_event_id)` 读取消费端发送的 `Last-Event-ID`。将上文回放循环中的 `projection.project(event)` 换成 `stream.write_event(event, &mut response_body)`。和首次连接一样从 `TaskStarted` 回放，以重建执行段状态。输出从指定消息之后开始，即使它位于一个任务事件产生的多条消息中间。

历史补读完成后，调用 `stream.finish_replay()` 确认找到了续传位置，再把暂存的实时事件按序写入同一个流。找不到 ID、任务不匹配或写入中断会返回错误。写入中断后，消费端用最后一条完整收到的 SSE ID 建立新流。SSE 流接收已提交事件，任务提交和审批通过本机应用协议完成。

事件字段对照了 AG-UI 官方规范和官方 TypeScript Schema，固定在提交 `3416b86da4b3288e87f827c7f2759a7fa18b5231`。[事件规范](https://github.com/ag-ui-protocol/ag-ui/blob/3416b86da4b3288e87f827c7f2759a7fa18b5231/docs/concepts/events.mdx) · [SDK Schema](https://github.com/ag-ui-protocol/ag-ui/blob/3416b86da4b3288e87f827c7f2759a7fa18b5231/sdks/typescript/packages/core/src/generated/schemas.ts)
