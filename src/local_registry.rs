use std::sync::Arc;

use percent_encoding::percent_decode_str;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

use crate::error::{DockerPullError, Result};
use crate::store::{Store, StoredReference};

const MANIFEST_CONTENT_TYPE: &str = "application/vnd.oci.image.manifest.v1+json";
const BLOB_CONTENT_TYPE: &str = "application/octet-stream";

pub struct LocalRegistry {
    address: String,
    repository: String,
    _task: JoinHandle<()>,
    shutdown: Option<oneshot::Sender<()>>,
}

impl LocalRegistry {
    pub async fn start(store: Arc<Store>, reference: StoredReference) -> Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let port = listener.local_addr()?.port();
        let address = format!("127.0.0.1:{port}");
        let repository = synthetic_repository(&reference.manifest.digest)?;
        let state = Arc::new(RegistryState {
            store,
            reference,
            repository: repository.clone(),
        });
        let (shutdown, shutdown_rx) = oneshot::channel();
        let task = tokio::spawn(run_registry(listener, state, shutdown_rx));

        Ok(Self {
            address,
            repository,
            _task: task,
            shutdown: Some(shutdown),
        })
    }

    pub fn synthetic_reference(&self) -> String {
        format!("{}/{}:latest", self.address, self.repository)
    }
}

impl Drop for LocalRegistry {
    fn drop(&mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
    }
}

struct RegistryState {
    store: Arc<Store>,
    reference: StoredReference,
    repository: String,
}

async fn run_registry(
    listener: TcpListener,
    state: Arc<RegistryState>,
    mut shutdown: oneshot::Receiver<()>,
) {
    loop {
        tokio::select! {
            result = listener.accept() => {
                let Ok((stream, _)) = result else {
                    break;
                };
                let state = state.clone();
                tokio::spawn(async move {
                    let _ = handle_connection(stream, state).await;
                });
            }
            _ = &mut shutdown => break,
        }
    }
}

async fn handle_connection(mut stream: TcpStream, state: Arc<RegistryState>) -> Result<()> {
    let request = read_request(&mut stream).await?;
    let response = route_request(&request, &state).await;
    write_response(&mut stream, response).await
}

struct Request {
    method: String,
    path: String,
}

async fn read_request(stream: &mut TcpStream) -> Result<Request> {
    let mut bytes = Vec::new();
    let mut buffer = [0_u8; 4096];
    loop {
        let read = stream.read(&mut buffer).await?;
        if read == 0 {
            break;
        }
        bytes.extend_from_slice(&buffer[..read]);
        if !looks_like_http_request(&bytes) {
            return Err(DockerPullError::BadResponse(
                "local registry received non-HTTP request".into(),
            ));
        }
        if bytes.windows(4).any(|window| window == b"\r\n\r\n") {
            break;
        }
        if bytes.len() > 64 * 1024 {
            return Err(DockerPullError::InvalidInput(
                "local registry request headers are too large".into(),
            ));
        }
    }

    let text = std::str::from_utf8(&bytes)
        .map_err(|error| DockerPullError::BadResponse(format!("invalid HTTP request: {error}")))?;
    let request_line = text
        .lines()
        .next()
        .ok_or_else(|| DockerPullError::BadResponse("empty HTTP request".into()))?;
    let mut parts = request_line.split_whitespace();
    let method = parts
        .next()
        .ok_or_else(|| DockerPullError::BadResponse("missing HTTP method".into()))?
        .to_string();
    let path = parts
        .next()
        .ok_or_else(|| DockerPullError::BadResponse("missing HTTP path".into()))?
        .to_string();
    Ok(Request { method, path })
}

