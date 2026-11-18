//! Canonical registration identities. Display names and request IDs are not
//! authority: a persisted approval must match the same host-selected contract.
use serde_json::Value;
use sha2::{Digest, Sha256};

/// A domain-separated SHA-256 over typed, length-framed JSON. Object key order
/// is irrelevant; array order, values and JSON types remain significant. Only
/// the resulting digest belongs in snapshots, never the input configuration.
pub fn authority_digest(domain: &str, value: &Value) -> String {
    fn bytes(hash: &mut Sha256, value: &[u8]) {
        hash.update((value.len() as u64).to_le_bytes());
        hash.update(value);
    }
    fn visit(hash: &mut Sha256, value: &Value) {
        match value {
            Value::Null => hash.update(b"n"),
            Value::Bool(value) => hash.update(if *value { b"t" } else { b"f" }),
            Value::Number(value) => {
                hash.update(b"d");
                bytes(hash, value.to_string().as_bytes());
            }
            Value::String(value) => {
                hash.update(b"s");
                bytes(hash, value.as_bytes());
            }
            Value::Array(values) => {
                hash.update(b"a");
                hash.update((values.len() as u64).to_le_bytes());
                for value in values {
                    visit(hash, value);
                }
            }
            Value::Object(values) => {
                hash.update(b"o");
                hash.update((values.len() as u64).to_le_bytes());
                let mut keys: Vec<_> = values.keys().collect();
                keys.sort_unstable();
                for key in keys {
                    bytes(hash, key.as_bytes());
                    visit(hash, &values[key]);
                }
            }
        }
    }
    let mut hash = Sha256::new();
    bytes(&mut hash, b"fliuno.registration.v1");
    bytes(&mut hash, domain.as_bytes());
    visit(&mut hash, value);
    format!("{:x}", hash.finalize())
}

pub(crate) fn valid(identity: &str) -> bool {
    identity.len() == 64
        && identity
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
}

#[cfg(test)]
mod tests;
