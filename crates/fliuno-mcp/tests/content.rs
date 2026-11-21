//! Real HTTP/SDK/SQLite contracts for approved resource and prompt operations.
#[path = "support/content.rs"]
mod content;
#[path = "support/http.rs"]
mod wire;
use fliuno_core::{CancellationToken, Tool, ToolContext, ToolError, ToolOutput};
use fliuno_protocol::{ToolCall, Workspace};
use serde_json::{Value, json};
use std::time::Duration;
use wire::{Fixture, Response};
#[path = "content/completion.rs"]
mod completion;
#[path = "content/persistence.rs"]
mod persistence;
#[path = "content/resource_import.rs"]
mod resource_import;
#[path = "content/skills.rs"]
mod skills;

fn connect(fixture: &Fixture, timeout: u64) -> Vec<Box<dyn Tool>> {
    fliuno_mcp::connect_server(serde_json::from_value(json!({
        "id":"remote","transport":"streamable_http","url":fixture.url("/mcp"),"timeoutMs":timeout,
    })).unwrap()).unwrap()
}
fn tool<'a>(tools: &'a [Box<dyn Tool>], label: &str) -> &'a dyn Tool {
    tools
        .iter()
        .find(|tool| tool.spec().name == format!("mcp_remote_{label}"))
        .unwrap()
        .as_ref()
}
fn invoke(
    tool: &dyn Tool,
    args: Value,
    cancellation: &CancellationToken,
) -> Result<ToolOutput, ToolError> {
    tool.execute(
        &ToolContext {
            session_id: "s",
            task_id: "t",
            workspace: &Workspace {
                id: "w".into(),
                root: "unused".into(),
            },
        },
        &ToolCall {
            id: "call".into(),
            name: tool.spec().name,
            arguments: args,
        },
        cancellation,
    )
}
fn count(fixture: &Fixture, method: &str) -> usize {
    fixture
        .requests()
        .iter()
        .filter(|request| request.body["method"] == method)
        .count()
}

#[test]
fn resource_only_peers_are_lazy_paginated_and_never_dereference_host_uris() {
    let fixture = Fixture::start(|request| {
        if request.body["method"] == "server/discover" {
            return content::result(
                request,
                json!({"resultType":"complete","supportedVersions":["2026-07-28"],
                "capabilities":{"resources":{}},"ttlMs":0,"cacheScope":"private"}),
            );
        }
        content::reply(request)
    });
    let tools = connect(&fixture, 2000);
    assert_eq!(tools.len(), 4);
    assert_eq!(fixture.requests().len(), 1);
    let list = tool(&tools, "resources_list");
    let first = invoke(list, json!({}), &CancellationToken::new()).unwrap();
    assert_eq!(first.output["result"]["nextCursor"], "second");
    assert_eq!(count(&fixture, "resources/list"), 1);
    let second = invoke(list, json!({"cursor":"second"}), &CancellationToken::new()).unwrap();
    assert_eq!(second.output["result"]["resources"][0]["name"], "second");
    assert_eq!(count(&fixture, "resources/list"), 2);
    let directory =
        std::env::temp_dir().join(format!("fliuno-resource-uri-{}", std::process::id()));
    std::fs::create_dir_all(&directory).unwrap();
    let host_file = directory.join("secret.txt");
    std::fs::write(&host_file, "HOST_SECRET_MUST_NOT_BE_READ").unwrap();
    let file_uri = reqwest::Url::from_file_path(&host_file)
        .unwrap()
        .to_string();
    let unvisited = Fixture::start(|_| panic!("resource URI caused an independent host fetch"));
    for uri in [
        file_uri,
        unvisited.url("/never-fetch"),
        "custom+scheme:opaque".into(),
    ] {
        let read = invoke(
            tool(&tools, "resources_read"),
            json!({"uri":uri}),
            &CancellationToken::new(),
        )
        .unwrap();
        assert_eq!(
            read.output["result"]["contents"][0]["text"],
            "REMOTE_CONTENT_ONLY"
        );
        assert_eq!(read.output["result"]["contents"][1]["blob"], "AAH/");
        assert_eq!(read.output["grantsPermissions"], false);
        assert!(
            !read
                .output
                .to_string()
                .contains("HOST_SECRET_MUST_NOT_BE_READ")
        );
        assert!(read.artifacts.is_empty());
    }
    assert_eq!(count(&fixture, "resources/read"), 3);
    assert_eq!(count(&fixture, "tools/list"), 0);
    assert!(unvisited.requests().is_empty());
    assert_eq!(
        std::fs::read_to_string(&host_file).unwrap(),
        "HOST_SECRET_MUST_NOT_BE_READ"
    );
    std::fs::remove_file(host_file).unwrap();
    std::fs::remove_dir(directory).unwrap();
}

