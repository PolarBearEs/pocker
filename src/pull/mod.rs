pub mod download;
mod load;
pub(crate) mod orchestrator;

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;

use futures_util::stream::{self, FuturesUnordered, StreamExt};
use tokio::sync::{Mutex as AsyncMutex, OwnedMutexGuard};
use tokio_util::sync::CancellationToken;
use tracing::warn;

use crate::docker;
use crate::error::{DockerPullError, Result};
use crate::image::pair_layers;
use crate::platform::Platform;
use crate::reference::ImageReference;
use crate::registry::{Descriptor, RegistryClient};
use crate::store::{Store, StoredReference};
use crate::ui::ProgressSink;

pub const DEFAULT_BLOB_RETRIES: u32 = 8;

#[derive(Clone)]
pub struct PullContext {
    pub store: Arc<Store>,
    pub registry: Arc<RegistryClient>,
    pub stop: CancellationToken,
    pub ui: Arc<dyn ProgressSink>,
    pub blob_retry_limit: Option<u32>,
    pub blob_locks: Arc<BlobDownloadLocks>,
    pub daemon_layer_cache: Option<Arc<docker::DaemonLayerCache>>,
}

#[derive(Debug, Clone)]
pub struct PullOptions {
    pub platform: Platform,
    pub concurrency: usize,
    pub no_load: bool,
    pub keep_layer_blobs: bool,
    pub load_mode: LoadMode,
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum LoadMode {
    Stream,
    Registry,
}

#[derive(Clone)]
pub struct Puller {
    context: PullContext,
}

#[derive(Default)]
pub struct BlobDownloadLocks {
    locks: Mutex<HashMap<String, Arc<AsyncMutex<()>>>>,
}

pub struct BlobDownloadGuard<'a> {
    locks: &'a BlobDownloadLocks,
    digest: String,
    lock: Arc<AsyncMutex<()>>,
    guard: Option<OwnedMutexGuard<()>>,
}

impl BlobDownloadLocks {
    pub async fn lock(&self, digest: &str) -> BlobDownloadGuard<'_> {
        let lock = {
            let mut locks = self.locks.lock().expect("blob lock state poisoned");
            locks
                .entry(digest.to_string())
                .or_insert_with(|| Arc::new(AsyncMutex::new(())))
                .clone()
        };
        let guard = lock.clone().lock_owned().await;
        BlobDownloadGuard {
            locks: self,
            digest: digest.to_string(),
            lock,
            guard: Some(guard),
        }
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.locks.lock().expect("blob lock state poisoned").len()
    }
}

impl Drop for BlobDownloadGuard<'_> {
    fn drop(&mut self) {
        drop(self.guard.take());

        let mut locks = self.locks.locks.lock().expect("blob lock state poisoned");
        let should_remove = locks
            .get(&self.digest)
            .is_some_and(|lock| Arc::ptr_eq(lock, &self.lock) && Arc::strong_count(lock) == 2);
        if should_remove {
            locks.remove(&self.digest);
        }
    }
}

impl Puller {
    pub fn new(context: PullContext) -> Self {
        Self { context }
    }

