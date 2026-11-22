//! Workspace-scoped durable memory with revision checks and an access audit.
mod vectors;
use super::*;
use crate::query::page_range;
use rusqlite::{Row, Transaction};
use sha2::{Digest, Sha256};
use std::{
    cmp::Reverse,
    collections::BTreeMap,
    time::{SystemTime, UNIX_EPOCH},
};
pub(super) use vectors::migrate as migrate_vectors;

const MAX_WORKSPACE_MEMORIES: i64 = 4096;
const MAX_CONTEXT_MEMORY_CANDIDATES: usize = 4096;
const MAX_CONTEXT_MEMORY_TERMS: usize = 64;
const MAX_CONTEXT_MEMORY_RECORDS: usize = MAX_EXPLICIT_CONTEXT_MEMORIES;
const MAX_CONTEXT_MEMORY_BYTES: usize = 16 * 1024;

/// Host-supplied retrieval result, scoped to the same workspace as the task.
#[derive(Debug, Clone, Copy, Default)]
pub enum TaskMemoryQuery<'a> {
    #[default]
    Keyword,
    Unavailable,
    Semantic {
        workspace_id: &'a str,
        embedding: &'a WorkspaceMemoryQueryEmbedding,
        minimum_similarity: f64,
        result_limit: u32,
    },
}

pub(crate) struct ContextMemoryAccess {
    workspace_id: String,
    memories: Vec<WorkspaceMemory>,
}

pub(super) fn migrate(db: &mut Connection) -> Result<()> {
    let tx = db.transaction()?;
    tx.execute_batch(
        "CREATE TABLE IF NOT EXISTS workspace_memory_records(
            record_sequence INTEGER PRIMARY KEY AUTOINCREMENT,
            id TEXT NOT NULL UNIQUE,
            workspace_id TEXT NOT NULL REFERENCES workspaces(id),
            memory_key TEXT NOT NULL,
            content TEXT NOT NULL,
            source_task_id TEXT,
            revision INTEGER NOT NULL CHECK(revision>0),
            created_at_ms INTEGER NOT NULL CHECK(created_at_ms>=0),
            updated_at_ms INTEGER NOT NULL CHECK(updated_at_ms>=0),
            deleted_at_ms INTEGER CHECK(deleted_at_ms IS NULL OR deleted_at_ms>=0),
            UNIQUE(workspace_id,memory_key)
        );
        CREATE INDEX IF NOT EXISTS workspace_memory_page
            ON workspace_memory_records(workspace_id,record_sequence);
        CREATE TABLE IF NOT EXISTS workspace_memory_audit(
            sequence INTEGER PRIMARY KEY AUTOINCREMENT,
            workspace_id TEXT NOT NULL REFERENCES workspaces(id),
            memory_id TEXT,
            memory_key TEXT,
            action TEXT NOT NULL,
            revision INTEGER,
            source_task_id TEXT,
            actor_task_id TEXT,
            content_sha256 TEXT,
            occurred_at_ms INTEGER NOT NULL CHECK(occurred_at_ms>=0)
        );
        CREATE INDEX IF NOT EXISTS workspace_memory_audit_page
            ON workspace_memory_audit(workspace_id,sequence);
        PRAGMA user_version=55;",
    )?;
    tx.commit()?;
    Ok(())
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(i64::MAX as u128) as i64
}

fn validate_key(key: &str) -> Result<()> {
    let chars = key.chars().count();
    if !(1..=MAX_WORKSPACE_MEMORY_KEY_CHARS).contains(&chars)
        || key.trim() != key
        || key.chars().any(char::is_control)
    {
        return Err(Error::Invalid(format!(
            "memory key must have 1..{MAX_WORKSPACE_MEMORY_KEY_CHARS} trimmed characters and no control characters"
        )));
    }
    Ok(())
}

fn validate_content(content: &str) -> Result<()> {
    if content.is_empty()
        || content.len() > MAX_WORKSPACE_MEMORY_CONTENT_BYTES
        || content.contains('\0')
    {
        return Err(Error::Invalid(format!(
            "memory content must contain 1..{MAX_WORKSPACE_MEMORY_CONTENT_BYTES} UTF-8 bytes and no NUL"
        )));
    }
    Ok(())
}

