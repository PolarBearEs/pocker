use std::collections::HashMap;
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use base64::Engine;
use serde::Deserialize;
use tokio::sync::Mutex;
use tracing::warn;

use crate::error::{DockerPullError, Result};

#[derive(Debug, Clone)]
pub enum Credentials {
    Basic { username: String, password: String },
}

#[derive(Debug)]
pub struct AuthResolver {
    cli_credentials: Option<Credentials>,
    docker_config: Option<DockerConfig>,
    resolved: Mutex<HashMap<String, Option<Credentials>>>,
}

#[derive(Debug, Clone, Deserialize)]
struct DockerConfig {
    #[serde(default)]
    auths: HashMap<String, DockerAuthEntry>,
    #[serde(default, rename = "credHelpers")]
    cred_helpers: HashMap<String, String>,
    #[serde(default, rename = "credsStore")]
    creds_store: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
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
            resolved: Mutex::new(HashMap::new()),
        })
    }

    pub async fn resolve(&self, registry: &str) -> Result<Option<Credentials>> {
        if let Some(credentials) = &self.cli_credentials {
            return Ok(Some(credentials.clone()));
        }

        let Some(config) = self.docker_config.clone() else {
            return Ok(None);
        };

        if let Some(credentials) = self.resolved.lock().await.get(registry).cloned() {
            return Ok(credentials);
        }

        let registry = registry.to_string();
        let cache_key = registry.clone();
        let resolved =
            tokio::task::spawn_blocking(move || resolve_docker_config(&config, &registry))
                .await
                .map_err(|error| {
                    DockerPullError::InvalidInput(format!("auth resolver task panicked: {error}"))
                })??;

        self.resolved
            .lock()
            .await
            .insert(cache_key, resolved.clone());
        Ok(resolved)
    }
}

fn resolve_docker_config(config: &DockerConfig, registry: &str) -> Result<Option<Credentials>> {
    for key in registry_keys(registry) {
        if let Some(helper) = config.cred_helpers.get(key)
            && let Some(credentials) = invoke_helper(helper, key)?
        {
            return Ok(Some(credentials));
        }
    }

    if let Some(helper) = &config.creds_store {
        for key in registry_keys(registry) {
            if let Some(credentials) = invoke_helper(helper, key)? {
                return Ok(Some(credentials));
            }
        }
    }

    for key in registry_keys(registry) {
        if let Some(entry) = config.auths.get(key)
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

fn load_docker_config() -> Result<Option<DockerConfig>> {
    let path = docker_config_path();
    if !path.exists() {
        return Ok(None);
    }

    let content = match std::fs::read_to_string(&path) {
        Ok(content) => content,
        Err(error) => {
            warn!(
                "failed to read docker config at `{}`: {}; continuing without docker auth",
                path.display(),
                error
            );
            return Ok(None);
        }
    };
    match serde_json::from_str(&content) {
        Ok(config) => Ok(Some(config)),
        Err(error) => {
            warn!(
                "failed to parse docker config at `{}`: {}; continuing without docker auth",
                path.display(),
                error
            );
            Ok(None)
        }
    }
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
    let helper_binary = format!("docker-credential-{helper}");
    let mut command = Command::new(&helper_binary);
    command.arg("get");
    invoke_helper_command(command, &helper_binary, registry)
}

fn invoke_helper_command(
    mut command: Command,
    helper_binary: &str,
    registry: &str,
) -> Result<Option<Credentials>> {
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            warn!(
                "docker credential helper `{}` not found for `{}`",
                helper_binary, registry
            );
            return Ok(None);
        }
        Err(error) => {
            warn!(
                "failed to spawn docker credential helper `{}` for `{}`: {}",
                helper_binary, registry, error
            );
            return Ok(None);
        }
    };

    if let Some(mut stdin) = child.stdin.take()
        && let Err(error) = stdin.write_all(registry.as_bytes())
    {
        drop(stdin);
        let _ = child.wait();
        warn!(
            "failed to write registry to docker credential helper `{}` for `{}`: {}",
            helper_binary, registry, error
        );
        return Ok(None);
    }
    let output = match child.wait_with_output() {
        Ok(output) => output,
        Err(error) => {
            warn!(
                "failed to wait for docker credential helper `{}` for `{}`: {}",
                helper_binary, registry, error
            );
            return Ok(None);
        }
    };
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        if stderr.is_empty() {
            warn!(
                "docker credential helper `{}` failed for `{}` with status {}",
                helper_binary, registry, output.status
            );
        } else {
            warn!(
                "docker credential helper `{}` failed for `{}` with status {}: {}",
                helper_binary, registry, output.status, stderr
            );
        }
        return Ok(None);
    }

    let response: HelperResponse = match serde_json::from_slice(&output.stdout) {
        Ok(response) => response,
        Err(error) => {
            if output.stdout.iter().all(|byte| byte.is_ascii_whitespace()) {
                warn!(
                    "docker credential helper `{}` returned invalid JSON for `{}`: {}",
                    helper_binary, registry, error
                );
            } else {
                warn!(
                    "docker credential helper `{}` returned invalid JSON for `{}`: {} (non-empty stdout omitted)",
                    helper_binary, registry, error
                );
            }
            return Ok(None);
        }
    };
    Ok(Some(Credentials::Basic {
        username: response.username,
        password: response.secret,
    }))
}

