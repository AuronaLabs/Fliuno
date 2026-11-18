//! Bounded client-selected maintenance, composed from the single-ID archive
//! boundary. Do not hold a root-wide lock or pretend the batch is one commit.
use crate::{Service, registry};
use fliuno_core::ToolError;
use fliuno_protocol::{MAX_TERMINAL_ARCHIVE_BATCH, TerminalArchiveBatch, TerminalArchiveBatchItem};
use std::collections::BTreeSet;

impl Service {
    /// Validate every ID before the first IO. Valid entries are attempted in
    /// request order, with independent durable tombstones and per-item errors.
    /// This is a trusted host-client operation; model tools use single-ID
    /// archive approvals and do not gain an implicit batch/retention grant.
    pub fn archive_batch(
        &self,
        session: &str,
        ids: &[String],
    ) -> Result<TerminalArchiveBatch, String> {
        if ids.is_empty() || ids.len() > MAX_TERMINAL_ARCHIVE_BATCH {
            return Err("terminal archive batch requires 1..32 IDs".into());
        }
        let mut seen = BTreeSet::new();
        for id in ids {
            registry::valid_id(id)?;
            if !seen.insert(id) {
                return Err("duplicate terminal ID in archive batch".into());
            }
        }
        let items = ids
            .iter()
            .map(|id| match self.archive(session, id) {
                Ok(archive) => TerminalArchiveBatchItem::Archived {
                    terminal_id: id.clone(),
                    archive: Box::new(archive),
                },
                Err(ToolError::Failed(message)) => TerminalArchiveBatchItem::Failed {
                    terminal_id: id.clone(),
                    message,
                },
                Err(ToolError::OutcomeUnknown(message))
                | Err(ToolError::OutcomeUnknownWithOutput { message, .. }) => {
                    TerminalArchiveBatchItem::OutcomeUnknown {
                        terminal_id: id.clone(),
                        message,
                    }
                }
            })
            .collect();
        Ok(TerminalArchiveBatch { items })
    }
}