fn validate_task_scope(
    tx: &Transaction<'_>,
    workspace_id: &str,
    task_id: Option<&str>,
    require_completed: bool,
) -> Result<()> {
    let Some(task_id) = task_id else {
        return Ok(());
    };
    if task_id.is_empty()
        || task_id.len() > 128
        || !task_id.bytes().all(|byte| (b'!'..=b'~').contains(&byte))
    {
        return Err(Error::Invalid("invalid memory task reference".into()));
    }
    let status: Option<String> = tx
        .query_row(
            "SELECT json_extract(t.data,'$.status') FROM tasks t
             JOIN sessions s ON s.id=t.session_id
             WHERE t.id=?1 AND s.workspace_id=?2",
            params![task_id, workspace_id],
            |row| row.get(0),
        )
        .optional()?;
    let Some(status) = status else {
        return Err(Error::Invalid(
            "memory task reference must belong to the selected workspace".into(),
        ));
    };
    if require_completed && status != "completed" {
        return Err(Error::Invalid(
            "memory source task must be completed".into(),
        ));
    }
    Ok(())
}

fn memory_row(row: &Row<'_>) -> rusqlite::Result<WorkspaceMemory> {
    Ok(WorkspaceMemory {
        workspace_id: row.get(0)?,
        id: row.get(1)?,
        key: row.get(2)?,
        content: row.get(3)?,
        source_task_id: row.get(4)?,
        revision: row.get::<_, i64>(5)? as u64,
        created_at_ms: row.get::<_, i64>(6)? as u64,
        updated_at_ms: row.get::<_, i64>(7)? as u64,
        deleted_at_ms: row.get::<_, Option<i64>>(8)?.map(|value| value as u64),
    })
}

const MEMORY_COLUMNS: &str = "workspace_id,id,memory_key,content,source_task_id,revision,created_at_ms,updated_at_ms,deleted_at_ms";

fn content_hash(content: &str) -> String {
    format!("{:x}", Sha256::digest(content.as_bytes()))
}

#[allow(clippy::too_many_arguments)]
fn audit(
    tx: &Transaction<'_>,
    workspace_id: &str,
    memory: Option<&WorkspaceMemory>,
    action: WorkspaceMemoryAction,
    actor_task_id: Option<&str>,
    include_content_hash: bool,
) -> Result<()> {
    let action = match action {
        WorkspaceMemoryAction::Created => "created",
        WorkspaceMemoryAction::Updated => "updated",
        WorkspaceMemoryAction::Restored => "restored",
        WorkspaceMemoryAction::Deleted => "deleted",
        WorkspaceMemoryAction::Read => "read",
        WorkspaceMemoryAction::Listed => "listed",
        WorkspaceMemoryAction::Searched => "searched",
        WorkspaceMemoryAction::ContextSearched => "context_searched",
        WorkspaceMemoryAction::ContextInjected => "context_injected",
        WorkspaceMemoryAction::Indexed => "indexed",
    };
    tx.execute(
        "INSERT INTO workspace_memory_audit(
             workspace_id,memory_id,memory_key,action,revision,source_task_id,
             actor_task_id,content_sha256,occurred_at_ms
         ) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9)",
        params![
            workspace_id,
            memory.map(|item| item.id.as_str()),
            memory.map(|item| item.key.as_str()),
            action,
            memory.map(|item| item.revision as i64),
            memory.and_then(|item| item.source_task_id.as_deref()),
            actor_task_id,
            memory
                .filter(|_| include_content_hash)
                .map(|item| content_hash(&item.content)),
            now_ms(),
        ],
    )?;
    Ok(())
}

pub(super) fn audit_context_access(
    tx: &Transaction<'_>,
    actor_task_id: &str,
    access: &ContextMemoryAccess,
) -> Result<()> {
    if access.memories.is_empty() {
        return audit(
            tx,
            &access.workspace_id,
            None,
            WorkspaceMemoryAction::ContextSearched,
            Some(actor_task_id),
            false,
        );
    }
    for memory in &access.memories {
        let still_current: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM workspace_memory_records
             WHERE id=?1 AND workspace_id=?2 AND revision=?3 AND deleted_at_ms IS NULL)",
            params![memory.id, access.workspace_id, memory.revision as i64],
            |row| row.get(0),
        )?;
        if !still_current {
            return Err(Error::Conflict(
                "memory changed while creating the task".into(),
            ));
        }
        audit(
            tx,
            &access.workspace_id,
            Some(memory),
            WorkspaceMemoryAction::ContextInjected,
            Some(actor_task_id),
            true,
        )?;
    }
    Ok(())
}

