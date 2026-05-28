use std::path::PathBuf;

use thiserror::Error;

pub type Result<T> = std::result::Result<T, DockerPullError>;

#[derive(Debug, Error)]
pub enum DockerPullError {
    #[error("{0}")]
    InvalidInput(String),
    #[error("unsupported media type `{0}`")]
    UnsupportedMediaType(String),
    #[error("unsupported digest algorithm `{0}`; pocker currently supports sha256 digests only")]
    UnsupportedDigestAlgorithm(String),
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
    #[error("digest mismatch: expected {expected}, got {actual}")]
    DigestMismatch { expected: String, actual: String },
    #[error("download interrupted")]
    Interrupted,
    #[error(
        "{operation} cannot continue because the cache is in use by another pocker process: {path}\nThe .lock file may remain after exit; only a live OS lock blocks this operation."
    )]
    CacheLocked {
        operation: &'static str,
        path: PathBuf,
    },
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
    #[error("failed to resolve image `{reference}`: {source}")]
    ImageResolutionFailed {
        reference: String,
        #[source]
        source: Box<DockerPullError>,
    },
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Http(#[from] reqwest::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    Compose(#[from] pocker_compose::ComposeError),
    #[error(transparent)]
    Url(#[from] url::ParseError),
    #[error(transparent)]
    Header(#[from] http::header::ToStrError),
    #[error(transparent)]
    Base64(#[from] base64::DecodeError),
}

#[cfg(test)]
mod tests {
    use super::DockerPullError;

    #[test]
    fn image_resolution_error_includes_reference_and_source() {
        let error = DockerPullError::ImageResolutionFailed {
            reference: "example.com/library/app:latest".to_string(),
            source: Box::new(DockerPullError::PlatformNotFound("linux/s390x".to_string())),
        };

        assert_eq!(
            error.to_string(),
            "failed to resolve image `example.com/library/app:latest`: requested platform `linux/s390x` not found in image index"
        );
    }
}
