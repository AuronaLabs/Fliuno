//! Keep a bounded tail while continuing to drain pipes, so output limits cannot
//! deadlock a child. Binary or split UTF-8 is preserved with explicit base64.
use base64::{Engine, engine::general_purpose::STANDARD};
use serde::Serialize;
use std::collections::VecDeque;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CapturedOutput {
    pub encoding: &'static str,
    pub data: String,
    pub bytes_seen: u64,
    pub retained_bytes: usize,
    pub truncated: bool,
}

pub(crate) struct Capture {
    tail: VecDeque<u8>,
    seen: u64,
    limit: usize,
}
impl Capture {
    pub fn new(limit: usize) -> Self {
        Self {
            tail: VecDeque::with_capacity(limit),
            seen: 0,
            limit,
        }
    }
    pub fn push(&mut self, bytes: &[u8]) {
        self.seen = self.seen.saturating_add(bytes.len() as u64);
        if bytes.len() >= self.limit {
            self.tail.clear();
            self.tail.extend(&bytes[bytes.len() - self.limit..]);
        } else {
            let discard = (self.tail.len() + bytes.len()).saturating_sub(self.limit);
            self.tail.drain(..discard);
            self.tail.extend(bytes);
        }
    }
    pub fn finish(self) -> CapturedOutput {
        let bytes: Vec<u8> = self.tail.into();
        let (encoding, data) = match std::str::from_utf8(&bytes) {
            Ok(text) => ("utf8", text.to_owned()),
            Err(_) => ("base64", STANDARD.encode(&bytes)),
        };
        CapturedOutput {
            encoding,
            data,
            bytes_seen: self.seen,
            retained_bytes: bytes.len(),
            truncated: self.seen > bytes.len() as u64,
        }
    }
}
