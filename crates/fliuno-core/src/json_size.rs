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
