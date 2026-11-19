use super::*;
use serde_json::json;
use std::io::Cursor;

const HANDSHAKE: &str = "{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"protocolVersion\":\"0.1\",\"serverName\":\"test\",\"serverVersion\":\"0\",\"capabilities\":{}}}\n";

fn fake_client(additional: &[u8]) -> Client<Cursor<Vec<u8>>, Vec<u8>> {
    let mut bytes = HANDSHAKE.as_bytes().to_vec();
    bytes.extend_from_slice(additional);
    Client::connect(Cursor::new(bytes), Vec::new(), "test", |_| {}).unwrap()
}

#[test]
fn local_request_limit_does_not_write_or_consume_a_correlation_id() {
    let mut client = fake_client(b"{\"jsonrpc\":\"2.0\",\"id\":2,\"result\":true}\n");
    let large = json!({"payload":"x".repeat(fliuno_protocol::MAX_APP_REQUEST_BYTES)});
    let rejected: Result<bool, _> = client.call("probe", &large, |_| {});
    assert!(matches!(rejected, Err(Error::RequestTooLarge)));
    assert!(client.call::<_, bool>("probe", &json!({}), |_| {}).unwrap());
    let (_, written) = client.into_parts();
    let sent: Vec<Value> = written
        .split(|b| *b == b'\n')
        .filter(|line| !line.is_empty())
        .map(|line| serde_json::from_slice(line).unwrap())
        .collect();
    assert_eq!(sent.len(), 2);
    assert_eq!(sent[1]["id"], 2);
}

#[test]
fn incorrect_correlation_retires_the_connection_without_a_second_send() {
    let mut client = fake_client(b"{\"jsonrpc\":\"2.0\",\"id\":3,\"result\":true}\n");
    let mismatched: Result<bool, _> = client.call("probe", &json!({}), |_| {});
    assert!(matches!(mismatched, Err(Error::InvalidFrame(_))));
    assert!(matches!(
        client.call::<_, bool>("probe", &json!({}), |_| {}),
        Err(Error::Closed)
    ));
    let (_, written) = client.into_parts();
    assert_eq!(written.iter().filter(|b| **b == b'\n').count(), 2);
}

#[test]
fn oversize_frame_retires_the_connection_without_draining_more_bytes() {
    let oversized = vec![b'x'; DEFAULT_RESPONSE_BYTES + 1];
    let mut client = fake_client(&oversized);
    assert!(matches!(
        client.read_notification(),
        Err(Error::ResponseTooLarge)
    ));
    assert!(matches!(client.read_notification(), Err(Error::Closed)));
    let (reader, _) = client.into_parts();
    assert_eq!(reader.position(), HANDSHAKE.len() as u64);
}

#[test]
fn replay_rejects_an_inconsistent_page_and_advances_across_an_empty_scoped_tail() {
    let mut client = fake_client(concat!(
        "{\"jsonrpc\":\"2.0\",\"id\":2,\"result\":{\"events\":[],\"nextAfter\":4,\"hasMore\":true}}\n",
        "{\"jsonrpc\":\"2.0\",\"id\":3,\"result\":{\"events\":[],\"nextAfter\":4,\"hasMore\":false}}\n",
    ).as_bytes());
    let mut replay = EventReplay::new(ReplayScope::Workspace("workspace".into()), 4, 10).unwrap();
    assert!(matches!(
        replay.next_page(&mut client, 1, None, |_| {}),
        Err(Error::InvalidReplayPage)
    ));
    assert_eq!(replay.cursor(), 4);
    assert!(
        replay
            .next_page(&mut client, 1, None, |_| {})
            .unwrap()
            .is_none()
    );
    assert_eq!(replay.cursor(), 10);
    assert!(
        replay
            .next_page(&mut client, 1, None, |_| {})
            .unwrap()
            .is_none()
    );
    let (_, written) = client.into_parts();
    let sent: Vec<Value> = written
        .split(|b| *b == b'\n')
        .filter(|line| !line.is_empty())
        .map(|line| serde_json::from_slice(line).unwrap())
        .collect();
    assert_eq!(sent.len(), 3);
    assert_eq!(sent[1]["method"], "events/listWorkspace");
    assert_eq!(sent[1]["params"], sent[2]["params"]);
    assert_eq!(sent[2]["params"]["through"], 10);
}

