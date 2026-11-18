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
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    if args.first().is_some_and(|arg| arg == "--probe-native") {
        println!("{}", inspect(&args[1], &args[2]));
        return Ok(());
    }
    if args.first().is_some_and(|arg| arg == "--content") {
        return content::run().await;
    }
    Fixture(std::sync::atomic::AtomicU64::new(0))
        .serve(rmcp::transport::stdio())
        .await?
        .waiting()
        .await?;
    Ok(())
}
