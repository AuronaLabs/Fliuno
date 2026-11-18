//! Serialized terminal lifecycle and durable mutation reservations. No socket
//! decoding or blocking native pipe writes run in this state machine.
use super::{
    Command,
    pty::{IoEvent, Pty},
};
use crate::{protocol::*, spool::Spool};
use serde_json::{Value, json};
use std::time::Duration;
use tokio::{sync::oneshot, time::Instant};

struct Pending {
    id: String,
    reply: oneshot::Sender<Result<Value, String>>,
}
pub(super) struct Actor {
    spool: Spool,
    pty: Pty,
    pending: Option<Pending>,
    stopping: Option<Instant>,
    expires: Instant,
}
impl Actor {
    pub fn new(spool: Spool, pty: Pty, lifetime_ms: u64) -> Result<Self, String> {
        let mut s = spool.snapshot()?;
        let lifetime_ms = lifetime_ms.min(s.expires_at_unix_ms.saturating_sub(now_ms()));
        s.status = if cfg!(windows) {
            Status::Starting
        } else {
            Status::Running
        };
        spool.save(&s)?;
        Ok(Self {
            spool,
            pty,
            pending: None,
            stopping: None,
            expires: Instant::now() + Duration::from_millis(lifetime_ms),
        })
    }
    pub fn command(&mut self, command: Command) -> Result<(), String> {
        if command.reply.is_closed() {
            return Ok(());
        }
        let s = self.spool.snapshot()?;
        if command.session_id != s.session_id {
            let _ = command
                .reply
                .send(Err("terminal belongs to another session".into()));
            return Ok(());
        }
        let request =
            serde_json::to_string(&command.operation).map_err(|_| "cannot encode operation")?;
        let op_id = match &command.operation {
            Operation::Write { operation_id, .. }
            | Operation::Resize { operation_id, .. }
            | Operation::Kill { operation_id }
            | Operation::Release { operation_id } => Some(operation_id.clone()),
            _ => None,
        };
        let is_stop = matches!(
            command.operation,
            Operation::Kill { .. } | Operation::Release { .. }
        );
        if let Some(id) = &op_id {
            if id.len() != 64 || !id.bytes().all(|b| b.is_ascii_hexdigit()) {
                let _ = command
                    .reply
                    .send(Err("invalid terminal operation id".into()));
                return Ok(());
            }
            match self.spool.reserve(id, &request, is_stop) {
                Ok(Some(result)) => {
                    let _ = command.reply.send(result);
                    return Ok(());
                }
                Err(error) => {
                    let _ = command.reply.send(Err(error));
                    return Ok(());
                }
                _ => {}
            }
        }
        let result = match command.operation {
            Operation::Status => Ok(json!(visible(s))),
            Operation::Output { after, limit } | Operation::Subscribe { after, limit } => {
                self.spool.output(after, limit).map(|mut o| {
                    o.snapshot.supervisor_connected = true;
                    json!(o)
                })
            }
            Operation::Write { operation_id, data } => {
                if s.status != Status::Running
                    || self.pending.is_some()
                    || data.is_empty()
                    || data.len() > INPUT_LIMIT
                    || s.input_bytes + data.len() as u64 > 1024 * 1024
                {
                    Err("terminal not writable or input budget exhausted".into())
                } else if self.pty.write(operation_id.clone(), data).is_err() {
                    Err("terminal input queue unavailable".into())
                } else {
                    self.pending = Some(Pending {
                        id: operation_id,
                        reply: command.reply,
                    });
                    return Ok(());
                }
            }
            Operation::Resize { rows, cols, .. } => {
                if s.status != Status::Running {
                    Err("terminal is not running".into())
                } else {
                    dimensions(rows, cols)
                        .and_then(|_| self.pty.resize(rows, cols))
                        .and_then(|_| {
                            let mut s = self.spool.snapshot()?;
                            s.rows = rows;
                            s.cols = cols;
                            self.spool.save(&s)?;
                            Ok(json!(visible(s)))
                        })
                }
            }
            op @ (Operation::Kill { .. } | Operation::Release { .. }) => {
                let release = matches!(op, Operation::Release { .. });
                let result = self.pty.stop();
                let mut s = self.spool.snapshot()?;
                s.status = if result.is_err() {
                    Status::OutcomeUnknown
                } else if release {
                    Status::Released
                } else {
                    Status::Killed
                };
                self.spool.save(&s)?;
                self.stop_after_drain();
                result.map(|_| json!(visible(s)))
            }
        };
        // Cache known failures too. Reserving without a result is reserved for
        // genuinely uncertain writes, not a validation error or exhausted cap.
        if let Some(id) = op_id {
            self.spool.finish(&id, &result)?;
        }
        let _ = command.reply.send(result);
        Ok(())
    }
    pub fn event(&mut self, event: IoEvent) -> Result<(), String> {
        match event {
            IoEvent::CursorQuery => self
                .pty
                .write("conpty-startup-cursor".into(), "\u{1b}[1;1R".into())?,
            IoEvent::Output(bytes) => self.spool.append(&bytes)?,
            IoEvent::Closed => {
                let mut s = self.spool.snapshot()?;
                s.output_closed = true;
                self.spool.save(&s)?;
            }
            IoEvent::ReadError => {
                let mut s = self.spool.snapshot()?;
                s.status = Status::OutcomeUnknown;
                s.output_closed = false;
                self.spool.save(&s)?;
                let _ = self.pty.stop();
                self.stop_after_drain();
            }
            IoEvent::Written { id, bytes, ok } => {
                if id == "conpty-startup-cursor" {
                    if !ok {
                        return Err("ConPTY startup cursor handshake failed".into());
                    }
                    let mut s = self.spool.snapshot()?;
                    s.status = Status::Running;
                    self.spool.save(&s)?;
                    return Ok(());
                }
                if let Some(p) = self.pending.take() {
                    if p.id != id {
                        return Err("terminal input identity mismatch".into());
                    }
                    let result = if ok {
                        let mut s = self.spool.snapshot()?;
                        s.input_bytes += bytes as u64;
                        self.spool.save(&s)?;
                        let value = json!({"terminalId":s.terminal_id,"operationId":id,"writtenBytes":bytes});
                        self.spool.finish(&id, &Ok(value.clone()))?;
                        Ok(value)
                    } else {
                        Err("terminal input outcome unknown; do not replay".into())
                    };
                    let _ = p.reply.send(result);
                }
            }
        }
        Ok(())
    }
    /// Root exit is distinct from drained output. A short drain window keeps
    /// final bytes, then process exit closes the Windows Job for any remaining
    /// descendants. The supervisor never remains indefinitely after root exit.
    pub fn tick(&mut self) -> Result<bool, String> {
        let mut s = self.spool.snapshot()?;
        if s.exit_code.is_none()
            && let Some(status) = self.pty.try_wait()?
        {
            s.exit_code = Some(status.exit_code());
            s.signal = status.signal().map(str::to_owned);
            if matches!(s.status, Status::Starting | Status::Running) {
                s.status = Status::Exited;
            }
            self.spool.save(&s)?;
            let _ = self.pty.stop();
            self.stop_after_drain();
        }
        if Instant::now() >= self.expires && self.stopping.is_none() {
            let result = self.pty.stop();
            s = self.spool.snapshot()?;
            if matches!(s.status, Status::Starting | Status::Running) {
                s.status = if result.is_ok() {
                    Status::Expired
                } else {
                    Status::OutcomeUnknown
                };
            }
            self.spool.save(&s)?;
            self.stop_after_drain();
        }
        Ok(self
            .stopping
            .is_some_and(|deadline| Instant::now() >= deadline))
    }
    fn stop_after_drain(&mut self) {
        self.stopping
            .get_or_insert(Instant::now() + Duration::from_millis(750));
    }
    pub fn snapshot(&self) -> Result<Snapshot, String> {
        self.spool.snapshot()
    }
}
fn visible(mut snapshot: Snapshot) -> Snapshot {
    snapshot.supervisor_connected = true;
    snapshot
}
