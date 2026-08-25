use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use serde::Deserialize;
use tar::Archive;
use tempfile::{NamedTempFile, TempDir};
use tokio::sync::OnceCell;
use tokio::task::{self, JoinSet};
use tracing::warn;

use crate::digest::{copy_reader_with_digest, parse_digest};
use crate::error::{DockerPullError, Result};
use crate::image::LayerSpec;
use crate::store::Store;

use super::daemon::{DaemonImage, DaemonImageSummary, DockerDaemon};

const DAEMON_INSPECT_CONCURRENCY: usize = 8;

#[derive(Debug, Default)]
pub struct DaemonLayerCache {
    images: OnceCell<Vec<DaemonImage>>,
}

impl DaemonLayerCache {
    pub fn new() -> Self {
        Self::default()
    }

    pub async fn coverage(&self, layers: &[LayerSpec]) -> Result<HashMap<String, String>> {
        let wanted = wanted_diff_ids(layers);
        if wanted.is_empty() {
            return Ok(HashMap::new());
        }

        let images = self
            .images
            .get_or_try_init(|| async {
                let daemon = DockerDaemon::shared().await?;
                list_daemon_images(daemon).await
            })
            .await?;
        let chosen = choose_from_daemon_images(images.iter().cloned(), &wanted);
        Ok(coverage_from_chosen(chosen))
    }
}

pub async fn daemon_layer_coverage(layers: &[LayerSpec]) -> Result<HashMap<String, String>> {
    DaemonLayerCache::new().coverage(layers).await
}

fn wanted_diff_ids(layers: &[LayerSpec]) -> HashSet<String> {
    let mut wanted = HashSet::new();
    for layer in layers {
        if !layer.diff_id.is_empty() {
            wanted.insert(layer.diff_id.clone());
        }
    }
    wanted
}

fn coverage_from_chosen(chosen: Vec<ChosenImageLayers>) -> HashMap<String, String> {
    let mut coverage = HashMap::new();
    for chosen in chosen {
        let label = chosen.image.label();
        for diff_id in chosen.diff_ids {
            coverage.insert(diff_id, label.clone());
        }
    }
    coverage
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
    layers: HashMap<String, MaterializedDaemonLayer>,
}

impl MaterializedDaemonLayers {
    pub(crate) fn layers(&self) -> &HashMap<String, MaterializedDaemonLayer> {
        &self.layers
    }
}

#[derive(Debug)]
pub(crate) struct MaterializedDaemonLayer {
    pub(crate) path: PathBuf,
    pub(crate) digest: String,
    pub(crate) size: i64,
    pub(crate) gzip_compressed: bool,
}

async fn list_daemon_images(daemon: &DockerDaemon) -> Result<Vec<DaemonImage>> {
    let ids = ordered_unique_image_ids(daemon.list_image_summaries().await?);
    if ids.is_empty() {
        return Ok(Vec::new());
    }

    let mut queue = JoinSet::new();
    let mut pending = ids.into_iter().enumerate();
    let mut images = vec![None; pending.len()];

    while queue.len() < DAEMON_INSPECT_CONCURRENCY {
        let Some((index, id)) = pending.next() else {
            break;
        };
        spawn_inspect_task(&mut queue, daemon.clone(), index, id);
    }

    while let Some(result) = queue.join_next().await {
        let (index, image) = match result {
            Ok(Ok(result)) => result,
            Ok(Err(error)) => {
                abort_inspect_tasks(&mut queue).await;
                return Err(error);
            }
            Err(error) => {
                abort_inspect_tasks(&mut queue).await;
                return Err(DockerPullError::CommandFailed(format!(
                    "docker image inspect task failed: {error}"
                )));
            }
        };
        images[index] = image;

        if let Some((index, id)) = pending.next() {
            spawn_inspect_task(&mut queue, daemon.clone(), index, id);
        }
    }

    Ok(images.into_iter().flatten().collect())
}

