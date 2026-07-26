use std::net::SocketAddr;
use std::process::{Child, Command as StdCommand, Stdio};
use std::time::Duration;

use assert_cmd::Command;
use pocker_test_registry::{TestImage, TestRegistry};
use tokio::net::TcpStream;

const COMMAND_TIMEOUT: Duration = Duration::from_secs(30);
const SERVER_START_TIMEOUT: Duration = Duration::from_secs(10);

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn serve_pull_missing_fetches_from_upstream_for_cache_client() {
    let upstream = TestRegistry::start(TestImage::single_layer()).await;
    let reference = upstream.reference("sample", "latest");
    let serve_cache = tempfile::tempdir().expect("serve cache tempdir should create");
    let client_cache = tempfile::tempdir().expect("client cache tempdir should create");
    let listen = reserve_loopback_address();
    let bin = assert_cmd::cargo::cargo_bin("pocker");

    let mut server = ChildGuard::spawn(
        StdCommand::new(&bin)
            .arg("--cache-dir")
            .arg(serve_cache.path())
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
            .stdout(Stdio::null())
            .stderr(Stdio::null()),
    );
    wait_for_listener(&mut server.child, listen).await;

    Command::new(bin)
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
        ])
        .arg(reference)
        .timeout(COMMAND_TIMEOUT)
        .assert()
        .success();

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
}

fn reserve_loopback_address() -> SocketAddr {
    std::net::TcpListener::bind("127.0.0.1:0")
        .expect("ephemeral port should be reserved")
        .local_addr()
        .expect("reserved listener should expose its address")
}

async fn wait_for_listener(child: &mut Child, address: SocketAddr) {
    let wait = async {
        loop {
            if let Some(status) = child
                .try_wait()
                .expect("pocker serve process should be inspectable")
            {
                panic!("pocker serve exited before listening with status {status}");
            }
            if TcpStream::connect(address).await.is_ok() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    };
    tokio::time::timeout(SERVER_START_TIMEOUT, wait)
        .await
        .expect("pocker serve should start listening");
}

struct ChildGuard {
    child: Child,
}

impl ChildGuard {
    fn spawn(command: &mut StdCommand) -> Self {
        Self {
            child: command.spawn().expect("pocker serve should start"),
        }
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