    pub async fn pull(&self, reference: ImageReference, options: PullOptions) -> Result<()> {
        if self.context.stop.is_cancelled() {
            return Err(DockerPullError::Interrupted);
        }

        let normalized = reference.normalized();
        self.context.ui.begin_image(&normalized);

        let resolved = self
            .context
            .registry
            .resolve_image(&reference, &options.platform)
            .await?;
        self.context
            .store
            .save_blob_bytes(&resolved.manifest, &resolved.manifest_bytes)
            .await?;
        let config_bytes = load_blob_bytes(&self.context, &reference, &resolved.config).await?;

        let stored_reference = StoredReference {
            reference: normalized.clone(),
            manifest: resolved.manifest.clone(),
            config_digest: resolved.config.digest.clone(),
        };

        if !options.no_load
            && load::finalize_existing_reference(
                &self.context,
                &reference,
                &stored_reference,
                &options,
            )
            .await?
        {
            return Ok(());
        }

        let layers = pair_layers(resolved.layers.clone(), &config_bytes)?;
        let layer_digests = layers
            .iter()
            .map(|layer| layer.descriptor.digest.clone())
            .collect::<Vec<_>>();
        self.context.ui.prepare_layers(&layer_digests);
        let daemon_layers = if options.load_mode == LoadMode::Stream {
            let coverage = match &self.context.daemon_layer_cache {
                Some(cache) => cache.coverage(&layers).await,
                None => docker::daemon_layer_coverage(&layers).await,
            };
            match coverage {
                Ok(coverage) => coverage,
                Err(error) => {
                    warn!("failed to inspect Docker daemon layer coverage: {error}");
                    let warning = format!(
                        "could not inspect existing Docker layers; downloading all missing cache layers: {error}"
                    );
                    self.context.ui.warn(&warning);
                    Default::default()
                }
            }
        } else {
            Default::default()
        };
        let mut downloads = Vec::new();
        let mut cache_checks = stream::iter(layers)
            .map(|layer| {
                let store = Arc::clone(&self.context.store);
                async move {
                    let cached = store
                        .ensure_blob_complete(
                            &layer.descriptor.digest,
                            layer.descriptor.expected_size()?,
                        )
                        .await?;
                    Result::Ok((layer, cached))
                }
            })
            .buffer_unordered(options.concurrency.max(1));

        while let Some(result) = cache_checks.next().await {
            let (layer, cached) = result?;
            if cached {
                self.context.ui.mark_layer_cached(&layer.descriptor.digest);
                continue;
            }
            if daemon_layers.contains_key(&layer.diff_id) {
                self.context.ui.mark_layer_daemon(&layer.descriptor.digest);
                continue;
            }
            downloads.push(layer.descriptor);
        }
        let mut queue = FuturesUnordered::new();

        for descriptor in downloads {
            while queue.len() >= options.concurrency {
                if let Some(result) = queue.next().await {
                    handle_download_result(result, &mut queue).await?;
                }
            }
            let context = self.context.clone();
            let reference = reference.clone();
            queue.push(tokio::spawn(async move {
                download::download_blob(&context, &reference, descriptor).await
            }));
        }

        while let Some(result) = queue.next().await {
            handle_download_result(result, &mut queue).await?;
        }

        self.context.store.save_reference(&stored_reference).await?;

        if options.no_load {
            self.context.ui.finish_image(&normalized, "Pulled");
        } else {
            load::load_reference(&self.context, &reference, &stored_reference, &options).await?;
        }

        Ok(())
    }
}

async fn handle_download_result(
    result: std::result::Result<Result<()>, tokio::task::JoinError>,
    queue: &mut FuturesUnordered<tokio::task::JoinHandle<Result<()>>>,
) -> Result<()> {
    match result {
        Ok(Ok(())) => Ok(()),
        Ok(Err(error)) => {
            abort_downloads(queue).await;
            Err(error)
        }
        Err(error) => {
            abort_downloads(queue).await;
            Err(DockerPullError::InvalidInput(format!(
                "download worker failed: {error}"
            )))
        }
    }
}

async fn abort_downloads(queue: &mut FuturesUnordered<tokio::task::JoinHandle<Result<()>>>) {
    for task in queue.iter() {
        task.abort();
    }
    while let Some(result) = queue.next().await {
        if let Err(error) = result
            && !error.is_cancelled()
        {
            warn!("download worker failed while aborting: {error}");
        }
    }
}

async fn load_blob_bytes(
    context: &PullContext,
    reference: &ImageReference,
    descriptor: &Descriptor,
) -> Result<Vec<u8>> {
    if let Some(bytes) = context
        .store
        .read_blob_bytes_if_complete(descriptor)
        .await?
    {
        return Ok(bytes);
    }

    let bytes = context
        .registry
        .get_blob_bytes(reference, &descriptor.digest)
        .await?;
    context.store.save_blob_bytes(descriptor, &bytes).await?;
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::BlobDownloadLocks;

    #[tokio::test]
    async fn blob_download_locks_remove_idle_entries() {
        let locks = BlobDownloadLocks::default();

        let guard = locks.lock("sha256:abc").await;
        assert_eq!(locks.len(), 1);

        drop(guard);
        assert_eq!(locks.len(), 0);
    }

    #[tokio::test]
    async fn blob_download_locks_keep_entries_for_waiters() {
        let locks = std::sync::Arc::new(BlobDownloadLocks::default());
        let guard = locks.lock("sha256:abc").await;
        let waiter_locks = std::sync::Arc::clone(&locks);

        let waiter = tokio::spawn(async move {
            let _guard = waiter_locks.lock("sha256:abc").await;
        });
        tokio::task::yield_now().await;

        drop(guard);
        assert_eq!(locks.len(), 1);

        waiter.await.expect("waiter task should finish");
        assert_eq!(locks.len(), 0);
    }
}
