pub use fliuno_config::Theme;
use fliuno_config::{Provider, Settings, user_directory};
use std::{
    ffi::OsString,
    path::{Path, PathBuf},
};

pub const HELP: &str = "Fliuno - local AI workbench

Usage: fliuno [--config PATH] [--demo] [--workspace PATH] [--db PATH] [--server PATH] [-- SERVER_ARGS...]

  --config PATH      Settings file (default: ~/.fliuno/config.toml)
  --init-config      Create the settings template and exit
  --demo             Use the offline demonstration provider
  --workspace PATH   Folder to open (default: current directory)
  --db PATH          Workbench database (default: ~/.fliuno/state/workbench.db)
  --server PATH      App-server executable (default: next to fliuno)
  --theme NAME       auto, dark, light, or mono
  --help             Print help
  --version          Print version";

#[derive(Debug, Clone)]
pub struct Options {
    pub demo: bool,
    pub force_demo: bool,
    pub config: Option<PathBuf>,
    pub workspace: PathBuf,
    pub database: PathBuf,
    pub server: PathBuf,
    pub server_args: Vec<OsString>,
    pub theme: Theme,
}

pub enum Entry {
    Help,
    Version,
    InitConfig(PathBuf),
    Run(Options),
}

impl Options {
    pub fn parse(mut args: impl Iterator<Item = OsString>) -> Result<Entry, String> {
        let (mut force_demo, mut init, mut workspace, mut database, mut server) =
            (false, false, None, None, None);
        let mut config = None;
        let mut server_args = Vec::new();
        let mut theme = None;
        while let Some(arg) = args.next() {
            match arg.to_str() {
                Some("--help" | "-h") => return Ok(Entry::Help),
                Some("--version" | "-V") => return Ok(Entry::Version),
                Some("--demo") => force_demo = true,
                Some("--init-config") => init = true,
                Some("--workspace") => workspace = Some(read_path(&mut args, "--workspace")?),
                Some("--db") => database = Some(read_path(&mut args, "--db")?),
                Some("--server") => server = Some(read_path(&mut args, "--server")?),
                Some("--config") => config = Some(read_path(&mut args, "--config")?),
                Some("--theme") => {
                    theme = Some(match args.next().as_deref().and_then(|arg| arg.to_str()) {
                        Some("auto") => Theme::Auto,
                        Some("dark") => Theme::Dark,
                        Some("light") => Theme::Light,
                        Some("mono") => Theme::Mono,
                        _ => return Err("--theme requires auto, dark, light, or mono".into()),
                    });
                }
                Some("--") => {
                    server_args.extend(args);
                    break;
                }
                _ => return Err(format!("unknown argument: {}", arg.to_string_lossy())),
            }
        }
        let directory = user_directory();
        let default_config = directory.as_ref().map(|dir| dir.join("config.toml"));
        if init {
            return config
                .or(default_config)
                .map(Entry::InitConfig)
                .ok_or_else(|| "Pass --config to choose a settings file".into());
        }
        if config.is_none()
            && let Some(path) = default_config
            && path
                .try_exists()
                .map_err(|error| format!("Cannot inspect {}: {error}", path.display()))?
        {
            config = Some(path);
        }
        let settings = config
            .as_deref()
            .map(Settings::load)
            .transpose()?
            .unwrap_or_default();
        let demo = force_demo || matches!(settings.model.provider, Provider::Demo);
        let server = match server.or(settings.workbench.server) {
            Some(path) => path,
            None => std::env::current_exe()
                .map_err(|error| error.to_string())?
                .with_file_name(format!("fliuno-app-server{}", std::env::consts::EXE_SUFFIX)),
        };
        let workspace = match workspace {
            Some(path) => path,
            None => std::env::current_dir().map_err(|error| error.to_string())?,
        };
        let database = if let Some(path) = database.or_else(|| {
            (!force_demo)
                .then_some(settings.workbench.database)
                .flatten()
        }) {
            path
        } else {
            default_database(directory.as_deref(), force_demo)?
        };
        if server_args
            .iter()
            .any(|arg| matches!(arg.to_str(), Some("--db" | "--config" | "--demo")))
        {
            return Err("Pass --db, --config, and --demo before --".into());
        }
        Ok(Entry::Run(Self {
            demo,
            force_demo: force_demo || config.is_none(),
            config,
            workspace,
            database,
            server,
            server_args,
            theme: theme.unwrap_or(settings.workbench.theme),
        }))
    }
}

fn read_path(args: &mut impl Iterator<Item = OsString>, flag: &str) -> Result<PathBuf, String> {
    args.next()
        .filter(|item| !item.is_empty())
        .map(PathBuf::from)
        .ok_or_else(|| format!("{flag} requires a path"))
}

fn default_database(directory: Option<&Path>, demo: bool) -> Result<PathBuf, String> {
    let name = if demo { "demo.db" } else { "workbench.db" };
    let preferred = directory.map(|dir| dir.join("state").join(name));
    if let Some(path) = &preferred
        && path.try_exists().map_err(|error| error.to_string())?
    {
        return Ok(path.clone());
    }
    // Keep opening an existing pre-0.0.12 database until the user explicitly switches paths.
    if let Some(path) = legacy_database(name)
        && path.try_exists().map_err(|error| error.to_string())?
    {
        return Ok(path);
    }
    preferred.ok_or_else(|| "Pass --db to choose a database path".into())
}

fn legacy_database(name: &str) -> Option<PathBuf> {
    let state = if cfg!(windows) {
        std::env::var_os("LOCALAPPDATA").map(PathBuf::from)
    } else {
        std::env::var_os("XDG_STATE_HOME")
            .map(PathBuf::from)
            .filter(|path| path.is_absolute())
            .or_else(|| std::env::home_dir().map(|path| path.join(".local/state")))
    };
    state.map(|path| path.join("fliuno").join(name))
}
