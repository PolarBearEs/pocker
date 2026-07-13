use std::env;
use std::path::{Path, PathBuf};

use reqwest::StatusCode;
use tokio::io::DuplexStream;
use tokio_util::io::ReaderStream;

use crate::error::{DockerPullError, Result};

#[path = "transport_reqwest.rs"]
mod reqwest_transport;
#[cfg(any(test, windows))]
#[path = "transport_windows.rs"]
pub(super) mod windows;

use reqwest_transport::ReqwestTransport;

#[cfg(windows)]
pub(super) const DEFAULT_DOCKER_HOST: &str = "npipe:////./pipe/docker_engine";
#[cfg(not(windows))]
pub(super) const DEFAULT_DOCKER_HOST: &str = "unix:///var/run/docker.sock";

#[derive(Debug, Clone)]
pub(super) enum DockerEndpoint {
    #[cfg(unix)]
    Unix(PathBuf),
    #[cfg(windows)]
    NamedPipe(PathBuf),
    Http(String),
}

#[derive(Debug, Clone)]
pub(super) enum DockerTransport {
    Reqwest(ReqwestTransport),
    #[cfg(windows)]
    NamedPipe {
        path: PathBuf,
    },
}

pub(super) struct DockerResponse {
    pub(super) status: StatusCode,
    pub(super) body: Vec<u8>,
}

pub(super) struct AtomicOutputFile {
    file: tokio::fs::File,
    path: tempfile::TempPath,
}

impl AtomicOutputFile {
    pub(super) async fn create(output: &Path) -> Result<Self> {
        let output = output.to_path_buf();
        let (file, path) = tokio::task::spawn_blocking(move || {
            let parent = output
                .parent()
                .filter(|parent| !parent.as_os_str().is_empty())
                .unwrap_or_else(|| Path::new("."));
            let temporary = tempfile::Builder::new()
                .prefix(".pocker-export-")
                .tempfile_in(parent)?;
            Ok::<_, std::io::Error>(temporary.into_parts())
        })
        .await
        .map_err(|error| {
            DockerPullError::InvalidInput(format!("temporary export task panicked: {error}"))
        })??;
        Ok(Self {
            path,
            file: tokio::fs::File::from_std(file),
        })
    }

    pub(super) fn file_mut(&mut self) -> &mut tokio::fs::File {
        &mut self.file
    }

    pub(super) async fn persist(mut self, output: &Path) -> Result<()> {
        use tokio::io::AsyncWriteExt;

        self.file.flush().await?;
        self.file.sync_all().await?;
        drop(self.file);
        let path = self.path;
        let output = output.to_path_buf();
        tokio::task::spawn_blocking(move || path.persist(output).map_err(|error| error.error))
            .await
            .map_err(|error| {
                DockerPullError::InvalidInput(format!("export persist task panicked: {error}"))
            })??;
        Ok(())
    }
}

impl DockerTransport {
    pub(super) fn connect() -> Result<Self> {
        let endpoint = docker_endpoint()?;

        #[cfg(unix)]
        let transport = match endpoint {
            DockerEndpoint::Unix(path) => {
                DockerTransport::Reqwest(ReqwestTransport::unix_socket(path)?)
            }
            DockerEndpoint::Http(base_url) => {
                DockerTransport::Reqwest(ReqwestTransport::http(base_url)?)
            }
        };

        #[cfg(not(unix))]
        let transport = match endpoint {
            #[cfg(windows)]
            DockerEndpoint::NamedPipe(path) => DockerTransport::NamedPipe { path },
            DockerEndpoint::Http(base_url) => {
                DockerTransport::Reqwest(ReqwestTransport::http(base_url)?)
            }
        };

        Ok(transport)
    }

    pub(super) async fn load_archive(&self, path: &Path) -> Result<()> {
        match self {
            DockerTransport::Reqwest(transport) => transport.load_archive(path).await,
            #[cfg(windows)]
            DockerTransport::NamedPipe { path: pipe_path } => {
                let file = tokio::fs::File::open(path).await?;
                let len = file.metadata().await?.len();
                let response = windows::request_file(
                    pipe_path,
                    "POST",
                    "/images/load?quiet=1",
                    "application/x-tar",
                    file,
                    len,
                )
                .await?;
                ensure_success_status(response.status, response.body, "docker image load")
            }
        }
    }

