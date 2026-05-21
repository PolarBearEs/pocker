use std::time::{Duration, Instant};

use futures_util::StreamExt;
use reqwest::StatusCode;
use tokio::fs::{self, OpenOptions};
use tokio::io::AsyncWriteExt;
use tracing::warn;

use crate::error::{DockerPullError, Result};
use crate::pull::PullContext;
use crate::reference::ImageReference;
use crate::registry::Descriptor;
use crate::store::DownloadPlan;

const CHECKPOINT_BYTES: u64 = 8 * 1024 * 1024;
const CHECKPOINT_INTERVAL: Duration = Duration::from_secs(2);

pub async fn download_blob(
    context: &PullContext,
    reference: &ImageReference,
    normalized_reference: &str,
    descriptor: Descriptor,
) -> Result<()> {
    let _blob_guard = context.blob_locks.lock(&descriptor.digest).await;

    if context
        .store
        .ensure_blob_complete(&descriptor.digest, descriptor.expected_size()?)
        .await?
    {
        return Ok(());
    }

    let head = context
        .registry
        .head_blob(reference, &descriptor.digest)
        .await?;
    let expected_size = head.size.unwrap_or(descriptor.expected_size()?);
    let mut plan = context
        .store
        .prepare_download(normalized_reference, &descriptor, expected_size)
        .await?;
    let mut offset = plan.durable_offset;
    context
        .ui
        .start_layer_download(&descriptor.digest, expected_size, offset);
    let mut bytes_since_checkpoint = 0_u64;
    let mut last_checkpoint = Instant::now();
    let mut retries = 0_u32;

    loop {
        if context.stop.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(DockerPullError::Interrupted);
        }
        if offset >= expected_size {
            break;
        }

        let response = context
            .registry
            .get_blob(reference, &descriptor.digest, offset)
            .await?;
        let status = response.status();
        if status == StatusCode::OK && offset > 0 {
            let delay = retry_delay(offset);
            retries = register_retry(
                context,
                &descriptor.digest,
                retries,
                "registry ignored ranged resume",
                delay,
            )?;
            warn!(
                "registry ignored range request for {}, restarting blob",
                descriptor.digest
            );
            reset_download_state(
                context,
                normalized_reference,
                &descriptor,
                expected_size,
                &mut plan,
                &mut offset,
            )
            .await?;
            reset_checkpoint_tracking(&mut bytes_since_checkpoint, &mut last_checkpoint);
            tokio::time::sleep(delay).await;
            continue;
        }
        if status == StatusCode::RANGE_NOT_SATISFIABLE {
            let delay = retry_delay(offset);
            retries = register_retry(
                context,
                &descriptor.digest,
                retries,
                "registry rejected the resumable range",
                delay,
            )?;
            context
                .store
                .reset_partial(&descriptor.digest, expected_size)
                .await?;
            reset_download_progress(context, &descriptor.digest, expected_size, &mut offset);
            reset_checkpoint_tracking(&mut bytes_since_checkpoint, &mut last_checkpoint);
            tokio::time::sleep(delay).await;
            continue;
        }
        validate_blob_response_status(status, offset, &descriptor.digest)?;

        let mut file = OpenOptions::new()
            .create(true)
            .append(offset > 0)
            .write(true)
            .truncate(offset == 0)
            .open(&plan.partial_path)
            .await?;
        let mut stream = response.bytes_stream();
        let mut stream_failed = false;

        while let Some(chunk) = stream.next().await {
            match chunk {
                Ok(chunk) => {
                    file.write_all(&chunk).await?;
                    offset += chunk.len() as u64;
                    bytes_since_checkpoint += chunk.len() as u64;
                    context
                        .ui
                        .advance_layer_download(&descriptor.digest, chunk.len() as u64);
                    if bytes_since_checkpoint >= CHECKPOINT_BYTES
                        || last_checkpoint.elapsed() >= CHECKPOINT_INTERVAL
                    {
                        file.flush().await?;
                        file.sync_data().await?;
                        context.store.checkpoint_download(
                            &descriptor.digest,
                            offset,
                            expected_size,
                        )?;
                        bytes_since_checkpoint = 0;
                        last_checkpoint = Instant::now();
                    }
                }
                Err(error) => {
                    let delay = retry_delay(offset);
                    retries = register_retry(
                        context,
                        &descriptor.digest,
                        retries,
                        format!("download interrupted: {error}"),
                        delay,
                    )?;
                    warn!("stream error for {}: {}", descriptor.digest, error);
                    file.flush().await?;
                    file.sync_data().await?;
                    context
                        .store
                        .checkpoint_download(&descriptor.digest, offset, expected_size)?;
                    tokio::time::sleep(delay).await;
                    stream_failed = true;
                    break;
                }
            }
        }

        if stream_failed {
            continue;
        }

        file.flush().await?;
        file.sync_data().await?;
        context
            .store
            .checkpoint_download(&descriptor.digest, offset, expected_size)?;
    }

    context
        .ui
        .set_layer_status(&descriptor.digest, "Verifying checksum");
    context.store.finalize_download(&descriptor).await?;
    if let Ok(partial) = context.store.partial_path(&descriptor.digest)
        && partial.exists()
    {
        let _ = fs::remove_file(partial).await;
    }
    context.ui.finish_layer_download(&descriptor.digest);
    Ok(())
}

