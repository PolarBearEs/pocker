use std::io::Read;
use std::process::Output;
use std::time::Duration;

use assert_cmd::Command;
use pocker_test_registry::{FakeDaemonImage, FakeDockerDaemon, TestImage, TestRegistry};
use tar::{Archive, Builder, Header};

const DONOR_ID: &str = "sha256:1111111111111111111111111111111111111111111111111111111111111111";
const SECOND_DONOR_ID: &str =
    "sha256:2222222222222222222222222222222222222222222222222222222222222222";

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pull_reuses_layer_exported_from_docker_daemon() {
    let fixture = TestImage::single_layer();
    let registry = TestRegistry::start(fixture.clone()).await;
    let daemon = FakeDockerDaemon::start(vec![donor_image(
        DONOR_ID,
        &fixture,
        Some(save_archive_with_layer(&fixture)),
    )])
    .await;

    let output = run_pull(&registry, &daemon).await;

    assert_pull_succeeded(&output, &daemon);
    assert_eq!(
        registry.layer_get_count(),
        0,
        "layer exported from Docker must not be downloaded: {}",
        describe(&output, &daemon)
    );
    assert_single_load_contains_layer(&daemon, &fixture);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pull_downloads_layer_when_docker_save_omits_its_blob() {
    // Docker 29 on the containerd image store can report a layer in RootFS
    // and exit successfully from `docker save` while leaving the layer blob
    // out of the archive. pocker must fetch it from the registry instead.
    let fixture = TestImage::single_layer();
    let registry = TestRegistry::start(fixture.clone()).await;
    let daemon = FakeDockerDaemon::start(vec![donor_image(
        DONOR_ID,
        &fixture,
        Some(save_archive_without_layer(&fixture)),
    )])
    .await;

    let output = run_pull(&registry, &daemon).await;

    assert_pull_succeeded(&output, &daemon);
    assert_eq!(registry.layer_get_count(), 1);
    assert_eq!(daemon.saves(), vec![DONOR_ID.to_string()]);
    assert_single_load_contains_layer(&daemon, &fixture);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pull_downloads_layer_when_docker_save_fails() {
    let fixture = TestImage::single_layer();
    let registry = TestRegistry::start(fixture.clone()).await;
    let daemon = FakeDockerDaemon::start(vec![donor_image(DONOR_ID, &fixture, None)]).await;

    let output = run_pull(&registry, &daemon).await;

    assert_pull_succeeded(&output, &daemon);
    assert_eq!(registry.layer_get_count(), 1);
    assert_single_load_contains_layer(&daemon, &fixture);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pull_tries_next_daemon_image_when_first_export_lacks_layer() {
    let fixture = TestImage::single_layer();
    let registry = TestRegistry::start(fixture.clone()).await;
    let daemon = FakeDockerDaemon::start(vec![
        donor_image(
            DONOR_ID,
            &fixture,
            Some(save_archive_without_layer(&fixture)),
        ),
        donor_image(
            SECOND_DONOR_ID,
            &fixture,
            Some(save_archive_with_layer(&fixture)),
        ),
    ])
    .await;

    let output = run_pull(&registry, &daemon).await;

    assert_pull_succeeded(&output, &daemon);
    assert_eq!(
        registry.layer_get_count(),
        0,
        "{}",
        describe(&output, &daemon)
    );
    assert_eq!(
        daemon.saves(),
        vec![DONOR_ID.to_string(), SECOND_DONOR_ID.to_string()]
    );
    assert_single_load_contains_layer(&daemon, &fixture);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pull_skips_load_when_containerd_index_has_unpacked_platform_manifest() {
    // Images pulled by Docker itself on the containerd store report the
    // index digest as their ID and list platform manifests separately.
    let fixture = TestImage::single_layer();
    let registry = TestRegistry::start(fixture.clone()).await;
    let reference = registry.reference("library/test", "latest");
    let daemon = FakeDockerDaemon::start(vec![containerd_index_image(
        &reference,
        &format!("sha256:{}", fixture.manifest_digest()),
    )])
    .await;

    let output = run_pull(&registry, &daemon).await;

    assert_pull_succeeded(&output, &daemon);
    assert!(
        daemon.loads().is_empty(),
        "image already present must not reload"
    );
    assert_eq!(registry.layer_get_count(), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pull_loads_when_containerd_index_has_other_platform_manifest() {
    let fixture = TestImage::single_layer();
    let registry = TestRegistry::start(fixture.clone()).await;
    let reference = registry.reference("library/test", "latest");
    let daemon = FakeDockerDaemon::start(vec![containerd_index_image(
        &reference,
        "sha256:3333333333333333333333333333333333333333333333333333333333333333",
    )])
    .await;

    let output = run_pull(&registry, &daemon).await;

    assert_pull_succeeded(&output, &daemon);
    assert_single_load_contains_layer(&daemon, &fixture);
}

async fn run_pull(registry: &TestRegistry, daemon: &FakeDockerDaemon) -> Output {
    let cache_dir = tempfile::tempdir().expect("cache tempdir should create");
    let reference = registry.reference("library/test", "latest");
    let docker_host = daemon.docker_host();
    tokio::task::spawn_blocking(move || {
        let output = Command::cargo_bin("pocker")
            .expect("pocker binary should be built")
            .env("DOCKER_HOST", docker_host)
            .arg("--cache-dir")
            .arg(cache_dir.path())
            .args([
                "pull",
                "--plain-http",
                "--quiet",
                "--request-retries",
                "0",
                "--blob-retries",
                "0",
            ])
            .arg(&reference)
            .timeout(Duration::from_secs(30))
            .output()
            .expect("pocker should run");
        drop(cache_dir);
        output
    })
    .await
    .expect("pocker subprocess should run")
}

fn donor_image(id: &str, fixture: &TestImage, save_archive: Option<Vec<u8>>) -> FakeDaemonImage {
    FakeDaemonImage {
        id: id.to_string(),
        names: Vec::new(),
        inspect_json: format!(
            r#"{{"Id":"{id}","RepoTags":["example.test/donor:1"],"RootFS":{{"Type":"layers","Layers":["{}"]}}}}"#,
            fixture.layer_diff_id()
        ),
        save_archive,
    }
}

fn containerd_index_image(reference: &str, platform_manifest_digest: &str) -> FakeDaemonImage {
    let index = "sha256:4444444444444444444444444444444444444444444444444444444444444444";
    FakeDaemonImage {
        id: index.to_string(),
        names: vec![reference.to_string()],
        inspect_json: format!(
            r#"{{"Id":"{index}","RepoTags":["{reference}"],"Descriptor":{{"digest":"{index}"}},"Manifests":[{{"Descriptor":{{"digest":"{platform_manifest_digest}"}},"Available":false,"ImageData":{{"Size":{{"Unpacked":1024}}}}}}]}}"#
        ),
        save_archive: None,
    }
}

fn layer_blob_path(fixture: &TestImage) -> String {
    format!("blobs/sha256/{}", fixture.layer_digest())
}

fn save_archive_with_layer(fixture: &TestImage) -> Vec<u8> {
    save_archive(fixture, true)
}

fn save_archive_without_layer(fixture: &TestImage) -> Vec<u8> {
    save_archive(fixture, false)
}

fn save_archive(fixture: &TestImage, include_layer: bool) -> Vec<u8> {
    let manifest = format!(
        r#"[{{"Config":"blobs/sha256/{}","RepoTags":null,"Layers":["{}"]}}]"#,
        fixture.config_digest(),
        layer_blob_path(fixture)
    );
    let mut builder = Builder::new(Vec::new());
    append(&mut builder, "manifest.json", manifest.as_bytes());
    if include_layer {
        append(
            &mut builder,
            &layer_blob_path(fixture),
            fixture.layer_bytes(),
        );
    }
    builder.into_inner().expect("save archive should finish")
}

fn append(builder: &mut Builder<Vec<u8>>, path: &str, bytes: &[u8]) {
    let mut header = Header::new_gnu();
    header.set_size(bytes.len() as u64);
    header.set_mode(0o644);
    header.set_cksum();
    builder
        .append_data(&mut header, path, bytes)
        .expect("archive entry should append");
}

fn assert_single_load_contains_layer(daemon: &FakeDockerDaemon, fixture: &TestImage) {
    let loads = daemon.loads();
    assert_eq!(loads.len(), 1, "expected exactly one docker load");
    let wanted = format!(
        "blobs/sha256/{}",
        fixture
            .layer_descriptor_digest()
            .strip_prefix("sha256:")
            .expect("fixture layer should use sha256")
    );
    let mut archive = Archive::new(loads[0].as_slice());
    for entry in archive.entries().expect("load archive should list") {
        let mut entry = entry.expect("load archive entry should read");
        if entry.path().expect("entry path").to_string_lossy() == wanted {
            let mut bytes = Vec::new();
            entry
                .read_to_end(&mut bytes)
                .expect("layer entry should read");
            assert_eq!(bytes, fixture.layer_bytes());
            return;
        }
    }
    panic!("docker load archive is missing layer {wanted}");
}

fn assert_pull_succeeded(output: &Output, daemon: &FakeDockerDaemon) {
    assert!(output.status.success(), "{}", describe(output, daemon));
    assert!(
        daemon.unexpected_requests().is_empty(),
        "{}",
        describe(output, daemon)
    );
}

fn describe(output: &Output, daemon: &FakeDockerDaemon) -> String {
    format!(
        "status: {}\nstdout:\n{}\nstderr:\n{}\nunexpected daemon requests: {:?}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
        daemon.unexpected_requests()
    )
}
