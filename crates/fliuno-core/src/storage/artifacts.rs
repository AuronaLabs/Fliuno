//! Task-scoped artifact reads. Only the legacy list loads every body.
use super::*;

impl Kernel {
    pub fn list_artifacts(&self, task_id: &str) -> Result<Vec<Artifact>> {
        self.require_task(task_id)?;
        let mut stmt = self
            .db
            .prepare("SELECT data FROM artifacts WHERE task_id=?1 ORDER BY rowid")?;
        decode_rows(stmt.query_map([task_id], |row| row.get::<_, String>(0))?)
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
        // SQLite extracts the metadata from both legacy and new rows without
        // copying or deserializing the artifact body into Rust or onto the wire.
        let mut statement = self.db.prepare_cached(
            "SELECT rowid, id, json_extract(data,'$.title'), json_extract(data,'$.mediaType'),
             length(CAST(json_extract(data,'$.content') AS BLOB)) FROM artifacts
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

    pub fn get_artifact(&self, task_id: &str, artifact_id: &str) -> Result<Artifact> {
        let data = self
            .db
            .query_row(
                "SELECT data FROM artifacts WHERE task_id=?1 AND id=?2",
                params![task_id, artifact_id],
                |row| row.get::<_, String>(0),
            )
            .optional()?
            .ok_or_else(|| Error::NotFound("artifact".into()))?;
        Ok(serde_json::from_str(&data)?)
    }
}
