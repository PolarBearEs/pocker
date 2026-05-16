use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use reqwest::header::{ACCEPT, HeaderValue, RANGE, RETRY_AFTER, WWW_AUTHENTICATE};
use reqwest::{Client, Method, Response, StatusCode};
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;
use tokio::time::sleep;
use tracing::{debug, warn};
use url::Url;

use crate::auth::{AuthResolver, Credentials};
use crate::error::{DockerPullError, Result};
use crate::platform::Platform;
use crate::reference::ImageReference;

const MANIFEST_ACCEPT: &str = concat!(
    "application/vnd.oci.image.index.v1+json,",
    "application/vnd.docker.distribution.manifest.list.v2+json,",
    "application/vnd.oci.image.manifest.v1+json,",
    "application/vnd.docker.distribution.manifest.v2+json"
);
pub const DEFAULT_REQUEST_RETRIES: u32 = 5;
const MAX_AUTH_RETRIES: u32 = 2;

#[derive(Debug, Clone)]
pub struct RegistryClient {
    client: Client,
    auth: Arc<AuthResolver>,
    token_cache: Arc<Mutex<HashMap<String, String>>>,
    plain_http: bool,
    request_retry_limit: Option<u32>,
    cache_from: Option<Url>,
    cache_only: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Descriptor {
    #[serde(rename = "mediaType", default)]
    pub media_type: String,
    pub digest: String,
    pub size: i64,
    #[serde(default)]
    pub platform: Option<Platform>,
    #[serde(default)]
    pub annotations: Option<HashMap<String, String>>,
}

#[derive(Debug, Deserialize)]
struct ManifestEnvelope {
    #[serde(rename = "schemaVersion")]
    schema_version: u32,
    #[serde(rename = "mediaType", default)]
    media_type: String,
}

#[derive(Debug, Deserialize)]
struct ImageManifest {
    #[serde(rename = "schemaVersion")]
    _schema_version: u32,
    #[serde(rename = "mediaType", default)]
    media_type: String,
    config: Descriptor,
    layers: Vec<Descriptor>,
}

#[derive(Debug, Deserialize)]
struct ImageIndex {
    #[serde(rename = "schemaVersion")]
    _schema_version: u32,
    #[serde(rename = "mediaType", default)]
    _media_type: String,
    manifests: Vec<Descriptor>,
}

#[derive(Debug, Clone)]
pub struct ResolvedImage {
    pub manifest: Descriptor,
    pub manifest_bytes: Vec<u8>,
    pub config: Descriptor,
    pub layers: Vec<Descriptor>,
}

#[derive(Debug)]
pub struct BlobMetadata {
    pub size: Option<u64>,
}

struct RegistryRequest<'a> {
    method: Method,
    url: Url,
    fallback_url: Option<Url>,
    accept: Option<&'a str>,
    range: Option<&'a str>,
    allow_retry: bool,
}

#[derive(Debug, Clone)]
pub struct RawManifest {
    pub descriptor: Descriptor,
    pub bytes: Vec<u8>,
}

impl RegistryClient {
    pub fn new(
        client: Client,
        auth: Arc<AuthResolver>,
        plain_http: bool,
        request_retry_limit: Option<u32>,
    ) -> Self {
        Self::new_with_cache_from(client, auth, plain_http, request_retry_limit, None, false)
    }

    pub fn new_with_cache_from(
        client: Client,
        auth: Arc<AuthResolver>,
        plain_http: bool,
        request_retry_limit: Option<u32>,
        cache_from: Option<Url>,
        cache_only: bool,
    ) -> Self {
        Self {
            client,
            auth,
            token_cache: Arc::new(Mutex::new(HashMap::new())),
            plain_http,
            request_retry_limit,
            cache_from,
            cache_only,
        }
    }

    pub async fn get_manifest_raw(
        &self,
        reference: &ImageReference,
        accept: Option<&str>,
    ) -> Result<RawManifest> {
        let response = self
            .send(
                RegistryRequest {
                    method: Method::GET,
                    url: self.manifest_url(reference)?,
                    fallback_url: self.fallback_manifest_url(reference)?,
                    accept,
                    range: None,
                    allow_retry: true,
                },
                reference,
            )
            .await?;
        raw_manifest_from_response(response).await
    }

