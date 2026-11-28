// Test-only stdio relay. It pauses one selected RPC until the PTY driver releases it.
use std::{
    env, fs,
    io::{self, BufRead, BufReader, Write},
    path::PathBuf,
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};

fn main() -> io::Result<()> {
    let dir = PathBuf::from(env::var_os("FLIUNO_RELAY_DIR").unwrap());
    let delayed = format!("\"method\":\"{}\"", env::var("FLIUNO_RELAY_DELAY").unwrap());
    let mut server = Command::new(env::var_os("FLIUNO_RELAY_SERVER").unwrap())
        .args(env::args_os().skip(1))
        .stdin(Stdio::piped())
        .stdout(Stdio::inherit())
        .spawn()?;
    let mut input = server.stdin.take().unwrap();
    let mut paused = false;
    let result = (|| {
        for line in BufReader::new(io::stdin()).lines() {
            let line = line?;
            if !paused && line.contains(&delayed) {
                paused = true;
                fs::write(dir.join("paused"), &line)?;
                let until = Instant::now() + Duration::from_secs(30);
                while !dir.join("continue").try_exists()? {
                    if Instant::now() >= until {
                        return Err(io::Error::new(
                            io::ErrorKind::TimedOut,
                            "PTY did not release the relay",
                        ));
                    }
                    thread::sleep(Duration::from_millis(10));
                }
            }
            if paused && dir.join("disconnect").try_exists()? {
                return Err(io::Error::new(
                    io::ErrorKind::ConnectionAborted,
                    "fixture disconnect",
                ));
            }
            if line.contains("\"method\":\"task/start\"") {
                let mut log = fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(dir.join("starts.jsonl"))?;
                writeln!(log, "{line}")?;
            }
            writeln!(input, "{line}")?;
            input.flush()?;
        }
        Ok(())
    })();
    drop(input);
    if result.is_err() {
        let _ = server.kill();
    }
    let _ = server.wait();
    result
}
