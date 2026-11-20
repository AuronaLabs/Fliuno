//! Metadata catalogs and incremental reads of immutable artifact bodies.
mod digests;
mod downloads;
use super::*;
use rusqlite::{Transaction, blob::Blob, types::ValueRef};
use sha2::{Digest, Sha256};

pub(super) fn migrate(db: &mut Connection) -> Result<()> {
    let tx = db.transaction()?;
    tx.execute_batch(
        "CREATE TABLE IF NOT EXISTS artifact_bodies(
            artifact_id TEXT PRIMARY KEY REFERENCES artifacts(id),
            body BLOB NOT NULL
        );
        INSERT INTO artifact_bodies(artifact_id,body)
        SELECT a.id,CAST(json_extract(a.data,'$.content') AS BLOB) FROM artifacts a
        WHERE NOT EXISTS(SELECT 1 FROM artifact_bodies b WHERE b.artifact_id=a.id);
        UPDATE artifacts SET data=json_object(
            'title',json_extract(data,'$.title'),
            'mediaType',json_extract(data,'$.mediaType'),
            'contentBytes',(SELECT length(body) FROM artifact_bodies b WHERE b.artifact_id=artifacts.id)
        ) WHERE json_type(data,'$.content') IS NOT NULL;
        PRAGMA user_version=21;",
    )?;
    tx.commit()?;
    Ok(())
}

pub(super) fn migrate_catalog(db: &mut Connection) -> Result<()> {
    let tx = db.transaction()?;
    tx.execute_batch(
        "CREATE TABLE IF NOT EXISTS artifact_catalog(
            sequence INTEGER PRIMARY KEY,
            artifact_id TEXT NOT NULL UNIQUE REFERENCES artifacts(id),
            session_id TEXT NOT NULL REFERENCES sessions(id),
            workspace_id TEXT NOT NULL REFERENCES workspaces(id)
        );
        INSERT INTO artifact_catalog(artifact_id,session_id,workspace_id)
        SELECT a.id,t.session_id,s.workspace_id FROM artifacts a
        JOIN tasks t ON t.id=a.task_id JOIN sessions s ON s.id=t.session_id
        WHERE NOT EXISTS(SELECT 1 FROM artifact_catalog c WHERE c.artifact_id=a.id)
        ORDER BY a.rowid;
        CREATE INDEX IF NOT EXISTS artifact_catalog_session ON artifact_catalog(session_id);
        CREATE INDEX IF NOT EXISTS artifact_catalog_workspace ON artifact_catalog(workspace_id);
        CREATE TRIGGER IF NOT EXISTS artifact_catalog_insert AFTER INSERT ON artifacts BEGIN
            INSERT INTO artifact_catalog(artifact_id,session_id,workspace_id)
            SELECT NEW.id,t.session_id,s.workspace_id FROM tasks t
            JOIN sessions s ON s.id=t.session_id WHERE t.id=NEW.task_id;
        END;
        PRAGMA user_version=26;",
    )?;
    tx.commit()?;
    Ok(())
}

pub(super) fn migrate_catalog_media_type(db: &mut Connection) -> Result<()> {
    let tx = db.transaction()?;
    let has_media_type: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM pragma_table_info('artifact_catalog') WHERE name='media_type')",
        [],
        |row| row.get(0),
    )?;
    if !has_media_type {
        tx.execute_batch(
            "ALTER TABLE artifact_catalog ADD COLUMN media_type TEXT NOT NULL DEFAULT '';",
        )?;
    }
    // Keep the original catalog rowids so unfiltered cursors survive the migration.
    tx.execute_batch(
        "UPDATE artifact_catalog SET media_type=(SELECT json_extract(data,'$.mediaType')
            FROM artifacts WHERE id=artifact_id);
        CREATE INDEX IF NOT EXISTS artifact_catalog_session_type ON artifact_catalog(session_id,media_type);
        CREATE INDEX IF NOT EXISTS artifact_catalog_workspace_type ON artifact_catalog(workspace_id,media_type);
        DROP TRIGGER IF EXISTS artifact_catalog_insert;
        CREATE TRIGGER artifact_catalog_insert AFTER INSERT ON artifacts BEGIN
            INSERT INTO artifact_catalog(artifact_id,session_id,workspace_id,media_type)
            SELECT NEW.id,t.session_id,s.workspace_id,json_extract(NEW.data,'$.mediaType') FROM tasks t
            JOIN sessions s ON s.id=t.session_id WHERE t.id=NEW.task_id;
        END;
        PRAGMA user_version=28;",
    )?;
    tx.commit()?;
    Ok(())
}