fn spawn_inspect_task(
    queue: &mut JoinSet<Result<(usize, Option<DaemonImage>)>>,
    daemon: DockerDaemon,
    index: usize,
    id: String,
) {
    queue.spawn(async move { Ok((index, daemon.inspect_daemon_image(&id).await?)) });
}

async fn abort_inspect_tasks(queue: &mut JoinSet<Result<(usize, Option<DaemonImage>)>>) {
    queue.abort_all();
    while let Some(result) = queue.join_next().await {
        if let Err(error) = result
            && !error.is_cancelled()
        {
            warn!("docker image inspect task failed while aborting: {error}");
        }
    }
}

pub(super) fn ordered_unique_image_ids(summaries: Vec<DaemonImageSummary>) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut ids = Vec::with_capacity(summaries.len());
    for summary in summaries {
        if seen.insert(summary.id.clone()) {
            ids.push(summary.id);
        }
    }
    ids
}

pub async fn materialize_daemon_layers(
    store: &Store,
    diff_ids: &[String],
) -> Result<MaterializedDaemonLayers> {
    let wanted = diff_ids.iter().cloned().collect::<HashSet<_>>();
    if wanted.is_empty() {
        return Ok(MaterializedDaemonLayers {
            _tempdir: tempfile::tempdir_in(store.root())?,
            layers: HashMap::new(),
        });
    }

    let daemon = DockerDaemon::shared().await?;
    let chosen = choose_daemon_images(daemon, &wanted).await?;
    let tempdir = tempfile::tempdir_in(store.root())?;
    let mut paths = HashMap::new();
    for chosen in &chosen {
        materialize_layers_from_saved_image(store, daemon, chosen, tempdir.path(), &mut paths)
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
        layers: paths,
    })
}

async fn choose_daemon_images(
    daemon: &DockerDaemon,
    wanted: &HashSet<String>,
) -> Result<Vec<ChosenImageLayers>> {
    let images = list_daemon_images(daemon).await?;
    Ok(choose_from_daemon_images(images, wanted))
}

fn choose_from_daemon_images(
    images: impl IntoIterator<Item = DaemonImage>,
    wanted: &HashSet<String>,
) -> Vec<ChosenImageLayers> {
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

    chosen
}

async fn materialize_layers_from_saved_image(
    store: &Store,
    daemon: &DockerDaemon,
    chosen: &ChosenImageLayers,
    output_root: &Path,
    paths: &mut HashMap<String, MaterializedDaemonLayer>,
) -> Result<()> {
    let temp = NamedTempFile::new_in(store.root())?;
    if daemon
        .save_image(&chosen.image.id, temp.path())
        .await
        .is_err()
    {
        return Ok(());
    }

    let archive_path = temp.path().to_path_buf();
    let output_root = output_root.to_path_buf();
    let chosen = chosen.clone();
    let extracted = task::spawn_blocking(move || {
        materialize_layers_from_saved_archive(&archive_path, &chosen, &output_root)
    })
    .await
    .map_err(|error| {
        DockerPullError::CommandFailed(format!("docker layer materialization task failed: {error}"))
    })??;

    paths.extend(extracted);
    Ok(())
}

