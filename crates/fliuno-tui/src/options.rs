use std::{ffi::OsString, path::PathBuf};

pub const HELP: &str = "Fliuno - local AI workbench

Usage: fliuno [--demo] [--workspace PATH] [--db PATH] [--server PATH] [-- SERVER_ARGS...]

  --demo             Use the offline demonstration provider
  --workspace PATH   Folder to open (default: current directory)
  --db PATH          Workbench database (default: per-user state directory)
  --server PATH      App-server executable (default: next to fliuno)
  --help             Print help
  --version          Print version";

#[derive(Debug, Clone)]
pub struct Options {
    pub demo: bool,
    pub workspace: PathBuf,
    pub database: PathBuf,
    pub server: PathBuf,
    pub server_args: Vec<OsString>,
}

pub enum Entry {
    Help,
    Version,
    Run(Options),
}

impl Options {
    pub fn parse(mut args: impl Iterator<Item = OsString>) -> Result<Entry, String> {
        let (mut demo, mut workspace, mut database, mut server) = (false, None, None, None);
        let mut server_args = Vec::new();
        while let Some(arg) = args.next() {
            match arg.to_str() {
                Some("--help" | "-h") => return Ok(Entry::Help),
                Some("--version" | "-V") => return Ok(Entry::Version),
                Some("--demo") => demo = true,
                Some("--workspace") => workspace = args.next().map(PathBuf::from),
                Some("--db") => database = args.next().map(PathBuf::from),
                Some("--server") => server = args.next().map(PathBuf::from),
                Some("--") => {
                    server_args.extend(args);
                    break;
                }
                _ => return Err(format!("unknown argument: {}", arg.to_string_lossy())),
            }
            if (arg == "--workspace" && workspace.is_none())
                || (arg == "--db" && database.is_none())
                || (arg == "--server" && server.is_none())
            {
                return Err(format!("{} requires a path", arg.to_string_lossy()));
            }
        }
        let server = match server {
            Some(path) => path,
            None => std::env::current_exe()
                .map_err(|error| error.to_string())?
                .with_file_name(format!("fliuno-app-server{}", std::env::consts::EXE_SUFFIX)),
        };
        let workspace = match workspace {
            Some(path) => path,
            None => std::env::current_dir().map_err(|error| error.to_string())?,
        };
        let database = database.map_or_else(|| default_database(demo), Ok)?;
        if server_args.iter().any(|arg| arg == "--db") {
            return Err("pass --db before -- to select the workbench database".into());
        }
        Ok(Entry::Run(Self {
            demo,
            workspace,
            database,
            server,
            server_args,
        }))
    }
}

fn default_database(demo: bool) -> Result<PathBuf, String> {
    let state = if cfg!(windows) {
        std::env::var_os("LOCALAPPDATA").map(PathBuf::from)
    } else {
        std::env::var_os("XDG_STATE_HOME")
            .map(PathBuf::from)
            .filter(|path| path.is_absolute())
            .or_else(|| {
                std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/state"))
            })
    };
    let name = if demo { "demo.db" } else { "workbench.db" };
    state
        .map(|path| path.join("fliuno").join(name))
        .ok_or_else(|| "set --db to choose a database path".into())
}
