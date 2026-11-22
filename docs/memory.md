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
- 以输入和记忆 key/正文的词项重叠计分，排除 `for`、`the`、`use` 等常见英文词。仍识别有词边界的完整 key（例如 `R`）。中文、日文和韩文按相邻双字提取词项。开启语义检索后，达标的向量结果按排名与词项结果融合。
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

使用 `multilingual-e5-small` 这类 E5 模型时，可将 `profile` 设为 `multilingual_e5_en_zh_v1`，并按实际模型填写 `model` 和 `dimensions`。app-server 会添加 `query: ` 或 `passage: ` 前缀；端点不应重复添加。启动后的后台索引先用两批固定的中英文通用短句取向量，并按语言与用途缓存均值。检索和索引都从归一化后的向量减去对应均值；含汉字的文本选用中文均值，其他文本选用英文均值。后台准备不就绪时回退词法。

该配置默认使用 `minimumSimilarity: 0.24`，只将余弦相似度最高且达标的一个向量候选和词法结果融合。索引身份自动加上 `e5-en-zh-v1:`，更换模型仍需填写新的 `modelIdentity`；未设置 `profile` 时继续使用原索引规则。自动索引从原记忆重建，配置切换不修改已冻结的任务快照。

## 中英检索评测

`scripts/benchmark_memory.py` 在真实 app-server 上重放 `scripts/memory_eval_cases.json`。校准和验收使用不同记忆与查询；每条查询使用新会话，从 `task/start` 响应检查实际冻结的记忆，随后取消 Demo Provider 的任务。计时包括本机 RPC、检索和任务落库，不包括取消、建索引或模型生成。命中按有相关记忆的问题计数；误注入按含任一无关记忆的问题计数。

Windows 调试构建的校准结果（同一 Ryzen 5 5600H 主机，8 题各测 20 次）：`v0.0.4` 命中 2/5，误注入 2/8，p50/p95 为 3.18/4.32 ms；当前仅用词法时命中 3/5，误注入 0/8，p50/p95 为 3.77/5.42 ms。预设验收样本门槛：命中至少 5/7，误注入最多 1/12，本机 `task/start` p95 相比同一验收样本的旧版额外不超过 50 ms。跨工作区和失效记录禁止注入，另由进程测试验证。该小样本用于版本验收，不代表通用检索准确率或云端延迟。

固定 E5 模型开启语义检索后的校准：余弦阈值 0.65 命中 5/5，但 8/8 有误注入；提高到 0.82 后仍有 5/8 误注入。更高的 0.84 将命中降到 3/5。

另试了一个保守方案：短 key（最多两个字符）仅做词法匹配，向量最多注入第一名，要求余弦分数 ≥0.65 且领先第二名至少 0.035。校准命中 4/5、误注入 1/8；在独立样本上命中 2/7、误注入 1/12，p50/p95 为 13.85/16.57 ms。相同独立样本的 `v0.0.4` 基线是命中 1/7、误注入 3/12、p50/p95 为 3.35/4.74 ms。两组独立样本均为每题重复 20 次。该方案未达到命中门槛，未合入；该批样本已用于评估，若据此改方案，需另留一批未参与调参的样本复验。

固定 E5 配置和 0.24 阈值后，用未参与调参的 `holdout_v2` 复验（各 240 次计时）：

| 配置 | 命中 | 含误注入 | p50 | p95 |
| --- | ---: | ---: | ---: | ---: |
| `v0.0.4` 词法基线 | 1/7 | 4/12 | 4.16 ms | 7.26 ms |
| `multilingual_e5_en_zh_v1` + 词法 | 6/7 | 0/12 | 14.55 ms | 21.75 ms |

中英两种系统开发改写均找到 Rust 偏好，保留了单字符 `Z` 的精确命中；未命中的是英文值班交接问题对中文记忆。延迟增量 p95 为 14.49 ms。该结果只对应固定模型、本机端点及这批样本，其他模型或远程端点需重新测量。

构建 `v0.0.4` 源码与当前 app-server 后，用同一命令分别运行。基线不需要模型或第三方 Python 库：

```powershell
python scripts/benchmark_memory.py PATH_TO_V004_APP_SERVER --split calibration --repetitions 20
```

语义轨使用 [`intfloat/multilingual-e5-small`](https://huggingface.co/intfloat/multilingual-e5-small/tree/614241f622f53c4eeff9890bdc4f31cfecc418b3/onnx) 的 `onnx/model_qint8_avx512_vnni.onnx` 和 `onnx/tokenizer.json`。脚本校验两个文件的 SHA-256，通过回环 HTTP 提供向量。Windows 验证使用 Python 3.12、`numpy==2.4.4`、`onnxruntime==1.25.1` 和 `tokenizers==0.22.2`：

```powershell
python scripts/benchmark_memory.py PATH_TO_CURRENT_APP_SERVER --split calibration --repetitions 2 `
    --weights PATH_TO_ONNX --tokenizer PATH_TO_TOKENIZER_JSON --minimum-similarity 0.82
```

验收复现使用 `--split holdout_v2 --repetitions 20 --profile multilingual_e5_en_zh_v1 --minimum-similarity 0.24` 和相同的模型文件。基线同样用 `--split holdout_v2 --repetitions 20`，省略所有模型参数。

## 按修订选择记忆

`initialize.capabilities.taskExplicitMemory` 表示支持 `context.memorySelection`。传入 `[{"id":"<memory-id>","revision":2}]` 可选定此工作区的 1～8 条活动记忆。`session/context` 使用同一 `options.memorySelection` 预览。提交时复核 revision 并保存正文，任务记录中的 `matchedBy` 为 `explicit`。

显式选择跳过词法检索和 Embedding 请求。这些正文计入 16 KiB 记忆上限与整个上下文的 `maxBytes`。选择超限会返回错误。Provider 预检可进一步精简历史，但不会在重试时省略显式选择的记忆。工作区记忆被更新或删除后，带旧修订的新任务会收到错误，已创建任务保留其快照。

自动注入可以用 `context.memoryRecencyHalfLifeDays` 给近期更新的记忆少量排序加权。它不影响显式选择，也不会让未通过相关性筛选的记录进入候选。旧记录至少保留原相关度分数的 80%。使用 `task/contextAttribution` 可查看任务冻结的记忆修订和命中来源。

### 0.0.6 独立样本复验

`holdout_v3` 增加中英改写、单字符键和无答案问题。固定沿用 E5 profile 和 0.24 阈值，每组 12 题、144 次计时。自动注入未达到之前设定的最多 1/12 含误注入门槛。需要精确控制来源时，可选定修订或关闭自动注入。

| 配置 | 命中 | 含误注入 | p50 | p95 |
| --- | ---: | ---: | ---: | ---: |
| `v0.0.5` 混合检索 | 5/7 | 4/12 | 16.97 ms | 22.47 ms |
| `v0.0.6` 混合检索 | 5/7 | 4/12 | 18.62 ms | 22.74 ms |
| `v0.0.6` 词法检索 | 2/7 | 1/12 | 6.22 ms | 9.09 ms |

这些是同一台 Windows 主机调试构建的实测值，测量时同机也运行构建任务。复测可沿用上一节命令，将样本改成 `--split holdout_v3 --repetitions 12`。工作区隔离与修订失效由进程测试覆盖。
