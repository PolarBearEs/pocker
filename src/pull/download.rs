use std::sync::Arc;
use std::time::{Duration, Instant};

use futures_util::StreamExt;
use reqwest::StatusCode;
use reqwest::header::{CONTENT_RANGE, HeaderMap};
use tokio::fs::{self, OpenOptions};
use tokio::io::AsyncWriteExt;
use tracing::warn;

use crate::error::{DockerPullError, Result};
use crate::pull::PullContext;
use crate::reference::ImageReference;
use crate::registry::Descriptor;
use crate::retry::{
    countdown_sleep, format_retry_delay, jittered_backoff_delay, record_retry_attempt,
};
use crate::store::DownloadPlan;

// Persist partial-download progress often enough that cancellation or flaky
// links do not lose much work, without syncing every small network chunk.
const CHECKPOINT_BYTES: u64 = 8 * 1024 * 1024;
// Time-based checkpointing protects very slow links that may take a long time
// to reach the byte threshold while still making legitimate progress.
const CHECKPOINT_INTERVAL: Duration = Duration::from_secs(2);

pub async fn download_blob(
    context: &PullContext,
    reference: &ImageReference,
    descriptor: Descriptor,
) -> Result<()> {
    let _blob_guard = context.blob_locks.lock(&descriptor.digest).await;
    let _file_guard = context
        .store
        .acquire_blob_download_lock_with_wait_notice(&descriptor.digest, &context.stop, || {
            context
                .ui
                .set_layer_status(&descriptor.digest, "Waiting for another pocker process");
        })
        .await?;
    let expected_size = descriptor.expected_size()?;

    if context
        .store
        .ensure_blob_complete(&descriptor.digest, expected_size)
        .await?
    {
        context.ui.mark_layer_cached(&descriptor.digest);
        return Ok(());
    }

    // OCI descriptors carry the authoritative blob size, so avoid a pre-flight
    // HEAD request and let the first ranged GET surface missing blobs.
    let mut plan = context
        .store
        .prepare_download(&descriptor, expected_size)
        .await?;
    let mut progress = DownloadProgress::new(plan.durable_offset);
    context
        .ui
        .start_layer_download(&descriptor.digest, expected_size, progress.offset);
    let mut retries = 0_u32;
    let retry_status_sink: Arc<dyn Fn(String) + Send + Sync> = {
        let digest = descriptor.digest.clone();
        let ui = Arc::clone(&context.ui);
        Arc::new(move |status| ui.set_layer_status(&digest, &status))
    };

    loop {
        if context.stop.is_cancelled() {
            return Err(DockerPullError::Interrupted);
        }
        if progress.offset >= expected_size {
            break;
        }

        let response = tokio::select! {
            result = context.registry.get_blob_with_retry_status(
                reference,
                &descriptor.digest,
                progress.offset,
                Some(Arc::clone(&retry_status_sink)),
            ) => result?,
            _ = context.stop.cancelled() => return Err(DockerPullError::Interrupted),
        };
        let status = response.status();
        if status == StatusCode::OK && progress.offset > 0 {
            let delay = jittered_backoff_delay(retries);
            let retry_budget = register_retry(
                context,
                &descriptor.digest,
                retries,
                "registry ignored ranged resume",
                delay,
            )?;
            retries = retry_budget.retries;
            warn!(
                "registry ignored range request for {}, restarting blob",
                descriptor.digest
            );
            reset_download_state(
                context,
                &descriptor,
                expected_size,
                &mut plan,
                &mut progress,
            )
            .await?;
            sleep_or_interrupt(context, &descriptor.digest, &retry_budget.budget, delay).await?;
            continue;
        }
        if status == StatusCode::RANGE_NOT_SATISFIABLE {
            let delay = jittered_backoff_delay(retries);
            let retry_budget = register_retry(
                context,
                &descriptor.digest,
                retries,
                "registry rejected the resumable range",
                delay,
            )?;
            retries = retry_budget.retries;
            context
                .store
                .reset_partial(&descriptor.digest, expected_size)
                .await?;
            progress.reset(context, &descriptor.digest, expected_size);
            sleep_or_interrupt(context, &descriptor.digest, &retry_budget.budget, delay).await?;
            continue;
        }
        validate_blob_response_status(
            status,
            response.headers(),
            progress.offset,
            &descriptor.digest,
        )?;

        let mut file = OpenOptions::new()
            .create(true)
            .append(progress.offset > 0)
            .write(true)
            .truncate(progress.offset == 0)
            .open(&plan.partial_path)
            .await?;
        let mut stream = response.bytes_stream();
        let mut stream_failed = false;

        loop {
            let chunk = tokio::select! {
                chunk = stream.next() => chunk,
                _ = context.stop.cancelled() => return Err(DockerPullError::Interrupted),
            };
            let Some(chunk) = chunk else {
                break;
            };
            match chunk {
                Ok(chunk) => {
                    file.write_all(&chunk).await?;
                    progress.advance(chunk.len() as u64);
                    context
                        .ui
                        .advance_layer_download(&descriptor.digest, chunk.len() as u64);
                    if progress.should_checkpoint() {
                        file.flush().await?;
                        file.sync_data().await?;
                        progress
                            .checkpoint(context, &descriptor.digest, expected_size)
                            .await?;
                    }
                }
                Err(error) => {
                    let delay = jittered_backoff_delay(retries);
                    let retry_budget = register_retry(
                        context,
                        &descriptor.digest,
                        retries,
                        format!("download interrupted: {error}"),
                        delay,
                    )?;
                    retries = retry_budget.retries;
                    warn!("stream error for {}: {}", descriptor.digest, error);
                    file.flush().await?;
                    file.sync_data().await?;
                    context
                        .store
                        .checkpoint_download(&descriptor.digest, progress.offset, expected_size)
                        .await?;
                    sleep_or_interrupt(context, &descriptor.digest, &retry_budget.budget, delay)
                        .await?;
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
        progress
            .checkpoint(context, &descriptor.digest, expected_size)
            .await?;
        if let Some(detail) = premature_eof_detail(progress.offset, expected_size) {
            let delay = jittered_backoff_delay(retries);
            let retry_budget = register_retry(context, &descriptor.digest, retries, detail, delay)?;
            retries = retry_budget.retries;
            warn!(
                "stream ended early for {} at byte {} of {}",
                descriptor.digest, progress.offset, expected_size
            );
            sleep_or_interrupt(context, &descriptor.digest, &retry_budget.budget, delay).await?;
        }
    }

    context
        .ui
        .set_layer_status(&descriptor.digest, "Verifying checksum");
    context.store.finalize_download(&descriptor).await?;
    if let Ok(partial) = context.store.partial_path(&descriptor.digest) {
        match fs::remove_file(partial).await {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => {}
        }
    }
    context.ui.finish_layer_download(&descriptor.digest);
    Ok(())
}

async fn sleep_or_interrupt(
    context: &PullContext,
    digest: &str,
    retry_budget: &str,
    delay: Duration,
) -> Result<()> {
    countdown_sleep(
        delay,
        |sleep_for| sleep_or_interrupt_on_token(&context.stop, sleep_for),
        |remaining| {
            context.ui.set_layer_status(
                digest,
                &format!(
                    "Retrying in {} ({retry_budget})",
                    format_retry_delay(remaining)
                ),
            );
        },
    )
    .await
}

async fn sleep_or_interrupt_on_token(
    stop: &tokio_util::sync::CancellationToken,
    delay: Duration,
) -> Result<()> {
    tokio::select! {
        _ = tokio::time::sleep(delay) => Ok(()),
        _ = stop.cancelled() => Err(DockerPullError::Interrupted),
    }
}

struct DownloadProgress {
    offset: u64,
    bytes_since_checkpoint: u64,
    last_checkpoint: Instant,
}

impl DownloadProgress {
    fn new(offset: u64) -> Self {
        Self {
            offset,
            bytes_since_checkpoint: 0,
            last_checkpoint: Instant::now(),
        }
    }

    fn advance(&mut self, bytes: u64) {
        self.offset += bytes;
        self.bytes_since_checkpoint += bytes;
    }

    fn should_checkpoint(&self) -> bool {
        self.bytes_since_checkpoint >= CHECKPOINT_BYTES
            || self.last_checkpoint.elapsed() >= CHECKPOINT_INTERVAL
    }

    async fn checkpoint(
        &mut self,
        context: &PullContext,
        digest: &str,
        expected_size: u64,
    ) -> Result<()> {
        context
            .store
            .checkpoint_download(digest, self.offset, expected_size)
            .await?;
        self.bytes_since_checkpoint = 0;
        self.last_checkpoint = Instant::now();
        Ok(())
    }

    fn reset(&mut self, context: &PullContext, digest: &str, expected_size: u64) {
        self.offset = 0;
        self.bytes_since_checkpoint = 0;
        self.last_checkpoint = Instant::now();
        context
            .ui
            .start_layer_download(digest, expected_size, self.offset);
    }
}

fn premature_eof_detail(offset: u64, expected_size: u64) -> Option<String> {
    (offset < expected_size)
        .then(|| format!("download ended at byte {offset} before expected size {expected_size}"))
}

fn validate_blob_response_status(
    status: StatusCode,
    headers: &HeaderMap,
    offset: u64,
    digest: &str,
) -> Result<()> {
    if status == StatusCode::NOT_FOUND {
        return Err(DockerPullError::BlobNotFound(digest.to_string()));
    }

    if offset == 0 && status == StatusCode::OK {
        return Ok(());
    }

    if offset > 0 && status == StatusCode::PARTIAL_CONTENT {
        let Some(start) = headers
            .get(CONTENT_RANGE)
            .and_then(|value| value.to_str().ok())
            .and_then(content_range_start)
        else {
            return Err(DockerPullError::BadResponse(format!(
                "missing or invalid Content-Range for {digest} at offset {offset}"
            )));
        };
        if start == offset {
            return Ok(());
        }
        return Err(DockerPullError::BadResponse(format!(
            "unexpected Content-Range start {start} for {digest} at offset {offset}"
        )));
    }

    Err(DockerPullError::BadResponse(format!(
        "unexpected blob response status {status} for {digest} at offset {offset}"
    )))
}

fn content_range_start(value: &str) -> Option<u64> {
    let value = value.strip_prefix("bytes ")?;
    let (range, _) = value.split_once('/')?;
    let (start, end) = range.split_once('-')?;
    let start = start.parse::<u64>().ok()?;
    let end = end.parse::<u64>().ok()?;
    (start <= end).then_some(start)
}

async fn reset_download_state(
    context: &PullContext,
    descriptor: &Descriptor,
    expected_size: u64,
    plan: &mut DownloadPlan,
    progress: &mut DownloadProgress,
) -> Result<()> {
    context
        .store
        .reset_partial(&descriptor.digest, expected_size)
        .await?;
    *plan = context
        .store
        .prepare_download(descriptor, expected_size)
        .await?;
    progress.reset(context, &descriptor.digest, expected_size);
    Ok(())
}

struct RegisteredRetry {
    retries: u32,
    budget: String,
}

fn register_retry(
    context: &PullContext,
    digest: &str,
    retries: u32,
    detail: impl Into<String>,
    delay: Duration,
) -> Result<RegisteredRetry> {
    let mut retries = retries;
    let retry_budget = record_retry_attempt(
        &mut retries,
        context.blob_retry_limit,
        format!("blob download {digest}"),
        detail.into(),
    )?;
    context.ui.set_layer_status(
        digest,
        &format!("Retrying in {} ({retry_budget})", format_retry_delay(delay)),
    );
    Ok(RegisteredRetry {
        retries,
        budget: retry_budget,
    })
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::{Duration, Instant};

    use reqwest::StatusCode;
    use reqwest::header::{CONTENT_RANGE, HeaderMap, HeaderValue};
    use tempfile::tempdir;

    use super::{
        DownloadProgress, download_blob, premature_eof_detail, sleep_or_interrupt_on_token,
        validate_blob_response_status,
    };
    use crate::auth::AuthResolver;
    use crate::error::DockerPullError;
    use crate::pull::{BlobDownloadLocks, CurrentPullLayers, PullContext};
    use crate::reference::ImageReference;
    use crate::registry::{Descriptor, RegistryClient};
    use crate::store::Store;
    use crate::ui::ProgressSink;

    #[test]
    fn missing_blob_status_maps_to_not_found() {
        let headers = HeaderMap::new();
        let error =
            validate_blob_response_status(StatusCode::NOT_FOUND, &headers, 0, "sha256:deadbeef")
                .expect_err("404 should not be treated as a streamable blob response");

        assert!(
            matches!(error, DockerPullError::BlobNotFound(digest) if digest == "sha256:deadbeef")
        );
    }

    #[test]
    fn resumed_blob_download_requires_partial_content() {
        let headers = HeaderMap::new();
        let error = validate_blob_response_status(StatusCode::OK, &headers, 42, "sha256:deadbeef")
            .expect_err("resumed downloads must not accept 200 responses without reset handling");

        assert!(matches!(error, DockerPullError::BadResponse(_)));
    }

    #[test]
    fn resumed_blob_download_rejects_missing_content_range() {
        let headers = HeaderMap::new();
        let error = validate_blob_response_status(
            StatusCode::PARTIAL_CONTENT,
            &headers,
            42,
            "sha256:deadbeef",
        )
        .expect_err("resumed downloads must require Content-Range");

        assert!(matches!(error, DockerPullError::BadResponse(_)));
    }

    #[test]
    fn resumed_blob_download_rejects_mismatched_content_range() {
        let mut headers = HeaderMap::new();
        headers.insert(CONTENT_RANGE, HeaderValue::from_static("bytes 40-99/100"));
        let error = validate_blob_response_status(
            StatusCode::PARTIAL_CONTENT,
            &headers,
            42,
            "sha256:deadbeef",
        )
        .expect_err("resumed downloads must validate Content-Range start");

        assert!(matches!(error, DockerPullError::BadResponse(_)));
    }

    #[test]
    fn fresh_blob_download_accepts_ok_response() {
        let headers = HeaderMap::new();
        validate_blob_response_status(StatusCode::OK, &headers, 0, "sha256:deadbeef")
            .expect("fresh downloads should accept 200 responses");
    }

    #[test]
    fn resumed_blob_download_accepts_partial_content() {
        let mut headers = HeaderMap::new();
        headers.insert(CONTENT_RANGE, HeaderValue::from_static("bytes 42-99/100"));
        validate_blob_response_status(StatusCode::PARTIAL_CONTENT, &headers, 42, "sha256:deadbeef")
            .expect("resumed downloads should accept matching 206 responses");
    }

    #[test]
    fn premature_eof_registers_retry_detail_when_short() {
        assert_eq!(
            premature_eof_detail(42, 100).as_deref(),
            Some("download ended at byte 42 before expected size 100")
        );
        assert!(premature_eof_detail(100, 100).is_none());
    }

    #[test]
    fn download_progress_tracks_offsets_and_checkpoints() {
        let mut progress = DownloadProgress::new(10);

        progress.advance(5);

        assert_eq!(progress.offset, 15);
        assert_eq!(progress.bytes_since_checkpoint, 5);
        assert!(!progress.should_checkpoint());
    }

    #[test]
    fn download_progress_checkpoints_after_interval_for_slow_streams() {
        let mut progress = DownloadProgress::new(10);
        progress.last_checkpoint = Instant::now() - Duration::from_secs(3);

        progress.advance(1);

        assert!(progress.should_checkpoint());
    }

    #[tokio::test]
    async fn retry_sleep_stops_when_cancelled() {
        let stop = tokio_util::sync::CancellationToken::new();
        stop.cancel();

        let error = sleep_or_interrupt_on_token(&stop, Duration::from_secs(60))
            .await
            .expect_err("cancelled retry sleep should return immediately");

        assert!(matches!(error, DockerPullError::Interrupted));
    }

    #[tokio::test]
    async fn download_blob_marks_layer_cached_after_waiting_for_other_process() {
        let dir = tempdir().expect("tempdir should create");
        let store = Arc::new(
            Store::open(dir.path().to_path_buf())
                .await
                .expect("store should open"),
        );
        let bytes = b"shared layer";
        let descriptor = Descriptor {
            media_type: "application/octet-stream".into(),
            digest: crate::digest::canonical_digest_bytes(bytes),
            size: bytes.len() as i64,
            platform: None,
            annotations: None,
        };
        let stop = tokio_util::sync::CancellationToken::new();
        let file_lock = store
            .acquire_blob_download_lock(&descriptor.digest, &stop)
            .await
            .expect("first process lock should acquire");
        let ui = Arc::new(RecordingProgress::default());
        let context = PullContext {
            store: Arc::clone(&store),
            registry: Arc::new(RegistryClient::new(
                reqwest::Client::new(),
                Arc::new(AuthResolver::new(None).expect("auth resolver should create")),
                true,
                Some(0),
            )),
            stop,
            ui: ui.clone(),
            blob_retry_limit: Some(0),
            blob_locks: Arc::new(BlobDownloadLocks::default()),
            layer_usage: Arc::new(CurrentPullLayers::default()),
            daemon_layer_cache: None,
        };
        let reference = ImageReference::parse("example.com/library/test:latest")
            .expect("reference should parse");

        let task = tokio::spawn({
            let context = context.clone();
            let reference = reference.clone();
            let descriptor = descriptor.clone();
            async move { download_blob(&context, &reference, descriptor).await }
        });
        tokio::time::timeout(Duration::from_secs(2), async {
            while !ui.waiting.load(Ordering::SeqCst) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("download should report lock wait");

        store
            .save_blob_bytes(&descriptor, bytes)
            .await
            .expect("other process should complete blob");
        drop(file_lock);

        task.await
            .expect("download task should not panic")
            .expect("download should reuse completed blob");
        assert!(
            ui.cached.load(Ordering::SeqCst),
            "layer should be marked cached after waiting for another process"
        );
    }

    #[derive(Default)]
    struct RecordingProgress {
        waiting: AtomicBool,
        cached: AtomicBool,
    }

    impl ProgressSink for RecordingProgress {
        fn begin_image(&self, _image: &str) {}
        fn begin_load(&self, _image: &str) {}
        fn set_image_status(&self, _image: &str, _status: &str) {}
        fn finish_image(&self, _image: &str, _status: &str) {}
        fn prepare_layers(&self, _digests: &[String]) {}
        fn mark_layer_cached(&self, _digest: &str) {
            self.cached.store(true, Ordering::SeqCst);
        }
        fn mark_layer_daemon(&self, _digest: &str) {}
        fn start_layer_download(&self, _digest: &str, _total_bytes: u64, _starting_offset: u64) {}
        fn advance_layer_download(&self, _digest: &str, _amount: u64) {}
        fn finish_layer_download(&self, _digest: &str) {}
        fn set_layer_status(&self, _digest: &str, status: &str) {
            if status == "Waiting for another pocker process" {
                self.waiting.store(true, Ordering::SeqCst);
            }
        }
        fn warn(&self, _message: &str) {}
    }
}
