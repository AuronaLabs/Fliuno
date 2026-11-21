//! Move bounded state between fresh sandboxes without sharing a live directory.
use cap_fs_ext::{DirExt, FollowSymlinks, MetadataExt, OpenOptionsFollowExt};
use cap_std::{
    ambient_authority,
    fs::{Dir, OpenOptions},
};
use std::{
    ffi::OsStr,
    fs,
    io::{self, Read},
    path::{Path, PathBuf},
};

pub(crate) const DIRECTORY: &str = "fliuno-state";
const MAX_BYTES: u64 = 32 * 1024 * 1024;
const MAX_ENTRIES: usize = 1024;
const MAX_DEPTH: usize = 12;

/// Host-selected checkpoint paths. Only a snapshot of the workload's state
/// directory is restored; the process never sees or opens either host path.
#[derive(Clone)]
pub struct PrivateState {
    pub previous: Option<PathBuf>,
    pub next: PathBuf,
}

pub(crate) struct PreparedState {
    plan: PrivateState,
    scratch: Dir,
    pub(crate) home: PathBuf,
}

impl PrivateState {
    pub(crate) fn restore(self, scratch: &Path) -> io::Result<PreparedState> {
        let root = Dir::open_ambient_dir(scratch, ambient_authority())?;
        // create_dir fails on collisions with staged workload companions.
        root.create_dir(DIRECTORY)?;
        if let Some(source) = &self.previous {
            let source = open_host_directory(source)?;
            let target = root.open_dir_nofollow(DIRECTORY)?;
            copy_tree(&source, &target, &mut Budget::default(), 0, false)?;
        }
        Ok(PreparedState {
            plan: self,
            scratch: root,
            home: scratch.join(DIRECTORY),
        })
    }
}

impl PreparedState {
    pub(crate) fn capture(&self) -> io::Result<()> {
        let source = self.scratch.open_dir_nofollow(DIRECTORY)?;
        reject_reparse(&source.dir_metadata()?)?;
        let parent = self.plan.next.parent().ok_or_else(invalid_path)?;
        let parent_dir = open_host_directory(parent)?;
        let name = self.plan.next.file_name().ok_or_else(invalid_path)?;
        match parent_dir.symlink_metadata(name) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
            Ok(_) => return Err(io::ErrorKind::AlreadyExists.into()),
        }
        let temporary = format!(".snapshot-{}", uuid::Uuid::new_v4().simple());
        // Host-only destination ACL/mode, even when the parent is shareable.
        crate::create_private_directory(&parent.join(&temporary))?;
        let result = (|| {
            let target = parent_dir.open_dir_nofollow(&temporary)?;
            copy_tree(&source, &target, &mut Budget::default(), 0, true)?;
            drop(target);
            parent_dir.rename(&temporary, &parent_dir, name)?;
            sync_directory(parent_dir.try_clone()?)
        })();
        if result.is_err() {
            let _ = parent_dir.remove_dir_all(&temporary);
        }
        result
    }
}

fn open_host_directory(path: &Path) -> io::Result<Dir> {
    if !path.is_absolute() {
        return Err(invalid_path());
    }
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(invalid_path());
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        if metadata.file_attributes()
            & windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT
            != 0
        {
            return Err(invalid_path());
        }
    }
    Dir::open_ambient_dir(path, ambient_authority())
}

#[derive(Default)]
struct Budget {
    entries: usize,
    bytes: u64,
}

fn invalid_path() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidInput,
        "invalid private state directory",
    )
}

fn invalid_entry() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        "unsupported private state entry",
    )
}

fn too_large() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        "private state exceeds its checkpoint budget",
    )
}

fn copy_tree(
    source: &Dir,
    target: &Dir,
    budget: &mut Budget,
    depth: usize,
    durable: bool,
) -> io::Result<()> {
    if depth > MAX_DEPTH {
        return Err(too_large());
    }
    for entry in source.entries()? {
        let entry = entry?;
        budget.entries += 1;
        if budget.entries > MAX_ENTRIES {
            return Err(too_large());
        }
        let name = entry.file_name();
        validate_basename(&name)?;
        let metadata = source.symlink_metadata(&name)?;
        reject_reparse(&metadata)?;
        if metadata.is_dir() {
            let child = source.open_dir_nofollow(&name)?;
            reject_reparse(&child.dir_metadata()?)?;
            target.create_dir(&name)?;
            copy_tree(
                &child,
                &target.open_dir_nofollow(&name)?,
                budget,
                depth + 1,
                durable,
            )?;
        } else if metadata.is_file() {
            copy_file(source, target, &name, budget, durable)?;
        } else {
            return Err(invalid_entry());
        }
    }
    if durable {
        sync_directory(target.try_clone()?)?;
    }
    Ok(())
}