    pub(super) async fn load_archive_stream(
        &self,
        stream: ReaderStream<DuplexStream>,
    ) -> Result<()> {
        match self {
            DockerTransport::Reqwest(transport) => transport.load_archive_stream(stream).await,
            #[cfg(windows)]
            DockerTransport::NamedPipe { path } => {
                let response = windows::request_chunked_stream(
                    path,
                    "POST",
                    "/images/load?quiet=1",
                    "application/x-tar",
                    stream,
                )
                .await?;
                ensure_success_status(response.status, response.body, "docker image load")
            }
        }
    }

    pub(super) async fn save_response_to_file(
        &self,
        path: &str,
        output: &Path,
        action: &str,
    ) -> Result<()> {
        match self {
            DockerTransport::Reqwest(transport) => {
                transport.save_response_to_file(path, output, action).await
            }
            #[cfg(windows)]
            DockerTransport::NamedPipe { path: pipe_path } => {
                windows::request_to_file(pipe_path, "GET", path, output, action).await
            }
        }
    }

    pub(super) async fn request_bytes(
        &self,
        method: &str,
        path: &str,
        body: Option<Vec<u8>>,
    ) -> Result<DockerResponse> {
        match self {
            DockerTransport::Reqwest(transport) => {
                transport.request_bytes(method, path, body).await
            }
            #[cfg(windows)]
            DockerTransport::NamedPipe { path: pipe_path } => {
                windows::request_bytes(pipe_path, method, path, body.unwrap_or_default()).await
            }
        }
    }
}

fn docker_endpoint() -> Result<DockerEndpoint> {
    match env::var("DOCKER_HOST") {
        Ok(host) if !host.trim().is_empty() => docker_endpoint_from_host(&host),
        _ => docker_endpoint_from_host(DEFAULT_DOCKER_HOST),
    }
}

pub(super) fn docker_endpoint_from_host(host: &str) -> Result<DockerEndpoint> {
    #[cfg(unix)]
    if let Some(path) = host.strip_prefix("unix://") {
        if path.is_empty() {
            return Err(DockerPullError::InvalidInput(
                "docker host unix socket path is empty".into(),
            ));
        }
        return Ok(DockerEndpoint::Unix(PathBuf::from(path)));
    }

    if host.starts_with("npipe://") {
        #[cfg(windows)]
        {
            let path = host
                .strip_prefix("npipe://")
                .expect("npipe prefix was already checked");
            return Ok(DockerEndpoint::NamedPipe(
                windows::normalize_named_pipe_path(path)?,
            ));
        }
        #[cfg(not(windows))]
        return Err(DockerPullError::InvalidInput(
            "docker named pipes are not supported; set DOCKER_HOST to a tcp://, http://, or https:// endpoint".into(),
        ));
    }

    if let Some(address) = host.strip_prefix("tcp://") {
        return Ok(DockerEndpoint::Http(format!(
            "http://{}",
            address.trim_end_matches('/')
        )));
    }

    if host.starts_with("http://") || host.starts_with("https://") {
        return Ok(DockerEndpoint::Http(host.trim_end_matches('/').to_string()));
    }

    Err(DockerPullError::InvalidInput(format!(
        "unsupported docker host `{host}`"
    )))
}

pub(super) fn ensure_success_status(status: StatusCode, body: Vec<u8>, action: &str) -> Result<()> {
    if status.is_success() {
        return Ok(());
    }

    Err(build_failure_error(status, &body, action))
}

pub(super) fn build_failure_error(
    status: StatusCode,
    body: &[u8],
    action: &str,
) -> DockerPullError {
    let body = String::from_utf8_lossy(body);
    let body = body.trim();
    let detail = if body.is_empty() {
        format!("status {status}")
    } else {
        format!("status {status}: {body}")
    };
    DockerPullError::CommandFailed(format!("{action} failed: {detail}"))
}
