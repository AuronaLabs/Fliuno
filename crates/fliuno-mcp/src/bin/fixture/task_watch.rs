use serde_json::{Value, json};
use std::{
    fs::File,
    io::{self, BufRead, Write},
};

const TASK: &str = "fixture/remote-task";
const CREATED: &str = "2026-11-21T00:00:00Z";
const SUBSCRIPTION: &str = "io.modelcontextprotocol/subscriptionId";

pub fn run(path: &str, mode: &str) -> Result<(), Box<dyn std::error::Error>> {
    let mut trace = File::create(path)?;
    let mut output = io::stdout().lock();
    let mut reads = 0;
    for line in io::stdin().lock().lines() {
        let request: Value = serde_json::from_str(&line?)?;
        writeln!(trace, "{request}")?;
        let id = &request["id"];
        let mut task = json!({"taskId":TASK,"status":"working","createdAt":CREATED,
            "lastUpdatedAt":CREATED,"ttlMs":null});
        let result = match request["method"].as_str() {
            Some("server/discover") => json!({"resultType":"complete",
                "supportedVersions":["2026-07-28"],"capabilities":{
                    "tools":{},"extensions":{"io.modelcontextprotocol/tasks":{}}},
                "ttlMs":0,"cacheScope":"private"}),
            Some("tools/list") => json!({"resultType":"complete","tools":[{
                "name":"start","inputSchema":{"type":"object"},"outputSchema":{"type":"integer"}
            }]}),
            Some("tools/call") => {
                task["resultType"] = json!("task");
                task
            }
            Some("tasks/get") => {
                assert_eq!(request["params"]["taskId"], TASK);
                task["resultType"] = json!("complete");
                reads += 1;
                if reads > 1 {
                    task["status"] = json!("completed");
                    task["result"] = json!({"content":[],"structuredContent":42});
                }
                task
            }
            Some("subscriptions/listen") => {
                let filter = match mode {
                    "overbroad" => json!({"taskIds":[TASK],"resourcesListChanged":true}),
                    "declined" => json!({"taskIds":[]}),
                    _ => json!({"taskIds":[TASK]}),
                };
                let meta = json!({(SUBSCRIPTION):id});
                let ack = json!({"jsonrpc":"2.0","method":"notifications/subscriptions/acknowledged",
                    "params":{"_meta":meta,"notifications":filter}});
                writeln!(output, "{ack}")?;
                match mode {
                    "eof" => {
                        output.flush()?;
                        return Ok(());
                    }
                    "ended" => json!({"resultType":"complete","_meta":meta}),
                    "notified" | "wrong_creation" | "timeout" => {
                        task["_meta"] = if mode == "timeout" {
                            json!({(SUBSCRIPTION):"unrelated-subscription"})
                        } else {
                            meta
                        };
                        task["status"] = json!("completed");
                        task["result"] = json!({"structuredContent":"unverified notice"});
                        if mode == "wrong_creation" {
                            task["createdAt"] = json!("2026-11-21T01:00:00Z");
                        }
                        writeln!(
                            output,
                            "{}",
                            json!({"jsonrpc":"2.0","method":"notifications/tasks","params":task})
                        )?;
                        output.flush()?;
                        continue;
                    }
                    _ => {
                        output.flush()?;
                        continue;
                    }
                }
            }
            Some("notifications/cancelled") => continue,
            other => panic!("unexpected fixture request: {other:?}"),
        };
        writeln!(
            output,
            "{}",
            json!({"jsonrpc":"2.0","id":id,"result":result})
        )?;
        output.flush()?;
    }
    Ok(())
}
