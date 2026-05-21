use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use reqwest::header::RANGE;
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Semaphore, oneshot};
use tracing::warn;

use crate::error::{DockerPullError, Result};
use crate::platform::Platform;
use crate::pull::{BlobDownloadLocks, PullContext, download};
use crate::reference::ImageReference;
use crate::registry::{Descriptor, MANIFEST_ACCEPT, RegistryClient, decode_cache_repository};
use crate::store::{Store, StoredReference};
use crate::ui::Ui;

const DEFAULT_MANIFEST_CONTENT_TYPE: &str = "application/vnd.oci.image.manifest.v1+json";
const BLOB_CONTENT_TYPE: &str = "application/octet-stream";

pub struct ServeConfig {
    pub listen: SocketAddr,
    pub store: Arc<Store>,
    pub registry: Arc<RegistryClient>,
    pub pull_missing: bool,
    pub blob_retry_limit: Option<u32>,
    pub concurrency: usize,
    pub quiet: bool,
}

pub async fn serve(config: ServeConfig) -> Result<()> {
    let listener = TcpListener::bind(config.listen).await?;
    serve_listener(
        listener,
        ServeListenerConfig {
            store: config.store,
            registry: config.registry,
            pull_missing: config.pull_missing,
            blob_retry_limit: config.blob_retry_limit,
            concurrency: config.concurrency,
            quiet: config.quiet,
        },
        None,
    )
    .await
}

pub(crate) struct ServeListenerConfig {
    pub store: Arc<Store>,
    pub registry: Arc<RegistryClient>,
    pub pull_missing: bool,
    pub blob_retry_limit: Option<u32>,
    pub concurrency: usize,
    pub quiet: bool,
}

pub(crate) async fn serve_listener(
    listener: TcpListener,
    config: ServeListenerConfig,
    shutdown: Option<oneshot::Receiver<()>>,
) -> Result<()> {
    let state = Arc::new(ServeState {
        store: config.store,
        registry: config.registry,
        pull_missing: config.pull_missing,
        blob_retry_limit: config.blob_retry_limit,
        quiet: config.quiet,
        downloads: Arc::new(Semaphore::new(config.concurrency)),
        blob_locks: Arc::new(BlobDownloadLocks::default()),
    });
    run_server(listener, state, shutdown).await
}

async fn run_server(
    listener: TcpListener,
    state: Arc<ServeState>,
    mut shutdown: Option<oneshot::Receiver<()>>,
) -> Result<()> {
    loop {
        tokio::select! {
            result = listener.accept() => {
                let (stream, _) = result?;
                let state = Arc::clone(&state);
                tokio::spawn(async move {
                    let _ = handle_connection(stream, state).await;
                });
            }
            _ = async {
                if let Some(shutdown) = shutdown.as_mut() {
                    let _ = shutdown.await;
                } else {
                    std::future::pending::<()>().await;
                }
            } => break,
        }
    }
    Ok(())
}

struct ServeState {
    store: Arc<Store>,
    registry: Arc<RegistryClient>,
    pull_missing: bool,
    blob_retry_limit: Option<u32>,
    quiet: bool,
    downloads: Arc<Semaphore>,
    blob_locks: Arc<BlobDownloadLocks>,
}

#[derive(Debug)]
struct Request {
    method: String,
    path: String,
    range: Option<String>,
}

async fn handle_connection(mut stream: TcpStream, state: Arc<ServeState>) -> Result<()> {
    let request = read_request(&mut stream).await?;
    let response = route_request(&request, state).await;
    write_response(&mut stream, response).await
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
                "cache registry received non-HTTP request".into(),
            ));
        }
        if bytes.windows(4).any(|window| window == b"\r\n\r\n") {
            break;
        }
        if bytes.len() > 64 * 1024 {
            return Err(DockerPullError::InvalidInput(
                "cache registry request headers are too large".into(),
            ));
        }
    }

    let text = std::str::from_utf8(&bytes)
        .map_err(|error| DockerPullError::BadResponse(format!("invalid HTTP request: {error}")))?;
    let mut lines = text.lines();
    let request_line = lines
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
    let mut range = None;
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        if name.eq_ignore_ascii_case(RANGE.as_str()) {
            range = Some(value.trim().to_string());
        }
    }
    Ok(Request {
        method,
        path,
        range,
    })
}

