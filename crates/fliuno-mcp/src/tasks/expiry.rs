use std::time::{SystemTime, UNIX_EPOCH};

/// A peer can extend its TTL. Check only fresh snapshots and leave old
/// handles in place so a reused task ID cannot acquire their authority.
pub(super) fn elapsed(created_at: &str, ttl_ms: Option<u64>) -> bool {
    let Some(ttl_ms) = ttl_ms else { return false };
    let Ok(created) = chrono::DateTime::parse_from_rfc3339(created_at) else {
        return false;
    };
    let Ok(now) = SystemTime::now().duration_since(UNIX_EPOCH) else {
        return false;
    };
    i128::from(created.timestamp_millis()) + i128::from(ttl_ms) <= now.as_millis() as i128
}