#[test]
fn argument_overrides_budgets_and_predispatch_cancellation_send_nothing() {
    let fixture = Fixture::start(content::reply);
    let tools = connect(&fixture, 2000);
    for tool in &tools {
        assert!(tool.spec().name.len() <= 64);
        assert!(matches!(
            tool.spec().effect,
            fliuno_core::ToolEffect::External
        ));
        for args in [
            json!({"endpoint":"https://other.invalid"}),
            json!({"_meta":{}}),
            json!({"requestState":"old"}),
        ] {
            assert!(tool.validate(&args).is_err());
            assert!(matches!(
                invoke(tool.as_ref(), args, &CancellationToken::new()),
                Err(ToolError::Failed(_))
            ));
        }
    }
    for (label, args) in [
        ("resources_list", json!({"cursor":null})),
        ("resources_templates", json!({"cursor":"x".repeat(4097)})),
        ("prompts_list", json!({"cursor":"\n"})),
        ("resources_read", json!({"uri":"../local-file"})),
        ("resources_read", json!({"uri":"file:/host file"})),
        (
            "prompts_get",
            json!({"name":"draft","arguments":{"topic":123}}),
        ),
        (
            "prompts_get",
            json!({"name":"draft","arguments":{"topic":"x".repeat(8193)}}),
        ),
    ] {
        assert!(tool(&tools, label).validate(&args).is_err());
    }
    let cancellation = CancellationToken::new();
    cancellation.cancel();
    assert!(matches!(
        invoke(
            tool(&tools, "prompts_get"),
            json!({"name":"draft"}),
            &cancellation
        ),
        Err(ToolError::Failed(_))
    ));
    assert_eq!(fixture.requests().len(), 1);
    assert!(
        invoke(
            tool(&tools, "prompts_list"),
            json!({}),
            &CancellationToken::new()
        )
        .is_ok()
    );
}

#[test]
fn expired_content_is_not_served_when_refresh_fails() {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    for (method, label, args) in [
        ("resources/list", "resources_list", json!({})),
        ("resources/templates/list", "resources_templates", json!({})),
        (
            "resources/read",
            "resources_read",
            json!({"uri":"fixture:first"}),
        ),
        ("prompts/list", "prompts_list", json!({})),
    ] {
        let calls = Arc::new(AtomicUsize::new(0));
        let observed = Arc::clone(&calls);
        let fixture = Fixture::start(move |request| {
            if request.body["method"] == method && observed.fetch_add(1, Ordering::SeqCst) == 1 {
                return Response::json(500, json!({"secret":"REMOTE_ERROR_MUST_NOT_LEAK"}));
            }
            content::reply_with_ttl(request, 1)
        });
        let tools = connect(&fixture, 2000);
        let target = tool(&tools, label);
        invoke(target, args.clone(), &CancellationToken::new()).unwrap();
        std::thread::sleep(Duration::from_millis(10));
        let error = invoke(target, args.clone(), &CancellationToken::new())
            .err()
            .unwrap();
        assert!(matches!(error, ToolError::OutcomeUnknown(_)));
        assert!(!error.to_string().contains("REMOTE_ERROR_MUST_NOT_LEAK"));
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert!(matches!(
            invoke(target, args, &CancellationToken::new()),
            Err(ToolError::Failed(_))
        ));
    }
}

