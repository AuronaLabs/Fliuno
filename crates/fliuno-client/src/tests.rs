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
