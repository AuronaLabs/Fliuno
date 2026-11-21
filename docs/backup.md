# 数据库备份与恢复

[文档中心](README.md) › [开发路线图](roadmap.md)

Fliuno Core 提供一致的 SQLite 在线快照、摘要校验和新路径还原 API。所有任务、事件、审批、工作区记忆和交付物正文都在同一 SQLite 数据库中；快照不会复制工作区目录或外部文件。

## 快照内容

每个备份目录包含：

| 文件 | 内容 |
| --- | --- |
| `database.sqlite` | SQLite 在线备份 API 生成的一致数据库快照，包含 WAL 中已提交的数据。 |
| `artifacts.sha256` | 按数据库顺序列出产物 ID、任务 ID、正文长度及逐块计算的 SHA-256。 |
| `manifest.json` | Fliuno/数据库版本、事件水位、任务和产物计数、数据库与产物清单摘要。 |

快照只包含内核数据库。数据库之外的工作区文件、凭据文件、环境变量、Agent 私有目录和远端服务状态不在其中。任务消息、Agent 更新、审批和本地历史会随数据库复制，其中可能含用户提交的敏感内容。Unix 上备份目录权限为 `0700`；Windows 使用目标父目录的继承 ACL。恢复数据库不会撤销备份之后已经发生的外部操作。

## 创建与保留

```shell
fliuno-app-server backup create .fliuno/kernel.db backups/manual --progress --timeout-ms 30000
fliuno-app-server backup list backups
fliuno-app-server backup rotate .fliuno/kernel.db backups 12 8589934592 --max-age-ms 2592000000
```

`create` 用独立只读连接在线复制指定数据库，不需要停止服务；备份目录必须是新路径。SQLite 会分批复制并短暂重试锁冲突，复制阶段最多运行 15 分钟。完成后写出产物清单和摘要，再以同卷目录重命名发布。`--progress` 将复制页数、校验和发布阶段以 JSON 行写到 stderr；复制进度可能随源库写入而回退。`--timeout-ms` 在复制批次之间和发布前检查，超时则清理临时目录；校验正在执行时要等到下一检查点。stdout 只输出最终 JSON。

`list` 返回根目录下完整通过校验的快照，按新到旧排列；损坏或尚未发布的快照不返回。`rotate` 创建新快照并应用数量、字节和可选年龄限制，返回新快照路径、清单和轮换统计。同一根目录只允许一个轮换进程执行。只有有效且符合轮换命名和文件布局的目录会参与删除，手动备份不计入限额。新快照单独超过容量限制时保留旧快照。总字节数按数据库、逐产物清单和 JSON 清单的实际长度计算，不含目录项和旧版空 WAL 所带的共享内存文件。

Rust 宿主也可用 `BackupSource::open` 对在线数据库操作，进度回调可在发布前返回错误取消。持有 `Kernel` 的宿主可直接使用如下接口：

```rust
let result = kernel.create_backup_with_retention(
    "backups",
    fliuno_core::BackupRetention {
        keep_count: 12,
        max_total_bytes: 8_u64 * 1024 * 1024 * 1024,
        max_age_ms: Some(30 * 24 * 60 * 60 * 1000),
    },
)?;
```

## 校验与还原

从本机运行以下命令。成功时 stdout 输出一行 JSON，包含 `operation` 和 `manifest`；失败时返回非零退出码，原因写到 stderr。

```shell
fliuno-app-server backup verify backups/snapshot
fliuno-app-server backup restore backups/snapshot recovery/kernel.db
fliuno-app-server backup verify-restored backups/snapshot recovery/kernel.db
```

校验核对文件摘要，再运行 SQLite `integrity_check` 与外键检查，核验 schema、事件水位、任务和产物计数。请在快照停止写入时校验，恢复后在启动服务前复核。0.0.4 快照可能留下空 WAL 和共享内存索引；校验兼容这类快照，恢复不复制这些文件。非空 WAL 或回滚日志会被拒绝。

还原目标必须是新数据库路径，旁边不能有旧的 SQLite 附属文件。还原持有目标 `.lock`，先把数据库复制到同卷临时目录并校验，再以硬链接原子发布，之后移除临时副本。目标卷必须支持硬链接，例如 NTFS。即使验证期间另一个进程创建了目标文件，还原也不会覆盖它。

```rust
let manifest = fliuno_core::Kernel::verify_backup("backups/latest")?;
fliuno_core::Kernel::restore_backup("backups/latest", "recovery/kernel.db")?;
fliuno_core::Kernel::verify_restored_database("recovery/kernel.db", &manifest)?;
```

这些管理命令不会初始化 Provider、启动任务或迁移数据库。确认恢复结果后，只有显式以 `--db recovery/kernel.db` 启动服务才会启用副本；此时会走正常的迁移、任务恢复和调度流程。先在隔离目录演练并保留原库。当前已验收 schema 64 的还原和启动；备份命令仍要求快照使用当前 schema。备份命令不属于 app-server RPC。
