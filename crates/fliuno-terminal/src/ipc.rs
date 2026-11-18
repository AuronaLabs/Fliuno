//! Bounded calls or an authenticated output stream. Timeouts are observation failures,
//! never permission to replay a mutating operation or spawn another worker.
use crate::protocol::*;
use interprocess::local_socket::{
    GenericFilePath, GenericNamespaced, Name,
    tokio::{Stream, prelude::*},
};
use serde_json::Value;
use std::{io, path::Path, time::Duration};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

pub(crate) fn name(endpoint: &str) -> io::Result<Name<'_>> {
    if endpoint.starts_with("fliuno-") {
        endpoint.to_ns_name::<GenericNamespaced>()
    } else {
        endpoint.to_fs_name::<GenericFilePath>()
    }
}
pub(crate) fn endpoint(dir: &Path) -> Result<String, String> {
    if GenericNamespaced::is_supported() {
        Ok(format!("fliuno-terminal-{}", uuid::Uuid::new_v4().simple()))
    } else {
        dir.join("ipc.sock")
            .into_os_string()
            .into_string()
            .map_err(|_| "terminal path must be UTF-8".into())
    }
}
// Length-prefixed frames avoid EOF/flush deadlocks on Windows named pipes.
pub(crate) async fn receive(
    reader: &mut (impl tokio::io::AsyncRead + Unpin),
) -> io::Result<Vec<u8>> {
    let length = reader.read_u32().await? as usize;
    if length > FRAME_LIMIT {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "terminal IPC frame too large",
        ));
    }
    let mut bytes = vec![0; length];
    reader.read_exact(&mut bytes).await?;
    Ok(bytes)
}
pub(crate) async fn send(
    writer: &mut (impl tokio::io::AsyncWrite + Unpin),
    value: &impl serde::Serialize,
) -> io::Result<()> {
    let data = serde_json::to_vec(value)?;
    if data.len() > FRAME_LIMIT {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "terminal IPC frame too large",
        ));
    }
    writer.write_u32(data.len() as u32).await?;
    writer.write_all(&data).await?;
    writer.flush().await
}
pub(crate) async fn call(
    d: &Descriptor,
    session_id: &str,
    operation: Operation,
) -> Result<Value, String> {
    tokio::time::timeout(Duration::from_secs(4), async {
        if d.version != 1 {
            return Err("unsupported terminal IPC version".into());
        }
        let stream = Stream::connect(name(&d.endpoint).map_err(|_| "invalid terminal endpoint")?)
            .await
            .map_err(|_| "terminal supervisor unavailable")?;
        let request = Request {
            version: 1,
            token: d.token.clone(),
            session_id: session_id.into(),
            operation,
        };
        send(&mut &stream, &request)
            .await
            .map_err(|_| "terminal IPC send failed; outcome may be unknown")?;
        let bytes = receive(&mut &stream)
            .await
            .map_err(|_| "terminal IPC response lost; outcome may be unknown")?;
        let response: Response =
            serde_json::from_slice(&bytes).map_err(|_| "invalid terminal IPC response")?;
        if let Some(error) = response.error {
            Err(error)
        } else {
            response
                .result
                .ok_or_else(|| "missing terminal IPC result".into())
        }
    })
    .await
    .map_err(|_| "terminal IPC timed out; outcome may be unknown".to_string())?
}

pub(crate) fn descriptor(dir: &Path) -> Result<Descriptor, String> {
    use std::io::Read;
    let file = std::fs::File::open(dir.join("endpoint.json"))
        .map_err(|_| "terminal endpoint unavailable")?;
    let mut bytes = Vec::new();
    file.take(4097)
        .read_to_end(&mut bytes)
        .map_err(|_| "cannot read terminal endpoint")?;
    if bytes.len() > 4096 {
        return Err("invalid terminal endpoint".into());
    }
    serde_json::from_slice(&bytes).map_err(|_| "invalid terminal endpoint".into())
}

