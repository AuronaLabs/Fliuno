# 跨任务会话上下文

[文档中心](README.md) › [接入客户端](README.md#接入客户端) · [应用协议](protocol.md) · [模型配置](setup.md#真实模型)

同一 `Session` 的后续任务默认接收此前已完成工作的有界历史。历史由 Fliuno 保存和选择，OpenAI、Anthropic 与 ACP 后端使用同一份中性记录。每个任务仍有自己的执行、审批和供应商续接状态。

[创建任务](#协议与使用) · [读取历史](#会话历史分页) · [打开单条记录](#打开一条历史记录) · [冻结快照](#冻结与幂等) · [选择与预算](#选择预算与省略) · [持久化](#持久化与迁移)

## 协议与使用

`initialize.capabilities` 声明 `sessionContext:true`、`contextTaskSelection:true`、`maxContextTurns:32`、`maxContextBytes:262144`。客户端可先预览，再提交任务：

```json
{"jsonrpc":"2.0","id":2,"method":"session/context","params":{"sessionId":"<session-id>","options":{"mode":"session","maxTurns":16,"maxBytes":131072}}}
```

```json
{"jsonrpc":"2.0","id":3,"method":"task/start","params":{"sessionId":"<session-id>","input":"继续处理刚才的报告","clientRequestId":"follow-1","context":{"mode":"session","maxTurns":16,"maxBytes":131072}}}
```

`options`、`context` 和各选项字段均可省略。默认 `mode=session`、`maxTurns=16`、`maxBytes=131072`；有效范围分别为 1..32 和 1024..262144 UTF-8 字节。预算非法返回 -32602，缺少会话返回 -32004。

`Task.context` 与预览返回相同结构：

```json
{
  "options": {"mode":"session","maxTurns":16,"maxBytes":131072},
  "throughSequence": 9,
  "turns": [{
    "taskId": "task_example",
    "completionSequence": 9,
    "backend": {"type":"model"},
    "messages": [
      {"role":"user","text":"之前的任务"},
      {"role":"assistant","text":"已完成的输出"}
    ],
    "artifacts": [],
    "hostTools": []
  }],
  "omittedTurns": 0
}
```

`throughSequence` 是捕获时的全库事件水位，`completionSequence` 是来源任务的完成事件序号。记录只来自指定会话；水位可能因其他会话的事件而增长。`Task.messages` 只含当前任务消息；历史单独保存，不插入当前执行状态机。

`context:{"mode":"isolated"}` 表示本任务不读取历史，快照记录空 `turns` 和 `omittedTurns:0`，模型或 ACP 不收到历史块。该模式只控制输入选择；该任务完成后仍可成为同会话后续任务的历史来源。需要独立工作范围时创建新会话。

### 显式任务选择

在 `options` 或 `context` 中传 `taskIds`，可跳过无关或超大的最近记录：

```json
{"jsonrpc":"2.0","id":4,"method":"task/start","params":{"sessionId":"<session-id>","input":"继续这两项工作","clientRequestId":"selected-1","context":{"taskIds":["task_second","task_first"],"maxTurns":16,"maxBytes":131072}}}
```

`taskIds` 只能用于 `mode=session`，包含 1..maxTurns 个不同的 ID，每个为 1..128 可打印 ASCII 字节且不能含空格。每项必须是该会话、当前水位之前的已完成历史记录。不存在、其他会话或尚未完成的 ID 均返回相同的 -32602 诊断，不加载其他任务来解释原因。空列表和重复项也拒绝；需要空历史时使用 isolated。

内核将选项中的 ID 按字典顺序规范化，以稳定幂等请求身份；实际 `turns` 仍按完成事件正序排列，输入列表顺序不能重排历史。`omittedTurns` 统计该会话已完成但未选中的全部记录。指定记录的完整快照超出 `maxBytes` 时整体拒绝，不静默删除任何指定项；失败的创建不保存任务、事件或幂等键。省略 `taskIds` 保持默认的最近连续后缀选择。

### 按字节预算选择近期记录

客户端可以用 `contextFitRecent` 和 `maxContextCandidates` 发现该能力。在 `session/context.options` 或 `task/start.context` 中传入 `"selection":"fit_recent"` 后，内核从最新记录向前检查最多 128 条已完成记录。每条记录要能完整放入剩余字节预算才会入选。入选数达到 `maxTurns` 时停止，最终按完成顺序返回；`omittedTurns` 统计所有未入选的记录。

该策略仅用于 `mode=session` 且未指定 `taskIds` 的任务。默认策略 `recent` 保留完整的近期连续后缀。任务创建时冻结所用策略及结果。SQLite v13 支持读取带新策略的快照，升级仅更新数据库版本号。

### 按关键词选择近期历史

`contextMatchRecent:true` 表示客户端可以在预览的 `options` 或任务的 `context` 中传入：

```json
{"selection":"match_recent","historyQuery":{"query":"项目代号","role":"user"},"detail":"conversation","maxTurns":8,"maxBytes":32768}
```

查询去掉首尾空白后须为 1 至 256 个 UTF-8 字节。匹配使用与 `session/searchHistory` 相同的 Unicode 小写字面子串规则；省略 `role` 时同时查用户和助手文本。每次最多从新到旧检查 128 条，只扫描不超过 128 KiB 的精简投影。命中的完整记录能放入剩余预算才会入选，最终按完成顺序返回。工具和产物内容不能触发命中，`detail` 仍决定入选记录包含哪些字段。

仅限 `mode=session`，不能与 `taskIds` 并用。`omittedTurns` 包含未命中、超预算或扫描范围之外的已完成记录。预览和任务创建采用相同规则，快照冻结时保存去掉首尾空白的查询；相同请求键重试沿用原快照。SQLite v30 为新快照格式标记版本，不改写原有记录。

### 精简对话记录

`contextConversationDetail:true` 表示 `session/context.options` 和 `task/start.context` 支持 `"detail":"conversation"`。该选项保留用户和助手文本、完成序号、来源后端与产物引用，省略工具调用参数、工具结果和 ACP 宿主操作明细。内核先选择记录格式，再按 `maxBytes` 计算预算。`maxTurns`、`taskIds` 和 `selection` 的规则同样适用。省略 `detail` 即使用默认的 `full`。精简格式仅适用于 `mode=session`。

SQLite v22 从已完成记录回填精简投影。只有包含工具或宿主操作明细的记录需要额外存储投影。原始任务、事件和产物仍可单独查询，已创建任务继续使用各自冻结的上下文。

### 按事件序号预览历史

`contextAtSequence:true` 表示 `session/context` 支持顶层参数 `throughSequence`。传入预览返回的 `throughSequence` 和同一组选项，可以在后续任务完成后重新查看当时的历史。内核只统计该全局序号及之前完成的记录。`0` 对应空历史。超出当前已提交事件的序号返回 `-32602`。

## 会话历史分页

客户端可以用 `initialize.capabilities.sessionHistoryList` 发现 `session/listHistory`。接口按完成序号倒序读取指定会话的持久对话，包括已归档会话。每条 `kind:"turn"` 的 `turn` 对象包含用户和助手消息、来源后端、完成序号和产物引用，格式与 `detail:"conversation"` 一致。

```json
{"jsonrpc":"2.0","id":3,"method":"session/listHistory","params":{"sessionId":"<session-id>","limit":16,"maxBytes":65536}}
```

首次请求省略 `throughSequence`，服务端返回当时的全局事件水位。在下一页沿用它，并把 `nextBeforeSequence` 作为排他的 `beforeSequence`。后续完成的任务会出现在新的分页视图里。`nextBeforeSequence:null` 表示当前快照已经读完。在显式水位 `0` 时返回空历史。

`limit` 默认 16，范围 1 至 32。`maxBytes` 默认 64 KiB，范围 1 至 256 KiB，计算 `entries` 数组的紧凑 UTF-8 JSON 字节数，包括方括号和分隔符。能单独放进一页的记录会完整返回，剩余空间不足时留到下一页。如果单条记录超过整页预算，返回 `{"kind":"oversized","taskId":"…","completionSequence":123}`。客户端可以增大预算，在同一快照重新读取该页。所有记录都使用现有的完成投影和索引，历史分页与模型上下文选择是独立操作。

Rust 客户端可将 `SessionHistoryListParams` 交给 `fliuno_client::SessionHistory::new`，逐次调用 `next_page`。持久化已消费记录时一并保存 `next_request()`，重连后把保存的参数交给同一构造函数即可续读。客户端核对水位、完成顺序和续页游标，请求失败时保留原参数。`through()` 提供固定水位，可用于从该位置补读新提交的会话事件。

服务声明 `sessionHistoryAfterSequence: true` 时，`session/listHistory` 可用 `afterSequence` 排除指定水位及以前的完成记录。下界等于 `throughSequence` 时返回空页，大于它返回 `-32602`；续页的 `beforeSequence` 必须大于下界，且不超过 `throughSequence`。读取后续页时上下界都保持不变。

### 订阅会话历史

服务声明 `sessionHistorySubscription: true` 时，一次调用 `session/subscribeHistory` 即可读取历史首页并切换本连接的会话事件水位订阅。它支持 `sessionId`、`afterSequence`、`limit`、`maxBytes`，返回与 `session/listHistory` 相同的页结构。服务在同一个提交锁下读取首页并挂接订阅；参数不合法或会话不存在时不切换。收到响应前的通知仍属于旧订阅。

```json
{"jsonrpc":"2.0","id":6,"method":"session/subscribeHistory","params":{"sessionId":"<session-id>","afterSequence":320,"limit":16}}
```

Rust 的 `Client::subscribe_session_history` 返回 `SessionHistorySubscription { page, history, events }`。用 `history.next_page()` 读完固定的历史窗口；把之后收到的通知交给 `events.observe()`，再用 `events.next_page()` 从 `page.throughSequence` 之后补读事件，包括 `task_completed`。实时事件的消费游标与历史续页参数分别保存；只需要补齐对话记录时，可以用上个完整历史窗口的 `throughSequence` 作为下次订阅的 `afterSequence`。旧服务可分开读历史并[从快照水位补读会话事件](protocol.md#实时预览与事件通知)。

## 打开一条历史记录

按关键词查找时，先用 [`session/searchHistory` 或 `workspace/searchHistory`](protocol.md#查找历史任务) 获得任务 ID 和完成序号。服务声明 `sessionHistoryGet` 后，客户端可以按命中直接读取对话，无需翻过前面的历史页。 稀疏搜索可按服务能力调大 `scanLimit`，减少翻过空页的往返；小窗口也可限制单次扫描，原游标仍能续用。

```json
{"jsonrpc":"2.0","id":4,"method":"session/getHistoryTurn","params":{"sessionId":"<session-id>","taskId":"<task-id>","throughSequence":320,"maxBytes":65536}}
```

`throughSequence` 沿用原搜索页的水位，省略时使用当前水位。`maxBytes` 默认 64 KiB，范围 1 至 256 KiB，计算单个 `SessionHistoryEntry` 的紧凑 JSON 字节数。记录太大时返回 `kind:"oversized"` 和原任务引用。可增大预算，在同一水位重新读取。记录包含产物引用，下载正文仍使用产物接口。

其他会话的记录、未完成的任务、缺失项和水位之后的记录统一返回 `-32004`；超出当前水位或字节预算范围则返回 `-32602`。读取复用持久对话投影和任务索引，无需数据库迁移。

## 冻结与幂等

任务创建时先选择历史，再将快照、任务、初始事件与幂等键一并提交。后续任务并发完成不会修改它，显式恢复、审批继续和服务重启也沿用原快照。预览是当前读取结果，不预留输入；预览之后可能有任务完成，因此提交时以返回的 `Task.context` 为准。

`clientRequestId` 绑定会话、当前输入、后端与上下文选项。相同请求返回原任务和原快照，不补入新历史、不重新执行。省略选项和显式默认选项对新任务等价；显式选择中，相同 ID 集合交换顺序仍等价。修改模式、预算、记录格式或 ID 集合时复用旧 key 会返回 -32009。更换供应商只能发生在新任务之间；同一任务中的供应商签名续接仍要求原供应商。

SQLite v14 从已完成记录回填每个会话的计数和最后完成序号。后续完成记录、计数与对应事件在同一事务中提交。读取当前历史时只需查会话计数，按更早的游标读取时仍按完成序号精确计数。

## 历史内容与执行边界

每条历史对应一个已知完成的任务，保留：

- 用户和助手文本、工具调用参数与已保存的工具结果。
- 产物 ID、标题和媒体类型；不会为了构建上下文自动读取全部产物正文。工具结果本身可能包含正文数据。
- 来源后端以及 ACP 宿主文件操作的调用、状态和已保存结果。

投影不包含审批对象、授权通道、供应商 `ProviderReplay`、签名思考块或来源任务继承的 `context`。运行中、等待审批、失败、取消、中断、模型/工具/Agent 结果未知的任务均不进入索引。ACP 未完成运行、待审权限或仍在途的宿主操作也不能投影为已完成历史。

模型适配器将快照编码成一条注明“历史数据”的 user 文本，放在当前输入之前。历史工具调用只存在于这段 JSON 文本中，不生成原生 `function_call`、`tool_use` 或当前工具回传；当前任务内部仍精确保留供应商要求的续接块。ACP 通过两个 TextContent 分别接收历史和当前输入，继续为每个任务启动新进程与新会话。这不是 ACP `session/load` 或供应商托管会话恢复。

提示说明要求把历史当作数据，不能单靠文字保证模型不会误解。实际执行仍只接受当前任务中新提出的注册工具调用，并经过当前策略和新的逐操作审批；历史调用 ID、旧审批和旧宿主状态不能授权新副作用。文件与工具结果可能已经过时，后续操作应重新读取并复核基线。

## 选择、预算与省略

未传 `taskIds` 时，按完成事件顺序选择最近的连续完整任务后缀，并按时间正序发送。任务数量上限与整个序列化 `TaskContext` 的 UTF-8 字节预算同时生效；不会切断工具调用和结果，也不会把一条超大记录截成不完整 JSON。显式选择也使用同一完整快照预算，包含 `options.taskIds` 等元数据。`omittedTurns` 精确统计该会话已完成但未选入的记录数。

默认选择中，最新任务单条超预算时，结果为空并报告省略数量；不跳过它改选更早记录。客户端可显式指定更早的记录。即使全部历史被省略，后端仍接收含 `omittedTurns` 的数据块。当前没有模型摘要、语义压缩或检索式选择。

SQLite 在读取候选时先检查记录字节数，最多检查 `maxTurns` 条、每条不超过 `maxBytes`。默认选择从新到旧逐条读取，第一次超额即停止，并用单次 JSON 计量精确计算完整快照长度。显式选择另外检查累计原始记录字节数和最终快照大小。Task 快照、事件、数据库页与 SDK 缓冲都可能重复占用空间，因此快照预算不等于进程总内存上限。

历史提示说明前缀不计入 `maxBytes`。ACP 当前输入单独最多 256 KiB，历史文本块最多 257 KiB；完整序列化 PromptRequest 还须不超过 1 MiB 减 1024 字节，为 RPC 信封预留空间。JSON 控制字符转义也参与该编码预算；超额在占用活动轮次或发送前拒绝，不裁切输入。

## 模型请求预算

这里的预算限制**单次请求**的输入与输出。要限制一个任务可以推进多少轮，提交时设置 [`maxModelSteps`](protocol.md#模型尝试次数)。

OpenAI、Anthropic 和 Gemini 可通过两个宿主环境变量启用请求预算预检：

```powershell
$env:FLIUNO_CONTEXT_WINDOW_TOKENS = '32768'
$env:FLIUNO_MAX_OUTPUT_TOKENS = '2048'
```

数值仅用于说明。模型窗口须由宿主按实际模型配置；输出上限至少为 16，且必须小于窗口。启用时，同一输出上限会发给供应商（OpenAI `max_output_tokens`、Anthropic `max_tokens`、Gemini `generation_config.max_output_tokens`）。两个变量都不设置时保持原有行为。

上限为窗口减去输出预留，另有 8 MiB 传输上限。`FLIUNO_CONTEXT_COUNT_MODE` 可选择：

- `local`（默认）：**1 个序列化 JSON 字节计 1 个预算单位**。它计入历史、当前任务消息、工具定义、工具结果和请求结构，不额外联网；多字节文本和转义可能让它提前拒绝供应商能接受的请求。
- `provider`：OpenAI 和 Anthropic 须同时配置窗口和输出上限。每步先向同一供应商地址发送一次计数请求：OpenAI `POST /v1/responses/input_tokens`，Anthropic `POST /v1/messages/count_tokens`。计数使用生成请求中的模型、系统提示、消息及工具字段，不发送仅生成用的字段；它会额外传输这些输入并增加一次往返。兼容服务必须实现对应端点。计数响应最多 16 KiB，操作最多 10 秒；超时、坏响应或超额均不会发送生成请求，也不会自动回退。供应商返回的计数不保证等于实际用量，窗口仍由宿主填写。Gemini Interactions 暂无预检 token 计数适配，启动时拒绝 `provider` 计数模式。

预检适用于每轮请求及流式模式。超额时当前生成步骤以已知未发送失败；此前已完成的工具操作仍保留。可在新任务用较小的 `maxBytes`、显式 `taskIds` 或 `isolated` 缩小历史。

`initialize.capabilities.modelBudget` 返回当前的 `windowTokens`、`maxInputTokens`、`maxOutputTokens`、`countMode` 和 `source`。计数模式为 `json_bytes_estimate` 或 `provider`，来源为 `configured` 或 `provider_metadata`。未配置预检时返回 `null`。

Anthropic 可以设置 `FLIUNO_CONTEXT_WINDOW_TOKENS=provider`，并继续填写 `FLIUNO_MAX_OUTPUT_TOKENS`。app-server 启动时用现有凭据向配置端点请求一次 `GET /v1/models/{model_id}`，限时 5 秒、最多 64 KiB。内核以返回的 `max_input_tokens` 为窗口，再预留宿主配置的输出 token，并核对该输出值是否超过返回的 `max_tokens`。若配置的是别名，则用返回的模型 ID 进行本次进程的计数与生成。探测失败时启动终止，SQLite 尚未打开。OpenAI 和 Gemini 使用数字窗口，不进行供应商自动窗口探测。

窗口来源和输入计数可独立配置。Anthropic 的窗口探测可配合任一受支持的计数模式；OpenAI 使用数字窗口配置，Gemini 目前仅使用数字窗口和本地估算。

## 持久化与迁移

历史索引在数据库 v5 引入。`storage/context` 的 `session_turns` 表保存完成记录投影，以 `(session_id,sequence)` 建索引；`task_id` 唯一，sequence 外键指向真实完成事件。投影与 TaskCompleted 及任务最终状态在同一事务提交，失败一起回滚；不会发布未提交的完成通知。

从 v4 及更早数据库升级时，迁移在事务中逐条回填已知完成的历史，不修改旧事件或任务输入。旧 `Task.context` 缺失时解码为 null；恢复旧任务仍没有新历史。旧幂等请求在未显式指定 context 时继续返回原任务，显式要求上下文选项则冲突。新任务可使用回填记录。

v9 加入可选的 `ContextOptions.taskIds` 并提升版本，阻止不理解新冻结输入的旧内核打开数据库。v8 到 v9 只更新版本号，不重写表、事件或快照；未选择任务时仍省略该字段，旧快照序列化形状保持一致。更早的 v6/v7/v8 兼容字段及注册绑定规则继续适用。

## 验证

```powershell
python examples/context_demo.py
cargo test -p fliuno-core -p fliuno-providers -p fliuno-app-server --all-features --locked
```

演示使用临时 SQLite 和本机确定性模型 fixture，检查实际 HTTP 请求中的历史、产物身份、幂等和重启，不调用真实模型；显式选择覆盖跳过超大最新记录、完成顺序、非法/跨会话/待审批/超预算拒绝、无失败写入，以及 ID 换序后的重启重试。Rust 测试另外覆盖完整快照边界、v8 到 v9 迁移、冻结、会话隔离、省略、事务失败回滚、v4 迁移、未知结果排除、跨供应商切换及当前任务续接。Windows AppContainer 进程测试覆盖模型与 ACP 双向历史、宿主文件操作数据、隔离模式，以及旧授权不能用于新操作。CI 已配置同类检查，远程执行结果须另行核验。
