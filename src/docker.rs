use std::collections::{HashMap, HashSet};
use std::env;
use std::fs::File;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use futures_util::StreamExt;
use percent_encoding::{AsciiSet, CONTROLS, utf8_percent_encode};
use reqwest::{Client, Response, StatusCode};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::Value;
use sha2::{Digest as _, Sha256};
use tar::Archive;
use tempfile::{NamedTempFile, TempDir};
use tokio::io::AsyncWriteExt;
use tokio_util::io::ReaderStream;

use crate::error::{DockerPullError, Result};
use crate::export::oci_archive::write_oci_archive;
use crate::image::LayerSpec;
use crate::reference::{ImageReference, ReferenceTarget};
use crate::store::{Store, StoredReference};

const DEFAULT_DOCKER_HOST: &str = "unix:///var/run/docker.sock";
const DEFAULT_DOCKER_BASE_URL: &str = "http://docker";
const PATH_SEGMENT_ENCODE_SET: &AsciiSet = &CONTROLS
    .add(b' ')
    .add(b'"')
    .add(b'#')
    .add(b'%')
    .add(b'/')
    .add(b':')
    .add(b'<')
    .add(b'>')
    .add(b'?')
    .add(b'@')
    .add(b'[')
    .add(b'\\')
    .add(b']')
    .add(b'^')
    .add(b'`')
    .add(b'{')
    .add(b'|')
    .add(b'}');

#[derive(Debug, Clone)]
enum DockerEndpoint {
    #[cfg(unix)]
    Unix(PathBuf),
    Http(String),
}

#[derive(Debug, Clone)]
struct DockerDaemon {
    client: Client,
    base_url: String,
}

impl DockerDaemon {
    fn connect() -> Result<Self> {
        let endpoint = docker_endpoint()?;
        let builder = Client::builder()
            .connect_timeout(Duration::from_secs(20))
            .user_agent(format!("pocker/{}", env!("CARGO_PKG_VERSION")))
            .http1_only();

        #[cfg(unix)]
        let (client, base_url) = match endpoint {
            DockerEndpoint::Unix(path) => (
                builder.unix_socket(path).build()?,
                DEFAULT_DOCKER_BASE_URL.to_string(),
            ),
            DockerEndpoint::Http(base_url) => (builder.build()?, base_url),
        };

        #[cfg(not(unix))]
        let (client, base_url) = match endpoint {
            DockerEndpoint::Http(base_url) => (builder.build()?, base_url),
        };

        Ok(Self { client, base_url })
    }

    async fn load_archive(&self, path: &Path) -> Result<()> {
        let file = tokio::fs::File::open(path).await?;
        let body = reqwest::Body::wrap_stream(ReaderStream::new(file));
        let response = self
            .client
            .post(self.url("/images/load?quiet=1"))
            .header(reqwest::header::CONTENT_TYPE, "application/x-tar")
            .body(body)
            .send()
            .await?;
        self.ensure_success(response, "docker image load").await?;
        Ok(())
    }

    async fn inspect_image(&self, image: &str) -> Result<Option<DaemonImage>> {
        let response = self
            .client
            .get(self.url(&format!("/images/{}/json", encode_path_segment(image))))
            .send()
            .await?;
        if response.status() == StatusCode::NOT_FOUND {
            return Ok(None);
        }
        let response = self
            .ensure_success(response, "docker image inspect")
            .await?;
        Ok(Some(response.json().await?))
    }

    async fn list_image_summaries(&self) -> Result<Vec<DaemonImageSummary>> {
        self.get_json("/images/json?all=1", "docker image ls").await
    }

    async fn save_image(&self, image: &str, path: &Path) -> Result<()> {
        let response = self
            .client
            .get(self.url(&format!("/images/{}/get", encode_path_segment(image))))
            .send()
            .await?;
        let response = self.ensure_success(response, "docker image save").await?;
        let mut file = tokio::fs::File::create(path).await?;
        let mut stream = response.bytes_stream();
        while let Some(chunk) = stream.next().await {
            file.write_all(&chunk?).await?;
        }
        file.flush().await?;
        Ok(())
    }

    async fn get_json<T>(&self, path: &str, action: &str) -> Result<T>
    where
        T: DeserializeOwned,
    {
        let response = self.client.get(self.url(path)).send().await?;
        let response = self.ensure_success(response, action).await?;
        response.json().await.map_err(Into::into)
    }

    async fn ensure_success(&self, response: Response, action: &str) -> Result<Response> {
        if response.status().is_success() {
            return Ok(response);
        }

        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        let body = body.trim();
        let detail = if body.is_empty() {
            format!("status {status}")
        } else {
            format!("status {status}: {body}")
        };
        Err(DockerPullError::CommandFailed(format!(
            "{action} failed: {detail}"
        )))
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.base_url, path)
    }
}

