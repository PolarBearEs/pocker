use std::future::Future;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use tokio::fs as tokio_fs;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Semaphore, oneshot};
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;
use tracing::warn;

use crate::digest::parse_digest;
use crate::error::{DockerPullError, Result};
use crate::platform::Platform;
use crate::pull::{BlobDownloadLocks, CurrentPullLayers, PullContext, download};
use crate::reference::ImageReference;
use crate::registry::{
    Descriptor, MANIFEST_ACCEPT, OCI_IMAGE_MANIFEST_MEDIA_TYPE, OCTET_STREAM_MEDIA_TYPE,
    RegistryClient, decode_cache_repository,
};
use crate::store::{Store, StoredReference};
use crate::ui::Ui;

use super::request::{Request, read_request};
use super::response::{RegistryResponse, write_response};

const MAX_CONNECTIONS: usize = 1024;

pub struct ServeConfig {
    pub listen: SocketAddr,
    pub store: Arc<Store>,
    pub registry: Arc<RegistryClient>,
    pub pull_missing: bool,
    pub blob_retry_limit: Option<u32>,
    pub blob_idle_timeout: Option<Duration>,
    pub concurrency: usize,
    pub quiet: bool,
    pub shutdown: Option<oneshot::Receiver<()>>,
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
            blob_idle_timeout: config.blob_idle_timeout,
            concurrency: config.concurrency,
            quiet: config.quiet,
        },
        config.shutdown,
    )
    .await
}

pub(crate) struct ServeListenerConfig {
    pub store: Arc<Store>,
    pub registry: Arc<RegistryClient>,
    pub pull_missing: bool,
    pub blob_retry_limit: Option<u32>,
    pub blob_idle_timeout: Option<Duration>,
    pub concurrency: usize,
    pub quiet: bool,
}

pub(crate) async fn serve_listener(
    listener: TcpListener,
    config: ServeListenerConfig,
    shutdown: Option<oneshot::Receiver<()>>,
) -> Result<()> {
    let state = Arc::new(ServeState {
        store: Arc::clone(&config.store),
        registry: Arc::clone(&config.registry),
        pull_missing: config.pull_missing,
        downloads: Arc::new(Semaphore::new(config.concurrency)),
        pull_context: Arc::new(PullContext {
            store: config.store,
            registry: config.registry,
            stop: CancellationToken::new(),
            ui: Arc::new(Ui::new(config.quiet, false)),
            blob_retry_limit: config.blob_retry_limit,
            blob_idle_timeout: config.blob_idle_timeout,
            blob_locks: Arc::new(BlobDownloadLocks::default()),
            layer_usage: Arc::new(CurrentPullLayers::default()),
            daemon_layer_cache: None,
        }),
    });
    run_server(listener, state, shutdown).await
}

async fn run_server(
    listener: TcpListener,
    state: Arc<ServeState>,
    mut shutdown: Option<oneshot::Receiver<()>>,
) -> Result<()> {
    let mut connections = JoinSet::new();
    loop {
        tokio::select! {
            result = listener.accept(), if connections.len() < MAX_CONNECTIONS => {
                let (stream, _) = result?;
                let state = Arc::clone(&state);
                connections.spawn(async move {
                    if let Err(error) = handle_connection(stream, state).await {
                        warn!("cache registry connection failed: {error}");
                    }
                });
            }
            Some(result) = connections.join_next(), if !connections.is_empty() => {
                if let Err(error) = result {
                    warn!("cache registry connection task failed: {error}");
                }
            }
            _ = async {
                if let Some(shutdown) = shutdown.as_mut() {
                    let _ = shutdown.await;
                } else {
                    std::future::pending::<()>().await;
                }
            } => {
                state.pull_context.stop.cancel();
                connections.abort_all();
                while connections.join_next().await.is_some() {}
                break;
            },
        }
    }
    Ok(())
}

struct ServeState {
    store: Arc<Store>,
    registry: Arc<RegistryClient>,
    pull_missing: bool,
    downloads: Arc<Semaphore>,
    pull_context: Arc<PullContext>,
}

