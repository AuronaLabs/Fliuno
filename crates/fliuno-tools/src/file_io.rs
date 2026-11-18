//! File bytes are bounded independently of metadata, which may change while a
//! descriptor is open. Unix opens must not wait for a FIFO peer before the
//! opened descriptor can be checked. This is not a deadline for storage IO.
use crate::{CancellationToken, READ_LIMIT, ToolError};
use cap_std::fs::{File, OpenOptions};
use std::io::Read;

pub(crate) fn options(writable: bool) -> OpenOptions {
    let mut options = OpenOptions::new();
    options.read(true).write(writable);
    #[cfg(unix)]
    {
        use cap_fs_ext::OpenOptionsSyncExt;
        options.nonblock(true);
    }
    options
}

/// Verify the descriptor itself, then retain the byte limit even if another
/// process grows its file. Never return a partial successful snapshot.
pub(crate) fn read(
    file: &mut File,
    cancellation: &CancellationToken,
) -> Result<Vec<u8>, ToolError> {
    require_regular_file(file, READ_BUDGET, cancellation)?;
    read_bounded(file, cancellation)
}

pub(super) type ReadBudget = (u64, &'static str);
const READ_BUDGET: ReadBudget = (READ_LIMIT, "file exceeds 256 KiB");

pub(super) fn require_regular_file(
    file: &File,
    (limit, too_large): ReadBudget,
    cancellation: &CancellationToken,
) -> Result<(), ToolError> {
    if cancellation.is_cancelled() {
        return Err("tool cancelled before file read".into());
    }
    let metadata = file.metadata().map_err(|_| "cannot read file metadata")?;
    if !metadata.is_file() {
        return Err("path is not a regular file".into());
    }
    if metadata.len() > limit {
        return Err(too_large.into());
    }
    Ok(())
}

fn read_bounded(reader: impl Read, cancellation: &CancellationToken) -> Result<Vec<u8>, ToolError> {
    let mut bytes = Vec::new();
    read_chunks(reader, READ_BUDGET, cancellation, |chunk| {
        bytes.extend_from_slice(chunk)
    })?;
    Ok(bytes)
}

pub(super) fn read_chunks(
    reader: impl Read,
    (limit, too_large): ReadBudget,
    cancellation: &CancellationToken,
    mut consume: impl FnMut(&[u8]),
) -> Result<u64, ToolError> {
    let mut reader = reader.take(limit + 1);
    let mut bytes = 0;
    let mut chunk = [0; 16 * 1024];
    loop {
        if cancellation.is_cancelled() {
            return Err("tool cancelled during file read".into());
        }
        let count = match reader.read(&mut chunk) {
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => return Err("cannot read workspace file".into()),
            Ok(count) => count,
        };
        // A reader can observe cancellation while returning EOF, too.
        if cancellation.is_cancelled() {
            return Err("tool cancelled during file read".into());
        }
        if count == 0 {
            return Ok(bytes);
        }
        bytes += count as u64;
        if bytes > limit {
            return Err(too_large.into());
        }
        consume(&chunk[..count]);
    }
}

#[cfg(test)]
mod tests;
