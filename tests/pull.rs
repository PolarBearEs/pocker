use std::process::{Command as StdCommand, Output, Stdio};
use std::time::Duration;

use assert_cmd::Command;
use pocker_test_registry::{TestImage, TestRegistry};
use predicates::str::contains;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pulls_oci_image_from_test_registry_into_cache() {
    let fixture = TestImage::single_layer();
    let server = TestRegistry::start(fixture.clone()).await;

    let cache_dir = tempfile::tempdir().expect("cache tempdir should create");
    let reference = server.reference("library/test", "latest");

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
        .join(fixture.manifest_digest());
    let layer_path = cache_dir
        .path()
        .join("blobs")
        .join("sha256")
        .join(fixture.layer_digest());
    let config_path = cache_dir
        .path()
        .join("blobs")
        .join("sha256")
        .join(fixture.config_digest());

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
async fn pulls_multiple_oci_images_from_test_registry_into_cache() {
    let fixture = TestImage::single_layer();
    let server = TestRegistry::start(fixture.clone()).await;

    let cache_dir = tempfile::tempdir().expect("cache tempdir should create");
    let first_reference = server.reference("library/test", "first");
    let second_reference = server.reference("library/test", "second");

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
        .join(fixture.manifest_digest());
    let layer_path = cache_dir
        .path()
        .join("blobs")
        .join("sha256")
        .join(fixture.layer_digest());
    let config_path = cache_dir
        .path()
        .join("blobs")
        .join("sha256")
        .join(fixture.config_digest());

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
        server.layer_get_count(),
        1,
        "shared layer blob should be downloaded once across concurrent image pulls"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_pocker_processes_share_layer_download() {
    let fixture = TestImage::single_layer();
    let server = TestRegistry::start_with_blocked_layer(fixture.clone()).await;

    let cache_dir = tempfile::tempdir().expect("cache tempdir should create");
    let reference = server.reference("library/test", "latest");
    let bin = assert_cmd::cargo::cargo_bin("pocker");

    let first = pocker_pull_command(&bin, cache_dir.path(), &reference)
        .spawn()
        .expect("first pocker process should start");
    let second = pocker_pull_command(&bin, cache_dir.path(), &reference)
        .spawn()
        .expect("second pocker process should start");

    server.wait_for_layer_gets(1).await;
    server.release_layer();

    let (first_output, second_output) = tokio::task::spawn_blocking(move || {
        let first_output = first
            .wait_with_output()
            .expect("first pocker process should exit");
        let second_output = second
            .wait_with_output()
            .expect("second pocker process should exit");
        (first_output, second_output)
    })
    .await
    .expect("pocker wait task should run");

    assert!(
        first_output.status.success(),
        "first pocker process should succeed\n{}",
        process_output("first", &first_output)
    );
    assert!(
        second_output.status.success(),
        "second pocker process should succeed\n{}",
        process_output("second", &second_output)
    );

    let layer_path = cache_dir
        .path()
        .join("blobs")
        .join("sha256")
        .join(fixture.layer_digest());
    assert!(layer_path.exists(), "expected layer blob at {layer_path:?}");
    assert_eq!(
        server.layer_get_count(),
        1,
        "shared layer blob should be downloaded once across pocker processes"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn interrupted_pocker_process_releases_cache_and_blob_locks() {
    let fixture = TestImage::single_layer();
    let server = TestRegistry::start_with_blocked_layer(fixture.clone()).await;

    let cache_dir = tempfile::tempdir().expect("cache tempdir should create");
    let reference = server.reference("library/test", "latest");
    let bin = assert_cmd::cargo::cargo_bin("pocker");
    let mut interrupted = pocker_pull_command(&bin, cache_dir.path(), &reference)
        .spawn()
        .expect("interrupted pocker process should start");

    server.wait_for_layer_gets(1).await;
    interrupted
        .kill()
        .expect("interrupted pocker process should be killed");
    let interrupted_status = tokio::task::spawn_blocking(move || {
        interrupted
            .wait()
            .expect("interrupted pocker process should exit")
    })
    .await
    .expect("interrupted wait task should run");
    assert!(
        !interrupted_status.success(),
        "interrupted process should not report success"
    );
    server.release_layer();

    Command::cargo_bin("pocker")
        .expect("pocker binary should be built")
        .arg("--cache-dir")
        .arg(cache_dir.path())
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

    let layer_path = cache_dir
        .path()
        .join("blobs")
        .join("sha256")
        .join(fixture.layer_digest());
    assert!(layer_path.exists(), "expected layer blob at {layer_path:?}");
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
    let fixture = TestImage::with_layer_algorithm(algorithm);
    let server = TestRegistry::start(fixture.clone()).await;

    let cache_dir = tempfile::tempdir().expect("cache tempdir should create");
    let reference = server.reference("library/test", "latest");

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
        .join(fixture.layer_digest());

    assert!(layer_path.exists(), "expected layer blob at {layer_path:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pull_rejects_malformed_layer_digest_before_cache_path_use() {
    let fixture = TestImage::with_layer_descriptor_digest("sha256:../../outside".to_string());
    let server = TestRegistry::start(fixture).await;

    let cache_dir = tempfile::tempdir().expect("cache tempdir should create");
    let reference = server.reference("library/test", "latest");

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

fn pocker_pull_command(
    bin: &std::path::Path,
    cache_dir: &std::path::Path,
    reference: &str,
) -> StdCommand {
    let mut command = StdCommand::new(bin);
    command
        .arg("--cache-dir")
        .arg(cache_dir)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
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
        .arg(reference);
    command
}

fn process_output(name: &str, output: &Output) -> String {
    format!(
        "{name} status: {}\n{name} stdout:\n{}\n{name} stderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}
