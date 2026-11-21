//! Blocking native IO is isolated from the lifecycle actor. Read backpressure
//! is bounded; blocking writes cannot prevent a separate control connection.
use portable_pty::{Child, CommandBuilder, MasterPty, PtySize};
use std::{
    collections::BTreeMap,
    io::{Read, Write},
    path::Path,
};
use tokio::sync::mpsc;
pub(crate) enum IoEvent {
    CursorQuery,
    Output(Vec<u8>),
    Closed,
    ReadError,
    Written { id: String, bytes: usize, ok: bool },
}
pub(crate) struct Pty {
    child: Box<dyn Child + Send + Sync>,
    master: Option<Box<dyn MasterPty + Send>>,
    writes: std::sync::mpsc::SyncSender<(String, String)>,
    stopped: bool,
}
impl Pty {
    pub fn stop(&mut self) -> Result<(), String> {
        if self.stopped {
            return Ok(());
        }
        self.stopped = true;
        #[cfg(unix)]
        if let Some(id) = self.child.process_id() {
            use nix::{
                sys::signal::{Signal, killpg},
                unistd::Pid,
            };
            // portable-pty creates a new session/group before exec. A trusted
            // child can still intentionally escape with setsid; this is not a
            // Unix sandbox or a cgroup-level containment promise.
            if let Err(e) = killpg(Pid::from_raw(id as i32), Signal::SIGKILL)
                && e != nix::errno::Errno::ESRCH
            {
                return Err("cannot kill terminal process group".into());
            }
        }
        let result = match self.child.try_wait() {
            Ok(Some(_)) => Ok(()),
            _ => self
                .child
                .kill()
                .map_err(|_| "cannot stop terminal process".into()),
        };
        if let Some(master) = self.master.take() {
            // ClosePseudoConsole may wait for the output reader. Close it away
            // from the actor, which must keep draining the bounded channel.
            std::thread::spawn(move || drop(master));
        }
        result
    }
}
impl Drop for Pty {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

impl Pty {
    pub fn write(&self, id: String, data: String) -> Result<(), String> {
        self.writes
            .try_send((id, data))
            .map_err(|_| "terminal input queue unavailable".into())
    }
    pub fn resize(&self, rows: u16, cols: u16) -> Result<(), String> {
        let master = self.master.as_ref().ok_or("terminal closed")?;
        master
            .resize(size(rows, cols))
            .map_err(|_| "terminal resize failed")?;
        let actual = master
            .get_size()
            .map_err(|_| "cannot inspect terminal size")?;
        if actual.rows != rows || actual.cols != cols {
            return Err("terminal resize was not applied".into());
        }
        Ok(())
    }
    pub fn try_wait(&mut self) -> Result<Option<portable_pty::ExitStatus>, String> {
        self.child
            .try_wait()
            .map_err(|_| "cannot observe terminal child".into())
    }
}

pub(crate) struct Process<'a> {
    pub command: &'a Path,
    pub args: &'a [String],
    pub environment: &'a BTreeMap<String, String>,
    pub cwd: &'a Path,
    pub dimensions: (u16, u16),
}

pub(crate) fn spawn(process: Process<'_>) -> Result<(Pty, mpsc::Receiver<IoEvent>), String> {
    let pair = portable_pty::native_pty_system()
        .openpty(size(process.dimensions.0, process.dimensions.1))
        .map_err(|_| "cannot create PTY")?;
    let mut builder = CommandBuilder::new(process.command);
    builder.args(process.args);
    builder.env_clear();
    for (key, value) in process.environment {
        builder.env(key, value);
    }
    builder.cwd(process.cwd);
    let child = pair
        .slave
        .spawn_command(builder)
        .map_err(|_| "cannot launch terminal preset")?;
    drop(pair.slave);
    let mut pty = Pty {
        child,
        master: Some(pair.master),
        writes: std::sync::mpsc::sync_channel(1).0,
        stopped: false,
    };
    let mut reader = pty
        .master
        .as_ref()
        .unwrap()
        .try_clone_reader()
        .map_err(|_| "cannot open terminal output")?;
    let mut writer = pty
        .master
        .as_ref()
        .unwrap()
        .take_writer()
        .map_err(|_| "cannot open terminal input")?;
    let (events_tx, events) = mpsc::channel(16);
    let output_tx = events_tx.clone();
    std::thread::Builder::new()
        .name("terminal-output".into())
        .spawn(move || {
            let mut buf = [0; 8192];
            let mut startup_cursor = StartupCursor::default();
            loop {
                match reader.read(&mut buf) {
                    Ok(0) => {
                        let _ = output_tx.blocking_send(IoEvent::Closed);
                        break;
                    }
                    Ok(n) => {
                        if cfg!(windows) && startup_cursor.observe(&buf[..n]) {
                            let _ = output_tx.blocking_send(IoEvent::CursorQuery);
                        }
                        if output_tx
                            .blocking_send(IoEvent::Output(buf[..n].to_vec()))
                            .is_err()
                        {
                            break;
                        }
                    }
                    // Linux PTY masters report EIO when the last slave closes.
                    Err(e) if cfg!(unix) && e.raw_os_error() == Some(5) => {
                        let _ = output_tx.blocking_send(IoEvent::Closed);
                        break;
                    }
                    Err(_) => {
                        let _ = output_tx.blocking_send(IoEvent::ReadError);
                        break;
                    }
                }
            }
        })
        .map_err(|_| "cannot start terminal reader")?;
    let (writes, write_rx) = std::sync::mpsc::sync_channel::<(String, String)>(1);
    pty.writes = writes;
    std::thread::Builder::new()
        .name("terminal-input".into())
        .spawn(move || {
            while let Ok((id, data)) = write_rx.recv() {
                let ok = writer
                    .write_all(data.as_bytes())
                    .and_then(|_| writer.flush())
                    .is_ok();
                if events_tx
                    .blocking_send(IoEvent::Written {
                        id,
                        bytes: data.len(),
                        ok,
                    })
                    .is_err()
                {
                    break;
                }
            }
        })
        .map_err(|_| "cannot start terminal writer")?;
    Ok((pty, events))
}
/// Only the first startup DSR is answered. This is a fresh empty terminal, so
/// the initial cursor is 1,1. Later application escape sequences stay data;
/// this module is not a terminal emulator and never synthesizes user commands.
#[derive(Default)]
struct StartupCursor {
    matched: usize,
    answered: bool,
}
impl StartupCursor {
    fn observe(&mut self, bytes: &[u8]) -> bool {
        if self.answered {
            return false;
        }
        for &byte in bytes {
            if byte == b"\x1b[6n"[self.matched] {
                self.matched += 1;
            } else {
                self.matched = usize::from(byte == 0x1b);
            }
            if self.matched == 4 {
                self.answered = true;
                return true;
            }
        }
        false
    }
}

fn size(rows: u16, cols: u16) -> PtySize {
    PtySize {
        rows,
        cols,
        pixel_width: 0,
        pixel_height: 0,
    }
}

#[cfg(test)]
mod tests {
    use super::StartupCursor;
    #[test]
    fn conpty_startup_cursor_is_matched_across_reads_and_answered_only_once() {
        let mut cursor = StartupCursor::default();
        assert!(!cursor.observe(b"prefix\x1b["));
        assert!(cursor.observe(b"6n"));
        assert!(!cursor.observe(b"\x1b[6n"));
        let mut cursor = StartupCursor::default();
        assert!(!cursor.observe(b"\x1b[5n"));
        assert!(cursor.observe(b"\x1b[6n"));
    }
}
