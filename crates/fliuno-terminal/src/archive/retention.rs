//! Opt-in maintenance for old, settled spools. The caller holds registry.lock.
use super::*;

const ARCHIVES_PER_PASS: usize = 8;

pub(crate) fn sweep(
    service: &Service,
    ledger: &mut Registry,
    now: u64,
    grace_ms: u64,
) -> Result<(), ToolError> {
    let Some(cutoff) = now.checked_sub(grace_ms) else {
        return Ok(());
    };
    let mut archived = 0;
    for entry in fs::read_dir(&*service.root)
        .map_err(|_| "cannot inspect terminal retention candidates")?
        .take(crate::service::REGISTRY_SCAN_LIMIT)
    {
        let name = entry
            .map_err(|_| "cannot inspect terminal retention candidate")?
            .file_name();
        let Some(id) = name.to_str().filter(|id| registry::valid_id(id).is_ok()) else {
            continue;
        };
        if ledger.get(id)?.is_some() {
            continue;
        }
        let Ok(directory) = service.directory(id) else {
            continue;
        };
        let Ok(snapshot) = archive_snapshot(&directory, id) else {
            continue;
        };
        if !eligible(&snapshot, cutoff) {
            continue;
        }
        let Ok(owner) = owner_lock(&directory) else {
            continue;
        };
        // The supervisor may have committed a newer snapshot before releasing
        // ownership. Eligibility must hold for the snapshot we archive.
        let Ok(snapshot) = archive_snapshot(&directory, id) else {
            continue;
        };
        if eligible(&snapshot, cutoff) {
            archive_owned(ledger, &directory, owner, snapshot, now)?;
            archived += 1;
            if archived == ARCHIVES_PER_PASS {
                break;
            }
        }
    }
    Ok(())
}

fn eligible(snapshot: &Snapshot, cutoff: u64) -> bool {
    snapshot.expires_at_unix_ms != 0
        && snapshot.expires_at_unix_ms <= cutoff
        && match snapshot.status {
            Status::Failed => true,
            Status::Exited | Status::Killed | Status::Expired | Status::Released => {
                snapshot.output_closed
            }
            Status::Starting | Status::Running | Status::OutcomeUnknown => false,
        }
}