    pub async fn get_manifest_digest_raw(
        &self,
        reference: &ImageReference,
        digest: &str,
        accept: Option<&str>,
    ) -> Result<RawManifest> {
        let response = self
            .send(
                RegistryRequest {
                    method: Method::GET,
                    url: self.manifest_digest_url(reference, digest)?,
                    fallback_url: self.fallback_manifest_digest_url(reference, digest)?,
                    accept,
                    range: None,
                    allow_retry: true,
                },
                reference,
            )
            .await?;
        raw_manifest_from_response(response).await
    }

    pub async fn resolve_image(
        &self,
        reference: &ImageReference,
        platform: &Platform,
    ) -> Result<ResolvedImage> {
        let url = self.manifest_url(reference)?;
        let response = self
            .send(
                RegistryRequest {
                    method: Method::GET,
                    url,
                    fallback_url: self.fallback_manifest_url(reference)?,
                    accept: Some(MANIFEST_ACCEPT),
                    range: None,
                    allow_retry: true,
                },
                reference,
            )
            .await?;
        if response.status() == StatusCode::NOT_FOUND {
            return Err(DockerPullError::ManifestNotFound);
        }
        let digest = header_string(&response, "docker-content-digest")?;
        let media_type = response_content_media_type(&response);
        let body = response.bytes().await?.to_vec();
        let envelope: ManifestEnvelope = serde_json::from_slice(&body)?;
        if envelope.schema_version != 2 {
            return Err(DockerPullError::BadResponse(format!(
                "unsupported schemaVersion {}",
                envelope.schema_version
            )));
        }

        let media_type = if media_type.is_empty() {
            envelope.media_type
        } else {
            media_type
        };

        if is_image_manifest(&media_type) {
            let manifest: ImageManifest = serde_json::from_slice(&body)?;
            let manifest_descriptor = Descriptor {
                media_type: manifest.media_type.clone(),
                digest: digest.unwrap_or_else(|| digest_bytes(&body)),
                size: body.len() as i64,
                platform: Some(platform.clone()),
                annotations: None,
            };
            return Ok(ResolvedImage {
                manifest: manifest_descriptor,
                manifest_bytes: body,
                config: manifest.config,
                layers: manifest.layers,
            });
        }

        if is_image_index(&media_type) {
            let index: ImageIndex = serde_json::from_slice(&body)?;
            let descriptor = index
                .manifests
                .into_iter()
                .find(|descriptor| {
                    descriptor
                        .platform
                        .as_ref()
                        .is_some_and(|candidate| platform.matches(candidate))
                })
                .ok_or_else(|| DockerPullError::PlatformNotFound(platform.as_string()))?;
            let manifest_url = self.manifest_digest_url(reference, &descriptor.digest)?;
            let response = self
                .send(
                    RegistryRequest {
                        method: Method::GET,
                        url: manifest_url,
                        fallback_url: self
                            .fallback_manifest_digest_url(reference, &descriptor.digest)?,
                        accept: Some(MANIFEST_ACCEPT),
                        range: None,
                        allow_retry: true,
                    },
                    reference,
                )
                .await?;
            if response.status() == StatusCode::NOT_FOUND {
                return Err(DockerPullError::ManifestNotFound);
            }
            if !response.status().is_success() {
                return Err(DockerPullError::BadResponse(format!(
                    "registry returned {} for manifest {}",
                    response.status(),
                    descriptor.digest
                )));
            }
            let body = response.bytes().await?.to_vec();
            let manifest: ImageManifest = serde_json::from_slice(&body)?;
            return Ok(ResolvedImage {
                manifest: Descriptor {
                    media_type: if descriptor.media_type.is_empty() {
                        manifest.media_type.clone()
                    } else {
                        descriptor.media_type
                    },
                    digest: descriptor.digest,
                    size: if descriptor.size == 0 {
                        body.len() as i64
                    } else {
                        descriptor.size
                    },
                    platform: descriptor.platform.clone(),
                    annotations: descriptor.annotations.clone(),
                },
                manifest_bytes: body,
                config: manifest.config,
                layers: manifest.layers,
            });
        }

        Err(DockerPullError::UnsupportedMediaType(media_type))
    }

