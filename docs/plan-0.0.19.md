# 0.0.19 模型接入与兼容协议

本版围绕接入真实模型和切换协议后的工作连续性。先让同一份用户配置能连接常见网关和本机模型，再让终端用户能确认连接和选择模型。应用服务与运行时承担协议、凭据、目录和能力判定。TUI 使用结构化结果。

## 调研与取舍

| 资料 | 落地要求 |
| --- | --- |
| [OpenAI Chat Completions](https://developers.openai.com/api/reference/resources/chat/subresources/completions/methods/create) | 实现独立请求和响应适配，覆盖 `max_completion_tokens`、按索引拼接的 `tool_calls`、结束原因、使用量与可选思考档位。Responses 配置沿用现有协议。 |
| [Ollama OpenAI 兼容接口](https://docs.ollama.com/api/openai-compatibility) 与 [llama.cpp 服务文档](https://github.com/ggml-org/llama.cpp/blob/master/tools/server/README.md) | 分别验证模型列表与对话。把官方契约、兼容接口和特定网关的可选字段作为独立测试用例，记录通过的模型能力。 |
| 本地 OpenCode `d284e7c` 的 `packages/ai/src/protocols/openai-chat.ts`、`openai-compatible-chat.ts` | 兼容网关复用 Chat 请求、流式解码与历史转译。测试缺失用量、并行工具分片、空分片和提前断开。 |
| 本地 Codex `a1604a8` 的 `codex-rs/model-provider-info/src/lib.rs` | 分离供应商 ID、端点、线上协议与能力。如同一主机提供多种协议，给用户明确选择。保持现有 `provider = "openai"` 的解释和已有配置文件可读。 |

## 交付与验收

| 板块 | 实现 | 验收 |
| --- | --- | --- |
| 协议适配 | Rust Provider 增加显式 Chat Completions 选择。目录复用 OpenAI 格式，保持协议身份进入任务续接与持久 Run 绑定。验证工具结果和调用 ID 一致，生成中断按已有结果语义处理。按实际公开字段处理思考强度，不自动带上网关不接受的参数。 | 本地 HTTP 合约覆盖 JSON 和 SSE、多工具调用与对应结果、用量、错误脱敏、输出上限、流中断及跨协议恢复。至少一套真实兼容服务通过端到端配置与对话。 |
| 模型接入检查 | 无界面入口检查已配置 profile 的凭据可读性、API 根路径及目录访问。用独立结果描述本地配置、远端认证、模型是否出现在目录及目录是否可用。可以继续手填只提供生成端点的网关。 | 使用配置文件和托管凭据，验证认证失败、端点缺失、列表为空、分页、模型 ID 未公布和 HTTP 超时。检查本身不发生成请求，不回显密钥。 |
| 客户端交互 | 在首启和模型管理中复用内核查询，在可用时搜索远端目录并选择模型。等待查询时保持输入可用。区分本次切换与保存默认值，延续草稿和历史。 | Windows ConPTY 与 Debian PTY 在 110、80、48 列验证，保存目录加载、空列表、手填、认证失败及切换后的实际请求截图。 |
| 发布 | 各板块拆分提交，发布前锁定候选 SHA。 | Windows/Linux 全量 CI，同 SHA 生成 MSI、Debian 包和便携包，用上一正式版测试升级、卸载、重装，发布资产与校验文件逐项比对。 |

## 开发验证

- Windows 使用 llama.cpp `b12821` 和 SmolLM2-135M-Instruct Q4_K_M，完成了带 Bearer 凭据的 `/v1/models` 查询、`--check-model` 和 stdio 对话。连接检查通过时模型在目录中，启动对话前数据库文件未创建。对话完成后保留 `openai-chat` 续接身份。
- Windows ConPTY 在 110、80 和 48 列抓取首启画面，检查协议选项、模型 ID、Base URL 和凭据错误提示。用 Chat Completions 完成设置并重新打开已保存的配置。远端目录选择和 Debian PTY 验收仍待完成。
