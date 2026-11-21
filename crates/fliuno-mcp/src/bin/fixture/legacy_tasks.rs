//! Wire fixture for the old revision. The current SDK's server types no longer
//! serialize the old capabilities, execution declarations, or task envelope.
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    fs::OpenOptions,
    io::{BufRead, Write},
};

pub(super) fn run(path: &str, capability: bool) -> Result<(), Box<dyn std::error::Error>> {
    let mut transcript = OpenOptions::new().create(true).append(true).open(path)?;
    let mut tasks = BTreeMap::<String, Value>::new();
    let mut stdout = std::io::BufWriter::new(std::io::stdout().lock());
    for line in std::io::stdin().lock().lines() {
        let message: Value = serde_json::from_str(&line?)?;
        writeln!(transcript, "{message}")?;
        let params = &message["params"];
        let task_id = params["taskId"].as_str().unwrap_or_default();
        let method = message["method"].as_str().unwrap_or_default();
        let result = match method {
            "server/discover" => Err(-32601),
            "initialize" => {
                assert_eq!(params["capabilities"]["tasks"], json!({}));
                assert!(params["capabilities"].get("extensions").is_none());
                let mut capabilities = json!({"tools":{}});
                if capability {
                    capabilities["tasks"] = json!({"requests":{"tools":{"call":{}}},"cancel":{}});
                } else {
                    capabilities["tasks"] = json!({"requests":{"tools":{}},"cancel":{}});
                }
                Ok(
                    json!({"protocolVersion":"2025-11-25","capabilities":capabilities,"serverInfo":{"name":"legacy-tasks","version":"1"}}),
                )
            }
            "notifications/initialized" | "notifications/cancelled" => continue,
            "tools/list" => Ok(json!({"tools":[
                {"name":"required","inputSchema":{"type":"object"},"execution":{"taskSupport":"required"},
                    "outputSchema":{"type":"object","properties":{"answer":{"type":"integer"}},"required":["answer"]}},
                {"name":"optional","inputSchema":{"type":"object"},"execution":{"taskSupport":"optional"}},
                {"name":"forbidden","inputSchema":{"type":"object"},"execution":{"taskSupport":"forbidden"}}
            ]})),
            "tools/call" => {
                let name = params["name"].as_str().unwrap();
                let augmented = params.get("task").is_some();
                assert!(name != "required" || augmented);
                assert!(!augmented || capability && name != "forbidden");
                if augmented {
                    assert_eq!(params["task"], json!({}));
                    let case = params["arguments"]["case"].as_str().unwrap();
                    if case == "lost" {
                        return Ok(());
                    }
                    let now = if case == "ttl" {
                        "2025-11-25T00:00:00Z".to_owned()
                    } else {
                        chrono::DateTime::<chrono::Utc>::from(std::time::SystemTime::now())
                            .to_rfc3339()
                    };
                    let handle = json!({"taskId":case,"status":"working","createdAt":now,
                        "lastUpdatedAt":now,"ttl":15000,"pollInterval":100});
                    let mut snapshot = handle.clone();
                    if case == "ttl" {
                        snapshot["status"] = json!("input_required");
                    } else if !matches!(case, "working" | "expired" | "notify" | "unrelated") {
                        snapshot["status"] = json!("completed");
                    }
                    if matches!(case, "notify" | "unrelated") {
                        snapshot["pollInterval"] = json!(60000);
                    }
                    tasks.insert(case.to_owned(), snapshot);
                    Ok(json!({"task":handle}))
                } else {
                    Ok(json!({"content":[{"type":"text","text":"synchronous"}]}))
                }
            }
            "tasks/get" => match tasks.get_mut(task_id).filter(|_| task_id != "expired") {
                Some(task) => {
                    let snapshot = task.clone();
                    if matches!(task_id, "notify" | "unrelated") && task["status"] == "working" {
                        task["status"] = json!("input_required");
                        let mut notice = json!({"jsonrpc":"2.0","method":"notifications/tasks/status","params":task});
                        if task_id == "unrelated" {
                            notice["params"]["taskId"] = json!("other-task");
                            writeln!(stdout, "{notice}")?;
                            notice["params"]["taskId"] = json!(task_id);
                            notice["params"]["createdAt"] = json!("2026-11-20T00:00:00Z");
                        }
                        // The wire snapshot may look newer, but the client
                        // must re-read and trust only the request reply.
                        notice["params"]["status"] = json!("completed");
                        writeln!(stdout, "{notice}")?;
                        writeln!(stdout, "{notice}")?;
                    }
                    Ok(snapshot)
                }
                None => Err(-32602),
            },
            "tasks/result"
                if tasks
                    .get(task_id)
                    .is_some_and(|task| task["status"] == "completed") =>
            {
                let answer = if task_id == "invalid" {
                    json!("invalid private data")
                } else {
                    json!(42)
                };
                Ok(json!({"content":[],"structuredContent":{"answer":answer},
                    "_meta":{"io.modelcontextprotocol/related-task":{"taskId":if task_id == "mismatch" { "another-task" } else { task_id }}}}))
            }
            "tasks/cancel"
                if tasks
                    .get(task_id)
                    .is_some_and(|task| task["status"] == "working") =>
            {
                let task = tasks.get_mut(task_id).unwrap();
                task["status"] = json!("cancelled");
                Ok(task.clone())
            }
            _ => Err(-32601),
        };
        let response = match result {
            Ok(result) => json!({"jsonrpc":"2.0","id":message["id"],"result":result}),
            Err(code) => {
                json!({"jsonrpc":"2.0","id":message["id"],"error":{"code":code,"message":"fixture error"}})
            }
        };
        writeln!(stdout, "{response}")?;
        stdout.flush()?;
    }
    Ok(())
}