    pub async fn head_blob(
        &self,
        reference: &ImageReference,
        digest: &str,
    ) -> Result<BlobMetadata> {
        let response = self
            .send(
                RegistryRequest {
                    method: Method::HEAD,
                    url: self.blob_url(reference, digest)?,
                    fallback_url: self.fallback_blob_url(reference, digest)?,
                    accept: None,
                    range: None,
                    allow_retry: true,
                },
                reference,
            )
            .await?;
        if response.status() == StatusCode::NOT_FOUND {
            return Err(DockerPullError::BlobNotFound(digest.to_string()));
        }
        let size = response
            .headers()
            .get(reqwest::header::CONTENT_LENGTH)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse::<u64>().ok());
        Ok(BlobMetadata { size })
    }

    pub async fn get_blob(
        &self,
        reference: &ImageReference,
        digest: &str,
        offset: u64,
    ) -> Result<Response> {
        let range = (offset > 0).then(|| format!("bytes={offset}-"));
        self.send(
            RegistryRequest {
                method: Method::GET,
                url: self.blob_url(reference, digest)?,
                fallback_url: self.fallback_blob_url(reference, digest)?,
                accept: None,
                range: range.as_deref(),
                allow_retry: true,
            },
            reference,
        )
        .await
    }

    pub async fn get_blob_bytes(
        &self,
        reference: &ImageReference,
        digest: &str,
    ) -> Result<Vec<u8>> {
        let response = self.get_blob(reference, digest, 0).await?;
        if response.status() == StatusCode::NOT_FOUND {
            return Err(DockerPullError::BlobNotFound(digest.to_string()));
        }
        Ok(response.bytes().await?.to_vec())
    }

    fn manifest_url(&self, reference: &ImageReference) -> Result<Url> {
        if let Some(cache_from) = &self.cache_from {
            return cache_url(
                cache_from,
                reference,
                "manifests",
                reference.manifest_reference(),
            );
        }
        self.direct_manifest_url(reference)
    }

    fn fallback_manifest_url(&self, reference: &ImageReference) -> Result<Option<Url>> {
        if self.uses_cache_from() && !self.cache_only {
            return self.direct_manifest_url(reference).map(Some);
        }
        Ok(None)
    }

    fn direct_manifest_url(&self, reference: &ImageReference) -> Result<Url> {
        self.direct_resource_url(reference, "manifests", reference.manifest_reference())
    }

    fn manifest_digest_url(&self, reference: &ImageReference, digest: &str) -> Result<Url> {
        if let Some(cache_from) = &self.cache_from {
            return cache_url(cache_from, reference, "manifests", digest);
        }
        self.direct_manifest_digest_url(reference, digest)
    }

    fn fallback_manifest_digest_url(
        &self,
        reference: &ImageReference,
        digest: &str,
    ) -> Result<Option<Url>> {
        if self.uses_cache_from() && !self.cache_only {
            return self.direct_manifest_digest_url(reference, digest).map(Some);
        }
        Ok(None)
    }

    fn direct_manifest_digest_url(&self, reference: &ImageReference, digest: &str) -> Result<Url> {
        self.direct_resource_url(reference, "manifests", digest)
    }

    fn blob_url(&self, reference: &ImageReference, digest: &str) -> Result<Url> {
        if let Some(cache_from) = &self.cache_from {
            return cache_url(cache_from, reference, "blobs", digest);
        }
        self.direct_blob_url(reference, digest)
    }

    fn fallback_blob_url(&self, reference: &ImageReference, digest: &str) -> Result<Option<Url>> {
        if self.uses_cache_from() && !self.cache_only {
            return self.direct_blob_url(reference, digest).map(Some);
        }
        Ok(None)
    }

    fn direct_blob_url(&self, reference: &ImageReference, digest: &str) -> Result<Url> {
        self.direct_resource_url(reference, "blobs", digest)
    }

    fn direct_resource_url(
        &self,
        reference: &ImageReference,
        resource: &str,
        suffix: &str,
    ) -> Result<Url> {
        Url::parse(&format!(
            "{}://{}/v2/{}/{resource}/{suffix}",
            self.scheme(),
            reference.registry,
            reference.repository
        ))
        .map_err(Into::into)
    }

    fn scheme(&self) -> &'static str {
        if self.plain_http { "http" } else { "https" }
    }

    async fn send(
        &self,
        request: RegistryRequest<'_>,
        reference: &ImageReference,
    ) -> Result<Response> {
        let response = self
            .send_to_url(
                &request,
                request.url.clone(),
                reference,
                self.uses_cache_from(),
            )
            .await?;
        if response.status() == StatusCode::NOT_FOUND
            && let Some(fallback_url) = request.fallback_url.clone()
        {
            return self
                .send_to_url(&request, fallback_url, reference, false)
                .await;
        }
        Ok(response)
    }

    async fn send_to_url(
        &self,
        request: &RegistryRequest<'_>,
        url: Url,
        reference: &ImageReference,
        cache_request: bool,
    ) -> Result<Response> {
        let cache_key = token_cache_key(reference);
        let mut retries = 0_u32;
        let mut auth_retries = 0_u32;
        loop {
            let token = if cache_request {
                None
            } else {
                self.token_cache.lock().await.get(&cache_key).cloned()
            };
            let credentials = if cache_request {
                None
            } else {
                self.auth.resolve(&reference.registry)?
            };
            let mut builder = self.client.request(request.method.clone(), url.clone());
            if let Some(accept) = request.accept {
                builder = builder.header(ACCEPT, accept);
            }
            if let Some(range) = request.range {
                builder = builder.header(RANGE, range);
            }
            if let Some(token) = &token {
                builder = builder.bearer_auth(token);
            } else if let Some(Credentials::Basic { username, password }) = &credentials {
                builder = builder.basic_auth(username, Some(password));
            }

            let response = match builder.send().await {
                Ok(response) => response,
                Err(error) if request.allow_retry && is_retryable_http_error(&error) => {
                    let detail = error.to_string();
                    if request_retry_limit_exhausted(retries, self.request_retry_limit) {
                        return Err(retry_limit_exceeded("registry request", retries, detail));
                    }
                    let next_retry = retries + 1;
                    let delay = backoff_delay(retries);
                    let retry_budget = format_retry_budget(next_retry, self.request_retry_limit);
                    warn!(
                        "request failed before response, retrying in {:?} ({})",
                        delay, retry_budget
                    );
                    sleep(delay).await;
                    retries = next_retry;
                    continue;
                }
                Err(error) => return Err(error.into()),
            };

            if response.status() == StatusCode::UNAUTHORIZED && cache_request {
                return Err(DockerPullError::Unauthorized(reference.normalized()));
            }

            if response.status() == StatusCode::UNAUTHORIZED {
                if auth_retries >= MAX_AUTH_RETRIES {
                    return Err(DockerPullError::Unauthorized(format!(
                        "authentication retry limit exceeded for {}",
                        reference.normalized()
                    )));
                }
                let challenge = response
                    .headers()
                    .get(WWW_AUTHENTICATE)
                    .cloned()
                    .ok_or_else(|| DockerPullError::Unauthorized("missing challenge".into()))?;
                if let Some(token) = self
                    .refresh_token(challenge, reference, credentials.clone())
                    .await?
                {
                    self.token_cache
                        .lock()
                        .await
                        .insert(cache_key.clone(), token);
                    auth_retries += 1;
                    continue;
                }
                return Err(DockerPullError::Unauthorized(reference.normalized()));
            }

            if request.allow_retry && is_retryable_status(response.status()) {
                let status = response.status();
                if request_retry_limit_exhausted(retries, self.request_retry_limit) {
                    return Err(retry_limit_exceeded(
                        "registry request",
                        retries,
                        format!("registry returned {status}"),
                    ));
                }
                let next_retry = retries + 1;
                let delay = retry_after_delay(response.headers().get(RETRY_AFTER))
                    .unwrap_or_else(|| backoff_delay(retries));
                let retry_budget = format_retry_budget(next_retry, self.request_retry_limit);
                warn!(
                    "registry returned {}, retrying in {:?} ({})",
                    status, delay, retry_budget
                );
                sleep(delay).await;
                retries = next_retry;
                continue;
            }

            if response.status().is_server_error() {
                return Err(DockerPullError::BadResponse(format!(
                    "registry returned {}",
                    response.status()
                )));
            }

            debug!("registry {} {}", request.method, url);
            return Ok(response);
        }
    }

    async fn refresh_token(
        &self,
        header: HeaderValue,
        reference: &ImageReference,
        credentials: Option<Credentials>,
    ) -> Result<Option<String>> {
        let challenge = parse_www_authenticate(header.to_str()?)?;
        if !challenge.scheme.eq_ignore_ascii_case("bearer") {
            return Ok(None);
        }

        let mut request = self.client.get(&challenge.realm);
        request = request.query(&[("service", challenge.service.as_deref().unwrap_or(""))]);
        let scope = challenge
            .scope
            .unwrap_or_else(|| reference.repository_scope());
        request = request.query(&[("scope", scope.as_str())]);
        if let Some(Credentials::Basic { username, password }) = credentials {
            request = request.basic_auth(username, Some(password));
        }
        let response = request.send().await?;
        if !response.status().is_success() {
            return Err(DockerPullError::Unauthorized(format!(
                "token endpoint returned {}",
                response.status()
            )));
        }
        let body: TokenResponse = response.json().await?;
        Ok(body.token.or(body.access_token))
    }

    fn uses_cache_from(&self) -> bool {
        self.cache_from.is_some()
    }
}