#[test]
fn borrowed_envelopes_preserve_null_results_and_reject_conflicting_fields() {
    let mut client = fake_client(
        concat!(
            "{\"jsonrpc\":\"2.0\",\"method\":\"test/notice\",\"params\":null}\n",
            "{\"jsonrpc\":\"2.0\",\"id\":2,\"result\":null}\n",
            "{\"jsonrpc\":\"2.0\",\"id\":3,\"result\":null,\"error\":null}\n",
        )
        .as_bytes(),
    );
    let notification = client.read_notification().unwrap();
    assert_eq!(notification.method, "test/notice");
    assert_eq!(notification.params, Value::Null);
    let result: Option<u32> = client.call("probe", &json!({}), |_| {}).unwrap();
    assert_eq!(result, None);
    assert!(matches!(
        client.call::<_, Value>("probe", &json!({}), |_| {}),
        Err(Error::InvalidFrame(_))
    ));
    assert!(matches!(client.read_notification(), Err(Error::Closed)));
}

#[test]
fn artifact_download_rejects_malformed_ranges_before_writing_and_keeps_connection() {
    use fliuno_protocol::{ArtifactDigest, ArtifactGetParams};
    let first = json!({"offset":0,"nextOffset":2,"contentBytes":2,"dataBase64":"AAE="});
    for (field, invalid) in [
        ("offset", json!(1)),
        ("nextOffset", json!(0)),
        ("contentBytes", json!(3)),
        ("dataBase64", json!("!!!!")),
        ("dataBase64", json!("AA==")),
    ] {
        let mut chunk = first.clone();
        chunk[field] = invalid;
        let frames = format!(
            "{}\n{{\"jsonrpc\":\"2.0\",\"id\":3,\"result\":true}}\n",
            json!({"jsonrpc":"2.0","id":2,"result":chunk})
        );
        let mut client = fake_client(frames.as_bytes());
        let key = ArtifactGetParams {
            task_id: "t".into(),
            artifact_id: "a".into(),
        };
        let digest = ArtifactDigest {
            content_bytes: 2,
            sha256: "0".repeat(64),
        };
        let mut output = Vec::new();
        assert!(
            matches!(
                client.download_artifact(&key, &digest, &mut output, |_| {}),
                Err(Error::InvalidArtifactChunk)
            ),
            "{field}"
        );
        assert!(output.is_empty());
        assert!(client.call::<_, bool>("probe", &json!({}), |_| {}).unwrap());
    }
}

#[test]
fn artifact_download_checks_empty_body_against_pinned_digest() {
    use fliuno_protocol::{ArtifactDigest, ArtifactGetParams};
    let mut client = fake_client(concat!(
        "{\"jsonrpc\":\"2.0\",\"id\":2,\"result\":{\"offset\":0,\"nextOffset\":0,\"contentBytes\":0,\"dataBase64\":\"\"}}\n",
        "{\"jsonrpc\":\"2.0\",\"id\":3,\"result\":{\"offset\":0,\"nextOffset\":0,\"contentBytes\":0,\"dataBase64\":\"\"}}\n"
    ).as_bytes());
    let key = ArtifactGetParams {
        task_id: "t".into(),
        artifact_id: "a".into(),
    };
    let mut digest = ArtifactDigest {
        content_bytes: 0,
        sha256: "not-a-hash".into(),
    };
    let mut output = Vec::new();
    assert!(matches!(
        client.download_artifact(&key, &digest, &mut output, |_| {}),
        Err(Error::InvalidArtifactDigest)
    ));
    digest.sha256 = "0".repeat(64);
    assert!(matches!(
        client.download_artifact(&key, &digest, &mut output, |_| {}),
        Err(Error::ArtifactDigestMismatch)
    ));
    digest.sha256 = "E3B0C44298FC1C149AFBF4C8996FB92427AE41E4649B934CA495991B7852B855".into();
    client
        .download_artifact(&key, &digest, &mut output, |_| {})
        .unwrap();
    assert!(output.is_empty());
}
