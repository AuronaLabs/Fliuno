use super::*;
use fliuno_protocol::MAX_ARTIFACT_DIGEST_BYTES;

#[test]
fn stored_digests_cover_both_new_and_legacy_artifact_bytes_across_restart() {
    let root =
        std::env::temp_dir().join(format!("fliuno-artifact-digest-{}", uuid::Uuid::new_v4()));
    fs::create_dir(&root).unwrap();
    let db = root.join("kernel.db");
    let content = "交付物🌿\0\n".repeat(7000);
    let sha = "a86cf7a0c4ce34584acd568c7d70d8440b6b396f0e3156ae5caaec2baf067e29";
    let expected = json!({"contentBytes":content.len(),"sha256":sha});
    let mut peer = Peer::start(&db);
    let workspace = peer.call("workspace/open", json!({"root":root}));
    let session = peer.call("session/create", json!({"workspaceId":workspace["id"]}));
    let create = |peer: &mut Peer| -> ArtifactGetParams {
        let task = peer.call(
            "task/startSummary",
            json!({"sessionId":session["id"],"input":content}),
        );
        let id = task["id"].as_str().unwrap();
        let waiting = peer.wait_for_status(id, "waiting_approval");
        peer.call("approval/respond", json!({"taskId":id,"approvalId":waiting["pendingApproval"]["id"],"decision":"allow_once"}));
        peer.wait_for_status(id, "completed");
        let summary = peer.call("artifacts/listSummaries", json!({"taskId":id}));
        assert_eq!(summary["items"].as_array().unwrap().len(), 1);
        assert!(summary["items"][0].get("sha256").is_none());
        ArtifactGetParams {
            task_id: id.into(),
            artifact_id: summary["items"][0]["id"].as_str().unwrap().into(),
        }
    };
    let new = create(&mut peer);
    let old = create(&mut peer);
    drop(peer);

    let stored = |conn: &rusqlite::Connection, artifact: &ArtifactGetParams| -> Option<String> {
        conn.query_row(
            "SELECT json_extract(data,'$.sha256') FROM artifacts WHERE id=?1",
            [&artifact.artifact_id],
            |row| row.get(0),
        )
        .unwrap()
    };
    {
        let conn = rusqlite::Connection::open(&db).unwrap();
        assert_eq!(stored(&conn, &new).as_deref(), Some(sha));
        // Simulate an older writer that stored compact metadata without a hash.
        conn.execute(
            "UPDATE artifacts SET data=json_remove(data,'$.sha256') WHERE id=?1",
            [&old.artifact_id],
        )
        .unwrap();
        assert_eq!(stored(&conn, &old), None);
        let large = (MAX_ARTIFACT_DIGEST_BYTES + 1) as i64;
        conn.execute("INSERT INTO artifacts(id,task_id,data) VALUES('oversized-fixture',?1,json_object('contentBytes',?2,'mediaType','application/octet-stream','title','fixture','binary',1))", rusqlite::params![new.task_id,large]).unwrap();
        conn.execute("INSERT INTO artifact_bodies(artifact_id,body) VALUES('oversized-fixture',zeroblob(?1))", [large]).unwrap();
    }

    let mut peer = Peer::start(&db);
    let pinned = peer.call("events/getWatermark", json!({}));
    let old_key = serde_json::to_value(&old).unwrap();
    assert_eq!(peer.call("artifacts/digest", old_key), expected);
    assert_eq!(peer.call("events/getWatermark", json!({})), pinned);
    assert_eq!(
        peer.request(
            "artifacts/digest",
            json!({"taskId":new.task_id,"artifactId":"oversized-fixture"})
        )["error"]["code"],
        -32602
    );
    drop(peer);

    {
        let conn = rusqlite::Connection::open(&db).unwrap();
        assert_eq!(stored(&conn, &old).as_deref(), Some(sha));
        // Simulate same-length damage to just the bodies while the app is stopped.
        for artifact in [&new, &old] {
            assert_eq!(
                conn.execute(
                    "UPDATE artifact_bodies SET body=zeroblob(length(body)) WHERE artifact_id=?1",
                    [&artifact.artifact_id]
                )
                .unwrap(),
                1
            );
        }
    }

    let (server, mut client) = super::ag_ui_run::connect(&db);
    assert_eq!(client.info().capabilities["storedArtifactDigests"], true);
    for artifact in [&new, &old] {
        let digest = client.artifact_digest(artifact, |_| {}).unwrap();
        assert_eq!(serde_json::to_value(&digest).unwrap(), expected);
        let mut destination = Vec::new();
        assert!(matches!(
            client.download_artifact(artifact, &digest, &mut destination, |_| {}),
            Err(Error::ArtifactDigestMismatch)
        ));
        assert_eq!(destination.len(), content.len());
    }
    let wrong_task = ArtifactGetParams {
        task_id: old.task_id,
        artifact_id: new.artifact_id,
    };
    assert!(matches!(
        client.artifact_digest(&wrong_task, |_| {}),
        Err(Error::Rpc { code: -32004, .. })
    ));
    drop(client);
    drop(server);
    fs::remove_dir_all(root).unwrap();
}
