//! Native controls must succeed before a sandbox rejection counts as evidence.
#[cfg(windows)]
mod windows {
    use fliuno_sandbox::{Child, LaunchSpec};
    use std::{
        collections::BTreeMap,
        fs,
        io::{Read, Write},
        net::TcpListener,
        path::PathBuf,
        process::Command,
        thread,
        time::{Duration, Instant},
    };

    fn probe() -> PathBuf {
        env!("CARGO_BIN_EXE_fliuno-sandbox-probe").into()
    }
    fn spec(args: Vec<String>) -> LaunchSpec {
        LaunchSpec {
            program_sha256: None,
            assets: vec![],
            program: probe(),
            args,
            env: BTreeMap::new(),
            resources: Default::default(),
            linux_cgroup_parent: None,
        }
    }
    fn finish(mut child: Child) -> (String, String) {
        drop(child.stdin.take());
        let mut stdout = child.stdout.take().unwrap();
        let mut stderr = child.stderr.take().unwrap();
        let reader = thread::spawn(move || {
            let mut output = String::new();
            stdout.read_to_string(&mut output).unwrap();
            output
        });
        let errors = thread::spawn(move || {
            let mut output = String::new();
            stderr.read_to_string(&mut output).unwrap();
            output
        });
        let deadline = Instant::now() + Duration::from_secs(10);
        let status = loop {
            if let Some(status) = child.try_wait().unwrap() {
                break status;
            }
            if Instant::now() >= deadline {
                child.kill().unwrap();
                panic!("sandbox probe timed out");
            }
            thread::sleep(Duration::from_millis(10));
        };
        child.kill().unwrap();
        let errors = errors.join().unwrap();
        assert!(status.success(), "sandbox probe exited {status}: {errors}");
        (reader.join().unwrap(), errors)
    }

    #[test]
    fn pipes_and_windows_argument_encoding_roundtrip() {
        let mut child = fliuno_sandbox::spawn(&spec(vec!["echo".into()])).unwrap();
        child
            .stdin
            .as_mut()
            .unwrap()
            .write_all("中文 protocol\n".as_bytes())
            .unwrap();
        assert_eq!(finish(child).0, "中文 protocol\n");
        let args = [
            "",
            "two words",
            "quote\"inside",
            "end\\",
            "slash\\\"quote",
            "中文",
        ];
        let expected = args
            .iter()
            .map(|arg| format!("{}:{arg}\n", arg.len()))
            .collect::<String>();
        let child = fliuno_sandbox::spawn(&spec(
            std::iter::once("args")
                .chain(args)
                .map(str::to_owned)
                .collect(),
        ))
        .unwrap();
        assert_eq!(finish(child).0.replace("\r\n", "\n"), expected);
    }

