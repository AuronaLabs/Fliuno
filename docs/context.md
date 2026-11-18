# 跨任务会话上下文

同一 `Session` 的后续任务默认接收此前已完成工作的有界历史。历史由 Fliuno 保存和选择，OpenAI、Anthropic 与 ACP 后端使用同一份中性记录。每个任务仍有自己的执行、审批和供应商续接状态。

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

## 冻结与幂等

任务创建时先选择历史，再将快照、任务、初始事件与幂等键一并提交。后续任务并发完成不会修改它，显式恢复、审批继续和服务重启也沿用原快照。预览是当前读取结果，不预留输入；预览之后可能有任务完成，因此提交时以返回的 `Task.context` 为准。

`clientRequestId` 绑定会话、当前输入、后端与上下文选项。相同请求返回原任务和原快照，不补入新历史、不重新执行。省略选项和显式默认选项对新任务等价；显式选择中，相同 ID 集合交换顺序仍等价。修改 mode、预算或 ID 集合时复用旧 key 会返回 -32009。更换供应商只能发生在新任务之间；同一任务中的供应商签名续接仍要求原供应商。

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

OpenAI 和 Anthropic 可通过两个宿主环境变量启用请求预算预检：

```powershell
$env:FLIUNO_CONTEXT_WINDOW_TOKENS = '32768'
$env:FLIUNO_MAX_OUTPUT_TOKENS = '2048'
```

数值仅用于说明。模型窗口须由宿主按实际模型配置；输出上限至少为 16，且必须小于窗口。启用时，同一输出上限会发给供应商（OpenAI `max_output_tokens`、Anthropic `max_tokens`）。两个变量都不设置时保持原有行为。

上限为窗口减去输出预留，另有 8 MiB 传输上限。`FLIUNO_CONTEXT_COUNT_MODE` 可选择：

- `local`（默认）：**1 个序列化 JSON 字节计 1 个预算单位**。它计入历史、当前任务消息、工具定义、工具结果和请求结构，不额外联网；多字节文本和转义可能让它提前拒绝供应商能接受的请求。
- `provider`：须同时配置窗口和输出上限。每步先向同一供应商地址发送一次计数请求：OpenAI `POST /v1/responses/input_tokens`，Anthropic `POST /v1/messages/count_tokens`。计数使用生成请求中的模型、系统提示、消息及工具字段，不发送仅生成用的字段；它会额外传输这些输入并增加一次往返。兼容服务必须实现对应端点。计数响应最多 16 KiB，操作最多 10 秒；超时、坏响应或超额均不会发送生成请求，也不会自动回退。供应商返回的计数不保证等于实际用量，窗口仍由宿主填写。

预检适用于每轮请求及流式模式。超额时当前生成步骤以已知未发送失败；此前已完成的工具操作仍保留。可在新任务用较小的 `maxBytes`、显式 `taskIds` 或 `isolated` 缩小历史。

## 持久化与迁移

历史索引在数据库 v5 引入，当前数据库版本为 v10。`storage/context` 的 `session_turns` 表保存完成记录投影，以 `(session_id,sequence)` 建索引；`task_id` 唯一，sequence 外键指向真实完成事件。投影与 TaskCompleted 及任务最终状态在同一事务提交，失败一起回滚；不会发布未提交的完成通知。

从 v4 及更早数据库升级时，迁移在事务中逐条回填已知完成的历史，不修改旧事件或任务输入。旧 `Task.context` 缺失时解码为 null；恢复旧任务仍没有新历史。旧幂等请求在未显式指定 context 时继续返回原任务，显式要求上下文选项则冲突。新任务可使用回填记录。

v9 加入可选的 `ContextOptions.taskIds` 并提升版本，阻止不理解新冻结输入的旧内核打开数据库。v8 到 v9 只更新版本号，不重写表、事件或快照；未选择任务时仍省略该字段，旧快照序列化形状保持一致。更早的 v6/v7/v8 兼容字段及注册绑定规则继续适用。

## 验证

```powershell
python examples/context_demo.py
cargo test -p fliuno-core -p fliuno-providers -p fliuno-app-server --all-features --locked
```

演示使用临时 SQLite 和本机确定性模型 fixture，检查实际 HTTP 请求中的历史、产物身份、幂等和重启，不调用真实模型；显式选择覆盖跳过超大最新记录、完成顺序、非法/跨会话/待审批/超预算拒绝、无失败写入，以及 ID 换序后的重启重试。Rust 测试另外覆盖完整快照边界、v8 到 v9 迁移、冻结、会话隔离、省略、事务失败回滚、v4 迁移、未知结果排除、跨供应商切换及当前任务续接。Windows AppContainer 进程测试覆盖模型与 ACP 双向历史、宿主文件操作数据、隔离模式，以及旧授权不能用于新操作。CI 已配置同类检查，远程执行结果须另行核验。