pub async fn write_reference_archive(
    store: &Store,
    reference: &StoredReference,
) -> Result<NamedTempFile> {
    let temp = NamedTempFile::new_in(store.root())?;
    write_oci_archive(temp.path(), store, reference).await?;
    Ok(temp)
}

pub async fn daemon_has_reference(reference: &ImageReference, config_digest: &str) -> Result<bool> {
    let inspect_target = daemon_inspect_target(reference, config_digest);
    let daemon = DockerDaemon::connect()?;
    let Some(image) = daemon.inspect_image(&inspect_target).await? else {
        return Ok(false);
    };

    Ok(normalize_image_id(&image.id) == normalize_image_id(config_digest))
}

pub async fn daemon_layer_coverage(layers: &[LayerSpec]) -> Result<HashMap<String, String>> {
    let mut wanted = HashSet::new();
    for layer in layers {
        if !layer.diff_id.is_empty() {
            wanted.insert(layer.diff_id.clone());
        }
    }
    if wanted.is_empty() {
        return Ok(HashMap::new());
    }

    let daemon = DockerDaemon::connect()?;
    let chosen = choose_daemon_images(&daemon, &wanted).await?;
    let mut coverage = HashMap::new();
    for chosen in chosen {
        let label = chosen.image.label();
        for diff_id in chosen.diff_ids {
            coverage.insert(diff_id, label.clone());
        }
    }
    Ok(coverage)
}

#[derive(Debug, Clone, Deserialize)]
struct DaemonImageSummary {
    #[serde(rename = "Id")]
    id: String,
    #[serde(rename = "Created")]
    created: Option<i64>,
    #[serde(rename = "RepoTags")]
    repo_tags: Option<Vec<String>>,
    #[serde(rename = "Size")]
    size: Option<u64>,
}

#[derive(Debug, Clone, Deserialize)]
struct DaemonImage {
    #[serde(rename = "Id")]
    id: String,
    #[serde(rename = "RepoTags")]
    repo_tags: Option<Vec<String>>,
    #[serde(default, rename = "RootFS")]
    rootfs: Option<RootFs>,
}

#[derive(Debug, Clone, Deserialize)]
struct RootFs {
    #[serde(default, rename = "Layers")]
    layers: Option<Vec<String>>,
}

impl DaemonImage {
    fn rootfs_layers(&self) -> &[String] {
        self.rootfs
            .as_ref()
            .and_then(|rootfs| rootfs.layers.as_deref())
            .unwrap_or(&[])
    }

    fn label(&self) -> String {
        self.repo_tags
            .as_ref()
            .and_then(|tags| tags.first())
            .cloned()
            .unwrap_or_else(|| self.id.clone())
    }
}

fn daemon_inspect_target(reference: &ImageReference, config_digest: &str) -> String {
    match reference.target {
        ReferenceTarget::Tag(_) => reference.display_name(),
        ReferenceTarget::Digest(_) => config_digest.to_string(),
    }
}

fn normalize_image_id(image_id: &str) -> &str {
    image_id.strip_prefix("sha256:").unwrap_or(image_id).trim()
}

#[derive(Debug, Deserialize)]
struct SaveManifestEntry {
    #[serde(rename = "Layers")]
    layers: Vec<String>,
}

#[derive(Debug, Clone)]
struct ChosenImageLayers {
    image: DaemonImage,
    diff_ids: Vec<String>,
}

pub struct MaterializedDaemonLayers {
    _tempdir: TempDir,
    paths: HashMap<String, PathBuf>,
}

impl MaterializedDaemonLayers {
    pub fn path_for(&self, diff_id: &str) -> Option<&PathBuf> {
        self.paths.get(diff_id)
    }
}

#[derive(Debug, Clone)]
pub struct ImageSummary {
    pub created: Option<i64>,
    pub id: String,
    pub repo_tags: Vec<String>,
    pub size: Option<u64>,
}

pub async fn load_archive(path: &Path) -> Result<()> {
    DockerDaemon::connect()?.load_archive(path).await
}

pub async fn inspect_image(reference: &str) -> Result<Option<Value>> {
    let daemon = DockerDaemon::connect()?;
    let response = daemon
        .client
        .get(daemon.url(&format!("/images/{}/json", encode_path_segment(reference))))
        .send()
        .await?;
    if response.status() == StatusCode::NOT_FOUND {
        return Ok(None);
    }
    let response = daemon
        .ensure_success(response, "docker image inspect")
        .await?;
    Ok(Some(response.json().await?))
}

