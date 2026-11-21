//! Opt-in native timings. Run this probe alone, with the same build profile and load.
#![cfg(any(windows, target_os = "linux"))]

use fliuno_sandbox::{LaunchSpec, ResourceLimits, spawn};
use std::{
    io::{BufRead, BufReader, Read},
    path::PathBuf,
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};

const SAMPLES: usize = 24;
const WARMUP: usize = 3;
const DEADLINE: Duration = Duration::from_secs(10);

#[test]
#[ignore = "native latency measurement; run alone with --ignored --nocapture"]
fn scratch_start_and_cancel() {
    let delegated = std::env::var_os("FLIUNO_TEST_CGROUP_V2_PARENT").map(PathBuf::from);
    let spec = LaunchSpec {
        program_sha256: None,
        assets: vec![],
        program: PathBuf::from(env!("CARGO_BIN_EXE_fliuno-sandbox-probe")),
        args: vec!["tree".into()],
        env: Default::default(),
        resources: ResourceLimits::default(),
        linux_cgroup_parent: delegated.clone(),
    };
    let mut launch = Vec::new();
    let mut stop = Vec::new();
    let mut cleanup = Vec::new();
    for iteration in 0..SAMPLES + WARMUP {
        let launched_at = Instant::now();
        let mut child = spawn(&spec).unwrap();
        let (notify, events) = mpsc::channel();
        let mut stdout = BufReader::new(child.stdout.take().unwrap());
        let reader = thread::spawn(move || {
            let mut line = String::new();
            stdout.read_line(&mut line).unwrap();
            if notify.send((line.trim() == "tree", Instant::now())).is_ok() {
                std::io::copy(&mut stdout.by_ref(), &mut std::io::sink()).unwrap();
                let _ = notify.send((true, Instant::now()));
            }
        });
        let (ready, ready_at) = events.recv_timeout(DEADLINE).unwrap();
        assert!(ready, "sandbox probe did not create its second process");
        assert_eq!(child.uses_cgroup_v2(), delegated.is_some());
        let containment = child.containment().unwrap();
        if cfg!(windows) {
            assert!(matches!(
                containment.tree_backend,
                fliuno_sandbox::TreeBackend::WindowsJob
            ));
            assert_eq!(containment.limits.unwrap().pids_max, Some(16));
            assert_eq!(
                containment.limits.unwrap().memory_max_bytes,
                Some(512 * 1024 * 1024)
            );
        } else {
            assert_eq!(containment.limits.unwrap(), ResourceLimits::default());
            assert_eq!(
                matches!(
                    containment.tree_backend,
                    fliuno_sandbox::TreeBackend::LinuxCgroupV2
                ),
                delegated.is_some()
            );
        }

        let directory = child.private_directory().to_owned();
        let stopped_at = Instant::now();
        child.terminate_tree().unwrap();
        let (_, eof_at) = events.recv_timeout(DEADLINE).unwrap();
        let reaped_at = Instant::now().max(eof_at);
        drop(child);
        assert!(!directory.exists(), "scratch directory was left behind");
        let cleaned_at = Instant::now();
        reader.join().unwrap();
        if iteration >= WARMUP {
            launch.push(ready_at - launched_at);
            stop.push(reaped_at - stopped_at);
            cleanup.push(cleaned_at - stopped_at);
        }
    }
    println!("samples={SAMPLES} delegated={}", delegated.is_some());
    for (label, samples) in [
        ("ready", &mut launch),
        ("reaped", &mut stop),
        ("cleaned", &mut cleanup),
    ] {
        samples.sort_unstable();
        let ms = |percent: usize| {
            let index = (samples.len() * percent).div_ceil(100) - 1;
            samples[index].as_secs_f64() * 1000.0
        };
        println!("{label} p50_ms={:.3} p95_ms={:.3}", ms(50), ms(95));
    }
}
