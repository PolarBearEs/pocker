mod fs;

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use tokio::fs as tokio_fs;

use crate::error::{DockerPullError, Result};
use crate::registry::Descriptor;
use fs::{
    atomic_write_bytes, atomic_write_json, digest_file, ensure_directory, read_json_if_exists,
    reconcile_partial_file,
};

#[derive(Debug, Clone)]
pub struct Store {
    root: PathBuf,
}

#[derive(Debug, Clone)]
pub struct StoredReference {
    pub reference: String,
    pub manifest: Descriptor,
    pub config_digest: String,
}

#[derive(Debug, Clone)]
pub struct DownloadPlan {
    pub durable_offset: u64,
    pub partial_path: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClearedCache {
    pub files: Vec<ClearedCacheFile>,
    pub reclaimed_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClearedCacheFile {
    pub path: PathBuf,
    pub size: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct DownloadCheckpoint {
    reference: String,
    media_type: String,
    expected_size: u64,
    durable_offset: u64,
}

impl Store {
    pub async fn open(root: PathBuf) -> Result<Self> {
        ensure_directory(&root)?;
        ensure_directory(&root.join("blobs"))?;
        ensure_directory(&root.join("blobs").join("sha256"))?;
        ensure_directory(&root.join("partials"))?;
        ensure_directory(&root.join("partials").join("sha256"))?;
        Ok(Self { root })
    }

    pub fn blob_path(&self, digest: &str) -> Result<PathBuf> {
        digest_path(self.root.join("blobs"), digest)
    }

    pub fn partial_path(&self, digest: &str) -> Result<PathBuf> {
        let path = digest_path(self.root.join("partials"), digest)?;
        Ok(path.with_extension("part"))
    }

    fn partial_metadata_path(&self, digest: &str) -> Result<PathBuf> {
        let path = digest_path(self.root.join("partials"), digest)?;
        Ok(path.with_extension("json"))
    }

    pub async fn ensure_blob_complete(&self, digest: &str, expected_size: i64) -> Result<bool> {
        let path = self.blob_path(digest)?;
        if !path.exists() {
            return Ok(false);
        }
        let metadata = tokio_fs::metadata(&path).await?;
        if metadata.len() != expected_size as u64 {
            tokio_fs::remove_file(&path).await?;
            return Ok(false);
        }
        let computed = digest_file(&path)?;
        if computed != digest {
            tokio_fs::remove_file(&path).await?;
            return Ok(false);
        }
        Ok(true)
    }

    pub async fn save_blob_bytes(&self, descriptor: &Descriptor, bytes: &[u8]) -> Result<()> {
        let path = self.blob_path(&descriptor.digest)?;
        if path.exists()
            && self
                .ensure_blob_complete(&descriptor.digest, descriptor.size)
                .await?
        {
            return Ok(());
        }
        let actual_digest = digest_bytes(bytes);
        if actual_digest != descriptor.digest {
            return Err(DockerPullError::DigestMismatch {
                digest: descriptor.digest.clone(),
                expected: descriptor.digest.clone(),
                actual: actual_digest,
            });
        }
        atomic_write_bytes(&path, bytes)?;
        Ok(())
    }

    pub async fn prepare_download(
        &self,
        reference: &str,
        descriptor: &Descriptor,
        expected_size: u64,
    ) -> Result<DownloadPlan> {
        let partial_path = self.partial_path(&descriptor.digest)?;
        let metadata_path = self.partial_metadata_path(&descriptor.digest)?;
        let stored = read_json_if_exists::<DownloadCheckpoint>(&metadata_path)?;
        let durable_offset = reconcile_partial_file(
            &partial_path,
            stored
                .as_ref()
                .map(|record| record.durable_offset)
                .unwrap_or(0),
        )?;
        atomic_write_json(
            &metadata_path,
            &DownloadCheckpoint {
                reference: reference.to_string(),
                media_type: descriptor.media_type.clone(),
                expected_size,
                durable_offset,
            },
        )?;
        Ok(DownloadPlan {
            durable_offset,
            partial_path,
        })
    }

    pub fn checkpoint_download(
        &self,
        digest: &str,
        durable_offset: u64,
        expected_size: u64,
    ) -> Result<()> {
        let path = self.partial_metadata_path(digest)?;
        let stored =
            read_json_if_exists::<DownloadCheckpoint>(&path)?.unwrap_or(DownloadCheckpoint {
                reference: String::new(),
                media_type: String::new(),
                expected_size,
                durable_offset: 0,
            });
        atomic_write_json(
            &path,
            &DownloadCheckpoint {
                reference: stored.reference,
                media_type: stored.media_type,
                expected_size,
                durable_offset,
            },
        )
    }

    pub async fn reset_partial(&self, digest: &str, expected_size: u64) -> Result<()> {
        let partial = self.partial_path(digest)?;
        if partial.exists() {
            tokio_fs::remove_file(&partial).await?;
        }
        self.checkpoint_download(digest, 0, expected_size)?;
        Ok(())
    }

    pub async fn finalize_download(&self, descriptor: &Descriptor) -> Result<()> {
        let partial = self.partial_path(&descriptor.digest)?;
        let final_path = self.blob_path(&descriptor.digest)?;
        if !partial.exists() {
            return Err(DockerPullError::MissingBlobFile(
                descriptor.digest.clone(),
                partial,
            ));
        }
        let computed = digest_file(&partial)?;
        if computed != descriptor.digest {
            return Err(DockerPullError::DigestMismatch {
                digest: descriptor.digest.clone(),
                expected: descriptor.digest.clone(),
                actual: computed,
            });
        }
        if let Some(parent) = final_path.parent() {
            ensure_directory(parent)?;
        }
        tokio_fs::rename(&partial, &final_path).await?;

        let metadata_path = self.partial_metadata_path(&descriptor.digest)?;
        if metadata_path.exists() {
            tokio_fs::remove_file(metadata_path).await?;
        }
        Ok(())
    }

    pub async fn prune_reference_layer_blobs(&self, reference: &StoredReference) -> Result<usize> {
        let manifest_path = self.blob_path(&reference.manifest.digest)?;
        let manifest_bytes = tokio_fs::read(&manifest_path).await?;
        let manifest: serde_json::Value = serde_json::from_slice(&manifest_bytes)?;
        let layers = manifest
            .get("layers")
            .and_then(|layers| layers.as_array())
            .ok_or_else(|| DockerPullError::BadResponse("manifest layers missing".into()))?;

        let mut removed = 0_usize;
        for layer in layers {
            let digest = layer
                .get("digest")
                .and_then(|value| value.as_str())
                .ok_or_else(|| DockerPullError::BadResponse("layer digest missing".into()))?;
            let path = self.blob_path(digest)?;
            if path.exists() {
                tokio_fs::remove_file(path).await?;
                removed += 1;
            }
        }

        Ok(removed)
    }

    pub async fn clear(&self) -> Result<ClearedCache> {
        let root = self.root.clone();
        let files = tokio::task::spawn_blocking(move || collect_cache_files(&root))
            .await
            .map_err(|e| {
                DockerPullError::InvalidInput(format!("cache scan task panicked: {e}"))
            })??;
        let reclaimed_bytes = files
            .iter()
            .fold(0_u64, |total, file| total.saturating_add(file.size));

        if self.root.exists() {
            tokio_fs::remove_dir_all(&self.root).await?;
        }

        Self::open(self.root.clone()).await?;
        Ok(ClearedCache {
            files,
            reclaimed_bytes,
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }
}

fn digest_path(root: PathBuf, digest: &str) -> Result<PathBuf> {
    let (algorithm, value) = digest.split_once(':').ok_or_else(|| {
        DockerPullError::InvalidInput(format!("invalid digest format `{digest}`"))
    })?;
    Ok(root.join(algorithm).join(value))
}

fn digest_bytes(bytes: &[u8]) -> String {
    use sha2::{Digest as _, Sha256};

    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("sha256:{}", hex::encode(hasher.finalize()))
}

fn collect_cache_files(root: &Path) -> Result<Vec<ClearedCacheFile>> {
    let mut files = Vec::new();
    collect_cache_files_recursive(root, root, &mut files)?;
    files.sort_by(|left, right| left.path.cmp(&right.path));
    Ok(files)
}

fn collect_cache_files_recursive(
    root: &Path,
    current: &Path,
    files: &mut Vec<ClearedCacheFile>,
) -> Result<()> {
    if !current.exists() {
        return Ok(());
    }

    for entry in std::fs::read_dir(current)? {
        let entry = entry?;
        let path = entry.path();
        let metadata = entry.metadata()?;
        if metadata.is_dir() {
            collect_cache_files_recursive(root, &path, files)?;
            continue;
        }

        if metadata.is_file() {
            let relative = path
                .strip_prefix(root)
                .unwrap_or(path.as_path())
                .to_path_buf();
            files.push(ClearedCacheFile {
                path: relative,
                size: metadata.len(),
            });
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;

    use super::{ClearedCacheFile, Store, StoredReference};
    use crate::registry::Descriptor;

    #[tokio::test]
    async fn prepare_download_clamps_to_existing_partial_size() {
        let dir = tempdir().expect("tempdir should create");
        let store = Store::open(dir.path().to_path_buf())
            .await
            .expect("store should open");
        let descriptor = Descriptor {
            media_type: "application/octet-stream".into(),
            digest: "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
                .into(),
            size: 10,
            platform: None,
            annotations: None,
        };
        store
            .prepare_download(
                "registry-1.docker.io/library/alpine:latest",
                &descriptor,
                10,
            )
            .await
            .expect("download checkpoint should be created");
        let partial = store
            .partial_path(&descriptor.digest)
            .expect("partial path");
        std::fs::write(&partial, b"1234").expect("partial file should be written");
        store
            .checkpoint_download(&descriptor.digest, 8, 10)
            .expect("download state should be updated");
        let plan = store
            .prepare_download(
                "registry-1.docker.io/library/alpine:latest",
                &descriptor,
                10,
            )
            .await
            .expect("download plan should be created");
        assert_eq!(plan.durable_offset, 4);
    }

    #[tokio::test]
    async fn prune_reference_layer_blobs_removes_only_layer_blobs() {
        let dir = tempdir().expect("tempdir should create");
        let store = Store::open(dir.path().to_path_buf())
            .await
            .expect("store should open");
        let manifest = Descriptor {
            media_type: "application/vnd.oci.image.manifest.v1+json".into(),
            digest: "sha256:1111111111111111111111111111111111111111111111111111111111111111"
                .into(),
            size: 321,
            platform: Some(
                crate::platform::Platform::parse("linux/amd64").expect("platform should parse"),
            ),
            annotations: None,
        };
        let config = Descriptor {
            media_type: "application/vnd.oci.image.config.v1+json".into(),
            digest: "sha256:2222222222222222222222222222222222222222222222222222222222222222"
                .into(),
            size: 123,
            platform: None,
            annotations: None,
        };
        let layer_digest =
            "sha256:3333333333333333333333333333333333333333333333333333333333333333";
        let manifest_bytes = format!(
            r#"{{
                "schemaVersion": 2,
                "config": {{"mediaType":"{}","digest":"{}","size":{}}},
                "layers": [{{"mediaType":"application/vnd.oci.image.layer.v1.tar+gzip","digest":"{}","size":5}}]
            }}"#,
            config.media_type, config.digest, config.size, layer_digest
        );

        let manifest_path = store.blob_path(&manifest.digest).expect("manifest path");
        std::fs::create_dir_all(manifest_path.parent().expect("manifest parent"))
            .expect("manifest parent should exist");
        std::fs::write(&manifest_path, manifest_bytes.as_bytes())
            .expect("manifest should be written");
        let config_path = store.blob_path(&config.digest).expect("config path");
        std::fs::create_dir_all(config_path.parent().expect("config parent"))
            .expect("config parent should exist");
        std::fs::write(&config_path, br#"{"rootfs":{"diff_ids":[]}}"#)
            .expect("config should be written");
        let layer_path = store.blob_path(layer_digest).expect("layer path");
        std::fs::create_dir_all(layer_path.parent().expect("layer parent"))
            .expect("layer parent should exist");
        std::fs::write(&layer_path, b"layer").expect("layer should be written");

        let removed = store
            .prune_reference_layer_blobs(&StoredReference {
                reference: "registry-1.docker.io/library/alpine:latest".into(),
                manifest: manifest.clone(),
                config_digest: config.digest.clone(),
            })
            .await
            .expect("layer prune should succeed");

        assert_eq!(removed, 1);
        assert!(
            !layer_path.exists(),
            "layer blob should be removed after pruning"
        );
        assert!(
            store
                .blob_path(&manifest.digest)
                .expect("manifest path")
                .exists(),
            "manifest blob should be retained"
        );
        assert!(
            store
                .blob_path(&config.digest)
                .expect("config path")
                .exists(),
            "config blob should be retained"
        );
    }

    #[tokio::test]
    async fn clear_removes_cached_files_and_recreates_layout() {
        let dir = tempdir().expect("tempdir should create");
        let store = Store::open(dir.path().to_path_buf())
            .await
            .expect("store should open");
        let blob = store
            .blob_path("sha256:4444444444444444444444444444444444444444444444444444444444444444")
            .expect("blob path");
        let partial = store
            .partial_path("sha256:5555555555555555555555555555555555555555555555555555555555555555")
            .expect("partial path");
        std::fs::create_dir_all(blob.parent().expect("blob parent"))
            .expect("blob parent should exist");
        std::fs::create_dir_all(partial.parent().expect("partial parent"))
            .expect("partial parent should exist");
        std::fs::write(&blob, b"blob").expect("blob should be written");
        std::fs::write(&partial, b"partial").expect("partial should be written");

        let cleared = store.clear().await.expect("clear should succeed");

        assert!(
            !blob.exists(),
            "blob should be removed when clearing the cache"
        );
        assert!(
            !partial.exists(),
            "partial should be removed when clearing the cache"
        );
        assert!(
            store.root().join("blobs").join("sha256").exists(),
            "blob cache layout should be recreated"
        );
        assert!(
            store.root().join("partials").join("sha256").exists(),
            "partial cache layout should be recreated"
        );
        assert_eq!(cleared.reclaimed_bytes, 11);
        assert_eq!(
            cleared.files,
            vec![
                ClearedCacheFile {
                    path: "blobs/sha256/4444444444444444444444444444444444444444444444444444444444444444"
                        .into(),
                    size: 4,
                },
                ClearedCacheFile {
                    path: "partials/sha256/5555555555555555555555555555555555555555555555555555555555555555.part"
                        .into(),
                    size: 7,
                },
            ]
        );
    }
}
