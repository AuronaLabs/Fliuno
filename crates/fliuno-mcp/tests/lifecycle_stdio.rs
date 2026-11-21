#![cfg(all(any(windows, target_os = "linux"), feature = "test-fixtures"))]

use serde_json::json;
use std::{fs, path::PathBuf};

#[test]
fn default_auto_supports_legacy_stdio_peers_that_keep_or_close_the_process() {
    for (mode, expected_starts) in [
        ("--legacy-stays-on-discover", "1"),
        ("--legacy-exit-on-discover", "2"),
        ("--legacy-silent-exit-on-discover", "2"),
    ] {
        let root = std::env::temp_dir().join(format!(
            "fliuno-mcp-lifecycle-{}-{:?}-{expected_starts}",
            std::process::id(),
            std::thread::current().id()
        ));
        fs::create_dir(&root).unwrap();
        let marker = root.join("starts.txt");
        let config = serde_json::from_value(json!({
            "id":"legacy",
            "command":PathBuf::from(env!("CARGO_BIN_EXE_fliuno-mcp-fixture")),
            "args":[mode, marker],
            "isolation":"trusted_host",
            "cwd":root
        }))
        .unwrap();

        let tools = fliuno_mcp::connect(config).unwrap();

        assert!(tools.is_empty());
        assert_eq!(fs::read_to_string(&marker).unwrap(), expected_starts);
        drop(tools);
        fs::remove_dir_all(root).unwrap();
    }
}
