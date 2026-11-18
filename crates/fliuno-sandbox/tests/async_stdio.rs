//! Real isolated/trusted peers and cancelled IO. Keep the native process alive while
//! dropping its parent endpoints, so process exit cannot hide blocking IO leaks.
#![cfg(all(feature = "async-io", any(windows, target_os = "linux")))]
use fliuno_sandbox::{Child, LaunchSpec};
use std::{
    collections::BTreeMap,
    io,
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[derive(Clone, Copy, Debug)]
enum Mode {
    Isolated,
    #[cfg(windows)]
    Host,
}
fn modes() -> &'static [Mode] {
    &[
        Mode::Isolated,
        #[cfg(windows)]
        Mode::Host,
    ]
}
enum NativeChild {
    Isolated(Child),
    #[cfg(windows)]
    Host(fliuno_sandbox::HostChild),
}
impl NativeChild {
    fn try_wait(&mut self) -> io::Result<Option<std::process::ExitStatus>> {
        match self {
            Self::Isolated(child) => child.try_wait(),
            #[cfg(windows)]
            Self::Host(child) => child.try_wait(),
        }
    }
    fn kill(&mut self) -> io::Result<()> {
        match self {
            Self::Isolated(child) => child.kill(),
            #[cfg(windows)]
            Self::Host(child) => child.kill(),
        }
    }
}
async fn spawn(
    mode: Mode,
    name: &str,
) -> Result<(NativeChild, fliuno_sandbox::AsyncStdio), fliuno_sandbox::AsyncSpawnError> {
    match mode {
        Mode::Isolated => fliuno_sandbox::spawn_async(spec(name))
            .await
            .map(|(child, pipes)| (NativeChild::Isolated(child), pipes)),
        #[cfg(windows)]
        Mode::Host => {
            let spec = spec(name);
            fliuno_sandbox::spawn_host_async(fliuno_sandbox::HostLaunchSpec {
                program: spec.program,
                args: spec.args,
                env: spec.env,
                cwd: std::env::current_dir().unwrap(),
                resources: Default::default(),
            })
            .await
            .map(|(child, pipes)| (NativeChild::Host(child), pipes))
        }
    }
}

fn spec(mode: &str) -> LaunchSpec {
    LaunchSpec {
        program_sha256: None,
        assets: vec![],
        program: env!("CARGO_BIN_EXE_fliuno-sandbox-probe").into(),
        args: vec![mode.into()],
        env: BTreeMap::new(),
        resources: Default::default(),
        linux_cgroup_parent: None,
    }
}
fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}
async fn drain(mut pipe: fliuno_sandbox::PipeReader) -> io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    match pipe.read_to_end(&mut bytes).await {
        Ok(_) => Ok(bytes),
        Err(error) if error.kind() == io::ErrorKind::BrokenPipe => Ok(bytes),
        Err(error) => Err(error),
    }
}
async fn reap(child: &mut NativeChild) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            assert!(status.success(), "probe failed: {status}");
            return;
        }
        assert!(Instant::now() < deadline, "probe did not exit");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[test]
fn native_async_pipes_deliver_full_input_eof_and_independent_output() {
    for &mode in modes() {
        runtime().block_on(async {
            let (mut child, pipes) = spawn(mode, "echo").await.unwrap();
            let payload = "中文 input\n".repeat(32 * 1024).into_bytes();
            let expected = payload.clone();
            let write = async move {
                let mut stdin = pipes.stdin;
                stdin.write_all(&payload).await?;
                stdin.shutdown().await?;
                drop(stdin);
                io::Result::Ok(())
            };
            let (_, stdout, stderr) = tokio::time::timeout(Duration::from_secs(30), async {
                tokio::try_join!(write, drain(pipes.stdout), drain(pipes.stderr))
            })
            .await
            .unwrap()
            .unwrap();
            assert_eq!(stdout, expected);
            assert!(stderr.is_empty());
            reap(&mut child).await;
        });
    }
}

