# 0.0.20：模型连接管理与对话续写

以 0.0.19 的配置和 SQLite 数据为升级基线。目标是让用户从首启到日常维护都能在产品里连接模型，并能顺畅续写对话。模型连接验证和持久化由 Rust 共享层提供。

## 研究依据

- [Ollama API Authentication](https://docs.ollama.com/api/authentication) 明确本机默认地址无需认证；[OpenAI Compatibility](https://docs.ollama.com/api/openai-compatibility) 示例中的假 Key 是 SDK 参数要求，不是本机服务的验证要求。
- [llama.cpp Server](https://github.com/ggml-org/llama.cpp/blob/master/tools/server/README.md) 把 `--api-key` 作为显式设置。OpenCode `d284e7c` 的 `packages/core/src/plugin/provider/local.ts` 与 `ollama.ts` 将本地连接和目录识别留在 Provider 层。
- [Codex Configuration](https://developers.openai.com/codex/config-reference) 将 `base_url`、协议和认证分开；本地 Codex `a1604a8` 的 `ProviderSettingsPatch.ts` 和 `ModelProviderConfigWriteParams.ts` 用专门的服务类型写配置。[Claude Code Model Configuration](https://code.claude.com/docs/en/model-config) 区分当前会话模型和持久偏好。

## 板块一：模型连接管理

1. 首启和工作台使用同一份 Rust 连接草稿。编辑与新增档案支持协议、Base URL、保留凭据、替换凭据，以及显式无凭据的本地连接。无凭据只面向显式配置的回环地址；远端及旧配置继续要求凭据。保留旧档案可读性和环境变量入口的兼容性。
2. 修改端点后不自动把旧 Key 发给新地址。路由和凭据的选择分别明确确认，先校验协议和规范化后的 API 根地址，再决定可否保留原凭据。新地址和新协议须显式选择凭据。目录验证可先用草稿在内存中完成；显式无凭据不能回退读取环境变量或旧凭据文件。凭据文本不出现在 RPC 响应、日志、终端捕获或最终 TOML。
3. 外部客户端继续使用同一套档案版本检查与原子写入。用不带 Authorization 的本地 HTTP fixture 和真实 Ollama 或 llama.cpp 验证；另验证远端与非回环地址的拒绝、替换失败不丢旧凭据、多档案继承与升级。

## 板块二：重开对话与提交体验

1. 针对 0.0.19 Debian 验收发现的忙碌窗口，量化重开后从可编辑到可提交的间隔。区分读取历史、生成、审批或保存档案。不应因纯后台历史刷新让用户重复按提交。用户提前按一次发送可在相同会话就绪后完成，须能取消且不会重复提交。与持久任务队列的含义区分开。
2. 原生 PTY 复现慢历史加载、提交期间退出、切换会话和重开。用请求 ID/业务幂等键确认确切发送次数。48、80、110 列检查状态、操作提示、草稿和长模型名的排版。

## 发布门槛

按板块拆成可独立理解的提交。先发规划，开发完成后锁定候选 SHA。Windows/Linux 内核检查与 Windows x64 MSI、Debian 12 amd64 安装验收共用这一 SHA。比对安装包与便携包的可执行文件，核对下载及远端发布文件哈希，逐图检查已安装版的截图。继续记录 D/E 盘的空间，不删除活动构建或测试证据。

## 开发验证

- Windows 上用真实 llama.cpp 完成无需密钥的模型目录查询和对话。HTTP 测试验证了自定义 Chat Completions 路由、仅回环地址可不带密钥，以及新地址不得继承旧凭据。托管凭据保存失败会回滚。
- Windows ConPTY 在 48、80、110 列均验证从 `/model` 新增本地档案、加载目录、编辑已激活档案、轮换地址和凭据、重新启用并接续草稿。对照实际 HTTP 请求确认更换后的地址与密钥生效；捕获文件没有出现密钥原文。
- 用本地 stdio 测试转发器暂停 `session/listHistory` 和自动跟进的 `task/get`。多次按 Enter 只发出一条 `task/start`，会话 ID 和客户端幂等键均保留；取消、切换、退出和断开连接没有发出请求。已检查 48、80、110 列的待发送提示及草稿排版。