async fn route_request(request: &Request, state: Arc<ServeState>) -> RegistryResponse {
    if request.path == "/v2/" {
        return RegistryResponse::empty(200, "OK");
    }

    if request.method != "GET" && request.method != "HEAD" {
        return RegistryResponse::empty(405, "Method Not Allowed");
    }

    let Some(rest) = request.path.strip_prefix("/v2/") else {
        return RegistryResponse::empty(404, "Not Found");
    };

    if let Some((repository, reference)) = split_route(rest, "/manifests/") {
        return manifest_response(request, state, repository, reference).await;
    }

    if let Some((repository, digest)) = split_route(rest, "/blobs/") {
        return blob_response(request, state, repository, digest).await;
    }

    RegistryResponse::empty(404, "Not Found")
}

async fn manifest_response(
    request: &Request,
    state: Arc<ServeState>,
    repository: &str,
    reference: &str,
) -> RegistryResponse {
    let decoded = match upstream_reference(repository, reference) {
        Ok(reference) => reference,
        Err(error) => return RegistryResponse::text(400, "Bad Request", error.to_string()),
    };

    let normalized = decoded.normalized();
    if let Ok(Some(record)) = state.store.load_reference(&normalized).await {
        return serve_manifest_blob(&state, &record.manifest, request.method == "HEAD").await;
    }

    if let Ok(path) = state.store.blob_path(reference)
        && path.exists()
    {
        let descriptor = match manifest_descriptor_from_blob(&state.store, reference).await {
            Ok(descriptor) => descriptor,
            Err(error) => {
                return RegistryResponse::text(500, "Internal Server Error", error.to_string());
            }
        };
        return serve_manifest_blob(&state, &descriptor, request.method == "HEAD").await;
    }

    if !state.pull_missing {
        return RegistryResponse::empty(404, "Not Found");
    }

    match fetch_manifest(&state, &decoded, reference).await {
        Ok(descriptor) => serve_manifest_blob(&state, &descriptor, request.method == "HEAD").await,
        Err(DockerPullError::ManifestNotFound) => RegistryResponse::empty(404, "Not Found"),
        Err(error) => RegistryResponse::text(502, "Bad Gateway", error.to_string()),
    }
}

async fn blob_response(
    request: &Request,
    state: Arc<ServeState>,
    repository: &str,
    digest: &str,
) -> RegistryResponse {
    let decoded = match upstream_reference(repository, "latest") {
        Ok(reference) => reference,
        Err(error) => return RegistryResponse::text(400, "Bad Request", error.to_string()),
    };

    if let Ok((path, size)) = blob_path_and_size(&state.store, digest).await {
        return RegistryResponse::file(
            200,
            "OK",
            BLOB_CONTENT_TYPE.to_string(),
            path,
            size,
            request.method == "HEAD",
        )
        .with_digest(digest)
        .with_range(request.range.as_deref());
    }

    if !state.pull_missing {
        return RegistryResponse::empty(404, "Not Found");
    }

    match fetch_blob(&state, &decoded, digest).await {
        Ok(()) => match blob_path_and_size(&state.store, digest).await {
            Ok((path, size)) => RegistryResponse::file(
                200,
                "OK",
                BLOB_CONTENT_TYPE.to_string(),
                path,
                size,
                request.method == "HEAD",
            )
            .with_digest(digest)
            .with_range(request.range.as_deref()),
            Err(error) => RegistryResponse::text(500, "Internal Server Error", error.to_string()),
        },
        Err(DockerPullError::BlobNotFound(_)) => RegistryResponse::empty(404, "Not Found"),
        Err(error) => RegistryResponse::text(502, "Bad Gateway", error.to_string()),
    }
}

async fn fetch_manifest(
    state: &ServeState,
    reference: &ImageReference,
    requested_reference: &str,
) -> Result<Descriptor> {
    let raw = if is_supported_digest_reference(requested_reference) {
        state
            .registry
            .get_manifest_digest_raw(reference, requested_reference, Some(MANIFEST_ACCEPT))
            .await?
    } else {
        state
            .registry
            .get_manifest_raw(reference, Some(MANIFEST_ACCEPT))
            .await?
    };
    state
        .store
        .save_blob_bytes(&raw.descriptor, &raw.bytes)
        .await?;
    state
        .store
        .save_reference(&StoredReference {
            reference: reference.normalized(),
            manifest: raw.descriptor.clone(),
            config_digest: manifest_config_digest(&raw.bytes).unwrap_or_default(),
        })
        .await?;
    Ok(raw.descriptor)
}