#[test]
fn remote_json_rpc_read_errors_are_known_and_keep_the_connection() {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    let failures = Arc::new(AtomicUsize::new(0));
    let observed = Arc::clone(&failures);
    let fixture = Fixture::start(move |request| {
        if request.body["method"] == "resources/list"
            && observed.fetch_add(1, Ordering::SeqCst) == 0
        {
            return Response::json(
                200,
                json!({
                    "jsonrpc":"2.0",
                    "id":request.body["id"],
                    "error":{"code":-32602,"message":"READ_ERROR_MUST_NOT_LEAK"}
                }),
            );
        }
        content::reply(request)
    });
    let tools = connect(&fixture, 2000);
    let list = tool(&tools, "resources_list");
    let error = invoke(list, json!({}), &CancellationToken::new())
        .err()
        .unwrap();
    assert!(matches!(error, ToolError::Failed(_)));
    assert!(!error.to_string().contains("READ_ERROR_MUST_NOT_LEAK"));
    assert!(invoke(list, json!({}), &CancellationToken::new()).is_ok());
    assert_eq!(count(&fixture, "resources/list"), 2);
}

#[test]
fn content_collection_cursor_and_transport_byte_limits_do_not_trigger_retries() {
    let cases = [
        (
            "resources/list",
            "resources_list",
            json!({}),
            json!({"resources":vec![json!({"name":"x","uri":"fixture:x"});257]}),
        ),
        (
            "resources/templates/list",
            "resources_templates",
            json!({}),
            json!({"resourceTemplates":vec![json!({"name":"x","uriTemplate":"fixture:{x}"});257]}),
        ),
        (
            "prompts/list",
            "prompts_list",
            json!({}),
            json!({"prompts":vec![json!({"name":"x"});257]}),
        ),
        (
            "resources/read",
            "resources_read",
            json!({"uri":"fixture:x"}),
            json!({"contents":vec![json!({"uri":"fixture:x","text":"x"});65]}),
        ),
        (
            "prompts/get",
            "prompts_get",
            json!({"name":"x"}),
            json!({"messages":vec![json!({"role":"user","content":{"type":"text","text":"x"}});65]}),
        ),
        (
            "resources/list",
            "resources_list",
            json!({}),
            json!({"resources":[],"nextCursor":""}),
        ),
        (
            "resources/read",
            "resources_read",
            json!({"uri":"fixture:x"}),
            json!({"contents":[{"uri":"fixture:x","text":"x".repeat(1024*1024)}]}),
        ),
    ];
    for (method, label, args, result) in cases {
        let fixture = Fixture::start(move |request| {
            if request.body["method"] == method {
                content::result(request, result.clone())
            } else {
                content::reply(request)
            }
        });
        let tools = connect(&fixture, 2000);
        assert!(matches!(
            invoke(tool(&tools, label), args, &CancellationToken::new()),
            Err(ToolError::OutcomeUnknown(_))
        ));
        assert_eq!(count(&fixture, method), 1);
    }
}

#[test]
fn resource_and_prompt_continuations_are_not_followed_or_disclosed() {
    for (method, label, args, mut partial) in [
        (
            "resources/read",
            "resources_read",
            json!({"uri":"fixture:x"}),
            json!({"resultType":"input_required"}),
        ),
        (
            "prompts/get",
            "prompts_get",
            json!({"name":"draft"}),
            json!({"resultType":"input_required"}),
        ),
        (
            "resources/read",
            "resources_read",
            json!({"uri":"fixture:x"}),
            json!({"resultType":"input_required","contents":[]}),
        ),
        (
            "prompts/get",
            "prompts_get",
            json!({"name":"draft"}),
            json!({"resultType":"task","messages":[]}),
        ),
        (
            "resources/list",
            "resources_list",
            json!({}),
            json!({"resultType":"extension","resources":[]}),
        ),
        (
            "resources/templates/list",
            "resources_templates",
            json!({}),
            json!({"resultType":"input_required","resourceTemplates":[]}),
        ),
        (
            "prompts/list",
            "prompts_list",
            json!({}),
            json!({"resultType":"extension","prompts":[]}),
        ),
    ] {
        partial["requestState"] = json!("PRIVATE_CONTINUATION");
        let fixture = Fixture::start(move |request| {
            if request.body["method"] == method {
                content::result(request, partial.clone())
            } else {
                content::reply(request)
            }
        });
        let tools = connect(&fixture, 2000);
        let Err(ToolError::OutcomeUnknown(message)) =
            invoke(tool(&tools, label), args, &CancellationToken::new())
        else {
            panic!("{method} accepted a non-final response");
        };
        assert!(!message.contains("PRIVATE_CONTINUATION"));
        assert!(matches!(
            invoke(
                tool(&tools, "prompts_list"),
                json!({}),
                &CancellationToken::new()
            ),
            Err(ToolError::Failed(_))
        ));
        assert_eq!(count(&fixture, method), 1);
    }
}

