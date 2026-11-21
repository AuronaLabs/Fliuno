use super::*;
use http_wire::{Fixture, Response};

struct SkillDirectory(PathBuf);
impl SkillDirectory {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!("fliuno-skill-{}", uuid::Uuid::new_v4()));
        let self_ = Self(root);
        fs::create_dir_all(self_.script().parent().unwrap()).unwrap();
        fs::write(self_.0.join("skills/text-transform/SKILL.md"), "---\nname: text-transform\ndescription: Transform input text.\n---\nUse the registered interpreter when approved.\n").unwrap();
        fs::write(self_.script(), "upper\n").unwrap();
        self_.configure(15_000);
        self_
    }

    fn script(&self) -> PathBuf {
        self.0.join("skills/text-transform/scripts/transform.txt")
    }

    fn digest(&self) -> String {
        fliuno_sandbox::program_digest(&self.script()).unwrap()
    }

    fn configure(&self, timeout: u64) {
        let isolation = if cfg!(windows) {
            "windows_app_container"
        } else {
            "linux_landlock"
        };
        fs::write(self.0.join("exec.json"), json!({"programs":[{
            "id":"text-transform", "command":env!("CARGO_BIN_EXE_fliuno-app-exec-fixture"),
            "isolation":isolation, "timeoutMs":timeout, "outputBytes":1024,
            "args":["skill-script","scripts/transform.txt"], "skill":{"script":"scripts/transform.txt", "artifactTitle":"Processed text"},
            "assets":[{"path":"scripts/transform.txt","source":self.script(),"sha256":self.digest()}]
        }]}).to_string()).unwrap();
    }

    fn peer(&self, base: &str) -> Peer {
        let mut command = Command::new(env!("CARGO_BIN_EXE_fliuno-app-server"));
        #[cfg(target_os = "linux")]
        command.env("TMPDIR", &self.0);
        let child = command
            .arg("--db")
            .arg(self.0.join("kernel.db"))
            .arg("--skills-dir")
            .arg(self.0.join("skills"))
            .arg("--exec-config")
            .arg(self.0.join("exec.json"))
            .env("FLIUNO_PROVIDER", "openai")
            .env("FLIUNO_MODEL", "fixture-model")
            .env("OPENAI_API_KEY", "fixture-key")
            .env("OPENAI_BASE_URL", base)
            .env("FLIUNO_EXEC_SECRET", "fixture-not-inherited")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        let mut peer = Peer::from_child(child);
        peer.call(
            "initialize",
            json!({"protocolVersion":"0.1","clientName":"skill-execution-test"}),
        );
        peer
    }

    fn wait_until_running(&self, token: uuid::Uuid) {
        #[cfg(windows)]
        let (root, prefix, relative) = (
            PathBuf::from(std::env::var_os("LOCALAPPDATA").unwrap()).join("Packages"),
            "fliuno.",
            "AC/LocalState/fliuno",
        );
        #[cfg(target_os = "linux")]
        let (root, prefix, relative) = (self.0.clone(), "fliuno-linux-", "");
        let marker = format!("ready-{}", token.simple());
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let found = fs::read_dir(&root)
                .unwrap()
                .filter_map(Result::ok)
                .any(|entry| {
                    entry.file_name().to_string_lossy().starts_with(prefix)
                        && fs::read(entry.path().join(relative).join(&marker))
                            .is_ok_and(|bytes| bytes == b"running")
                });
            if found {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "no Skill process startup marker {marker} beneath {root:?}"
            );
            thread::sleep(Duration::from_millis(10));
        }
    }
}
impl Drop for SkillDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn model() -> Fixture {
    Fixture::start(|request| {
        assert_eq!(request.path, "/v1/responses");
        let input = request.body["input"].as_array().unwrap();
        if let Some(output) = input
            .iter()
            .find(|item| item["type"] == "function_call_output")
        {
            let _result: Value = serde_json::from_str(output["output"].as_str().unwrap()).unwrap();
            return Response::json(
                200,
                json!({"status":"completed","output":[{"type":"message","content":[{"type":"output_text","text":"Skill result read"}]}]}),
            );
        }
        let text = input.last().unwrap()["content"].as_str().unwrap();
        let (name, args) = if text == "discover" {
            ("skills_list", json!({}))
        } else {
            let name = request.body["tools"]
                .as_array()
                .unwrap()
                .iter()
                .find_map(|tool| {
                    tool["name"]
                        .as_str()
                        .filter(|name| name.starts_with("skill_text-transform_"))
                })
                .unwrap();
            let args = if text == "override" {
                json!({"stdin":"content", "args":["--other-script"]})
            } else {
                json!({"stdin":text})
            };
            (name, args)
        };
        Response::json(
            200,
            json!({"status":"completed","output":[{"type":"function_call","call_id":"skill-call","name":name,"arguments":args.to_string()}]}),
        )
    })
}

