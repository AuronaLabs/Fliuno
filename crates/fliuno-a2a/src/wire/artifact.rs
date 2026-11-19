use super::{Error, Result, parts, valid_id};
use serde::Serialize;
use serde_json::Value;
use std::io::{self, Write};

const MAX_BYTES: usize = 256 * 1024;
pub(super) const MAX_PARTS: usize = 256;
const APPEND_FIELDS: [&str; 3] = ["name", "description", "metadata"];

/// An A2A artifact with validated Parts and an exact compact JSON byte count.
#[derive(Debug, Clone)]
pub struct ArtifactPayload {
    raw: Value,
    bytes: usize,
}

impl ArtifactPayload {
    pub(super) fn parse(raw: Value) -> Result<Self> {
        if !raw["artifactId"].as_str().is_some_and(valid_id) {
            return Err(Error::Protocol);
        }
        let bytes = compact_bytes(&raw)?;
        if bytes > MAX_BYTES {
            return Err(Error::Budget);
        }
        parts(&raw)?;
        if raw
            .get("name")
            .is_some_and(|n| !n.is_null() && n.as_str().is_none_or(|s| s.len() > 256))
        {
            return Err(Error::Protocol);
        }
        Ok(Self { raw, bytes })
    }

    pub fn id(&self) -> &str {
        self.raw["artifactId"]
            .as_str()
            .expect("validated artifact ID")
    }

    pub fn as_json(&self) -> &Value {
        &self.raw
    }

    /// Append new Parts in order without revisiting the existing Part bodies.
    /// Stream completion and task identity are handled by the caller.
    pub fn append(&mut self, mut chunk: Self) -> Result<()> {
        if self.id() != chunk.id() {
            return Err(Error::Protocol);
        }
        let old = self.raw["parts"].as_array().expect("validated Parts");
        let added = chunk.raw["parts"].as_array().expect("validated Parts");
        if old.len() + added.len() > MAX_PARTS {
            return Err(Error::Protocol);
        }
        // Both arrays are nonempty: appending replaces two brackets with one comma.
        let mut bytes = self.bytes + compact_bytes(added)? - 1;
        for key in APPEND_FIELDS {
            if let Some(new) = chunk.raw.get(key) {
                bytes += compact_bytes(new)?;
                match self.raw.get(key) {
                    Some(previous) => bytes -= compact_bytes(previous)?,
                    None => bytes += key.len() + 4, // quotes, colon and comma for an ASCII key
                }
            }
        }
        if bytes > MAX_BYTES {
            return Err(Error::Budget);
        }
        let mut parts = chunk.raw["parts"].take();
        self.raw["parts"]
            .as_array_mut()
            .expect("validated Parts")
            .append(parts.as_array_mut().expect("validated Parts"));
        let fields = chunk.raw.as_object_mut().expect("validated object");
        for key in APPEND_FIELDS {
            if let Some(value) = fields.remove(key) {
                self.raw[key] = value;
            }
        }
        self.bytes = bytes;
        Ok(())
    }
}

fn compact_bytes(value: &impl Serialize) -> Result<usize> {
    struct Count(usize);
    impl Write for Count {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0 += bytes.len();
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    let mut count = Count(0);
    serde_json::to_writer(&mut count, value).map_err(|_| Error::Protocol)?;
    Ok(count.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn append_counts_escaped_parts_and_replaced_metadata_at_the_byte_limit() {
        let first = json!({"artifactId":"report","parts":[{"raw":"aGk="}],
            "description":"previous", "metadata":{"old":true}, "vendor\"key":17});
        let mut assembled = ArtifactPayload::parse(first).unwrap();
        let mut chunk = json!({"artifactId":"report","parts":[{"data":{"escaped":"\"\n\u{0000}"}},{"text":""}],
            "description":null,"metadata":{"new":[7]},"name":"\u{4ef6}","vendor\"key":99});
        let mut expected = json!({"artifactId":"report","parts":[{"raw":"aGk="},{"data":{"escaped":"\"\n\u{0000}"}},{"text":""}],
            "description":null,"metadata":{"new":[7]},"name":"\u{4ef6}","vendor\"key":17});
        let padding = MAX_BYTES - serde_json::to_vec(&expected).unwrap().len();
        chunk["parts"][1]["text"] = json!("x".repeat(padding + 1));
        let old_json = assembled.as_json().clone();
        assert!(matches!(
            assembled.append(ArtifactPayload::parse(chunk.clone()).unwrap()),
            Err(Error::Budget)
        ));
        assert_eq!(assembled.as_json(), &old_json);
        chunk["parts"][1]["text"] = json!("x".repeat(padding));
        expected["parts"][2]["text"] = chunk["parts"][1]["text"].clone();
        assembled
            .append(ArtifactPayload::parse(chunk).unwrap())
            .unwrap();
        assert_eq!(serde_json::to_vec(&expected).unwrap().len(), MAX_BYTES);
        assert_eq!(assembled.as_json(), &expected);
    }

    #[test]
    fn append_keeps_the_total_part_limit_and_validates_each_chunk() {
        let mut assembled = ArtifactPayload::parse(json!({"artifactId":"report",
            "parts":vec![json!({"raw":"AAE="}); MAX_PARTS-1]}))
        .unwrap();
        assert!(
            ArtifactPayload::parse(json!({"artifactId":"report","parts":[{"raw":"!"}]})).is_err()
        );
        let last = ArtifactPayload::parse(json!({"artifactId":"report","parts":[{"raw":"AgM="}]}))
            .unwrap();
        let foreign =
            ArtifactPayload::parse(json!({"artifactId":"unrelated","parts":[{"text":"other"}]}))
                .unwrap();
        assert!(assembled.append(foreign).is_err());
        assembled.append(last.clone()).unwrap();
        assert!(assembled.append(last).is_err());
        assert_eq!(
            assembled.as_json()["parts"].as_array().unwrap().len(),
            MAX_PARTS
        );
    }
}
