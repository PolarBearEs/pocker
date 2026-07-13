use std::path::Path;
#[cfg(unix)]
use std::path::PathBuf;

use futures_util::StreamExt;
use reqwest::{Client, Response};
use tokio::io::AsyncWriteExt;
use tokio::io::DuplexStream;
use tokio_util::io::ReaderStream;

use crate::error::{DockerPullError, Result};
use crate::http::USER_AGENT;

use super::{AtomicOutputFile, DockerResponse, build_failure_error};

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
        let mut file = AtomicOutputFile::create(output).await?;
        let mut stream = response.bytes_stream();
        while let Some(chunk) = stream.next().await {
            file.file_mut().write_all(&chunk?).await?;
        }
        file.persist(output).await
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
    Client::builder().user_agent(USER_AGENT).http1_only()
}

async fn ensure_success(response: Response, action: &str) -> Result<Response> {
    if response.status().is_success() {
        return Ok(response);
    }

    let status = response.status();
    let body = response.text().await.unwrap_or_default();
    Err(build_failure_error(status, body.as_bytes(), action))
}

#[cfg(test)]
mod tests {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;
    use tokio::time::{sleep, timeout};

    use super::{ReqwestTransport, builder};

    #[tokio::test]
    async fn image_load_allows_slow_daemon_response_headers() {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("listener should bind");
        let address = listener.local_addr().expect("listener address");

        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("connection should arrive");
            let mut request = Vec::new();
            let mut buffer = [0_u8; 1024];
            while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                let read = stream.read(&mut buffer).await.expect("request read");
                assert_ne!(read, 0, "client closed before sending request headers");
                request.extend_from_slice(&buffer[..read]);
            }
            while !request_body_complete(&request) {
                let read = stream.read(&mut buffer).await.expect("request body read");
                assert_ne!(read, 0, "client closed before sending request body");
                request.extend_from_slice(&buffer[..read]);
            }

            // Docker can spend a long time importing an already-uploaded archive
            // before it sends response headers. The Docker client has no
            // request/read timeout so slow devices are not failed mid-load.
            sleep(std::time::Duration::from_millis(200)).await;
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                .await
                .expect("response write");
        });

        let archive = tempfile::NamedTempFile::new().expect("temp archive");
        std::fs::write(archive.path(), b"not really a tar").expect("archive write");
        let transport = ReqwestTransport {
            client: builder().build().expect("client"),
            base_url: format!("http://{address}"),
        };

        timeout(
            std::time::Duration::from_secs(2),
            transport.load_archive(archive.path()),
        )
        .await
        .expect("load should not hang")
        .expect("load should tolerate delayed daemon response");
        server.await.expect("server task");
    }

    #[tokio::test]
    async fn failed_image_save_preserves_existing_destination() {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("listener should bind");
        let address = listener.local_addr().expect("listener address");
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("connection should arrive");
            let mut request = [0_u8; 2048];
            let _ = stream.read(&mut request).await;
            stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Length: 12\r\nConnection: close\r\n\r\npartial",
                )
                .await
                .expect("partial response should be written");
        });
        let dir = tempfile::tempdir().expect("tempdir");
        let output = dir.path().join("image.tar");
        tokio::fs::write(&output, b"existing archive")
            .await
            .expect("existing destination should be written");
        let transport = ReqwestTransport {
            client: builder().build().expect("client"),
            base_url: format!("http://{address}"),
        };

        transport
            .save_response_to_file("/images/get?names=test", &output, "docker image save")
            .await
            .expect_err("truncated response should fail");
        server.await.expect("server task");

        assert_eq!(
            tokio::fs::read(&output)
                .await
                .expect("existing destination should remain readable"),
            b"existing archive"
        );
        let mut read_dir = tokio::fs::read_dir(dir.path())
            .await
            .expect("output directory should be readable");
        let mut entries = Vec::new();
        while let Some(entry) = read_dir
            .next_entry()
            .await
            .expect("directory entry should be readable")
        {
            entries.push(entry);
        }
        assert_eq!(
            entries.len(),
            1,
            "failed export should clean up its temp file"
        );
    }

    fn request_body_complete(request: &[u8]) -> bool {
        let Some(header_end) = request.windows(4).position(|window| window == b"\r\n\r\n") else {
            return false;
        };
        let body_start = header_end + 4;
        let headers = String::from_utf8_lossy(&request[..header_end]).to_ascii_lowercase();
        if headers.contains("transfer-encoding: chunked") {
            return request[body_start..]
                .windows(5)
                .any(|window| window == b"0\r\n\r\n");
        }
        let Some(content_length) = headers.lines().find_map(|line| {
            let value = line.strip_prefix("content-length:")?;
            value.trim().parse::<usize>().ok()
        }) else {
            return true;
        };
        request.len().saturating_sub(body_start) >= content_length
    }
}
