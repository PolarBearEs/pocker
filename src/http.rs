use std::path::Path;
use std::time::Duration;

use reqwest::{Certificate, Client, ClientBuilder};

use crate::error::Result;

pub(crate) const USER_AGENT: &str = concat!("pocker/", env!("CARGO_PKG_VERSION"));
pub(crate) const DEFAULT_CONNECT_TIMEOUT_SECONDS: i64 = 20;

pub fn build_http_client(
    plain_http: bool,
    insecure_skip_tls_verify: bool,
    ca_file: Option<&Path>,
) -> Result<Client> {
    build_http_client_with_connect_timeout(
        plain_http,
        insecure_skip_tls_verify,
        ca_file,
        default_connect_timeout(),
    )
}

pub(crate) fn build_http_client_with_connect_timeout(
    plain_http: bool,
    insecure_skip_tls_verify: bool,
    ca_file: Option<&Path>,
    connect_timeout: Option<Duration>,
) -> Result<Client> {
    build_http_client_with_ca(
        plain_http,
        insecure_skip_tls_verify,
        ca_file,
        connect_timeout,
    )
}

fn build_http_client_with_ca(
    plain_http: bool,
    insecure_skip_tls_verify: bool,
    ca_file: Option<&Path>,
    connect_timeout: Option<Duration>,
) -> Result<Client> {
    let mut builder = http_client_builder(plain_http, insecure_skip_tls_verify, connect_timeout)
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
    connect_timeout: Option<Duration>,
) -> ClientBuilder {
    let mut builder = Client::builder()
        .danger_accept_invalid_certs(insecure_skip_tls_verify)
        .redirect(reqwest::redirect::Policy::limited(10));

    if let Some(connect_timeout) = connect_timeout {
        builder = builder.connect_timeout(connect_timeout);
    }

    if plain_http {
        builder = builder.https_only(false);
    }

    builder
}

pub(crate) fn connect_timeout_from_seconds(seconds: i64) -> Result<Option<Duration>> {
    match seconds {
        -1 => Ok(None),
        seconds if seconds >= 0 => Ok(Some(Duration::from_secs(seconds as u64))),
        _ => Err(crate::error::DockerPullError::InvalidInput(
            "connect timeout must be -1 or a non-negative number of seconds".into(),
        )),
    }
}

pub(crate) fn default_connect_timeout() -> Option<Duration> {
    Some(Duration::from_secs(DEFAULT_CONNECT_TIMEOUT_SECONDS as u64))
}

pub(crate) fn parse_connect_timeout_seconds(value: &str) -> std::result::Result<i64, String> {
    let seconds = value
        .parse::<i64>()
        .map_err(|error| format!("invalid connect timeout: {error}"))?;
    if seconds < -1 {
        return Err("connect timeout must be -1 or a non-negative number of seconds".into());
    }
    Ok(seconds)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;
    use tokio::time::{sleep, timeout};

    use super::build_http_client_with_ca;

    #[tokio::test]
    async fn client_allows_slow_response_headers() {
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

        let client =
            build_http_client_with_ca(true, false, None, None).expect("client should build");
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
}
