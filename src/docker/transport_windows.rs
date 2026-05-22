#[cfg(windows)]
use std::path::{Path, PathBuf};
#[cfg(windows)]
use std::time::Duration;

#[cfg(windows)]
use futures_util::StreamExt;
use reqwest::StatusCode;
#[cfg(any(test, windows))]
use tokio::io::{AsyncReadExt, AsyncWriteExt};
#[cfg(windows)]
use tokio::net::windows::named_pipe::ClientOptions;
#[cfg(windows)]
use tokio_util::io::ReaderStream;

use crate::error::{DockerPullError, Result};
#[cfg(windows)]
use crate::http::USER_AGENT;

#[cfg(windows)]
use super::{DockerResponse, ensure_success_status};
#[cfg(windows)]
use tokio::io::DuplexStream;

#[cfg(windows)]
const ERROR_PIPE_BUSY: i32 = 231;
#[cfg(windows)]
const NAMED_PIPE_OPEN_RETRIES: usize = 20;
#[cfg(windows)]
const NAMED_PIPE_OPEN_RETRY_DELAY: Duration = Duration::from_millis(50);
#[cfg(windows)]
const MAX_RESPONSE_HEAD_BYTES: usize = 64 * 1024;

#[cfg(windows)]
pub(super) fn normalize_named_pipe_path(path: &str) -> Result<PathBuf> {
    if path.is_empty() {
        return Err(DockerPullError::InvalidInput(
            "docker host named pipe path is empty".into(),
        ));
    }

    let path = path.replace('/', "\\");
    if !path.starts_with(r"\\.\pipe\") {
        return Err(DockerPullError::InvalidInput(format!(
            "unsupported docker named pipe `{path}`"
        )));
    }
    Ok(PathBuf::from(path))
}

#[cfg(windows)]
pub(super) async fn request_bytes(
    pipe_path: &Path,
    method: &str,
    path: &str,
    body: Vec<u8>,
) -> Result<DockerResponse> {
    let mut pipe = open_named_pipe(pipe_path).await?;
    let len = body.len();
    let headers = format!(
        "{method} {path} HTTP/1.1\r\nHost: docker\r\nUser-Agent: {USER_AGENT}\r\nConnection: close\r\nContent-Length: {len}\r\n\r\n"
    );
    pipe.write_all(headers.as_bytes()).await?;
    pipe.write_all(&body).await?;
    pipe.flush().await?;
    read_response(&mut pipe).await
}

#[cfg(windows)]
pub(super) async fn request_file(
    pipe_path: &Path,
    method: &str,
    path: &str,
    content_type: &str,
    mut file: tokio::fs::File,
    len: u64,
) -> Result<DockerResponse> {
    let mut pipe = open_named_pipe(pipe_path).await?;
    let headers = format!(
        "{method} {path} HTTP/1.1\r\nHost: docker\r\nUser-Agent: {USER_AGENT}\r\nConnection: close\r\nContent-Type: {content_type}\r\nContent-Length: {len}\r\n\r\n"
    );
    pipe.write_all(headers.as_bytes()).await?;
    tokio::io::copy(&mut file, &mut pipe).await?;
    pipe.flush().await?;
    read_response(&mut pipe).await
}

#[cfg(windows)]
pub(super) async fn request_chunked_stream(
    pipe_path: &Path,
    method: &str,
    path: &str,
    content_type: &str,
    mut stream: ReaderStream<DuplexStream>,
) -> Result<DockerResponse> {
    let mut pipe = open_named_pipe(pipe_path).await?;
    let headers = format!(
        "{method} {path} HTTP/1.1\r\nHost: docker\r\nUser-Agent: {USER_AGENT}\r\nConnection: close\r\nContent-Type: {content_type}\r\nTransfer-Encoding: chunked\r\n\r\n"
    );
    pipe.write_all(headers.as_bytes()).await?;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        pipe.write_all(format!("{:x}\r\n", chunk.len()).as_bytes())
            .await?;
        pipe.write_all(&chunk).await?;
        pipe.write_all(b"\r\n").await?;
    }
    pipe.write_all(b"0\r\n\r\n").await?;
    pipe.flush().await?;
    read_response(&mut pipe).await
}