pub async fn list_images() -> Result<Vec<ImageSummary>> {
    let daemon = DockerDaemon::connect()?;
    let images = daemon.list_image_summaries().await?;
    Ok(images
        .into_iter()
        .map(|image| ImageSummary {
            created: image.created,
            id: image.id,
            repo_tags: image.repo_tags.unwrap_or_default(),
            size: image.size,
        })
        .collect())
}

pub async fn save_image(reference: &str, path: &Path) -> Result<()> {
    DockerDaemon::connect()?.save_image(reference, path).await
}

async fn list_daemon_images(daemon: &DockerDaemon) -> Result<Vec<DaemonImage>> {
    let ids = daemon
        .list_image_summaries()
        .await?
        .into_iter()
        .map(|image| image.id)
        .collect::<HashSet<_>>();
    if ids.is_empty() {
        return Ok(Vec::new());
    }

    let mut images = Vec::with_capacity(ids.len());
    for id in ids {
        if let Some(image) = daemon.inspect_image(&id).await? {
            images.push(image);
        }
    }
    Ok(images)
}

pub async fn materialize_daemon_layers(
    store: &Store,
    diff_ids: &[String],
) -> Result<MaterializedDaemonLayers> {
    let wanted = diff_ids.iter().cloned().collect::<HashSet<_>>();
    if wanted.is_empty() {
        return Ok(MaterializedDaemonLayers {
            _tempdir: tempfile::tempdir_in(store.root())?,
            paths: HashMap::new(),
        });
    }

    let daemon = DockerDaemon::connect()?;
    let chosen = choose_daemon_images(&daemon, &wanted).await?;
    let tempdir = tempfile::tempdir_in(store.root())?;
    let mut paths = HashMap::new();
    for chosen in &chosen {
        materialize_layers_from_saved_image(store, &daemon, chosen, tempdir.path(), &mut paths)
            .await?;
    }

    let unresolved = diff_ids
        .iter()
        .filter(|diff_id| !paths.contains_key(diff_id.as_str()))
        .cloned()
        .collect::<Vec<_>>();
    if !unresolved.is_empty() {
        return Err(DockerPullError::BadResponse(format!(
            "docker daemon is missing required layer(s): {}",
            unresolved.join(", ")
        )));
    }

    Ok(MaterializedDaemonLayers {
        _tempdir: tempdir,
        paths,
    })
}

async fn choose_daemon_images(
    daemon: &DockerDaemon,
    wanted: &HashSet<String>,
) -> Result<Vec<ChosenImageLayers>> {
    let images = list_daemon_images(daemon).await?;
    let mut chosen = Vec::new();
    let mut unresolved = wanted.clone();

    for image in images {
        let provided = image
            .rootfs_layers()
            .iter()
            .filter(|diff_id| unresolved.contains(*diff_id))
            .cloned()
            .collect::<Vec<_>>();
        if provided.is_empty() {
            continue;
        }
        for diff_id in &provided {
            unresolved.remove(diff_id);
        }
        chosen.push(ChosenImageLayers {
            image,
            diff_ids: provided,
        });
        if unresolved.is_empty() {
            break;
        }
    }

    Ok(chosen)
}

async fn materialize_layers_from_saved_image(
    store: &Store,
    daemon: &DockerDaemon,
    chosen: &ChosenImageLayers,
    output_root: &Path,
    paths: &mut HashMap<String, PathBuf>,
) -> Result<()> {
    let temp = NamedTempFile::new_in(store.root())?;
    if daemon
        .save_image(&chosen.image.id, temp.path())
        .await
        .is_err()
    {
        return Ok(());
    }

    let entries = save_manifest_entries(temp.path())?;
    let Some(entry) = entries.into_iter().next() else {
        return Ok(());
    };
    if entry.layers.len() != chosen.image.rootfs_layers().len() {
        return Ok(());
    }

    let targets = chosen
        .image
        .rootfs_layers()
        .iter()
        .cloned()
        .zip(entry.layers)
        .filter(|(diff_id, _)| chosen.diff_ids.contains(diff_id))
        .map(|(diff_id, path)| (path, diff_id))
        .collect::<HashMap<_, _>>();
    if targets.is_empty() {
        return Ok(());
    }

    let file = File::open(temp.path())?;
    let mut archive = Archive::new(file);

    for entry in archive.entries()? {
        let mut entry = entry?;
        let path = entry.path()?.to_string_lossy().into_owned();
        let Some(diff_id) = targets.get(&path) else {
            continue;
        };
        if paths.contains_key(diff_id.as_str()) {
            continue;
        }
        let destination = extracted_layer_path(output_root, diff_id)?;
        copy_archive_entry_with_digest(&mut entry, &destination, diff_id)?;
        paths.insert(diff_id.clone(), destination);
    }

    Ok(())
}

