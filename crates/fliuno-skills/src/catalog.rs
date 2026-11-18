//! Capability-scoped, bounded reads of configured skill directories.
use crate::{SkillMetadata, metadata::parse};
use cap_fs_ext::{DirExt, FollowSymlinks, OpenOptionsFollowExt};
use cap_std::{ambient_authority, fs::Dir};
use std::{collections::BTreeMap, io::Read, path::Path};
const MAX_SKILL_BYTES: u64 = 128 * 1024;

pub struct SkillCatalog {
    pub(crate) entries: BTreeMap<String, Entry>,
    pub(crate) authority: String,
}

pub(crate) struct Entry {
    pub metadata: SkillMetadata,
    /// Keep the discovered directory capability. Later reads cannot silently
    /// switch to a sibling or a replaced path in the catalog's ambient root.
    pub directory: Dir,
}

impl SkillCatalog {
    pub(crate) fn contains(&self, name: &str) -> bool {
        self.entries.contains_key(name)
    }
    pub fn open(path: impl AsRef<Path>) -> Result<Self, String> {
        let path = path.as_ref();
        if !path.is_absolute() {
            return Err("skills directory must be absolute".into());
        }
        let canonical =
            std::fs::canonicalize(path).map_err(|_| "cannot resolve skills directory")?;
        let root = Dir::open_ambient_dir(&canonical, ambient_authority())
            .map_err(|_| "cannot open skills directory")?;
        let mut catalog = Self {
            entries: BTreeMap::new(),
            authority: String::new(),
        };
        for (index, entry) in root
            .entries()
            .map_err(|_| "cannot list skills")?
            .enumerate()
        {
            if index >= 1024 {
                return Err("skills root exceeds 1024 directory entries".into());
            }
            let entry = entry.map_err(|_| "invalid skills directory entry")?;
            let entry_type = entry
                .file_type()
                .map_err(|_| "cannot inspect skill entry")?;
            if entry_type.is_symlink() || !entry_type.is_dir() {
                continue;
            }
            let folder = entry
                .file_name()
                .into_string()
                .map_err(|_| "skill folder must be UTF-8")?;
            let directory = root
                .open_dir_nofollow(&folder)
                .map_err(|_| "cannot open skill directory")?;
            let file = match open_file(&directory, "SKILL.md") {
                Ok(file) => file,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(_) => return Err(format!("cannot open skill {folder}")),
            };
            let content = read_bounded(file)?;
            let metadata = parse(&content, &folder)?;
            if catalog.entries.len() >= 256 {
                return Err("skills catalog exceeds 256 entries".into());
            }
            catalog.entries.insert(
                metadata.name.clone(),
                Entry {
                    metadata,
                    directory,
                },
            );
        }
        catalog.authority = fliuno_core::authority_digest(
            "skills.catalog.v1",
            &serde_json::json!({
                "root":canonical.to_str().ok_or("skills directory must be UTF-8")?,
                "metadata":catalog.list(),
            }),
        );
        Ok(catalog)
    }

    pub fn list(&self) -> Vec<SkillMetadata> {
        self.entries
            .values()
            .map(|entry| entry.metadata.clone())
            .collect()
    }

    pub fn read(&self, name: &str) -> Result<String, String> {
        let entry = self.entries.get(name).ok_or("unknown skill")?;
        let file = open_file(&entry.directory, "SKILL.md").map_err(|_| "cannot read skill")?;
        let content = read_bounded(file)?;
        parse(&content, name)?; // Detect an invalid edit since discovery.
        Ok(content)
    }
}

/// Resolve a single leaf without following links, then verify the opened file
/// before reading. O_NONBLOCK prevents a replaced FIFO from blocking a worker.
pub(crate) fn open_file(directory: &Dir, leaf: &str) -> std::io::Result<cap_std::fs::File> {
    let mut options = cap_std::fs::OpenOptions::new();
    options.read(true).follow(FollowSymlinks::No);
    #[cfg(unix)]
    {
        use cap_std::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NONBLOCK);
    }
    let file = directory.open_with(leaf, &options)?;
    if !file.metadata()?.is_file() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "skill file must be regular",
        ));
    }
    Ok(file)
}

fn read_bounded(file: cap_std::fs::File) -> Result<String, String> {
    if !file
        .metadata()
        .map_err(|_| "cannot inspect skill file")?
        .is_file()
    {
        return Err("SKILL.md must be a regular file".into());
    }
    let mut bytes = Vec::new();
    file.take(MAX_SKILL_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| "cannot read SKILL.md")?;
    if bytes.len() as u64 > MAX_SKILL_BYTES {
        return Err("SKILL.md exceeds 128 KiB".into());
    }
    String::from_utf8(bytes).map_err(|_| "SKILL.md must be UTF-8".into())
}