fn freeze(memory: &WorkspaceMemory, matched_by: ContextMemoryMatch) -> ContextMemory {
    ContextMemory {
        id: memory.id.clone(),
        key: memory.key.clone(),
        content: memory.content.clone(),
        revision: memory.revision,
        source_task_id: memory.source_task_id.clone(),
        matched_by: Some(matched_by),
    }
}

impl Kernel {
    pub(crate) fn inject_context_memories(
        &self,
        session_id: &str,
        input: &str,
        context: &mut TaskContext,
        query: TaskMemoryQuery<'_>,
    ) -> Result<Option<ContextMemoryAccess>> {
        if context.options.mode == ContextMode::Isolated
            || context.options.max_bytes == 0
            || context.options.disable_memory
        {
            return Ok(None);
        }
        if context.options.memory_selection.is_none()
            && input.trim().is_empty()
            && !matches!(query, TaskMemoryQuery::Semantic { .. })
        {
            return Ok(None);
        }

        let connection = &self.db;
        let workspace_id: Option<String> = connection
            .query_row(
                "SELECT workspace_id FROM sessions WHERE id=?1",
                [session_id],
                |row| row.get(0),
            )
            .optional()?;
        let workspace_id = workspace_id.ok_or_else(|| Error::NotFound("session".into()))?;
        if let Some(selection) = &context.options.memory_selection {
            let mut load = connection.prepare(&format!(
                "SELECT {MEMORY_COLUMNS} FROM workspace_memory_records
                 WHERE workspace_id=?1 AND id=?2 AND deleted_at_ms IS NULL"
            ))?;
            let mut memories = Vec::with_capacity(selection.len());
            let mut bytes = 0usize;
            for item in selection {
                let memory = load
                    .query_row(params![workspace_id, item.id], memory_row)
                    .optional()?
                    .ok_or_else(|| {
                        Error::NotFound("selected workspace memory is unavailable".into())
                    })?;
                if memory.revision != item.revision {
                    return Err(Error::Conflict(
                        "selected workspace memory revision changed".into(),
                    ));
                }
                bytes += memory.content.len();
                if bytes > MAX_CONTEXT_MEMORY_BYTES {
                    return Err(Error::Invalid(
                        "selected memory exceeds 16 KiB; choose fewer records".into(),
                    ));
                }
                context
                    .memories
                    .push(freeze(&memory, ContextMemoryMatch::Explicit));
                memories.push(memory);
            }
            if crate::json_size(context)? > context.options.max_bytes as usize {
                return Err(Error::Invalid(
                    "selected memory exceeds maxBytes; reduce history or choose fewer records"
                        .into(),
                ));
            }
            return Ok(Some(ContextMemoryAccess {
                workspace_id,
                memories,
            }));
        }
        let query_terms = lexical_terms(input, MAX_CONTEXT_MEMORY_TERMS);
        let (semantic, fallback) = rank_task_vectors(connection, &workspace_id, query)?;
        context.memory_fallback = fallback;
        if fallback.is_some() && crate::json_size(context)? > context.options.max_bytes as usize {
            context.memory_fallback = None;
        }
        let mut statement = connection.prepare(&format!(
            "SELECT {MEMORY_COLUMNS},record_sequence FROM workspace_memory_records
             WHERE workspace_id=?1 AND deleted_at_ms IS NULL
             ORDER BY record_sequence DESC LIMIT ?2"
        ))?;
        let rows = statement.query_map(
            params![workspace_id, MAX_CONTEXT_MEMORY_CANDIDATES as i64],
            |row| Ok((row.get::<_, i64>(9)?, memory_row(row)?)),
        )?;
        let mut candidates = Vec::new();
        for row in rows {
            let (sequence, memory) = row?;
            let terms = lexical_terms(&format!("{} {}", memory.key, memory.content), 512);
            let score = query_terms.intersection(&terms).count()
                + usize::from(contains_exact_key(input, &memory.key));
            let semantic_rank = semantic.get(&sequence).copied();
            if score > 0 || semantic_rank.is_some() {
                candidates.push((score, semantic_rank, memory));
            }
        }
        drop(statement);
        candidates.sort_by(|(left_score, _, left), (right_score, _, right)| {
            right_score
                .cmp(left_score)
                .then_with(|| right.updated_at_ms.cmp(&left.updated_at_ms))
                .then_with(|| left.key.cmp(&right.key))
        });
        // Fuse ranks so cosine calibration does not overwhelm lexical matches.
        let mut candidates: Vec<_> = candidates
            .into_iter()
            .enumerate()
            .map(|(rank, (keyword, semantic, memory))| {
                let score = if keyword > 0 {
                    120_000 / (30 + rank)
                } else {
                    0
                } + semantic.map_or(0, |rank| 100_000 / (30 + rank));
                let matched_by = match (keyword > 0, semantic.is_some()) {
                    (true, true) => ContextMemoryMatch::Both,
                    (true, false) => ContextMemoryMatch::Keyword,
                    _ => ContextMemoryMatch::Semantic,
                };
                (score, matched_by, memory)
            })
            .collect();
        if let Some(days) = context.options.memory_recency_half_life_days {
            let now = now_ms().max(0) as u64;
            let half_life_ms = f64::from(days) * 86_400_000.0;
            for (score, _, memory) in &mut candidates {
                let half_lives = now.saturating_sub(memory.updated_at_ms) as f64 / half_life_ms;
                // Even old records retain 80% of their relevance score.
                *score = (*score as f64 * (0.8 + 0.2 * (-half_lives).exp2())).round() as usize;
            }
        }
        if !semantic.is_empty() || context.options.memory_recency_half_life_days.is_some() {
            candidates.sort_by_key(|entry| Reverse(entry.0));
        }

        let mut selected_bytes = 0;
        let mut selected = Vec::new();
        for (_, matched_by, memory) in candidates {
            if selected.len() == MAX_CONTEXT_MEMORY_RECORDS {
                break;
            }
            if selected_bytes + memory.content.len() > MAX_CONTEXT_MEMORY_BYTES {
                continue;
            }
            context.memories.push(freeze(&memory, matched_by));
            if crate::json_size(context)? > context.options.max_bytes as usize {
                context.memories.pop();
                continue;
            }
            selected_bytes += memory.content.len();
            selected.push(memory);
        }
        Ok(Some(ContextMemoryAccess {
            workspace_id,
            memories: selected,
        }))
    }

