//! Count the compact wire representation without allocating a JSON buffer.
use std::io::{self, Write};

pub(crate) fn json_size(value: &impl serde::Serialize) -> serde_json::Result<usize> {
    #[derive(Default)]
    struct ByteCount(usize);

    impl Write for ByteCount {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0 += bytes.len();
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    let mut count = ByteCount::default();
    serde_json::to_writer(&mut count, value)?;
    Ok(count.0)
}
