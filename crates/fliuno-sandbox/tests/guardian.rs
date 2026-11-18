//! Kill a real sandbox owner without running its destructors. The remaining
//! stdout and delegated cgroup are independent evidence of tree containment.
#[cfg(target_os = "linux")]
mod linux {
    use std::{
        fs,
        io::{BufRead, BufReader, Write},
        path::PathBuf,
        process::{Child, Command, Stdio},
        sync::mpsc,
        thread,
        time::{Duration, Instant},
    };

    struct Owner {
        process: Child,
        group: Option<PathBuf>,
        private_directory: Option<PathBuf>,
    }
    impl Drop for Owner {
        fn drop(&mut self) {
            let _ = self.process.kill();
            let _ = self.process.wait();
            // Only this test's exact private cgroup is touched on assertion
            // failure; no PID scans or mutations of the delegation parent.
            if let Some(group) = &self.group {
                let _ = fs::write(group.join("cgroup.kill"), b"1");
                for _ in 0..500 {
                    if !group.exists() || fs::remove_dir(group).is_ok() {
                        break;
                    }
                    thread::sleep(Duration::from_millis(10));
                }
            }
            if let Some(path) = &self.private_directory {
                let _ = fs::remove_dir_all(path);
            }
        }
    }

    #[test]
    #[ignore = "requires delegated cgroup v2; run scripts/check_linux_cgroup.py"]
    fn host_sigkill_reaps_setsid_descendants_after_the_spawning_thread_exits() {
        let parent = PathBuf::from(std::env::var_os("FLIUNO_TEST_CGROUP_V2_PARENT").unwrap());
        let before = fs::read_dir(&parent)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect::<Vec<_>>();
        let mut process = Command::new(env!("CARGO_BIN_EXE_fliuno-sandbox-probe"))
            .args(["guarded-owner", parent.to_str().unwrap()])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        let stdout = process.stdout.take().unwrap();
        let mut owner = Owner {
            process,
            group: None,
            private_directory: None,
        };
        let (sent, received) = mpsc::channel();
        let reader = thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                if sent.send(line.unwrap()).is_err() {
                    return;
                }
            }
            let _ = sent.send("eof".into());
        });
        let deadline = Instant::now() + Duration::from_secs(10);
        owner.group = Some(loop {
            let groups = fs::read_dir(&parent)
                .unwrap()
                .map(|entry| entry.unwrap().path())
                .filter(|path| path.is_dir() && !before.contains(path))
                .collect::<Vec<_>>();
            if let Some(group) = groups.first() {
                break group.clone();
            }
            if owner.process.try_wait().unwrap().is_some() {
                panic!("sandbox owner failed to start");
            }
            assert!(
                Instant::now() < deadline,
                "sandbox owner did not create a cgroup"
            );
            thread::sleep(Duration::from_millis(10));
        });
        let mut descendant = None;
        let mut spawner_exited = false;
        for _ in 0..4 {
            let line = received.recv_timeout(Duration::from_secs(10)).unwrap();
            if let Some(value) = line.strip_prefix("private_directory=") {
                owner.private_directory = Some(value.into());
            } else if let Some(value) = line.strip_prefix("descendant=") {
                descendant = Some(value.parse::<u32>().unwrap());
            } else if line == "spawner-exited" {
                spawner_exited = true;
            } else {
                assert_eq!(line, "tree");
            }
        }
        assert!(spawner_exited);
        let descendant = descendant.unwrap();
        owner
            .process
            .stdin
            .as_mut()
            .unwrap()
            .write_all(b"x")
            .unwrap();
        assert_eq!(
            received.recv_timeout(Duration::from_secs(5)).unwrap(),
            "detached-ready"
        );
        let group = owner.group.as_ref().unwrap();
        let members = fs::read_to_string(group.join("cgroup.procs")).unwrap();
        assert!(members.lines().any(|pid| pid == descendant.to_string()));
        owner.process.kill().unwrap();
        owner.process.wait().unwrap();
        assert_eq!(
            received.recv_timeout(Duration::from_secs(10)).unwrap(),
            "eof",
            "guardian or sandbox descendant retained the host stdout pipe"
        );
        reader.join().unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        while group.exists() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        assert!(!group.exists(), "guardian left its delegated cgroup behind");
        if let Ok(stat) = fs::read_to_string(format!("/proc/{descendant}/stat")) {
            let state = stat
                .rsplit_once(')')
                .unwrap()
                .1
                .split_whitespace()
                .next()
                .unwrap();
            assert!(
                matches!(state, "Z" | "X"),
                "setsid descendant survived: {state}"
            );
        }
    }
}