    pub fn put_workspace_memory(
        &mut self,
        params: &WorkspaceMemoryPutParams,
    ) -> Result<WorkspaceMemory> {
        self.require_workspace(&params.workspace_id)?;
        validate_key(&params.key)?;
        validate_content(&params.content)?;
        validate_revision(params.expected_revision)?;
        let tx = self.db.transaction()?;
        validate_task_scope(
            &tx,
            &params.workspace_id,
            params.source_task_id.as_deref(),
            true,
        )?;
        let existing = tx
            .query_row(
                &format!("SELECT {MEMORY_COLUMNS} FROM workspace_memory_records WHERE workspace_id=?1 AND memory_key=?2"),
                params![params.workspace_id, params.key],
                memory_row,
            )
            .optional()?;
        let now = now_ms();
        let (memory_id, action) = match existing {
            None => {
                if params
                    .expected_revision
                    .is_some_and(|revision| revision != 0)
                {
                    return Err(Error::Conflict("workspace memory revision changed".into()));
                }
                let count: i64 = tx.query_row(
                    "SELECT COUNT(*) FROM workspace_memory_records WHERE workspace_id=?1",
                    [&params.workspace_id],
                    |row| row.get(0),
                )?;
                if count >= MAX_WORKSPACE_MEMORIES {
                    return Err(Error::Conflict("workspace memory limit reached".into()));
                }
                let id = format!("memory_{}", uuid::Uuid::new_v4());
                tx.execute(
                    "INSERT INTO workspace_memory_records(
                        id,workspace_id,memory_key,content,source_task_id,revision,
                        created_at_ms,updated_at_ms,deleted_at_ms
                    ) VALUES(?1,?2,?3,?4,?5,1,?6,?6,NULL)",
                    params![
                        id,
                        params.workspace_id,
                        params.key,
                        params.content,
                        params.source_task_id,
                        now
                    ],
                )?;
                (id, WorkspaceMemoryAction::Created)
            }
            Some(mut memory) => {
                if params
                    .expected_revision
                    .is_some_and(|revision| revision != memory.revision)
                {
                    return Err(Error::Conflict("workspace memory revision changed".into()));
                }
                if memory.deleted_at_ms.is_none()
                    && memory.content == params.content
                    && memory.source_task_id == params.source_task_id
                {
                    tx.commit()?;
                    return Ok(memory);
                }
                if params.expected_revision != Some(memory.revision) {
                    return Err(Error::Conflict(
                        "updating or restoring memory requires its current expectedRevision".into(),
                    ));
                }
                let action = if memory.deleted_at_ms.is_some() {
                    WorkspaceMemoryAction::Restored
                } else {
                    WorkspaceMemoryAction::Updated
                };
                memory.content.clone_from(&params.content);
                memory.source_task_id.clone_from(&params.source_task_id);
                memory.revision += 1;
                memory.updated_at_ms = now as u64;
                memory.deleted_at_ms = None;
                tx.execute(
                    "UPDATE workspace_memory_records SET content=?3,source_task_id=?4,
                        revision=?5,updated_at_ms=?6,deleted_at_ms=NULL
                     WHERE workspace_id=?1 AND memory_key=?2",
                    params![
                        params.workspace_id,
                        params.key,
                        params.content,
                        params.source_task_id,
                        memory.revision as i64,
                        now
                    ],
                )?;
                (memory.id, action)
            }
        };
        let memory = tx.query_row(
            &format!("SELECT {MEMORY_COLUMNS} FROM workspace_memory_records WHERE id=?1"),
            [&memory_id],
            memory_row,
        )?;
        audit(&tx, &params.workspace_id, Some(&memory), action, None, true)?;
        tx.commit()?;
        Ok(memory)
    }

    pub fn get_workspace_memory(
        &mut self,
        params: &WorkspaceMemoryGetParams,
    ) -> Result<WorkspaceMemory> {
        self.require_workspace(&params.workspace_id)?;
        let tx = self.db.transaction()?;
        validate_task_scope(
            &tx,
            &params.workspace_id,
            params.actor_task_id.as_deref(),
            false,
        )?;
        let memory = tx
            .query_row(
                &format!(
                    "SELECT {MEMORY_COLUMNS} FROM workspace_memory_records
                    WHERE workspace_id=?1 AND memory_key=?2 AND deleted_at_ms IS NULL"
                ),
                params![params.workspace_id, params.key],
                memory_row,
            )
            .optional()?
            .ok_or_else(|| Error::NotFound("workspace memory".into()))?;
        audit(
            &tx,
            &params.workspace_id,
            Some(&memory),
            WorkspaceMemoryAction::Read,
            params.actor_task_id.as_deref(),
            false,
        )?;
        tx.commit()?;
        Ok(memory)
    }

    pub fn list_workspace_memories(
        &mut self,
        params: &WorkspaceMemoryListParams,
    ) -> Result<ObjectPage<WorkspaceMemory>> {
        self.require_workspace(&params.workspace_id)?;
        let (cursor, fetch) = page_range(params.after, params.limit)?;
        let tx = self.db.transaction()?;
        validate_task_scope(
            &tx,
            &params.workspace_id,
            params.actor_task_id.as_deref(),
            false,
        )?;
        let deleted = if params.include_deleted {
            ""
        } else {
            "AND deleted_at_ms IS NULL"
        };
        let sql = format!(
            "SELECT record_sequence,{MEMORY_COLUMNS} FROM workspace_memory_records
            WHERE workspace_id=?1 AND record_sequence>?2 {deleted}
            ORDER BY record_sequence LIMIT ?3"
        );
        let mut statement = tx.prepare(&sql)?;
        let mut rows = statement.query(params![params.workspace_id, cursor, fetch])?;
        let mut page = ObjectPage {
            items: Vec::with_capacity(params.limit as usize),
            next_after: params.after,
            has_more: false,
        };
        while let Some(row) = rows.next()? {
            if page.items.len() == params.limit as usize {
                page.has_more = true;
                break;
            }
            page.next_after = row.get::<_, i64>(0)? as u64;
            page.items.push(memory_row_at(row, 1)?);
        }
        drop(rows);
        drop(statement);
        audit(
            &tx,
            &params.workspace_id,
            None,
            WorkspaceMemoryAction::Listed,
            params.actor_task_id.as_deref(),
            false,
        )?;
        tx.commit()?;
        Ok(page)
    }

    pub fn search_workspace_memories(
        &mut self,
        params: &WorkspaceMemorySearchParams,
    ) -> Result<ObjectPage<WorkspaceMemory>> {
        self.require_workspace(&params.workspace_id)?;
        validate_query(&params.query)?;
        let (cursor, fetch) = page_range(params.after, params.limit)?;
        let tx = self.db.transaction()?;
        validate_task_scope(
            &tx,
            &params.workspace_id,
            params.actor_task_id.as_deref(),
            false,
        )?;
        let mut statement = tx.prepare(&format!(
            "SELECT record_sequence,{MEMORY_COLUMNS} FROM workspace_memory_records
             WHERE workspace_id=?1 AND record_sequence>?2 AND deleted_at_ms IS NULL
               AND (instr(memory_key,?3)>0 OR instr(content,?3)>0)
             ORDER BY record_sequence LIMIT ?4"
        ))?;
        let mut rows =
            statement.query(params![params.workspace_id, cursor, params.query, fetch])?;
        let mut page = ObjectPage {
            items: Vec::with_capacity(params.limit as usize),
            next_after: params.after,
            has_more: false,
        };
        while let Some(row) = rows.next()? {
            if page.items.len() == params.limit as usize {
                page.has_more = true;
                break;
            }
            page.next_after = row.get::<_, i64>(0)? as u64;
            page.items.push(memory_row_at(row, 1)?);
        }
        drop(rows);
        drop(statement);
        audit(
            &tx,
            &params.workspace_id,
            None,
            WorkspaceMemoryAction::Searched,
            params.actor_task_id.as_deref(),
            false,
        )?;
        tx.commit()?;
        Ok(page)
    }

    pub fn delete_workspace_memory(
        &mut self,
        params: &WorkspaceMemoryDeleteParams,
    ) -> Result<WorkspaceMemory> {
        self.require_workspace(&params.workspace_id)?;
        validate_key(&params.key)?;
        validate_revision(Some(params.expected_revision))?;
        let tx = self.db.transaction()?;
        validate_task_scope(
            &tx,
            &params.workspace_id,
            params.actor_task_id.as_deref(),
            false,
        )?;
        let mut memory = tx
            .query_row(
                &format!("SELECT {MEMORY_COLUMNS} FROM workspace_memory_records WHERE workspace_id=?1 AND memory_key=?2"),
                params![params.workspace_id, params.key],
                memory_row,
            )
            .optional()?
            .ok_or_else(|| Error::NotFound("workspace memory".into()))?;
        if memory.revision != params.expected_revision {
            return Err(Error::Conflict("workspace memory revision changed".into()));
        }
        if memory.deleted_at_ms.is_some() {
            tx.commit()?;
            return Ok(memory);
        }
        let now = now_ms();
        memory.revision += 1;
        memory.updated_at_ms = now as u64;
        memory.deleted_at_ms = Some(now as u64);
        tx.execute(
            "UPDATE workspace_memory_records SET revision=?3,updated_at_ms=?4,deleted_at_ms=?4
             WHERE workspace_id=?1 AND memory_key=?2",
            params![params.workspace_id, params.key, memory.revision as i64, now],
        )?;
        audit(
            &tx,
            &params.workspace_id,
            Some(&memory),
            WorkspaceMemoryAction::Deleted,
            params.actor_task_id.as_deref(),
            true,
        )?;
        tx.commit()?;
        Ok(memory)
    }

    pub fn list_workspace_memory_audit(
        &self,
        params: &WorkspaceMemoryAuditListParams,
    ) -> Result<ObjectPage<WorkspaceMemoryAuditEntry>> {
        self.require_workspace(&params.workspace_id)?;
        let (cursor, fetch) = page_range(params.after, params.limit)?;
        let mut statement = self.db.prepare_cached(
            "SELECT sequence,workspace_id,memory_id,memory_key,action,revision,
                    source_task_id,actor_task_id,content_sha256,occurred_at_ms
             FROM workspace_memory_audit WHERE workspace_id=?1 AND sequence>?2
             ORDER BY sequence LIMIT ?3",
        )?;
        let mut rows = statement.query(params![params.workspace_id, cursor, fetch])?;
        let mut page = ObjectPage {
            items: Vec::with_capacity(params.limit as usize),
            next_after: params.after,
            has_more: false,
        };
        while let Some(row) = rows.next()? {
            if page.items.len() == params.limit as usize {
                page.has_more = true;
                break;
            }
            page.next_after = row.get::<_, i64>(0)? as u64;
            page.items.push(audit_row(row)?);
        }
        Ok(page)
    }
}

