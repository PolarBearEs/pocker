use std::time::{Duration, Instant};

use futures_util::StreamExt;
use tokio::fs::{self, OpenOptions};
use tokio::io::AsyncWriteExt;
use tracing::warn;

use crate::error::{DockerPullError, Result};
use crate::pull::PullContext;
use crate::reference::ImageReference;
use crate::registry::Descriptor;

const CHECKPOINT_BYTES: u64 = 8 * 1024 * 1024;
const CHECKPOINT_INTERVAL: Duration = Duration::from_secs(2);
const MAX_BLOB_RETRIES: u32 = 8;

pub async fn download_blob(
    context: &PullContext,
    reference: &ImageReference,
    normalized_reference: &str,
    descriptor: Descriptor,
) -> Result<()> {
    if context
        .store
        .ensure_blob_complete(&descriptor.digest, descriptor.size)
        .await?
    {
        return Ok(());
    }

    let head = context
        .registry
        .head_blob(reference, &descriptor.digest)
        .await?;
    let expected_size = head.size.unwrap_or(descriptor.size as u64);
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
        if status == reqwest::StatusCode::OK && offset > 0 {
            retries = register_retry(
                context,
                &descriptor.digest,
                retries,
                "registry ignored ranged resume",
                retry_delay(offset),
            )?;
            warn!(
                "registry ignored range request for {}, restarting blob",
                descriptor.digest
            );
            context
                .store
                .reset_partial(&descriptor.digest, expected_size)
                .await?;
            offset = 0;
            plan = context
                .store
                .prepare_download(normalized_reference, &descriptor, expected_size)
                .await?;
            context
                .ui
                .start_layer_download(&descriptor.digest, expected_size, offset);
            tokio::time::sleep(retry_delay(offset)).await;
        }
        if status == reqwest::StatusCode::RANGE_NOT_SATISFIABLE {
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
            offset = 0;
            context
                .ui
                .start_layer_download(&descriptor.digest, expected_size, offset);
            tokio::time::sleep(delay).await;
            continue;
        }

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
    if next_retry > MAX_BLOB_RETRIES {
        return Err(DockerPullError::RetryLimitExceeded {
            operation: format!("blob download {digest}"),
            retries,
            detail,
        });
    }

    context.ui.warn(format!(
        "{detail} for {digest}; retrying in {:?} ({}/{})",
        delay, next_retry, MAX_BLOB_RETRIES
    ));
    Ok(next_retry)
}