fn validate_basename(name: &OsStr) -> io::Result<()> {
    // Directory entries are already single components. Reject Windows stream
    // syntax explicitly before passing a child-selected name to open().
    if name.len() > 255 || name.as_encoded_bytes().contains(&b':') {
        return Err(invalid_entry());
    }
    Ok(())
}

fn copy_file(
    source: &Dir,
    target: &Dir,
    name: &OsStr,
    budget: &mut Budget,
    durable: bool,
) -> io::Result<()> {
    let mut read = OpenOptions::new();
    read.read(true).follow(FollowSymlinks::No);
    #[cfg(target_os = "linux")]
    {
        use cap_std::fs::OpenOptionsExt;
        read.custom_flags(libc::O_NONBLOCK);
    }
    let input = source.open_with(name, &read)?;
    let metadata = input.metadata()?;
    reject_reparse(&metadata)?;
    if !metadata.is_file() || metadata.nlink() != 1 {
        return Err(invalid_entry());
    }
    let remaining = MAX_BYTES - budget.bytes;
    if metadata.len() > remaining {
        return Err(too_large());
    }
    let mut write = OpenOptions::new();
    write
        .write(true)
        .create_new(true)
        .follow(FollowSymlinks::No);
    #[cfg(unix)]
    {
        use cap_std::fs::OpenOptionsExt;
        write.mode(0o600);
    }
    let mut output = target.open_with(name, &write)?;
    let written = io::copy(&mut input.take(remaining + 1), &mut output)?;
    if written > remaining {
        return Err(too_large());
    }
    if durable {
        output.sync_all()?;
    }
    budget.bytes += written;
    Ok(())
}

fn reject_reparse(metadata: &cap_std::fs::Metadata) -> io::Result<()> {
    #[cfg(windows)]
    {
        use cap_std::fs::MetadataExt;
        if metadata.file_attributes()
            & windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT
            != 0
        {
            return Err(invalid_entry());
        }
    }
    #[cfg(not(windows))]
    let _ = metadata;
    Ok(())
}

fn sync_directory(directory: Dir) -> io::Result<()> {
    #[cfg(unix)]
    {
        directory.open(".")?.sync_all()
    }
    #[cfg(not(unix))]
    {
        let _ = directory;
        Ok(())
    }
}

#[cfg(all(test, any(windows, target_os = "linux")))]
mod tests {
    use super::*;

    #[test]
    fn capture_copies_ordinary_files_without_publishing_rejected_state() {
        let root = std::env::temp_dir().join(format!("fliuno-state-test-{}", uuid::Uuid::new_v4()));
        crate::create_private_directory(&root).unwrap();
        struct Cleanup(PathBuf);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                let _ = fs::remove_dir_all(&self.0);
            }
        }
        let _cleanup = Cleanup(root.clone());
        let scratch = root.join("scratch");
        fs::create_dir(&scratch).unwrap();
        let state = PrivateState {
            previous: None,
            next: root.join("stored"),
        };
        let state = state.restore(&scratch).unwrap();
        let data = &state.home;
        fs::create_dir(data.join("subdir")).unwrap();
        fs::write(data.join("subdir/log"), [0, 0xff, 123]).unwrap();
        fs::write(scratch.join("program"), b"not state").unwrap();
        let outside = root.join("outside");
        fs::write(&outside, b"do not capture").unwrap();
        fs::hard_link(&outside, data.join("link")).unwrap();
        assert_eq!(
            state.capture().unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
        assert!(!state.plan.next.exists());
        fs::remove_file(data.join("link")).unwrap();
        let large = fs::File::create(data.join("large")).unwrap();
        large.set_len(MAX_BYTES + 1).unwrap();
        drop(large);
        assert_eq!(
            state.capture().unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
        assert!(!state.plan.next.exists());
        fs::remove_file(data.join("large")).unwrap();
        state.capture().unwrap();
        assert_eq!(
            state.capture().unwrap_err().kind(),
            io::ErrorKind::AlreadyExists
        );
        assert!(!state.plan.next.join("program").exists());
        let other = root.join("new-scratch");
        fs::create_dir(&other).unwrap();
        let next = PrivateState {
            previous: Some(state.plan.next.clone()),
            next: root.join("next"),
        };
        let restored = next.restore(&other).unwrap();
        assert_eq!(
            fs::read(restored.home.join("subdir/log")).unwrap(),
            [0, 0xff, 123]
        );
    }
}
