use std::path::Path;

use reqwest::StatusCode;
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::Value;
use tokio::io::DuplexStream;
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

impl DockerDaemon {
    pub(super) fn connect() -> Result<Self> {
        Ok(Self {
            transport: DockerTransport::connect()?,
        })
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

    pub(super) async fn inspect_image_json(&self, image: &str) -> Result<Option<Value>> {
        self.inspect_image_bytes(image)
            .await?
            .map(|bytes| serde_json::from_slice(&bytes).map_err(Into::into))
            .transpose()
    }

    async fn inspect_image_bytes(&self, image: &str) -> Result<Option<Vec<u8>>> {
        let response = self
            .transport
            .request_bytes(
                "GET",
                &format!("/images/{}/json", encode_path_segment(image)),
                None,
            )
            .await?;
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
                    encode_query_value(repository),
                    encode_query_value(tag)
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
                    encode_query_value(repository),
                    encode_query_value(tag)
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
}

#[derive(Debug, Clone, Deserialize)]
struct RootFs {
    #[serde(default, rename = "Layers")]
    layers: Option<Vec<String>>,
}

impl DaemonImage {
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
