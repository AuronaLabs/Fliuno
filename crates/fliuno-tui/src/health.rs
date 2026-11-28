use crate::options::Options;
use fliuno_client::ServerInfo;
use std::{fmt::Write, fs, io::Read, path::Path};

pub struct Report {
    pub text: String,
    pub healthy: bool,
}

impl Report {
    fn check(&mut self, name: &str, value: &str, error: bool) {
        let _ = writeln!(self.text, "{name}: {value}");
        self.healthy &= !error;
    }
}

pub fn inspect(
    options: &Options,
    service: Option<&ServerInfo>,
    selected_model: Option<&fliuno_protocol::ModelProfileEntry>,
) -> Report {
    let mut report = Report {
        text: String::new(),
        healthy: true,
    };
    if let Some(service) = service {
        let capabilities = &service.capabilities;
        report.check(
            "Service",
            &format!(
                "Connected · {} {} · protocol {}",
                service.server_name, service.server_version, service.protocol_version
            ),
            false,
        );
        if let Some(model) = selected_model {
            report.check(
                "Active model",
                &format!("{} / {}", model.provider, model.model),
                false,
            );
            report.check(
                "Active profile",
                model.profile.as_deref().unwrap_or("[model]"),
                false,
            );
            report.check(
                "Active reasoning effort",
                model
                    .reasoning_effort
                    .as_deref()
                    .unwrap_or("Provider default"),
                false,
            );
        } else if let Some(provider) = capabilities
            .get("provider")
            .and_then(serde_json::Value::as_str)
        {
            report.check("Active provider", provider, false);
        }
        let enabled = [
            ("externalTools", "external tools"),
            ("externalAgents", "external agents"),
            ("terminalSessions", "persistent terminals"),
            ("taskMemoryHybridRetrieval", "semantic memory"),
            ("modelProgressNotifications", "model progress"),
        ]
        .into_iter()
        .filter_map(|(key, label)| {
            (capabilities.get(key).and_then(serde_json::Value::as_bool) == Some(true))
                .then_some(label)
        })
        .collect::<Vec<_>>();
        if !enabled.is_empty() {
            report.check("Available", &enabled.join(", "), false);
        }
    }
    report.check(
        "Config",
        &options.config.as_ref().map_or_else(
            || {
                if options.force_demo {
                    "Built-in offline demo".into()
                } else {
                    "No model profile. Run fliuno in a terminal to configure it.".into()
                }
            },
            |path| format!("{}", path.display()),
        ),
        false,
    );
    if options.config.is_none() && !options.force_demo {
        report.healthy = false;
    } else if options.demo {
        report.check("Model", "Offline demo", false);
    } else if let Ok(model) = options
        .settings
        .model_for_launch(options.model_profile.as_deref())
    {
        if let Some(profile) = options.model_profile.as_deref().or(options
            .settings
            .workbench
            .model_profile
            .as_deref())
        {
            report.check(
                if selected_model.is_some() {
                    "Launch profile"
                } else {
                    "Model profile"
                },
                profile,
                false,
            );
        }
        report.check(
            if selected_model.is_some() {
                "Launch model"
            } else {
                "Model"
            },
            &format!(
                "{} / {}",
                model.runtime_provider_name(),
                model.name.as_deref().unwrap_or("unnamed")
            ),
            false,
        );
        if model
            .name
            .as_deref()
            .is_none_or(|name| name.trim().is_empty())
        {
            report.check("Model settings", "[model].name is required", true);
        }
        if let Err(error) = model.effective_protocol() {
            report.check("Model protocol", error, true);
        }
        if let Some(value) = &model.base_url {
            match fliuno_config::provider_api_root(value) {
                Ok(base) => report.check("Base URL", base.as_str(), false),
                Err(error) => report.check("Base URL", error, true),
            }
        }
        if let Some(effort) = model.reasoning_effort {
            match model.validate_reasoning_effort() {
                Ok(()) => report.check(
                    if selected_model.is_some() {
                        "Launch reasoning effort"
                    } else {
                        "Reasoning effort"
                    },
                    effort.as_str(),
                    false,
                ),
                Err(error) => report.check("Reasoning effort", &error, true),
            }
        }
        match model.read_model_key() {
            Ok(None) => report.check("Credentials", "Keyless loopback connection", false),
            Ok(Some(_)) if model.api_key_file.is_some() => {
                report.check("Credentials", "Configured key file is readable", false);
            }
            Ok(_) => report.check(
                "Credentials",
                "Windows Credential Manager entry is readable",
                false,
            ),
            Err(error) => report.check("Credentials", error, true),
        }
        if let Err(error) = model.validate_budget() {
            report.check("Model budget", error, true);
        }
    }
    path_check(&mut report, "App-server", &options.server, executable);
    path_check(&mut report, "Workspace", &options.workspace, |path| {
        path.metadata().is_ok_and(|entry| entry.is_dir())
    });
    match database(&options.database) {
        Ok(state) => report.check(
            "Database",
            &format!("{} · {state}", options.database.display()),
            false,
        ),
        Err(error) => report.check(
            "Database",
            &format!("{} · {error}", options.database.display()),
            true,
        ),
    }
    if !options.force_demo {
        let integrations = &options.settings.integrations;
        for (name, path) in [
            ("MCP", &integrations.mcp),
            ("MCP OAuth", &integrations.mcp_oauth),
            ("ACP", &integrations.acp),
            ("A2A", &integrations.a2a),
            ("Execution", &integrations.execution),
            ("Terminal", &integrations.terminal),
            ("Memory embeddings", &integrations.memory_embedding),
        ] {
            if let Some(path) = path {
                path_check(&mut report, name, path, |path| {
                    path.metadata().is_ok_and(|entry| entry.is_file())
                });
            }
        }
        if let Some(path) = &integrations.skills {
            path_check(&mut report, "Skills", path, |path| {
                path.metadata().is_ok_and(|entry| entry.is_dir())
            });
        }
    }
    report
}

fn path_check(report: &mut Report, name: &str, path: &Path, check: impl FnOnce(&Path) -> bool) {
    let ready = check(path);
    let state = if ready {
        "available"
    } else {
        "missing or invalid"
    };
    report.check(name, &format!("{} · {state}", path.display()), !ready);
}

fn executable(path: &Path) -> bool {
    let Ok(metadata) = path.metadata() else {
        return false;
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o111 == 0 {
            return false;
        }
    }
    metadata.is_file()
}

fn database(path: &Path) -> Result<&'static str, String> {
    match path.metadata() {
        Ok(entry) if entry.is_file() && entry.len() == 0 => Ok("empty file"),
        Ok(entry) if entry.is_file() => {
            let mut header = [0; 16];
            fs::File::open(path)
                .and_then(|mut file| file.read_exact(&mut header))
                .map_err(|error| error.to_string())?;
            if &header != b"SQLite format 3\0" {
                return Err("expected a SQLite database".into());
            }
            Ok("SQLite header verified")
        }
        Ok(_) => Err("expected a database file".into()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let parent = path
                .ancestors()
                .skip(1)
                .find(|part| !part.as_os_str().is_empty() && part.exists());
            if parent.is_some_and(|path| !path.is_dir()) {
                return Err("parent path is a file".into());
            }
            Ok("created when the service starts")
        }
        Err(error) => Err(error.to_string()),
    }
}
