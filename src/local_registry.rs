use std::net::SocketAddr;
use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::header::{CONTENT_LENGTH, CONTENT_TYPE};
use axum::http::{Response, StatusCode};
use axum::routing::get;
use serde::Serialize;
use tokio::fs;
use tokio::net::TcpListener;
use tokio_util::io::ReaderStream;

use crate::error::{DockerPullError, Result};
use crate::store::{CachedReference, Store};

const REGISTRY_API_VERSION: &str = "registry/2.0";

#[derive(Clone)]
struct RegistryState {
    store: Arc<Store>,
}

pub async fn serve_cached_registry(store: Arc<Store>, bind: SocketAddr) -> Result<()> {
    let app = Router::new()
        .route("/v2", get(v2_ping).head(v2_ping))
        .route("/v2/", get(v2_ping).head(v2_ping))
        .route("/v2/{*rest}", get(handle_get).head(handle_head))
        .with_state(RegistryState { store });
    let listener = TcpListener::bind(bind).await?;
    println!("serving cached registry on http://{bind}");
    axum::serve(listener, app).await.map_err(Into::into)
}

async fn v2_ping() -> Response<Body> {
    response(StatusCode::OK, Body::empty(), Vec::new())
}

async fn handle_get(
    State(state): State<RegistryState>,
    Path(rest): Path<String>,
) -> Response<Body> {
    handle_request(state, rest, false).await
}

async fn handle_head(
    State(state): State<RegistryState>,
    Path(rest): Path<String>,
) -> Response<Body> {
    handle_request(state, rest, true).await
}

async fn handle_request(state: RegistryState, rest: String, head_only: bool) -> Response<Body> {
    match parse_request_path(&rest) {
        Some(RouteTarget::Manifest {
            local_repository,
            reference,
        }) => serve_manifest(state, &local_repository, &reference, head_only).await,
        Some(RouteTarget::Blob { digest }) => serve_blob(state, &digest, head_only).await,
        None => error_response(
            StatusCode::NOT_FOUND,
            "NAME_UNKNOWN",
            "repository path is not available in the cache",
        ),
    }
}

async fn serve_manifest(
    state: RegistryState,
    local_repository: &str,
    reference: &str,
    head_only: bool,
) -> Response<Body> {
    let cached = match lookup_reference(&state.store, local_repository, reference) {
        Ok(Some(cached)) => cached,
        Ok(None) => {
            return error_response(
                StatusCode::NOT_FOUND,
                "MANIFEST_UNKNOWN",
                "manifest is not available in the cache",
            );
        }
        Err(error) => return internal_error_response(error),
    };

    let path = match state.store.blob_path(&cached.manifest_digest) {
        Ok(path) => path,
        Err(error) => return internal_error_response(error),
    };
    if !path.exists() {
        return error_response(
            StatusCode::NOT_FOUND,
            "MANIFEST_UNKNOWN",
            "manifest blob is missing from the cache",
        );
    }

    if head_only {
        return response(
            StatusCode::OK,
            Body::empty(),
            vec![
                (
                    CONTENT_TYPE.as_str().to_string(),
                    cached.manifest_media_type.clone(),
                ),
                (
                    "docker-content-digest".to_string(),
                    cached.manifest_digest.clone(),
                ),
                (
                    CONTENT_LENGTH.as_str().to_string(),
                    cached.manifest_size.to_string(),
                ),
            ],
        );
    }

    match fs::read(&path).await {
        Ok(bytes) => response(
            StatusCode::OK,
            Body::from(bytes),
            vec![
                (
                    CONTENT_TYPE.as_str().to_string(),
                    cached.manifest_media_type,
                ),
                ("docker-content-digest".to_string(), cached.manifest_digest),
                (
                    CONTENT_LENGTH.as_str().to_string(),
                    cached.manifest_size.to_string(),
                ),
            ],
        ),
        Err(error) => internal_error_response(error.into()),
    }
}

