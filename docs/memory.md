# 工作区长期记忆

[文档中心](README.md) › [开发路线图](roadmap.md) · [应用协议](protocol.md#工作区长期记忆)

工作区记忆是由宿主或上层 Agent 显式读写的持久文本记录。它按 workspace 隔离，并独立于会话历史。创建新任务时，内核会用用户输入执行有界词项检索，也可以使用宿主显式配置的语义检索。匹配项冻结进任务上下文快照；后续编辑或删除不会改写历史快照，也不会改变工具或文件访问权限。

## 数据边界

- 每个工作区最多保存 4096 条记录，包含已删除的 tombstone。
- 每条记录由工作区内唯一的 `key` 标识，正文最多 16 KiB UTF-8，来源可指向同工作区的已完成任务。
- 更新与恢复使用乐观并发：调用方必须提交当前 `expectedRevision`。删除同样检查 revision，并保留正文、来源和删除时间。
- 关键词搜索是 key/正文中的区分大小写字面子串匹配。被删除的记录不参与搜索。
- 创建、更新、恢复、删除、单条读取、列表、搜索及任务上下文访问都会进入独立 SQLite 审计表。审计只保留正文哈希，不复制正文；读取、列表和搜索由单条聚合记录表示，审计查询本身不会继续产生日志。任务创建时的访问审计与任务及首事件在同一事务提交，失败时不会留下孤立审计记录。

## 任务上下文

- 只检索当前会话所属 workspace 中未删除的记忆，最多检查最近 4096 条候选记录。
- 以输入和记忆 key/正文的词项重叠计分，额外识别有词边界的完整 key（例如 `R`）。中文、日文和韩文按相邻双字提取词项。开启语义检索后，达标的向量结果按排名与词项结果融合。
- 每个任务最多冻结 8 条、合计最多 16 KiB 正文；每条还必须能放入 `context.maxBytes` 的完整序列化预算。超预算项会跳过，继续检查后续候选。
- `context.disableMemory:true` 保留会话历史但关闭本次记忆检索；隔离上下文也关闭检索。无匹配时省略 `memories`。
- 冻结项包含记忆 ID、key、正文、revision、可选来源任务，以及 `matchedBy`（`keyword`、`semantic` 或 `both`）。语义检索回退时，如预算允许，快照含 `memoryFallback`。旧快照可能没有这些字段。上下文提示将记忆标为不可信的参考资料。
- 审计 action 使用 `context_searched` 表示本次检索没有注入记忆（无匹配或候选都超出预算），使用 `context_injected` 逐条记录已注入记录及其正文哈希和发起任务。

## RPC

| 方法 | 作用 |
| --- | --- |
| `workspace/memory/put` | 创建、更新或恢复；更新/恢复要求当前 revision |
| `workspace/memory/get` | 按 workspace 和 key 读取活动记录 |
| `workspace/memory/list` | 游标分页；可选择包含已删除项 |
| `workspace/memory/search` | 对 key 和正文做字面关键词检索 |
| `workspace/memory/indexVectors` | 按当前修订号批量保存派生向量 |
| `workspace/memory/semanticSearch` | 余弦相似度检索，可退回字面关键词 |
| `workspace/memory/delete` | 按当前 revision 软删除 |
| `workspace/memory/listAudit` | 游标分页读取操作和访问记录 |

任务来源只能引用同一工作区内已完成的任务。可选 `actorTaskId` 同样必须属于该工作区，用于关联读取和搜索审计。错误回包不会包含记忆正文。

## 重建与查询语义索引

宿主先用 `workspace/memory/list` 读取当前记忆，再用自行选择的嵌入模型生成向量，以 `{workspaceId, modelIdentity, vectors:[{memoryId, revision, values}]}` 写入。每批最多 32 条、每条最多 2048 维。一批中任一项的修订号过期、已删除或不属于工作区，整批回滚。换模型或预处理方案时使用新的 `modelIdentity`；每条原始记录至多保留当前的一条向量。重建时重新读原始记忆并写入同一批接口即可。

查询提交 `{workspaceId, query, limit, embedding:{modelIdentity, values}}`，同一工作区内按余弦相似度返回最多 16 条当前修订号的原始记忆。模型不可用时省略 `embedding`；缺少兼容索引、索引校验失败或没有正分项时也会使用 `query` 查询原始记录，并通过 `mode` 和 `fallbackReason` 说明实际路径。修改或删除记忆会作废旧向量，已冻结的任务上下文不受影响。

## 自动索引与任务检索

默认使用本机词项检索。宿主可通过 `--memory-embedding-config PATH` 指定 OpenAI 兼容的 Embedding 端点与工作区 ID。先通过 `workspace/open` 或 `workspace/list` 取得真实 ID，再写入配置并重启 app-server：

```json
{
  "endpoint": "https://api.openai.com/v1/embeddings",
  "model": "text-embedding-3-small",
  "modelIdentity": "openai:text-embedding-3-small:512:v1",
  "dimensions": 512,
  "apiKeyEnv": "OPENAI_API_KEY",
  "workspaces": ["workspace_replace_with_id"],
  "minimumSimilarity": 0.65
}
```

- 后台每秒轮查一个已启用的工作区，每批最多 8 条活动记忆。请求只发送 key 和正文。模型、维度或文本处理方式改变时使用新的 `modelIdentity`；写回时核对每条记录仍属于该工作区且 revision 未变。
- `task/start` 和 `task/startSummary` 仅给获准的工作区发起查询，发送最多 4 KiB 的当前输入文本。不会发送媒体、文件、历史或任务标识。关闭记忆、隔离上下文、外部 Agent 和已记录的幂等请求跳过查询；任务 Run 的子任务继续使用本机词项检索。
- 查询最多等待 750 毫秒，网络调用不持有内核锁。端点不可用、无兼容索引、索引损坏或没有达到默认相似度 `0.65` 的候选时，使用本机词项检索。初始化能力 `taskMemoryHybridRetrieval` 表示已配置服务，`taskMemoryOptOut` 表示支持单次关闭。
- 端点必须是 HTTPS，或使用环回 IP 的 HTTP。不跟随重定向。`apiKeyEnv` 只保存环境变量名称；密钥不会写入任务或内核数据库。启用前应确认端点可以收到所选工作区的记忆和当前输入文本。
