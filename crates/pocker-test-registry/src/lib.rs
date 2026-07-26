//! Minimal OCI registry fixture shared by pocker's integration tests.

use std::io;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use sha2::{Digest, Sha256, Sha384, Sha512};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Notify;
use tokio::task::JoinHandle;

const REQUEST_HEAD_LIMIT: usize = 8 * 1024;
const FIXTURE_WAIT_TIMEOUT: Duration = Duration::from_secs(10);
const DEFAULT_LAYER_PAYLOAD: &[u8] = b"fake layer payload";

/// Generated single-layer OCI image content served by [`TestRegistry`].
#[derive(Debug, Clone)]
pub struct TestImage {
    manifest_bytes: Vec<u8>,
    manifest_digest: String,
    config_bytes: Vec<u8>,
    config_digest: String,
    layer_bytes: Vec<u8>,
    layer_descriptor_digest: String,
    layer_digest: String,
}

impl TestImage {
    /// Builds an image with a small SHA-256 layer.
    pub fn single_layer() -> Self {
        Self::with_layer_payload(DEFAULT_LAYER_PAYLOAD.to_vec())
    }

    /// Builds an image whose layer descriptor uses the requested digest algorithm.
    pub fn with_layer_algorithm(algorithm: &str) -> Self {
        let layer_bytes = DEFAULT_LAYER_PAYLOAD.to_vec();
        let layer_digest = digest_hex(algorithm, &layer_bytes);
        Self::build(layer_bytes, format!("{algorithm}:{layer_digest}"))
    }

    /// Builds an image with caller-supplied layer bytes and a SHA-256 descriptor.
    pub fn with_layer_payload(layer_bytes: Vec<u8>) -> Self {
        let layer_digest = sha256_hex(&layer_bytes);
        Self::build(layer_bytes, format!("sha256:{layer_digest}"))
    }

    /// Builds an image with a caller-supplied descriptor for malformed-input tests.
    pub fn with_layer_descriptor_digest(layer_descriptor_digest: String) -> Self {
        Self::build(DEFAULT_LAYER_PAYLOAD.to_vec(), layer_descriptor_digest)
    }

    fn build(layer_bytes: Vec<u8>, layer_descriptor_digest: String) -> Self {
        let layer_diff_id = sha256_hex(&layer_bytes);
        let layer_digest = layer_descriptor_digest
            .split_once(':')
            .map(|(_, value)| value.to_string())
            .unwrap_or_else(|| layer_descriptor_digest.clone());
        let config_bytes = format!(
            r#"{{"architecture":"amd64","os":"linux","rootfs":{{"type":"layers","diff_ids":["sha256:{layer_diff_id}"]}}}}"#
        )
        .into_bytes();
        let config_digest = sha256_hex(&config_bytes);
        let manifest_bytes = format!(
            r#"{{"schemaVersion":2,"mediaType":"application/vnd.oci.image.manifest.v1+json","config":{{"mediaType":"application/vnd.oci.image.config.v1+json","digest":"sha256:{config_digest}","size":{config_size}}},"layers":[{{"mediaType":"application/vnd.oci.image.layer.v1.tar","digest":"{layer_descriptor_digest}","size":{layer_size}}}]}}"#,
            config_size = config_bytes.len(),
            layer_size = layer_bytes.len(),
        )
        .into_bytes();
        let manifest_digest = sha256_hex(&manifest_bytes);

        Self {
            manifest_bytes,
            manifest_digest,
            config_bytes,
            config_digest,
            layer_bytes,
            layer_descriptor_digest,
            layer_digest,
        }
    }

    /// Returns the bare manifest digest hex used as its cache path component.
    pub fn manifest_digest(&self) -> &str {
        &self.manifest_digest
    }

    /// Returns the bare config digest hex used as its cache path component.
    pub fn config_digest(&self) -> &str {
        &self.config_digest
    }

    /// Returns the bare layer digest hex used as its cache path component.
    pub fn layer_digest(&self) -> &str {
        &self.layer_digest
    }
}

/// Loopback OCI registry with request counters and optional response gating.
#[derive(Debug)]
pub struct TestRegistry {
    address: SocketAddr,
    state: Arc<ServerState>,
    task: JoinHandle<()>,
}

impl TestRegistry {
    /// Starts a registry that serves `image` without blocking response bodies.
    pub async fn start(image: TestImage) -> Self {
        Self::start_inner(ResponseMode::Image(image), false).await
    }

    /// Starts a registry whose layer body stays blocked until [`Self::release_layer`].
    ///
    /// The gate is global and one-shot: releasing it unblocks all current and
    /// future layer responses for this registry instance.
    pub async fn start_with_blocked_layer(image: TestImage) -> Self {
        Self::start_inner(ResponseMode::Image(image), true).await
    }

