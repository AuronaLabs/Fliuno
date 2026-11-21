//! Derived vectors are never the source of memory text or permissions.
use super::*;
use std::collections::BTreeSet;

pub(in crate::storage) fn migrate(db: &Connection) -> Result<()> {
    db.execute_batch(
        "BEGIN IMMEDIATE;
         CREATE TABLE IF NOT EXISTS workspace_memory_vectors(
             memory_id TEXT PRIMARY KEY REFERENCES workspace_memory_records(id) ON DELETE CASCADE,
             revision INTEGER NOT NULL,
             model_identity TEXT NOT NULL,
             dimensions INTEGER NOT NULL,
             vector BLOB NOT NULL,
             checksum BLOB NOT NULL
         );
         CREATE TRIGGER IF NOT EXISTS workspace_memory_vector_invalidate
             AFTER UPDATE OF revision ON workspace_memory_records
             WHEN OLD.revision <> NEW.revision BEGIN
                 DELETE FROM workspace_memory_vectors WHERE memory_id=NEW.id;
             END;
         PRAGMA user_version=62; COMMIT;",
    )?;
    Ok(())
}

fn validate_model(identity: &str) -> Result<()> {
    if identity.is_empty()
        || identity.len() > 128
        || identity.trim() != identity
        || identity.chars().any(char::is_control)
    {
        return Err(Error::Invalid(
            "invalid memory embedding model identity".into(),
        ));
    }
    Ok(())
}

fn normalize(values: &[f32]) -> Result<Vec<f32>> {
    if !(1..=MAX_MEMORY_EMBEDDING_DIMENSIONS).contains(&values.len()) {
        return Err(Error::Invalid(
            "memory embedding has invalid dimensions".into(),
        ));
    }
    let norm_squared = values.iter().map(|&v| f64::from(v).powi(2)).sum::<f64>();
    if !norm_squared.is_finite() || norm_squared == 0.0 {
        return Err(Error::Invalid(
            "memory embedding must be finite and nonzero".into(),
        ));
    }
    let norm = norm_squared.sqrt();
    Ok(values
        .iter()
        .map(|&v| (f64::from(v) / norm) as f32)
        .collect())
}

impl Kernel {
    pub fn index_workspace_memory_vectors(
        &mut self,
        params: &WorkspaceMemoryIndexParams,
    ) -> Result<usize> {
        self.require_workspace(&params.workspace_id)?;
        validate_model(&params.model_identity)?;
        if !(1..=MAX_MEMORY_INDEX_BATCH).contains(&params.vectors.len()) {
            return Err(Error::Invalid("invalid memory embedding batch size".into()));
        }
        let dimensions = params.vectors[0].values.len();
        let mut seen = BTreeSet::new();
        let mut normalized = Vec::with_capacity(params.vectors.len());
        for item in &params.vectors {
            if item.memory_id.is_empty()
                || item.memory_id.len() > 128
                || !seen.insert(&item.memory_id)
                || item.values.len() != dimensions
            {
                return Err(Error::Invalid(
                    "memory vector IDs must be distinct and dimensions consistent".into(),
                ));
            }
            validate_revision(Some(item.revision))?;
            let vector = normalize(&item.values)?
                .into_iter()
                .flat_map(f32::to_le_bytes)
                .collect::<Vec<_>>();
            normalized.push(vector);
        }
        let tx = self.db.transaction()?;
        for (item, vector) in params.vectors.iter().zip(&normalized) {
            let checksum = Sha256::digest(vector);
            let written = tx.execute(
                "INSERT INTO workspace_memory_vectors
                 SELECT id,revision,?4,?5,?6,?7 FROM workspace_memory_records
                 WHERE id=?1 AND workspace_id=?2 AND revision=?3 AND deleted_at_ms IS NULL
                 ON CONFLICT(memory_id) DO UPDATE SET revision=excluded.revision,
                    model_identity=excluded.model_identity,dimensions=excluded.dimensions,
                    vector=excluded.vector,checksum=excluded.checksum",
                params![
                    item.memory_id,
                    params.workspace_id,
                    item.revision as i64,
                    params.model_identity,
                    dimensions as i64,
                    vector,
                    &checksum[..]
                ],
            )?;
            if written != 1 {
                return Err(Error::Conflict(
                    "memory index entry is absent, deleted, or has a different revision".into(),
                ));
            }
        }
        audit(
            &tx,
            &params.workspace_id,
            None,
            WorkspaceMemoryAction::Indexed,
            None,
            false,
        )?;
        tx.commit()?;
        Ok(normalized.len())
    }