async fn fetch_blob(state: &ServeState, reference: &ImageReference, digest: &str) -> Result<()> {
    let _permit = state.downloads.acquire().await.map_err(|error| {
        DockerPullError::CommandFailed(format!("download limiter closed: {error}"))
    })?;
    let metadata = state.registry.head_blob(reference, digest).await?;
    let Some(size) = metadata.size else {
        let bytes = state.registry.get_blob_bytes(reference, digest).await?;
        let descriptor = Descriptor {
            media_type: BLOB_CONTENT_TYPE.to_string(),
            digest: digest.to_string(),
            size: bytes.len() as i64,
            platform: None,
            annotations: None,
        };
        return state.store.save_blob_bytes(&descriptor, &bytes).await;
    };
    let descriptor = Descriptor {
        media_type: BLOB_CONTENT_TYPE.to_string(),
        digest: digest.to_string(),
        size: size as i64,
        platform: None,
        annotations: None,
    };
    let context = PullContext {
        store: Arc::clone(&state.store),
        registry: Arc::clone(&state.registry),
        stop: Arc::new(AtomicBool::new(false)),
        ui: Arc::new(Ui::new(state.quiet, false)),
        blob_retry_limit: state.blob_retry_limit,
        blob_locks: Arc::clone(&state.blob_locks),
    };
    download::download_blob(&context, reference, &reference.normalized(), descriptor).await
}

async fn serve_manifest_blob(
    state: &ServeState,
    descriptor: &Descriptor,
    headers_only: bool,
) -> RegistryResponse {
    let path = match state.store.blob_path(&descriptor.digest) {
        Ok(path) => path,
        Err(error) => {
            return RegistryResponse::text(500, "Internal Server Error", error.to_string());
        }
    };
    let content_type = if descriptor.media_type.is_empty() {
        DEFAULT_MANIFEST_CONTENT_TYPE.to_string()
    } else {
        descriptor.media_type.clone()
    };
    let size = match descriptor.expected_size() {
        Ok(size) => size,
        Err(error) => {
            warn!(
                "cannot serve cached manifest {} with invalid size {}: {}",
                descriptor.digest, descriptor.size, error
            );
            return RegistryResponse::text(500, "Internal Server Error", error.to_string());
        }
    };
    RegistryResponse::file(200, "OK", content_type, path, size, headers_only)
        .with_digest(&descriptor.digest)
}

fn upstream_reference(repository: &str, reference: &str) -> Result<ImageReference> {
    let (registry, upstream_repository) = decode_cache_repository(repository)?;
    let separator = if is_supported_digest_reference(reference) {
        '@'
    } else {
        ':'
    };
    ImageReference::parse(&format!(
        "{registry}/{upstream_repository}{separator}{reference}"
    ))
}

fn is_supported_digest_reference(reference: &str) -> bool {
    matches!(
        reference.split_once(':'),
        Some(("sha256" | "sha384" | "sha512", value)) if !value.is_empty()
    )
}

fn split_route<'a>(path: &'a str, separator: &str) -> Option<(&'a str, &'a str)> {
    path.split_once(separator)
}

fn looks_like_http_request(bytes: &[u8]) -> bool {
    const METHODS: [&[u8]; 3] = [b"GET", b"HEAD", b"POST"];
    METHODS
        .iter()
        .any(|method| method.starts_with(bytes) || bytes.starts_with(method))
}

async fn blob_path_and_size(store: &Store, digest: &str) -> Result<(PathBuf, u64)> {
    let path = store.blob_path(digest)?;
    let metadata = tokio::fs::metadata(&path).await.map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            DockerPullError::MissingBlobFile(digest.to_string(), path.clone())
        } else {
            error.into()
        }
    })?;
    Ok((path, metadata.len()))
}

async fn manifest_descriptor_from_blob(store: &Store, digest: &str) -> Result<Descriptor> {
    let path = store.blob_path(digest)?;
    let bytes = tokio::fs::read(&path).await?;
    Ok(Descriptor {
        media_type: manifest_media_type(&bytes).unwrap_or_default(),
        digest: digest.to_string(),
        size: bytes.len() as i64,
        platform: Some(Platform::host()),
        annotations: None,
    })
}

