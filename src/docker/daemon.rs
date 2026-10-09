use std::collections::HashMap;
use std::path::Path;

use reqwest::StatusCode;
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::Value;
use tokio::io::DuplexStream;
use tokio::sync::OnceCell;
use tokio_util::io::ReaderStream;

use crate::error::Result;

use super::transport::{DockerTransport, ensure_success_status};
use super::{
    encode_path_segment, encode_query_value, ensure_json_stream_success, split_tagged_reference,
};

#[derive(Debug, Clone)]
pub(super) struct DockerDaemon {
    transport: DockerTransport,
}

static SHARED_DAEMON: OnceCell<DockerDaemon> = OnceCell::const_new();

impl DockerDaemon {
    pub(super) fn connect() -> Result<Self> {
        Ok(Self {
            transport: DockerTransport::connect()?,
        })
    }

    pub(super) async fn shared() -> Result<&'static Self> {
        SHARED_DAEMON
            .get_or_try_init(|| async { Self::connect() })
            .await
    }

    pub(super) async fn load_archive(&self, path: &Path) -> Result<()> {
        self.transport.load_archive(path).await
    }

    pub(super) async fn load_archive_stream(
        &self,
        stream: ReaderStream<DuplexStream>,
    ) -> Result<()> {
        self.transport.load_archive_stream(stream).await
    }

    pub(super) async fn inspect_daemon_image(&self, image: &str) -> Result<Option<DaemonImage>> {
        self.inspect_image_bytes(image)
            .await?
            .map(|bytes| serde_json::from_slice(&bytes).map_err(Into::into))
            .transpose()
    }

    /// Inspect including the per-platform manifest list that Docker's
    /// containerd image store reports with `manifests=1` (API 1.48+). Older
    /// daemons ignore the query parameter and omit the list.
    pub(super) async fn inspect_daemon_image_with_manifests(
        &self,
        image: &str,
    ) -> Result<Option<DaemonImage>> {
        self.inspect_image_bytes_at(&format!(
            "/images/{}/json?manifests=1",
            encode_path_segment(image)
        ))
        .await?
        .map(|bytes| serde_json::from_slice(&bytes).map_err(Into::into))
        .transpose()
    }

    pub(super) async fn inspect_image_json(&self, image: &str) -> Result<Option<Value>> {
        self.inspect_image_bytes(image)
            .await?
            .map(|bytes| serde_json::from_slice(&bytes).map_err(Into::into))
            .transpose()
    }

    async fn inspect_image_bytes(&self, image: &str) -> Result<Option<Vec<u8>>> {
        self.inspect_image_bytes_at(&format!("/images/{}/json", encode_path_segment(image)))
            .await
    }

    async fn inspect_image_bytes_at(&self, path: &str) -> Result<Option<Vec<u8>>> {
        let response = self.transport.request_bytes("GET", path, None).await?;
        if response.status == StatusCode::NOT_FOUND {
            return Ok(None);
        }
        ensure_success_status(
            response.status,
            response.body.clone(),
            "docker image inspect",
        )?;
        Ok(Some(response.body))
    }

    pub(super) async fn list_image_summaries(&self) -> Result<Vec<DaemonImageSummary>> {
        self.get_json("/images/json?all=1", "docker image ls").await
    }

    pub(super) async fn save_image(&self, image: &str, path: &Path) -> Result<()> {
        self.transport
            .save_response_to_file(
                &format!("/images/{}/get", encode_path_segment(image)),
                path,
                "docker image save",
            )
            .await
    }

    pub(super) async fn pull_image(&self, reference: &str) -> Result<()> {
        let (repository, tag) = split_tagged_reference(reference)?;
        let response = self
            .transport
            .request_bytes(
                "POST",
                &format!(
                    "/images/create?fromImage={}&tag={}",
                    encode_query_value(&repository),
                    encode_query_value(&tag)
                ),
                None,
            )
            .await?;
        ensure_success_status(response.status, response.body.clone(), "docker image pull")?;
        ensure_json_stream_success(
            String::from_utf8_lossy(&response.body).into_owned(),
            "docker image pull",
        )
    }

    pub(super) async fn tag_image(&self, source: &str, target: &str) -> Result<()> {
        let (repository, tag) = split_tagged_reference(target)?;
        let response = self
            .transport
            .request_bytes(
                "POST",
                &format!(
                    "/images/{}/tag?repo={}&tag={}",
                    encode_path_segment(source),
                    encode_query_value(&repository),
                    encode_query_value(&tag)
                ),
                None,
            )
            .await?;
        ensure_success_status(response.status, response.body, "docker image tag")
    }

    pub(super) async fn remove_image_tag(&self, reference: &str) -> Result<()> {
        let response = self
            .transport
            .request_bytes(
                "DELETE",
                &format!(
                    "/images/{}?force=1&noprune=1",
                    encode_path_segment(reference)
                ),
                None,
            )
            .await?;
        ensure_success_status(response.status, response.body, "docker image remove")
    }

    async fn get_json<T>(&self, path: &str, action: &str) -> Result<T>
    where
        T: DeserializeOwned,
    {
        let response = self.transport.request_bytes("GET", path, None).await?;
        ensure_success_status(response.status, response.body.clone(), action)?;
        serde_json::from_slice(&response.body).map_err(Into::into)
    }
}

