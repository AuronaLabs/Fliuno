//! Link-free directory traversal for project metadata and context.
use cap_fs_ext::DirExt;
use cap_std::{ambient_authority, fs::Dir};
use std::{
    io,
    path::{Component, Path},
};

// The stored workspace root is canonical. Traverse its components without
// following links so replacing that root or an ancestor cannot redirect access.
pub(crate) fn open_workspace_root(root: &Path) -> io::Result<Dir> {
    let mut anchor = std::path::PathBuf::new();
    let mut parts = root.components().peekable();
    if matches!(parts.peek(), Some(Component::Prefix(_))) {
        anchor.push(parts.next().expect("prefix checked"));
    }
    if !matches!(parts.peek(), Some(Component::RootDir)) {
        return Err(io::ErrorKind::InvalidInput.into());
    }
    anchor.push(parts.next().expect("root checked"));
    let mut dir = Dir::open_ambient_dir(anchor, ambient_authority())?;
    for part in parts {
        let Component::Normal(name) = part else {
            return Err(io::ErrorKind::InvalidInput.into());
        };
        dir = dir.open_dir_nofollow(name)?;
        if is_reparse_point(&dir.dir_metadata()?) {
            return Err(io::ErrorKind::PermissionDenied.into());
        }
    }
    Ok(dir)
}

#[cfg(windows)]
pub(crate) fn is_reparse_point(metadata: &cap_std::fs::Metadata) -> bool {
    use cap_std::fs::MetadataExt;
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
    metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

#[cfg(not(windows))]
pub(crate) fn is_reparse_point(_: &cap_std::fs::Metadata) -> bool {
    false
}