fn manifest_media_type(bytes: &[u8]) -> Option<String> {
    serde_json::from_slice::<serde_json::Value>(bytes)
        .ok()?
        .get("mediaType")?
        .as_str()
        .map(ToString::to_string)
}

fn manifest_config_digest(bytes: &[u8]) -> Option<String> {
    serde_json::from_slice::<serde_json::Value>(bytes)
        .ok()?
        .get("config")?
        .get("digest")?
        .as_str()
        .map(ToString::to_string)
}

enum RegistryBody {
    Empty,
    Text(Vec<u8>),
    File(PathBuf),
}

struct RegistryResponse {
    status: u16,
    reason: &'static str,
    content_type: String,
    body: RegistryBody,
    content_length: u64,
    digest: Option<String>,
    range: Option<ResponseRange>,
}

#[derive(Clone, Copy)]
struct ResponseRange {
    start: u64,
    end: u64,
    total: u64,
}

impl RegistryResponse {
    fn empty(status: u16, reason: &'static str) -> Self {
        Self {
            status,
            reason,
            content_type: "text/plain".to_string(),
            body: RegistryBody::Empty,
            content_length: 0,
            digest: None,
            range: None,
        }
    }

    fn text(status: u16, reason: &'static str, text: String) -> Self {
        Self {
            status,
            reason,
            content_type: "text/plain".to_string(),
            content_length: text.len() as u64,
            body: RegistryBody::Text(text.into_bytes()),
            digest: None,
            range: None,
        }
    }

    fn file(
        status: u16,
        reason: &'static str,
        content_type: String,
        path: PathBuf,
        content_length: u64,
        headers_only: bool,
    ) -> Self {
        Self {
            status,
            reason,
            content_type,
            content_length,
            body: if headers_only {
                RegistryBody::Empty
            } else {
                RegistryBody::File(path)
            },
            digest: None,
            range: None,
        }
    }

    fn with_digest(mut self, digest: &str) -> Self {
        self.digest = Some(digest.to_string());
        self
    }

    fn with_range(mut self, range: Option<&str>) -> Self {
        let Some(range) = range else {
            return self;
        };
        let Some(range) = parse_byte_range(range, self.content_length) else {
            return RegistryResponse::empty(416, "Range Not Satisfiable");
        };
        self.status = 206;
        self.reason = "Partial Content";
        self.content_length = range.end - range.start + 1;
        self.range = Some(range);
        self
    }
}

fn parse_byte_range(value: &str, total: u64) -> Option<ResponseRange> {
    let value = value.strip_prefix("bytes=")?;
    let (start, end) = value.split_once('-')?;
    if start.is_empty() {
        return None;
    }
    let start = start.parse::<u64>().ok()?;
    let end = if end.is_empty() {
        total.checked_sub(1)?
    } else {
        end.parse::<u64>().ok()?
    };
    if start > end || end >= total {
        return None;
    }
    Some(ResponseRange { start, end, total })
}