fn registry_keys(registry: &str) -> Vec<&str> {
    let mut keys = vec![registry];
    for alias in docker_hub_aliases(registry) {
        if alias != registry {
            keys.push(alias);
        }
    }
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

pub(crate) fn read_credentials(
    username: Option<String>,
    password_stdin: bool,
) -> Result<Option<Credentials>> {
    let password = if password_stdin {
        Some(read_password_stdin()?)
    } else {
        None
    };

    credentials_from_parts(username, password)
}

fn read_password_stdin() -> Result<String> {
    use std::io::Read;

    let mut password = String::new();
    std::io::stdin().read_to_string(&mut password)?;
    Ok(password.trim_end_matches(['\n', '\r']).to_string())
}

fn credentials_from_parts(
    username: Option<String>,
    password: Option<String>,
) -> Result<Option<Credentials>> {
    match (username, password) {
        (Some(username), Some(password)) => Ok(Some(Credentials::Basic { username, password })),
        (None, None) => Ok(None),
        (Some(_), None) => Err(DockerPullError::InvalidInput(
            "`--username` requires `--password-stdin`".into(),
        )),
        (None, Some(_)) => Err(DockerPullError::InvalidInput(
            "`--password-stdin` requires `--username`".into(),
        )),
    }
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;
    use std::fs;
    use std::panic::{AssertUnwindSafe, catch_unwind, resume_unwind};
    #[cfg(unix)]
    use std::process::Command;
    use std::sync::{Mutex, MutexGuard, OnceLock};

    use base64::Engine as _;
    use tempfile::tempdir;

    #[cfg(unix)]
    use super::invoke_helper_command;
    use super::{
        AuthResolver, Credentials, credentials_from_parts, load_docker_config, registry_keys,
    };
    use crate::error::DockerPullError;

    #[test]
    fn credentials_reject_username_without_password_stdin() {
        let error = credentials_from_parts(Some("alice".into()), None)
            .expect_err("username without password should fail");

        assert!(
            matches!(error, DockerPullError::InvalidInput(message) if message == "`--username` requires `--password-stdin`")
        );
    }

    #[test]
    fn credentials_reject_password_stdin_without_username() {
        let error = credentials_from_parts(None, Some("secret".into()))
            .expect_err("password without username should fail");

        assert!(
            matches!(error, DockerPullError::InvalidInput(message) if message == "`--password-stdin` requires `--username`")
        );
    }

    fn docker_config_env_lock() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }

    fn lock_docker_config_env() -> MutexGuard<'static, ()> {
        docker_config_env_lock()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    struct DockerConfigEnvGuard {
        _lock: MutexGuard<'static, ()>,
        previous: Option<OsString>,
        restored: bool,
    }

    impl DockerConfigEnvGuard {
        fn new(path: &std::path::Path) -> Self {
            Self::new_with_lock(lock_docker_config_env(), path)
        }

        fn new_with_lock(lock: MutexGuard<'static, ()>, path: &std::path::Path) -> Self {
            let previous = std::env::var_os("DOCKER_CONFIG");
            unsafe {
                std::env::set_var("DOCKER_CONFIG", path);
            }
            Self {
                _lock: lock,
                previous,
                restored: false,
            }
        }

        fn restore(&mut self) {
            if self.restored {
                return;
            }
            restore_docker_config_env(self.previous.as_deref());
            self.restored = true;
        }
    }

    impl Drop for DockerConfigEnvGuard {
        fn drop(&mut self) {
            self.restore();
        }
    }

    fn restore_docker_config_env(previous: Option<&std::ffi::OsStr>) {
        unsafe {
            if let Some(previous) = previous {
                std::env::set_var("DOCKER_CONFIG", previous);
            } else {
                std::env::remove_var("DOCKER_CONFIG");
            }
        }
    }

    fn restore_path_env(previous: Option<&std::ffi::OsStr>) {
        unsafe {
            if let Some(previous) = previous {
                std::env::set_var("PATH", previous);
            } else {
                std::env::remove_var("PATH");
            }
        }
    }

    fn with_docker_config_env<T>(path: &std::path::Path, run: impl FnOnce() -> T) -> T {
        let guard = DockerConfigEnvGuard::new(path);
        let result = catch_unwind(AssertUnwindSafe(run));
        drop(guard);
        match result {
            Ok(value) => value,
            Err(payload) => resume_unwind(payload),
        }
    }

    #[cfg(unix)]
    #[test]
    fn failing_helper_falls_back_instead_of_aborting_auth_resolution() {
        let mut command = Command::new("sh");
        command.arg("-c").arg("echo helper-broke >&2; exit 1");

        let result =
            invoke_helper_command(command, "docker-credential-test", "registry-1.docker.io")
                .expect("helper failure should fall back");

        assert!(
            result.is_none(),
            "failed helper should not return credentials"
        );
    }

    #[test]
    fn docker_hub_registry_keys_include_aliases_without_duplicates() {
        assert_eq!(
            registry_keys("registry-1.docker.io"),
            vec![
                "registry-1.docker.io",
                "https://registry-1.docker.io",
                "index.docker.io",
                "https://index.docker.io/v1/",
                "docker.io",
            ]
        );
    }

    #[test]
    fn malformed_docker_config_falls_back_to_no_auth() {
        let dir = tempdir().expect("tempdir should create");
        fs::write(dir.path().join("config.json"), "{not-json").expect("config should be written");

        let config = with_docker_config_env(dir.path(), || {
            load_docker_config().expect("load should not fail on malformed config")
        });

        assert!(
            config.is_none(),
            "malformed docker config should be ignored"
        );
    }

    #[tokio::test]
    async fn resolves_docker_hub_alias_auth_entry_from_config() {
        let dir = tempdir().expect("tempdir should create");
        let auth = base64::engine::general_purpose::STANDARD.encode("demo-user:demo-pass");
        let config = format!(
            r#"{{
                "auths": {{
                    "https://index.docker.io/v1/": {{
                        "auth": "{auth}"
                    }}
                }}
            }}"#
        );
        fs::write(dir.path().join("config.json"), config).expect("config should be written");

        let resolver = with_docker_config_env(dir.path(), || {
            AuthResolver::new(None).expect("resolver should build")
        });
        let credentials = resolver
            .resolve("registry-1.docker.io")
            .await
            .expect("auth resolution should succeed");

        assert!(matches!(
            credentials,
            Some(Credentials::Basic { username, password })
                if username == "demo-user" && password == "demo-pass"
        ));
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "current_thread")]
    async fn docker_config_helper_resolution_is_cached() {
        use std::os::unix::fs::PermissionsExt;

        let lock = lock_docker_config_env();
        let dir = tempdir().expect("tempdir should create");
        let bin_dir = dir.path().join("bin");
        fs::create_dir(&bin_dir).expect("bin dir should create");
        let helper_path = bin_dir.join("docker-credential-test");
        let count_path = dir.path().join("helper-count");
        fs::write(
            &helper_path,
            format!(
                r#"#!/bin/sh
count_file="{}"
count="$(cat "$count_file" 2>/dev/null || printf 0)"
count="$((count + 1))"
printf '%s' "$count" > "$count_file"
printf '{{"Username":"helper-user","Secret":"helper-pass"}}'
"#,
                count_path.display()
            ),
        )
        .expect("helper should be written");
        let mut permissions = fs::metadata(&helper_path)
            .expect("helper metadata")
            .permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&helper_path, permissions).expect("helper should be executable");
        fs::write(
            dir.path().join("config.json"),
            r#"{"credHelpers":{"registry-1.docker.io":"test"}}"#,
        )
        .expect("config should be written");

        let previous_path = std::env::var_os("PATH");
        unsafe {
            std::env::set_var(
                "PATH",
                format!(
                    "{}:{}",
                    bin_dir.display(),
                    previous_path
                        .as_deref()
                        .and_then(std::ffi::OsStr::to_str)
                        .unwrap_or_default()
                ),
            );
        }
        let mut docker_config = DockerConfigEnvGuard::new_with_lock(lock, dir.path());
        let resolver = AuthResolver::new(None).expect("resolver should build");

        let first = resolver
            .resolve("registry-1.docker.io")
            .await
            .expect("first auth resolution should succeed");
        let second = resolver
            .resolve("registry-1.docker.io")
            .await
            .expect("second auth resolution should succeed");

        docker_config.restore();
        restore_path_env(previous_path.as_deref());

        assert!(matches!(
            first,
            Some(Credentials::Basic { username, password })
                if username == "helper-user" && password == "helper-pass"
        ));
        assert!(matches!(
            second,
            Some(Credentials::Basic { username, password })
                if username == "helper-user" && password == "helper-pass"
        ));
        assert_eq!(
            fs::read_to_string(count_path).expect("count should read"),
            "1",
            "helper should only be invoked once for a cached registry"
        );
    }

    #[test]
    fn docker_config_env_is_restored_after_panic() {
        let dir = tempdir().expect("tempdir should create");
        let lock = lock_docker_config_env();
        let previous = std::env::var_os("DOCKER_CONFIG");
        let (result, restored) = {
            let mut guard = DockerConfigEnvGuard::new_with_lock(lock, dir.path());
            let result = catch_unwind(AssertUnwindSafe(|| panic!("boom")));
            guard.restore();
            let restored = std::env::var_os("DOCKER_CONFIG");
            (result, restored)
        };

        assert!(result.is_err(), "closure panic should propagate");
        assert_eq!(
            restored, previous,
            "DOCKER_CONFIG should be restored after panic"
        );
    }
}