fn rank_task_vectors(
    db: &Connection,
    workspace_id: &str,
    query: TaskMemoryQuery<'_>,
) -> Result<(BTreeMap<i64, usize>, Option<WorkspaceMemoryFallback>)> {
    let mut ranks = BTreeMap::new();
    let fallback = match query {
        TaskMemoryQuery::Keyword => None,
        TaskMemoryQuery::Unavailable => Some(WorkspaceMemoryFallback::EmbeddingUnavailable),
        TaskMemoryQuery::Semantic {
            workspace_id: scope,
            embedding,
            minimum_similarity,
            result_limit,
        } => {
            if scope != workspace_id
                || !(0.0 < minimum_similarity && minimum_similarity <= 1.0)
                || !(1..=MAX_MEMORY_SEMANTIC_RESULTS).contains(&result_limit)
            {
                return Err(Error::Invalid(
                    "invalid task memory query scope or similarity threshold".into(),
                ));
            }
            let scores = vectors::validate_model(&embedding.model_identity)
                .and_then(|_| vectors::normalize(&embedding.values))
                .and_then(|normalized| {
                    vectors::rank_scores(db, workspace_id, &embedding.model_identity, &normalized)
                });
            match scores {
                Ok(Ok(scores)) => {
                    ranks.extend(
                        scores
                            .into_iter()
                            .filter(|(_, score)| *score >= minimum_similarity)
                            .take(result_limit as usize)
                            .enumerate()
                            .map(|(rank, (id, _))| (id, rank)),
                    );
                    ranks
                        .is_empty()
                        .then_some(WorkspaceMemoryFallback::BelowSimilarityThreshold)
                }
                Ok(Err(reason)) => Some(reason),
                Err(_) => Some(WorkspaceMemoryFallback::CorruptIndex),
            }
        }
    };
    Ok((ranks, fallback))
}