fn session(peer: &mut Peer, directory: &SkillDirectory) -> Value {
    let workspace = peer.call("workspace/open", json!({"root":directory.0}));
    peer.call("session/create", json!({"workspaceId":workspace["id"]}))["id"].clone()
}
fn start(peer: &mut Peer, session: &Value, text: &str, key: &str, status: &str) -> Value {
    let task = peer.call("task/start", json!({"sessionId":session,"input":text,"clientRequestId":key,"context":{"mode":"isolated"}}));
    peer.wait_for_status(task["id"].as_str().unwrap(), status)
}
fn respond(peer: &mut Peer, task: &Value, decision: &str) {
    peer.call(
        "approval/respond",
        json!({"taskId":task["id"],"approvalId":task["pendingApproval"]["id"],"decision":decision}),
    );
}
fn events(peer: &mut Peer, task: &Value) -> Vec<Value> {
    peer.call("events/list", json!({"taskId":task["id"]}))["events"]
        .as_array()
        .unwrap()
        .iter()
        .map(|event| event["data"].clone())
        .collect()
}
fn list_artifacts(peer: &mut Peer, task: &Value) -> Vec<Value> {
    peer.call("artifacts/list", json!({"taskId":task["id"]}))
        .as_array()
        .unwrap()
        .clone()
}

#[test]
fn offline_skill_discovers_reviews_executes_and_delivers_once() {
    let directory = SkillDirectory::new();
    let fixture = model();
    let base = fixture.url("/v1/");
    let mut peer = directory.peer(&base);
    let session = session(&mut peer, &directory);
    let discovery = start(
        &mut peer,
        &session,
        "discover",
        "discovery",
        "waiting_approval",
    );
    assert_eq!(discovery["pendingApproval"]["call"]["name"], "skills_list");
    respond(&mut peer, &discovery, "allow_once");
    let discovered = peer.wait_for_status(discovery["id"].as_str().unwrap(), "completed");
    assert!(
        discovered["messages"]
            .to_string()
            .contains("text-transform")
    );
    assert!(list_artifacts(&mut peer, &discovery).is_empty());

    let task = start(
        &mut peer,
        &session,
        "hello 中文",
        "transform",
        "waiting_approval",
    );
    let lookup = json!({"taskId":task["id"],"approvalId":task["pendingApproval"]["id"]});
    let plan = task["pendingApproval"]["executionPlan"].clone();
    assert_eq!(
        peer.call("approval/get", lookup.clone())["approval"]["executionPlan"],
        plan
    );
    assert_eq!(plan["kind"], "offline_skill");
    assert_eq!(plan["scriptSha256"], directory.digest());
    assert_eq!(
        plan["args"],
        json!(["skill-script", "scripts/transform.txt"])
    );
    assert_eq!(plan["network"], "no_network_grant");
    assert_eq!(plan["input"]["bytes"], "hello 中文".len());
    assert_eq!(plan["output"]["maxBytes"], 1024);
    assert!(list_artifacts(&mut peer, &task).is_empty());
    drop(peer);

    let mut peer = directory.peer(&base);
    assert_eq!(
        peer.call("approval/get", lookup)["approval"]["executionPlan"],
        plan
    );
    respond(&mut peer, &task, "allow_once");
    let completed = peer.wait_for_status(task["id"].as_str().unwrap(), "completed");
    assert!(!completed["messages"].to_string().contains("HELLO 中文"));
    let artifacts = list_artifacts(&mut peer, &task);
    assert_eq!(artifacts.len(), 1);
    let artifact = peer.call(
        "artifacts/get",
        json!({"taskId":task["id"],"artifactId":artifacts[0]["id"]}),
    );
    assert_eq!(artifact["content"], "HELLO 中文");
    assert_eq!(artifact["title"], "Processed text");
    drop(peer);
    let mut peer = directory.peer(&base);
    let retried = start(&mut peer, &session, "hello 中文", "transform", "completed");
    assert_eq!(retried["id"], task["id"]);
    assert_eq!(list_artifacts(&mut peer, &task), artifacts);
    assert_eq!(
        events(&mut peer, &task)
            .iter()
            .filter(|event| event["type"] == "tool_started")
            .count(),
        1
    );
}