async fn raw_manifest_from_response(response: Response) -> Result<RawManifest> {
    if response.status() == StatusCode::NOT_FOUND {
        return Err(DockerPullError::ManifestNotFound);
    }
    if !response.status().is_success() {
        return Err(DockerPullError::BadResponse(format!(
            "registry returned {} for manifest",
            response.status()
        )));
    }
    let digest = header_string(&response, "docker-content-digest")?;
    let media_type = response_content_media_type(&response);
    let bytes = response.bytes().await?.to_vec();
    let envelope: ManifestEnvelope = serde_json::from_slice(&bytes)?;
    Ok(RawManifest {
        descriptor: Descriptor {
            media_type: if media_type.is_empty() {
                envelope.media_type
            } else {
                media_type
            },
            digest: digest.unwrap_or_else(|| digest_bytes(&bytes)),
            size: bytes.len() as i64,
            platform: None,
            annotations: None,
        },
        bytes,
    })
}

pub fn cache_repository(registry: &str, repository: &str) -> String {
    format!("{registry}/{repository}")
}

pub fn decode_cache_repository(repository: &str) -> Result<(String, String)> {
    let mut parts = repository.splitn(2, '/');
    let Some(registry) = parts.next() else {
        return Err(invalid_cache_repository(repository));
    };
    let Some(upstream_repository) = parts.next() else {
        return Err(invalid_cache_repository(repository));
    };
    if registry.is_empty() || upstream_repository.is_empty() {
        return Err(invalid_cache_repository(repository));
    }
    Ok((registry.to_string(), upstream_repository.to_string()))
}

