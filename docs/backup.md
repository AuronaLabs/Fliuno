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

备份不会写入 API key、环境变量凭据或运行期秘密。数据库本身包含任务消息、Agent 更新、审批和本地历史，备份目录应按敏感数据存放。Unix 上目录权限为 `0700`；Windows 使用目标父目录的继承 ACL。

## 创建与保留

`Kernel::create_backup` 创建一个指定的新目录；目录必须不存在。在线备份会分块读取 SQLite，遇到临时锁定会短暂重试，最多运行 15 分钟。完成后写出产物清单与校验摘要，再以同卷目录重命名提交快照；失败会清理临时目录，不会修改运行中的数据库。

`Kernel::create_backup_with_retention` 在指定目录中创建 `fliuno-backup-*` 快照，并按保留数量、总字节数及可选最大保存时长轮换。只有能够完整校验的 Fliuno 备份目录会参与轮换，其他文件和目录不会被删除。新快照先完整提交；若单个快照已超过容量限制，会删除新快照并保留旧快照。总字节数按数据库、逐产物清单和 JSON 清单的实际长度计算，不含文件系统目录项开销。

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

`Kernel::verify_backup` 先核对文件摘要，再运行 SQLite `integrity_check` 与外键检查，核验 schema、水位、计数和产物清单。还原目标必须是新数据库路径；已有文件不会被覆盖。还原会获取目标 `.lock` 锁、复制数据库、校验摘要和结构，再原子地将文件移动到目标路径。

```rust
let manifest = fliuno_core::Kernel::verify_backup("backups/latest")?;
fliuno_core::Kernel::restore_backup("backups/latest", "recovery/kernel.db")?;
fliuno_core::Kernel::verify_restored_database("recovery/kernel.db", &manifest)?;
```

确认还原结果后，将服务配置切换到新数据库路径。打开数据库时，内核会沿正常启动路径执行版本迁移与任务恢复；请在隔离目录先做演练，再替换正在使用的数据库路径。备份 API 属于 Rust 内核库能力，尚未暴露为 app-server RPC。