fn materialize_layers_from_saved_archive(
    archive_path: &Path,
    chosen: &ChosenImageLayers,
    output_root: &Path,
) -> Result<HashMap<String, MaterializedDaemonLayer>> {
    let entries = save_manifest_entries(archive_path)?;
    let Some(entry) = entries.into_iter().next() else {
        return Ok(HashMap::new());
    };
    if entry.layers.len() != chosen.image.rootfs_layers().len() {
        return Ok(HashMap::new());
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
        return Ok(HashMap::new());
    }

    let mut paths = HashMap::new();
    let file = File::open(archive_path)?;
    let mut archive = Archive::new(file);

    for entry in archive.entries()? {
        let mut entry = entry?;
        let path = entry.path()?.to_string_lossy().into_owned();
        let Some(diff_id) = targets.get(&path) else {
            continue;
        };
        if paths.contains_key(diff_id) {
            continue;
        }
        let entry_offset = entry.raw_file_position();
        let entry_size = entry.size();
        let destination = extracted_layer_path(output_root, diff_id)?;
        let actual_digest = copy_archive_entry_with_digest(&mut entry, &destination, diff_id)?;
        let gzip_compressed = if actual_digest == *diff_id {
            false
        } else if archive_entry_has_gzip_header(archive_path, entry_offset, entry_size)? {
            // Containerd-backed Docker saves preserve compressed OCI blobs.
            // Keep that representation and its actual content digest; the OCI
            // archive writer will describe it as gzip instead of expanding it
            // into another layer-sized temporary file.
            true
        } else {
            return Err(DockerPullError::DigestMismatch {
                expected: diff_id.clone(),
                actual: actual_digest,
            });
        };
        let size = i64::try_from(entry_size).map_err(|_| {
            DockerPullError::InvalidInput(format!(
                "saved Docker layer `{path}` is too large for an OCI descriptor"
            ))
        })?;
        paths.insert(
            diff_id.clone(),
            MaterializedDaemonLayer {
                path: destination,
                digest: actual_digest,
                size,
                gzip_compressed,
            },
        );
    }

    Ok(paths)
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
    let parsed = parse_digest(diff_id)?;
    Ok(root.join(format!("{}-{}.tar", parsed.algorithm, parsed.value)))
}

fn copy_archive_entry_with_digest<R: Read>(
    reader: &mut R,
    path: &Path,
    digest_template: &str,
) -> Result<String> {
    let mut file = File::create(path)?;
    let actual_digest = copy_reader_with_digest(digest_template, reader, &mut file)?;
    file.flush()?;
    file.sync_data()?;
    Ok(actual_digest)
}

