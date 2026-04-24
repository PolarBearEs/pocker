use std::collections::HashMap;
use std::ffi::OsString;
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use base64::Engine;
use serde::Deserialize;

use crate::error::{DockerPullError, Result};

#[derive(Debug, Clone)]
pub enum Credentials {
    Basic { username: String, password: String },
}

#[derive(Debug)]
pub struct AuthResolver {
    cli_credentials: Option<Credentials>,
    docker_config: Option<DockerConfig>,
}

#[derive(Debug, Deserialize)]
struct DockerConfig {
    #[serde(default)]
    auths: HashMap<String, DockerAuthEntry>,
    #[serde(default, rename = "credHelpers")]
    cred_helpers: HashMap<String, String>,
    #[serde(default, rename = "credsStore")]
    creds_store: Option<String>,
}

#[derive(Debug, Deserialize)]
struct DockerAuthEntry {
    auth: Option<String>,
}

#[derive(Debug, Deserialize)]
struct HelperResponse {
    #[serde(rename = "Username")]
    username: String,
    #[serde(rename = "Secret")]
    secret: String,
}

impl AuthResolver {
    pub fn new(cli_credentials: Option<Credentials>) -> Result<Self> {
        Ok(Self {
            cli_credentials,
            docker_config: load_docker_config()?,
        })
    }

    pub fn resolve(&self, registry: &str) -> Result<Option<Credentials>> {
        if let Some(credentials) = &self.cli_credentials {
            return Ok(Some(credentials.clone()));
        }

        let Some(config) = &self.docker_config else {
            return Ok(None);
        };

        if let Some(helper) = config.cred_helpers.get(registry).or_else(|| {
            docker_hub_aliases(registry).find_map(|alias| config.cred_helpers.get(alias))
        }) && let Some(credentials) = invoke_helper(helper, registry)?
        {
            return Ok(Some(credentials));
        }

        if let Some(helper) = &config.creds_store
            && let Some(credentials) = invoke_helper(helper, registry)?
        {
            return Ok(Some(credentials));
        }

        for key in registry_keys(registry) {
            if let Some(entry) = config.auths.get(&key)
                && let Some(auth) = &entry.auth
            {
                let decoded = base64::engine::general_purpose::STANDARD.decode(auth)?;
                let value = String::from_utf8(decoded).map_err(|error| {
                    DockerPullError::InvalidInput(format!("invalid docker auth entry: {error}"))
                })?;
                let (username, password) = value.split_once(':').ok_or_else(|| {
                    DockerPullError::InvalidInput("docker auth entry is missing separator".into())
                })?;
                return Ok(Some(Credentials::Basic {
                    username: username.to_string(),
                    password: password.to_string(),
                }));
            }
        }

        Ok(None)
    }
}

fn load_docker_config() -> Result<Option<DockerConfig>> {
    let path = docker_config_path();
    if !path.exists() {
        return Ok(None);
    }

    let content = std::fs::read_to_string(path)?;
    Ok(Some(serde_json::from_str(&content)?))
}

fn docker_config_path() -> PathBuf {
    std::env::var_os("DOCKER_CONFIG")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            directories::BaseDirs::new()
                .map(|dirs| dirs.home_dir().join(".docker"))
                .unwrap_or_else(|| PathBuf::from(".docker"))
        })
        .join("config.json")
}

fn invoke_helper(helper: &str, registry: &str) -> Result<Option<Credentials>> {
    let helper_binary: OsString = format!("docker-credential-{helper}").into();
    let mut child = match Command::new(&helper_binary)
        .arg("get")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(child) => child,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };

    if let Some(stdin) = child.stdin.as_mut() {
        stdin.write_all(registry.as_bytes())?;
    }
    let output = child.wait_with_output()?;
    if !output.status.success() {
        return Ok(None);
    }

    let response: HelperResponse = serde_json::from_slice(&output.stdout)?;
    Ok(Some(Credentials::Basic {
        username: response.username,
        password: response.secret,
    }))
}

fn registry_keys(registry: &str) -> Vec<String> {
    let mut keys = docker_hub_aliases(registry)
        .map(ToString::to_string)
        .collect::<Vec<_>>();
    keys.push(registry.to_string());
    keys
}

fn docker_hub_aliases(registry: &str) -> impl Iterator<Item = &'static str> {
    let is_hub = matches!(
        registry,
        "registry-1.docker.io" | "docker.io" | "index.docker.io"
    );
    let aliases: &'static [&'static str] = if is_hub {
        &[
            "registry-1.docker.io",
            "https://registry-1.docker.io",
            "index.docker.io",
            "https://index.docker.io/v1/",
            "docker.io",
        ]
    } else {
        &[]
    };
    aliases.iter().copied()
}