fn catalog_cursor(after: u64, limit: u32) -> Result<i64> {
    let cursor =
        i64::try_from(after).map_err(|_| Error::Invalid("artifact cursor must fit i64".into()))?;
    if !(1..=100).contains(&limit) {
        return Err(Error::Invalid("artifact list limit must be 1..100".into()));
    }
    Ok(cursor)
}

pub(super) fn record(tx: &Transaction<'_>, task_id: &str, artifact: &Artifact) -> Result<()> {
    if artifact.task_id != task_id {
        return Err(Error::Invalid("artifact belongs to another task".into()));
    }
    // Keep metadata and its byte identity together in the same commit as the
    // body. The optional metadata field needs no migration or body backfill.
    let bytes = artifact.body.as_bytes();
    let sha256 =
        (bytes.len() <= MAX_ARTIFACT_DIGEST_BYTES).then(|| format!("{:x}", Sha256::digest(bytes)));
    tx.prepare_cached(
        "INSERT INTO artifacts(id,task_id,data) VALUES(?1,?2,
            json_object('title',?3,'mediaType',?4,'contentBytes',?5,'binary',?6,'sha256',?7))",
    )?
    .execute(params![
        artifact.id,
        task_id,
        artifact.title,
        artifact.media_type,
        bytes.len() as i64,
        matches!(artifact.body, ArtifactBody::Binary { .. }),
        sha256,
    ])?;
    tx.prepare_cached("INSERT INTO artifact_bodies(artifact_id,body) VALUES(?1,?2)")?
        .execute(params![artifact.id, bytes])?;
    Ok(())
}

fn artifact_row(row: &rusqlite::Row<'_>, task_id: &str) -> rusqlite::Result<Artifact> {
    Ok(Artifact {
        id: row.get(0)?,
        task_id: task_id.into(),
        title: row.get(1)?,
        media_type: row.get(2)?,
        body: match row.get_ref(3)? {
            ValueRef::Blob(bytes) => ArtifactBody::Binary {
                bytes: bytes.to_vec(),
            },
            _ => ArtifactBody::Text {
                content: row.get(3)?,
            },
        },
    })
}

fn summary_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<ArtifactSummary> {
    Ok(ArtifactSummary {
        id: row.get(0)?,
        task_id: row.get(5)?,
        title: row.get(1)?,
        media_type: row.get(2)?,
        content_bytes: row.get::<_, i64>(3)? as u64,
        binary: row.get(6)?,
    })
}

fn hash_artifact_body(body: &Blob<'_>) -> rusqlite::Result<String> {
    let length = body.len();
    let mut sha256 = Sha256::new();
    let mut buffer = vec![0; length.min(MAX_ARTIFACT_READ_BYTES as usize)];
    let mut offset = 0;
    while offset < length {
        let count = buffer.len().min(length - offset);
        let chunk = &mut buffer[..count];
        body.read_at_exact(chunk, offset)?;
        sha256.update(chunk);
        offset += count;
    }
    Ok(format!("{:x}", sha256.finalize()))
}

impl Kernel {
    pub fn list_artifacts(&self, task_id: &str) -> Result<Vec<Artifact>> {
        self.require_task(task_id)?;
        let mut statement = self.db.prepare_cached(
            "SELECT a.id,json_extract(a.data,'$.title'),json_extract(a.data,'$.mediaType'),
             CASE WHEN json_extract(a.data,'$.binary') THEN b.body ELSE CAST(b.body AS TEXT) END
             FROM artifacts a JOIN artifact_bodies b ON b.artifact_id=a.id
             WHERE a.task_id=?1 ORDER BY a.rowid",
        )?;
        let artifacts = statement.query_map([task_id], |row| artifact_row(row, task_id))?;
        Ok(artifacts.collect::<std::result::Result<_, _>>()?)
    }

