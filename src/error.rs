use std::path::PathBuf;

use thiserror::Error;

pub type Result<T> = std::result::Result<T, DockerPullError>;

#[derive(Debug, Error)]
pub enum DockerPullError {
    #[error("{0}")]
    InvalidInput(String),
    #[error("unsupported media type `{0}`")]
    UnsupportedMediaType(String),
    #[error("manifest not found")]
    ManifestNotFound,
    #[error("blob not found: {0}")]
    BlobNotFound(String),
    #[error("requested platform `{0}` not found in image index")]
    PlatformNotFound(String),
    #[error("unauthorized: {0}")]
    Unauthorized(String),
    #[error("bad registry response: {0}")]
    BadResponse(String),
    #[error("digest mismatch for {digest}: expected {expected}, got {actual}")]
    DigestMismatch {
        digest: String,
        expected: String,
        actual: String,
    },
    #[error("download interrupted")]
    Interrupted,
    #[error("missing blob file `{0}` at `{1}`")]
    MissingBlobFile(String, PathBuf),
    #[error("command failed: {0}")]
    CommandFailed(String),
    #[error("retry limit exceeded for {operation} after {retries} retries: {detail}")]
    RetryLimitExceeded {
        operation: String,
        retries: u32,
        detail: String,
    },
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Http(#[from] reqwest::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    Yaml(#[from] serde_yml::Error),
    #[error(transparent)]
    Url(#[from] url::ParseError),
    #[error(transparent)]
    Header(#[from] http::header::ToStrError),
    #[error(transparent)]
    Base64(#[from] base64::DecodeError),
}
