use std::fs::File;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::{Child, Command as StdCommand, Output, Stdio};
use std::time::Duration;

use assert_cmd::Command;
use pocker_test_registry::{TestImage, TestRegistry};
use tokio::net::TcpStream;

const COMMAND_TIMEOUT: Duration = Duration::from_secs(30);
const SERVER_START_TIMEOUT: Duration = Duration::from_secs(10);
const SERVER_START_ATTEMPTS: usize = 3;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn serve_pull_missing_fetches_from_upstream_for_cache_client() {
    let fixture = TestImage::single_layer();
    let upstream = TestRegistry::start(fixture.clone()).await;
    let reference = upstream.reference("sample", "latest");
    let serve_cache = tempfile::tempdir().expect("serve cache tempdir should create");
    let client_cache = tempfile::tempdir().expect("client cache tempdir should create");
    let log_dir = tempfile::tempdir().expect("serve log tempdir should create");
    let bin = assert_cmd::cargo::cargo_bin("pocker");

    let (listen, server) = start_server(&bin, serve_cache.path(), log_dir.path()).await;

    let output = Command::new(bin)
        .arg("--cache-dir")
        .arg(client_cache.path())
        .args([
            "pull",
            "--plain-http",
            "--no-load",
            "--quiet",
            "--request-retries",
            "0",
            "--blob-retries",
            "0",
            "--cache-from",
            &format!("http://{listen}"),
            "--cache-only",
        ])
        .arg(reference)
        .timeout(COMMAND_TIMEOUT)
        .output()
        .expect("pocker pull should run");

    assert!(
        output.status.success(),
        "cache-only pull failed:\n{}\npocker serve log:\n{}",
        format_output(&output),
        server.log()
    );

    assert_eq!(
        upstream.layer_get_count(),
        1,
        "serve --pull-missing should fetch the upstream layer once"
    );
    assert!(
        upstream.unexpected_requests().is_empty(),
        "upstream fixture received unexpected requests: {:?}",
        upstream.unexpected_requests()
    );
    for (kind, digest) in [
        ("manifest", fixture.manifest_digest()),
        ("config", fixture.config_digest()),
        ("layer", fixture.layer_digest()),
    ] {
        let blob = serve_cache.path().join("blobs").join("sha256").join(digest);
        assert!(
            blob.is_file(),
            "serve --pull-missing did not cache the {kind} blob at {blob:?}\n\
             pocker serve log:\n{}",
            server.log()
        );
    }
}

async fn start_server(bin: &Path, cache: &Path, log_dir: &Path) -> (SocketAddr, ChildGuard) {
    let mut failures = Vec::new();

    for attempt in 1..=SERVER_START_ATTEMPTS {
        let listen = reserve_loopback_address();
        let log_path = log_dir.join(format!("pocker-serve-{attempt}.log"));
        let mut server = ChildGuard::spawn(bin, cache, listen, log_path);

        match wait_for_listener(&mut server.child, listen).await {
            Ok(()) => return (listen, server),
            Err(error) => {
                failures.push(format!(
                    "attempt {attempt} on {listen}: {error}\n{}",
                    server.log()
                ));
                drop(server);
            }
        }
    }

    panic!(
        "pocker serve did not start after {SERVER_START_ATTEMPTS} attempts:\n{}",
        failures.join("\n")
    );
}

fn reserve_loopback_address() -> SocketAddr {
    std::net::TcpListener::bind("127.0.0.1:0")
        .expect("ephemeral port should be reserved")
        .local_addr()
        .expect("reserved listener should expose its address")
}

async fn wait_for_listener(child: &mut Child, address: SocketAddr) -> Result<(), String> {
    let wait = async {
        loop {
            if let Some(status) = child
                .try_wait()
                .map_err(|error| format!("could not inspect pocker serve: {error}"))?
            {
                return Err(format!(
                    "pocker serve exited before listening with status {status}"
                ));
            }
            if TcpStream::connect(address).await.is_ok() {
                // A bind loser exits promptly. Recheck after connecting so an unrelated
                // process that won the released ephemeral port cannot satisfy startup.
                tokio::time::sleep(Duration::from_millis(25)).await;
                if let Some(status) = child
                    .try_wait()
                    .map_err(|error| format!("could not inspect pocker serve: {error}"))?
                {
                    return Err(format!(
                        "pocker serve exited after another listener took the port with status \
                         {status}"
                    ));
                }
                return Ok(());
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    };

    tokio::time::timeout(SERVER_START_TIMEOUT, wait)
        .await
        .map_err(|_| format!("timed out waiting for pocker serve on {address}"))?
}

struct ChildGuard {
    child: Child,
    log_path: PathBuf,
}

impl ChildGuard {
    fn spawn(bin: &Path, cache: &Path, listen: SocketAddr, log_path: PathBuf) -> Self {
        let stdout = File::create(&log_path).expect("pocker serve log should create");
        let stderr = stdout
            .try_clone()
            .expect("pocker serve log should be cloneable");
        let child = StdCommand::new(bin)
            .arg("--cache-dir")
            .arg(cache)
            .args([
                "serve",
                "--listen",
                &listen.to_string(),
                "--pull-missing",
                "--plain-http",
                "--quiet",
                "--request-retries",
                "0",
                "--blob-retries",
                "0",
            ])
            .stdout(Stdio::from(stdout))
            .stderr(Stdio::from(stderr))
            .spawn()
            .expect("pocker serve should start");

        Self { child, log_path }
    }

    fn log(&self) -> String {
        std::fs::read_to_string(&self.log_path)
            .unwrap_or_else(|error| format!("<could not read serve log: {error}>"))
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn format_output(output: &Output) -> String {
    format!(
        "status: {}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}