fn contains_exact_key(input: &str, key: &str) -> bool {
    let word = |ch: char| ch.is_alphanumeric() || ch == '_';
    input.match_indices(key).any(|(start, key)| {
        !input[..start].chars().next_back().is_some_and(word)
            && !input[start + key.len()..].chars().next().is_some_and(word)
    })
}

fn lexical_terms(text: &str, limit: usize) -> std::collections::BTreeSet<String> {
    fn flush_word(word: &mut String, terms: &mut std::collections::BTreeSet<String>, limit: usize) {
        // Generic English words alone should not inject a workspace memory.
        // Explicit keys are matched separately against the original input.
        let common = matches!(
            word.as_str(),
            "an" | "and"
                | "are"
                | "as"
                | "at"
                | "be"
                | "by"
                | "do"
                | "for"
                | "from"
                | "how"
                | "in"
                | "is"
                | "it"
                | "me"
                | "my"
                | "of"
                | "on"
                | "or"
                | "should"
                | "that"
                | "the"
                | "this"
                | "to"
                | "use"
                | "was"
                | "what"
                | "when"
                | "where"
                | "which"
                | "with"
                | "would"
                | "you"
                | "your"
        );
        if !common && word.chars().count() >= 2 && terms.len() < limit {
            terms.insert(std::mem::take(word));
        } else {
            word.clear();
        }
    }

    fn flush_cjk(
        chars: &mut Vec<char>,
        terms: &mut std::collections::BTreeSet<String>,
        limit: usize,
    ) {
        for pair in chars.windows(2) {
            if terms.len() >= limit {
                break;
            }
            terms.insert(pair.iter().collect());
        }
        chars.clear();
    }

    let mut terms = std::collections::BTreeSet::new();
    let mut word = String::new();
    let mut cjk = Vec::new();
    for ch in text.chars().flat_map(char::to_lowercase) {
        let code = ch as u32;
        let is_cjk = matches!(code, 0x3400..=0x9fff | 0xf900..=0xfaff | 0x20000..=0x3134f)
            || matches!(code, 0x3040..=0x30ff | 0xac00..=0xd7af);
        if is_cjk {
            flush_word(&mut word, &mut terms, limit);
            cjk.push(ch);
        } else if ch.is_alphanumeric() {
            flush_cjk(&mut cjk, &mut terms, limit);
            word.push(ch);
        } else {
            flush_word(&mut word, &mut terms, limit);
            flush_cjk(&mut cjk, &mut terms, limit);
        }
        if terms.len() >= limit {
            break;
        }
    }
    flush_word(&mut word, &mut terms, limit);
    flush_cjk(&mut cjk, &mut terms, limit);
    terms
}

