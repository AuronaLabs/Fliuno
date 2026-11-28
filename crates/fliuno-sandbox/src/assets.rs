//! Explicit, content-pinned companion files. Stage into a fresh private root;
//! never mount or copy a host directory tree implicitly. Child execution starts
//! only after every file's copied bytes match the host's expected digest.
use cap_std::{
    ambient_authority,
    fs::{Dir, OpenOptions},
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File},
    io::{self, Read, Write},
    path::{Path, PathBuf},
};

pub const MAX_ASSET_BYTES: u64 = 128 * 1024 * 1024;
pub const MAX_BUNDLE_BYTES: u64 = 512 * 1024 * 1024;
pub const MAX_BUNDLE_FILES: usize = 1024;

/// Host configuration only. Paths are portable relative ASCII names using '/'.
/// The expected digest is mandatory; changing it creates a new bundle identity.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct StagedFile {
    pub source: PathBuf,
    pub path: String,
    pub sha256: String,
    /// Initial Unix mode is 0500 instead of 0400. This is not an immutable-file
    /// guarantee: the workload owns its scratch directory after launch.
    #[serde(default)]
    pub executable: bool,
}

fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}

fn key(path: &str) -> io::Result<String> {
    if path.is_empty() || path.len() > 512 {
        return Err(invalid("invalid staged file path length"));
    }
    for part in path.split('/') {
        let stem = part.split('.').next().unwrap_or("").to_ascii_uppercase();
        let device = matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
            || stem
                .strip_prefix("COM")
                .or_else(|| stem.strip_prefix("LPT"))
                .is_some_and(|suffix| {
                    suffix.len() == 1 && matches!(suffix.as_bytes()[0], b'1'..=b'9')
                });
        if part.is_empty()
            || part.len() > 128
            || matches!(part, "." | "..")
            || part.ends_with('.')
            || device
            || !part
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
        {
            return Err(invalid(
                "staged file path is not a portable relative file name",
            ));
        }
    }
    let key = path.to_ascii_lowercase();
    if ["program", "program.exe"]
        .iter()
        .any(|reserved| key == *reserved || key.starts_with(&format!("{reserved}/")))
    {
        return Err(invalid(
            "staged file path conflicts with the native executable",
        ));
    }
    Ok(key)
}

/// Validate portable names and file/directory collisions independently of IO.
pub fn validate_staged_file_paths<'a>(paths: impl IntoIterator<Item = &'a str>) -> io::Result<()> {
    let mut unique = BTreeSet::new();
    let mut directories = BTreeMap::new();
    for path in paths {
        if unique.len() == MAX_BUNDLE_FILES {
            return Err(invalid("too many staged files"));
        }
        let normalized = key(path)?;
        for (index, _) in path.match_indices('/') {
            let spelling = &path[..index];
            if let Some(previous) = directories.insert(spelling.to_ascii_lowercase(), spelling)
                && previous != spelling
            {
                return Err(invalid("staged directories have inconsistent case"));
            }
        }
        if !unique.insert(normalized) {
            return Err(invalid("duplicate staged file path"));
        }
    }
    for path in &unique {
        for (index, _) in path.match_indices('/') {
            if unique.contains(&path[..index]) {
                return Err(invalid("staged file conflicts with a parent directory"));
            }
        }
    }
    Ok(())
}

/// Validate the complete manifest before creating files or launching processes.
/// Source metadata is advisory; the copy loop separately enforces byte bounds.
pub fn validate_staged_files(files: &[StagedFile]) -> io::Result<()> {
    validate_staged_file_paths(files.iter().map(|file| file.path.as_str()))?;
    let mut bytes = 0u64;
    for file in files {
        if file.sha256.len() != 64
            || !file
                .sha256
                .bytes()
                .all(|b| b.is_ascii_digit() || matches!(b, b'a'..=b'f'))
        {
            return Err(invalid(
                "staged file SHA-256 must be 64 lowercase hexadecimal digits",
            ));
        }
        if !file.source.is_absolute() {
            return Err(invalid("staged file source must be absolute"));
        }
        let metadata = fs::symlink_metadata(&file.source)?;
        if !metadata.is_file() || metadata.len() > MAX_ASSET_BYTES {
            return Err(invalid(
                "staged source must be a regular file up to 128 MiB",
            ));
        }
        bytes = bytes
            .checked_add(metadata.len())
            .ok_or_else(|| invalid("staged size overflow"))?;
        if bytes > MAX_BUNDLE_BYTES {
            return Err(invalid("staged bundle exceeds 512 MiB"));
        }
    }
    Ok(())
}

/// A stable content/policy identity, excluding secret file bytes. Used by
/// adapters to prevent a changed bundle from reusing an older tool approval.
pub fn staged_files_digest(files: &[StagedFile]) -> io::Result<String> {
    let encoded = identity_bytes(files)?;
    Ok(format!("{:x}", Sha256::digest(encoded)))
}
fn identity_bytes(files: &[StagedFile]) -> io::Result<Vec<u8>> {
    // Canonical field framing avoids ambiguous concatenation and keeps this
    // OS crate independent of JSON wire formats.
    let mut bytes = Vec::new();
    for file in files {
        for field in [
            file.source
                .to_str()
                .ok_or_else(|| invalid("staged source must be UTF-8"))?,
            &file.path,
            &file.sha256,
        ] {
            bytes.extend_from_slice(&(field.len() as u64).to_le_bytes());
            bytes.extend_from_slice(field.as_bytes());
        }
        bytes.push(u8::from(file.executable));
    }
    Ok(bytes)
}

fn source(path: &Path) -> io::Result<File> {
    let mut options = fs::OpenOptions::new();
    options.read(true);
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        options.custom_flags(windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT);
    }
    let file = options.open(path)?;
    let metadata = file.metadata()?;
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        if metadata.file_attributes()
            & windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT
            != 0
        {
            return Err(invalid("staged source is a reparse point"));
        }
    }
    if !metadata.is_file() {
        return Err(invalid("staged source changed its file type"));
    }
    Ok(file)
}

#[cfg(test)]
mod tests;

/// Only fresh, host-owned private directories are valid destinations. Directory
/// capabilities confine path resolution even if another actor inserts a link.
pub(crate) fn stage(root: &Path, files: &[StagedFile]) -> io::Result<()> {
    validate_staged_files(files)?;
    let directory = Dir::open_ambient_dir(root, ambient_authority())?;
    let mut total = 0u64;
    for file in files {
        let mut input = source(&file.source)?;
        let path = Path::new(&file.path);
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            directory.create_dir_all(parent)?;
        }
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use cap_std::fs::OpenOptionsExt;
            options.mode(if file.executable { 0o500 } else { 0o400 });
        }
        let mut output = directory.open_with(path, &options)?;
        let mut digest = Sha256::new();
        let mut count = 0u64;
        let mut buffer = [0u8; 64 * 1024];
        loop {
            let read = input.read(&mut buffer)?;
            if read == 0 {
                break;
            }
            count += read as u64;
            total += read as u64;
            if count > MAX_ASSET_BYTES || total > MAX_BUNDLE_BYTES {
                return Err(invalid("staged content grew beyond its byte budget"));
            }
            output.write_all(&buffer[..read])?;
            digest.update(&buffer[..read]);
        }
        output.flush()?;
        if format!("{:x}", digest.finalize()) != file.sha256 {
            return Err(invalid(
                "staged file content does not match its pinned SHA-256",
            ));
        }
    }
    Ok(())
}