fn invalid_cache_repository(repository: &str) -> DockerPullError {
    DockerPullError::InvalidInput(format!("invalid pocker cache repository `{repository}`"))
}

fn cache_url(
    base_url: &Url,
    reference: &ImageReference,
    resource: &str,
    suffix: &str,
) -> Result<Url> {
    let base = base_url.as_str().trim_end_matches('/');
    let repository = cache_repository(&reference.registry, &reference.repository);
    Url::parse(&format!("{base}/v2/{repository}/{resource}/{suffix}")).map_err(Into::into)
}

fn token_cache_key(reference: &ImageReference) -> String {
    format!("{}|{}", reference.registry, reference.repository_scope())
}

#[derive(Debug, Deserialize)]
struct TokenResponse {
    token: Option<String>,
    access_token: Option<String>,
}

#[derive(Debug)]
struct WwwAuthenticateChallenge {
    scheme: String,
    realm: String,
    service: Option<String>,
    scope: Option<String>,
}

fn parse_www_authenticate(header: &str) -> Result<WwwAuthenticateChallenge> {
    let (scheme, rest) = header
        .split_once(' ')
        .ok_or_else(|| DockerPullError::Unauthorized("invalid auth challenge".into()))?;
    let mut realm = None;
    let mut service = None;
    let mut scope = None;

    for item in split_auth_attributes(rest) {
        let (key, value) = item
            .trim()
            .split_once('=')
            .ok_or_else(|| DockerPullError::Unauthorized("invalid auth attribute".into()))?;
        let value = value.trim_matches('"');
        match key {
            "realm" => realm = Some(value.to_string()),
            "service" => service = Some(value.to_string()),
            "scope" => scope = Some(value.to_string()),
            _ => {}
        }
    }

    Ok(WwwAuthenticateChallenge {
        scheme: scheme.to_string(),
        realm: realm.ok_or_else(|| DockerPullError::Unauthorized("missing token realm".into()))?,
        service,
        scope,
    })
}