fn validate_revision(revision: Option<u64>) -> Result<()> {
    if revision.is_some_and(|value| value > i64::MAX as u64) {
        return Err(Error::Invalid("memory revision must fit i64".into()));
    }
    Ok(())
}

fn validate_query(query: &str) -> Result<()> {
    let chars = query.chars().count();
    if !(1..=MAX_WORKSPACE_MEMORY_QUERY_CHARS).contains(&chars)
        || query.trim().is_empty()
        || query.chars().any(char::is_control)
    {
        return Err(Error::Invalid(format!(
            "memory query must have 1..{MAX_WORKSPACE_MEMORY_QUERY_CHARS} printable characters"
        )));
    }
    Ok(())
}

fn memory_row_at(row: &Row<'_>, offset: usize) -> rusqlite::Result<WorkspaceMemory> {
    Ok(WorkspaceMemory {
        workspace_id: row.get(offset)?,
        id: row.get(offset + 1)?,
        key: row.get(offset + 2)?,
        content: row.get(offset + 3)?,
        source_task_id: row.get(offset + 4)?,
        revision: row.get::<_, i64>(offset + 5)? as u64,
        created_at_ms: row.get::<_, i64>(offset + 6)? as u64,
        updated_at_ms: row.get::<_, i64>(offset + 7)? as u64,
        deleted_at_ms: row
            .get::<_, Option<i64>>(offset + 8)?
            .map(|value| value as u64),
    })
}

