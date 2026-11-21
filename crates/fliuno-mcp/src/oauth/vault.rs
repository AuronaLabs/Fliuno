//! Encrypted OAuth records outside the kernel database. SQLite only sees
//! ciphertext, identity hashes and compare-and-swap revisions.
use super::{OAuthError, StoredGrant};
use ring::{
    aead,
    rand::{SecureRandom, SystemRandom},
};
use rusqlite::{Connection, OpenFlags, OptionalExtension, TransactionBehavior, params};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use uuid::Uuid;
use zeroize::Zeroizing;

const APPLICATION_ID: i64 = 0x464C_4F56;
// Also keep older Fliuno builds from mistaking the vault for a kernel DB.
const VAULT_VERSION: i64 = 0x0046_4C01;
const MAX_RECORD: usize = 96 * 1024;
const NONCE_BYTES: usize = aead::NONCE_LEN;
const CHECK: &[u8] = b"fliuno oauth vault v1";
const STORE: OAuthError = OAuthError::CredentialStore;

#[derive(Clone)]
pub struct CredentialVault(Arc<Vault>);

struct Vault {
    path: PathBuf,
    key: aead::LessSafeKey,
}

#[derive(Clone)]
pub(super) struct Slot {
    vault: CredentialVault,
    id: String,
}

pub(super) struct Snapshot {
    pub revision: Option<String>,
    pub grant: Option<StoredGrant>,
}

impl CredentialVault {
    /// Open an explicitly configured file using a random, host-protected key.
    /// The caller must keep this file separate from the business database.
    pub fn open(path: &Path, key: &[u8; 32]) -> Result<Self, OAuthError> {
        let path = std::path::absolute(path).map_err(|_| STORE)?;
        let key = aead::UnboundKey::new(&aead::AES_256_GCM, key).map_err(|_| STORE)?;
        let vault = Vault {
            path,
            key: aead::LessSafeKey::new(key),
        };
        let mut db = vault.connect(true)?;
        let tx = sql(db.transaction_with_behavior(TransactionBehavior::Immediate))?;
        let app_id: i64 = sql(tx.pragma_query_value(None, "application_id", |row| row.get(0)))?;
        let version: i64 = sql(tx.pragma_query_value(None, "user_version", |row| row.get(0)))?;
        if app_id == 0 && version == 0 {
            let objects: i64 =
                sql(tx.query_row("SELECT count(*) FROM sqlite_schema", [], |row| row.get(0)))?;
            if objects != 0 {
                return Err(STORE);
            }
            sql(tx.pragma_update(None, "application_id", APPLICATION_ID))?;
            sql(tx.pragma_update(None, "user_version", VAULT_VERSION))?;
            sql(tx.execute_batch(
                "CREATE TABLE key_check (id INTEGER PRIMARY KEY CHECK(id = 1), payload BLOB NOT NULL) STRICT;
                 CREATE TABLE grants (slot TEXT PRIMARY KEY, revision TEXT NOT NULL, payload BLOB) STRICT;"
            ))?;
            let check = vault.seal(b"vault-check", CHECK)?;
            sql(tx.execute("INSERT INTO key_check VALUES (1, ?1)", [check]))?;
        } else if app_id != APPLICATION_ID || version != VAULT_VERSION {
            return Err(STORE);
        }
        vault.check(&tx)?;
        sql(tx.commit())?;
        Ok(Self(Arc::new(vault)))
    }

    pub(super) fn slot(&self, id: String) -> Slot {
        Slot {
            vault: self.clone(),
            id,
        }
    }
}

impl Vault {
    fn connect(&self, create: bool) -> Result<Connection, OAuthError> {
        let mut flags = OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX;
        if create {
            flags |= OpenFlags::SQLITE_OPEN_CREATE;
        }
        let db = sql(Connection::open_with_flags(&self.path, flags))?;
        sql(db.busy_timeout(Duration::from_secs(2)))?;
        sql(db.pragma_update(None, "trusted_schema", "OFF"))?;
        sql(db.pragma_update(None, "synchronous", "EXTRA"))?;
        let mode: String = sql(db.pragma_query_value(None, "journal_mode", |row| row.get(0)))?;
        if mode != "delete" {
            return Err(STORE);
        }
        Ok(db)
    }

    fn check(&self, db: &Connection) -> Result<(), OAuthError> {
        let check: Vec<u8> = sql(db.query_row(
            "SELECT payload FROM key_check WHERE id = 1 AND length(payload) < 128",
            [],
            |row| row.get(0),
        ))?;
        if self.unseal(b"vault-check", check)?.as_slice() != CHECK {
            return Err(STORE);
        }
        Ok(())
    }

