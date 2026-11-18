//! Push committed spool pages over one authenticated socket. Watch wakeups are
//! coalesced hints; only the durable byte cursor determines what to send next.
use crate::{Service, ipc, protocol::*, worker::Command};
use fliuno_protocol::TerminalSubscriptionEndReason as EndReason;
use interprocess::local_socket::tokio::{Stream, prelude::*};
use std::time::Duration;
use tokio::{
    io::AsyncReadExt,
    sync::{mpsc, oneshot, watch},
};

pub struct Subscription {
    service: Service,
    session: String,
    terminal: String,
    limit: usize,
    cursor: u64,
    stream: Option<Stream>,
    initial: Option<Output>,
    ended: Option<EndReason>,
}

impl Service {
    /// Observe without gaining control of the PTY. Dropping the subscription
    /// closes only this socket, never the supervisor or the native process.
    pub async fn subscribe(
        &self,
        session: &str,
        terminal: &str,
        after: u64,
        limit: usize,
    ) -> Result<Subscription, String> {
        // Validate ownership, cursor and read budget before opening live IPC.
        let saved = self.offline(session, terminal, after, limit)?;
        let live = if saved.snapshot.archived_at_unix_ms.is_some() {
            None
        } else {
            match self
                .directory(terminal)
                .and_then(|directory| ipc::descriptor(&directory))
            {
                Ok(descriptor) => connect(&descriptor, session, after, limit).await?,
                Err(_) => None,
            }
        };
        let (stream, initial) = match live {
            Some((stream, output)) => (Some(stream), output),
            None => (None, self.offline(session, terminal, after, limit)?),
        };
        let subscription = Subscription {
            service: self.clone(),
            session: session.into(),
            terminal: terminal.into(),
            limit,
            cursor: after,
            stream,
            initial: Some(initial),
            ended: None,
        };
        subscription.validate(subscription.initial.as_ref().expect("initial output"))?;
        Ok(subscription)
    }
}

impl Subscription {
    pub fn end_reason(&self) -> Option<EndReason> {
        self.ended
    }

    /// Returns one bounded page or a changed snapshot. If the supervisor goes
    /// away, drain the last durable pages and report conservative offline state.
    pub async fn next(&mut self) -> Result<Option<Output>, String> {
        if self.ended.is_some() {
            return Ok(None);
        }
        let output = if let Some(output) = self.initial.take() {
            output
        } else if let Some(stream) = &self.stream {
            match ipc::receive(&mut &*stream).await {
                Ok(bytes) => decode(&bytes)?,
                Err(_) => {
                    self.stream = None;
                    self.service
                        .offline(&self.session, &self.terminal, self.cursor, self.limit)?
                }
            }
        } else {
            self.service
                .offline(&self.session, &self.terminal, self.cursor, self.limit)?
        };
        self.validate(&output)?;
        self.cursor = output.next;
        if output.next == output.snapshot.end_offset {
            if output.snapshot.archived_at_unix_ms.is_some() {
                self.ended = Some(EndReason::Archived);
                self.stream = None;
            } else if output.snapshot.output_closed
                && matches!(
                    output.snapshot.status,
                    Status::Exited
                        | Status::Killed
                        | Status::Expired
                        | Status::Released
                        | Status::Failed
                )
            {
                self.ended = Some(EndReason::Closed);
                self.stream = None;
            } else if self.stream.is_none() {
                self.ended = Some(EndReason::SupervisorUnavailable);
            }
        }
        Ok(Some(output))
    }

    fn validate(&self, output: &Output) -> Result<(), String> {
        use base64::Engine;
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(&output.data_base64)
            .map_err(|_| "invalid terminal stream bytes")?;
        let snapshot = &output.snapshot;
        if snapshot.session_id != self.session
            || snapshot.terminal_id != self.terminal
            || snapshot.start_offset > snapshot.end_offset
            || snapshot.end_offset > i64::MAX as u64
            || output.from != self.cursor.max(snapshot.start_offset)
            || output.gap != (self.cursor < snapshot.start_offset)
            || output.next < output.from
            || output.next > snapshot.end_offset
            || output.next - output.from != bytes.len() as u64
            || bytes.len() > self.limit
            || (bytes.is_empty() && output.next < snapshot.end_offset)
        {
            return Err("invalid terminal stream identity or cursor".into());
        }
        Ok(())
    }
}

fn decode(bytes: &[u8]) -> Result<Output, String> {
    let response: Response =
        serde_json::from_slice(bytes).map_err(|_| "invalid terminal stream response")?;
    if let Some(error) = response.error {
        return Err(error);
    }
    serde_json::from_value(response.result.ok_or("missing terminal stream output")?)
        .map_err(|_| "invalid terminal stream output".into())
}

async fn connect(
    d: &Descriptor,
    session: &str,
    after: u64,
    limit: usize,
) -> Result<Option<(Stream, Output)>, String> {
    if d.version != 1 {
        return Err("unsupported terminal IPC version".into());
    }
    let attempt = tokio::time::timeout(Duration::from_secs(4), async {
        let stream = Stream::connect(ipc::name(&d.endpoint)?).await?;
        ipc::send(
            &mut &stream,
            &Request {
                version: 1,
                token: d.token.clone(),
                session_id: session.into(),
                operation: Operation::Subscribe { after, limit },
            },
        )
        .await?;
        let bytes = ipc::receive(&mut &stream).await?;
        Ok::<_, std::io::Error>((stream, bytes))
    })
    .await;
    match attempt {
        Ok(Ok((stream, bytes))) => Ok(Some((stream, decode(&bytes)?))),
        _ => Ok(None),
    }
}