fn audit_row(row: &Row<'_>) -> rusqlite::Result<WorkspaceMemoryAuditEntry> {
    let action = match row.get::<_, String>(4)?.as_str() {
        "created" => WorkspaceMemoryAction::Created,
        "updated" => WorkspaceMemoryAction::Updated,
        "restored" => WorkspaceMemoryAction::Restored,
        "deleted" => WorkspaceMemoryAction::Deleted,
        "read" => WorkspaceMemoryAction::Read,
        "listed" => WorkspaceMemoryAction::Listed,
        "searched" => WorkspaceMemoryAction::Searched,
        "context_searched" => WorkspaceMemoryAction::ContextSearched,
        "context_injected" => WorkspaceMemoryAction::ContextInjected,
        "indexed" => WorkspaceMemoryAction::Indexed,
        _ => return Err(rusqlite::Error::InvalidQuery),
    };
    Ok(WorkspaceMemoryAuditEntry {
        sequence: row.get::<_, i64>(0)? as u64,
        workspace_id: row.get(1)?,
        memory_id: row.get(2)?,
        key: row.get(3)?,
        action,
        revision: row.get::<_, Option<i64>>(5)?.map(|value| value as u64),
        source_task_id: row.get(6)?,
        actor_task_id: row.get(7)?,
        content_sha256: row.get(8)?,
        occurred_at_ms: row.get::<_, i64>(9)? as u64,
    })
}