fn split_auth_attributes(value: &str) -> Vec<&str> {
    let mut attributes = Vec::new();
    let mut start = 0usize;
    let mut in_quotes = false;
    let mut escaped = false;

    for (index, ch) in value.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }

        match ch {
            '\\' if in_quotes => escaped = true,
            '"' => in_quotes = !in_quotes,
            ',' if !in_quotes => {
                attributes.push(value[start..index].trim());
                start = index + ch.len_utf8();
            }
            _ => {}
        }
    }

    attributes.push(value[start..].trim());
    attributes
}

fn is_image_manifest(media_type: &str) -> bool {
    matches!(
        media_type,
        "application/vnd.oci.image.manifest.v1+json"
            | "application/vnd.docker.distribution.manifest.v2+json"
    )
}

fn is_image_index(media_type: &str) -> bool {
    matches!(
        media_type,
        "application/vnd.oci.image.index.v1+json"
            | "application/vnd.docker.distribution.manifest.list.v2+json"
    )
}

fn is_retryable_status(status: StatusCode) -> bool {
    matches!(
        status,
        StatusCode::REQUEST_TIMEOUT
            | StatusCode::TOO_MANY_REQUESTS
            | StatusCode::BAD_GATEWAY
            | StatusCode::SERVICE_UNAVAILABLE
            | StatusCode::GATEWAY_TIMEOUT
            | StatusCode::INTERNAL_SERVER_ERROR
    )
}

fn is_retryable_http_error(error: &reqwest::Error) -> bool {
    error.is_timeout() || error.is_connect() || error.is_request() || error.is_body()
}

fn backoff_delay(attempt: u32) -> Duration {
    let seconds = 2_u64.saturating_pow(attempt.min(5)) + 1;
    Duration::from_secs(seconds)
}

fn retry_after_delay(value: Option<&HeaderValue>) -> Option<Duration> {
    let raw = value?.to_str().ok()?;
    if let Ok(seconds) = raw.parse::<u64>() {
        return Some(Duration::from_secs(seconds));
    }
    let retry_at = httpdate::parse_http_date(raw).ok()?;
    retry_at.duration_since(std::time::SystemTime::now()).ok()
}

fn request_retry_limit_exhausted(retries: u32, retry_limit: Option<u32>) -> bool {
    retry_limit.is_some_and(|limit| retries >= limit)
}

fn format_retry_budget(next_retry: u32, retry_limit: Option<u32>) -> String {
    match retry_limit {
        Some(limit) => format!("{next_retry}/{limit}"),
        None => format!("{next_retry}/unlimited"),
    }
}

fn header_string(response: &Response, name: &str) -> Result<Option<String>> {
    Ok(response
        .headers()
        .get(name)
        .map(|value| value.to_str().map(ToString::to_string))
        .transpose()?)
}

fn response_content_media_type(response: &Response) -> String {
    response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .map(|value| value.split(';').next().unwrap_or(value).trim().to_string())
        .unwrap_or_default()
}

fn digest_bytes(bytes: &[u8]) -> String {
    use sha2::{Digest as _, Sha256};

    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("sha256:{}", hex::encode(hasher.finalize()))
}

