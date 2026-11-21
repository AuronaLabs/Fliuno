use fliuno_core::{BackupManifest, BackupProgress, BackupRetention, BackupSource, Error, Kernel};
use serde::Serialize;
use std::{
    ffi::{OsStr, OsString},
    io::{self, Write},
    path::Path,
    time::{Duration, Instant},
};

pub const USAGE: &str = "fliuno-app-server backup create DATABASE NEW_BACKUP_DIR [--progress] [--timeout-ms N]\n\
    fliuno-app-server backup list BACKUP_ROOT\n\
    fliuno-app-server backup rotate DATABASE BACKUP_ROOT KEEP_COUNT MAX_BYTES [--max-age-ms N] [--progress] [--timeout-ms N]\n\
    fliuno-app-server backup verify BACKUP_DIR\n\
    fliuno-app-server backup restore BACKUP_DIR NEW_DATABASE\n\
    fliuno-app-server backup verify-restored BACKUP_DIR DATABASE";

#[derive(Serialize)]
struct Report {
    operation: &'static str,
    manifest: BackupManifest,
}

pub fn run(mut args: impl Iterator<Item = OsString>) -> Result<(), Box<dyn std::error::Error>> {
    let command = args.next();
    let command = command.as_deref().and_then(OsStr::to_str);
    match command {
        Some("create") => return create("create", args),
        Some("rotate") => return create("rotate", args),
        _ => {}
    }
    let directory = args.next();
    let destination = args.next();
    if args.next().is_some() {
        return Err(USAGE.into());
    }
    let (operation, manifest) = match (
        command,
        directory.as_deref().map(Path::new),
        destination.as_deref().map(Path::new),
    ) {
        (Some("--help" | "-h"), None, None) => {
            writeln!(io::stdout().lock(), "{USAGE}")?;
            return Ok(());
        }
        (Some("verify"), Some(directory), None) => ("verify", Kernel::verify_backup(directory)?),
        (Some("list"), Some(directory), None) => {
            return stdout(&serde_json::json!({
                "operation": "list", "backups": BackupSource::list(directory)?,
            }));
        }
        (Some("restore"), Some(directory), Some(destination)) => {
            ("restore", Kernel::restore_backup(directory, destination)?)
        }
        (Some("verify-restored"), Some(directory), Some(destination)) => {
            let manifest = Kernel::verify_backup(directory)?;
            Kernel::verify_restored_database(destination, &manifest)?;
            ("verify-restored", manifest)
        }
        _ => return Err(USAGE.into()),
    };
    stdout(&Report {
        operation,
        manifest,
    })
}

fn create(
    operation: &'static str,
    mut args: impl Iterator<Item = OsString>,
) -> Result<(), Box<dyn std::error::Error>> {
    let database = args.next().ok_or(USAGE)?;
    let target = args.next().ok_or(USAGE)?;
    let mut policy = if operation == "rotate" {
        Some(BackupRetention {
            keep_count: number(args.next(), "keep count")?,
            max_total_bytes: number(args.next(), "max bytes")?,
            max_age_ms: None,
        })
    } else {
        None
    };
    let mut show_progress = false;
    let mut timeout = None;
    while let Some(flag) = args.next() {
        match flag.to_str() {
            Some("--progress") if !show_progress => show_progress = true,
            Some("--timeout-ms") if timeout.is_none() => {
                timeout = Some(Duration::from_millis(number(args.next(), "timeout")?));
            }
            Some("--max-age-ms") => {
                let policy = policy
                    .as_mut()
                    .filter(|p| p.max_age_ms.is_none())
                    .ok_or(USAGE)?;
                policy.max_age_ms = Some(number(args.next(), "max age")?);
            }
            _ => return Err(USAGE.into()),
        }
    }
    let started = Instant::now();
    let mut last_report = None::<Instant>;
    let progress = |event| {
        if timeout.is_some_and(|timeout| started.elapsed() >= timeout) {
            return Err(Error::Conflict(
                "backup timed out before publication".into(),
            ));
        }
        let copy = matches!(event, BackupProgress::Copying { .. });
        if show_progress
            && (!copy
                || last_report
                    .is_none_or(|previous| previous.elapsed() >= Duration::from_millis(250)))
        {
            let mut err = io::stderr().lock();
            serde_json::to_writer(&mut err, &event)?;
            writeln!(err)?;
            last_report = Some(Instant::now());
        }
        Ok(())
    };
    let source = BackupSource::open(Path::new(&database))?;
    if let Some(policy) = policy {
        let result = source.rotate(Path::new(&target), policy, progress)?;
        stdout(&serde_json::json!({ "operation": operation, "rotation": result }))
    } else {
        let manifest = source.create(Path::new(&target), progress)?;
        stdout(&Report {
            operation,
            manifest,
        })
    }
}

fn number<T: std::str::FromStr>(arg: Option<OsString>, label: &str) -> Result<T, String> {
    arg.as_deref()
        .and_then(OsStr::to_str)
        .and_then(|value| value.parse().ok())
        .ok_or_else(|| format!("{label} must be an unsigned integer"))
}

fn stdout(value: &impl Serialize) -> Result<(), Box<dyn std::error::Error>> {
    let mut out = io::stdout().lock();
    serde_json::to_writer(&mut out, value)?;
    writeln!(out)?;
    Ok(())
}
