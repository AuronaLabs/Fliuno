//! Explicit test-fixtures feature only; real SDK server for process contracts.
use rmcp::{
    ErrorData, RoleServer, ServerHandler, ServiceExt,
    model::{
        CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, ListToolsResult,
        PaginatedRequestParams, ServerCapabilities, ServerConfig,
    },
    service::RequestContext,
};
use serde_json::json;
#[path = "fixture/content.rs"]
mod content;
#[path = "fixture/legacy_tasks.rs"]
mod legacy_tasks;
#[path = "fixture/task_watch.rs"]
mod task_watch;

struct Fixture(std::sync::atomic::AtomicU64);
impl ServerHandler for Fixture {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
    }
    async fn list_tools(
        &self,
        _: Option<PaginatedRequestParams>,
        _: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        Ok(serde_json::from_value(json!({
            "tools": [{
                "name": "inspect",
                "description": "Isolation fixture",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "hostPath": {"type": "string"},
                        "endpoint": {"type": "string"},
                        "stagedPath": {"type": "string"},
                        "probeChild": {"type": "boolean"},
                        "delayMs": {"type": "integer", "minimum": 0, "maximum": 60000}
                    },
                    "required": ["hostPath", "endpoint"],
                    "additionalProperties": false
                }
            }]
        }))
        .unwrap())
    }
    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        let arguments = request.arguments.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(
            arguments
                .get("delayMs")
                .and_then(|n| n.as_u64())
                .unwrap_or(0),
        ))
        .await;
        let mut report = inspect(
            arguments["hostPath"].as_str().unwrap(),
            arguments["endpoint"].as_str().unwrap(),
        );
        if let Some(path) = arguments.get("stagedPath").and_then(|value| value.as_str()) {
            report["asset"] = json!(std::fs::read_to_string(path).unwrap());
        }
        if arguments
            .get("probeChild")
            .and_then(|value| value.as_bool())
            == Some(true)
        {
            let result = std::env::current_exe().and_then(|program| {
                std::process::Command::new(program)
                    .arg("--probe-exit")
                    .status()
            });
            report["child"] = json!(result.is_ok_and(|status| status.success()));
        }
        report["calls"] = json!(self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1);
        Ok(CallToolResult::success(vec![ContentBlock::text(report.to_string())]).into())
    }
}
fn inspect(host_path: &str, endpoint: &str) -> serde_json::Value {
    json!({"container":fliuno_sandbox::current_process_is_app_container().unwrap_or(false),
        "read":std::fs::read(host_path).is_ok(), "write":std::fs::write(host_path,"fixture modified").is_ok(),
        "network":std::net::TcpStream::connect_timeout(&endpoint.parse().unwrap(), std::time::Duration::from_millis(100)).is_ok(),
        "cwd":std::env::current_dir().unwrap(), "insideWrite":std::fs::write("inside.txt","fixture output").is_ok()})
}
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    if args.first().is_some_and(|arg| arg == "--task-watch") {
        return task_watch::run(&args[1], &args[2]);
    }
    if args.first().is_some_and(|arg| arg == "--legacy-tasks") {
        return legacy_tasks::run(
            &args[1],
            args.get(2).is_none_or(|mode| mode != "no-capability"),
        );
    }
    if let Some(mode) = args.first().filter(|mode| {
        mode.as_str() == "--legacy-exit-on-discover"
            || mode.as_str() == "--legacy-stays-on-discover"
            || mode.as_str() == "--legacy-silent-exit-on-discover"
    }) {
        return legacy_stdio_compat(&args[1], mode.as_str());
    }
    // The stdio transport uses blocking pipe handles. A timer is enough here;
    // Tokio's Unix signal reactor would try to create a blocked socket pair.
    tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()?
        .block_on(run())
}

async fn run() -> Result<(), Box<dyn std::error::Error>> {
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    if args.first().is_some_and(|arg| arg == "--probe-exit") {
        return Ok(());
    }
    if args.first().is_some_and(|arg| arg == "--probe-native") {
        println!("{}", inspect(&args[1], &args[2]));
        return Ok(());
    }
    if let Some(mode) = args.first().map(String::as_str) {
        match mode {
            "--content" => return content::run(false).await,
            "--content-legacy" => return content::run(true).await,
            _ => {}
        }
    }
    Fixture(std::sync::atomic::AtomicU64::new(0))
        .serve(rmcp::transport::stdio())
        .await?
        .waiting()
        .await?;
    Ok(())
}

fn legacy_stdio_compat(marker: &str, mode: &str) -> Result<(), Box<dyn std::error::Error>> {
    use std::io::{BufRead, Write};

    let starts = std::fs::read_to_string(marker)
        .ok()
        .and_then(|value| value.parse::<u8>().ok())
        .unwrap_or(0)
        + 1;
    std::fs::write(marker, starts.to_string())?;
    let stdin = std::io::stdin();
    let mut stdout = std::io::BufWriter::new(std::io::stdout().lock());
    for line in stdin.lock().lines() {
        let message: serde_json::Value = serde_json::from_str(&line?)?;
        match message["method"].as_str().unwrap_or_default() {
            "server/discover" if starts == 1 => {
                if mode == "--legacy-silent-exit-on-discover" {
                    return Ok(());
                }
                serde_json::to_writer(
                    &mut stdout,
                    &json!({"jsonrpc":"2.0","id":message["id"],"error":{"code":-32601,"message":"Method not found"}}),
                )?;
                stdout.write_all(b"\n")?;
                stdout.flush()?;
                if mode == "--legacy-exit-on-discover" {
                    return Ok(());
                }
            }
            "initialize" => {
                serde_json::to_writer(
                    &mut stdout,
                    &json!({"jsonrpc":"2.0","id":message["id"],"result":{
                        "protocolVersion":"2025-11-25",
                        "capabilities":{"tools":{}},
                        "serverInfo":{"name":"legacy-fixture","version":"1.0.0"}
                    }}),
                )?;
                stdout.write_all(b"\n")?;
                stdout.flush()?;
            }
            "tools/list" => {
                serde_json::to_writer(
                    &mut stdout,
                    &json!({"jsonrpc":"2.0","id":message["id"],"result":{"tools":[]}}),
                )?;
                stdout.write_all(b"\n")?;
                stdout.flush()?;
            }
            _ => {}
        }
    }
    Ok(())
}