#[cfg(windows)]
pub(super) async fn request_to_file(
    pipe_path: &Path,
    method: &str,
    path: &str,
    output: &Path,
    action: &str,
) -> Result<()> {
    let mut pipe = open_named_pipe(pipe_path).await?;
    let headers = format!(
        "{method} {path} HTTP/1.1\r\nHost: docker\r\nUser-Agent: {USER_AGENT}\r\nConnection: close\r\nContent-Length: 0\r\n\r\n"
    );
    pipe.write_all(headers.as_bytes()).await?;
    pipe.flush().await?;

    let (status, headers, body_start) = read_response_head(&mut pipe).await?;
    if !status.is_success() {
        let body = read_body(&mut pipe, &headers, body_start).await?;
        return ensure_success_status(status, body, action);
    }

    let mut file = tokio::fs::File::create(output).await?;
    write_body_to_file(&mut pipe, &headers, body_start, &mut file).await?;
    file.flush().await?;
    Ok(())
}

#[cfg(windows)]
async fn open_named_pipe(
    pipe_path: &Path,
) -> Result<tokio::net::windows::named_pipe::NamedPipeClient> {
    for attempt in 0..=NAMED_PIPE_OPEN_RETRIES {
        match ClientOptions::new().open(pipe_path) {
            Ok(pipe) => return Ok(pipe),
            Err(error)
                if error.raw_os_error() == Some(ERROR_PIPE_BUSY)
                    && attempt < NAMED_PIPE_OPEN_RETRIES =>
            {
                tokio::time::sleep(NAMED_PIPE_OPEN_RETRY_DELAY).await;
            }
            Err(error) => return Err(error.into()),
        }
    }

    unreachable!("named pipe retry loop either returns or propagates the last open error")
}

#[cfg(windows)]
async fn read_response<R>(reader: &mut R) -> Result<DockerResponse>
where
    R: tokio::io::AsyncRead + Unpin,
{
    let (status, headers, body_start) = read_response_head(reader).await?;
    let body = read_body(reader, &headers, body_start).await?;
    Ok(DockerResponse { status, body })
}

#[cfg(windows)]
async fn read_response_head<R>(
    reader: &mut R,
) -> Result<(StatusCode, Vec<(String, String)>, Vec<u8>)>
where
    R: tokio::io::AsyncRead + Unpin,
{
    let mut buffer = Vec::new();
    let mut chunk = [0_u8; 8192];
    let header_end = loop {
        let read = reader.read(&mut chunk).await?;
        if read == 0 {
            return Err(DockerPullError::BadResponse(
                "docker API response ended before headers".into(),
            ));
        }
        buffer.extend_from_slice(&chunk[..read]);
        if let Some(index) = find_bytes(&buffer, b"\r\n\r\n") {
            break index + 4;
        }
        if buffer.len() > MAX_RESPONSE_HEAD_BYTES {
            return Err(DockerPullError::BadResponse(
                "docker API response headers are too large".into(),
            ));
        }
    };

    let (status, headers) = parse_response_head(&buffer[..header_end])?;
    Ok((status, headers, buffer[header_end..].to_vec()))
}

#[cfg(windows)]
async fn read_body<R>(
    reader: &mut R,
    headers: &[(String, String)],
    mut body: Vec<u8>,
) -> Result<Vec<u8>>
where
    R: tokio::io::AsyncRead + Unpin,
{
    reader.read_to_end(&mut body).await?;

    if header_value(headers, "transfer-encoding")
        .is_some_and(|value| value.to_ascii_lowercase().contains("chunked"))
    {
        body = decode_chunked_body(&body)?;
    } else if let Some(content_length) = header_value(headers, "content-length") {
        let content_length = content_length.parse::<usize>().map_err(|error| {
            DockerPullError::BadResponse(format!("invalid docker API content length: {error}"))
        })?;
        body.truncate(content_length);
    }

    Ok(body)
}