async fn handle_connection(mut stream: TcpStream, state: Arc<ServeState>) -> Result<()> {
    let request = read_request(&mut stream).await?;
    let response = route_request(&request, state).await;
    write_response(&mut stream, response).await
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

    cached_or_fetched(
        cached_manifest_response(&state, &decoded, reference, request.method == "HEAD").await,
        state.pull_missing,
        || fetch_manifest_response(&state, &decoded, reference, request.method == "HEAD"),
        |error| matches!(error, DockerPullError::ManifestNotFound),
    )
    .await
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

    cached_or_fetched(
        cached_blob_response(&state, digest, request).await,
        state.pull_missing,
        || fetch_blob_response(&state, &decoded, digest, request),
        |error| matches!(error, DockerPullError::BlobNotFound(_)),
    )
    .await
}

async fn cached_or_fetched<F, Fut, IsNotFound>(
    cached: Result<Option<RegistryResponse>>,
    pull_missing: bool,
    fetch: F,
    is_not_found: IsNotFound,
) -> RegistryResponse
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = Result<RegistryResponse>>,
    IsNotFound: Fn(&DockerPullError) -> bool,
{
    match cached {
        Ok(Some(response)) => return response,
        Ok(None) => {}
        Err(error) => {
            return RegistryResponse::text(500, "Internal Server Error", error.to_string());
        }
    }

    if !pull_missing {
        return RegistryResponse::empty(404, "Not Found");
    }

    match fetch().await {
        Ok(response) => response,
        Err(error) if is_not_found(&error) => RegistryResponse::empty(404, "Not Found"),
        Err(error) => RegistryResponse::text(502, "Bad Gateway", error.to_string()),
    }
}

async fn cached_manifest_response(
    state: &ServeState,
    reference: &ImageReference,
    requested_reference: &str,
    headers_only: bool,
) -> Result<Option<RegistryResponse>> {
    let normalized = reference.normalized();
    if let Some(record) = state.store.load_reference(&normalized).await? {
        return Ok(Some(
            serve_manifest_blob(state, &record.manifest, headers_only).await,
        ));
    }

    let Ok(path) = state.store.blob_path(requested_reference) else {
        return Ok(None);
    };
    if tokio_fs::try_exists(&path).await? {
        let descriptor = manifest_descriptor_from_blob(&state.store, requested_reference).await?;
        return Ok(Some(
            serve_manifest_blob(state, &descriptor, headers_only).await,
        ));
    }

    Ok(None)
}

async fn fetch_manifest_response(
    state: &ServeState,
    reference: &ImageReference,
    requested_reference: &str,
    headers_only: bool,
) -> Result<RegistryResponse> {
    let descriptor = fetch_manifest(state, reference, requested_reference).await?;
    Ok(serve_manifest_blob(state, &descriptor, headers_only).await)
}

async fn cached_blob_response(
    state: &ServeState,
    digest: &str,
    request: &Request,
) -> Result<Option<RegistryResponse>> {
    let Ok((path, size)) = blob_path_and_size(&state.store, digest).await else {
        return Ok(None);
    };
    Ok(Some(blob_file_response(path, size, digest, request)))
}

async fn fetch_blob_response(
    state: &ServeState,
    reference: &ImageReference,
    digest: &str,
    request: &Request,
) -> Result<RegistryResponse> {
    fetch_blob(state, reference, digest).await?;
    let (path, size) = blob_path_and_size(&state.store, digest).await?;
    Ok(blob_file_response(path, size, digest, request))
}

fn blob_file_response(
    path: PathBuf,
    size: u64,
    digest: &str,
    request: &Request,
) -> RegistryResponse {
    RegistryResponse::file(
        200,
        "OK",
        OCTET_STREAM_MEDIA_TYPE.to_string(),
        path,
        size,
        request.method == "HEAD",
    )
    .with_digest(digest)
    .with_range(request.range.as_deref())
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
            config_digest: manifest_summary(&raw.bytes)
                .config_digest
                .unwrap_or_default(),
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
            media_type: OCTET_STREAM_MEDIA_TYPE.to_string(),
            digest: digest.to_string(),
            size: bytes.len() as i64,
            platform: None,
            annotations: None,
        };
        return state.store.save_blob_bytes(&descriptor, &bytes).await;
    };
    let descriptor = Descriptor {
        media_type: OCTET_STREAM_MEDIA_TYPE.to_string(),
        digest: digest.to_string(),
        size: size as i64,
        platform: None,
        annotations: None,
    };
    download::download_blob(&state.pull_context, reference, descriptor).await
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
        OCI_IMAGE_MANIFEST_MEDIA_TYPE.to_string()
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
    parse_digest(reference).is_ok()
}