    /// Starts a registry that returns `503` with `Retry-After: 0` for every request.
    pub async fn start_unavailable() -> Self {
        Self::start_inner(ResponseMode::Unavailable, false).await
    }

    async fn start_inner(response: ResponseMode, block_layer: bool) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("test registry should bind");
        let address = listener
            .local_addr()
            .expect("test registry listener should expose its address");
        let state = Arc::new(ServerState {
            response,
            manifest_gets: AtomicUsize::new(0),
            manifest_get_notify: Notify::new(),
            layer_gets: AtomicUsize::new(0),
            layer_get_notify: Notify::new(),
            layer_gate: block_layer.then(LayerGate::new),
            unexpected_requests: Mutex::new(Vec::new()),
        });
        let server_state = Arc::clone(&state);
        let task = tokio::spawn(async move {
            while let Ok((stream, _peer)) = listener.accept().await {
                let state = Arc::clone(&server_state);
                tokio::spawn(async move {
                    let _ = handle_connection(stream, state).await;
                });
            }
        });

        Self {
            address,
            state,
            task,
        }
    }

    /// Returns an image reference pointing at this registry.
    pub fn reference(&self, repository: &str, tag: &str) -> String {
        format!("{}/{repository}:{tag}", self.address)
    }

    /// Returns the number of layer `GET` requests observed.
    pub fn layer_get_count(&self) -> usize {
        self.state.layer_gets.load(Ordering::SeqCst)
    }

    /// Waits for at least `expected` manifest `GET` requests.
    pub async fn wait_for_manifest_gets(&self, expected: usize) {
        wait_for_count(
            &self.state.manifest_gets,
            &self.state.manifest_get_notify,
            expected,
            "manifest",
        )
        .await;
    }

    /// Waits for at least `expected` layer `GET` requests.
    pub async fn wait_for_layer_gets(&self, expected: usize) {
        wait_for_count(
            &self.state.layer_gets,
            &self.state.layer_get_notify,
            expected,
            "layer",
        )
        .await;
    }

    /// Permanently releases the registry's one-shot layer response gate.
    pub fn release_layer(&self) {
        if let Some(gate) = &self.state.layer_gate {
            gate.release();
        }
    }

    /// Returns requests that did not match a fixture route.
    pub fn unexpected_requests(&self) -> Vec<String> {
        self.state
            .unexpected_requests
            .lock()
            .expect("unexpected request log poisoned")
            .clone()
    }
}

impl Drop for TestRegistry {
    fn drop(&mut self) {
        self.release_layer();
        self.task.abort();
    }
}

#[derive(Debug)]
struct ServerState {
    response: ResponseMode,
    manifest_gets: AtomicUsize,
    manifest_get_notify: Notify,
    layer_gets: AtomicUsize,
    layer_get_notify: Notify,
    layer_gate: Option<LayerGate>,
    unexpected_requests: Mutex<Vec<String>>,
}

#[derive(Debug)]
enum ResponseMode {
    Image(TestImage),
    Unavailable,
}

#[derive(Debug)]
struct LayerGate {
    released: AtomicBool,
    notify: Notify,
}

impl LayerGate {
    fn new() -> Self {
        Self {
            released: AtomicBool::new(false),
            notify: Notify::new(),
        }
    }

    async fn wait(&self) {
        loop {
            let notified = self.notify.notified();
            if self.released.load(Ordering::Acquire) {
                return;
            }
            notified.await;
        }
    }

    fn release(&self) {
        self.released.store(true, Ordering::Release);
        self.notify.notify_waiters();
    }
}

async fn wait_for_count(count: &AtomicUsize, notify: &Notify, expected: usize, resource: &str) {
    let wait = async {
        loop {
            let notified = notify.notified();
            if count.load(Ordering::SeqCst) >= expected {
                return;
            }
            notified.await;
        }
    };
    tokio::time::timeout(FIXTURE_WAIT_TIMEOUT, wait)
        .await
        .unwrap_or_else(|_| panic!("test registry did not receive {expected} {resource} requests"));
}

