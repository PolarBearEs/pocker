use std::path::PathBuf;

use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};
use tokio::net::TcpStream;

use crate::error::Result;

pub(super) enum RegistryBody {
    Empty,
    Text(Vec<u8>),
    File(PathBuf),
}

pub(super) struct RegistryResponse {
    status: u16,
    reason: &'static str,
    content_type: String,
    body: RegistryBody,
    content_length: u64,
    digest: Option<String>,
    range: Option<ResponseRange>,
}

#[derive(Clone, Copy)]
pub(super) struct ResponseRange {
    pub(super) start: u64,
    pub(super) end: u64,
    pub(super) total: u64,
}

impl RegistryResponse {
    pub(super) fn empty(status: u16, reason: &'static str) -> Self {
        Self {
            status,
            reason,
            content_type: "text/plain".to_string(),
            body: RegistryBody::Empty,
            content_length: 0,
            digest: None,
            range: None,
        }
    }

    pub(super) fn text(status: u16, reason: &'static str, text: String) -> Self {
        Self {
            status,
            reason,
            content_type: "text/plain".to_string(),
            content_length: text.len() as u64,
            body: RegistryBody::Text(text.into_bytes()),
            digest: None,
            range: None,
        }
    }

    pub(super) fn file(
        status: u16,
        reason: &'static str,
        content_type: String,
        path: PathBuf,
        content_length: u64,
        headers_only: bool,
    ) -> Self {
        Self {
            status,
            reason,
            content_type,
            content_length,
            body: if headers_only {
                RegistryBody::Empty
            } else {
                RegistryBody::File(path)
            },
            digest: None,
            range: None,
        }
    }

    pub(super) fn with_digest(mut self, digest: &str) -> Self {
        self.digest = Some(digest.to_string());
        self
    }

    pub(super) fn with_range(mut self, range: Option<&str>) -> Self {
        let Some(range) = range else {
            return self;
        };
        let Some(range) = parse_byte_range(range, self.content_length) else {
            return RegistryResponse::empty(416, "Range Not Satisfiable");
        };
        self.status = 206;
        self.reason = "Partial Content";
        self.content_length = range.end - range.start + 1;
        self.range = Some(range);
        self
    }
}

pub(super) fn parse_byte_range(value: &str, total: u64) -> Option<ResponseRange> {
    let value = value.strip_prefix("bytes=")?;
    let (start, end) = value.split_once('-')?;
    if start.is_empty() {
        return None;
    }
    let start = start.parse::<u64>().ok()?;
    let end = if end.is_empty() {
        total.checked_sub(1)?
    } else {
        end.parse::<u64>().ok()?
    };
    if start > end || end >= total {
        return None;
    }
    Some(ResponseRange { start, end, total })
}

pub(super) async fn write_response(
    stream: &mut TcpStream,
    response: RegistryResponse,
) -> Result<()> {
    let content_type =
        safe_header_value(&response.content_type).unwrap_or("application/octet-stream");
    let mut headers = format!(
        "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n",
        response.status, response.reason, content_type, response.content_length
    );
    headers.push_str("Docker-Distribution-API-Version: registry/2.0\r\n");
    if let Some(digest) = &response.digest {
        headers.push_str(&format!("Docker-Content-Digest: {digest}\r\n"));
    }
    if let Some(range) = response.range {
        headers.push_str(&format!(
            "Content-Range: bytes {}-{}/{}\r\n",
            range.start, range.end, range.total
        ));
    }
    headers.push_str("\r\n");
    stream.write_all(headers.as_bytes()).await?;
    match response.body {
        RegistryBody::Empty => {}
        RegistryBody::Text(bytes) => stream.write_all(&bytes).await?,
        RegistryBody::File(path) => {
            let mut file = tokio::fs::File::open(path).await?;
            if let Some(range) = response.range {
                file.seek(std::io::SeekFrom::Start(range.start)).await?;
                let mut take = file.take(response.content_length);
                tokio::io::copy(&mut take, stream).await?;
            } else {
                tokio::io::copy(&mut file, stream).await?;
            }
        }
    }
    stream.flush().await?;
    stream.shutdown().await?;
    Ok(())
}

fn safe_header_value(value: &str) -> Option<&str> {
    if value.bytes().any(|byte| matches!(byte, b'\r' | b'\n')) {
        None
    } else {
        Some(value)
    }
}

#[cfg(test)]
mod tests {
    use super::safe_header_value;

    #[test]
    fn header_value_rejects_crlf() {
        assert_eq!(
            safe_header_value("application/octet-stream"),
            Some("application/octet-stream")
        );
        assert_eq!(safe_header_value("text/plain\r\nInjected: yes"), None);
        assert_eq!(safe_header_value("text/plain\nInjected: yes"), None);
    }
}