pub(crate) async fn serve_connection(
    stream: Stream,
    token: &str,
    tx: tokio::sync::mpsc::Sender<crate::worker::Command>,
    changes: tokio::sync::watch::Receiver<()>,
    subscribers: std::sync::Arc<tokio::sync::Semaphore>,
) {
    let request = tokio::time::timeout(Duration::from_secs(5), async {
        let bytes = receive(&mut &stream).await?;
        let request: Request = serde_json::from_slice(&bytes)?;
        if request.version != 1 || request.token != token {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "invalid terminal capability",
            ));
        }
        Ok(request)
    })
    .await;
    let Ok(Ok(request)) = request else { return };
    if let Operation::Subscribe { after, limit } = request.operation {
        let Ok(_permit) = subscribers.try_acquire_owned() else {
            let _ = tokio::time::timeout(
                Duration::from_secs(5),
                send(
                    &mut &stream,
                    &Response {
                        result: None,
                        error: Some("terminal supervisor subscription limit reached".into()),
                    },
                ),
            )
            .await;
            return;
        };
        crate::subscription::serve(&stream, &request.session_id, after, limit, tx, changes).await;
        return;
    }
    let _ = tokio::time::timeout(Duration::from_secs(5), async {
        let (reply, response) = tokio::sync::oneshot::channel();
        tx.try_send(crate::worker::Command {
            session_id: request.session_id,
            operation: request.operation,
            reply,
        })
        .map_err(|_| io::Error::other("terminal request queue full"))?;
        let result = response
            .await
            .map_err(|_| io::Error::other("terminal stopped"))?;
        let response = match result {
            Ok(value) => Response {
                result: Some(value),
                error: None,
            },
            Err(message) => Response {
                result: None,
                error: Some(message),
            },
        };
        send(&mut &stream, &response).await
    })
    .await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use interprocess::local_socket::ListenerOptions;
    use tokio::sync::mpsc;
    #[tokio::test]
    async fn auth_version_and_frame_length_are_checked_before_actor_delivery() {
        for (version, token, operation) in [
            (1, "wrong", Operation::Status),
            (99, "correct", Operation::Status),
            (1, "wrong", Operation::Subscribe { after: 0, limit: 1 }),
            (99, "correct", Operation::Subscribe { after: 0, limit: 1 }),
        ] {
            let directory =
                std::env::temp_dir().join(format!("fliuno-ipc-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir(&directory).unwrap();
            let endpoint = endpoint(&directory).unwrap();
            let listener = ListenerOptions::new()
                .name(name(&endpoint).unwrap())
                .create_tokio()
                .unwrap();
            let (tx, mut rx) = mpsc::channel(1);
            let server = tokio::spawn(async move {
                let c = listener.accept().await.unwrap();
                serve_connection(
                    c,
                    "correct",
                    tx,
                    tokio::sync::watch::channel(()).1,
                    std::sync::Arc::new(tokio::sync::Semaphore::new(8)),
                )
                .await;
            });
            let c = Stream::connect(name(&endpoint).unwrap()).await.unwrap();
            send(
                &mut &c,
                &Request {
                    version,
                    token: token.into(),
                    session_id: "s".into(),
                    operation,
                },
            )
            .await
            .unwrap();
            assert!(
                tokio::time::timeout(Duration::from_secs(2), receive(&mut &c))
                    .await
                    .unwrap()
                    .is_err()
            );
            drop(c);
            server.await.unwrap();
            assert!(rx.try_recv().is_err());
            std::fs::remove_dir_all(directory).unwrap();
        }
    }
    #[tokio::test]
    async fn oversized_prefix_is_rejected_without_waiting_for_the_payload() {
        let bytes = (FRAME_LIMIT as u32 + 1).to_be_bytes();
        let mut input = &bytes[..];
        let error = receive(&mut input).await.unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        let bytes = 2u32.to_be_bytes();
        let mut input = &bytes[..];
        assert_eq!(
            receive(&mut input).await.unwrap_err().kind(),
            io::ErrorKind::UnexpectedEof
        );
    }
}