fn retry_limit_exceeded(
    operation: &str,
    retries: u32,
    detail: impl Into<String>,
) -> DockerPullError {
    DockerPullError::RetryLimitExceeded {
        operation: operation.to_string(),
        retries,
        detail: detail.into(),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    use super::{
        DEFAULT_REQUEST_RETRIES, RegistryClient, cache_repository, decode_cache_repository,
        parse_www_authenticate, token_cache_key,
    };
    use crate::auth::AuthResolver;
    use crate::error::DockerPullError;
    use crate::platform::Platform;
    use crate::reference::ImageReference;

    #[tokio::test]
    async fn resolve_image_stops_after_retry_budget_exhaustion() {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("listener should bind");
        let address = listener.local_addr().expect("listener address");

        let server = tokio::spawn(async move {
            for _ in 0..=DEFAULT_REQUEST_RETRIES {
                let (mut stream, _) = listener.accept().await.expect("connection should arrive");
                let mut buffer = [0_u8; 2048];
                let _ = stream.read(&mut buffer).await;
                stream
                    .write_all(
                        b"HTTP/1.1 503 Service Unavailable\r\nRetry-After: 0\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                    )
                    .await
                    .expect("response should be written");
            }
        });

        let client = RegistryClient::new(
            reqwest::Client::builder()
                .https_only(false)
                .build()
                .expect("client should build"),
            Arc::new(AuthResolver::new(None).expect("auth resolver should build")),
            true,
            Some(DEFAULT_REQUEST_RETRIES),
        );
        let reference = ImageReference::parse(&format!("{address}/sample:latest"))
            .expect("reference should parse");

        let error = client
            .resolve_image(&reference, &crate::platform::Platform::host())
            .await
            .expect_err("request should fail after retries are exhausted");
        match error {
            DockerPullError::RetryLimitExceeded {
                operation,
                retries,
                detail,
            } => {
                assert_eq!(operation, "registry request");
                assert_eq!(retries, DEFAULT_REQUEST_RETRIES);
                assert!(detail.contains("503"));
            }
            other => panic!("unexpected error: {other}"),
        }

        server.await.expect("server task should finish");
    }

    #[test]
    fn token_cache_key_includes_registry_host() {
        let left =
            ImageReference::parse("ghcr.io/acme/app:latest").expect("reference should parse");
        let right =
            ImageReference::parse("docker.io/acme/app:latest").expect("reference should parse");

        assert_ne!(token_cache_key(&left), token_cache_key(&right));
    }

    #[test]
    fn parse_www_authenticate_keeps_commas_inside_quoted_values() {
        let challenge = parse_www_authenticate(
            r#"Bearer realm="https://auth.example/token",service="registry.example",scope="repository:acme/app:pull,push""#,
        )
        .expect("challenge should parse");

        assert_eq!(challenge.scheme, "Bearer");
        assert_eq!(challenge.realm, "https://auth.example/token");
        assert_eq!(challenge.service.as_deref(), Some("registry.example"));
        assert_eq!(
            challenge.scope.as_deref(),
            Some("repository:acme/app:pull,push")
        );
    }

    #[test]
    fn cache_repository_keeps_registry_as_first_segment() {
        let repository = cache_repository("registry.example:5000", "team/app");

        assert_eq!(repository, "registry.example:5000/team/app");
        assert_eq!(
            decode_cache_repository(&repository).expect("cache repository should decode"),
            ("registry.example:5000".to_string(), "team/app".to_string())
        );
    }

    #[test]
    fn decode_cache_repository_rejects_malformed_paths() {
        assert!(decode_cache_repository("registry.example").is_err());
        assert!(decode_cache_repository("/team/app").is_err());
        assert!(decode_cache_repository("registry.example/").is_err());
    }

    #[tokio::test]
    async fn missing_child_manifest_from_index_reports_manifest_not_found() {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("listener should bind");
        let address = listener.local_addr().expect("listener address");

        let index_body = br#"{
            "schemaVersion": 2,
            "mediaType": "application/vnd.oci.image.index.v1+json",
            "manifests": [
                {
                    "mediaType": "application/vnd.oci.image.manifest.v1+json",
                    "digest": "sha256:1111111111111111111111111111111111111111111111111111111111111111",
                    "size": 123,
                    "platform": {"os": "linux", "architecture": "amd64"}
                }
            ]
        }"#;

        let server = tokio::spawn(async move {
            let (mut first, _) = listener
                .accept()
                .await
                .expect("first connection should arrive");
            let mut buffer = [0_u8; 4096];
            let _ = first.read(&mut buffer).await;
            first
                .write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/vnd.oci.image.index.v1+json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        index_body.len(),
                        std::str::from_utf8(index_body).expect("index body should be utf8"),
                    )
                    .as_bytes(),
                )
                .await
                .expect("index response should be written");

            let (mut second, _) = listener
                .accept()
                .await
                .expect("second connection should arrive");
            let _ = second.read(&mut buffer).await;
            second
                .write_all(
                    b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                )
                .await
                .expect("manifest response should be written");
        });

        let client = RegistryClient::new(
            reqwest::Client::builder()
                .https_only(false)
                .build()
                .expect("client should build"),
            Arc::new(AuthResolver::new(None).expect("auth resolver should build")),
            true,
            Some(DEFAULT_REQUEST_RETRIES),
        );
        let reference = ImageReference::parse(&format!("{address}/sample:latest"))
            .expect("reference should parse");

        let error = client
            .resolve_image(
                &reference,
                &Platform::parse("linux/amd64").expect("platform should parse"),
            )
            .await
            .expect_err("child manifest lookup should fail");
        assert!(matches!(error, DockerPullError::ManifestNotFound));

        server.await.expect("server task should finish");
    }

    #[tokio::test]
    async fn cache_from_manifest_miss_falls_back_to_upstream() {
        let cache = spawn_single_response(
            b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec(),
        )
        .await;
        let manifest = sample_manifest_bytes();
        let upstream = spawn_single_response(
            format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/vnd.oci.image.manifest.v1+json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                manifest.len(),
                std::str::from_utf8(&manifest).expect("manifest should be utf8"),
            )
            .into_bytes(),
        )
        .await;
        let client = cache_from_client(cache, false);
        let reference = ImageReference::parse(&format!("{upstream}/sample:latest"))
            .expect("reference should parse");

        client
            .resolve_image(
                &reference,
                &Platform::parse("linux/amd64").expect("platform should parse"),
            )
            .await
            .expect("cache miss should fall back to upstream");
    }

    #[tokio::test]
    async fn cache_only_manifest_miss_does_not_fall_back_to_upstream() {
        let cache = spawn_single_response(
            b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec(),
        )
        .await;
        let client = cache_from_client(cache, true);
        let reference =
            ImageReference::parse("127.0.0.1:1/sample:latest").expect("reference should parse");

        let error = client
            .resolve_image(
                &reference,
                &Platform::parse("linux/amd64").expect("platform should parse"),
            )
            .await
            .expect_err("cache-only miss should fail");

        assert!(matches!(error, DockerPullError::ManifestNotFound));
    }

    #[tokio::test]
    async fn cache_from_blob_miss_falls_back_to_upstream() {
        let cache = spawn_single_response(
            b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec(),
        )
        .await;
        let body = b"blob";
        let upstream = spawn_single_response(
            format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                std::str::from_utf8(body).expect("blob should be utf8"),
            )
            .into_bytes(),
        )
        .await;
        let client = cache_from_client(cache, false);
        let reference = ImageReference::parse(&format!("{upstream}/sample:latest"))
            .expect("reference should parse");

        let bytes = client
            .get_blob_bytes(
                &reference,
                "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            )
            .await
            .expect("cache blob miss should fall back to upstream");

        assert_eq!(bytes, body);
    }

    async fn spawn_single_response(response: Vec<u8>) -> std::net::SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("listener should bind");
        let address = listener.local_addr().expect("listener address");
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("connection should arrive");
            let mut buffer = [0_u8; 4096];
            let _ = stream.read(&mut buffer).await;
            stream
                .write_all(&response)
                .await
                .expect("response should be written");
        });
        address
    }

    fn cache_from_client(cache: std::net::SocketAddr, cache_only: bool) -> RegistryClient {
        RegistryClient::new_with_cache_from(
            reqwest::Client::builder()
                .https_only(false)
                .build()
                .expect("client should build"),
            Arc::new(AuthResolver::new(None).expect("auth resolver should build")),
            true,
            Some(DEFAULT_REQUEST_RETRIES),
            Some(
                format!("http://{cache}")
                    .parse()
                    .expect("cache URL should parse"),
            ),
            cache_only,
        )
    }

    fn sample_manifest_bytes() -> Vec<u8> {
        br#"{
            "schemaVersion": 2,
            "mediaType": "application/vnd.oci.image.manifest.v1+json",
            "config": {
                "mediaType": "application/vnd.oci.image.config.v1+json",
                "digest": "sha256:2222222222222222222222222222222222222222222222222222222222222222",
                "size": 2
            },
            "layers": []
        }"#
        .to_vec()
    }
}