    pub fn list_artifact_summaries(
        &self,
        task_id: &str,
        after: u64,
        limit: u32,
    ) -> Result<ObjectPage<ArtifactSummary>> {
        let cursor = catalog_cursor(after, limit)?;
        self.require_task(task_id)?;
        self.query_catalog_page(
            "SELECT id,json_extract(data,'$.title'),json_extract(data,'$.mediaType'),
             json_extract(data,'$.contentBytes'),rowid,task_id,
             COALESCE(json_extract(data,'$.binary'),0) FROM artifacts
             WHERE task_id=?1 AND rowid>?2 ORDER BY rowid LIMIT ?3",
            params![task_id, cursor, limit + 1],
            cursor,
            limit,
        )
    }

    pub fn list_artifact_catalog(
        &self,
        scope: &ArtifactCatalogScope,
        after: u64,
        limit: u32,
    ) -> Result<ObjectPage<ArtifactSummary>> {
        self.list_artifact_catalog_by_type(scope, after, limit, None)
    }

    pub fn list_artifact_catalog_by_type(
        &self,
        scope: &ArtifactCatalogScope,
        after: u64,
        limit: u32,
        media_type: Option<&str>,
    ) -> Result<ObjectPage<ArtifactSummary>> {
        self.scoped_artifact_page(scope, after, i64::MAX, limit, media_type)
    }

    pub fn list_artifact_snapshot(
        &self,
        params: ArtifactCatalogSnapshotParams,
    ) -> Result<ArtifactCatalogSnapshotPage> {
        // The primary key stores append-only catalog order; capture the upper
        // bound using the same scope-independent sequence space as nextAfter.
        let latest: i64 = self
            .db
            .prepare_cached("SELECT COALESCE(MAX(sequence),0) FROM artifact_catalog")?
            .query_row([], |row| row.get(0))?;
        let through_sequence = params.through_sequence.unwrap_or(latest as u64);
        if through_sequence > latest as u64 || params.after > through_sequence {
            return Err(Error::Invalid(
                "artifact snapshot requires after <= throughSequence <= current catalog sequence"
                    .into(),
            ));
        }
        let page = self.scoped_artifact_page(
            &params.scope,
            params.after,
            through_sequence as i64,
            params.limit,
            params.media_type.as_deref(),
        )?;
        Ok(ArtifactCatalogSnapshotPage {
            items: page.items,
            through_sequence,
            next_after: page.next_after,
            has_more: page.has_more,
        })
    }

    fn scoped_artifact_page(
        &self,
        scope: &ArtifactCatalogScope,
        after: u64,
        through: i64,
        limit: u32,
        media_type: Option<&str>,
    ) -> Result<ObjectPage<ArtifactSummary>> {
        let cursor = catalog_cursor(after, limit)?;
        if media_type.is_some_and(|value| {
            value.trim().is_empty() || value.len() > 256 || value.chars().any(char::is_control)
        }) {
            return Err(Error::Invalid(
                "mediaType must be 1 to 256 UTF-8 bytes without control characters".into(),
            ));
        }
        let (column, typed_index, id) = match scope {
            ArtifactCatalogScope::Session { session_id } => {
                self.require_session(session_id)?;
                ("session_id", "artifact_catalog_session_type", session_id)
            }
            ArtifactCatalogScope::Workspace { workspace_id } => {
                self.require_workspace(workspace_id)?;
                (
                    "workspace_id",
                    "artifact_catalog_workspace_type",
                    workspace_id,
                )
            }
        };
        // SQLite may otherwise scan the scope index and filter each row when
        // there are no planner statistics and the requested type is rare.
        let (catalog, media_predicate) = if media_type.is_some() {
            (
                format!("artifact_catalog AS c INDEXED BY {typed_index}"),
                "AND c.media_type=?5",
            )
        } else {
            ("artifact_catalog AS c".into(), "")
        };
        let sql = format!(
            "SELECT a.id,json_extract(a.data,'$.title'),c.media_type,
             json_extract(a.data,'$.contentBytes'),c.sequence,a.task_id,
             COALESCE(json_extract(a.data,'$.binary'),0)
             FROM {catalog} JOIN artifacts a ON a.id=c.artifact_id
             WHERE c.{column}=?1 AND c.sequence>?2 AND c.sequence<=?3 {media_predicate}
             ORDER BY c.sequence LIMIT ?4"
        );
        if let Some(media_type) = media_type {
            self.query_catalog_page(
                &sql,
                params![id, cursor, through, limit + 1, media_type],
                cursor,
                limit,
            )
        } else {
            self.query_catalog_page(&sql, params![id, cursor, through, limit + 1], cursor, limit)
        }
    }

