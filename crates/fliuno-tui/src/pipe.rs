//! Bridge blocking child stdout to the connection worker's input selection.
use crossbeam_channel::{Receiver, bounded};
use std::{
    cell::Cell,
    io::{self, BufRead, Read},
    process::ChildStdout,
    rc::Rc,
    thread,
};

type Chunk = io::Result<Vec<u8>>;

pub struct PipeInbox {
    pub chunks: Receiver<Chunk>,
    pub staged: Cell<Option<Chunk>>,
    pub buffered: Cell<bool>,
}

pub struct PipeReader {
    inbox: Rc<PipeInbox>,
    chunk: Vec<u8>,
    consumed: usize,
}

impl PipeReader {
    pub fn open(mut stdout: ChildStdout) -> io::Result<(Self, Rc<PipeInbox>)> {
        let (tx, chunks) = bounded(4);
        thread::Builder::new()
            .name("fliuno-server-stdout".into())
            .spawn(move || {
                let mut buf = [0; 8192];
                loop {
                    match stdout.read(&mut buf) {
                        Ok(0) => break,
                        Ok(count) => {
                            if tx.send(Ok(buf[..count].to_vec())).is_err() {
                                break;
                            }
                        }
                        Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                        Err(error) => {
                            let _ = tx.send(Err(error));
                            break;
                        }
                    }
                }
            })?;
        let inbox = Rc::new(PipeInbox {
            chunks,
            staged: Cell::new(None),
            buffered: Cell::new(false),
        });
        Ok((
            Self {
                inbox: Rc::clone(&inbox),
                chunk: Vec::new(),
                consumed: 0,
            },
            inbox,
        ))
    }
}

impl Read for PipeReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let count = self.fill_buf()?.read(buf)?;
        self.consume(count);
        Ok(count)
    }
}

impl BufRead for PipeReader {
    fn fill_buf(&mut self) -> io::Result<&[u8]> {
        if self.consumed == self.chunk.len() {
            let next = self
                .inbox
                .staged
                .take()
                .or_else(|| self.inbox.chunks.recv().ok());
            self.chunk = next.transpose()?.unwrap_or_default();
            self.consumed = 0;
            self.inbox.buffered.set(!self.chunk.is_empty());
        }
        Ok(&self.chunk[self.consumed..])
    }

    fn consume(&mut self, amount: usize) {
        self.consumed = (self.consumed + amount).min(self.chunk.len());
        self.inbox.buffered.set(self.consumed < self.chunk.len());
    }
}