#[test]
fn cancelling_or_timing_out_content_after_dispatch_closes_the_shared_host() {
    for cancel in [false, true] {
        let fixture = Fixture::start(|request| {
            if request.body["method"] == "resources/read" {
                Response::hold()
            } else {
                content::reply(request)
            }
        });
        let tools = connect(&fixture, 600);
        let cancellation = CancellationToken::new();
        std::thread::scope(|scope| {
            let read = scope.spawn(|| {
                invoke(
                    tool(&tools, "resources_read"),
                    json!({"uri":"fixture:x"}),
                    &cancellation,
                )
            });
            fixture.wait_for("resources/read");
            if cancel {
                cancellation.cancel();
            }
            assert!(matches!(
                read.join().unwrap(),
                Err(ToolError::OutcomeUnknown(_))
            ));
        });
        assert!(matches!(
            invoke(
                tool(&tools, "prompts_list"),
                json!({}),
                &CancellationToken::new()
            ),
            Err(ToolError::Failed(_))
        ));
        assert_eq!(count(&fixture, "resources/read"), 1);
        assert_eq!(count(&fixture, "prompts/list"), 0);
    }
}

#[test]
fn cancelling_a_prompt_waiting_behind_a_resource_does_not_close_the_owner() {
    use std::{
        sync::{Mutex, mpsc},
        time::Duration,
    };
    let (release, released) = mpsc::sync_channel(1);
    let released = Mutex::new(released);
    let fixture = Fixture::start(move |request| {
        if request.body["method"] == "resources/read" {
            released
                .lock()
                .unwrap()
                .recv_timeout(Duration::from_secs(5))
                .unwrap();
        }
        content::reply(request)
    });
    let tools = connect(&fixture, 4000);
    let cancellation = CancellationToken::new();
    std::thread::scope(|scope| {
        let owner = scope.spawn(|| {
            invoke(
                tool(&tools, "resources_read"),
                json!({"uri":"fixture:x"}),
                &CancellationToken::new(),
            )
        });
        fixture.wait_for("resources/read");
        let (attempted, attempt) = mpsc::sync_channel(1);
        let (result, returned) = mpsc::sync_channel(1);
        let queued_tool = tool(&tools, "prompts_list");
        let queued_cancellation = cancellation.clone();
        let waiting = scope.spawn(move || {
            attempted.send(()).unwrap();
            let _ = result.send(invoke(queued_tool, json!({}), &queued_cancellation));
        });
        attempt.recv_timeout(Duration::from_secs(1)).unwrap();
        cancellation.cancel();
        let queued_result = returned.recv_timeout(Duration::from_secs(1));
        let calls_before_release = count(&fixture, "prompts/list");
        release.send(()).unwrap();
        owner.join().unwrap().unwrap();
        waiting.join().unwrap();
        assert!(
            matches!(queued_result.unwrap(), Err(ToolError::Failed(message)) if message.contains("no request sent"))
        );
        assert_eq!(calls_before_release, 0);
        invoke(
            tool(&tools, "prompts_list"),
            json!({}),
            &CancellationToken::new(),
        )
        .unwrap();
    });
    assert_eq!(count(&fixture, "resources/read"), 1);
    assert_eq!(count(&fixture, "prompts/list"), 1);
}
