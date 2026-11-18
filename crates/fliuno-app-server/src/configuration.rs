//! Startup JSON has its own byte budget. Do not allocate the complete file
//! before enforcing it, or echo configured strings through serde diagnostics.
use serde::de::DeserializeOwned;
use std::{fs::File, io::Read, path::Path};

pub const MAX_CONFIG_BYTES: usize = 1024 * 1024;

pub fn load<T: DeserializeOwned>(path: &Path, kind: &str) -> Result<T, String> {
    // Reject an already non-regular path before opening (in particular a FIFO).
    // Recheck the opened handle; host paths are not a hostile-filesystem API.
    let metadata =
        std::fs::metadata(path).map_err(|_| format!("cannot inspect {kind} configuration"))?;
    if !metadata.is_file() {
        return Err(format!("{kind} configuration must be a regular file"));
    }
    let file = File::open(path).map_err(|_| format!("cannot open {kind} configuration"))?;
    let metadata = file
        .metadata()
        .map_err(|_| format!("cannot inspect opened {kind} configuration"))?;
    if !metadata.is_file() {
        return Err(format!("{kind} configuration must be a regular file"));
    }
    parse(file, kind)
}

fn parse<T: DeserializeOwned>(reader: impl Read, kind: &str) -> Result<T, String> {
    let mut bytes = Vec::new();
    reader
        .take(MAX_CONFIG_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| format!("cannot read {kind} configuration"))?;
    if bytes.len() > MAX_CONFIG_BYTES {
        return Err(format!("{kind} configuration exceeds 1 MiB"));
    }
    serde_json::from_slice(&bytes).map_err(|error| {
        // serde's type/unknown-field diagnostics can include raw values. Keep
        // only position metadata, never config text, paths or credentials.
        format!(
            "invalid {kind} JSON configuration (line {}, column {})",
            error.line(),
            error.column()
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;
    use std::{cell::Cell, io, rc::Rc};

    struct Growing {
        read: Rc<Cell<usize>>,
    }
    impl Read for Growing {
        fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
            output.fill(b' ');
            self.read.set(self.read.get() + output.len());
            Ok(output.len())
        }
    }

    #[test]
    fn growing_input_is_bounded_and_exact_limit_remains_valid_json() {
        let count = Rc::new(Cell::new(0));
        let error = parse::<Value>(
            Growing {
                read: Rc::clone(&count),
            },
            "fixture",
        )
        .unwrap_err();
        assert_eq!(count.get(), MAX_CONFIG_BYTES + 1);
        assert!(error.contains("exceeds 1 MiB"));
        let mut input = vec![b' '; MAX_CONFIG_BYTES];
        input[..2].copy_from_slice(b"{}");
        assert_eq!(
            parse::<Value>(&input[..], "fixture").unwrap(),
            serde_json::json!({})
        );
    }

    #[test]
    fn type_errors_and_reader_errors_cannot_echo_configured_secrets() {
        let error =
            parse::<Vec<u64>>(&b"[\"private-configured-token\"]"[..], "fixture").unwrap_err();
        assert!(error.contains("line 1, column"));
        assert!(!error.contains("private-configured-token"));
        struct Broken;
        impl Read for Broken {
            fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
                Err(io::Error::other("private-file-path"))
            }
        }
        assert_eq!(
            parse::<Value>(Broken, "fixture").unwrap_err(),
            "cannot read fixture configuration"
        );
    }
}
