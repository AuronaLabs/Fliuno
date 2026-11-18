//! Local integration probe. Its actions are selected explicitly by test argv.
use std::{
    io::{self, Read, Write},
    net::TcpStream,
    path::Path,
    time::Duration,
};

#[cfg(target_os = "linux")]
#[allow(unsafe_code)]
mod linux_probe {
    use std::io;

    pub(super) fn start_new_session() -> io::Result<()> {
        // SAFETY: setsid takes no pointers and this process is the newly spawned child.
        if unsafe { libc::setsid() } == -1 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}

fn main() -> io::Result<()> {
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    match args.first().map(String::as_str) {
        #[cfg(target_os = "linux")]
        Some("guarded-owner") => {
            let spec = fliuno_sandbox::LaunchSpec {
                program_sha256: None,
                assets: vec![],
                program: std::env::current_exe()?,
                args: vec!["tree-detached".into()],
                env: Default::default(),
                resources: Default::default(),
                linux_cgroup_parent: Some(args[1].clone().into()),
            };
            // Return the handle from a thread that then exits: monitoring the
            // spawning thread with PDEATHSIG would incorrectly kill this tree.
            let mut child = std::thread::spawn(move || fliuno_sandbox::spawn(&spec))
                .join()
                .unwrap()?;
            writeln!(
                io::stdout(),
                "private_directory={}",
                child.private_directory().display()
            )?;
            println!("spawner-exited");
            io::stdout().flush()?;
            let mut input = child.stdin.take().unwrap();
            std::thread::spawn(move || io::copy(&mut io::stdin(), &mut input));
            io::copy(&mut child.stdout.take().unwrap(), &mut io::stdout())?;
            child.kill()?;
        }
        Some("isolated-boundaries") => {
            let mut child = fliuno_sandbox::spawn(&fliuno_sandbox::LaunchSpec {
                program_sha256: None,
                assets: vec![],
                program: std::env::current_exe()?,
                args: std::iter::once("boundaries".to_owned())
                    .chain(args[1..].iter().cloned())
                    .collect(),
                env: Default::default(),
                resources: Default::default(),
                linux_cgroup_parent: None,
            })?;
            drop(child.stdin.take());
            let mut errors = child.stderr.take().unwrap();
            let drain = std::thread::spawn(move || io::copy(&mut errors, &mut io::sink()));
            io::copy(&mut child.stdout.take().unwrap(), &mut io::stdout())?;
            child.kill()?;
            drain.join().unwrap()?;
        }
        Some("echo") => {
            let mut bytes = Vec::new();
            io::stdin().read_to_end(&mut bytes)?;
            io::stdout().write_all(&bytes)?;
        }
        Some("boundaries") => {
            let outside = Path::new(&args[1]);
            let inside_write = std::fs::write("inside.txt", "sandbox output").is_ok();
            let report = format!(
                "container={}\nisolated={}\nread={}\nwrite={}\nnetwork={}\nenv={}\ninside_write={}\n",
                fliuno_sandbox::current_process_is_app_container().unwrap_or(false),
                fliuno_sandbox::current_process_is_isolated().unwrap_or(false),
                std::fs::read(outside).is_ok(),
                std::fs::write(outside, "modified").is_ok(),
                TcpStream::connect_timeout(&args[2].parse().unwrap(), Duration::from_millis(500))
                    .is_ok(),
                std::env::var_os("FLIUNO_PROBE_SECRET").is_some(),
                inside_write
            );
            io::stdout().write_all(report.as_bytes())?;
        }
        Some("args") => {
            for arg in &args[1..] {
                writeln!(io::stdout(), "{}:{arg}", arg.len())?;
            }
        }
        Some("wait") => {
            println!("ready");
            io::stdout().flush()?;
            std::thread::sleep(Duration::from_secs(60));
        }
        Some("wait-detached") => {
            let mut start = [0u8; 1];
            io::stdin().read_exact(&mut start)?;
            if start != *b"x" {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "invalid detached probe start byte",
                ));
            }
            #[cfg(target_os = "linux")]
            linux_probe::start_new_session()?;
            println!("detached-ready");
            io::stdout().flush()?;
            let mut stop = [0u8; 1];
            io::stdin().read_exact(&mut stop)?;
            if stop == *b"q" {
                println!("detached-exit");
                io::stdout().flush()?;
            }
        }
        Some("tree") => {
            let mut descendant = std::process::Command::new(std::env::current_exe()?)
                .arg("wait")
                .spawn()?;
            println!("tree");
            io::stdout().flush()?;
            std::thread::sleep(Duration::from_secs(60));
            descendant.kill()?;
            descendant.wait()?;
        }
        Some("process-limit") => {
            match std::process::Command::new(std::env::current_exe()?)
                .arg("wait")
                .spawn()
            {
                Ok(mut child) => {
                    println!("spawned");
                    child.kill()?;
                    child.wait()?;
                }
                Err(_) => println!("spawn-blocked"),
            }
        }
        #[cfg(windows)]
        Some("memory-limit") => {
            let mut buffer = Vec::<u8>::new();
            let allocated = buffer.try_reserve_exact(96 * 1024 * 1024).is_ok();
            std::hint::black_box(buffer);
            println!(
                "{}",
                if allocated {
                    "allocated"
                } else {
                    "allocation-blocked"
                }
            );
        }
        Some("tree-detached") => {
            let descendant = std::process::Command::new(std::env::current_exe()?)
                .arg("wait-detached")
                .spawn()?;
            println!("tree");
            println!("descendant={}", descendant.id());
            io::stdout().flush()?;
            std::thread::sleep(Duration::from_secs(60));
        }
        _ => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "unknown probe mode",
            ));
        }
    }
    Ok(())
}
