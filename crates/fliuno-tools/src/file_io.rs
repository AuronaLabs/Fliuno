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
    if cancellation.is_cancelled() {
        return Err("tool cancelled before file read".into());
    }
    let metadata = file.metadata().map_err(|_| "cannot read file metadata")?;
    if !metadata.is_file() {
        return Err("path is not a regular file".into());
    }
    if metadata.len() > READ_LIMIT {
        return Err("file exceeds 256 KiB".into());
    }
    read_bounded(file, cancellation)
}

fn read_bounded(reader: impl Read, cancellation: &CancellationToken) -> Result<Vec<u8>, ToolError> {
    let mut reader = reader.take(READ_LIMIT + 1);
    let mut bytes = Vec::new();
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
        if bytes.len() + count > READ_LIMIT as usize {
            return Err("file exceeds 256 KiB".into());
        }
        bytes.extend_from_slice(&chunk[..count]);
    }
}

#[cfg(test)]
mod tests;