    fn seal(&self, context: &[u8], data: &[u8]) -> Result<Vec<u8>, OAuthError> {
        if data.len() > MAX_RECORD - NONCE_BYTES - aead::MAX_TAG_LEN {
            return Err(STORE);
        }
        let mut nonce = [0; NONCE_BYTES];
        SystemRandom::new().fill(&mut nonce).map_err(|_| STORE)?;
        let mut payload = Zeroizing::new(data.to_vec());
        self.key
            .seal_in_place_append_tag(
                aead::Nonce::assume_unique_for_key(nonce),
                aead::Aad::from(context),
                &mut *payload,
            )
            .map_err(|_| STORE)?;
        let mut record = Vec::with_capacity(NONCE_BYTES + payload.len());
        record.extend_from_slice(&nonce);
        record.extend_from_slice(&payload);
        Ok(record)
    }

    fn unseal(&self, context: &[u8], data: Vec<u8>) -> Result<Zeroizing<Vec<u8>>, OAuthError> {
        let mut data = Zeroizing::new(data);
        let nonce = data
            .get(..NONCE_BYTES)
            .and_then(|b| b.try_into().ok())
            .ok_or(STORE)?;
        let plaintext = self
            .key
            .open_in_place(
                aead::Nonce::assume_unique_for_key(nonce),
                aead::Aad::from(context),
                &mut data[NONCE_BYTES..],
            )
            .map_err(|_| STORE)?;
        Ok(Zeroizing::new(plaintext.to_vec()))
    }
}

impl Slot {
    pub(super) async fn read(&self) -> Result<Snapshot, OAuthError> {
        let slot = self.clone();
        tokio::task::spawn_blocking(move || slot.read_sync())
            .await
            .map_err(|_| STORE)?
    }

    fn read_sync(&self) -> Result<Snapshot, OAuthError> {
        let vault = &self.vault.0;
        let db = vault.connect(false)?;
        vault.check(&db)?;
        let row: Option<(String, Option<Vec<u8>>)> = sql(db
            .query_row(
                "SELECT revision, CASE WHEN length(payload) > ?2 THEN zeroblob(1) ELSE payload END
             FROM grants WHERE slot = ?1 AND length(revision) = 36",
                params![self.id, MAX_RECORD as i64],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional())?;
        let Some((revision, payload)) = row else {
            return Ok(Snapshot {
                revision: None,
                grant: None,
            });
        };
        let grant = payload
            .map(|cipher| {
                let plaintext = vault.unseal(self.aad(&revision).as_bytes(), cipher)?;
                serde_json::from_slice(&plaintext).map_err(|_| STORE)
            })
            .transpose()?;
        Ok(Snapshot {
            revision: Some(revision),
            grant,
        })
    }

    /// A null payload is a durable tombstone: no caller can repeat the exchange.
    pub(super) async fn replace(
        &self,
        expected: Option<String>,
        grant: Option<StoredGrant>,
    ) -> Result<String, OAuthError> {
        let slot = self.clone();
        tokio::task::spawn_blocking(move || slot.replace_sync(expected, grant))
            .await
            .map_err(|_| STORE)?
    }

    fn replace_sync(
        &self,
        expected: Option<String>,
        grant: Option<StoredGrant>,
    ) -> Result<String, OAuthError> {
        let vault = &self.vault.0;
        let revision = Uuid::new_v4().to_string();
        let payload = grant
            .map(|grant| {
                let bytes = Zeroizing::new(serde_json::to_vec(&grant).map_err(|_| STORE)?);
                vault.seal(self.aad(&revision).as_bytes(), &bytes)
            })
            .transpose()?;
        let mut db = vault.connect(false)?;
        let tx = sql(db.transaction_with_behavior(TransactionBehavior::Immediate))?;
        vault.check(&tx)?;
        let changed = match expected {
            Some(old) => sql(tx.execute(
                "UPDATE grants SET revision = ?1, payload = ?2 WHERE slot = ?3 AND revision = ?4",
                params![revision, payload, self.id, old],
            ))?,
            None => sql(tx.execute(
                "INSERT INTO grants VALUES (?1, ?2, ?3) ON CONFLICT(slot) DO NOTHING",
                params![self.id, revision, payload],
            ))?,
        };
        if changed != 1 {
            return Err(STORE);
        }
        sql(tx.commit())?;
        Ok(revision)
    }

    fn aad(&self, revision: &str) -> String {
        format!("fliuno-oauth-grant-v1:{}:{revision}", self.id)
    }
}

fn sql<T>(result: rusqlite::Result<T>) -> Result<T, OAuthError> {
    result.map_err(|_| STORE)
}
