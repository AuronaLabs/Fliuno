//! Real CLI startup boundaries: all JSON is bounded and parsed before a DB,
//! configured subprocess or MCP connection can be created.
use serde_json::json;
use std::{
    fs,
    path::PathBuf,
    process::{Command, Output, Stdio},
    thread,
    time::{Duration, Instant},
};

#[path = "../../fliuno-mcp/tests/support/http.rs"]
mod wire;

struct Directory(PathBuf);
impl Directory {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("fliuno-startup-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&path).unwrap();
        Self(path)
    }
    fn run(&self, options: &[(&str, &std::path::Path)]) -> Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_fliuno-app-server"));
        command
            .arg("--db")
            .arg(self.0.join("kernel.db"))
            .env("FLIUNO_PROVIDER", "demo")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for (flag, path) in options {
            command.arg(flag).arg(path);
        }
        let mut child = command.spawn().unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if child.try_wait().unwrap().is_some() {
                return child.wait_with_output().unwrap();
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                let output = child.wait_with_output().unwrap();
                panic!(
                    "startup did not terminate: {}",
                    String::from_utf8_lossy(&output.stderr)
                );
            }
            thread::sleep(Duration::from_millis(10));
        }
    }
}
impl Drop for Directory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn every_configuration_flag_rejects_oversize_and_redacts_parse_errors_before_database_creation() {
    let fixture = Directory::new();
    let path = fixture.0.join("config.json");
    for (flag, label) in [
        ("--mcp-config", "MCP"),
        ("--acp-config", "ACP"),
        ("--a2a-config", "A2A"),
        ("--exec-config", "execution"),
        ("--terminal-config", "terminal"),
    ] {
        let mut oversized = vec![b' '; 1024 * 1024 + 1];
        oversized[..2].copy_from_slice(b"{}");
        fs::write(&path, oversized).unwrap();
        let output = fixture.run(&[(flag, &path)]);
        assert!(!output.status.success());
        assert!(output.stdout.is_empty());
        assert!(
            String::from_utf8_lossy(&output.stderr)
                .contains(&format!("{label} configuration exceeds 1 MiB"))
        );
        assert!(!fixture.0.join("kernel.db").exists());
        fs::write(
            &path,
            br#"{"private-field-do-not-echo":"private-value-do-not-echo"}"#,
        )
        .unwrap();
        let output = fixture.run(&[(flag, &path)]);
        let error = String::from_utf8_lossy(&output.stderr);
        assert!(!output.status.success());
        assert!(output.stdout.is_empty());
        assert!(
            error.contains(&format!("invalid {label} JSON configuration (line")),
            "{error}"
        );
        assert!(!error.contains("private-field-do-not-echo"));
        assert!(!error.contains("private-value-do-not-echo"));
        assert!(!fixture.0.join("kernel.db").exists());
    }
}

#[test]
fn invalid_later_agent_json_prevents_earlier_mcp_network_initialization() {
    let fixture = Directory::new();
    let peer = wire::Fixture::start(|request| wire::reply(request, wire::Version::Modern));
    let mcp = fixture.0.join("mcp.json");
    let acp = fixture.0.join("acp.json");
    fs::write(
        &mcp,
        json!({"servers":[{"id":"remote","transport":"streamable_http","url":peer.url("/mcp")}]})
            .to_string(),
    )
    .unwrap();
    fs::write(&acp, b"not-valid-json").unwrap();
    let output = fixture.run(&[("--mcp-config", &mcp), ("--acp-config", &acp)]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("invalid ACP JSON configuration"));
    assert!(peer.requests().is_empty());
    assert!(!fixture.0.join("kernel.db").exists());
}

#[test]
fn directories_and_missing_configuration_paths_fail_without_echoing_the_path() {
    let fixture = Directory::new();
    let missing = fixture.0.join("private-config-path.json");
    for path in [&missing, &fixture.0] {
        let output = fixture.run(&[("--mcp-config", path)]);
        assert!(!output.status.success());
        let error = String::from_utf8_lossy(&output.stderr);
        assert!(error.contains("MCP configuration"));
        assert!(!error.contains("private-config-path"));
        assert!(!fixture.0.join("kernel.db").exists());
    }
}
