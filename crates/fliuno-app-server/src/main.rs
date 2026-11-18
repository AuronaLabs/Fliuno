//! stdio is reserved for protocol data; diagnostics go to stderr.
mod configuration;
use fliuno_app_server::RpcServer;
use fliuno_core::Kernel;
use serde_json::json;
use std::{
    io::{self, BufRead, BufWriter, Write},
    path::PathBuf,
    thread,
};

const MAX_FRAME_BYTES: usize = 1024 * 1024;

fn main() {
    if std::env::args().nth(1).as_deref() == Some("--terminal-worker") {
        let result = fliuno_terminal::worker::run();
        if let Err(error) = &result {
            eprintln!("fliuno terminal worker: {error}");
        }
        // Dedicated supervisor ownership (including Windows Job handles) lasts
        // until OS exit. Never unwind into public app-server startup.
        std::process::exit(if result.is_ok() { 0 } else { 1 });
    }
    if let Err(error) = run() {
        eprintln!("fliuno: {error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let mut db = PathBuf::from(".fliuno/kernel.db");
    let mut mcp_config: Option<PathBuf> = None;
    let mut acp_config: Option<PathBuf> = None;
    let mut a2a_config: Option<PathBuf> = None;
    let mut exec_config: Option<PathBuf> = None;
    let mut terminal_config: Option<PathBuf> = None;
    let mut skills_dir: Option<PathBuf> = None;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--schema" => {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&fliuno_protocol::schema_bundle())?
                );
                return Ok(());
            }
            "--skills-dir" => {
                skills_dir = Some(args.next().ok_or("--skills-dir requires a path")?.into())
            }
            "--mcp-config" => {
                mcp_config = Some(args.next().ok_or("--mcp-config requires a path")?.into())
            }
            "--acp-config" => {
                acp_config = Some(args.next().ok_or("--acp-config requires a path")?.into())
            }
            "--a2a-config" => {
                a2a_config = Some(args.next().ok_or("--a2a-config requires a path")?.into())
            }
            "--exec-config" => {
                exec_config = Some(args.next().ok_or("--exec-config requires a path")?.into())
            }
            "--terminal-config" => {
                terminal_config = Some(
                    args.next()
                        .ok_or("--terminal-config requires a path")?
                        .into(),
                )
            }
            "--db" => db = args.next().ok_or("--db requires a path")?.into(),
            "--help" | "-h" => {
                println!(
                    "fliuno-app-server [--db PATH] [--mcp-config PATH] [--acp-config PATH] [--a2a-config PATH] [--exec-config PATH] [--terminal-config PATH] [--skills-dir PATH] [--schema]\nLocal stdio JSON-RPC. --a2a-config registers host-selected remote Agents. --exec-config registers host-selected commands. --terminal-config registers persistent trusted-host PTYs. Each invocation or terminal input requires approval. FLIUNO_PROVIDER=demo|openai|anthropic; live providers require FLIUNO_MODEL and an API key."
                );
                return Ok(());
            }
            _ => return Err(format!("unknown argument: {arg}").into()),
        }
    }
    // Parse every startup file before opening SQLite or allowing an earlier
    // adapter to connect/spawn. Semantic validation remains adapter-owned.
    let mcp_config = mcp_config
        .as_deref()
        .map(|path| configuration::load::<fliuno_mcp::McpConfig>(path, "MCP"))
        .transpose()?;
    let acp_config = acp_config
        .as_deref()
        .map(|path| configuration::load::<fliuno_agents::AgentsConfig>(path, "ACP"))
        .transpose()?;
    let a2a_config = a2a_config
        .as_deref()
        .map(|path| configuration::load::<fliuno_agents::A2aConfig>(path, "A2A"))
        .transpose()?;
    let exec_config = exec_config
        .as_deref()
        .map(|path| configuration::load::<fliuno_exec::Config>(path, "execution"))
        .transpose()?;
    let terminal_config = terminal_config
        .as_deref()
        .map(|path| configuration::load::<fliuno_terminal::Config>(path, "terminal"))
        .transpose()?;
    let (provider_name, provider) = fliuno_providers::from_env()?;
    let mut kernel = Kernel::open(db, provider)?;
    fliuno_tools::register(&mut kernel)?;
    if let Some(path) = skills_dir {
        fliuno_skills::register(&mut kernel, fliuno_skills::SkillCatalog::open(path)?)?;
    }
    if let Some(config) = mcp_config {
        fliuno_mcp::register_config(&mut kernel, config)?;
    }
    if let Some(config) = acp_config {
        fliuno_agents::register_config(&mut kernel, config)?;
    }
    if let Some(config) = a2a_config {
        fliuno_agents::register_a2a_config(&mut kernel, config)?;
    }
    if let Some(config) = exec_config {
        fliuno_exec::register_config(&mut kernel, config)?;
    }
    let terminals = if let Some(config) = terminal_config {
        Some(fliuno_terminal::register_config(
            &mut kernel,
            config,
            std::env::current_exe()?,
        )?)
    } else {
        None
    };
    let mut server = RpcServer::with_terminals(kernel, provider_name, terminals);
    let terminal_rx = server.take_terminal_notification_receiver();
    let (event_rx, event_subscription) = server.take_event_receiver().into_parts();
    let mut event_subscription = Some(event_subscription);
    let (progress_rx, progress_subscription) = server.take_progress_receiver().into_parts();
    let mut progress_subscription = Some(progress_subscription);
    let (input_tx, input_rx) = crossbeam_channel::bounded::<InputFrame>(64);
    thread::Builder::new()
        .name("fliuno-stdio-reader".into())
        .spawn(move || read_input_frames(input_tx))?;
    // Coalesce serde's small writes while keeping the flush after each frame.
    let mut stdout = BufWriter::with_capacity(64 * 1024, io::stdout().lock());
    let mut last_event_sequence = 0;
    let mut event_rx = event_rx;
    let mut progress_rx = progress_rx;
    loop {
        crossbeam_channel::select! {
            recv(terminal_rx) -> _ => {
                if let Some(notification) = server.next_terminal_notification() {
                    write_frame(&mut stdout, &notification)?;
                }
            },
            recv(input_rx) -> frame => match frame {
                Ok(InputFrame::Line(line)) => {
                    if let Some(response) = server.handle_line(&line) {
                        write_frame(&mut stdout, &response)?;
                    }
                    if let Some((through_sequence, receiver)) = server.take_pending_event_subscription() {
                        last_event_sequence = through_sequence;
                        let (new_rx, subscription) = receiver.into_parts();
                        event_rx = new_rx;
                        event_subscription = Some(subscription);
                    }
                    if let Some(receiver) = server.take_pending_progress_receiver() {
                        let (new_rx, subscription) = receiver.into_parts();
                        progress_rx = new_rx;
                        progress_subscription = Some(subscription);
                    }
                }
                Ok(InputFrame::InvalidUtf8) => {
                    if let Some(response) = server.handle_line("invalid UTF-8") {
                        write_frame(&mut stdout, &response)?;
                    }
                }
                Ok(InputFrame::TooLarge) => return Err("RPC frame exceeds 1 MiB".into()),
                Err(_) => break,
            },
            recv(event_rx) -> event => match event {
                Ok(event) => {
                    last_event_sequence = event.sequence;
                    write_frame(&mut stdout, &json!({
                        "jsonrpc":"2.0", "method":"notifications/event",
                        "params":{"event":event}
                    }))?;
                }
                Err(_) => {
                    write_frame(&mut stdout, &json!({
                        "jsonrpc":"2.0", "method":"notifications/eventGap",
                        "params":{"afterSequence":last_event_sequence}
                    }))?;
                    drop(event_subscription.take());
                    event_rx = crossbeam_channel::never();
                    server.mark_event_stream_gap();
                }
            },
            recv(progress_rx) -> progress => match progress {
                Ok(progress) => {
                    write_frame(&mut stdout, &json!({
                        "jsonrpc":"2.0", "method":"notifications/modelProgress",
                        "params":{"progress":progress}
                    }))?;
                }
                Err(_) => {
                    write_frame(&mut stdout, &json!({
                        "jsonrpc":"2.0", "method":"notifications/modelProgressGap",
                        "params":{}
                    }))?;
                    drop(progress_subscription.take());
                    progress_rx = crossbeam_channel::never();
                    server.mark_progress_stream_gap();
                }
            }
        }
    }
    Ok(())
}