async fn write_response(stream: &mut TcpStream, response: RegistryResponse) -> Result<()> {
    let mut headers = format!(
        "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n",
        response.status, response.reason, response.content_type, response.content_length
    );
    headers.push_str("Docker-Distribution-API-Version: registry/2.0\r\n");
    if let Some(digest) = &response.digest {
        headers.push_str(&format!("Docker-Content-Digest: {digest}\r\n"));
    }
    if let Some(range) = response.range {
        headers.push_str(&format!(
            "Content-Range: bytes {}-{}/{}\r\n",
            range.start, range.end, range.total
        ));
    }
    headers.push_str("\r\n");
    stream.write_all(headers.as_bytes()).await?;
    match response.body {
        RegistryBody::Empty => {}
        RegistryBody::Text(bytes) => stream.write_all(&bytes).await?,
        RegistryBody::File(path) => {
            let mut file = tokio::fs::File::open(path).await?;
            if let Some(range) = response.range {
                file.seek(std::io::SeekFrom::Start(range.start)).await?;
                let mut take = file.take(response.content_length);
                tokio::io::copy(&mut take, stream).await?;
            } else {
                tokio::io::copy(&mut file, stream).await?;
            }
        }
    }
    stream.flush().await?;
    stream.shutdown().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::AtomicBool;

    use reqwest::StatusCode;
    use tempfile::tempdir;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;
    use tokio::sync::Semaphore;

    use super::{ServeState, is_supported_digest_reference, parse_byte_range, run_server};
    use crate::auth::AuthResolver;
    use crate::platform::Platform;
    use crate::pull::{BlobDownloadLocks, PullContext, PullOptions, Puller};
    use crate::reference::ImageReference;
    use crate::registry::{Descriptor, RegistryClient, cache_repository};
    use crate::store::{Store, StoredReference};
    use crate::ui::Ui;

    #[tokio::test]
    async fn cached_manifest_is_served() {
        let dir = tempdir().expect("tempdir should create");
        let store = Arc::new(
            Store::open(dir.path().to_path_buf())
                .await
                .expect("store should open"),
        );
        let manifest = br#"{"schemaVersion":2,"mediaType":"application/vnd.oci.image.manifest.v1+json","config":{"digest":"sha256:2222222222222222222222222222222222222222222222222222222222222222"},"layers":[]}"#;
        let descriptor = Descriptor {
            media_type: "application/vnd.oci.image.manifest.v1+json".into(),
            digest: "sha256:5ae1916d0eccd953c0f9dec4f51f6bc7d16b1a2225a166aa2fcfd3f4322d4d24"
                .into(),
            size: manifest.len() as i64,
            platform: None,
            annotations: None,
        };
        store
            .save_blob_bytes(&descriptor, manifest)
            .await
            .expect("manifest should save");
        store
            .save_reference(&StoredReference {
                reference: "registry-1.docker.io/library/alpine:latest".into(),
                manifest: descriptor.clone(),
                config_digest:
                    "sha256:2222222222222222222222222222222222222222222222222222222222222222"
                        .into(),
            })
            .await
            .expect("reference should save");
        let address = spawn_server(store, false).await;
        let path = format!(
            "http://{}/v2/{}/manifests/latest",
            address,
            cache_repository("registry-1.docker.io", "library/alpine")
        );

        let response = reqwest::get(path).await.expect("request should succeed");

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response
                .headers()
                .get("docker-content-digest")
                .and_then(|value| value.to_str().ok()),
            Some(descriptor.digest.as_str())
        );
    }

    #[tokio::test]
    async fn missing_manifest_is_404_without_pull_missing() {
        let dir = tempdir().expect("tempdir should create");
        let store = Arc::new(
            Store::open(dir.path().to_path_buf())
                .await
                .expect("store should open"),
        );
        let address = spawn_server(store, false).await;
        let path = format!(
            "http://{}/v2/{}/manifests/latest",
            address,
            cache_repository("registry-1.docker.io", "library/missing")
        );

        let response = reqwest::get(path).await.expect("request should succeed");

        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn cached_blob_supports_head_and_range() {
        let dir = tempdir().expect("tempdir should create");
        let store = Arc::new(
            Store::open(dir.path().to_path_buf())
                .await
                .expect("store should open"),
        );
        let bytes = b"0123456789";
        let descriptor = Descriptor {
            media_type: "application/octet-stream".into(),
            digest: "sha256:84d89877f0d4041efb6bf91a16f0248f2fd573e6af05c19f96bedb9f882f7882"
                .into(),
            size: bytes.len() as i64,
            platform: None,
            annotations: None,
        };
        store
            .save_blob_bytes(&descriptor, bytes)
            .await
            .expect("blob should save");
        let address = spawn_server(store, false).await;
        let path = format!(
            "http://{}/v2/{}/blobs/{}",
            address,
            cache_repository("registry-1.docker.io", "library/alpine"),
            descriptor.digest
        );
        let client = reqwest::Client::new();

        let head = client
            .head(&path)
            .send()
            .await
            .expect("HEAD should succeed");
        assert_eq!(head.status(), StatusCode::OK);
        assert_eq!(
            head.headers()
                .get("content-length")
                .and_then(|value| value.to_str().ok()),
            Some("10")
        );

        let ranged = client
            .get(&path)
            .header("Range", "bytes=4-")
            .send()
            .await
            .expect("range request should succeed");
        assert_eq!(ranged.status(), StatusCode::PARTIAL_CONTENT);
        assert_eq!(ranged.text().await.expect("body should read"), "456789");
    }

    #[test]
    fn parse_byte_range_accepts_open_and_closed_ranges() {
        let open = parse_byte_range("bytes=4-", 10).expect("open range should parse");
        assert_eq!((open.start, open.end, open.total), (4, 9, 10));

        let closed = parse_byte_range("bytes=4-7", 10).expect("closed range should parse");
        assert_eq!((closed.start, closed.end, closed.total), (4, 7, 10));
    }

    #[test]
    fn parse_byte_range_rejects_unsatisfiable_ranges() {
        assert!(parse_byte_range("bytes=99-", 10).is_none());
        assert!(parse_byte_range("bytes=7-4", 10).is_none());
        assert!(parse_byte_range("bytes=4-99", 10).is_none());
        assert!(parse_byte_range("bytes=-4", 10).is_none());
    }

    #[test]
    fn supported_digest_reference_accepts_sha512() {
        assert!(is_supported_digest_reference(&format!(
            "sha512:{}",
            "a".repeat(128)
        )));
        assert!(is_supported_digest_reference(&format!(
            "sha256:{}",
            "A".repeat(64)
        )));
        assert!(is_supported_digest_reference("sha384:abc"));
        assert!(!is_supported_digest_reference(&format!(
            "sha224:{}",
            "a".repeat(56)
        )));
        assert!(!is_supported_digest_reference("sha512:"));
    }

    #[tokio::test]
    async fn invalid_blob_range_returns_416() {
        let dir = tempdir().expect("tempdir should create");
        let store = Arc::new(
            Store::open(dir.path().to_path_buf())
                .await
                .expect("store should open"),
        );
        let bytes = b"0123456789";
        let descriptor = Descriptor {
            media_type: "application/octet-stream".into(),
            digest: "sha256:84d89877f0d4041efb6bf91a16f0248f2fd573e6af05c19f96bedb9f882f7882"
                .into(),
            size: bytes.len() as i64,
            platform: None,
            annotations: None,
        };
        store
            .save_blob_bytes(&descriptor, bytes)
            .await
            .expect("blob should save");
        let address = spawn_server(store, false).await;
        let path = format!(
            "http://{}/v2/{}/blobs/{}",
            address,
            cache_repository("registry-1.docker.io", "library/alpine"),
            descriptor.digest
        );

        let response = reqwest::Client::new()
            .get(path)
            .header("Range", "bytes=99-")
            .send()
            .await
            .expect("range request should succeed");

        assert_eq!(response.status(), StatusCode::RANGE_NOT_SATISFIABLE);
    }

    #[tokio::test]
    async fn malformed_blob_digest_is_not_served_from_cache_path() {
        let dir = tempdir().expect("tempdir should create");
        let store = Arc::new(
            Store::open(dir.path().to_path_buf())
                .await
                .expect("store should open"),
        );
        let address = spawn_server(store, false).await;
        let path = format!(
            "http://{}/v2/{}/blobs/sha256:%2E%2E%2Foutside",
            address,
            cache_repository("registry-1.docker.io", "library/alpine"),
        );

        let response = reqwest::get(path).await.expect("request should succeed");

        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn pull_missing_fetches_manifest_from_upstream() {
        let upstream_body = br#"{"schemaVersion":2,"mediaType":"application/vnd.oci.image.manifest.v1+json","config":{"digest":"sha256:2222222222222222222222222222222222222222222222222222222222222222"},"layers":[]}"#;
        let upstream = spawn_upstream_manifest(upstream_body).await;
        let dir = tempdir().expect("tempdir should create");
        let store = Arc::new(
            Store::open(dir.path().to_path_buf())
                .await
                .expect("store should open"),
        );
        let address = spawn_server(store, true).await;
        let path = format!(
            "http://{}/v2/{}/manifests/latest",
            address,
            cache_repository(&upstream.to_string(), "sample")
        );

        let response = reqwest::get(path).await.expect("request should succeed");

        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn puller_uses_cache_from_server() {
        let server_dir = tempdir().expect("server tempdir should create");
        let server_store = Arc::new(
            Store::open(server_dir.path().to_path_buf())
                .await
                .expect("server store should open"),
        );
        let config = br#"{"rootfs":{"diff_ids":[]}}"#;
        let config_descriptor = Descriptor {
            media_type: "application/vnd.oci.image.config.v1+json".into(),
            digest: sha256_digest(config),
            size: config.len() as i64,
            platform: None,
            annotations: None,
        };
        let manifest = format!(
            r#"{{"schemaVersion":2,"mediaType":"application/vnd.oci.image.manifest.v1+json","config":{{"mediaType":"{}","digest":"{}","size":{}}},"layers":[]}}"#,
            config_descriptor.media_type, config_descriptor.digest, config_descriptor.size,
        );
        let manifest_descriptor = Descriptor {
            media_type: "application/vnd.oci.image.manifest.v1+json".into(),
            digest: sha256_digest(manifest.as_bytes()),
            size: manifest.len() as i64,
            platform: None,
            annotations: None,
        };
        server_store
            .save_blob_bytes(&config_descriptor, config)
            .await
            .expect("config should save");
        server_store
            .save_blob_bytes(&manifest_descriptor, manifest.as_bytes())
            .await
            .expect("manifest should save");
        server_store
            .save_reference(&StoredReference {
                reference: "registry-1.docker.io/library/alpine:latest".into(),
                manifest: manifest_descriptor,
                config_digest: config_descriptor.digest.clone(),
            })
            .await
            .expect("reference should save");
        let address = spawn_server(server_store, false).await;

        let client_dir = tempdir().expect("client tempdir should create");
        let client_store = Arc::new(
            Store::open(client_dir.path().to_path_buf())
                .await
                .expect("client store should open"),
        );
        let registry = Arc::new(RegistryClient::new_with_cache_from(
            reqwest::Client::builder()
                .https_only(false)
                .build()
                .expect("client should build"),
            Arc::new(AuthResolver::new(None).expect("auth resolver should build")),
            true,
            Some(0),
            Some(
                format!("http://{address}")
                    .parse()
                    .expect("cache URL should parse"),
            ),
            true,
        ));
        let puller = Puller::new(PullContext {
            store: Arc::clone(&client_store),
            registry,
            stop: Arc::new(AtomicBool::new(false)),
            ui: Arc::new(Ui::new(true, false)),
            blob_retry_limit: Some(1),
            blob_locks: Arc::new(BlobDownloadLocks::default()),
        });

        puller
            .pull(
                ImageReference::parse("alpine:latest").expect("reference should parse"),
                PullOptions {
                    platform: Platform::host(),
                    concurrency: 1,
                    no_load: true,
                    keep_layer_blobs: true,
                    load_mode: crate::pull::LoadMode::Stream,
                },
            )
            .await
            .expect("pull through cache server should succeed");

        assert!(
            client_store
                .read_blob_bytes_if_complete(&config_descriptor)
                .await
                .expect("config lookup should succeed")
                .is_some()
        );
        assert!(
            client_store
                .load_reference("registry-1.docker.io/library/alpine:latest")
                .await
                .expect("reference lookup should succeed")
                .is_some()
        );
    }

    async fn spawn_server(store: Arc<Store>, pull_missing: bool) -> std::net::SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("listener should bind");
        let address = listener.local_addr().expect("listener address");
        let registry = Arc::new(RegistryClient::new(
            reqwest::Client::builder()
                .https_only(false)
                .build()
                .expect("client should build"),
            Arc::new(AuthResolver::new(None).expect("auth resolver should build")),
            true,
            Some(0),
        ));
        let state = Arc::new(ServeState {
            store,
            registry,
            pull_missing,
            blob_retry_limit: Some(1),
            quiet: true,
            downloads: Arc::new(Semaphore::new(1)),
            blob_locks: Arc::new(BlobDownloadLocks::default()),
        });
        tokio::spawn(async move {
            let _ = run_server(listener, state, None).await;
        });
        address
    }

    async fn spawn_upstream_manifest(body: &'static [u8]) -> std::net::SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("listener should bind");
        let address = listener.local_addr().expect("listener address");
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("connection should arrive");
            let mut buffer = [0_u8; 4096];
            let _ = stream.read(&mut buffer).await;
            stream
                .write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/vnd.oci.image.manifest.v1+json\r\nDocker-Content-Digest: sha256:5ae1916d0eccd953c0f9dec4f51f6bc7d16b1a2225a166aa2fcfd3f4322d4d24\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        body.len(),
                        std::str::from_utf8(body).expect("body should be utf8")
                    )
                    .as_bytes(),
                )
                .await
                .expect("response should be written");
        });
        address
    }

    fn sha256_digest(bytes: &[u8]) -> String {
        use sha2::{Digest as _, Sha256};

        let mut hasher = Sha256::new();
        hasher.update(bytes);
        format!("sha256:{}", hex::encode(hasher.finalize()))
    }
}