fn validate_blob_response_status(status: StatusCode, offset: u64, digest: &str) -> Result<()> {
    if status == StatusCode::NOT_FOUND {
        return Err(DockerPullError::BlobNotFound(digest.to_string()));
    }

    if offset == 0 && status == StatusCode::OK {
        return Ok(());
    }

    if offset > 0 && status == StatusCode::PARTIAL_CONTENT {
        return Ok(());
    }

    Err(DockerPullError::BadResponse(format!(
        "unexpected blob response status {status} for {digest} at offset {offset}"
    )))
}

async fn reset_download_state(
    context: &PullContext,
    normalized_reference: &str,
    descriptor: &Descriptor,
    expected_size: u64,
    plan: &mut DownloadPlan,
    offset: &mut u64,
) -> Result<()> {
    context
        .store
        .reset_partial(&descriptor.digest, expected_size)
        .await?;
    *plan = context
        .store
        .prepare_download(normalized_reference, descriptor, expected_size)
        .await?;
    reset_download_progress(context, &descriptor.digest, expected_size, offset);
    Ok(())
}

fn reset_download_progress(
    context: &PullContext,
    digest: &str,
    expected_size: u64,
    offset: &mut u64,
) {
    *offset = 0;
    context
        .ui
        .start_layer_download(digest, expected_size, *offset);
}

fn reset_checkpoint_tracking(bytes_since_checkpoint: &mut u64, last_checkpoint: &mut Instant) {
    *bytes_since_checkpoint = 0;
    *last_checkpoint = Instant::now();
}

fn retry_delay(offset: u64) -> Duration {
    if offset == 0 {
        Duration::from_secs(2)
    } else {
        Duration::from_secs(1)
    }
}

fn register_retry(
    context: &PullContext,
    digest: &str,
    retries: u32,
    detail: impl Into<String>,
    delay: Duration,
) -> Result<u32> {
    let detail = detail.into();
    let next_retry = retries + 1;
    if context
        .blob_retry_limit
        .is_some_and(|limit| next_retry > limit)
    {
        return Err(DockerPullError::RetryLimitExceeded {
            operation: format!("blob download {digest}"),
            retries,
            detail,
        });
    }

    let retry_budget = match context.blob_retry_limit {
        Some(limit) => format!("{next_retry}/{limit}"),
        None => format!("{next_retry}/unlimited"),
    };
    context.ui.warn(format!(
        "{detail} for {digest}; retrying in {:?} ({retry_budget})",
        delay
    ));
    Ok(next_retry)
}

#[cfg(test)]
mod tests {
    use reqwest::StatusCode;

    use super::validate_blob_response_status;
    use crate::error::DockerPullError;

    #[test]
    fn missing_blob_status_maps_to_not_found() {
        let error = validate_blob_response_status(StatusCode::NOT_FOUND, 0, "sha256:deadbeef")
            .expect_err("404 should not be treated as a streamable blob response");

        assert!(
            matches!(error, DockerPullError::BlobNotFound(digest) if digest == "sha256:deadbeef")
        );
    }

    #[test]
    fn resumed_blob_download_requires_partial_content() {
        let error = validate_blob_response_status(StatusCode::OK, 42, "sha256:deadbeef")
            .expect_err("resumed downloads must not accept 200 responses without reset handling");

        assert!(matches!(error, DockerPullError::BadResponse(_)));
    }

    #[test]
    fn fresh_blob_download_accepts_ok_response() {
        validate_blob_response_status(StatusCode::OK, 0, "sha256:deadbeef")
            .expect("fresh downloads should accept 200 responses");
    }

    #[test]
    fn resumed_blob_download_accepts_partial_content() {
        validate_blob_response_status(StatusCode::PARTIAL_CONTENT, 42, "sha256:deadbeef")
            .expect("resumed downloads should accept 206 responses");
    }
}
