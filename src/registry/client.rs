use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use reqwest::header::{ACCEPT, HeaderName, HeaderValue, RANGE, RETRY_AFTER, WWW_AUTHENTICATE};
use reqwest::{Client, Method, Response, StatusCode};
use serde::Deserialize;
use tokio::sync::Mutex;
use tokio::time::sleep;
use tracing::{debug, warn};
use url::Url;

use super::cache::{cache_url, resource_url};
use super::types::{
    BlobMetadata, Descriptor, ImageIndex, ImageManifest, ManifestEnvelope, RawManifest,
    ResolvedImage,
};
use super::{
    DOCKER_IMAGE_MANIFEST_MEDIA_TYPE, DOCKER_MANIFEST_LIST_MEDIA_TYPE, MANIFEST_ACCEPT,
    OCI_IMAGE_INDEX_MEDIA_TYPE, OCI_IMAGE_MANIFEST_MEDIA_TYPE,
};
use crate::auth::{AuthResolver, Credentials};
use crate::digest::canonical_digest_bytes;
use crate::error::{DockerPullError, Result};
use crate::platform::Platform;
use crate::reference::ImageReference;
use crate::retry::{jittered_backoff_delay, record_retry_attempt};

// Metadata requests are small and should fail fast enough to surface bad
// registries, while still tolerating transient DNS/TLS/5xx failures.
pub const DEFAULT_REQUEST_RETRIES: u32 = 5;
// Auth retries are intentionally tighter than network retries so bad or stale
// credentials do not spin indefinitely.
const MAX_AUTH_RETRIES: u32 = 2;
const DOCKER_CONTENT_DIGEST: HeaderName = HeaderName::from_static("docker-content-digest");

type RetryWarningSink = Arc<dyn Fn(String) + Send + Sync>;
pub(crate) type RetryStatusSink = Arc<dyn Fn(String) + Send + Sync>;

#[derive(Clone)]
pub struct RegistryClient {
    client: Client,
    auth: Arc<AuthResolver>,
    token_cache: Arc<Mutex<HashMap<String, String>>>,
    plain_http: bool,
    request_retry_limit: Option<u32>,
    cache_from: Option<Url>,
    cache_only: bool,
    retry_warning_sink: Option<RetryWarningSink>,
}

struct RegistryRequest<'a> {
    method: Method,
    url: Url,
    fallback_url: Option<Url>,
    accept: Option<&'a str>,
    range: Option<&'a str>,
    allow_retry: bool,
    retry_status_sink: Option<RetryStatusSink>,
}

struct RegistryAuthContext {
    token: Option<String>,
    credentials: Option<Credentials>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RequestMode {
    Direct,
    Cache,
}

impl RequestMode {
    fn uses_registry_auth(self) -> bool {
        matches!(self, Self::Direct)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RetryReason {
    SendError,
    Status(StatusCode),
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
            retry_warning_sink: None,
        }
    }