fn save_manifest_entries(path: &Path) -> Result<Vec<SaveManifestEntry>> {
    let file = File::open(path)?;
    let mut archive = Archive::new(file);
    for entry in archive.entries()? {
        let mut entry = entry?;
        let path = entry.path()?.to_string_lossy().into_owned();
        if path == "manifest.json" {
            let mut bytes = Vec::new();
            std::io::Read::read_to_end(&mut entry, &mut bytes)?;
            return serde_json::from_slice(&bytes).map_err(Into::into);
        }
    }
    Err(DockerPullError::BadResponse(
        "docker save archive is missing manifest.json".into(),
    ))
}

fn extracted_layer_path(root: &Path, diff_id: &str) -> Result<PathBuf> {
    let (algorithm, value) = diff_id
        .split_once(':')
        .ok_or_else(|| DockerPullError::InvalidInput(format!("invalid digest `{diff_id}`")))?;
    Ok(root.join(format!("{algorithm}-{value}.tar")))
}

fn copy_archive_entry_with_digest<R: Read>(
    reader: &mut R,
    path: &Path,
    expected_digest: &str,
) -> Result<()> {
    let mut file = File::create(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        file.write_all(&buffer[..read])?;
        hasher.update(&buffer[..read]);
    }
    file.flush()?;
    file.sync_data()?;
    let actual_digest = format!("sha256:{}", hex::encode(hasher.finalize()));
    if actual_digest != expected_digest {
        return Err(DockerPullError::DigestMismatch {
            digest: expected_digest.to_string(),
            expected: expected_digest.to_string(),
            actual: actual_digest,
        });
    }
    Ok(())
}

fn docker_endpoint() -> Result<DockerEndpoint> {
    match env::var("DOCKER_HOST") {
        Ok(host) if !host.trim().is_empty() => docker_endpoint_from_host(&host),
        _ => docker_endpoint_from_host(DEFAULT_DOCKER_HOST),
    }
}

fn docker_endpoint_from_host(host: &str) -> Result<DockerEndpoint> {
    #[cfg(unix)]
    if let Some(path) = host.strip_prefix("unix://") {
        if path.is_empty() {
            return Err(DockerPullError::InvalidInput(
                "docker host unix socket path is empty".into(),
            ));
        }
        return Ok(DockerEndpoint::Unix(PathBuf::from(path)));
    }

    if let Some(address) = host.strip_prefix("tcp://") {
        return Ok(DockerEndpoint::Http(format!(
            "http://{}",
            address.trim_end_matches('/')
        )));
    }

    if host.starts_with("http://") || host.starts_with("https://") {
        return Ok(DockerEndpoint::Http(host.trim_end_matches('/').to_string()));
    }

    Err(DockerPullError::InvalidInput(format!(
        "unsupported docker host `{host}`"
    )))
}

fn encode_path_segment(value: &str) -> String {
    utf8_percent_encode(value, PATH_SEGMENT_ENCODE_SET).to_string()
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::{
        DEFAULT_DOCKER_HOST, daemon_inspect_target, docker_endpoint_from_host, encode_path_segment,
    };
    use crate::reference::ImageReference;

    #[test]
    fn inspect_target_uses_display_name_for_tagged_references() {
        let reference = ImageReference::parse("alpine:latest").expect("reference should parse");
        assert_eq!(
            daemon_inspect_target(&reference, "sha256:deadbeef"),
            "alpine:latest"
        );
    }

    #[test]
    fn inspect_target_uses_config_digest_for_digest_references() {
        let reference =
            ImageReference::parse("ghcr.io/acme/app@sha256:beef").expect("reference should parse");
        assert_eq!(
            daemon_inspect_target(&reference, "sha256:deadbeef"),
            "sha256:deadbeef"
        );
    }

    #[cfg(unix)]
    #[test]
    fn docker_host_defaults_to_unix_socket() {
        let endpoint = docker_endpoint_from_host(DEFAULT_DOCKER_HOST)
            .expect("default docker host should parse");
        assert!(
            matches!(endpoint, super::DockerEndpoint::Unix(path) if path == Path::new("/var/run/docker.sock"))
        );
    }

    #[test]
    fn docker_host_supports_tcp() {
        let endpoint = docker_endpoint_from_host("tcp://127.0.0.1:2375")
            .expect("tcp docker host should parse");
        assert!(
            matches!(endpoint, super::DockerEndpoint::Http(base) if base == "http://127.0.0.1:2375")
        );
    }

    #[test]
    fn encodes_image_names_for_api_paths() {
        assert_eq!(
            encode_path_segment("docker.io/library/alpine:latest"),
            "docker.io%2Flibrary%2Falpine%3Alatest"
        );
    }
}
