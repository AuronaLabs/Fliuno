pub use fliuno_config::Theme;
use fliuno_config::{Provider, Settings, user_directory};
use std::{
    ffi::OsString,
    path::{Path, PathBuf},
};

pub const HELP: &str = "Fliuno - local AI workbench

Run from a project folder. First launch guides model setup.

Usage: fliuno [--config PATH] [--model-profile ID] [--demo] [--workspace PATH] [--db PATH] [--server PATH] [-- SERVER_ARGS...]

  --config PATH      Settings file (default: ~/.fliuno/config.toml)
  --model-profile ID  Select a named model from the settings file
  --init-config      Create an editable settings template and exit
  --doctor           Check local configuration and paths without starting a service
  --demo             Use the offline demonstration provider
  --workspace PATH   Folder to open (default: current directory)
  --db PATH          Workbench database (default: ~/.fliuno/state/workbench.db)
  --server PATH      App-server executable (default: next to fliuno)
  --theme NAME       auto, dark, light, or mono
  --help             Print help
  --version          Print version";

#[derive(Debug)]
pub struct Options {
    pub demo: bool,
    pub settings: Settings,
    pub force_demo: bool,
    pub config: Option<PathBuf>,
    pub model_profile: Option<String>,
    pub workspace: PathBuf,
    pub database: PathBuf,
    pub server: PathBuf,
    pub server_args: Vec<OsString>,
    pub theme: Theme,
}

pub struct SetupOptions {
    pub config: PathBuf,
    pub server: PathBuf,
    pub theme: Theme,
}

pub enum Entry {
    Help,
    Version,
    InitConfig(PathBuf),
    Setup(SetupOptions),
    Run(Options),
    Doctor(Options),
}

impl Options {
    pub fn parse(mut args: impl Iterator<Item = OsString>) -> Result<Entry, String> {
        let (mut force_demo, mut init, mut workspace, mut database, mut server) =
            (false, false, None, None, None);
        let mut config = None;
        let mut model_profile = None;
        let mut server_args = Vec::new();
        let mut theme = None;
        let mut doctor = false;
        while let Some(arg) = args.next() {
            match arg.to_str() {
                Some("--help" | "-h") => return Ok(Entry::Help),
                Some("--version" | "-V") => return Ok(Entry::Version),
                Some("--demo") => force_demo = true,
                Some("--model-profile") => {
                    model_profile = Some(
                        args.next()
                            .and_then(|arg| arg.into_string().ok())
                            .ok_or("--model-profile requires a profile ID")?,
                    )
                }
                Some("--init-config") => init = true,
                Some("--doctor") => doctor = true,
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
        if model_profile.is_some() && (force_demo || init) {
            return Err("Use --model-profile with an existing live configuration".into());
        }
        if init && doctor {
            return Err("Choose either --doctor or --init-config".into());
        }
        if init {
            return config
                .or(default_config)
                .map(Entry::InitConfig)
                .ok_or_else(|| "Pass --config to choose a settings file".into());
        }
        if server_args.iter().any(|arg| {
            matches!(
                arg.to_str(),
                Some("--db" | "--config" | "--demo" | "--model-profile")
            )
        }) {
            return Err("Pass --db, --config, --demo, and --model-profile before --".into());
        }
        let explicit_config = config.is_some();
        config = config.or(default_config);
        if let Some(path) = config.as_ref()
            && !path
                .try_exists()
                .map_err(|error| format!("Cannot inspect {}: {error}", path.display()))?
        {
            if model_profile.is_some() {
                return Err("--model-profile requires an existing settings file".into());
            }
            if !force_demo && !doctor {
                return Ok(Entry::Setup(SetupOptions {
                    config: path.clone(),
                    server: server_or_default(server)?,
                    theme: theme.unwrap_or_default(),
                }));
            }
            if !explicit_config {
                config = None;
            }
        }
        if config.is_none() && !force_demo && !doctor {
            return Err("Pass --config to choose a model profile".into());
        }
        let mut settings = config
            .as_deref()
            .map(Settings::load)
            .transpose()?
            .unwrap_or_default();
        let demo = force_demo
            || matches!(
                settings
                    .model_for_launch(model_profile.as_deref())?
                    .provider,
                Provider::Demo
            );
        let server = server_or_default(server.or_else(|| settings.workbench.server.take()))?;
        let workspace = match workspace {
            Some(path) => path,
            None => std::env::current_dir().map_err(|error| error.to_string())?,
        };
        let database = if let Some(path) = database.or_else(|| {
            (!force_demo)
                .then(|| settings.workbench.database.take())
                .flatten()
        }) {
            path
        } else {
            default_database(directory.as_deref(), force_demo)?
        };
        let options = Self {
            demo,
            force_demo,
            config,
            model_profile,
            workspace,
            database,
            server,
            server_args,
            theme: theme.unwrap_or(settings.workbench.theme),
            settings,
        };
        Ok(if doctor {
            Entry::Doctor(options)
        } else {
            Entry::Run(options)
        })
    }
}

fn server_or_default(server: Option<PathBuf>) -> Result<PathBuf, String> {
    match server {
        Some(path) => Ok(path),
        None => Ok(std::env::current_exe()
            .map_err(|error| error.to_string())?
            .with_file_name(format!("fliuno-app-server{}", std::env::consts::EXE_SUFFIX))),
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
