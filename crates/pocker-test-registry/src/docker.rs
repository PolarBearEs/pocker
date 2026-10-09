//! Minimal Docker Engine API fixture for exercising pocker's daemon paths.
//!
//! Point pocker at it with `DOCKER_HOST=tcp://<address>`. It serves image
//! listing, inspect, `docker save`, and `docker load`, records every load
//! body, and logs requests that match no route.

use std::io;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::Mutex;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;

const REQUEST_HEAD_LIMIT: usize = 16 * 1024;

/// One image known to [`FakeDockerDaemon`].
#[derive(Debug, Clone)]
pub struct FakeDaemonImage {
    /// Image ID reported by `GET /images/json`.
    pub id: String,
    /// Extra names (tags) the image can be inspected by, besides its ID.
    pub names: Vec<String>,
    /// Raw JSON body returned by `GET /images/{name}/json`.
    pub inspect_json: String,
    /// Body returned by `GET /images/{id}/get`; `None` answers with a 500.
    pub save_archive: Option<Vec<u8>>,
}

/// Loopback Docker Engine API fixture.
#[derive(Debug)]
pub struct FakeDockerDaemon {
    address: SocketAddr,
    state: Arc<DaemonState>,
    task: JoinHandle<()>,
}

#[derive(Debug)]
struct DaemonState {
    images: Vec<FakeDaemonImage>,
    loads: Mutex<Vec<Vec<u8>>>,
    saves: Mutex<Vec<String>>,
    unexpected_requests: Mutex<Vec<String>>,
}

impl FakeDockerDaemon {
    /// Starts a daemon that knows exactly `images`.
    pub async fn start(images: Vec<FakeDaemonImage>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("fake docker daemon should bind");
        let address = listener
            .local_addr()
            .expect("fake docker daemon should expose its address");
        let state = Arc::new(DaemonState {
            images,
            loads: Mutex::new(Vec::new()),
            saves: Mutex::new(Vec::new()),
            unexpected_requests: Mutex::new(Vec::new()),
        });
        let server_state = Arc::clone(&state);
        let task = tokio::spawn(async move {
            while let Ok((stream, _peer)) = listener.accept().await {
                let state = Arc::clone(&server_state);
                tokio::spawn(async move {
                    let _ = handle_connection(stream, state).await;
                });
            }
        });
        Self {
            address,
            state,
            task,
        }
    }

    /// Returns the `DOCKER_HOST` value pointing at this daemon.
    pub fn docker_host(&self) -> String {
        format!("tcp://{}", self.address)
    }

    /// Returns every archive body received by `POST /images/load`.
    pub fn loads(&self) -> Vec<Vec<u8>> {
        self.state.loads.lock().expect("load log poisoned").clone()
    }

    /// Returns the image names requested through `docker save`, in order.
    pub fn saves(&self) -> Vec<String> {
        self.state.saves.lock().expect("save log poisoned").clone()
    }

    /// Returns requests that did not match a fixture route.
    pub fn unexpected_requests(&self) -> Vec<String> {
        self.state
            .unexpected_requests
            .lock()
            .expect("unexpected request log poisoned")
            .clone()
    }
}