    #[test]
    fn app_container_denies_private_host_file_and_network_with_native_controls() {
        let root = std::env::temp_dir().join(format!("fliuno-boundary-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&root).unwrap();
        let outside = root.join("private.txt");
        fs::write(&outside, "private").unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let args = vec![
            "boundaries".into(),
            outside.to_string_lossy().into_owned(),
            listener.local_addr().unwrap().to_string(),
        ];
        let native = Command::new(probe())
            .args(&args)
            .current_dir(&root)
            .env("FLIUNO_PROBE_SECRET", "native-fixture")
            .output()
            .unwrap();
        assert!(
            native.status.success(),
            "{}",
            String::from_utf8_lossy(&native.stderr)
        );
        let control = String::from_utf8(native.stdout).unwrap();
        for expected in [
            "container=false",
            "read=true",
            "write=true",
            "network=true",
            "isolated=false",
            "env=true",
        ] {
            assert!(
                control.contains(expected),
                "native control failed: {control}"
            );
        }
        fs::write(&outside, "private").unwrap();
        let inherited_check = Command::new(probe())
            .arg("isolated-boundaries")
            .args(&args[1..])
            .env("FLIUNO_PROBE_SECRET", "parent-fixture")
            .output()
            .unwrap();
        assert!(
            inherited_check.status.success(),
            "{}",
            String::from_utf8_lossy(&inherited_check.stderr)
        );
        let inherited_check = String::from_utf8(inherited_check.stdout).unwrap();
        assert!(
            inherited_check.contains("container=true") && inherited_check.contains("env=false"),
            "parent environment leaked: {inherited_check}"
        );
        let child = fliuno_sandbox::spawn(&spec(args)).unwrap();
        let private_directory = child.private_directory().to_owned();
        let (report, _) = finish(child);
        for expected in [
            "container=true",
            "read=false",
            "write=false",
            "network=false",
            "isolated=true",
            "env=false",
        ] {
            assert!(report.contains(expected), "boundary failed: {report}");
        }
        assert_eq!(fs::read_to_string(&outside).unwrap(), "private");
        assert!(
            !private_directory.exists(),
            "sandbox profile storage was not removed"
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn job_process_limit_prevents_spawning_a_child() {
        let mut limited = spec(vec!["process-limit".into()]);
        limited.resources.pids_max = Some(1);
        let child = fliuno_sandbox::spawn(&limited).unwrap();
        let (report, _) = finish(child);
        assert!(
            report.contains("spawn-blocked"),
            "limit was not enforced: {report}"
        );
    }

    #[test]
    fn dropping_child_terminates_a_running_process_and_closes_its_pipes() {
        let mut child = fliuno_sandbox::spawn(&spec(vec!["tree".into()])).unwrap();
        let directory = child.private_directory().to_owned();
        let stdout = child.stdout.take().unwrap();
        let (sent, recv) = std::sync::mpsc::channel();
        let reader = thread::spawn(move || {
            let mut reader = std::io::BufReader::new(stdout);
            for _ in 0..2 {
                let mut line = String::new();
                std::io::BufRead::read_line(&mut reader, &mut line).unwrap();
                sent.send(line).unwrap();
            }
            let mut rest = String::new();
            reader.read_to_string(&mut rest).unwrap();
            sent.send("eof".into()).unwrap();
        });
        let mut lines = (0..2)
            .map(|_| {
                recv.recv_timeout(Duration::from_secs(5))
                    .unwrap()
                    .trim()
                    .to_owned()
            })
            .collect::<Vec<_>>();
        lines.sort();
        assert_eq!(lines, ["ready", "tree"]);
        drop(child);
        assert_eq!(
            recv.recv_timeout(Duration::from_secs(5)).unwrap(),
            "eof",
            "descendant retained its stdout pipe"
        );
        reader.join().unwrap();
        assert!(!directory.exists());
    }
}

#[cfg(all(not(windows), not(target_os = "linux")))]
#[test]
fn unsupported_platform_refuses_to_launch_instead_of_falling_back() {
    let error = fliuno_sandbox::spawn(&fliuno_sandbox::LaunchSpec {
        program_sha256: None,
        assets: vec![],
        program: std::env::current_exe().unwrap(),
        args: vec![],
        env: Default::default(),
        resources: Default::default(),
        linux_cgroup_parent: None,
    })
    .err()
    .expect("must refuse unsupported native isolation");
    assert_eq!(error.kind(), std::io::ErrorKind::Unsupported);
}

#[cfg(target_os = "linux")]
mod linux {
    #[allow(unsafe_code)]
    mod helpers {
        use std::io;

        pub(super) fn kill_probe_process(pid: libc::pid_t) -> io::Result<()> {
            // SAFETY: the test fixture reported this live descendant PID directly.
            if unsafe { libc::kill(pid, libc::SIGKILL) } == -1 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        }
    }

    use fliuno_sandbox::{Child, LaunchSpec};
    use std::{
        collections::BTreeMap,
        fs,
        io::{Read, Write},
        net::TcpListener,
        path::PathBuf,
        process::Command,
        thread,
        time::{Duration, Instant},
    };

    fn probe() -> PathBuf {
        env!("CARGO_BIN_EXE_fliuno-sandbox-probe").into()
    }

    fn spec(args: Vec<String>) -> LaunchSpec {
        LaunchSpec {
            program_sha256: None,
            assets: vec![],
            program: probe(),
            args,
            env: BTreeMap::new(),
            resources: Default::default(),
            linux_cgroup_parent: std::env::var_os("FLIUNO_TEST_CGROUP_V2_PARENT")
                .map(PathBuf::from),
        }
    }

    fn finish(mut child: Child) -> String {
        drop(child.stdin.take());
        let mut stdout = child.stdout.take().unwrap();
        let mut stderr = child.stderr.take().unwrap();
        let output = thread::spawn(move || {
            let mut value = String::new();
            stdout.read_to_string(&mut value).unwrap();
            value
        });
        let errors = thread::spawn(move || {
            let mut value = String::new();
            stderr.read_to_string(&mut value).unwrap();
            value
        });
        let deadline = Instant::now() + Duration::from_secs(10);
        let status = loop {
            if let Some(status) = child.try_wait().unwrap() {
                break status;
            }
            if Instant::now() >= deadline {
                child.kill().unwrap();
                panic!("Linux sandbox probe timed out");
            }
            thread::sleep(Duration::from_millis(10));
        };
        child.kill().unwrap();
        let errors = errors.join().unwrap();
        assert!(status.success(), "sandbox probe exited {status}: {errors}");
        output.join().unwrap()
    }

    #[test]
    fn landlock_and_seccomp_deny_host_paths_and_network_but_allow_private_writes() {
        let root = std::env::temp_dir().join(format!("fliuno-linux-test-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&root).unwrap();
        let outside = root.join("private.txt");
        fs::write(&outside, "private").unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let args = vec![
            "boundaries".into(),
            outside.to_string_lossy().into_owned(),
            listener.local_addr().unwrap().to_string(),
        ];

        let native = Command::new(probe())
            .args(&args)
            .current_dir(&root)
            .env("FLIUNO_PROBE_SECRET", "native-fixture")
            .output()
            .unwrap();
        assert!(
            native.status.success(),
            "{}",
            String::from_utf8_lossy(&native.stderr)
        );
        let native = String::from_utf8(native.stdout).unwrap();
        for expected in [
            "read=true",
            "write=true",
            "network=true",
            "inside_write=true",
            "env=true",
        ] {
            assert!(native.contains(expected), "native control failed: {native}");
        }

        fs::write(&outside, "private").unwrap();
        let child = fliuno_sandbox::spawn(&spec(args)).unwrap();
        let private_directory = child.private_directory().to_owned();
        let report = finish(child);
        for expected in [
            "read=false",
            "write=false",
            "network=false",
            "inside_write=true",
            "env=false",
        ] {
            assert!(
                report.contains(expected),
                "sandbox boundary failed: {report}"
            );
        }
        assert_eq!(fs::read_to_string(&outside).unwrap(), "private");
        assert!(!private_directory.exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn configured_limits_fail_closed_when_the_parent_has_no_cgroup_controls() {
        let root = std::env::temp_dir().join(format!("fliuno-not-cgroup-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&root).unwrap();
        let mut requested = spec(vec!["process-limit".into()]);
        requested.resources.pids_max = Some(1);
        requested.linux_cgroup_parent = Some(root.clone());
        let error = fliuno_sandbox::spawn(&requested)
            .err()
            .expect("ordinary directories cannot host cgroup limits");
        assert_eq!(error.kind(), std::io::ErrorKind::Unsupported);
        assert!(fs::read_dir(&root).unwrap().next().is_none());
        fs::remove_dir(root).unwrap();
    }

    #[test]
    fn delegated_cgroup_enforces_the_configured_process_limit() {
        let required = std::env::var_os("FLIUNO_REQUIRE_RESOURCE_LIMITS").is_some();
        let mut limited = spec(vec!["process-limit".into()]);
        limited.resources.pids_max = Some(1);
        limited.resources.memory_max_bytes = Some(256 * 1024 * 1024);
        limited.resources.cpu_quota_micros = Some(100_000);
        let child = match fliuno_sandbox::spawn(&limited) {
            Ok(child) => child,
            Err(error) if error.kind() == std::io::ErrorKind::Unsupported && !required => {
                eprintln!("skipping resource enforcement assertion: {error}");
                return;
            }
            Err(error) => panic!("required cgroup resource limit failed: {error}"),
        };
        assert!(
            child.uses_cgroup_v2(),
            "resource-limited process escaped cgroup"
        );
        assert_eq!(child.containment().unwrap().limits, Some(limited.resources));
        let report = finish(child);
        assert!(
            report.contains("spawn-blocked"),
            "limit was not enforced: {report}"
        );
    }

    #[test]
    fn dropping_child_kills_process_group_and_closes_pipes() {
        let mut child = fliuno_sandbox::spawn(&spec(vec!["tree".into()])).unwrap();
        let directory = child.private_directory().to_owned();
        let stdout = child.stdout.take().unwrap();
        let (sent, recv) = std::sync::mpsc::channel();
        let reader = thread::spawn(move || {
            let mut reader = std::io::BufReader::new(stdout);
            for _ in 0..2 {
                let mut line = String::new();
                std::io::BufRead::read_line(&mut reader, &mut line).unwrap();
                sent.send(line).unwrap();
            }
            let mut rest = String::new();
            reader.read_to_string(&mut rest).unwrap();
            sent.send("eof".into()).unwrap();
        });
        let mut lines = (0..2)
            .map(|_| {
                recv.recv_timeout(Duration::from_secs(5))
                    .unwrap()
                    .trim()
                    .to_owned()
            })
            .collect::<Vec<_>>();
        lines.sort();
        assert_eq!(lines, ["ready", "tree"]);
        child.kill().unwrap();
        child.kill().unwrap();
        assert_eq!(
            recv.recv_timeout(Duration::from_secs(5)).unwrap(),
            "eof",
            "descendant retained its stdout pipe"
        );
        reader.join().unwrap();
        drop(child);
        assert!(!directory.exists());
    }

    #[test]
    fn delegated_cgroup_kills_a_descendant_after_setsid() {
        let mut child = fliuno_sandbox::spawn(&spec(vec!["tree-detached".into()])).unwrap();
        let directory = child.private_directory().to_owned();
        let uses_cgroup = child.uses_cgroup_v2();
        let stdout = child.stdout.take().unwrap();
        let (sent, recv) = std::sync::mpsc::channel();
        let reader = thread::spawn(move || {
            let mut reader = std::io::BufReader::new(stdout);
            loop {
                let mut line = String::new();
                match std::io::BufRead::read_line(&mut reader, &mut line) {
                    Ok(0) => {
                        let _ = sent.send("eof".into());
                        break;
                    }
                    Ok(_) => {
                        if sent.send(line).is_err() {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
        });

        let first = recv.recv_timeout(Duration::from_secs(5)).unwrap();
        let second = recv.recv_timeout(Duration::from_secs(5)).unwrap();
        let pid_line = [first.as_str(), second.as_str()]
            .into_iter()
            .find(|line| line.starts_with("descendant="))
            .expect("probe did not report descendant PID");
        assert!([first.as_str(), second.as_str()].contains(&"tree\n"));
        let descendant_pid = pid_line
            .trim_start_matches("descendant=")
            .trim()
            .parse::<libc::pid_t>()
            .unwrap();

        child.stdin.as_mut().unwrap().write_all(b"x").unwrap();
        let readiness = recv.recv_timeout(Duration::from_secs(5));
        if readiness.as_deref() != Ok("detached-ready\n") {
            let _ = child.stdin.as_mut().unwrap().write_all(b"q");
            let _ = child.kill();
            let _ = recv.recv_timeout(Duration::from_secs(5));
            panic!("descendant did not detach successfully: {readiness:?}");
        }

        if !uses_cgroup && std::env::var_os("FLIUNO_REQUIRE_DELEGATED_CGROUP").is_some() {
            let _ = child.stdin.as_mut().unwrap().write_all(b"q");
            let _ = recv.recv_timeout(Duration::from_secs(5));
            let _ = child.kill();
            let _ = recv.recv_timeout(Duration::from_secs(5));
            panic!("required delegated cgroup v2 was unavailable");
        }
        if !uses_cgroup {
            eprintln!("skipping cgroup kill assertion: no writable delegated cgroup v2");
            child.stdin.as_mut().unwrap().write_all(b"q").unwrap();
            assert_eq!(
                recv.recv_timeout(Duration::from_secs(5)).unwrap(),
                "detached-exit\n"
            );
        }
        if let Err(error) = child.kill() {
            let _ = helpers::kill_probe_process(descendant_pid);
            let _ = recv.recv_timeout(Duration::from_secs(5));
            panic!("sandbox tree cleanup failed: {error}");
        }
        child.kill().unwrap();
        let eof = recv.recv_timeout(Duration::from_secs(5));
        if eof.as_deref() != Ok("eof") {
            let _ = helpers::kill_probe_process(descendant_pid);
            let _ = recv.recv_timeout(Duration::from_secs(5));
            panic!("sandbox descendant retained stdout after cleanup: {eof:?}");
        }
        reader.join().unwrap();
        drop(child);
        assert!(!directory.exists());
    }
}