pub(crate) async fn serve(
    stream: &Stream,
    session: &str,
    mut after: u64,
    limit: usize,
    tx: mpsc::Sender<Command>,
    mut changes: watch::Receiver<()>,
) {
    let mut previous = None;
    loop {
        // Mark before the actor read. A commit racing the read stays pending;
        // it cannot disappear between replay and registration for live updates.
        changes.borrow_and_update();
        let result = tokio::time::timeout(Duration::from_secs(4), async {
            let (reply, response) = oneshot::channel();
            tx.try_send(Command {
                session_id: session.into(),
                operation: Operation::Output { after, limit },
                reply,
            })
            .map_err(|_| "terminal request queue full".to_string())?;
            let value = response
                .await
                .map_err(|_| "terminal stopped".to_string())??;
            serde_json::from_value::<Output>(value).map_err(|_| "invalid terminal output".into())
        })
        .await;
        let result = match result {
            Ok(value) => value,
            Err(_) => Err("terminal stream observation timed out".into()),
        };
        let output = match result {
            Ok(output) => output,
            Err(error) => {
                let _ = tokio::time::timeout(
                    Duration::from_secs(5),
                    ipc::send(
                        &mut &*stream,
                        &Response {
                            result: None,
                            error: Some(error),
                        },
                    ),
                )
                .await;
                return;
            }
        };
        let has_more = output.next < output.snapshot.end_offset;
        let changed =
            previous.as_ref() != Some(&output.snapshot) || output.next != after || output.gap;
        after = output.next;
        if changed {
            previous = Some(output.snapshot.clone());
            let response = Response {
                result: Some(serde_json::json!(output)),
                error: None,
            };
            if !matches!(
                tokio::time::timeout(Duration::from_secs(5), ipc::send(&mut &*stream, &response))
                    .await,
                Ok(Ok(()))
            ) {
                return;
            }
        }
        if has_more {
            continue;
        }
        // Client disconnects are observed even while an idle PTY has no output.
        // Any post-handshake input is invalid; this channel is observation-only.
        let mut byte = [0];
        let mut reader = stream;
        tokio::select! {
            result = changes.changed() => if result.is_err() { return },
            _ = reader.read(&mut byte) => return,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use interprocess::local_socket::ListenerOptions;
    use serde_json::json;

    fn output(from: u64, next: u64, end: u64, data: &str) -> serde_json::Value {
        json!({"from":from,"next":next,"gap":false,"dataBase64":data,"snapshot": {
            "terminalId":"t","sessionId":"s","presetId":"p","status":"running",
            "startupError":null,"supervisorConnected":true,"rows":24,"cols":80,
            "exitCode":null,"signal":null,"startOffset":0,"endOffset":end,
            "inputBytes":0,"outputClosed":false,"expiresAtUnixMs":1
        }})
    }

    #[tokio::test]
    async fn stream_replays_pages_catches_commit_during_read_and_does_not_poll_idle_pty() {
        let directory =
            std::env::temp_dir().join(format!("fliuno-stream-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&directory).unwrap();
        let endpoint = ipc::endpoint(&directory).unwrap();
        let listener = ListenerOptions::new()
            .name(ipc::name(&endpoint).unwrap())
            .create_tokio()
            .unwrap();
        let (tx, mut commands) = mpsc::channel(4);
        let (changes, receiver) = watch::channel(());
        let server = tokio::spawn(async move {
            let stream = listener.accept().await.unwrap();
            serve(&stream, "s", 0, 2, tx, receiver).await;
        });
        let client = Stream::connect(ipc::name(&endpoint).unwrap())
            .await
            .unwrap();
        let first = commands.recv().await.unwrap();
        assert!(matches!(
            first.operation,
            Operation::Output { after: 0, limit: 2 }
        ));
        // Bytes 0..3 were committed before this read. A third byte remains,
        // so replay must continue without requiring another output event.
        first.reply.send(Ok(output(0, 2, 3, "AP8="))).unwrap();
        let page = decode(&ipc::receive(&mut &client).await.unwrap()).unwrap();
        assert_eq!(page.data_base64, "AP8=");
        let second = commands.recv().await.unwrap();
        assert!(matches!(
            second.operation,
            Operation::Output { after: 2, limit: 2 }
        ));
        // Commit while the last backlog read is in flight. This notification
        // must remain pending across the switch from replay to live waiting.
        changes.send_replace(());
        second.reply.send(Ok(output(2, 3, 3, "5A=="))).unwrap();
        let page = decode(&ipc::receive(&mut &client).await.unwrap()).unwrap();
        assert_eq!(page.next, 3);
        let third = tokio::time::timeout(Duration::from_secs(1), commands.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(
            third.operation,
            Operation::Output { after: 3, limit: 2 }
        ));
        third.reply.send(Ok(output(3, 5, 5, "uK0="))).unwrap();
        assert_eq!(
            decode(&ipc::receive(&mut &client).await.unwrap())
                .unwrap()
                .next,
            5
        );
        // The split UTF-8 bytes e4 b8 ad are transported without decoding.
        assert!(
            tokio::time::timeout(Duration::from_millis(100), commands.recv())
                .await
                .is_err()
        );
        drop(client);
        tokio::time::timeout(Duration::from_secs(1), server)
            .await
            .unwrap()
            .unwrap();
        assert!(commands.recv().await.is_none());
        std::fs::remove_dir_all(directory).unwrap();
    }
}
