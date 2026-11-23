use super::*;
use std::{
    io::{BufRead, Write},
    process::Stdio,
    sync::mpsc,
    time::Duration,
};

#[test]
fn surrogate_owner_fixture() {
    match std::env::var("FLIUNO_SURROGATE_OWNER").as_deref() {
        Ok("nested") => {
            verify_nested_workload();
            return;
        }
        Ok("child") => {
            std::io::copy(&mut std::io::stdin(), &mut std::io::sink()).unwrap();
            return;
        }
        Ok("1") => {}
        _ => return,
    }
    let pipes = super::super::stdio::Pipes::create("OW").unwrap();
    let owner = Inheritance::new(pipes.child_handles()).unwrap();
    // SAFETY: read-only diagnostic of our own still-live process handle.
    let pid = unsafe { GetProcessId(owner.process.as_raw_handle()) };
    // A serial libtest runner prints the test name without a trailing newline.
    println!("\nsurrogate:{pid}");
    std::io::stdout().flush().unwrap();
    std::io::copy(&mut std::io::stdin(), &mut std::io::sink()).unwrap();
    drop(owner);
}

fn verify_nested_workload() {
    let outer = super::super::job::create(crate::ResourceLimits::default()).unwrap();
    // SAFETY: this is a dedicated fixture process, never the parent test runner.
    check(unsafe { AssignProcessToJobObject(outer.as_raw_handle(), GetCurrentProcess()) }).unwrap();
    let outer_handle = outer.as_raw_handle();
    // OS-lifetime ownership avoids killing this fixture during Rust unwinding.
    // Its exit closes this non-inheritable handle, as in a real service worker.
    std::mem::forget(outer);
    let child = crate::spawn(&crate::LaunchSpec {
        program_sha256: None,
        assets: vec![],
        program: std::env::current_exe().unwrap(),
        args: vec![
            "--exact".into(),
            "windows::inheritance::tests::surrogate_owner_fixture".into(),
            "--nocapture".into(),
        ],
        env: [("FLIUNO_SURROGATE_OWNER".into(), "child".into())].into(),
        resources: Default::default(),
        linux_cgroup_parent: None,
    })
    .unwrap();
    let mut inside = 0;
    // SAFETY: still-owned child observation handle and live outer Job handle.
    check(unsafe {
        IsProcessInJob(
            child.inner.state.process.as_raw_handle(),
            outer_handle,
            &mut inside,
        )
    })
    .unwrap();
    assert_ne!(
        inside, 0,
        "workload escaped the enclosing non-breakaway job"
    );
    drop(child);
    println!("outer-job-preserved");
}

#[test]
fn workload_preserves_an_enclosing_non_breakaway_job() {
    let mut fixture = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "windows::inheritance::tests::surrogate_owner_fixture",
            "--nocapture",
        ])
        .env("FLIUNO_SURROGATE_OWNER", "nested")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    let finished = loop {
        if fixture.try_wait().unwrap().is_some() {
            break true;
        }
        if std::time::Instant::now() >= deadline {
            break false;
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    if !finished {
        fixture.kill().unwrap();
    }
    let output = fixture.wait_with_output().unwrap();
    assert!(
        finished && output.status.success(),
        "nested Job fixture failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("outer-job-preserved"));
}

#[test]
fn hard_killed_creator_does_not_leave_a_suspended_surrogate() {
    let mut creator = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "windows::inheritance::tests::surrogate_owner_fixture",
            "--nocapture",
        ])
        .env("FLIUNO_SURROGATE_OWNER", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let output = creator.stdout.take().unwrap();
    let (sender, receiver) = mpsc::sync_channel(1);
    let reader = std::thread::spawn(move || {
        for line in std::io::BufReader::new(output).lines() {
            if let Ok(line) = line
                && let Some(pid) = line.strip_prefix("surrogate:")
            {
                let _ = sender.send(pid.parse::<u32>().unwrap());
                break;
            }
        }
    });
    let observed = receiver.recv_timeout(Duration::from_secs(20));
    let process = observed.as_ref().ok().map(|pid| {
        // SAFETY: PID is used only to open a read/wait diagnostic handle.
        // The creator remains alive and owns this surrogate during lookup.
        let raw = unsafe {
            OpenProcess(
                PROCESS_SYNCHRONIZE | PROCESS_QUERY_LIMITED_INFORMATION,
                0,
                *pid,
            )
        };
        if raw.is_null() {
            Err(io::Error::last_os_error())
        } else {
            // SAFETY: successful OpenProcess returned a unique observation handle.
            Ok(unsafe { own(raw) })
        }
    });
    creator.kill().unwrap();
    creator.wait().unwrap();
    reader.join().unwrap();
    observed.expect("creator never reported its surrogate");
    let process = process.unwrap().expect("cannot observe surrogate");
    // SAFETY: live observation handle, bounded wait after creator hard kill.
    let exited = unsafe { WaitForSingleObject(process.as_raw_handle(), 5000) };
    assert_eq!(
        exited, WAIT_OBJECT_0,
        "creator crash left a suspended surrogate"
    );
}
