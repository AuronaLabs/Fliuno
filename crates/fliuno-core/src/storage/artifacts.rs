//! Task-scoped metadata and incremental reads of immutable artifact bodies.
use super::*;
use rusqlite::Transaction;

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

pub(super) fn record(tx: &Transaction<'_>, task_id: &str, artifact: &Artifact) -> Result<()> {
    if artifact.task_id != task_id {
        return Err(Error::Invalid("artifact belongs to another task".into()));
    }
    // Keep the existing metadata rowid as the catalog cursor. Body bytes live
    // separately so paging and context projection never touch the body pages.
    tx.prepare_cached(
        "INSERT INTO artifacts(id,task_id,data) VALUES(?1,?2,
            json_object('title',?3,'mediaType',?4,'contentBytes',?5))",
    )?
    .execute(params![
        artifact.id,
        task_id,
        artifact.title,
        artifact.media_type,
        artifact.content.len() as i64
    ])?;
    tx.prepare_cached("INSERT INTO artifact_bodies(artifact_id,body) VALUES(?1,?2)")?
        .execute(params![artifact.id, artifact.content.as_bytes()])?;
    Ok(())
}

fn artifact_row(row: &rusqlite::Row<'_>, task_id: &str) -> rusqlite::Result<Artifact> {
    Ok(Artifact {
        id: row.get(0)?,
        task_id: task_id.into(),
        title: row.get(1)?,
        media_type: row.get(2)?,
        content: row.get(3)?,
    })
}

impl Kernel {
    pub fn list_artifacts(&self, task_id: &str) -> Result<Vec<Artifact>> {
        self.require_task(task_id)?;
        let mut statement = self.db.prepare_cached(
            "SELECT a.id,json_extract(a.data,'$.title'),json_extract(a.data,'$.mediaType'),CAST(b.body AS TEXT)
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
        let cursor = i64::try_from(after)
            .map_err(|_| Error::Invalid("artifact cursor must fit i64".into()))?;
        if !(1..=100).contains(&limit) {
            return Err(Error::Invalid("artifact list limit must be 1..100".into()));
        }
        self.require_task(task_id)?;
        let mut statement = self.db.prepare_cached(
            "SELECT rowid,id,json_extract(data,'$.title'),json_extract(data,'$.mediaType'),
             json_extract(data,'$.contentBytes') FROM artifacts
             WHERE task_id=?1 AND rowid>?2 ORDER BY rowid LIMIT ?3",
        )?;
        let mut rows = statement.query(params![task_id, cursor, limit + 1])?;
        let mut items = Vec::with_capacity(limit as usize);
        let mut next_after = after;
        let mut has_more = false;
        while let Some(row) = rows.next()? {
            if items.len() == limit as usize {
                has_more = true;
                break;
            }
            next_after = row.get::<_, i64>(0)? as u64;
            items.push(ArtifactSummary {
                id: row.get(1)?,
                task_id: task_id.into(),
                title: row.get(2)?,
                media_type: row.get(3)?,
                content_bytes: row.get::<_, i64>(4)? as u64,
            });
        }
        Ok(ObjectPage {
            items,
            next_after,
            has_more,
        })
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
        let rowid = self
            .db
            .prepare_cached(
                "SELECT b.rowid FROM artifacts a JOIN artifact_bodies b ON b.artifact_id=a.id
             WHERE a.task_id=?1 AND a.id=?2",
            )?
            .query_row(params![task_id, artifact_id], |row| row.get(0))
            .optional()?
            .ok_or_else(|| Error::NotFound("artifact".into()))?;
        let blob = self
            .db
            .blob_open("main", "artifact_bodies", "body", rowid, true)?;
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

    pub fn get_artifact(&self, task_id: &str, artifact_id: &str) -> Result<Artifact> {
        self.db.prepare_cached(
            "SELECT a.id,json_extract(a.data,'$.title'),json_extract(a.data,'$.mediaType'),CAST(b.body AS TEXT)
             FROM artifacts a JOIN artifact_bodies b ON b.artifact_id=a.id
             WHERE a.task_id=?1 AND a.id=?2",
        )?.query_row(params![task_id, artifact_id], |row| artifact_row(row, task_id)).optional()?
            .ok_or_else(|| Error::NotFound("artifact".into()))
    }
}