    fn query_catalog_page(
        &self,
        sql: &str,
        parameters: impl rusqlite::Params,
        cursor: i64,
        limit: u32,
    ) -> Result<ObjectPage<ArtifactSummary>> {
        let mut statement = self.db.prepare_cached(sql)?;
        let mut rows = statement.query(parameters)?;
        let mut items = Vec::with_capacity(limit as usize);
        let mut next_after = cursor as u64;
        let mut has_more = false;
        while let Some(row) = rows.next()? {
            if items.len() == limit as usize {
                has_more = true;
                break;
            }
            next_after = row.get::<_, i64>(4)? as u64;
            items.push(summary_row(row)?);
        }
        Ok(ObjectPage {
            items,
            next_after,
            has_more,
        })
    }

    pub fn get_artifact_summary(
        &self,
        task_id: &str,
        artifact_id: &str,
    ) -> Result<ArtifactSummary> {
        self.db
            .prepare_cached(
                "SELECT id,json_extract(data,'$.title'),json_extract(data,'$.mediaType'),
             json_extract(data,'$.contentBytes'),rowid,task_id,
             COALESCE(json_extract(data,'$.binary'),0) FROM artifacts WHERE task_id=?1 AND id=?2",
            )?
            .query_row(params![task_id, artifact_id], summary_row)
            .optional()?
            .ok_or_else(|| Error::NotFound("artifact".into()))
    }