#[cfg(windows)]
async fn write_body_to_file<R>(
    reader: &mut R,
    headers: &[(String, String)],
    body_start: Vec<u8>,
    file: &mut tokio::fs::File,
) -> Result<()>
where
    R: tokio::io::AsyncRead + Unpin,
{
    if header_value(headers, "transfer-encoding")
        .is_some_and(|value| value.to_ascii_lowercase().contains("chunked"))
    {
        write_chunked_body_to_file(reader, body_start, file).await?;
        return Ok(());
    }

    if let Some(content_length) = header_value(headers, "content-length") {
        let content_length = content_length.parse::<usize>().map_err(|error| {
            DockerPullError::BadResponse(format!("invalid docker API content length: {error}"))
        })?;
        write_content_length_body_to_file(reader, body_start, file, content_length).await?;
        return Ok(());
    }

    file.write_all(&body_start).await?;
    let mut buffer = [0_u8; 8192];
    loop {
        let read = reader.read(&mut buffer).await?;
        if read == 0 {
            return Ok(());
        }
        file.write_all(&buffer[..read]).await?;
    }
}

#[cfg(windows)]
async fn write_content_length_body_to_file<R>(
    reader: &mut R,
    body_start: Vec<u8>,
    file: &mut tokio::fs::File,
    content_length: usize,
) -> Result<()>
where
    R: tokio::io::AsyncRead + Unpin,
{
    let initial_len = body_start.len().min(content_length);
    file.write_all(&body_start[..initial_len]).await?;
    let mut remaining = content_length - initial_len;
    let mut buffer = [0_u8; 8192];
    while remaining > 0 {
        let limit = remaining.min(buffer.len());
        let read = reader.read(&mut buffer[..limit]).await?;
        if read == 0 {
            return Err(DockerPullError::BadResponse(
                "docker API response ended before content length".into(),
            ));
        }
        file.write_all(&buffer[..read]).await?;
        remaining -= read;
    }
    Ok(())
}

#[cfg(any(test, windows))]
async fn write_chunked_body_to_file<R>(
    reader: &mut R,
    mut buffer: Vec<u8>,
    file: &mut tokio::fs::File,
) -> Result<()>
where
    R: tokio::io::AsyncRead + Unpin,
{
    loop {
        let size = read_chunk_size(reader, &mut buffer).await?;
        if size == 0 {
            return Ok(());
        }

        write_chunk_data(reader, &mut buffer, file, size).await?;
        consume_chunk_crlf(reader, &mut buffer).await?;
    }
}

#[cfg(any(test, windows))]
async fn read_chunk_size<R>(reader: &mut R, buffer: &mut Vec<u8>) -> Result<usize>
where
    R: tokio::io::AsyncRead + Unpin,
{
    loop {
        if let Some(line_end) = find_bytes(buffer, b"\r\n") {
            let line = buffer.drain(..line_end + 2).collect::<Vec<_>>();
            let size_line = std::str::from_utf8(&line[..line_end]).map_err(|error| {
                DockerPullError::BadResponse(format!("invalid chunk size: {error}"))
            })?;
            let size_hex = size_line
                .split_once(';')
                .map(|(size, _)| size)
                .unwrap_or(size_line);
            return usize::from_str_radix(size_hex.trim(), 16).map_err(|error| {
                DockerPullError::BadResponse(format!("invalid chunk size: {error}"))
            });
        }
        read_more(reader, buffer).await?;
    }
}

#[cfg(any(test, windows))]
async fn write_chunk_data<R>(
    reader: &mut R,
    buffer: &mut Vec<u8>,
    file: &mut tokio::fs::File,
    mut remaining: usize,
) -> Result<()>
where
    R: tokio::io::AsyncRead + Unpin,
{
    while remaining > 0 {
        if buffer.is_empty() {
            read_more(reader, buffer).await?;
        }

        let count = remaining.min(buffer.len());
        file.write_all(&buffer[..count]).await?;
        buffer.drain(..count);
        remaining -= count;
    }
    Ok(())
}

#[cfg(any(test, windows))]
async fn consume_chunk_crlf<R>(reader: &mut R, buffer: &mut Vec<u8>) -> Result<()>
where
    R: tokio::io::AsyncRead + Unpin,
{
    while buffer.len() < 2 {
        read_more(reader, buffer).await?;
    }
    if &buffer[..2] != b"\r\n" {
        return Err(DockerPullError::BadResponse(
            "chunked docker API response is missing chunk terminator".into(),
        ));
    }
    buffer.drain(..2);
    Ok(())
}

