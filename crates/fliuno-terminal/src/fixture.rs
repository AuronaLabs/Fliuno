//! Deterministic native PTY peer used only by explicit offline tests.
/// Reserve a fresh spool and serialize its original bootstrap without starting
/// a worker. Process tests can then archive it before delivering that bootstrap
/// and prove that the pre-PTY tombstone barrier rejects a delayed launch.
pub fn reserve_delayed_worker(
    service: &crate::Service,
    session: &str,
    terminal: &str,
    cwd: &std::path::Path,
    command: &std::path::Path,
    args: Vec<String>,
) -> Result<Vec<u8>, String> {
    use crate::protocol::*;
    crate::registry::valid_id(terminal)?;
    let preset = crate::config::Preset::resolve(
        crate::PresetConfig {
            id: "probe".into(),
            command: command.into(),
            isolation: crate::Isolation::TrustedHost,
            args: args.clone(),
            env_allow: vec![],
            lifetime_ms: 30000,
            spool_bytes: 65536,
            resources: Default::default(),
            linux_cgroup_parent: None,
        },
        &service.root,
    )?;
    let _lock = crate::registry::lock(&service.root)?;
    let directory = service.root.join(terminal);
    fliuno_sandbox::create_private_directory(&directory)
        .map_err(|_| "cannot reserve fixture directory")?;
    let snapshot = Snapshot {
        terminal_id: terminal.into(),
        session_id: session.into(),
        preset_id: "probe".into(),
        status: Status::Starting,
        startup_error: None,
        supervisor_connected: false,
        rows: 24,
        cols: 80,
        exit_code: None,
        signal: None,
        start_offset: 0,
        end_offset: 0,
        input_bytes: 0,
        output_closed: false,
        expires_at_unix_ms: now_ms() + 30000,
        archived_at_unix_ms: None,
    };
    drop(crate::spool::Spool::create(&directory, 65536, &snapshot)?);
    let launch = Launch {
        descriptor: Descriptor {
            version: 1,
            endpoint: crate::ipc::endpoint(&directory)?,
            token: "f".repeat(64),
            supervisor_pid: 0,
        },
        directory,
        command: command.into(),
        args,
        environment: preset.environment,
        executable_digest: preset.executable_digest,
        resources: Default::default(),
        linux_cgroup_parent: None,
        cwd: cwd.into(),
        spool_bytes: 65536,
        lifetime_ms: 30000,
        snapshot,
    };
    serde_json::to_vec(&launch).map_err(|_| "cannot encode fixture bootstrap".into())
}

pub fn run() -> std::io::Result<()> {
    use std::io::{BufRead, IsTerminal, Write};
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    writeln!(
        out,
        "TERMINAL_READY {}",
        serde_json::json!({"stdinTty":std::io::stdin().is_terminal(),"stdoutTty":stdout.is_terminal(),"cwd":std::env::current_dir()?,"secret":std::env::var_os("FLIUNO_TERMINAL_SECRET").is_some()})
    )?;
    out.flush()?;
    for line in std::io::stdin().lock().lines() {
        let line = line?;
        match line.as_str() {
            "exit" => break,
            "flood" => {
                out.write_all(&vec![b'x'; 256 * 1024])?;
                writeln!(out, "FLOOD_END")?;
            }
            "spawn" | "spawn-exit" | "spawn-detached" => {
                let mut child = std::process::Command::new(std::env::current_exe()?)
                    .arg("terminal-descendant")
                    .spawn()?;
                writeln!(out, "DESCENDANT {}", child.id())?;
                out.flush()?;
                if line == "spawn-exit" {
                    break;
                }
                if line == "spawn" {
                    let _ = child.wait();
                }
            }
            _ => writeln!(out, "TERMINAL_REPLY {line}")?,
        }
        out.flush()?;
    }
    writeln!(out, "TERMINAL_EXIT")?;
    out.flush()
}
pub fn descendant() -> ! {
    #[cfg(target_os = "linux")]
    let _ = nix::unistd::setsid();
    loop {
        std::thread::sleep(std::time::Duration::from_secs(1));
    }
}
