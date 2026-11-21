//! Explicit process fixture. No model or environment-controlled execution.
use std::{
    io::{self, Read, Write},
    time::Duration,
};
pub fn main() -> io::Result<()> {
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    match args.first().map(String::as_str) {
        Some("skill-script") => {
            assert!(fliuno_sandbox::current_process_is_isolated()?);
            assert!(std::env::var_os("FLIUNO_EXEC_SECRET").is_none());
            let script = std::fs::read_to_string(&args[1])?;
            let mut input = String::new();
            io::stdin().read_to_string(&mut input)?;
            match input.as_str() {
                value if value == "hold" || value.starts_with("hold:") => {
                    println!("skill-ready");
                    io::stdout().flush()?;
                    if let Some(token) = value.strip_prefix("hold:") {
                        let token = uuid::Uuid::parse_str(token).map_err(io::Error::other)?;
                        std::fs::write(format!("ready-{}", token.simple()), b"running")?;
                    }
                    std::thread::sleep(Duration::from_secs(60));
                }
                "flood" => io::stdout().write_all(&[b'x'; 2048])?,
                _ if script.trim() == "upper" => print!("{}", input.to_uppercase()),
                _ if script.trim() == "reverse" => {
                    print!("{}", input.chars().rev().collect::<String>())
                }
                _ => return Err(io::Error::other("unsupported Skill fixture script")),
            }
        }
        Some("asset") => {
            println!(
                "{}",
                serde_json::json!({"asset":std::fs::read_to_string(&args[1])?, "isolated":fliuno_sandbox::current_process_is_isolated().unwrap_or(false)})
            );
        }
        Some("echo") => {
            let mut input = String::new();
            io::stdin().read_to_string(&mut input)?;
            println!(
                "{}",
                serde_json::json!({"args":&args[1..],"stdin":input,"cwd":std::env::current_dir()?,"container":fliuno_sandbox::current_process_is_app_container().unwrap_or(false),"secret":std::env::var_os("FLIUNO_EXEC_SECRET").is_some()})
            );
            eprintln!("stderr:中文");
        }
        Some("child-status") => {
            let child = std::process::Command::new(std::env::current_exe()?)
                .arg("child-exit")
                .status();
            println!(
                "{}",
                serde_json::json!({"spawned": child.is_ok_and(|status| status.success())})
            );
        }
        Some("child-exit") => {}
        Some("exit") => {
            println!("known failure");
            std::process::exit(7);
        }
        Some("flood") => {
            for _ in 0..128 {
                io::stdout().write_all(&[b'o'; 8192])?;
                io::stderr().write_all(&[b'e'; 8192])?;
            }
            println!("tail-你好");
        }
        Some("binary") => {
            io::stdout().write_all(&[0, 255, 254, 10])?;
        }
        Some("hang") => {
            println!("ready");
            io::stdout().flush()?;
            std::thread::sleep(Duration::from_secs(60));
        }
        Some("acp-terminal") => {
            assert!(fliuno_sandbox::current_process_is_isolated()?);
            if args.get(1).is_some_and(|arg| arg == "hold") {
                println!("pid:{}", std::process::id());
                io::stdout().flush()?;
                std::thread::sleep(Duration::from_secs(60));
            } else {
                print!("{}last", "\u{754c}".repeat(128));
                io::stdout().flush()?;
            }
        }
        Some("hang-file") => {
            println!("ready");
            io::stdout().flush()?;
            // The parent observes this marker only after output is in the pipe.
            std::fs::write(&args[1], "ready")?;
            std::thread::sleep(Duration::from_secs(60));
        }
        Some("ignore-input") => {
            println!("not reading stdin");
            io::stdout().flush()?;
            std::thread::sleep(Duration::from_secs(60));
        }
        Some("touch") => {
            std::fs::write(&args[1], "executed")?;
            println!("touched");
        }
        Some("run-handoff") => {
            let mut starts = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open("handoff.starts")?;
            writeln!(starts, "started")?;
            let deadline = std::time::Instant::now() + Duration::from_secs(20);
            while !std::path::Path::new("handoff.release").exists() {
                if std::time::Instant::now() >= deadline {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "fixture release missing",
                    ));
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            std::fs::write("handoff.completed", "completed")?;
            println!("exactly once");
        }
        Some("tree") | Some("orphan") | Some("tree-hold") => {
            let mut child = std::process::Command::new(std::env::current_exe()?)
                .arg(if args[0] == "tree-hold" {
                    "hold-child"
                } else {
                    "delayed-touch"
                })
                .arg(&args[1])
                .spawn()?;
            println!("descendant:{}", child.id());
            io::stdout().flush()?;
            if args[0] != "orphan" {
                std::thread::sleep(Duration::from_secs(60));
                child.kill()?;
                child.wait()?;
            }
        }
        Some("hold-child") => {
            println!("descendant-ready");
            io::stdout().flush()?;
            std::thread::sleep(Duration::from_secs(60));
        }
        Some("delayed-touch") => {
            std::thread::sleep(Duration::from_millis(800));
            std::fs::write(&args[1], "descendant survived")?;
        }
        _ => return Err(io::Error::new(io::ErrorKind::InvalidInput, "unknown probe")),
    }
    Ok(())
}
