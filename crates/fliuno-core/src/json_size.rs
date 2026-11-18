//! Compact JSON byte counts without allocating a second serialized buffer.
use crate::Result;

#[derive(Default)]
struct JsonSize(usize);

impl std::io::Write for JsonSize {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0 += bytes.len();
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

pub(crate) fn json_size(value: &impl serde::Serialize) -> Result<usize> {
    let mut size = JsonSize::default();
    serde_json::to_writer(&mut size, value)?;
    Ok(size.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn counts_compact_utf8_json_bytes_including_escapes_and_nested_values() {
        let values = [
            json!(null),
            json!(["ASCII", "你好😀", "line\nbreak", "quote\\\"", 1.25, true]),
            json!({"nested":{"items":[1,2,3]},"escaped":"\u{0}\t"}),
            json!("界".repeat(256 * 1024)),
        ];

        for value in values {
            assert_eq!(
                json_size(&value).unwrap(),
                serde_json::to_vec(&value).unwrap().len()
            );
        }
    }
}
