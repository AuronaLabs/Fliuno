# AG-UI 接入

[文档中心](README.md) › [接入客户端](README.md#接入客户端) · [应用协议](protocol.md#rust-调用方)

宿主可以通过 Rust 客户端提交文本、内嵌图片、PDF 或同一会话的交付物引用，并把已提交的任务事件转换成 AG-UI 消息。`fliuno-ag-ui-http` 提供可嵌入的 HTTP 路由，宿主负责监听地址和会话授权。审批可以由宿主通过应用协议处理，也可以挂载独立授权的审批入口。

| 要做什么 | 对应步骤 |
| --- | --- |
| **挂载 HTTP 接口** | [路由与宿主授权](#http-接入) · [宿主模型尝试上限](#宿主模型尝试上限) · [离线示例](../examples/ag_ui_http_demo.py) |
| **运行任务并展示进度** | [提交文本任务](#提交文本任务) · [提交内嵌图片](#提交内嵌图片) · [提交内嵌 PDF](#提交内嵌-pdf) · [复用交付物](#引用已有交付物) · [投影内核事件](#投影内核事件) |
| **恢复运行** | [SSE 输出与续传](#sse-输出与续传) · [审批中断与续跑](#审批中断与续跑) |

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
| `messages` | 接受 `user` 或 `assistant` 消息，最后一条必须是 `user`。内核使用最后一条消息和本地会话历史。正文支持最多 256 KiB 的文本，也支持[内嵌图片](#提交内嵌图片)和 [PDF](#提交内嵌-pdf)。 |
| `protocolVersion` | 可省略。提供时使用 `1.0`。 |
| 其余输入 | JSON 正文最多 1 MiB。`state` 可省略、设为 `null` 或空对象。`forwardedProps` 也接受这些值，并可用 Fliuno 扩展[引用已有交付物](#引用已有交付物)。`tools`、`context` 和 `resume` 可省略或使用空数组。当前不支持 `parentRunId`、工具调用、加密消息或其他媒体。 |

用返回值的 `task()` 取得已创建的任务摘要。连接中断后，可以调用 `Client::attach_ag_ui_run(session_id, run_id, on_notification)`，从数据库找回同一个任务。用 `run.subscribe_sse(...)` 输出事件，即可沿用首次执行的 `runId`。后续显式恢复的执行段仍使用各自派生的 ID。

### 提交内嵌图片

`Client::start_ag_ui_run`、`Client::start_ag_ui_input_run` 和 `POST /run` 接受 AG-UI 图片片段。最后一条用户消息的 `content` 可以混排文字和图片，也可以只含图片。使用 `source.type: "data"`，将 PNG、JPEG 或 WebP 通过标准 Base64 放进 `value`，并在 `mimeType` 声明格式。

```json
[
  {"type":"text","text":"请描述这张图片"},
  {"type":"image","source":{"type":"data","mimeType":"image/png","value":"iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+yhE0AAAAASUVORK5CYII="}}
]
```

单条混合消息最多 32 个片段。图片和 PDF 合计最多 4 个，Base64 总长最多 512 KiB。服务会拒绝 URL 和文件句柄。图片随任务保存在本机数据库，由配置的模型服务接收。任务摘要和会话历史使用 `[image]` 占位符。重试时沿用原运行 ID 和原来的有序片段。

标准的会话授权也适用于图片提交。审批续跑仍要求独立的审批授权和空的 `messages`。Rust 客户端会先检查 `taskImageInput` 能力。

### 提交内嵌 PDF

在同一个 `content` 数组中加入 `{"type":"document","source":{"type":"data","mimeType":"application/pdf","value":"…"}}`。`value` 是 PDF 文件的标准 Base64 编码。可以和文本、图片混排，也可单独提交 PDF。Rust 客户端会检查 `taskPdfInput` 能力。PDF 字节保留在原任务，任务摘要和会话历史使用 `[document]` 占位符。模型必须支持 PDF 输入。

运行以下离线演示可提交一张图片和一份 PDF，审批后按帧续传。演示使用本机模型夹具，在审批参数中显示媒体占位符。

```sh
python examples/ag_ui_http_demo.py --interrupts --inline-media
```

### 引用已有交付物

宿主可调用 `RouterBuilder::new(client, authorize).artifact_inputs().build()`，允许 `POST /run` 使用 `forwardedProps.fliuno.artifactInputs`。Rust 客户端的 `start_ag_ui_run` 和 `start_ag_ui_input_run` 也识别这个扩展，并检查服务是否声明 `taskArtifactInput`。

```json
{
  "threadId": "session_example",
  "runId": "review-report",
  "messages": [{"id":"msg-1","role":"user","content":"检查报告中的数字"}],
  "forwardedProps": {
    "fliuno": {
      "artifactInputs": [{"taskId":"task_source","artifactId":"artifact_report"}]
    }
  }
}
```

内核只接受授权会话内已经提交的 PNG、JPEG、WebP 或 PDF，并按引用顺序附在新消息之后。只提交交付物时，最后一条用户消息的 `content` 可以设为 `""`。内嵌媒体与引用共用任务输入预算。重试时沿用同一 `runId`、原消息与有序引用。可从交付物目录取得所需 ID。[应用协议](protocol.md#将交付物用作任务输入)

扩展只适用于初始运行，审批续跑请求携带审批决定。引用与模型供应商的文件句柄使用独立字段。宿主未启用扩展或输入超限时返回 `400`，引用不属于授权会话或不存在时返回 `404`，同一运行 ID 的输入发生冲突时返回 `409`。内核缺少 `taskArtifactInput` 能力时，HTTP 返回 `502`。

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

上表是默认文本模式的行为。[审批中断模式](#审批中断与续跑)会在提交工具审批请求时结束本次执行段。进度预览、宿主工具参数和远端原始载荷通过各自的协议处理。

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

宿主需要服务多个读取方时，可以用 `client.event_watermark` 读一次已提交水位，再为每个任务建立 `run.replay_sse(through, last_event_id)`。各个 `AgUiSseReplay` 共用 stdio 连接，但各自保存投影和游标。`next_chunk` 每次读取并编码一页，由宿主释放客户端借用后再将字节写给消费者。服务声明 `taskEventWatermarks` 时，宿主可用 `client.task_event_watermarks` 批量读取各任务水位，只推进有新事件的读取方。

读完窗口后，用 `advance_through` 接入新的已提交水位。宿主可以查询水位，或从自己维护的实时通知中得到水位。每个读取方只读所属任务，回放和水位查询都保留现有实时订阅。

### 帧格式和底层接口

`fliuno_client::ag_ui::AgUiSseStream` 可以把单个持久事件的投影写入实现 `Write` 的响应体。由宿主提供 HTTP 路由和连接，响应使用 `AG_UI_SSE_CONTENT_TYPE`（`text/event-stream`）。每条消息有一行 `id` 和一行 `data`。`data` 是 AG-UI 事件 JSON，`id` 由任务 ID、内核事件序号和该事件内从 1 开始的消息编号组成。

重连时，读取客户端发送的 `Last-Event-ID`。通过 AG-UI 提交的任务用 `run.sse(last_event_id)` 创建输出流。直接投影内核任务时，使用 `AgUiSseStream::new(&summary, last_event_id)`。将上文回放循环中的 `projection.project(event)` 换成 `stream.write_event(event, &mut response_body)`。和首次连接一样从 `TaskStarted` 回放，以重建执行段状态。输出从指定消息之后开始，即使它位于一个任务事件产生的多条消息中间。

历史补读完成后，调用 `stream.finish_replay()` 确认找到了续传位置，再把暂存的实时事件按序写入同一个流。找不到 ID、任务不匹配或写入中断会返回错误。写入中断后，消费端用最后一条完整收到的 SSE ID 建立新流。SSE 流输出已提交事件。

## HTTP 接入

在仓库根目录运行 `python examples/ag_ui_http_demo.py`。演示启动 Rust 宿主和内置模型，分别验证任务提交与审批身份，再从一个 SSE 帧续传。宿主使用临时数据库和本机回环地址。

Rust 宿主演示位于 [`local_host.rs`](../crates/fliuno-ag-ui-http/examples/local_host.rs)，可以查看如何启动 app-server、连接 stdio 和挂载路由。

`fliuno_ag_ui_http::router` 返回可挂载到 Axum 的路由。传入 `Arc<tokio::sync::Mutex<Client<_, _>>>` 及授权回调。回调根据请求头返回授权的本机会话 ID，返回 `None` 时请求在调用内核前被拒绝。宿主可以在两次 SSE 页读取之间通过同一个客户端处理任务审批。

| 请求 | 行为 |
| --- | --- |
| `POST /run` | 接收 `application/json` 的任务请求，要求 `threadId` 与授权会话一致。返回 `text/event-stream`。 |
| `GET /runs/{run_id}` | 在授权会话中找回运行。可发送 `Last-Event-ID`，从指定帧之后续传。不存在或不属于该会话的运行返回 `404`。 |
| `DELETE /runs/{run_id}` | 取消授权会话中的运行，成功返回 `204`。重复取消仍返回 `204`。已经完成或失败的任务返回 `409`。 |

路由每次读取一页持久事件，释放客户端后再把编码好的页交给响应体。当前限制同时 16 条响应或待解析请求，POST 正文最多 1 MiB，读取超时 10 秒。重连先用全局水位校验恢复位置。空闲响应共享约每 150 毫秒一次的批量任务水位查询，同一任务只查询一次，无关任务的事件不会触发空闲流读页。旧服务继续使用共享的全局水位查询。空闲时定期发送 SSE 保活注释，执行段结束后关闭响应。

### HTTP 审批

用 `router_with_approvals` 挂载审批入口。第二个授权回调根据请求头确定审批者及其可审阅的本机会话。通过任务提交授权的身份仍需单独通过审批授权。

| 请求 | 行为 |
| --- | --- |
| `GET /runs/{run_id}/approvals/{approval_id}` | 读取绑定到该任务的待审批工具及参数。 |
| `POST /runs/{run_id}/approvals/{approval_id}` | 提交 `{"decision":"allow_once"}` 或 `{"decision":"deny"}`，返回持久决定事件。 |

审批 ID 来自 SSE 的 `fliuno.approval_requested` 事件。重复提交同一决定返回原事件，提交相反决定返回 `409`。属于其他任务或会话的审批返回 `404`。宿主可以在响应保持打开时处理审批，审批入口有独立的并发名额，并通过 `approval/respondDecision` 提交决定。

### 宿主模型尝试上限

宿主可为一个路由设置 [每任务模型尝试上限](protocol.md#模型尝试次数)。下面的 Rust 代码给新任务最多两次尝试；`client` 是已经连接的共享客户端，`authorize` 和 `reviewer` 分别验证提交者和审批者。

```rust
use fliuno_ag_ui_http::RouterBuilder;
use fliuno_client::ag_ui::AgUiModelStepLimit;

let router = RouterBuilder::new(client, authorize)
    .model_step_limit(AgUiModelStepLimit::new(2)?)
    .approvals(reviewer)
    .build();
```

需要审批中断时改用 `.interrupts(reviewer)`。直接通过 Rust 客户端接入时，可用 `AgUiRunOptions::model_step_limit` 与 `Client::start_ag_ui_text_run`。不配置时沿用服务默认值。已配置限额而服务未声明支持时，HTTP 返回 `502`，不会降级为不限额提交。HTTP 请求体不能设置 `maxModelSteps`。

限额与任务一起保存。审批续跑和 SSE 重连沿用该任务，无论宿主重启后设置了什么新上限。用同一个根 `runId` 重新提交时，如果有效上限已经改变，返回 `409`；通过 `GET /runs/{run_id}` 可继续读取原运行。最后一次模型响应提议的工具获批后仍可执行，若之后还需要模型则 SSE 输出 `RUN_ERROR`，已保存的交付物仍可读取。

### 审批中断与续跑

新宿主可选择 `router_with_interrupts(client, authorize, reviewer)`。需要 app-server 声明 `approvalResumeRun` 和 `approvalRunPending`。现有 `router_with_approvals` 继续使用上一节的行为；两个模式的运行 ID 各自查找。

1. 提交者向 `POST /run` 提交任务。需要审批时，父流在已提交的审批请求处输出 `RUN_FINISHED`，随后关闭。`outcome` 中含有一项 `interrupts`；详情（包括工具参数）仍通过审批者专用的 `GET /runs/{run_id}/approvals/{approval_id}` 读取。运行 ID 必须是发出该审批的执行段；已处理的审批和其他执行段的 ID 都返回 `404`。
2. 审批者向 `POST /run` 发送下方 JSON。`threadId` 必须匹配其授权的会话，`parentRunId` 引用刚结束的执行段，`runId` 是新的 ID。每次续跑只接受一个中断答复，`messages` 为空。
3. 响应从子运行的 `RUN_STARTED` 开始，继续同一内核任务。子运行若再次需要审批，可以用它作为下一次续跑的父运行。每条 SSE 响应最多包含自己的执行段，重连时使用对应的 `runId` 与 `Last-Event-ID`。

```json
{
  "threadId": "local-session-id",
  "runId": "review-002",
  "parentRunId": "request-001",
  "messages": [],
  "resume": [{
    "interruptId": "approval-id-from-outcome",
    "status": "resolved",
    "payload": { "decision": "allow_once" }
  }]
}
```

拒绝可使用 `decision: "deny"`，或使用 `status: "cancelled"` 并省略 `payload`。拒绝会留下审批决定，子运行以 `RUN_ERROR` 结束。`POST` 审批详情路由在这个模式下不开放。审批者可以读取本会话的 SSE，但只持有审批权限不能创建任务或取消运行。

同一个子 `runId` 重试相同答复时返回同一执行段，重启后也不会重新执行工具。重复 ID 换成不同父运行、审批或决定时返回 `409`。把一个执行段的 `Last-Event-ID` 传给另一个执行段返回 `400`。子运行接管后，通过父 ID 取消任务返回 `409`。

审批中断模式的 `GET /runs/{run_id}` 通过 `Client::attach_ag_ui_interrupt_replay` 建立回放。服务声明 `approvalRunReplay` 时，一次 `approval/getRunReplay` 返回执行段边界和已提交水位。已结束的执行段以本段最后一个事件为上限；仍在运行的执行段以该任务最新提交的事件为起始水位，之后可继续跟踪新的提交。有 `Last-Event-ID` 时从游标所在事件开始读取，并验证具体帧编号；无游标时从本段首事件读取。越界或指向另一执行段的游标返回 `400`。

旧服务仍读取 `approval/getRun` 和全局水位；若声明 `approvalRunWindow` 且有游标，再读取执行段边界。默认连续文本模式仍从全局水位开始，并从任务开头重建投影。

Rust 宿主可先用 `AgUiRunInput::parse` 解码请求，再调用 `start_ag_ui_interrupt_run` 或 `resume_ag_ui_run`。后者应在宿主独立验证审批身份后调用。只需读取运行绑定时，可用 `attach_ag_ui_interrupt_run`。底层手动投影子运行时，应先喂入 `approval/getRun` 返回的 `startSequence` 对应的事件；`replay_sse` 和 `subscribe_sse` 会自动定位。

```sh
python examples/ag_ui_http_demo.py --interrupts
```

事件字段对照了 AG-UI 官方规范和官方 TypeScript Schema，固定在提交 `3416b86da4b3288e87f827c7f2759a7fa18b5231`。[事件规范](https://github.com/ag-ui-protocol/ag-ui/blob/3416b86da4b3288e87f827c7f2759a7fa18b5231/docs/concepts/events.mdx) · [SDK Schema](https://github.com/ag-ui-protocol/ag-ui/blob/3416b86da4b3288e87f827c7f2759a7fa18b5231/sdks/typescript/packages/core/src/generated/schemas.ts)
