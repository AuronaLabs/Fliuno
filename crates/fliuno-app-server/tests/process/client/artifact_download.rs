use super::*;
use fliuno_protocol::MAX_ARTIFACT_READ_BYTES;
use std::io;

struct ReadProbe {
    stdin: ChildStdin,
    remaining: Option<usize>,
    offsets: Vec<u64>,
}

impl Write for ReadProbe {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let request: Value = serde_json::from_slice(bytes).unwrap();
        if request["method"] == "artifacts/read" {
            if let Some(remaining) = &mut self.remaining {
                if *remaining == 0 {
                    return Err(io::ErrorKind::BrokenPipe.into());
                }
                *remaining -= 1;
            }
            self.offsets
                .push(request["params"]["after"].as_u64().unwrap());
        }
        self.stdin.write_all(bytes)?;
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.stdin.flush()
    }
}

#[test]
fn artifact_download_resumes_a_staging_file_after_server_restart() {
    let root =
        std::env::temp_dir().join(format!("fliuno-download-resume-{}", uuid::Uuid::new_v4()));
    fs::create_dir(&root).unwrap();
    let db = root.join("kernel.db");
    let mut peer = Peer::start(&db);
    let workspace = peer.call("workspace/open", json!({"root":root}));
    let session = peer.call("session/create", json!({"workspaceId":workspace["id"]}));
    let content = "中🌿\0\n".repeat(24_000);
    let task = peer.call(
        "task/startSummary",
        json!({"sessionId":session["id"],"input":content}),
    );
    let id = task["id"].as_str().unwrap();
    let waiting = peer.wait_for_status(id, "waiting_approval");
    peer.call(
        "approval/respond",
        json!({"taskId":id,"approvalId":waiting["pendingApproval"]["id"],"decision":"allow_once"}),
    );
    peer.wait_for_status(id, "completed");
    let summaries = peer.call("artifacts/listSummaries", json!({"taskId":id}));
    let artifact = ArtifactGetParams {
        task_id: id.into(),
        artifact_id: summaries["items"][0]["id"].as_str().unwrap().into(),
    };
    drop(peer);

    let (server, mut client) = super::ag_ui_run::connect_with_writer(&db, |stdin| ReadProbe {
        stdin,
        remaining: Some(1),
        offsets: Vec::new(),
    });
    let expected = client.artifact_digest(&artifact, |_| {}).unwrap();
    assert_eq!(expected.content_bytes, content.len() as u64);
    let path = root.join("staging.part");
    let mut file = fs::File::create_new(&path).unwrap();
    assert!(
        matches!(client.download_artifact(&artifact, &expected, &mut file, |_| {}),
        Err(Error::Io(e)) if e.kind() == io::ErrorKind::BrokenPipe)
    );
    let (_, probe) = client.into_parts();
    assert_eq!(probe.offsets, [0]);
    assert_eq!(
        file.metadata().unwrap().len(),
        u64::from(MAX_ARTIFACT_READ_BYTES)
    );
    // A partial last write is still resumable, even inside a UTF-8 character.
    let kept = u64::from(MAX_ARTIFACT_READ_BYTES) - 2;
    assert!(!content.is_char_boundary(kept as usize));
    file.set_len(kept).unwrap();
    file.sync_all().unwrap();
    drop(file);
    drop(server);
    drop(probe);

    let (server, mut client) = super::ag_ui_run::connect_with_writer(&db, |stdin| ReadProbe {
        stdin,
        remaining: None,
        offsets: Vec::new(),
    });
    let mut file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&path)
        .unwrap();
    client
        .resume_artifact_download(&artifact, &expected, &mut file, |_| {})
        .unwrap();
    // The already complete staging file is checked without a fresh request.
    client
        .resume_artifact_download(&artifact, &expected, &mut file, |_| {})
        .unwrap();
    let (_, probe) = client.into_parts();
    let stride = u64::from(MAX_ARTIFACT_READ_BYTES);
    assert_eq!(probe.offsets, [kept, kept + stride, kept + 2 * stride]);
    assert_eq!(fs::read(&path).unwrap(), content.as_bytes());
    drop(file);
    drop(probe);
    drop(server);
    fs::remove_dir_all(root).unwrap();
}