fn archive_entry_has_gzip_header(
    archive_path: &Path,
    entry_offset: u64,
    entry_size: u64,
) -> Result<bool> {
    let mut archive = File::open(archive_path)?;
    archive.seek(SeekFrom::Start(entry_offset))?;
    let entry = archive.take(entry_size);
    let decoder = flate2::read::GzDecoder::new(entry);
    Ok(decoder.header().is_some())
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::io::{Cursor, Write};

    use flate2::Compression;
    use flate2::write::GzEncoder;
    use tar::{Builder, Header};
    use tempfile::tempdir;

    use super::{
        ChosenImageLayers, copy_archive_entry_with_digest, materialize_layers_from_saved_archive,
    };
    use crate::digest::canonical_digest_bytes;
    use crate::docker::daemon::DaemonImage;

    #[test]
    fn daemon_layer_materialization_preserves_uncompressed_entries() {
        let dir = tempdir().expect("tempdir should create");
        let destination = dir.path().join("layer.tar");
        let layer = b"uncompressed layer tar bytes";
        let digest = canonical_digest_bytes(layer);

        let actual = copy_archive_entry_with_digest(&mut Cursor::new(layer), &destination, &digest)
            .expect("uncompressed layer should materialize");

        assert_eq!(actual, digest);
        assert_eq!(
            fs::read(destination).expect("materialized layer should read"),
            layer
        );
    }

    #[test]
    fn daemon_layer_materialization_prefers_a_matching_raw_digest_over_gzip_decoding() {
        let dir = tempdir().expect("tempdir should create");
        let destination = dir.path().join("layer.tar");
        let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
        encoder
            .write_all(b"decompressed bytes must not be selected")
            .expect("layer should gzip successfully");
        let layer = encoder.finish().expect("gzip stream should finish");
        let digest = canonical_digest_bytes(&layer);

        let actual =
            copy_archive_entry_with_digest(&mut Cursor::new(&layer), &destination, &digest)
                .expect("matching raw bytes should materialize without decoding");

        assert_eq!(actual, digest);
        assert_eq!(
            fs::read(destination).expect("materialized layer should read"),
            layer
        );
    }

    #[test]
    fn daemon_layer_materialization_hashes_gzip_bytes_without_expanding_them() {
        let dir = tempdir().expect("tempdir should create");
        let destination = dir.path().join("layer.tar");
        // An empty tar stream reproduces the well-known Docker diff_id
        // sha256:5f70bf18..., whose gzip-compressed blob has a different
        // registry digest.
        let layer = vec![0_u8; 1024];
        let digest = canonical_digest_bytes(&layer);
        let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
        encoder
            .write_all(&layer)
            .expect("layer should gzip successfully");
        let compressed = encoder.finish().expect("gzip stream should finish");

        let actual = copy_archive_entry_with_digest(
            &mut Cursor::new(compressed.clone()),
            &destination,
            &digest,
        )
        .expect("gzip bytes should be copied without decoding");

        assert_eq!(actual, canonical_digest_bytes(&compressed));
        assert_ne!(actual, digest);
        assert_eq!(
            fs::read(destination).expect("materialized layer should read"),
            compressed
        );
    }

    #[test]
    fn containerd_style_docker_save_archive_materializes_compressed_blob_by_diff_id() {
        let dir = tempdir().expect("tempdir should create");
        let archive_path = dir.path().join("image.tar");
        let output_root = dir.path().join("materialized");
        fs::create_dir(&output_root).expect("output directory should create");

        let layer = vec![0_u8; 1024];
        let diff_id = canonical_digest_bytes(&layer);
        assert_eq!(
            diff_id,
            "sha256:5f70bf18a086007016e948b04aed3b82103a36bea41755b6cddfaf10ace3c6ef"
        );

        let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
        encoder
            .write_all(&layer)
            .expect("layer should gzip successfully");
        let compressed = encoder.finish().expect("gzip stream should finish");
        assert_ne!(canonical_digest_bytes(&compressed), diff_id);

        let blob_digest = canonical_digest_bytes(&compressed);
        let blob_path = format!(
            "blobs/sha256/{}",
            blob_digest
                .strip_prefix("sha256:")
                .expect("test blob should use sha256")
        );
        let manifest = serde_json::to_vec(&serde_json::json!([{
            "Config": "blobs/sha256/config",
            "RepoTags": ["example.test/image:latest"],
            "Layers": [&blob_path]
        }]))
        .expect("manifest should serialize");

        let archive_file = fs::File::create(&archive_path).expect("archive should create");
        let mut archive = Builder::new(archive_file);
        append_archive_bytes(&mut archive, "manifest.json", &manifest);
        append_archive_bytes(&mut archive, &blob_path, &compressed);
        archive.finish().expect("archive should finish");
        drop(archive);

        let image: DaemonImage = serde_json::from_value(serde_json::json!({
            "Id": "sha256:test-image",
            "RepoTags": ["example.test/image:latest"],
            "RootFS": { "Layers": [&diff_id] }
        }))
        .expect("daemon image should deserialize");
        let chosen = ChosenImageLayers {
            image,
            diff_ids: vec![diff_id.clone()],
        };

        let paths = materialize_layers_from_saved_archive(&archive_path, &chosen, &output_root)
            .expect("containerd-style archive should materialize");
        let materialized = paths
            .get(&diff_id)
            .expect("materialized diff_id should be present");
        assert_eq!(
            fs::read(&materialized.path).expect("materialized layer should read"),
            compressed
        );
        assert_eq!(materialized.digest, blob_digest);
        assert_eq!(materialized.size, compressed.len() as i64);
        assert!(materialized.gzip_compressed);
    }

    fn append_archive_bytes(archive: &mut Builder<fs::File>, path: &str, bytes: &[u8]) {
        let mut header = Header::new_gnu();
        header.set_size(bytes.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        archive
            .append_data(&mut header, path, bytes)
            .expect("archive entry should append");
    }
}