#[test]
fn offline_skill_denies_overrides_tampering_and_truncated_delivery() {
    let directory = SkillDirectory::new();
    let fixture = model();
    let base = fixture.url("/v1/");
    let mut peer = directory.peer(&base);
    let session = session(&mut peer, &directory);
    let invalid = start(&mut peer, &session, "override", "override", "failed");
    assert!(invalid["pendingApproval"].is_null());
    let denied = start(&mut peer, &session, "private", "deny", "waiting_approval");
    respond(&mut peer, &denied, "deny");
    assert!(list_artifacts(&mut peer, &denied).is_empty());
    assert!(
        !events(&mut peer, &denied)
            .iter()
            .any(|event| event["type"] == "tool_started")
    );
    let flood = start(&mut peer, &session, "flood", "flood", "waiting_approval");
    respond(&mut peer, &flood, "allow_once");
    let flooded = peer.wait_for_status(flood["id"].as_str().unwrap(), "completed");
    assert!(
        flooded["messages"]
            .to_string()
            .contains("stdout must be complete UTF-8")
    );
    assert!(list_artifacts(&mut peer, &flood).is_empty());
    let tampered = start(&mut peer, &session, "hello", "tampered", "waiting_approval");
    let old_pin = tampered["pendingApproval"]["executionPlan"]["scriptSha256"].clone();
    fs::write(directory.script(), "reverse\n").unwrap();
    respond(&mut peer, &tampered, "allow_once");
    peer.wait_for_status(tampered["id"].as_str().unwrap(), "failed");
    assert!(list_artifacts(&mut peer, &tampered).is_empty());
    let pending = start(
        &mut peer,
        &session,
        "hello",
        "old-approval",
        "waiting_approval",
    );
    drop(peer);
    directory.configure(15_000);
    let mut peer = directory.peer(&base);
    respond(&mut peer, &pending, "allow_once");
    let rejected = peer.wait_for_status(pending["id"].as_str().unwrap(), "failed");
    assert_eq!(rejected["output"], "approved tool is no longer registered");
    assert!(
        !events(&mut peer, &pending)
            .iter()
            .any(|event| event["type"] == "tool_started")
    );
    assert!(list_artifacts(&mut peer, &pending).is_empty());
    let updated = start(&mut peer, &session, "hello", "updated", "waiting_approval");
    assert_ne!(
        updated["pendingApproval"]["executionPlan"]["scriptSha256"],
        old_pin
    );
    respond(&mut peer, &updated, "allow_once");
    peer.wait_for_status(updated["id"].as_str().unwrap(), "completed");
    let artifact_id = list_artifacts(&mut peer, &updated)[0]["id"].clone();
    assert_eq!(
        peer.call(
            "artifacts/get",
            json!({"taskId":updated["id"],"artifactId":artifact_id})
        )["content"],
        "olleh"
    );
}

#[test]
fn offline_skill_timeout_cancel_and_hard_restart_do_not_redeliver() {
    let directory = SkillDirectory::new();
    let fixture = model();
    let base = fixture.url("/v1/");
    directory.configure(2500);
    let mut peer = directory.peer(&base);
    let session = session(&mut peer, &directory);
    let timeout = start(&mut peer, &session, "hold", "timeout", "waiting_approval");
    respond(&mut peer, &timeout, "allow_once");
    peer.wait_for_status(timeout["id"].as_str().unwrap(), "interrupted");
    let timed = events(&mut peer, &timeout)
        .into_iter()
        .find(|event| event["type"] == "tool_outcome_unknown")
        .unwrap();
    assert_eq!(timed["output"]["process"]["termination"], "timed_out");
    assert!(
        timed["output"]["process"]["stdout"]["data"]
            .as_str()
            .unwrap()
            .contains("skill-ready")
    );
    assert_eq!(timed["output"]["artifact"]["status"], "not_produced");
    assert!(list_artifacts(&mut peer, &timeout).is_empty());

    drop(peer);
    directory.configure(15_000);
    let mut peer = directory.peer(&base);
    let cancel_token = uuid::Uuid::new_v4();
    let cancel = start(
        &mut peer,
        &session,
        &format!("hold:{cancel_token}"),
        "cancel",
        "waiting_approval",
    );
    respond(&mut peer, &cancel, "allow_once");
    directory.wait_until_running(cancel_token);
    peer.call("task/cancel", json!({"taskId":cancel["id"]}));
    peer.wait_for_status(cancel["id"].as_str().unwrap(), "cancelled");
    let cancelled = peer.wait_for_event(cancel["id"].as_str().unwrap(), "tool_outcome_unknown");
    let output = &cancelled["params"]["event"]["data"]["output"];
    assert_eq!(output["process"]["termination"], "cancelled");
    assert_eq!(output["process"]["stdout"]["data"], "skill-ready\n");
    assert!(list_artifacts(&mut peer, &cancel).is_empty());

    let crash_token = uuid::Uuid::new_v4();
    let crash_input = format!("hold:{crash_token}");
    let crash = start(
        &mut peer,
        &session,
        &crash_input,
        "crash",
        "waiting_approval",
    );
    respond(&mut peer, &crash, "allow_once");
    directory.wait_until_running(crash_token);
    let before = events(&mut peer, &crash);
    assert_eq!(
        before
            .iter()
            .filter(|ev| ev["type"] == "tool_started")
            .count(),
        1
    );
    drop(peer);
    let mut peer = directory.peer(&base);
    let recovered = start(&mut peer, &session, &crash_input, "crash", "interrupted");
    assert_eq!(recovered["id"], crash["id"]);
    assert!(list_artifacts(&mut peer, &crash).is_empty());
    assert_eq!(
        events(&mut peer, &crash)
            .iter()
            .filter(|ev| ev["type"] == "tool_started")
            .count(),
        1
    );
}