async fn route_request(request: &Request, state: &RegistryState) -> RegistryResponse {
    if request.path == "/v2/" {
        return RegistryResponse::empty(200, "OK");
    }

    let Some(rest) = request.path.strip_prefix("/v2/") else {
        return RegistryResponse::empty(404, "Not Found");
    };

    if let Some(reference) = route_suffix(rest, &state.repository, "/manifests/") {
        let reference = percent_decode_path_segment(reference);
        if reference != "latest" && reference != state.reference.manifest.digest {
            return RegistryResponse::empty(404, "Not Found");
        }
        return match read_manifest(state).await {
            Ok((bytes, media_type)) => {
                let content_type = if media_type.is_empty() {
                    MANIFEST_CONTENT_TYPE.to_string()
                } else {
                    media_type
                };
                RegistryResponse::bytes(200, "OK", content_type, bytes, request.method == "HEAD")
                    .with_digest(&state.reference.manifest.digest)
            }
            Err(error) => RegistryResponse::text(500, "Internal Server Error", error.to_string()),
        };
    }

    if let Some(digest) = route_suffix(rest, &state.repository, "/blobs/") {
        let digest = percent_decode_path_segment(digest);
        return match read_blob(state, &digest).await {
            Ok(bytes) => RegistryResponse::bytes(
                200,
                "OK",
                BLOB_CONTENT_TYPE.to_string(),
                bytes,
                request.method == "HEAD",
            )
            .with_digest(&digest),
            Err(DockerPullError::MissingBlobFile(_, _)) => {
                RegistryResponse::empty(404, "Not Found")
            }
            Err(error) => RegistryResponse::text(500, "Internal Server Error", error.to_string()),
        };
    }

    RegistryResponse::empty(404, "Not Found")
}

fn percent_decode_path_segment(value: &str) -> String {
    percent_decode_str(value).decode_utf8_lossy().into_owned()
}

fn route_suffix<'a>(path: &'a str, repository: &str, separator: &str) -> Option<&'a str> {
    let (candidate, suffix) = path.split_once(separator)?;
    (candidate == repository).then_some(suffix)
}

fn looks_like_http_request(bytes: &[u8]) -> bool {
    const METHODS: [&[u8]; 3] = [b"GET", b"HEAD", b"POST"];
    METHODS
        .iter()
        .any(|method| method.starts_with(bytes) || bytes.starts_with(method))
}

async fn read_manifest(state: &RegistryState) -> Result<(Vec<u8>, String)> {
    let path = state.store.blob_path(&state.reference.manifest.digest)?;
    let bytes = tokio::fs::read(path).await?;
    Ok((bytes, state.reference.manifest.media_type.clone()))
}

async fn read_blob(state: &RegistryState, digest: &str) -> Result<Vec<u8>> {
    let path = state.store.blob_path(digest)?;
    if !path.exists() {
        return Err(DockerPullError::MissingBlobFile(digest.to_string(), path));
    }
    tokio::fs::read(path).await.map_err(Into::into)
}

struct RegistryResponse {
    status: u16,
    reason: &'static str,
    content_type: String,
    body: Vec<u8>,
    content_length: usize,
    digest: Option<String>,
}

impl RegistryResponse {
    fn empty(status: u16, reason: &'static str) -> Self {
        Self {
            status,
            reason,
            content_type: "text/plain".to_string(),
            body: Vec::new(),
            content_length: 0,
            digest: None,
        }
    }

    fn text(status: u16, reason: &'static str, text: String) -> Self {
        Self {
            status,
            reason,
            content_type: "text/plain".to_string(),
            content_length: text.len(),
            body: text.into_bytes(),
            digest: None,
        }
    }

    fn bytes(
        status: u16,
        reason: &'static str,
        content_type: String,
        bytes: Vec<u8>,
        headers_only: bool,
    ) -> Self {
        let content_length = bytes.len();
        Self {
            status,
            reason,
            content_type,
            content_length,
            body: if headers_only { Vec::new() } else { bytes },
            digest: None,
        }
    }

    fn with_digest(mut self, digest: &str) -> Self {
        self.digest = Some(digest.to_string());
        self
    }
}

async fn write_response(stream: &mut TcpStream, response: RegistryResponse) -> Result<()> {
    let mut headers = format!(
        "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n",
        response.status, response.reason, response.content_type, response.content_length
    );
    headers.push_str("Docker-Distribution-API-Version: registry/2.0\r\n");
    if let Some(digest) = response.digest {
        headers.push_str(&format!("Docker-Content-Digest: {digest}\r\n"));
    }
    headers.push_str("\r\n");
    stream.write_all(headers.as_bytes()).await?;
    stream.write_all(&response.body).await?;
    stream.flush().await?;
    stream.shutdown().await?;
    Ok(())
}

fn synthetic_repository(digest: &str) -> Result<String> {
    let (algorithm, value) = digest
        .split_once(':')
        .ok_or_else(|| DockerPullError::InvalidInput(format!("invalid digest `{digest}`")))?;
    Ok(format!("pocker-cache/{algorithm}-{value}"))
}