#[derive(Debug, Clone, Deserialize)]
pub(super) struct DaemonImageSummary {
    #[serde(rename = "Id")]
    pub(super) id: String,
    #[serde(rename = "Created")]
    pub(super) created: Option<i64>,
    #[serde(rename = "RepoTags")]
    pub(super) repo_tags: Option<Vec<String>>,
    #[serde(rename = "Size")]
    pub(super) size: Option<u64>,
}

#[derive(Debug, Clone, Deserialize)]
pub(super) struct DaemonImage {
    #[serde(rename = "Id")]
    pub(super) id: String,
    #[serde(rename = "RepoTags")]
    repo_tags: Option<Vec<String>>,
    #[serde(default, rename = "RootFS")]
    rootfs: Option<RootFs>,
    #[serde(default, rename = "Descriptor")]
    descriptor: Option<DaemonDescriptor>,
    #[serde(default, rename = "Manifests")]
    manifests: Option<Vec<DaemonManifestSummary>>,
}

#[derive(Debug, Clone, Deserialize)]
struct DaemonDescriptor {
    #[serde(default)]
    digest: Option<String>,
    #[serde(default)]
    annotations: Option<HashMap<String, String>>,
}

#[derive(Debug, Clone, Deserialize)]
struct DaemonManifestSummary {
    #[serde(rename = "Descriptor")]
    descriptor: DaemonDescriptor,
    #[serde(default, rename = "Available")]
    available: bool,
    #[serde(default, rename = "ImageData")]
    image_data: Option<DaemonManifestImageData>,
}

#[derive(Debug, Clone, Deserialize)]
struct DaemonManifestImageData {
    #[serde(default, rename = "Size")]
    size: Option<DaemonManifestImageSize>,
}

#[derive(Debug, Clone, Deserialize)]
struct DaemonManifestImageSize {
    #[serde(default, rename = "Unpacked")]
    unpacked: i64,
}

#[derive(Debug, Clone, Deserialize)]
struct RootFs {
    #[serde(default, rename = "Layers")]
    layers: Option<Vec<String>>,
}

impl DaemonImage {
    pub(super) fn config_digest_annotation(&self) -> Option<&str> {
        self.descriptor
            .as_ref()?
            .annotations
            .as_ref()?
            .get(super::POCKER_CONFIG_DIGEST_ANNOTATION)
            .map(String::as_str)
    }

    pub(super) fn descriptor_digest(&self) -> Option<&str> {
        self.descriptor.as_ref()?.digest.as_deref()
    }

    /// Whether the image's index lists `digest` as a platform manifest that
    /// the daemon can run. Containerd may have discarded the compressed
    /// content after unpacking (`Available` is false), but an unpacked
    /// snapshot still makes the image usable.
    pub(super) fn has_usable_platform_manifest(&self, digest: &str) -> bool {
        self.manifests.iter().flatten().any(|manifest| {
            manifest.descriptor.digest.as_deref() == Some(digest)
                && (manifest.available
                    || manifest
                        .image_data
                        .as_ref()
                        .and_then(|data| data.size.as_ref())
                        .is_some_and(|size| size.unpacked > 0))
        })
    }

    pub(super) fn rootfs_layers(&self) -> &[String] {
        self.rootfs
            .as_ref()
            .and_then(|rootfs| rootfs.layers.as_deref())
            .unwrap_or(&[])
    }

    pub(super) fn label(&self) -> String {
        self.repo_tags
            .as_ref()
            .and_then(|tags| tags.first())
            .cloned()
            .unwrap_or_else(|| self.id.clone())
    }
}