async fn serve_blob(state: RegistryState, digest: &str, head_only: bool) -> Response<Body> {
    let path = match state.store.blob_path(digest) {
        Ok(path) => path,
        Err(error) => return internal_error_response(error),
    };
    if !path.exists() {
        return error_response(
            StatusCode::NOT_FOUND,
            "BLOB_UNKNOWN",
            "blob is not available in the cache",
        );
    }

    let metadata = match fs::metadata(&path).await {
        Ok(metadata) => metadata,
        Err(error) => return internal_error_response(error.into()),
    };
    if head_only {
        return response(
            StatusCode::OK,
            Body::empty(),
            vec![
                (
                    CONTENT_TYPE.as_str().to_string(),
                    "application/octet-stream".to_string(),
                ),
                ("docker-content-digest".to_string(), digest.to_string()),
                (
                    CONTENT_LENGTH.as_str().to_string(),
                    metadata.len().to_string(),
                ),
            ],
        );
    }

    match fs::File::open(&path).await {
        Ok(file) => response(
            StatusCode::OK,
            Body::from_stream(ReaderStream::new(file)),
            vec![
                (
                    CONTENT_TYPE.as_str().to_string(),
                    "application/octet-stream".to_string(),
                ),
                ("docker-content-digest".to_string(), digest.to_string()),
                (
                    CONTENT_LENGTH.as_str().to_string(),
                    metadata.len().to_string(),
                ),
            ],
        ),
        Err(error) => internal_error_response(error.into()),
    }
}

fn lookup_reference(
    store: &Store,
    local_repository: &str,
    reference: &str,
) -> Result<Option<CachedReference>> {
    if reference.contains(':') {
        store.load_cached_digest(local_repository, reference)
    } else {
        store.load_cached_tag(local_repository, reference)
    }
}

fn response(status: StatusCode, body: Body, headers: Vec<(String, String)>) -> Response<Body> {
    let mut builder = Response::builder().status(status);
    builder = builder.header("docker-distribution-api-version", REGISTRY_API_VERSION);
    for (name, value) in headers {
        builder = builder.header(name, value);
    }
    builder.body(body).unwrap_or_else(|_| {
        internal_error_response(DockerPullError::BadResponse(
            "failed to build response".into(),
        ))
    })
}

fn error_response(status: StatusCode, code: &str, message: &str) -> Response<Body> {
    let body = serde_json::to_vec(&RegistryErrors {
        errors: vec![RegistryError {
            code: code.to_string(),
            message: message.to_string(),
        }],
    })
    .unwrap_or_default();
    response(
        status,
        Body::from(body),
        vec![(
            CONTENT_TYPE.as_str().to_string(),
            "application/json".to_string(),
        )],
    )
}

fn internal_error_response(error: DockerPullError) -> Response<Body> {
    response(
        StatusCode::INTERNAL_SERVER_ERROR,
        Body::from(error.to_string()),
        vec![(
            CONTENT_TYPE.as_str().to_string(),
            "text/plain; charset=utf-8".to_string(),
        )],
    )
}

fn parse_request_path(rest: &str) -> Option<RouteTarget> {
    if let Some((local_repository, reference)) = rest.rsplit_once("/manifests/")
        && !local_repository.is_empty()
        && !reference.is_empty()
    {
        return Some(RouteTarget::Manifest {
            local_repository: local_repository.to_string(),
            reference: reference.to_string(),
        });
    }

    if let Some((local_repository, digest)) = rest.rsplit_once("/blobs/")
        && !local_repository.is_empty()
        && !digest.is_empty()
    {
        return Some(RouteTarget::Blob {
            digest: digest.to_string(),
        });
    }

    None
}

enum RouteTarget {
    Manifest {
        local_repository: String,
        reference: String,
    },
    Blob {
        digest: String,
    },
}

#[derive(Serialize)]
struct RegistryErrors {
    errors: Vec<RegistryError>,
}

#[derive(Serialize)]
struct RegistryError {
    code: String,
    message: String,
}
