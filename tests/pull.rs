use std::sync::Arc;
use std::time::Duration;

use assert_cmd::Command;
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pulls_oci_image_from_fake_registry_into_cache() {
    let fixture = Arc::new(Fixture::build());
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("fake registry should bind");
    let address = listener
        .local_addr()
        .expect("listener should expose address");

    let server_fixture = Arc::clone(&fixture);
    let server = tokio::spawn(async move {
        loop {
            let (stream, _peer) = match listener.accept().await {
                Ok(pair) => pair,
                Err(_) => return,
            };
            let request_fixture = Arc::clone(&server_fixture);
            tokio::spawn(async move {
                let _ = handle_connection(stream, request_fixture).await;
            });
        }
    });

    let cache_dir = tempfile::tempdir().expect("cache tempdir should create");
    let reference = format!("{address}/library/test:latest");

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
    server.abort();

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
        let layer_bytes = b"fake layer payload".to_vec();
        let layer_digest = sha256_hex(&layer_bytes);

        let config_bytes = format!(
            r#"{{"architecture":"amd64","os":"linux","rootfs":{{"type":"layers","diff_ids":["sha256:{layer_digest}"]}}}}"#
        )
        .into_bytes();
        let config_digest = sha256_hex(&config_bytes);

        let manifest_bytes = format!(
            r#"{{"schemaVersion":2,"mediaType":"application/vnd.oci.image.manifest.v1+json","config":{{"mediaType":"application/vnd.oci.image.config.v1+json","digest":"sha256:{config_digest}","size":{config_size}}},"layers":[{{"mediaType":"application/vnd.oci.image.layer.v1.tar","digest":"sha256:{layer_digest}","size":{layer_size}}}]}}"#,
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
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hex::encode(hasher.finalize())
}

async fn handle_connection(mut stream: TcpStream, fixture: Arc<Fixture>) -> std::io::Result<()> {
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