async fn handle_connection(mut stream: TcpStream, state: Arc<ServerState>) -> io::Result<()> {
    let (method, path) = read_request(&mut stream).await?;
    let head_only = method == "HEAD";

    if matches!(&state.response, ResponseMode::Unavailable) {
        return write_unavailable(&mut stream).await;
    }
    let ResponseMode::Image(image) = &state.response else {
        unreachable!("unavailable responses return before image routing")
    };

    if path == "/v2/" {
        return write_response(&mut stream, "200 OK", "text/plain", &[], None, head_only).await;
    }
    if path.contains("/manifests/") {
        if !head_only {
            state.manifest_gets.fetch_add(1, Ordering::SeqCst);
            state.manifest_get_notify.notify_waiters();
        }
        let digest = format!("sha256:{}", image.manifest_digest);
        return write_response(
            &mut stream,
            "200 OK",
            "application/vnd.oci.image.manifest.v1+json",
            &image.manifest_bytes,
            Some(&digest),
            head_only,
        )
        .await;
    }
    if path.ends_with(&format!("sha256:{}", image.config_digest)) {
        let digest = format!("sha256:{}", image.config_digest);
        return write_response(
            &mut stream,
            "200 OK",
            "application/vnd.oci.image.config.v1+json",
            &image.config_bytes,
            Some(&digest),
            head_only,
        )
        .await;
    }
    if path.ends_with(&image.layer_descriptor_digest) {
        if !head_only {
            state.layer_gets.fetch_add(1, Ordering::SeqCst);
            state.layer_get_notify.notify_waiters();
        }
        write_response_head(
            &mut stream,
            "200 OK",
            "application/vnd.oci.image.layer.v1.tar",
            image.layer_bytes.len(),
            Some(&image.layer_descriptor_digest),
        )
        .await?;
        if !head_only {
            if let Some(gate) = &state.layer_gate {
                gate.wait().await;
            }
            stream.write_all(&image.layer_bytes).await?;
        }
        stream.shutdown().await.ok();
        return Ok(());
    }

    state
        .unexpected_requests
        .lock()
        .expect("unexpected request log poisoned")
        .push(format!("{method} {path}"));
    write_response(
        &mut stream,
        "404 Not Found",
        "text/plain",
        &[],
        None,
        head_only,
    )
    .await
}

async fn write_unavailable(stream: &mut TcpStream) -> io::Result<()> {
    let response = "HTTP/1.1 503 Service Unavailable\r\nRetry-After: 0\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
    stream.write_all(response.as_bytes()).await?;
    stream.shutdown().await.ok();
    Ok(())
}

async fn read_request(stream: &mut TcpStream) -> io::Result<(String, String)> {
    let mut buffer = Vec::with_capacity(2 * 1024);
    let mut chunk = [0_u8; 1024];
    loop {
        let read = stream.read(&mut chunk).await?;
        if read == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "request ended before its headers",
            ));
        }
        buffer.extend_from_slice(&chunk[..read]);
        if buffer.windows(4).any(|window| window == b"\r\n\r\n") {
            break;
        }
        if buffer.len() > REQUEST_HEAD_LIMIT {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "request headers exceeded fixture limit",
            ));
        }
    }

    let head = std::str::from_utf8(&buffer)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    let mut tokens = head
        .split("\r\n")
        .next()
        .unwrap_or_default()
        .split_whitespace();
    Ok((
        tokens.next().unwrap_or_default().to_string(),
        tokens.next().unwrap_or_default().to_string(),
    ))
}

async fn write_response(
    stream: &mut TcpStream,
    status: &str,
    content_type: &str,
    body: &[u8],
    digest: Option<&str>,
    head_only: bool,
) -> io::Result<()> {
    write_response_head(stream, status, content_type, body.len(), digest).await?;
    if !head_only {
        stream.write_all(body).await?;
    }
    stream.shutdown().await.ok();
    Ok(())
}

async fn write_response_head(
    stream: &mut TcpStream,
    status: &str,
    content_type: &str,
    content_length: usize,
    digest: Option<&str>,
) -> io::Result<()> {
    let digest_header = digest
        .map(|digest| format!("Docker-Content-Digest: {digest}\r\n"))
        .unwrap_or_default();
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {content_length}\r\nDocker-Distribution-API-Version: registry/2.0\r\n{digest_header}Connection: close\r\n\r\n"
    );
    stream.write_all(response.as_bytes()).await
}

fn sha256_hex(bytes: &[u8]) -> String {
    digest_hex("sha256", bytes)
}

fn digest_hex(algorithm: &str, bytes: &[u8]) -> String {
    match algorithm {
        "sha256" => hash_hex::<Sha256>(bytes),
        "sha384" => hash_hex::<Sha384>(bytes),
        "sha512" => hash_hex::<Sha512>(bytes),
        other => panic!("unsupported test digest algorithm {other}"),
    }
}

fn hash_hex<D: Digest>(bytes: &[u8]) -> String {
    let mut hasher = D::new();
    hasher.update(bytes);
    hex::encode(hasher.finalize())
}
