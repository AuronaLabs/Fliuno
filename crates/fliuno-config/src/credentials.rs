use super::{ModelSettings, read_api_key, validate_api_key};
use std::path::Path;

const NO_SOURCE: &str = "Set [model].api_key_file or configure a managed credential";

impl ModelSettings {
    pub fn read_api_key(&self) -> Result<String, &'static str> {
        match (&self.api_key_file, &self.api_key_credential) {
            (Some(file), None) => read_api_key(file),
            (None, Some(id)) => read_managed_api_key(id),
            (None, None) => Err(NO_SOURCE),
            (Some(_), Some(_)) => Err("Choose one API key source in [model]"),
        }
    }
}

/// A new credential is provisional until the referencing configuration is saved.
/// Dropping this value rolls back the new credential if a write fails or loses a race.
pub struct PendingApiKey {
    setting: String,
    provisional: Option<Location>,
}

impl PendingApiKey {
    pub fn stage(config: &Path, key: &str) -> Result<Self, &'static str> {
        validate_api_key(key)?;
        let id = uuid::Uuid::new_v4().to_string();
        Self::stage_local(config, &id, key)
    }

    pub fn setting(&self) -> &str {
        &self.setting
    }

    pub fn commit(mut self) {
        self.provisional.take();
    }
}

impl Drop for PendingApiKey {
    fn drop(&mut self) {
        if let Some(location) = self.provisional.take() {
            location.remove();
        }
    }
}

#[cfg(unix)]
struct Location(std::path::PathBuf);

#[cfg(unix)]
impl Location {
    fn remove(self) {
        let _ = std::fs::remove_file(self.0);
    }
}

#[cfg(unix)]
impl PendingApiKey {
    fn stage_local(config: &Path, id: &str, key: &str) -> Result<Self, &'static str> {
        use std::{
            fs,
            fs::OpenOptions,
            io::Write,
            os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt},
        };
        const ERROR: &str = "Could not store the API key in a private file";
        let reference = format!("credentials/{id}.key");
        let parent = config.parent().unwrap_or_else(|| Path::new(""));
        let directory = parent.join("credentials");
        fs::DirBuilder::new()
            .mode(0o700)
            .recursive(true)
            .create(&directory)
            .map_err(|_| ERROR)?;
        let info = fs::symlink_metadata(&directory).map_err(|_| ERROR)?;
        if !info.is_dir() || info.permissions().mode() & 0o022 != 0 {
            return Err("The credentials directory must be private and cannot be a symbolic link");
        }
        let path = parent.join(&reference);
        let mut file = OpenOptions::new()
            .mode(0o600)
            .write(true)
            .create_new(true)
            .open(&path)
            .map_err(|_| ERROR)?;
        let pending = Self {
            setting: format!("api_key_file = \"{reference}\""),
            provisional: Some(Location(path)),
        };
        file.write_all(key.as_bytes())
            .and_then(|()| file.sync_all())
            .map_err(|_| ERROR)?;
        Ok(pending)
    }
}

#[cfg(windows)]
struct Location(keyring_core::Entry);

#[cfg(windows)]
impl Location {
    fn remove(self) {
        let _ = self.0.delete_credential();
    }
}

#[cfg(windows)]
impl PendingApiKey {
    fn stage_local(_config: &Path, id: &str, key: &str) -> Result<Self, &'static str> {
        if key.len() > 2560 {
            return Err("This key is too long for Windows Credential Manager. Use a key file.");
        }
        let entry = windows_entry(id)?;
        entry
            .set_secret(key.as_bytes())
            .map_err(|_| "Could not store the API key in Windows Credential Manager")?;
        Ok(Self {
            setting: format!("api_key_credential = \"{id}\""),
            provisional: Some(Location(entry)),
        })
    }
}

#[cfg(windows)]
fn windows_entry(id: &str) -> Result<keyring_core::Entry, &'static str> {
    use keyring_core::api::CredentialStoreApi;
    use std::collections::HashMap;
    if uuid::Uuid::parse_str(id).is_err() {
        return Err("Invalid Windows Credential Manager reference in [model]");
    }
    let options = HashMap::from([("persistence", "Local")]);
    windows_native_keyring_store::Store::new()
        .and_then(|store| store.build("fliuno-model", id, Some(&options)))
        .map_err(|_| "Windows Credential Manager is unavailable")
}

#[cfg(windows)]
fn read_managed_api_key(id: &str) -> Result<String, &'static str> {
    let secret = windows_entry(id)?
        .get_secret()
        .map_err(|_| "Cannot read the API key from Windows Credential Manager")?;
    let key = String::from_utf8(secret).map_err(|_| "Invalid API key encoding")?;
    validate_api_key(&key)?;
    Ok(key)
}

#[cfg(not(windows))]
fn read_managed_api_key(_id: &str) -> Result<String, &'static str> {
    Err(
        "This API key reference requires Windows Credential Manager. Use a key file on this platform.",
    )
}

#[cfg(not(any(unix, windows)))]
struct Location;

#[cfg(not(any(unix, windows)))]
impl Location {
    fn remove(self) {}
}

#[cfg(not(any(unix, windows)))]
impl PendingApiKey {
    fn stage_local(_config: &Path, _id: &str, _key: &str) -> Result<Self, &'static str> {
        Err("Managed credentials are unavailable on this platform. Use a key file.")
    }
}

#[cfg(all(test, any(unix, windows)))]
mod tests {
    use super::*;
    use crate::Settings;

    #[test]
    fn pending_credential_is_read_by_settings_and_removed_on_rollback() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        let pending = PendingApiKey::stage(&path, "test-only-key").unwrap();
        let settings = format!("[model]\nprovider = \"openai\"\n{}\n", pending.setting());
        assert!(!settings.contains("test-only-key"));
        std::fs::write(&path, settings).unwrap();
        let loaded = Settings::load(&path).unwrap();
        assert_eq!(loaded.model.read_api_key().unwrap(), "test-only-key");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let key_file = loaded.model.api_key_file.as_ref().unwrap();
            assert_eq!(
                std::fs::metadata(key_file).unwrap().permissions().mode() & 0o077,
                0
            );
        }
        #[cfg(windows)]
        {
            let entry = windows_entry(loaded.model.api_key_credential.as_deref().unwrap()).unwrap();
            assert_eq!(entry.get_attributes().unwrap()["persistence"], "Local");
        }
        drop(pending);
        assert!(loaded.model.read_api_key().is_err());
    }
}