    pub(crate) fn with_retry_warning_sink(mut self, sink: RetryWarningSink) -> Self {
        self.retry_warning_sink = Some(sink);
        self
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
                    retry_status_sink: None,
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
                    retry_status_sink: None,
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
                    retry_status_sink: None,
                },
                reference,
            )
            .await?;
        if response.status() == StatusCode::NOT_FOUND {
            return Err(DockerPullError::ManifestNotFound);
        }
        ensure_success_status(response.status(), "manifest")?;
        let digest = header_string(&response, &DOCKER_CONTENT_DIGEST)?;
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
                digest: digest.unwrap_or_else(|| canonical_digest_bytes(&body)),
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
                        retry_status_sink: None,
                    },
                    reference,
                )
                .await?;
            if response.status() == StatusCode::NOT_FOUND {
                return Err(DockerPullError::ManifestNotFound);
            }
            ensure_success_status(
                response.status(),
                &format!("manifest {}", descriptor.digest),
            )?;
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
                    retry_status_sink: None,
                },
                reference,
            )
            .await?;
        if response.status() == StatusCode::NOT_FOUND {
            return Err(DockerPullError::BlobNotFound(digest.to_string()));
        }
        if !response.status().is_success() {
            return Err(DockerPullError::BadResponse(format!(
                "registry returned {} for blob {digest}",
                response.status()
            )));
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
        self.get_blob_with_retry_status(reference, digest, offset, None)
            .await
    }

    pub(crate) async fn get_blob_with_retry_status(
        &self,
        reference: &ImageReference,
        digest: &str,
        offset: u64,
        retry_status_sink: Option<RetryStatusSink>,
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
                retry_status_sink,
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
        if !response.status().is_success() {
            return Err(DockerPullError::BadResponse(format!(
                "registry returned {} for blob {digest}",
                response.status()
            )));
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
        let base = Url::parse(&format!("{}://{}", self.scheme(), reference.registry))?;
        resource_url(base, &reference.repository, resource, suffix)
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
                if self.uses_cache_from() {
                    RequestMode::Cache
                } else {
                    RequestMode::Direct
                },
            )
            .await?;
        if response.status() == StatusCode::NOT_FOUND
            && let Some(fallback_url) = request.fallback_url.clone()
        {
            return self
                .send_to_url(&request, fallback_url, reference, RequestMode::Direct)
                .await;
        }
        Ok(response)
    }

    async fn send_to_url(
        &self,
        request: &RegistryRequest<'_>,
        url: Url,
        reference: &ImageReference,
        mode: RequestMode,
    ) -> Result<Response> {
        let cache_key = token_cache_key(reference);
        let mut retries = 0_u32;
        let mut auth_retries = 0_u32;
        loop {
            let auth = self.auth_context(reference, &cache_key, mode).await?;
            let builder = self.request_builder(request, &url, &auth);

            let response = match builder.send().await {
                Ok(response) => response,
                Err(error) if request.allow_retry && is_retryable_http_error(&error) => {
                    let delay = jittered_backoff_delay(retries);
                    self.retry_request(
                        reference,
                        request.retry_status_sink.as_ref(),
                        &mut retries,
                        error.to_string(),
                        delay,
                        RetryReason::SendError,
                    )
                    .await?;
                    continue;
                }
                Err(error) => return Err(error.into()),
            };

            if response.status() == StatusCode::UNAUTHORIZED && mode == RequestMode::Cache {
                return Err(DockerPullError::Unauthorized(reference.normalized()));
            }

            if response.status() == StatusCode::UNAUTHORIZED {
                if self
                    .refresh_auth_token(response, reference, &cache_key, auth, &mut auth_retries)
                    .await?
                {
                    continue;
                }
                return Err(DockerPullError::Unauthorized(reference.normalized()));
            }

            if request.allow_retry && is_retryable_status(response.status()) {
                let status = response.status();
                let delay = retry_after_delay(response.headers().get(RETRY_AFTER))
                    .unwrap_or_else(|| jittered_backoff_delay(retries));
                self.retry_request(
                    reference,
                    request.retry_status_sink.as_ref(),
                    &mut retries,
                    format!("registry returned {status}"),
                    delay,
                    RetryReason::Status(status),
                )
                .await?;
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

    async fn auth_context(
        &self,
        reference: &ImageReference,
        cache_key: &str,
        mode: RequestMode,
    ) -> Result<RegistryAuthContext> {
        if !mode.uses_registry_auth() {
            return Ok(RegistryAuthContext {
                token: None,
                credentials: None,
            });
        }

        Ok(RegistryAuthContext {
            token: self.token_cache.lock().await.get(cache_key).cloned(),
            credentials: self.auth.resolve(&reference.registry).await?,
        })
    }

    fn request_builder(
        &self,
        request: &RegistryRequest<'_>,
        url: &Url,
        auth: &RegistryAuthContext,
    ) -> reqwest::RequestBuilder {
        let mut builder = self.client.request(request.method.clone(), url.clone());
        if let Some(accept) = request.accept {
            builder = builder.header(ACCEPT, accept);
        }
        if let Some(range) = request.range {
            builder = builder.header(RANGE, range);
        }
        if let Some(token) = &auth.token {
            builder = builder.bearer_auth(token);
        } else if let Some(Credentials::Basic { username, password }) = &auth.credentials {
            builder = builder.basic_auth(username, Some(password));
        }
        builder
    }

    async fn refresh_auth_token(
        &self,
        response: Response,
        reference: &ImageReference,
        cache_key: &str,
        auth: RegistryAuthContext,
        auth_retries: &mut u32,
    ) -> Result<bool> {
        if *auth_retries >= MAX_AUTH_RETRIES {
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
            .refresh_token(challenge, reference, auth.credentials)
            .await?
        {
            self.token_cache
                .lock()
                .await
                .insert(cache_key.to_string(), token);
            *auth_retries += 1;
            return Ok(true);
        }
        Ok(false)
    }

    async fn retry_request(
        &self,
        reference: &ImageReference,
        retry_status_sink: Option<&RetryStatusSink>,
        retries: &mut u32,
        detail: String,
        delay: Duration,
        reason: RetryReason,
    ) -> Result<()> {
        let retry_budget = record_retry_attempt(
            retries,
            self.request_retry_limit,
            "registry request",
            detail,
        )?;
        let inline_status = match reason {
            RetryReason::SendError => {
                format!("Retrying request in {delay:?} ({retry_budget})")
            }
            RetryReason::Status(status) => {
                format!("Retrying after {status} in {delay:?} ({retry_budget})")
            }
        };
        let warning = match reason {
            RetryReason::SendError => format!(
                "registry request for {} failed before response; retrying in {delay:?} ({retry_budget})",
                reference.display_name()
            ),
            RetryReason::Status(status) => {
                format!(
                    "registry request for {} returned {status}; retrying in {delay:?} ({retry_budget})",
                    reference.display_name()
                )
            }
        };
        if let Some(sink) = retry_status_sink {
            sink(inline_status);
        } else {
            self.warn_retry(warning);
        }
        sleep(delay).await;
        Ok(())
    }

    fn warn_retry(&self, warning: String) {
        if let Some(sink) = &self.retry_warning_sink {
            sink(warning);
        } else {
            warn!("{warning}");
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

impl fmt::Debug for RegistryClient {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RegistryClient")
            .field("plain_http", &self.plain_http)
            .field("request_retry_limit", &self.request_retry_limit)
            .field("cache_from", &self.cache_from)
            .field("cache_only", &self.cache_only)
            .field("retry_warning_sink", &self.retry_warning_sink.is_some())
            .finish_non_exhaustive()
    }
}

async fn raw_manifest_from_response(response: Response) -> Result<RawManifest> {
    if response.status() == StatusCode::NOT_FOUND {
        return Err(DockerPullError::ManifestNotFound);
    }
    ensure_success_status(response.status(), "manifest")?;
    let digest = header_string(&response, &DOCKER_CONTENT_DIGEST)?;
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
            digest: digest.unwrap_or_else(|| canonical_digest_bytes(&bytes)),
            size: bytes.len() as i64,
            platform: None,
            annotations: None,
        },
        bytes,
    })
}

fn ensure_success_status(status: StatusCode, resource: &str) -> Result<()> {
    if status.is_success() {
        return Ok(());
    }
    Err(DockerPullError::BadResponse(format!(
        "registry returned {status} for {resource}"
    )))
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
        OCI_IMAGE_MANIFEST_MEDIA_TYPE | DOCKER_IMAGE_MANIFEST_MEDIA_TYPE
    )
}

fn is_image_index(media_type: &str) -> bool {
    matches!(
        media_type,
        OCI_IMAGE_INDEX_MEDIA_TYPE | DOCKER_MANIFEST_LIST_MEDIA_TYPE
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

fn retry_after_delay(value: Option<&HeaderValue>) -> Option<Duration> {
    let raw = value?.to_str().ok()?;
    if let Ok(seconds) = raw.parse::<u64>() {
        return Some(Duration::from_secs(seconds));
    }
    let retry_at = httpdate::parse_http_date(raw).ok()?;
    retry_at.duration_since(std::time::SystemTime::now()).ok()
}

fn header_string(response: &Response, name: &HeaderName) -> Result<Option<String>> {
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

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex as StdMutex};

    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;
    use url::Url;

    use super::super::cache::{cache_repository, decode_cache_repository, resource_url};
    use super::{DEFAULT_REQUEST_RETRIES, RegistryClient, parse_www_authenticate, token_cache_key};
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

        let warnings = Arc::new(StdMutex::new(Vec::new()));
        let warning_sink = Arc::clone(&warnings);
        let client = RegistryClient::new(
            reqwest::Client::builder()
                .https_only(false)
                .build()
                .expect("client should build"),
            Arc::new(AuthResolver::new(None).expect("auth resolver should build")),
            true,
            Some(DEFAULT_REQUEST_RETRIES),
        )
        .with_retry_warning_sink(Arc::new(move |warning| {
            warning_sink
                .lock()
                .expect("warning sink should not be poisoned")
                .push(warning);
        }));
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

        {
            let warnings = warnings
                .lock()
                .expect("warning sink should not be poisoned");
            assert_eq!(warnings.len(), DEFAULT_REQUEST_RETRIES as usize);
            assert!(warnings[0].contains("registry request for"));
            assert!(warnings[0].contains("returned 503 Service Unavailable"));
            assert!(warnings[0].contains("(1/5)"));
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

    #[test]
    fn descriptor_expected_size_rejects_negative_values() {
        let descriptor = crate::registry::Descriptor {
            media_type: "application/octet-stream".into(),
            digest: "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
                .into(),
            size: -1,
            platform: None,
            annotations: None,
        };

        let error = descriptor
            .expected_size()
            .expect_err("negative descriptor size should fail");

        assert!(matches!(error, DockerPullError::InvalidInput(_)));
    }

    #[test]
    fn resource_url_encodes_path_segments_without_losing_base_path() {
        let url = resource_url(
            Url::parse("http://cache.example:5000/pocker?ignored=true")
                .expect("base URL should parse"),
            "registry-1.docker.io/library/alpine",
            "manifests",
            "release/with/slashes",
        )
        .expect("resource URL should build");

        assert_eq!(
            url.as_str(),
            "http://cache.example:5000/pocker/v2/registry-1.docker.io/library/alpine/manifests/release%2Fwith%2Fslashes"
        );
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
    async fn resolve_image_rejects_non_success_manifest_status() {
        let registry = spawn_single_response(
            b"HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec(),
        )
        .await;
        let client = RegistryClient::new(
            reqwest::Client::builder()
                .https_only(false)
                .build()
                .expect("client should build"),
            Arc::new(AuthResolver::new(None).expect("auth resolver should build")),
            true,
            Some(DEFAULT_REQUEST_RETRIES),
        );
        let reference = ImageReference::parse(&format!("{registry}/sample:latest"))
            .expect("reference should parse");

        let error = client
            .resolve_image(
                &reference,
                &Platform::parse("linux/amd64").expect("platform should parse"),
            )
            .await
            .expect_err("non-success manifest response should fail before JSON parsing");

        assert!(matches!(error, DockerPullError::BadResponse(message) if message.contains("403")));
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

    #[tokio::test]
    async fn head_blob_rejects_non_success_status() {
        let registry = spawn_single_response(
            b"HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec(),
        )
        .await;
        let client = RegistryClient::new(
            reqwest::Client::builder()
                .https_only(false)
                .build()
                .expect("client should build"),
            Arc::new(AuthResolver::new(None).expect("auth resolver should build")),
            true,
            Some(DEFAULT_REQUEST_RETRIES),
        );
        let reference =
            ImageReference::parse(&format!("{registry}/sample:latest")).expect("reference parse");

        let error = client
            .head_blob(
                &reference,
                "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            )
            .await
            .expect_err("non-success HEAD responses should fail");

        assert!(matches!(error, DockerPullError::BadResponse(message) if message.contains("403")));
    }

    #[tokio::test]
    async fn get_blob_bytes_rejects_non_success_status() {
        let registry = spawn_single_response(
            b"HTTP/1.1 403 Forbidden\r\nContent-Length: 9\r\nConnection: close\r\n\r\nforbidden"
                .to_vec(),
        )
        .await;
        let client = RegistryClient::new(
            reqwest::Client::builder()
                .https_only(false)
                .build()
                .expect("client should build"),
            Arc::new(AuthResolver::new(None).expect("auth resolver should build")),
            true,
            Some(DEFAULT_REQUEST_RETRIES),
        );
        let reference =
            ImageReference::parse(&format!("{registry}/sample:latest")).expect("reference parse");

        let error = client
            .get_blob_bytes(
                &reference,
                "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            )
            .await
            .expect_err("non-success blob byte responses should fail");

        assert!(matches!(error, DockerPullError::BadResponse(message) if message.contains("403")));
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
