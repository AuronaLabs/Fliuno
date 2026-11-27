//! Resolve environment and credentials once. The exact frozen values used by
//! the transport also produce its approval identity; raw values are not stored.
use crate::{ProcessIsolation, ServerConfig, StdioServerConfig};
use reqwest::{Url, header::HeaderMap};
use serde_json::json;
use std::collections::BTreeMap;

pub(crate) enum Resolved {
    Stdio {
        environment: BTreeMap<String, String>,
        executable_digest: String,
    },
    Http {
        url: Url,
        headers: HeaderMap,
    },
}
fn environment(
    config: &StdioServerConfig,
    resolve: &mut impl FnMut(&str) -> Result<Option<String>, String>,
) -> Result<BTreeMap<String, String>, String> {
    let mut names: Vec<&str> = if config.isolation == ProcessIsolation::TrustedHost {
        vec![
            "PATH",
            "SystemRoot",
            "WINDIR",
            "TEMP",
            "TMP",
            "HOME",
            "USERPROFILE",
        ]
    } else {
        vec![]
    };
    names.extend(config.env_allow.iter().map(String::as_str));
    if cfg!(windows) {
        names.push("SystemRoot");
    }
    let mut env = BTreeMap::new();
    let mut resolved_names = std::collections::BTreeSet::new();
    for name in names {
        let key = if cfg!(windows) {
            name.to_ascii_uppercase()
        } else {
            name.to_owned()
        };
        if !resolved_names.insert(key.clone()) {
            continue;
        }
        if let Some(value) = resolve(name)? {
            env.insert(key, value);
        }
    }
    // The Windows sandbox fills an absent SystemRoot at launch. Registered
    // adapters must freeze it now, so that fallback cannot add fresh authority.
    if cfg!(windows) && !env.contains_key("SYSTEMROOT") {
        return Err("MCP SystemRoot must be available and Unicode at registration".into());
    }
    Ok(env)
}
pub(crate) fn resolve(config: &ServerConfig) -> Result<(Resolved, String), String> {
    resolve_with(config, |name| match std::env::var(name) {
        Ok(value) => Ok(Some(value)),
        Err(std::env::VarError::NotPresent) => Ok(None),
        Err(_) => Err("MCP inherited environment is not Unicode".into()),
    })
}
fn resolve_with(
    config: &ServerConfig,
    mut lookup: impl FnMut(&str) -> Result<Option<String>, String>,
) -> Result<(Resolved, String), String> {
    let (resolved, details) = match config {
        ServerConfig::Stdio(config) => {
            let environment = environment(config, &mut lookup)?;
            let executable_digest = fliuno_sandbox::program_digest(&config.command)
                .map_err(|_| "cannot hash MCP executable")?;
            let details = json!({"environment":environment,"executableSha256":executable_digest});
            (
                Resolved::Stdio {
                    environment,
                    executable_digest,
                },
                details,
            )
        }
        ServerConfig::Http(config) => {
            let url = crate::http::validate(config)?;
            let headers = crate::http::headers(config, |name| lookup(name).ok().flatten())?;
            let credentials: BTreeMap<_, _> = headers
                .iter()
                .map(|(name, value)| (name.as_str(), value.as_bytes()))
                .collect();
            let details = json!({"credentials":credentials});
            (Resolved::Http { url, headers }, details)
        }
    };
    let authority = fliuno_core::authority_digest(
        "mcp.registration.v1",
        &json!({"configuration":config,"resolved":details}),
    );
    Ok((resolved, authority))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn credentials_are_frozen_and_rotation_changes_the_registration_identity() {
        let config = serde_json::from_value(json!({"id":"test","transport":"streamable_http",
            "url":"http://127.0.0.1:1/mcp","bearerTokenEnv":"TOKEN"}))
        .unwrap();
        let (first, identity) = resolve_with(&config, |name| {
            Ok((name == "TOKEN").then(|| "secret-one".into()))
        })
        .unwrap();
        let (_, changed) = resolve_with(&config, |name| {
            Ok((name == "TOKEN").then(|| "secret-two".into()))
        })
        .unwrap();
        assert_ne!(identity, changed);
        let Resolved::Http { headers, .. } = first else {
            panic!("wrong transport")
        };
        assert_eq!(headers[reqwest::header::AUTHORIZATION], "Bearer secret-one");
        assert!(headers[reqwest::header::AUTHORIZATION].is_sensitive());
        assert!(identity.bytes().all(|byte| byte.is_ascii_hexdigit()));
    }
    #[test]
    fn resolved_stdio_environment_changes_binding_and_is_used_without_a_second_lookup() {
        // Resolution hashes bytes without launching them. The test runner can exceed
        // the production executable limit when linked with all Linux features.
        let fixture = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml");
        let config = serde_json::from_value(json!({"id":"test","command":fixture,
            "isolation":"trusted_host","cwd":std::env::temp_dir(),"envAllow":["RUN_MODE"]}))
        .unwrap();
        let (first, identity) = resolve_with(&config, |name| {
            Ok(match name {
                "RUN_MODE" => Some("old-value".into()),
                "SystemRoot" => Some("C:\\Windows".into()),
                _ => None,
            })
        })
        .unwrap();
        let (_, changed) = resolve_with(&config, |name| {
            Ok(match name {
                "RUN_MODE" => Some("new-value".into()),
                "SystemRoot" => Some("C:\\Windows".into()),
                _ => None,
            })
        })
        .unwrap();
        assert_ne!(identity, changed);
        let Resolved::Stdio {
            environment,
            executable_digest,
        } = first
        else {
            panic!("wrong transport")
        };
        assert_eq!(environment["RUN_MODE"], "old-value");
        assert_eq!(
            executable_digest,
            fliuno_sandbox::program_digest(&fixture).unwrap()
        );
    }

    #[cfg(windows)]
    #[test]
    fn missing_system_root_cannot_be_inherited_later_without_a_binding() {
        let config: ServerConfig = serde_json::from_value(json!({
            "id":"test", "command":std::env::current_exe().unwrap(),
            "isolation":"windows_app_container"
        }))
        .unwrap();
        let ServerConfig::Stdio(config) = config else {
            panic!("wrong transport")
        };
        assert!(
            environment(&config, &mut |_| Ok(None))
                .unwrap_err()
                .contains("SystemRoot")
        );
    }
}