    pub fn semantic_search_workspace_memories(
        &mut self,
        params: &WorkspaceMemorySemanticSearchParams,
    ) -> Result<WorkspaceMemorySemanticResult> {
        self.require_workspace(&params.workspace_id)?;
        validate_query(&params.query)?;
        if !(1..=MAX_MEMORY_SEMANTIC_RESULTS).contains(&params.limit) {
            return Err(Error::Invalid(
                "invalid semantic memory result limit".into(),
            ));
        }
        let reason = if let Some(embedding) = &params.embedding {
            validate_model(&embedding.model_identity)?;
            let query = normalize(&embedding.values)?;
            let tx = self.db.transaction()?;
            validate_task_scope(
                &tx,
                &params.workspace_id,
                params.actor_task_id.as_deref(),
                false,
            )?;
            match rank(&tx, params, &embedding.model_identity, &query) {
                Ok(Ok(items)) => {
                    audit(
                        &tx,
                        &params.workspace_id,
                        None,
                        WorkspaceMemoryAction::Searched,
                        params.actor_task_id.as_deref(),
                        false,
                    )?;
                    tx.commit()?;
                    return Ok(WorkspaceMemorySemanticResult {
                        mode: WorkspaceMemorySearchMode::Semantic,
                        fallback_reason: None,
                        items,
                    });
                }
                Ok(Err(reason)) => reason,
                Err(_) => WorkspaceMemoryFallback::CorruptIndex,
            }
        } else {
            WorkspaceMemoryFallback::EmbeddingUnavailable
        };
        let fallback = self.search_workspace_memories(&WorkspaceMemorySearchParams {
            workspace_id: params.workspace_id.clone(),
            query: params.query.clone(),
            after: 0,
            limit: params.limit,
            actor_task_id: params.actor_task_id.clone(),
        })?;
        Ok(WorkspaceMemorySemanticResult {
            mode: WorkspaceMemorySearchMode::Keyword,
            fallback_reason: Some(reason),
            items: fallback
                .items
                .into_iter()
                .map(|memory| WorkspaceMemorySemanticMatch {
                    memory,
                    similarity: None,
                })
                .collect(),
        })
    }
}

fn rank(
    tx: &Transaction<'_>,
    params: &WorkspaceMemorySemanticSearchParams,
    model: &str,
    query: &[f32],
) -> Result<std::result::Result<Vec<WorkspaceMemorySemanticMatch>, WorkspaceMemoryFallback>> {
    let mut select = tx.prepare_cached(
        "SELECT m.record_sequence,v.vector,v.checksum
         FROM workspace_memory_records m JOIN workspace_memory_vectors v
         ON m.id=v.memory_id AND m.revision=v.revision
         WHERE m.workspace_id=?1 AND m.deleted_at_ms IS NULL AND v.model_identity=?2 AND v.dimensions=?3",
    )?;
    let mut rows = select.query(params![params.workspace_id, model, query.len() as i64])?;
    let mut scores = Vec::new();
    let mut indexed = false;
    while let Some(row) = rows.next()? {
        indexed = true;
        let (Ok(vector), Ok(expected)) = (row.get_ref(1)?.as_blob(), row.get_ref(2)?.as_blob())
        else {
            return Ok(Err(WorkspaceMemoryFallback::CorruptIndex));
        };
        if vector.len() != query.len() * 4 || Sha256::digest(vector)[..] != *expected {
            return Ok(Err(WorkspaceMemoryFallback::CorruptIndex));
        }
        let similarity: f64 = vector
            .as_chunks::<4>()
            .0
            .iter()
            .zip(query)
            .map(|(bytes, &q)| {
                let weight = f32::from_le_bytes(*bytes);
                f64::from(weight) * f64::from(q)
            })
            .sum();
        if !similarity.is_finite() {
            return Ok(Err(WorkspaceMemoryFallback::CorruptIndex));
        }
        if similarity > 0.0 {
            scores.push((row.get::<_, i64>(0)?, similarity));
        }
    }
    if scores.is_empty() {
        return Ok(Err(if indexed {
            WorkspaceMemoryFallback::NoPositiveMatch
        } else {
            WorkspaceMemoryFallback::NoCompatibleIndex
        }));
    }
    scores.sort_unstable_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    let mut read = tx.prepare_cached(&format!(
        "SELECT {MEMORY_COLUMNS} FROM workspace_memory_records WHERE record_sequence=?1"
    ))?;
    let items = scores
        .into_iter()
        .take(params.limit as usize)
        .map(|(id, similarity)| {
            Ok(WorkspaceMemorySemanticMatch {
                memory: read.query_row([id], memory_row)?,
                similarity: Some(similarity.min(1.0)),
            })
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(Ok(items))
}