    /// Resolve an attachment by its owning session before opening its body.
    pub(crate) fn read_artifact_input(
        &self,
        session: &str,
        reference: &ArtifactGetParams,
        budget: &mut TaskInputBudget,
    ) -> Result<UserContentPart> {
        let (rowid, media_type, binary): (i64, String, bool) = self.db.prepare_cached(
            "SELECT b.rowid,json_extract(a.data,'$.mediaType'),COALESCE(json_extract(a.data,'$.binary'),0)
             FROM tasks t JOIN artifacts a ON a.task_id=t.id
             JOIN artifact_bodies b ON b.artifact_id=a.id
             WHERE t.session_id=?1 AND t.id=?2 AND a.id=?3",
        )?.query_row(params![session, reference.task_id, reference.artifact_id], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?))
        }).optional()?.ok_or_else(|| Error::NotFound("artifact in session".into()))?;
        let blob = self
            .db
            .blob_open("main", "artifact_bodies", "body", rowid, true)?;
        let length = blob.len();
        // Charge the raw text or encoded media size before allocating the body.
        if binary {
            let encoded = base64::encoded_len(length, true)
                .ok_or_else(|| Error::Invalid("artifact input size overflow".into()))?;
            budget.reserve_media(encoded)
        } else {
            budget.reserve_text(length)
        }
        .map_err(|message| Error::Invalid(message.into()))?;
        let mut bytes = vec![0; length];
        blob.read_at_exact(&mut bytes, 0)?;
        if binary {
            UserContentPart::from_media_bytes(&media_type, &bytes)
                .map_err(|message| Error::Invalid(message.into()))
        } else {
            let text = String::from_utf8(bytes)
                .map_err(|_| Error::Invalid("text artifact input must be UTF-8".into()))?;
            Ok(UserContentPart::Text { text })
        }
    }

    /// Return one byte range and the immutable content length, even when the
    /// cursor falls inside a UTF-8 character.
    pub fn read_artifact_chunk(
        &self,
        task_id: &str,
        artifact_id: &str,
        after: u64,
        limit: u32,
    ) -> Result<(u64, Vec<u8>)> {
        i64::try_from(after).map_err(|_| Error::Invalid("artifact offset must fit i64".into()))?;
        if !(1..=MAX_ARTIFACT_READ_BYTES).contains(&limit) {
            return Err(Error::Invalid(format!(
                "artifact read limit must be 1..{MAX_ARTIFACT_READ_BYTES}"
            )));
        }
        let blob = self.artifact_body(task_id, artifact_id)?;
        let length = blob.len();
        if after > length as u64 {
            return Err(Error::Invalid("artifact offset is past the end".into()));
        }
        let count = (length - after as usize).min(limit as usize);
        let mut data = vec![0; count];
        if count != 0 {
            blob.read_at_exact(&mut data, after as usize)?;
        }
        Ok((length as u64, data))
    }

    /// Read the committed SHA-256 without reopening the body. Older artifacts
    /// get a bounded, persisted digest on their first lookup.
    pub fn digest_artifact(&self, task_id: &str, artifact_id: &str) -> Result<ArtifactDigest> {
        let (content_bytes, sha256): (i64, Option<String>) = self
            .db
            .prepare_cached(
                "SELECT json_extract(data,'$.contentBytes'),json_extract(data,'$.sha256')
                 FROM artifacts WHERE task_id=?1 AND id=?2",
            )?
            .query_row(params![task_id, artifact_id], |row| {
                Ok((row.get(0)?, row.get(1)?))
            })
            .optional()?
            .ok_or_else(|| Error::NotFound("artifact".into()))?;
        let content_bytes =
            u64::try_from(content_bytes).map_err(|_| rusqlite::Error::InvalidQuery)?;
        if content_bytes > MAX_ARTIFACT_DIGEST_BYTES as u64 {
            return Err(Error::Invalid(
                "artifact exceeds the digest byte budget".into(),
            ));
        }
        let sha256 = match sha256 {
            Some(sha256) => sha256,
            None => self.backfill_artifact_digest(task_id, artifact_id, content_bytes)?,
        };
        Ok(ArtifactDigest {
            content_bytes,
            sha256,
        })
    }

    /// Read the checksum and first bounded range for a new download.
    pub fn open_artifact_download(
        &self,
        task_id: &str,
        artifact_id: &str,
    ) -> Result<(ArtifactDigest, Vec<u8>)> {
        let digest = self.digest_artifact(task_id, artifact_id)?;
        let (length, data) =
            self.read_artifact_chunk(task_id, artifact_id, 0, MAX_ARTIFACT_READ_BYTES)?;
        if length != digest.content_bytes {
            return Err(rusqlite::Error::InvalidQuery.into());
        }
        Ok((digest, data))
    }

    fn backfill_artifact_digest(
        &self,
        task_id: &str,
        artifact_id: &str,
        length: u64,
    ) -> Result<String> {
        // Pin the legacy bytes while computing the hash. Close the read-only
        // blob handle before writing the metadata on this same connection.
        let tx = self.db.unchecked_transaction()?;
        let sha256 = {
            let body = self.artifact_body(task_id, artifact_id)?;
            if body.len() as u64 != length {
                return Err(rusqlite::Error::InvalidQuery.into());
            }
            hash_artifact_body(&body)?
        };
        tx.prepare_cached(
            "UPDATE artifacts SET data=json_set(data,'$.sha256',?3) WHERE task_id=?1 AND id=?2",
        )?
        .execute(params![task_id, artifact_id, sha256])?;
        tx.commit()?;
        Ok(sha256)
    }

    fn artifact_body(&self, task_id: &str, artifact_id: &str) -> Result<Blob<'_>> {
        let rowid = self
            .db
            .prepare_cached(
                "SELECT b.rowid FROM artifacts a JOIN artifact_bodies b ON b.artifact_id=a.id
             WHERE a.task_id=?1 AND a.id=?2",
            )?
            .query_row(params![task_id, artifact_id], |row| row.get(0))
            .optional()?
            .ok_or_else(|| Error::NotFound("artifact".into()))?;
        Ok(self
            .db
            .blob_open("main", "artifact_bodies", "body", rowid, true)?)
    }

    pub fn get_artifact(&self, task_id: &str, artifact_id: &str) -> Result<Artifact> {
        self.db
            .prepare_cached(
                "SELECT a.id,json_extract(a.data,'$.title'),json_extract(a.data,'$.mediaType'),
             CASE WHEN json_extract(a.data,'$.binary') THEN b.body ELSE CAST(b.body AS TEXT) END
             FROM artifacts a JOIN artifact_bodies b ON b.artifact_id=a.id
             WHERE a.task_id=?1 AND a.id=?2",
            )?
            .query_row(params![task_id, artifact_id], |row| {
                artifact_row(row, task_id)
            })
            .optional()?
            .ok_or_else(|| Error::NotFound("artifact".into()))
    }
}
