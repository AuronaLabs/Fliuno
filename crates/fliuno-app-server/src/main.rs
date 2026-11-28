//! stdio is reserved for protocol data; diagnostics go to stderr.
mod backup;
mod configuration;
mod diagnostics;
mod mcp_oauth;
use fliuno_app_server::{MemoryEmbeddingConfig, MemoryEmbeddingProvider, RpcServer};
use fliuno_core::Kernel;
use fliuno_protocol::{Event, EventWatermark, MAX_APP_REQUEST_BYTES, ModelProgress};
use serde::Serialize;
use serde_json::json;
use std::{
    ffi::OsStr,
    io::{self, BufRead, BufWriter, Write},
    path::PathBuf,
    thread,
};

fn main() {
    let mut args = std::env::args_os().skip(1);
    let mode = args.next();
    let mode = mode.as_deref().and_then(OsStr::to_str);
    if mode == Some("backup") {
        if let Err(error) = backup::run(args) {
            eprintln!("fliuno backup: {error}");
            std::process::exit(1);
        }
        return;
    }
    if mode == Some(fliuno_exec::supervisor::WORKER_ARG) {
        if fliuno_exec::supervisor::run(|destinations| {
            diagnostics::start(
                destinations.path.as_deref(),
                destinations.endpoint.as_deref(),
            )
            .ok()
            .flatten()
        })
        .is_err()
        {
            eprintln!("fliuno: process supervisor could not complete its handoff");
            std::process::exit(1);
        }
        return;
    }
    if mode == Some("--terminal-worker") {
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
    let mut db: Option<PathBuf> = None;
    let mut user_config: Option<PathBuf> = None;
    let mut demo = false;
    let mut model_profile = None;
    let mut list_models = false;
    let mut check_model = false;
    let mut model_cursor = None;
    let mut mcp_config: Option<PathBuf> = None;
    let mut mcp_oauth_config: Option<PathBuf> = None;
    let mut mcp_oauth_vault: Option<PathBuf> = None;
    #[cfg(feature = "test-fixtures")]
    let mut mcp_oauth_loopback = false;
    let mut acp_config: Option<PathBuf> = None;
    let mut a2a_config: Option<PathBuf> = None;
    let mut exec_config: Option<PathBuf> = None;
    let mut terminal_config: Option<PathBuf> = None;
    let mut memory_embedding_config: Option<PathBuf> = None;
    let mut skills_dir: Option<PathBuf> = None;
    let mut trace_log: Option<PathBuf> = None;
    let mut otlp_endpoint = None;
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
            "--trace-log" => {
                trace_log = Some(
                    args.next()
                        .ok_or("--trace-log requires a file path")?
                        .into(),
                )
            }
            "--otlp-endpoint" => {
                otlp_endpoint = Some(args.next().ok_or("--otlp-endpoint requires a URL")?)
            }
            "--mcp-config" => {
                mcp_config = Some(args.next().ok_or("--mcp-config requires a path")?.into())
            }
            "--mcp-oauth-config" => {
                mcp_oauth_config = Some(
                    args.next()
                        .ok_or("--mcp-oauth-config requires a path")?
                        .into(),
                )
            }
            "--mcp-oauth-vault" => {
                mcp_oauth_vault = Some(
                    args.next()
                        .ok_or("--mcp-oauth-vault requires a path")?
                        .into(),
                )
            }
            #[cfg(feature = "test-fixtures")]
            "--mcp-oauth-loopback-fixture" => mcp_oauth_loopback = true,
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
            "--db" => db = Some(args.next().ok_or("--db requires a path")?.into()),
            "--config" => user_config = Some(args.next().ok_or("--config requires a path")?.into()),
            "--demo" => demo = true,
            "--list-models" => list_models = true,
            "--check-model" => check_model = true,
            "--cursor" => model_cursor = Some(args.next().ok_or("--cursor requires a value")?),
            "--model-profile" => {
                model_profile = Some(args.next().ok_or("--model-profile requires a profile ID")?)
            }
            "--memory-embedding-config" => {
                memory_embedding_config = Some(
                    args.next()
                        .ok_or("--memory-embedding-config requires a path")?
                        .into(),
                );
            }
            "--help" | "-h" => {
                println!("{}", backup::USAGE);
                println!("fliuno-app-server --check-model --config PATH [--model-profile ID]");
                println!(
                    "fliuno-app-server --list-models --config PATH [--model-profile ID] [--cursor TOKEN]"
                );
                println!(
                    "fliuno-app-server [--config PATH] [--model-profile ID] [--demo] [--db PATH] [--mcp-config PATH] [--mcp-oauth-config PATH] [--mcp-oauth-vault PATH] [--acp-config PATH] [--a2a-config PATH] [--exec-config PATH] [--terminal-config PATH] [--skills-dir PATH] [--trace-log PATH] [--otlp-endpoint URL] [--schema]\nLocal stdio JSON-RPC. --mcp-oauth-config enables host authorization before initialize. --mcp-oauth-vault persists encrypted grants separately from the kernel database. --a2a-config registers host-selected remote Agents. --exec-config registers host-selected commands. --terminal-config registers persistent trusted-host PTYs. Each invocation or terminal input requires approval. Use --config for model selection and a local API key file."
                );
                println!(
                    "--memory-embedding-config PATH enables hybrid memory retrieval for host-selected workspaces."
                );
                return Ok(());
            }
            _ => return Err(format!("unknown argument: {arg}").into()),
        }
    }
    if model_profile.is_some() && (demo || user_config.is_none()) {
        return Err("--model-profile requires --config with a live model".into());
    }
    if list_models && check_model {
        return Err("Choose either --list-models or --check-model".into());
    }
    if list_models {
        let path = user_config
            .as_deref()
            .ok_or("--list-models requires --config")?;
        if demo {
            return Err("--list-models requires a live model provider".into());
        }
        let config = fliuno_config::Settings::load(path)?;
        let model = config.model_for_launch(model_profile.as_deref())?;
        let page = fliuno_providers::discover_models(model, model_cursor.as_deref())?;
        println!("{}", serde_json::to_string_pretty(&page)?);
        return Ok(());
    }
    if model_cursor.is_some() {
        return Err("--cursor requires --list-models".into());
    }
    if check_model {
        let path = user_config
            .as_deref()
            .ok_or("--check-model requires --config")?;
        if demo {
            return Err("--check-model requires a live model provider".into());
        }
        let config = fliuno_config::Settings::load(path)?;
        let model = config.model_for_launch(model_profile.as_deref())?;
        let check = fliuno_providers::check_model_connection(model);
        println!("{}", serde_json::to_string_pretty(&check)?);
        return Ok(());
    }
    // Parse every startup file before opening SQLite or allowing an earlier
    // adapter to connect/spawn. Semantic validation remains adapter-owned.
    let user_config_path = user_config;
    let mut user_config = user_config_path
        .as_deref()
        .map(fliuno_config::Settings::load)
        .transpose()?;
    if let Some(config) = &mut user_config {
        db = db.or_else(|| config.workbench.database.take());
        mcp_config = mcp_config.or_else(|| config.integrations.mcp.take());
        mcp_oauth_config = mcp_oauth_config.or_else(|| config.integrations.mcp_oauth.take());
        mcp_oauth_vault = mcp_oauth_vault.or_else(|| config.integrations.mcp_oauth_vault.take());
        acp_config = acp_config.or_else(|| config.integrations.acp.take());
        a2a_config = a2a_config.or_else(|| config.integrations.a2a.take());
        exec_config = exec_config.or_else(|| config.integrations.execution.take());
        terminal_config = terminal_config.or_else(|| config.integrations.terminal.take());
        skills_dir = skills_dir.or_else(|| config.integrations.skills.take());
        memory_embedding_config =
            memory_embedding_config.or_else(|| config.integrations.memory_embedding.take());
    }
    let db = db.unwrap_or_else(|| PathBuf::from(".fliuno/kernel.db"));
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
    let skill_catalog = skills_dir
        .map(fliuno_skills::SkillCatalog::open)
        .transpose()?;
    if let Some(config) = &exec_config {
        for program in &config.programs {
            if let Some(skill) = &program.skill {
                let catalog = skill_catalog
                    .as_ref()
                    .ok_or("offline Skill execution requires --skills-dir")?;
                let script = program
                    .assets
                    .iter()
                    .find(|asset| asset.path == skill.script)
                    .ok_or("offline Skill execution requires a pinned script asset")?;
                catalog.verify_script_pin(
                    &program.id,
                    &skill.script,
                    &script.source,
                    &script.sha256,
                )?;
            }
        }
    }
    let terminal_config = terminal_config
        .as_deref()
        .map(|path| configuration::load::<fliuno_terminal::Config>(path, "terminal"))
        .transpose()?;
    let memory_embeddings = memory_embedding_config
        .as_deref()
        .map(|path| -> Result<_, Box<dyn std::error::Error>> {
            let config = configuration::load::<MemoryEmbeddingConfig>(path, "memory embedding")?;
            let directory = path.parent().unwrap_or_else(|| std::path::Path::new("."));
            Ok(MemoryEmbeddingProvider::from_directory(config, directory)?)
        })
        .transpose()?;
    #[cfg(feature = "test-fixtures")]
    if mcp_oauth_loopback && mcp_oauth_config.is_none() {
        return Err("MCP OAuth fixture mode requires OAuth configuration".into());
    }
    let oauth_clients = if let Some(path) = mcp_oauth_config {
        let mcp = mcp_config
            .as_ref()
            .ok_or("MCP OAuth requires --mcp-config")?;
        let policy = configuration::load::<fliuno_mcp::McpOAuthConfig>(&path, "MCP OAuth")?;
        #[cfg(feature = "test-fixtures")]
        let clients = if mcp_oauth_loopback {
            mcp.oauth_fixture_clients(policy)?
        } else {
            mcp.oauth_clients(policy)?
        };
        #[cfg(not(feature = "test-fixtures"))]
        let clients = mcp.oauth_clients(policy)?;
        clients
    } else {
        Default::default()
    };
    if let Some(vault) = &mcp_oauth_vault {
        if oauth_clients.is_empty() {
            return Err("MCP OAuth vault requires --mcp-oauth-config".into());
        }
        if std::path::absolute(vault)? == std::path::absolute(&db)? {
            return Err("MCP OAuth vault must be separate from the kernel database".into());
        }
    }
    let (provider_name, provider) = if demo {
        fliuno_providers::from_settings(&fliuno_config::ModelSettings::default())?
    } else if let Some(config) = &user_config {
        fliuno_providers::from_settings(config.model_for_launch(model_profile.as_deref())?)?
    } else {
        fliuno_providers::from_env()?
    };
    let (input_tx, input_rx) = crossbeam_channel::bounded::<InputFrame>(64);
    thread::Builder::new()
        .name("fliuno-stdio-reader".into())
        .spawn(move || read_input_frames(input_tx))?;
    // Reuse the frame reader and writer for both phases of the same stdio session.
    let mut stdout = BufWriter::with_capacity(64 * 1024, io::stdout().lock());
    let grants = mcp_oauth::authorize(oauth_clients, mcp_oauth_vault, &input_rx, &mut stdout)?;
    let _diagnostics = diagnostics::start(trace_log.as_deref(), otlp_endpoint.as_deref())?;
    let mut kernel = Kernel::open(db, provider)?;
    fliuno_tools::register(&mut kernel)?;
    if let Some(catalog) = skill_catalog {
        fliuno_skills::register(&mut kernel, catalog)?;
    }
    if let Some(config) = mcp_config {
        fliuno_mcp::register_config_with_oauth(&mut kernel, config, grants)?;
    }
    let agent_terminal_logins = acp_config
        .map(|config| fliuno_agents::register_config_with_logins(&mut kernel, config))
        .transpose()?;
    if let Some(config) = a2a_config {
        fliuno_agents::register_a2a_config(&mut kernel, config)?;
    }
    if let Some(config) = exec_config {
        fliuno_exec::register_supervised_config(&mut kernel, config, std::env::current_exe()?)?;
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
    let live_model = provider_name != "demo";
    let mut server = RpcServer::with_terminals(kernel, provider_name, terminals);
    if live_model && let Some(path) = user_config_path {
        let initial =
            model_profile.or_else(|| user_config.and_then(|config| config.workbench.model_profile));
        server.enable_local_model_profiles(path, initial);
    }
    if let Some(registrations) = agent_terminal_logins {
        server.enable_local_agent_terminal_login(registrations);
    }
    if let Some(provider) = memory_embeddings {
        server.enable_memory_embeddings(provider)?;
    }
    let terminal_rx = server.take_terminal_notification_receiver();
    let (event_rx, event_subscription) = server.take_event_receiver().into_parts();
    let mut event_subscription = Some(event_subscription);
    let (progress_rx, progress_subscription) = server.take_progress_receiver().into_parts();
    let mut progress_subscription = Some(progress_subscription);
    let mut last_event_sequence = 0;
    let mut event_rx = event_rx;
    let mut watermark_rx = crossbeam_channel::never();
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
                        watermark_rx = crossbeam_channel::never();
                        event_subscription = Some(subscription);
                    } else if let Some((through_sequence, receiver)) = server.take_pending_watermark_subscription() {
                        last_event_sequence = through_sequence;
                        let (new_rx, subscription) = receiver.into_parts();
                        watermark_rx = new_rx;
                        event_rx = crossbeam_channel::never();
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
                    write_notification(&mut stdout, "notifications/event", EventParams { event })?;
                }
                Err(_) => {
                    write_event_gap(&mut stdout, last_event_sequence)?;
                    drop(event_subscription.take());
                    event_rx = crossbeam_channel::never();
                    server.mark_event_stream_gap();
                }
            },
            recv(watermark_rx) -> watermark => match watermark {
                Ok(pending) => {
                    let through_sequence = pending.through_sequence();
                    last_event_sequence = through_sequence;
                    write_notification(&mut stdout, "notifications/eventWatermark", EventWatermark { through_sequence })?;
                }
                Err(_) => {
                    write_event_gap(&mut stdout, last_event_sequence)?;
                    drop(event_subscription.take());
                    watermark_rx = crossbeam_channel::never();
                    server.mark_event_stream_gap();
                }
            },
            recv(progress_rx) -> progress => match progress {
                Ok(progress) => {
                    write_notification(&mut stdout, "notifications/modelProgress", ProgressParams { progress })?;
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
            if frame.len() + count > MAX_APP_REQUEST_BYTES {
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

#[derive(Serialize)]
struct EventParams {
    event: Event,
}

#[derive(Serialize)]
struct ProgressParams {
    progress: ModelProgress,
}

fn write_event_gap(stdout: &mut impl Write, after_sequence: u64) -> io::Result<()> {
    write_frame(
        stdout,
        &json!({
            "jsonrpc":"2.0", "method":"notifications/eventGap",
            "params":{"afterSequence":after_sequence}
        }),
    )
}

fn write_notification(
    stdout: &mut impl Write,
    method: &str,
    params: impl Serialize,
) -> io::Result<()> {
    #[derive(Serialize)]
    struct Notification<'a, P> {
        jsonrpc: &'static str,
        method: &'a str,
        params: P,
    }
    write_frame(
        stdout,
        &Notification {
            jsonrpc: "2.0",
            method,
            params,
        },
    )
}

fn write_frame(stdout: &mut impl Write, frame: &impl Serialize) -> io::Result<()> {
    serde_json::to_writer(&mut *stdout, frame)?;
    stdout.write_all(b"\n")?;
    stdout.flush()
}