fn split_route<'a>(path: &'a str, separator: &str) -> Option<(&'a str, &'a str)> {
    path.rsplit_once(separator)
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
    let summary = manifest_summary(&bytes);
    Ok(Descriptor {
        media_type: summary.media_type.unwrap_or_default(),
        digest: digest.to_string(),
        size: bytes.len() as i64,
        platform: Some(Platform::host()),
        annotations: None,
    })
}

#[derive(Debug, Default, PartialEq, Eq)]
struct ManifestSummary {
    media_type: Option<String>,
    config_digest: Option<String>,
}

fn manifest_summary(bytes: &[u8]) -> ManifestSummary {
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(bytes) else {
        return ManifestSummary::default();
    };
    ManifestSummary {
        media_type: value
            .get("mediaType")
            .and_then(|value| value.as_str())
            .map(ToString::to_string),
        config_digest: value
            .get("config")
            .and_then(|value| value.get("digest"))
            .and_then(|value| value.as_str())
            .map(ToString::to_string),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use reqwest::StatusCode;
    use tempfile::tempdir;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;
    use tokio::sync::Semaphore;
    use tokio_util::sync::CancellationToken;

    use super::{
        ManifestSummary, ServeState, is_supported_digest_reference, manifest_summary, run_server,
        split_route,
    };
    use crate::auth::AuthResolver;
    use crate::platform::Platform;
    use crate::pull::{BlobDownloadLocks, CurrentPullLayers, PullContext, PullOptions, Puller};
    use crate::reference::ImageReference;
    use crate::registry::{Descriptor, RegistryClient, cache_repository};
    use crate::serve_registry::response::parse_byte_range;
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
            "sha384:{}",
            "b".repeat(96)
        )));
        assert!(is_supported_digest_reference(&format!(
            "sha256:{}",
            "c".repeat(64)
        )));
        assert!(!is_supported_digest_reference(&format!(
            "sha256:{}",
            "A".repeat(64)
        )));
        assert!(!is_supported_digest_reference("sha384:abc"));
        assert!(!is_supported_digest_reference(&format!(
            "sha224:{}",
            "a".repeat(56)
        )));
        assert!(!is_supported_digest_reference("sha512:"));
    }

    #[test]
    fn split_route_uses_last_route_separator() {
        let (repository, reference) = split_route(
            "registry.test/team/manifests/app/manifests/latest",
            "/manifests/",
        )
        .expect("route should split");

        assert_eq!(repository, "registry.test/team/manifests/app");
        assert_eq!(reference, "latest");
    }

    #[test]
    fn manifest_summary_reads_media_type_and_config_digest() {
        let summary = manifest_summary(
            br#"{"mediaType":"application/vnd.oci.image.manifest.v1+json","config":{"digest":"sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"}}"#,
        );

        assert_eq!(
            summary,
            ManifestSummary {
                media_type: Some("application/vnd.oci.image.manifest.v1+json".into()),
                config_digest: Some(
                    "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
                        .into()
                ),
            }
        );
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
        let reference = ImageReference::parse("alpine:latest").expect("reference should parse");
        let resolved = registry
            .resolve_image(&reference, &Platform::host())
            .await
            .expect("image should resolve through cache server");
        let puller = Puller::new(PullContext {
            store: Arc::clone(&client_store),
            registry,
            stop: CancellationToken::new(),
            ui: Arc::new(Ui::new(true, false)),
            blob_retry_limit: Some(1),
            blob_idle_timeout: None,
            blob_locks: Arc::new(BlobDownloadLocks::default()),
            layer_usage: Arc::new(CurrentPullLayers::default()),
            daemon_layer_cache: None,
        });

        puller
            .pull_resolved(
                reference,
                resolved,
                PullOptions {
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
            store: Arc::clone(&store),
            registry: Arc::clone(&registry),
            pull_missing,
            downloads: Arc::new(Semaphore::new(1)),
            pull_context: Arc::new(PullContext {
                store,
                registry,
                stop: CancellationToken::new(),
                ui: Arc::new(Ui::new(true, false)),
                blob_retry_limit: Some(1),
                blob_idle_timeout: None,
                blob_locks: Arc::new(BlobDownloadLocks::default()),
                layer_usage: Arc::new(CurrentPullLayers::default()),
                daemon_layer_cache: None,
            }),
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
        crate::digest::canonical_digest_bytes(bytes)
    }
}