impl Drop for FakeDockerDaemon {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl DaemonState {
    fn find(&self, name: &str) -> Option<&FakeDaemonImage> {
        self.images
            .iter()
            .find(|image| image.id == name || image.names.iter().any(|candidate| candidate == name))
    }
}

async fn handle_connection(mut stream: TcpStream, state: Arc<DaemonState>) -> io::Result<()> {
    let request = read_request(&mut stream).await?;
    let path = request.path.split('?').next().unwrap_or_default();
    let path = percent_decode(path);

    if request.method == "GET" && path == "/images/json" {
        let body = format!(
            "[{}]",
            state
                .images
                .iter()
                .map(|image| format!(r#"{{"Id":"{}","RepoTags":[]}}"#, image.id))
                .collect::<Vec<_>>()
                .join(",")
        );
        return write_response(&mut stream, "200 OK", body.as_bytes()).await;
    }
    if request.method == "POST" && path == "/images/load" {
        state
            .loads
            .lock()
            .expect("load log poisoned")
            .push(request.body);
        return write_response(&mut stream, "200 OK", br#"{"stream":"Loaded image"}"#).await;
    }
    if let Some(name) = path
        .strip_prefix("/images/")
        .and_then(|rest| rest.strip_suffix("/json"))
        && request.method == "GET"
    {
        return match state.find(name) {
            Some(image) => {
                write_response(&mut stream, "200 OK", image.inspect_json.as_bytes()).await
            }
            None => {
                write_response(
                    &mut stream,
                    "404 Not Found",
                    br#"{"message":"No such image"}"#,
                )
                .await
            }
        };
    }
    if let Some(name) = path
        .strip_prefix("/images/")
        .and_then(|rest| rest.strip_suffix("/get"))
        && request.method == "GET"
    {
        state
            .saves
            .lock()
            .expect("save log poisoned")
            .push(name.to_string());
        return match state
            .find(name)
            .and_then(|image| image.save_archive.as_ref())
        {
            Some(archive) => write_response(&mut stream, "200 OK", archive).await,
            None => {
                write_response(
                    &mut stream,
                    "500 Internal Server Error",
                    br#"{"message":"content digest not found"}"#,
                )
                .await
            }
        };
    }

    state
        .unexpected_requests
        .lock()
        .expect("unexpected request log poisoned")
        .push(format!("{} {}", request.method, request.path));
    write_response(&mut stream, "404 Not Found", br#"{"message":"not found"}"#).await
}

struct Request {
    method: String,
    path: String,
    body: Vec<u8>,
}

async fn read_request(stream: &mut TcpStream) -> io::Result<Request> {
    let mut buffer = Vec::with_capacity(2 * 1024);
    let mut chunk = [0_u8; 8 * 1024];
    let head_end = loop {
        if let Some(position) = buffer.windows(4).position(|window| window == b"\r\n\r\n") {
            break position + 4;
        }
        if buffer.len() > REQUEST_HEAD_LIMIT {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "request headers exceeded fixture limit",
            ));
        }
        let read = stream.read(&mut chunk).await?;
        if read == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "request ended before its headers",
            ));
        }
        buffer.extend_from_slice(&chunk[..read]);
    };

    let head = std::str::from_utf8(&buffer[..head_end])
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?
        .to_string();
    let mut lines = head.split("\r\n");
    let mut tokens = lines.next().unwrap_or_default().split_whitespace();
    let method = tokens.next().unwrap_or_default().to_string();
    let path = tokens.next().unwrap_or_default().to_string();
    let mut content_length = None;
    let mut chunked = false;
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim();
        if name.eq_ignore_ascii_case("content-length") {
            content_length = value.parse::<usize>().ok();
        } else if name.eq_ignore_ascii_case("transfer-encoding")
            && value.eq_ignore_ascii_case("chunked")
        {
            chunked = true;
        }
    }

    let mut reader = BufferedReader {
        stream,
        pending: buffer[head_end..].to_vec(),
    };
    let body = if chunked {
        read_chunked_body(&mut reader).await?
    } else {
        reader.read_exact(content_length.unwrap_or(0)).await?
    };
    Ok(Request { method, path, body })
}

struct BufferedReader<'a> {
    stream: &'a mut TcpStream,
    pending: Vec<u8>,
}

impl BufferedReader<'_> {
    async fn fill(&mut self) -> io::Result<()> {
        let mut chunk = [0_u8; 8 * 1024];
        let read = self.stream.read(&mut chunk).await?;
        if read == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "request body ended early",
            ));
        }
        self.pending.extend_from_slice(&chunk[..read]);
        Ok(())
    }

    async fn read_exact(&mut self, len: usize) -> io::Result<Vec<u8>> {
        while self.pending.len() < len {
            self.fill().await?;
        }
        Ok(self.pending.drain(..len).collect())
    }

    async fn read_line(&mut self) -> io::Result<String> {
        loop {
            if let Some(position) = self.pending.windows(2).position(|window| window == b"\r\n") {
                let line = self.pending.drain(..position + 2).collect::<Vec<_>>();
                return String::from_utf8(line[..position].to_vec())
                    .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error));
            }
            self.fill().await?;
        }
    }
}

async fn read_chunked_body(reader: &mut BufferedReader<'_>) -> io::Result<Vec<u8>> {
    let mut body = Vec::new();
    loop {
        let line = reader.read_line().await?;
        let size_text = line.split(';').next().unwrap_or_default().trim();
        let size = usize::from_str_radix(size_text, 16)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        if size == 0 {
            // Skip optional trailers up to the terminating blank line.
            while !reader.read_line().await?.is_empty() {}
            return Ok(body);
        }
        body.extend(reader.read_exact(size).await?);
        reader.read_exact(2).await?;
    }
}

async fn write_response(stream: &mut TcpStream, status: &str, body: &[u8]) -> io::Result<()> {
    let head = format!(
        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes()).await?;
    stream.write_all(body).await?;
    stream.shutdown().await.ok();
    Ok(())
}

fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%'
            && index + 2 < bytes.len()
            && let Ok(byte) = u8::from_str_radix(&value[index + 1..index + 3], 16)
        {
            decoded.push(byte);
            index += 3;
            continue;
        }
        decoded.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&decoded).into_owned()
}
