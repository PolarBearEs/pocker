use reqwest::header::RANGE;
use tokio::io::AsyncReadExt;
use tokio::net::TcpStream;

use crate::error::{DockerPullError, Result};

const MAX_REQUEST_HEAD_BYTES: usize = 64 * 1024;

#[derive(Debug)]
pub(super) struct Request {
    pub(super) method: String,
    pub(super) path: String,
    pub(super) range: Option<String>,
}

pub(super) async fn read_request(stream: &mut TcpStream) -> Result<Request> {
    let mut bytes = Vec::new();
    let mut buffer = [0_u8; 4096];
    let header_end = loop {
        let scan_from = bytes.len().saturating_sub(3);
        let read = stream.read(&mut buffer).await?;
        if read == 0 {
            return Err(DockerPullError::BadResponse(
                "cache registry request ended before headers".into(),
            ));
        }
        bytes.extend_from_slice(&buffer[..read]);
        if let Some(index) = find_double_crlf(&bytes, scan_from) {
            break index + 4;
        }
        if bytes.len() > MAX_REQUEST_HEAD_BYTES {
            return Err(DockerPullError::InvalidInput(
                "cache registry request headers are too large".into(),
            ));
        }
    };

    parse_request_head(&bytes[..header_end])
}

fn parse_request_head(bytes: &[u8]) -> Result<Request> {
    let mut headers = [httparse::EMPTY_HEADER; 64];
    let mut request = httparse::Request::new(&mut headers);
    let parsed = request
        .parse(bytes)
        .map_err(|error| DockerPullError::BadResponse(format!("invalid HTTP request: {error}")))?;
    if parsed.is_partial() {
        return Err(DockerPullError::BadResponse(
            "cache registry request headers are incomplete".into(),
        ));
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
        }
    }
    Ok(Request {
        method,
        path,
        range,
    })
}

fn find_double_crlf(bytes: &[u8], start: usize) -> Option<usize> {
    let mut index = start;
    while index + 3 < bytes.len() {
        if &bytes[index..index + 4] == b"\r\n\r\n" {
            return Some(index);
        }
        index += 1;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::{find_double_crlf, parse_request_head};

    #[test]
    fn request_head_parses_method_path_and_range() {
        let request =
            parse_request_head(b"GET /v2/library/alpine/blobs/sha256:abc HTTP/1.1\r\nRange: bytes=4-\r\nHost: cache\r\n\r\n")
                .expect("request should parse");

        assert_eq!(request.method, "GET");
        assert_eq!(request.path, "/v2/library/alpine/blobs/sha256:abc");
        assert_eq!(request.range.as_deref(), Some("bytes=4-"));
    }

    #[test]
    fn double_crlf_scan_can_start_near_new_bytes() {
        let bytes = b"GET / HTTP/1.1\r\nHost: cache\r\n\r\n";

        assert_eq!(
            find_double_crlf(bytes, bytes.len() - 5),
            Some(bytes.len() - 4)
        );
    }
}
