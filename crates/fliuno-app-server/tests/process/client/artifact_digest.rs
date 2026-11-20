use super::*;
use fliuno_protocol::{
    ArtifactDigestBatchParams, ArtifactDigestEntry, MAX_ARTIFACT_DIGEST_BATCH,
    MAX_ARTIFACT_DIGEST_BYTES, MAX_ARTIFACT_DIGEST_ID_BYTES, MAX_ARTIFACT_DOWNLOAD_BATCH,
};

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
    let wrong_task = ArtifactGetParams {
        task_id: old.task_id.clone(),
        artifact_id: new.artifact_id.clone(),
    };
    let missing = ArtifactGetParams {
        task_id: old.task_id.clone(),
        artifact_id: "absent-artifact".into(),
    };
    let oversized = ArtifactGetParams {
        task_id: new.task_id.clone(),
        artifact_id: "oversized-fixture".into(),
    };
    let ready = json!({"status":"ready","digest":expected});
    let lookup = json!({"artifacts":[old, wrong_task, new, missing, new, oversized]});
    let expected_batch = json!({"items":[
        {"status":"unrecorded","contentBytes":content.len()},
        {"status":"missing"}, ready, {"status":"missing"}, ready,
        {"status":"unrecorded","contentBytes":MAX_ARTIFACT_DIGEST_BYTES+1},
    ]});
    for _ in 0..2 {
        assert_eq!(
            peer.call("artifacts/getDigests", lookup.clone()),
            expected_batch
        );
    }
    assert_eq!(peer.call("events/getWatermark", json!({})), pinned);
    let max = vec![&new; MAX_ARTIFACT_DIGEST_BATCH];
    assert_eq!(
        peer.call("artifacts/getDigests", json!({"artifacts":max}))["items"],
        json!(vec![&ready; max.len()])
    );
    for invalid in [
        json!({"artifacts":[]}),
        json!({"artifacts":vec![&new; MAX_ARTIFACT_DIGEST_BATCH+1]}),
        json!({"artifacts":[new,{"taskId":"","artifactId":new.artifact_id}]}),
        json!({"artifacts":[{"taskId":new.task_id,"artifactId":"x".repeat(MAX_ARTIFACT_DIGEST_ID_BYTES+1)}]}),
        json!({"artifacts":[{"taskId":new.task_id,"artifactId":"invalid id"}]}),
    ] {
        assert_eq!(
            peer.request("artifacts/getDigests", invalid)["error"]["code"],
            -32602
        );
    }
    let prefix = peer.call(
        "artifacts/read",
        json!({"taskId":new.task_id,"artifactId":new.artifact_id,"limit":17}),
    );
    let available = json!({"status":"ready","download":{"digest":expected,"firstChunk":prefix}});
    assert_eq!(
        peer.call(
            "artifacts/openDownloads",
            json!({"artifacts":lookup["artifacts"],"limit":17})
        ),
        json!({"items":[
            {"status":"unrecorded","contentBytes":content.len()},
            {"status":"missing"},available,{"status":"missing"},available,
            {"status":"unrecorded","contentBytes":MAX_ARTIFACT_DIGEST_BYTES+1},
        ]})
    );
    let max = vec![&new; MAX_ARTIFACT_DOWNLOAD_BATCH];
    let batch = peer.call("artifacts/openDownloads", json!({"artifacts":max}));
    let entries = batch["items"].as_array().unwrap();
    assert_eq!(entries.len(), MAX_ARTIFACT_DOWNLOAD_BATCH);
    assert!(entries.iter().all(|item| item == &entries[0]));
    assert_eq!(
        entries[0]["download"]["firstChunk"]["nextOffset"],
        16 * 1024
    );
    for invalid in [
        json!({"artifacts":[]}),
        json!({"artifacts":vec![&missing;MAX_ARTIFACT_DOWNLOAD_BATCH+1]}),
        json!({"artifacts":max,"limit":16*1024+1}),
        json!({"artifacts":[new],"limit":0}),
        json!({"artifacts":[new],"limit":65537}),
        json!({"artifacts":[{"taskId":new.task_id,"artifactId":"bad id"}]}),
    ] {
        assert_eq!(
            peer.request("artifacts/openDownloads", invalid)["error"]["code"],
            -32602
        );
    }
    assert_eq!(peer.call("events/getWatermark", json!({})), pinned);
    assert_eq!(
        peer.call("artifacts/getDigests", json!({"artifacts":[old]}))["items"][0]["status"],
        "unrecorded"
    );
    let old_key = serde_json::to_value(&old).unwrap();
    let opened = peer.call("artifacts/openDownload", old_key.clone());
    assert_eq!(opened["digest"], expected);
    let mut read = old_key.clone();
    read["limit"] = json!(fliuno_protocol::MAX_ARTIFACT_READ_BYTES);
    assert_eq!(opened["firstChunk"], peer.call("artifacts/read", read));
    assert_eq!(peer.call("artifacts/digest", old_key), expected);
    assert_eq!(peer.call("events/getWatermark", json!({})), pinned);
    assert_eq!(
        peer.request(
            "artifacts/digest",
            json!({"taskId":new.task_id,"artifactId":"oversized-fixture"})
        )["error"]["code"],
        -32602
    );
    for (artifact, code) in [
        (&wrong_task, -32004),
        (&missing, -32004),
        (&oversized, -32602),
    ] {
        assert_eq!(
            peer.request("artifacts/openDownload", json!(artifact))["error"]["code"],
            code
        );
    }
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
    assert_eq!(client.info().capabilities["artifactDigestBatch"], true);
    assert_eq!(
        client.info().capabilities["maxArtifactDigestBatch"],
        MAX_ARTIFACT_DIGEST_BATCH
    );
    let request = ArtifactDigestBatchParams {
        artifacts: vec![old.clone(), new.clone()],
    };
    let batch = client.artifact_digests(&request, |_| {}).unwrap();
    for (artifact, item) in request.artifacts.iter().zip(batch.items) {
        let ArtifactDigestEntry::Ready { digest } = item else {
            panic!("expected stored digest")
        };
        assert_eq!(serde_json::to_value(&digest).unwrap(), expected);
        let mut destination = Vec::new();
        assert!(matches!(
            client.download_artifact(artifact, &digest, &mut destination, |_| {}),
            Err(Error::ArtifactDigestMismatch)
        ));
        assert_eq!(destination.len(), content.len());
    }
    assert!(matches!(
        client.artifact_digest(&wrong_task, |_| {}),
        Err(Error::Rpc { code: -32004, .. })
    ));
    drop(client);
    drop(server);
    fs::remove_dir_all(root).unwrap();
}
