//! Serialize local writers and preserve unrelated TOML content.
use super::{Settings, read_file};
use sha2::{Digest, Sha256};
use std::{
    ffi::OsString,
    fs::{self, File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
};
use toml_edit::{DocumentMut, Item, TableLike};

pub(super) const CONFIG_LIMIT: u64 = 64 * 1024;

pub(super) fn revision(text: &str) -> String {
    format!("{:x}", Sha256::digest(text.as_bytes()))
}

pub(super) struct ConfigEdit<'a> {
    requested_path: &'a Path,
    path: PathBuf,
    original: String,
    _lock: File,
}

impl<'a> ConfigEdit<'a> {
    pub fn open(requested_path: &'a Path) -> Result<Self, String> {
        let path =
            fs::canonicalize(requested_path).map_err(|_| "Cannot open model configuration")?;
        let parent = path
            .parent()
            .ok_or("Configuration has no parent directory")?;
        let mut lock_name = OsString::from(path.file_name().ok_or("Invalid configuration name")?);
        lock_name.push(".lock");
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(parent.join(lock_name))
            .map_err(|_| "Cannot open model configuration lock")?;
        lock.try_lock()
            .map_err(|_| "Model configuration is being edited. Try again.")?;
        let original =
            read_file(&path, CONFIG_LIMIT).map_err(|_| "Cannot read model configuration")?;
        Ok(Self {
            requested_path,
            path,
            original,
            _lock: lock,
        })
    }

    pub fn require_revision(&self, expected: &str) -> Result<(), String> {
        if expected == revision(&self.original) {
            Ok(())
        } else {
            Err("Model configuration changed. Reload it and try again.".into())
        }
    }

    pub fn settings(&self) -> Result<Settings, String> {
        // Credential paths are relative to the selected config path, including symlinks.
        Settings::parse(self.requested_path, &self.original)
    }

    pub fn document(&self) -> Result<DocumentMut, String> {
        self.original
            .parse()
            .map_err(|_| "Cannot edit model configuration".into())
    }

    pub fn parse(&self, updated: &str) -> Result<Settings, String> {
        if updated.len() as u64 > CONFIG_LIMIT {
            return Err("Updated model configuration exceeds its size limit".into());
        }
        Settings::parse(self.requested_path, updated)
    }

    pub fn commit(&self, updated: &str) -> Result<(), String> {
        let parent = self
            .path
            .parent()
            .ok_or("Configuration has no parent directory")?;
        let mut temporary = tempfile::NamedTempFile::new_in(parent)
            .map_err(|_| "Cannot stage model configuration changes")?;
        temporary
            .write_all(updated.as_bytes())
            .and_then(|()| temporary.as_file().sync_all())
            .map_err(|_| "Cannot stage model configuration changes")?;
        if fs::canonicalize(self.requested_path).ok().as_ref() != Some(&self.path)
            || read_file(&self.path, CONFIG_LIMIT).ok().as_deref() != Some(self.original.as_str())
        {
            return Err("Model configuration changed. Reload it and try again.".into());
        }
        temporary
            .persist(&self.path)
            .map_err(|_| "Cannot save model configuration")?;
        Ok(())
    }
}

pub(super) fn replace_value(table: &mut dyn TableLike, name: &str, mut item: Item) {
    if let Some(old) = table.get(name).and_then(Item::as_value)
        && let Some(new) = item.as_value_mut()
    {
        *new.decor_mut() = old.decor().clone();
    }
    table.insert(name, item);
}
