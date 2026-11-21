//! Executable content identity and staging use the same bounded byte reader.
//! A pin is checked against copied bytes, closing the pre-hash/copy race for
//! isolated workloads. Trusted-host execution still uses an external OS path.
use sha2::{Digest, Sha256};
use std::{
    fs::{File, OpenOptions},
    io::{self, Read, Write},
    path::Path,
};
pub const MAX_PROGRAM_BYTES: u64 = 128 * 1024 * 1024;
pub const MAX_TRUSTED_PROGRAM_BYTES: u64 = 256 * 1024 * 1024;
#[cfg(test)]
mod tests;

pub(crate) fn valid_digest(digest: &str) -> bool {
    digest.len() == 64
        && digest
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
}

fn copy(program: &Path, mut output: impl Write, max_bytes: u64) -> io::Result<String> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::fs::OpenOptionsExt;
        // Configured executable symlinks are allowed; the actual read bytes
        // still must match the pin. Do not block if a regular file becomes FIFO.
        options.custom_flags(libc::O_NONBLOCK);
    }
    let source = options.open(program)?;
    let metadata = source.metadata()?;
    if !metadata.is_file() || metadata.len() > max_bytes {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "native executable is not a regular file or exceeds its byte limit",
        ));
    }
    let mut source = source.take(max_bytes + 1);
    let mut hash = Sha256::new();
    let mut count = 0u64;
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let read = source.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        count += read as u64;
        if count > max_bytes {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "native executable grew beyond its byte limit",
            ));
        }
        output.write_all(&buffer[..read])?;
        hash.update(&buffer[..read]);
    }
    output.flush()?;
    Ok(format!("{:x}", hash.finalize()))
}

/// Snapshot a host-selected executable. Paths and bytes never appear in its
/// digest. Recheck the copied content at isolated startup using program_sha256.
pub fn program_digest(program: &Path) -> io::Result<String> {
    copy(program, io::sink(), MAX_PROGRAM_BYTES)
}

/// Snapshot a trusted host executable without allocating space to stage it.
pub fn trusted_program_digest(program: &Path) -> io::Result<String> {
    copy(program, io::sink(), MAX_TRUSTED_PROGRAM_BYTES)
}

pub(crate) fn stage(program: &Path, destination: &Path, expected: Option<&str>) -> io::Result<()> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o500);
    }
    // New Windows files inherit private directory ACLs, unlike fs::copy.
    let target: File = options.open(destination)?;
    let actual = copy(program, target, MAX_PROGRAM_BYTES)?;
    if expected.is_some_and(|expected| expected != actual) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "native executable does not match its pinned SHA-256",
        ));
    }
    Ok(())
}