enum InputFrame {
    Line(String),
    InvalidUtf8,
    TooLarge,
}

fn read_input_frames(sender: crossbeam_channel::Sender<InputFrame>) {
    let stdin = io::stdin();
    let mut stdin = stdin.lock();
    loop {
        let mut frame = Vec::new();
        loop {
            let available = match stdin.fill_buf() {
                Ok(available) => available,
                Err(_) => return,
            };
            if available.is_empty() {
                break;
            }
            let count = available
                .iter()
                .position(|byte| *byte == b'\n')
                .map_or(available.len(), |position| position + 1);
            if frame.len() + count > MAX_FRAME_BYTES {
                let _ = sender.send(InputFrame::TooLarge);
                return;
            }
            frame.extend_from_slice(&available[..count]);
            let ended = available[count - 1] == b'\n';
            stdin.consume(count);
            if ended {
                break;
            }
        }
        if frame.is_empty() {
            return;
        }
        let frame = match String::from_utf8(frame) {
            Ok(line) => InputFrame::Line(line),
            Err(_) => InputFrame::InvalidUtf8,
        };
        if sender.send(frame).is_err() {
            return;
        }
    }
}

fn write_frame(stdout: &mut impl Write, frame: &serde_json::Value) -> io::Result<()> {
    serde_json::to_writer(&mut *stdout, frame)?;
    stdout.write_all(b"\n")?;
    stdout.flush()
}
