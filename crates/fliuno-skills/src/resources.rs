//! Progressive resource reads stay inside one discovered skill capability.
//! A script is text here; reading it never selects an executable or runs code.
use crate::{SkillCatalog, catalog::open_file};
use base64::{Engine, engine::general_purpose::STANDARD};
use cap_fs_ext::DirExt;
use cap_std::fs::Dir;
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::io::Read;
mod tool;
pub(crate) use tool::tools;

const MAX_RESOURCE_BYTES: u64 = 256 * 1024;
const MAX_RESOURCE_ENTRIES: usize = 256;

#[derive(Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ResourceKind {
    File,
    Directory,
}
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ResourceEntry {
    pub name: String,
    pub kind: ResourceKind,
    pub bytes: Option<u64>,
}
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SkillResource {
    pub path: String,
    pub content: String,
    pub bytes: usize,
    pub sha256: String,
}

/// Opaque bytes, never a decoded image/document or an executable artifact.
/// Length/hash describe original bytes rather than their base64 representation.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SkillAsset {
    pub path: String,
    pub encoding: &'static str,
    pub data: String,
    pub bytes: usize,
    pub sha256: String,
}

pub(crate) fn validate_path(path: &str, root_allowed: bool) -> Result<(), String> {
    if path.is_empty() && root_allowed {
        return Ok(());
    }
    let parts: Vec<_> = path.split('/').collect();
    if path.is_empty() || path.len() > 512 || parts.len() > 16 {
        return Err("skill resource path exceeds its relative-path budget".into());
    }
    for part in parts {
        let stem = part
            .split('.')
            .next()
            .unwrap_or_default()
            .to_ascii_uppercase();
        let device = matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
            || ["COM", "LPT"].iter().any(|prefix| {
                stem.strip_prefix(prefix).is_some_and(|suffix| {
                    matches!(
                        suffix,
                        "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³"
                    )
                })
            });
        if part.is_empty()
            || part.len() > 128
            || matches!(part, "." | "..")
            || part.ends_with('.')
            || device
            || !part.chars().all(|character| {
                character.is_alphanumeric() || matches!(character, '.' | '_' | '-')
            })
        {
            return Err("skill resource path requires safe relative components".into());
        }
    }
    Ok(())
}

impl SkillCatalog {
    fn resource_directory(&self, name: &str, path: &str) -> Result<Dir, String> {
        let entry = self.entries.get(name).ok_or("unknown skill")?;
        let mut directory = entry
            .directory
            .try_clone()
            .map_err(|_| "cannot clone skill directory")?;
        if !path.is_empty() {
            for component in path.split('/') {
                directory = directory
                    .open_dir_nofollow(component)
                    .map_err(|_| "cannot open skill resource directory without following links")?;
            }
        }
        Ok(directory)
    }

    /// List one directory, never recursively walking or opening file contents.
    /// Links/special files and names outside the supported path subset are not
    /// exposed. Scanning itself is bounded, even when many entries are skipped.
    pub fn list_resources(&self, name: &str, path: &str) -> Result<Vec<ResourceEntry>, String> {
        validate_path(path, true)?;
        let directory = self.resource_directory(name, path)?;
        let mut entries = Vec::new();
        for (index, entry) in directory
            .entries()
            .map_err(|_| "cannot list skill resources")?
            .enumerate()
        {
            if index >= MAX_RESOURCE_ENTRIES {
                return Err("skill resource directory exceeds 256 entries".into());
            }
            let entry = entry.map_err(|_| "cannot inspect skill resource entry")?;
            let name = entry
                .file_name()
                .into_string()
                .map_err(|_| "skill resource names must be UTF-8")?;
            if validate_path(&name, false).is_err() {
                continue;
            }
            let metadata = directory
                .symlink_metadata(&name)
                .map_err(|_| "cannot inspect skill resource")?;
            if metadata.file_type().is_symlink() {
                continue;
            }
            let (kind, bytes) = if metadata.is_file() {
                (ResourceKind::File, Some(metadata.len()))
            } else if metadata.is_dir() {
                (ResourceKind::Directory, None)
            } else {
                continue;
            };
            entries.push(ResourceEntry { name, kind, bytes });
        }
        entries.sort_unstable_by(|left, right| left.name.cmp(&right.name));
        Ok(entries)
    }

    /// Bounded UTF-8 content and its digest are returned together. The digest
    /// describes observed bytes, not permission to execute or trust the file.
    pub fn read_resource(&self, name: &str, path: &str) -> Result<SkillResource, String> {
        let bytes = self.resource_bytes(name, path)?;
        let sha256 = format!("{:x}", Sha256::digest(&bytes));
        let content = String::from_utf8(bytes).map_err(|_| "skill resource is not UTF-8 text")?;
        Ok(SkillResource {
            path: path.into(),
            bytes: content.len(),
            content,
            sha256,
        })
    }

    /// The caller explicitly selects binary transfer. The same directory,
    /// regular-file and raw-byte budget applies before any encoding occurs.
    pub fn read_asset(&self, name: &str, path: &str) -> Result<SkillAsset, String> {
        let bytes = self.resource_bytes(name, path)?;
        Ok(SkillAsset {
            path: path.into(),
            encoding: "base64",
            data: STANDARD.encode(&bytes),
            bytes: bytes.len(),
            sha256: format!("{:x}", Sha256::digest(&bytes)),
        })
    }

    fn resource_bytes(&self, name: &str, path: &str) -> Result<Vec<u8>, String> {
        validate_path(path, false)?;
        let (parent, leaf) = path.rsplit_once('/').unwrap_or(("", path));
        let directory = self.resource_directory(name, parent)?;
        let file = open_file(&directory, leaf)
            .map_err(|_| "cannot open regular skill resource without following links")?;
        if file
            .metadata()
            .map_err(|_| "cannot inspect skill resource")?
            .len()
            > MAX_RESOURCE_BYTES
        {
            return Err("skill resource exceeds 256 KiB".into());
        }
        let mut bytes = Vec::new();
        file.take(MAX_RESOURCE_BYTES + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| "cannot read skill resource")?;
        if bytes.len() as u64 > MAX_RESOURCE_BYTES {
            return Err("skill resource exceeds 256 KiB".into());
        }
        Ok(bytes)
    }
}

#[cfg(test)]
mod tests;
