#[cfg(unix)]
use std::io::Read;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use assert_cmd::Command;
#[cfg(unix)]
use portable_pty::{CommandBuilder, NativePtySystem, PtySize, PtySystem};
use predicates::str::contains;
use sha2::{Digest, Sha256, Sha384, Sha512};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pulls_oci_image_from_fake_registry_into_cache() {
    let fixture = Arc::new(Fixture::build());
    let server = RegistryFixtureServer::spawn(Arc::clone(&fixture)).await;

    let cache_dir = tempfile::tempdir().expect("cache tempdir should create");
    let reference = format!("{}/library/test:latest", server.address());

    let pocker_run = tokio::task::spawn_blocking({
        let cache = cache_dir.path().to_path_buf();
        move || {
            Command::cargo_bin("pocker")
                .expect("pocker binary should be built")
                .arg("--cache-dir")
                .arg(&cache)
                .args([
                    "pull",
                    "--plain-http",
                    "--no-load",
                    "--quiet",
                    "--request-retries",
                    "0",
                    "--blob-retries",
                    "0",
                ])
                .arg(&reference)
                .timeout(Duration::from_secs(30))
                .assert()
                .success();
        }
    });

    pocker_run.await.expect("pocker subprocess should run");

    let manifest_path = cache_dir
        .path()
        .join("blobs")
        .join("sha256")
        .join(&fixture.manifest_digest);
    let layer_path = cache_dir
        .path()
        .join("blobs")
        .join("sha256")
        .join(&fixture.layer_digest);
    let config_path = cache_dir
        .path()
        .join("blobs")
        .join("sha256")
        .join(&fixture.config_digest);

    assert!(
        manifest_path.exists(),
        "expected manifest blob at {manifest_path:?}"
    );
    assert!(
        config_path.exists(),
        "expected config blob at {config_path:?}"
    );
    assert!(layer_path.exists(), "expected layer blob at {layer_path:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pulls_multiple_oci_images_from_fake_registry_into_cache() {
    let fixture = Arc::new(Fixture::build());
    let layer_gets = Arc::new(AtomicUsize::new(0));
    let server =
        RegistryFixtureServer::spawn_with_layer_gets(Arc::clone(&fixture), Arc::clone(&layer_gets))
            .await;

    let cache_dir = tempfile::tempdir().expect("cache tempdir should create");
    let first_reference = format!("{}/library/test:first", server.address());
    let second_reference = format!("{}/library/test:second", server.address());

    let pocker_run = tokio::task::spawn_blocking({
        let cache = cache_dir.path().to_path_buf();
        move || {
            Command::cargo_bin("pocker")
                .expect("pocker binary should be built")
                .arg("--cache-dir")
                .arg(&cache)
                .args([
                    "pull",
                    "--plain-http",
                    "--no-load",
                    "--quiet",
                    "--request-retries",
                    "0",
                    "--blob-retries",
                    "0",
                ])
                .arg(&first_reference)
                .arg(&second_reference)
                .timeout(Duration::from_secs(30))
                .assert()
                .success();
        }
    });

    pocker_run.await.expect("pocker subprocess should run");

    let manifest_path = cache_dir
        .path()
        .join("blobs")
        .join("sha256")
        .join(&fixture.manifest_digest);
    let layer_path = cache_dir
        .path()
        .join("blobs")
        .join("sha256")
        .join(&fixture.layer_digest);
    let config_path = cache_dir
        .path()
        .join("blobs")
        .join("sha256")
        .join(&fixture.config_digest);

    assert!(
        manifest_path.exists(),
        "expected manifest blob at {manifest_path:?}"
    );
    assert!(
        config_path.exists(),
        "expected config blob at {config_path:?}"
    );
    assert!(layer_path.exists(), "expected layer blob at {layer_path:?}");
    assert_eq!(
        layer_gets.load(Ordering::SeqCst),
        1,
        "shared layer blob should be downloaded once across concurrent image pulls"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pulls_oci_image_with_sha384_layer_digest_into_cache() {
    pulls_oci_image_with_layer_algorithm_into_cache("sha384").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pulls_oci_image_with_sha512_layer_digest_into_cache() {
    pulls_oci_image_with_layer_algorithm_into_cache("sha512").await;
}

async fn pulls_oci_image_with_layer_algorithm_into_cache(algorithm: &'static str) {
    let fixture = Arc::new(Fixture::build_with_layer_algorithm(algorithm));
    let server = RegistryFixtureServer::spawn(Arc::clone(&fixture)).await;

    let cache_dir = tempfile::tempdir().expect("cache tempdir should create");
    let reference = format!("{}/library/test:latest", server.address());

    let pocker_run = tokio::task::spawn_blocking({
        let cache = cache_dir.path().to_path_buf();
        move || {
            Command::cargo_bin("pocker")
                .expect("pocker binary should be built")
                .arg("--cache-dir")
                .arg(&cache)
                .args([
                    "pull",
                    "--plain-http",
                    "--no-load",
                    "--quiet",
                    "--request-retries",
                    "0",
                    "--blob-retries",
                    "0",
                ])
                .arg(&reference)
                .timeout(Duration::from_secs(30))
                .assert()
                .success();
        }
    });

    pocker_run.await.expect("pocker subprocess should run");

    let layer_path = cache_dir
        .path()
        .join("blobs")
        .join(algorithm)
        .join(&fixture.layer_digest);

    assert!(layer_path.exists(), "expected layer blob at {layer_path:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pull_rejects_malformed_layer_digest_before_cache_path_use() {
    let fixture = Arc::new(Fixture::build_with_layer_digest(
        "sha256:../../outside".to_string(),
    ));
    let server = RegistryFixtureServer::spawn(Arc::clone(&fixture)).await;

    let cache_dir = tempfile::tempdir().expect("cache tempdir should create");
    let reference = format!("{}/library/test:latest", server.address());

    let pocker_run = tokio::task::spawn_blocking({
        let cache = cache_dir.path().to_path_buf();
        move || {
            Command::cargo_bin("pocker")
                .expect("pocker binary should be built")
                .arg("--cache-dir")
                .arg(&cache)
                .args([
                    "pull",
                    "--plain-http",
                    "--no-load",
                    "--quiet",
                    "--request-retries",
                    "0",
                    "--blob-retries",
                    "0",
                ])
                .arg(&reference)
                .timeout(Duration::from_secs(30))
                .assert()
                .failure()
                .stderr(contains("invalid sha256 digest value"));
        }
    });

    pocker_run.await.expect("pocker subprocess should run");

    assert!(
        !cache_dir.path().join("outside").exists(),
        "malformed digest must not create paths outside the digest cache"
    );
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn compose_pull_progress_shows_layer_bars_on_tty() {
    let fixture = Arc::new(Fixture::build());
    let server = RegistryFixtureServer::spawn(Arc::clone(&fixture)).await;

    let dir = tempfile::tempdir().expect("tempdir should create");
    let cache_dir = dir.path().join("cache");
    let compose_path = dir.path().join("docker-compose.yml");
    let reference = format!("{}/library/test:latest", server.address());
    std::fs::write(
        &compose_path,
        format!("services:\n  app:\n    image: {reference}\n"),
    )
    .expect("compose file should write");

    let output = tokio::task::spawn_blocking({
        let cache_dir = cache_dir.clone();
        let compose_path = compose_path.clone();
        move || {
            run_pocker_in_pty(&[
                "--cache-dir",
                cache_dir.to_str().expect("cache path should be UTF-8"),
                "compose",
                "-f",
                compose_path.to_str().expect("compose path should be UTF-8"),
                "pull",
                "--plain-http",
                "--no-load",
                "--request-retries",
                "0",
                "--blob-retries",
                "0",
                "--max-parallel-images",
                "1",
                "app",
            ])
        }
    })
    .await
    .expect("pocker subprocess task should run")
    .expect("pocker should run in a pty");

    assert!(
        output.status.success(),
        "pocker should succeed; output:\n{}",
        output.transcript
    );
    assert!(
        output
            .transcript
            .contains(&format!("{} [", &fixture.layer_digest[..12])),
        "compose progress should render per-layer progress bars; output:\n{}",
        output.transcript
    );
}

struct RegistryFixtureServer {
    address: SocketAddr,
    task: JoinHandle<()>,
}

#[cfg(unix)]
struct PtyRunOutput {
    status: portable_pty::ExitStatus,
    transcript: String,
}

#[cfg(unix)]
const PTY_RUN_TIMEOUT: Duration = Duration::from_secs(30);
#[cfg(unix)]
const PTY_READER_DRAIN_TIMEOUT: Duration = Duration::from_secs(2);

#[cfg(unix)]
fn run_pocker_in_pty(
    args: &[&str],
) -> std::result::Result<PtyRunOutput, Box<dyn std::error::Error + Send + Sync>> {
    let pty_system = NativePtySystem::default();
    let pair = pty_system.openpty(PtySize {
        rows: 24,
        cols: 120,
        pixel_width: 0,
        pixel_height: 0,
    })?;
    let mut command = CommandBuilder::new(assert_cmd::cargo::cargo_bin("pocker"));
    for arg in args {
        command.arg(arg);
    }

    let mut child = pair.slave.spawn_command(command)?;
    drop(pair.slave);

    let mut reader = pair.master.try_clone_reader()?;
    let (reader_sender, reader_receiver) = std::sync::mpsc::channel();
    let reader_thread = std::thread::spawn(move || {
        let mut transcript = String::new();
        let result = reader
            .read_to_string(&mut transcript)
            .map(|_| transcript)
            .map_err(|error| error.to_string());
        let _ = reader_sender.send(result);
    });
    let deadline = std::time::Instant::now() + PTY_RUN_TIMEOUT;
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if std::time::Instant::now() >= deadline {
            let _ = child.kill();
            let transcript = reader_receiver
                .recv_timeout(PTY_READER_DRAIN_TIMEOUT)
                .ok()
                .and_then(std::result::Result::ok)
                .unwrap_or_default();
            return Err(format!(
                "pocker PTY run timed out after {:?}; output:\n{}",
                PTY_RUN_TIMEOUT, transcript
            )
            .into());
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    let transcript = reader_receiver.recv_timeout(PTY_READER_DRAIN_TIMEOUT)??;
    reader_thread
        .join()
        .map_err(|_| "PTY reader thread panicked")?;

    Ok(PtyRunOutput { status, transcript })
}

impl RegistryFixtureServer {
    async fn spawn(fixture: Arc<Fixture>) -> Self {
        Self::spawn_with_optional_layer_gets(fixture, None).await
    }

    async fn spawn_with_layer_gets(fixture: Arc<Fixture>, layer_gets: Arc<AtomicUsize>) -> Self {
        Self::spawn_with_optional_layer_gets(fixture, Some(layer_gets)).await
    }

    async fn spawn_with_optional_layer_gets(
        fixture: Arc<Fixture>,
        layer_gets: Option<Arc<AtomicUsize>>,
    ) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("fake registry should bind");
        let address = listener
            .local_addr()
            .expect("listener should expose address");

        let task = tokio::spawn(async move {
            loop {
                let (stream, _peer) = match listener.accept().await {
                    Ok(pair) => pair,
                    Err(_) => return,
                };
                let request_fixture = Arc::clone(&fixture);
                let request_layer_gets = layer_gets.clone();
                tokio::spawn(async move {
                    let _ = handle_connection_with_optional_layer_gets(
                        stream,
                        request_fixture,
                        request_layer_gets,
                    )
                    .await;
                });
            }
        });

        Self { address, task }
    }

    fn address(&self) -> SocketAddr {
        self.address
    }
}

impl Drop for RegistryFixtureServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

struct Fixture {
    manifest_bytes: Vec<u8>,
    manifest_digest: String,
    config_bytes: Vec<u8>,
    config_digest: String,
    layer_bytes: Vec<u8>,
    layer_digest: String,
}

impl Fixture {
    fn build() -> Self {
        Self::build_with_layer_algorithm("sha256")
    }

    fn build_with_layer_algorithm(algorithm: &str) -> Self {
        let layer_bytes = b"fake layer payload".to_vec();
        let layer_digest = digest_hex(algorithm, &layer_bytes);
        Self::build_with_layer_digest(format!("{algorithm}:{layer_digest}"))
    }

    fn build_with_layer_digest(layer_descriptor_digest: String) -> Self {
        let layer_bytes = b"fake layer payload".to_vec();
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
            layer_digest,
        }
    }
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

async fn handle_connection_with_optional_layer_gets(
    mut stream: TcpStream,
    fixture: Arc<Fixture>,
    layer_gets: Option<Arc<AtomicUsize>>,
) -> std::io::Result<()> {
    let mut buffer = Vec::with_capacity(2048);
    let mut chunk = [0_u8; 1024];
    loop {
        let n = stream.read(&mut chunk).await?;
        if n == 0 {
            return Ok(());
        }
        buffer.extend_from_slice(&chunk[..n]);
        if buffer.windows(4).any(|window| window == b"\r\n\r\n") {
            break;
        }
        if buffer.len() > 8192 {
            return Ok(());
        }
    }

    let head =
        std::str::from_utf8(&buffer).map_err(|err| std::io::Error::other(err.to_string()))?;
    let mut tokens = head
        .split("\r\n")
        .next()
        .unwrap_or_default()
        .split_whitespace();
    let method = tokens.next().unwrap_or_default().to_string();
    let path = tokens.next().unwrap_or_default().to_string();

    let (status, content_type, body): (&str, &str, &[u8]) = if path.contains("/manifests/") {
        (
            "200 OK",
            "application/vnd.oci.image.manifest.v1+json",
            &fixture.manifest_bytes,
        )
    } else if path.ends_with(&fixture.config_digest) {
        (
            "200 OK",
            "application/vnd.oci.image.config.v1+json",
            &fixture.config_bytes,
        )
    } else if path.ends_with(&fixture.layer_digest) {
        if method != "HEAD"
            && let Some(layer_gets) = &layer_gets
        {
            layer_gets.fetch_add(1, Ordering::SeqCst);
        }
        ("200 OK", "application/octet-stream", &fixture.layer_bytes)
    } else {
        ("404 Not Found", "text/plain", &[][..])
    };

    let response_head = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {len}\r\nConnection: close\r\n\r\n",
        len = body.len(),
    );
    stream.write_all(response_head.as_bytes()).await?;
    if method != "HEAD" {
        stream.write_all(body).await?;
    }
    stream.shutdown().await.ok();
    Ok(())
}
