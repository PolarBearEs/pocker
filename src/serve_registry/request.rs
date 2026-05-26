use reqwest::header::RANGE;
use tokio::io::AsyncReadExt;
use tokio::net::TcpStream;
use tokio::time::{Duration, timeout};

use crate::error::{DockerPullError, Result};

// Cap request headers to avoid unbounded memory growth from malformed clients.
const MAX_REQUEST_HEAD_BYTES: usize = 64 * 1024;
// This protects the local registry from clients that connect and stop sending
// headers. It applies only before a request is parsed, not to blob streaming.
const REQUEST_HEAD_TIMEOUT: Duration = Duration::from_secs(120);

#[derive(Debug)]
pub(super) struct Request {
    pub(super) method: String,
    pub(super) path: String,
    pub(super) range: Option<String>,
}

pub(super) async fn read_request(stream: &mut TcpStream) -> Result<Request> {
    timeout(REQUEST_HEAD_TIMEOUT, read_request_inner(stream))
        .await
        .map_err(|_| DockerPullError::BadResponse("cache registry request timed out".into()))?
}

async fn read_request_inner(stream: &mut TcpStream) -> Result<Request> {
    let mut bytes = Vec::new();
    let mut buffer = [0_u8; 4096];
    loop {
        let read = stream.read(&mut buffer).await?;
        if read == 0 {
            return Err(DockerPullError::BadResponse(
                "cache registry request ended before headers".into(),
            ));
        }
        bytes.extend_from_slice(&buffer[..read]);
        if let Some(request) = parse_request_head(&bytes)? {
            return Ok(request);
        }
        if bytes.len() > MAX_REQUEST_HEAD_BYTES {
            return Err(DockerPullError::InvalidInput(
                "cache registry request headers are too large".into(),
            ));
        }
    }
}

fn parse_request_head(bytes: &[u8]) -> Result<Option<Request>> {
    let mut headers = [httparse::EMPTY_HEADER; 64];
    let mut request = httparse::Request::new(&mut headers);
    let parsed = request
        .parse(bytes)
        .map_err(|error| DockerPullError::BadResponse(format!("invalid HTTP request: {error}")))?;
    if parsed.is_partial() {
        return Ok(None);
    }

    let method = request
        .method
        .ok_or_else(|| DockerPullError::BadResponse("missing HTTP method".into()))?
        .to_string();
    let path = request
        .path
        .ok_or_else(|| DockerPullError::BadResponse("missing HTTP path".into()))?
        .to_string();
    let mut range = None;
    for header in request.headers {
        if header.name.eq_ignore_ascii_case(RANGE.as_str()) {
            let value = std::str::from_utf8(header.value).map_err(|error| {
                DockerPullError::BadResponse(format!("invalid HTTP header value: {error}"))
            })?;
            range = Some(value.trim().to_string());
            break;
        }
    }
    Ok(Some(Request {
        method,
        path,
        range,
    }))
}

#[cfg(test)]
mod tests {
    use super::parse_request_head;

    #[test]
    fn request_head_parses_method_path_and_range() {
        let request =
            parse_request_head(b"GET /v2/library/alpine/blobs/sha256:abc HTTP/1.1\r\nRange: bytes=4-\r\nHost: cache\r\n\r\n")
                .expect("request should parse")
                .expect("request should be complete");

        assert_eq!(request.method, "GET");
        assert_eq!(request.path, "/v2/library/alpine/blobs/sha256:abc");
        assert_eq!(request.range.as_deref(), Some("bytes=4-"));
    }

    #[test]
    fn request_head_uses_first_range_header() {
        let request =
            parse_request_head(b"GET /v2/library/alpine/blobs/sha256:abc HTTP/1.1\r\nRange: bytes=4-\r\nRange: bytes=7-\r\n\r\n")
                .expect("request should parse")
                .expect("request should be complete");

        assert_eq!(request.range.as_deref(), Some("bytes=4-"));
    }

    #[test]
    fn partial_request_head_waits_for_more_bytes() {
        let request = parse_request_head(b"GET /v2/ HTTP/1.1\r\nHost: cache\r\n")
            .expect("partial request should not fail");

        assert!(request.is_none());
    }
}