#[test]
fn cancelled_pipe_io_does_not_hold_runtime_shutdown_until_peer_exit() {
    for &mode in modes() {
        let (owner, owned) = mpsc::sync_channel(1);
        let (armed, ready) = mpsc::sync_channel(1);
        let (done, finished) = mpsc::sync_channel(1);
        let worker = thread::spawn(move || {
            let runtime = runtime();
            runtime.block_on(async {
                let (child, mut pipes) = spawn(mode, "wait").await.unwrap();
                owner
                    .send(child)
                    .unwrap_or_else(|_| panic!("lost probe owner"));
                let mut greeting = [0u8; 6];
                tokio::time::timeout(
                    Duration::from_secs(30),
                    pipes.stdout.read_exact(&mut greeting),
                )
                .await
                .unwrap()
                .unwrap();
                assert_eq!(&greeting, b"ready\n");
                let data = vec![b'x'; 64 * 1024];
                #[cfg(windows)]
                {
                    // mio accepts this one overlapped request before completion.
                    // The pipe's 16 KiB capacity cannot accept it without a reader;
                    // shutdown must wait for the pending write, not report success.
                    pipes.stdin.write_all(&data).await.unwrap();
                    assert!(
                        tokio::time::timeout(Duration::from_millis(100), pipes.stdin.shutdown())
                            .await
                            .is_err()
                    );
                }
                #[cfg(target_os = "linux")]
                {
                    assert!(
                        tokio::time::timeout(Duration::from_millis(100), async {
                            // A Linux anonymous pipe can hold the entire first
                            // chunk (unlike this Windows probe's fixed 16 KiB
                            // pipe). Fill repeatedly before claiming backpressure.
                            for _ in 0..512 {
                                pipes.stdin.write_all(&data).await?;
                            }
                            pipes.stdin.shutdown().await
                        })
                        .await
                        .is_err()
                    );
                }
                let mut buffer = [0u8; 1];
                assert!(
                    tokio::time::timeout(
                        Duration::from_millis(100),
                        pipes.stdout.read(&mut buffer)
                    )
                    .await
                    .is_err()
                );
                armed.send(()).unwrap();
                drop(pipes);
            });
            drop(runtime);
            done.send(()).unwrap();
        });
        let mut child = owned.recv_timeout(Duration::from_secs(45)).unwrap();
        let armed = ready.recv_timeout(Duration::from_secs(35));
        let returned = armed.is_ok() && finished.recv_timeout(Duration::from_secs(3)).is_ok();
        // Always release the native peer before joining, including a regression to
        // blocking pipe IO. A failing teardown assertion cannot deadlock the harness.
        let alive = child.try_wait().unwrap().is_none();
        child.kill().unwrap();
        worker.join().unwrap();
        assert!(armed.is_ok(), "worker failed before exercising pending IO");
        assert!(returned, "runtime shutdown waited for the pipe peer");
        assert!(alive, "process exit masked the pipe cancellation probe");
    }
}

#[test]
fn a_closed_native_reader_cannot_turn_buffered_input_into_success() {
    for &mode in modes() {
        runtime().block_on(async {
            let (mut child, mut pipes) = spawn(mode, "args").await.unwrap();
            assert!(drain(pipes.stdout).await.unwrap().is_empty());
            reap(&mut child).await;
            let result = tokio::time::timeout(Duration::from_secs(3), async {
                pipes.stdin.write_all(b"reader has exited").await?;
                pipes.stdin.shutdown().await
            })
            .await
            .unwrap();
            assert_eq!(result.unwrap_err().kind(), io::ErrorKind::BrokenPipe);
        });
    }
}

#[cfg(windows)]
#[test]
fn failed_pending_native_write_is_reported_by_shutdown() {
    for &mode in modes() {
        runtime().block_on(async {
            let (mut child, mut pipes) = spawn(mode, "wait").await.unwrap();
            let mut greeting = [0u8; 6];
            tokio::time::timeout(
                Duration::from_secs(30),
                pipes.stdout.read_exact(&mut greeting),
            )
            .await
            .unwrap()
            .unwrap();
            assert_eq!(&greeting, b"ready\n");
            pipes.stdin.write_all(&vec![b'x'; 64 * 1024]).await.unwrap();
            child.kill().unwrap();
            // The request was accepted before the peer was killed; flush must
            // observe its asynchronous completion error rather than report success.
            let result = tokio::time::timeout(Duration::from_secs(3), pipes.stdin.shutdown())
                .await
                .unwrap();
            assert_eq!(result.unwrap_err().kind(), io::ErrorKind::BrokenPipe);
        });
    }
}

#[cfg(windows)]
#[test]
fn trusted_resource_limits_are_enforced_by_the_windows_job() {
    use fliuno_sandbox::{HostLaunchSpec, ResourceLimits, spawn_host_async};

    let cases = [
        ("process-limit", ResourceLimits::default(), "spawned"),
        (
            "process-limit",
            ResourceLimits {
                pids_max: Some(1),
                ..Default::default()
            },
            "spawn-blocked",
        ),
        ("memory-limit", ResourceLimits::default(), "allocated"),
        (
            "memory-limit",
            ResourceLimits {
                memory_max_bytes: Some(32 * 1024 * 1024),
                ..Default::default()
            },
            "allocation-blocked",
        ),
    ];
    runtime().block_on(async {
        for (mode, resources, expected) in cases {
            let probe = spec(mode);
            let (child, pipes) = spawn_host_async(HostLaunchSpec {
                program: probe.program,
                args: probe.args,
                env: probe.env,
                cwd: std::env::current_dir().unwrap(),
                resources,
            })
            .await
            .unwrap();
            let mut child = NativeChild::Host(child);
            drop(pipes.stdin);
            let output = tokio::time::timeout(Duration::from_secs(10), drain(pipes.stdout))
                .await
                .unwrap()
                .unwrap();
            reap(&mut child).await;
            assert_eq!(std::str::from_utf8(&output).unwrap().trim(), expected);
        }
    });
}