#[cfg(any(test, windows))]
async fn read_more<R>(reader: &mut R, buffer: &mut Vec<u8>) -> Result<()>
where
    R: tokio::io::AsyncRead + Unpin,
{
    let mut chunk = [0_u8; 8192];
    let read = reader.read(&mut chunk).await?;
    if read == 0 {
        return Err(DockerPullError::BadResponse(
            "chunked docker API response is truncated".into(),
        ));
    }
    buffer.extend_from_slice(&chunk[..read]);
    Ok(())
}

#[cfg(any(test, windows))]
pub(crate) fn parse_response_head(bytes: &[u8]) -> Result<(StatusCode, Vec<(String, String)>)> {
    let mut headers = [httparse::EMPTY_HEADER; 128];
    let mut response = httparse::Response::new(&mut headers);
    let parsed = response.parse(bytes).map_err(|error| {
        DockerPullError::BadResponse(format!("invalid docker API headers: {error}"))
    })?;
    if parsed.is_partial() {
        return Err(DockerPullError::BadResponse(
            "docker API response headers are incomplete".into(),
        ));
    }

    let status = response.code.ok_or_else(|| {
        DockerPullError::BadResponse("docker API response is missing status".into())
    })?;
    let status = StatusCode::from_u16(status).map_err(|error| {
        DockerPullError::BadResponse(format!("invalid docker API status: {error}"))
    })?;

    let headers = response
        .headers
        .iter()
        .map(|header| {
            let value = std::str::from_utf8(header.value).map_err(|error| {
                DockerPullError::BadResponse(format!("invalid docker API header value: {error}"))
            })?;
            Ok((header.name.to_ascii_lowercase(), value.trim().to_string()))
        })
        .collect::<Result<Vec<_>>>()?;
    Ok((status, headers))
}

#[cfg(any(test, windows))]
pub(crate) fn header_value<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(header_name, _)| header_name == name)
        .map(|(_, value)| value.as_str())
}

#[cfg(any(test, windows))]
fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

#[cfg(any(test, windows))]
pub(crate) fn decode_chunked_body(bytes: &[u8]) -> Result<Vec<u8>> {
    let mut decoded = Vec::new();
    let mut index = 0;
    loop {
        let Some(line_end) = find_bytes(&bytes[index..], b"\r\n") else {
            return Err(DockerPullError::BadResponse(
                "chunked docker API response is missing chunk size".into(),
            ));
        };
        let size_line = std::str::from_utf8(&bytes[index..index + line_end]).map_err(|error| {
            DockerPullError::BadResponse(format!("invalid chunk size: {error}"))
        })?;
        let size_hex = size_line
            .split_once(';')
            .map(|(size, _)| size)
            .unwrap_or(size_line);
        let size = usize::from_str_radix(size_hex.trim(), 16).map_err(|error| {
            DockerPullError::BadResponse(format!("invalid chunk size: {error}"))
        })?;
        index += line_end + 2;
        if size == 0 {
            return Ok(decoded);
        }
        let chunk_end = index + size;
        if bytes.len() < chunk_end + 2 || &bytes[chunk_end..chunk_end + 2] != b"\r\n" {
            return Err(DockerPullError::BadResponse(
                "chunked docker API response is truncated".into(),
            ));
        }
        decoded.extend_from_slice(&bytes[index..chunk_end]);
        index = chunk_end + 2;
    }
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;
    use tokio::io::AsyncWriteExt;

    use super::{decode_chunked_body, write_chunked_body_to_file};

    #[tokio::test]
    async fn chunked_body_is_streamed_to_file() {
        let dir = tempdir().expect("tempdir should create");
        let path = dir.path().join("body.bin");
        let mut file = tokio::fs::File::create(&path)
            .await
            .expect("file should create");
        let (mut writer, mut reader) = tokio::io::duplex(64);

        tokio::spawn(async move {
            writer
                .write_all(b"ck\r\n2\r\ner\r\n0\r\n\r\n")
                .await
                .expect("chunked body should write");
        });

        write_chunked_body_to_file(&mut reader, b"4\r\npo".to_vec(), &mut file)
            .await
            .expect("chunked body should stream");
        file.flush().await.expect("file should flush");
        drop(file);

        assert_eq!(std::fs::read(path).expect("file should read"), b"pocker");
    }

    #[test]
    fn zero_length_chunk_ends_decoding() {
        let body = decode_chunked_body(b"0\r\n\r\nignored")
            .expect("zero-length chunk should finish the body");

        assert!(body.is_empty());
    }
}
