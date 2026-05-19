use std::path::{Path, PathBuf};
use std::time::Duration;

use futures_util::StreamExt;
use reqwest::{Client, Response};
use tokio::io::AsyncWriteExt;
use tokio::io::DuplexStream;
use tokio_util::io::ReaderStream;

use crate::error::{DockerPullError, Result};

use super::DockerResponse;

#[derive(Debug, Clone)]
pub(in crate::docker) struct ReqwestTransport {
    client: Client,
    base_url: String,
}

impl ReqwestTransport {
    #[cfg(unix)]
    pub(super) fn unix_socket(path: PathBuf) -> Result<Self> {
        let client = builder().unix_socket(path).build()?;
        Ok(Self {
            client,
            base_url: "http://docker".to_string(),
        })
    }

    pub(super) fn http(base_url: String) -> Result<Self> {
        Ok(Self {
            client: builder().build()?,
            base_url,
        })
    }

    pub(super) async fn load_archive(&self, path: &Path) -> Result<()> {
        let file = tokio::fs::File::open(path).await?;
        let body = reqwest::Body::wrap_stream(ReaderStream::new(file));
        let response = self
            .client
            .post(self.url("/images/load?quiet=1"))
            .header(reqwest::header::CONTENT_TYPE, "application/x-tar")
            .body(body)
            .send()
            .await?;
        ensure_success(response, "docker image load").await?;
        Ok(())
    }

    pub(super) async fn load_archive_stream(
        &self,
        stream: ReaderStream<DuplexStream>,
    ) -> Result<()> {
        let body = reqwest::Body::wrap_stream(stream);
        let response = self
            .client
            .post(self.url("/images/load?quiet=1"))
            .header(reqwest::header::CONTENT_TYPE, "application/x-tar")
            .body(body)
            .send()
            .await?;
        ensure_success(response, "docker image load").await?;
        Ok(())
    }

    pub(super) async fn save_response_to_file(
        &self,
        path: &str,
        output: &Path,
        action: &str,
    ) -> Result<()> {
        let response = self.client.get(self.url(path)).send().await?;
        let response = ensure_success(response, action).await?;
        let mut file = tokio::fs::File::create(output).await?;
        let mut stream = response.bytes_stream();
        while let Some(chunk) = stream.next().await {
            file.write_all(&chunk?).await?;
        }
        file.flush().await?;
        Ok(())
    }

    pub(super) async fn request_bytes(
        &self,
        method: &str,
        path: &str,
        body: Option<Vec<u8>>,
    ) -> Result<DockerResponse> {
        let method = reqwest::Method::from_bytes(method.as_bytes()).map_err(|error| {
            DockerPullError::InvalidInput(format!("invalid docker API method: {error}"))
        })?;
        let mut request = self.client.request(method, self.url(path));
        if let Some(body) = body {
            request = request.body(body);
        }
        let response = request.send().await?;
        let status = response.status();
        let body = response.bytes().await?.to_vec();
        Ok(DockerResponse { status, body })
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.base_url, path)
    }
}

fn builder() -> reqwest::ClientBuilder {
    Client::builder()
        .connect_timeout(Duration::from_secs(20))
        .user_agent(format!("pocker/{}", env!("CARGO_PKG_VERSION")))
        .http1_only()
}

async fn ensure_success(response: Response, action: &str) -> Result<Response> {
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
