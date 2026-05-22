use std::path::Path;
use std::time::Duration;

use reqwest::{Certificate, Client, ClientBuilder};

use crate::error::Result;

pub(crate) const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);
pub(crate) const DEFAULT_READ_TIMEOUT: Duration = Duration::from_secs(60);
pub(crate) const USER_AGENT: &str = concat!("pocker/", env!("CARGO_PKG_VERSION"));

pub fn build_http_client(
    plain_http: bool,
    insecure_skip_tls_verify: bool,
    ca_file: Option<&Path>,
) -> Result<Client> {
    build_http_client_with_timeouts(
        plain_http,
        insecure_skip_tls_verify,
        ca_file,
        None,
        Some(DEFAULT_READ_TIMEOUT),
    )
}

fn build_http_client_with_timeouts(
    plain_http: bool,
    insecure_skip_tls_verify: bool,
    ca_file: Option<&Path>,
    timeout: Option<Duration>,
    read_timeout: Option<Duration>,
) -> Result<Client> {
    let mut builder =
        http_client_builder(plain_http, insecure_skip_tls_verify, timeout, read_timeout)
            .user_agent(USER_AGENT);

    if let Some(path) = ca_file {
        let pem = std::fs::read(path)?;
        builder = builder.add_root_certificate(Certificate::from_pem(&pem)?);
    }

    Ok(builder.build()?)
}

fn http_client_builder(
    plain_http: bool,
    insecure_skip_tls_verify: bool,
    timeout: Option<Duration>,
    read_timeout: Option<Duration>,
) -> ClientBuilder {
    let mut builder = Client::builder()
        .connect_timeout(CONNECT_TIMEOUT)
        .danger_accept_invalid_certs(insecure_skip_tls_verify)
        .redirect(reqwest::redirect::Policy::limited(10));

    if let Some(timeout) = timeout {
        builder = builder.timeout(timeout);
    }
    if let Some(read_timeout) = read_timeout {
        builder = builder.read_timeout(read_timeout);
    }

    if plain_http {
        builder = builder.https_only(false);
    }

    builder
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;
    use tokio::time::{sleep, timeout};

    use super::build_http_client_with_timeouts;

    #[tokio::test]
    async fn client_without_total_timeout_allows_slow_response_headers() {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("listener should bind");
        let address = listener.local_addr().expect("listener address");
        let requests = Arc::new(AtomicUsize::new(0));
        let requests_task = Arc::clone(&requests);

        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("connection should arrive");
            requests_task.fetch_add(1, Ordering::SeqCst);
            let mut buffer = [0_u8; 1024];
            let _ = stream.read(&mut buffer).await;
            sleep(Duration::from_millis(300)).await;
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok")
                .await
                .expect("response should be written");
        });

        let client = build_http_client_with_timeouts(true, false, None, None, None)
            .expect("client should build");
        let response = timeout(
            Duration::from_secs(2),
            client.get(format!("http://{address}/slow")).send(),
        )
        .await
        .expect("request should finish before test timeout")
        .expect("request should succeed");
        assert_eq!(response.status(), reqwest::StatusCode::OK);
        assert_eq!(requests.load(Ordering::SeqCst), 1);
        server.await.expect("server task should finish");
    }

    #[tokio::test]
    async fn client_with_total_timeout_fails_slow_response_headers() {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("listener should bind");
        let address = listener.local_addr().expect("listener address");

        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("connection should arrive");
            let mut buffer = [0_u8; 1024];
            let _ = stream.read(&mut buffer).await;
            sleep(Duration::from_millis(300)).await;
            let _ = stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok")
                .await;
        });

        let client = build_http_client_with_timeouts(
            true,
            false,
            None,
            Some(Duration::from_millis(100)),
            None,
        )
        .expect("client should build");
        let error = client
            .get(format!("http://{address}/slow"))
            .send()
            .await
            .expect_err("request should time out");
        assert!(error.is_timeout());
        server.await.expect("server task should finish");
    }

    #[tokio::test]
    async fn client_with_read_timeout_fails_stalled_response_headers() {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("listener should bind");
        let address = listener.local_addr().expect("listener address");

        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("connection should arrive");
            let mut buffer = [0_u8; 1024];
            let _ = stream.read(&mut buffer).await;
            sleep(Duration::from_millis(300)).await;
            let _ = stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok")
                .await;
        });

        let client = build_http_client_with_timeouts(
            true,
            false,
            None,
            None,
            Some(Duration::from_millis(100)),
        )
        .expect("client should build");
        let error = client
            .get(format!("http://{address}/slow"))
            .send()
            .await
            .expect_err("request should time out while waiting for response headers");
        assert!(error.is_timeout());
        server.await.expect("server task should finish");
    }
}
