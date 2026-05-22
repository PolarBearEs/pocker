use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use serde::Deserialize;
use sha2::{Digest as _, Sha256};
use tar::Archive;
use tempfile::{NamedTempFile, TempDir};
use tokio::task::JoinSet;

use crate::digest::parse_digest;
use crate::error::{DockerPullError, Result};
use crate::image::LayerSpec;
use crate::store::Store;

use super::daemon::{DaemonImage, DaemonImageSummary, DockerDaemon};

const DAEMON_INSPECT_CONCURRENCY: usize = 8;

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
    pub(crate) fn paths(&self) -> &HashMap<String, PathBuf> {
        &self.paths
    }
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
                queue.abort_all();
                return Err(error);
            }
            Err(error) => {
                queue.abort_all();
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
    let parsed = parse_digest(diff_id)?;
    Ok(root.join(format!("{}-{}.tar", parsed.algorithm, parsed.value)))
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
