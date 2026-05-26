pub mod download;
mod load;
pub(crate) mod orchestrator;

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::Mutex;

use futures_util::stream::{self, FuturesUnordered, StreamExt};
use tokio::sync::{Mutex as AsyncMutex, OwnedMutexGuard};
use tokio_util::sync::CancellationToken;
use tracing::warn;

use crate::docker;
use crate::error::{DockerPullError, Result};
use crate::image::pair_layers;
use crate::reference::ImageReference;
use crate::registry::{Descriptor, RegistryClient, ResolvedImage};
use crate::store::{Store, StoredReference};
use crate::ui::ProgressSink;

// Blob retries are higher than metadata request retries because large layer
// transfers are the most likely operation to hit transient slow-network stalls.
// Retries resume from durable partial files rather than restarting from zero.
pub const DEFAULT_BLOB_RETRIES: u32 = 8;

#[derive(Clone)]
pub struct PullContext {
    pub store: Arc<Store>,
    pub registry: Arc<RegistryClient>,
    pub stop: CancellationToken,
    pub ui: Arc<dyn ProgressSink>,
    pub blob_retry_limit: Option<u32>,
    pub blob_locks: Arc<BlobDownloadLocks>,
    pub layer_usage: Arc<CurrentPullLayers>,
    pub daemon_layer_cache: Option<Arc<docker::DaemonLayerCache>>,
}

#[derive(Debug, Clone)]
pub struct PullOptions {
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

#[derive(Default)]
pub struct CurrentPullLayers {
    state: Mutex<CurrentPullLayerState>,
}

#[derive(Default)]
struct CurrentPullLayerState {
    planned_remaining: HashMap<String, usize>,
    active_unplanned: HashMap<String, usize>,
    planned: bool,
}

pub struct BlobDownloadGuard<'a> {
    locks: &'a BlobDownloadLocks,
    digest: String,
    lock: Arc<AsyncMutex<()>>,
    guard: Option<OwnedMutexGuard<()>>,
}

pub struct LayerClaimGuard<'a> {
    usage: &'a CurrentPullLayers,
    planned_digests: Vec<String>,
    unplanned_digests: Vec<String>,
}

impl BlobDownloadLocks {
    pub async fn lock(&self, digest: &str) -> BlobDownloadGuard<'_> {
        // Keep one mutex per digest while a download is active or another task
        // is waiting for it, so concurrent pulls deduplicate the same blob
        // without leaking entries after the final guard is gone.
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

impl CurrentPullLayers {
    pub fn from_image_layers(images: &[Vec<String>]) -> Self {
        let mut planned_remaining = HashMap::new();
        for digests in images {
            for digest in unique_digests(digests) {
                *planned_remaining.entry(digest).or_default() += 1;
            }
        }
        Self {
            state: Mutex::new(CurrentPullLayerState {
                planned_remaining,
                active_unplanned: HashMap::new(),
                planned: true,
            }),
        }
    }

    pub fn claim(&self, digests: &[String]) -> LayerClaimGuard<'_> {
        let digests = unique_digests(digests);
        let mut state = self.state.lock().expect("layer usage state poisoned");
        let mut planned_digests = Vec::new();
        let mut unplanned_digests = Vec::new();
        for digest in digests {
            if state.planned && state.planned_remaining.contains_key(&digest) {
                planned_digests.push(digest);
            } else {
                *state.active_unplanned.entry(digest.clone()).or_default() += 1;
                unplanned_digests.push(digest);
            }
        }
        LayerClaimGuard {
            usage: self,
            planned_digests,
            unplanned_digests,
        }
    }

    fn protected_digests(&self, digests: &[String]) -> HashSet<String> {
        let state = self.state.lock().expect("layer usage state poisoned");
        digests
            .iter()
            .filter(|digest| {
                let planned = state
                    .planned_remaining
                    .get(*digest)
                    .copied()
                    .unwrap_or_default();
                let unplanned = state
                    .active_unplanned
                    .get(*digest)
                    .copied()
                    .unwrap_or_default();
                planned + unplanned > 1
            })
            .cloned()
            .collect()
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.state.lock().expect("layer usage state poisoned").len()
    }
}

impl LayerClaimGuard<'_> {
    pub fn protected_digests(&self) -> HashSet<String> {
        let mut digests = self.planned_digests.clone();
        digests.extend(self.unplanned_digests.clone());
        self.usage.protected_digests(&digests)
    }
}

impl Drop for LayerClaimGuard<'_> {
    fn drop(&mut self) {
        let mut state = self.usage.state.lock().expect("layer usage state poisoned");
        for digest in &self.planned_digests {
            decrement_count(&mut state.planned_remaining, digest);
        }
        for digest in &self.unplanned_digests {
            decrement_count(&mut state.active_unplanned, digest);
        }
    }
}

impl CurrentPullLayerState {
    #[cfg(test)]
    fn len(&self) -> usize {
        self.planned_remaining.len() + self.active_unplanned.len()
    }
}

fn decrement_count(counts: &mut HashMap<String, usize>, digest: &str) {
    let Some(count) = counts.get_mut(digest) else {
        return;
    };
    *count -= 1;
    if *count == 0 {
        counts.remove(digest);
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

fn unique_digests(digests: &[String]) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut unique = Vec::new();
    for digest in digests {
        if seen.insert(digest.as_str()) {
            unique.push(digest.clone());
        }
    }
    unique
}

impl Puller {
    pub fn new(context: PullContext) -> Self {
        Self { context }
    }

    pub async fn pull_resolved(
        &self,
        reference: ImageReference,
        resolved: ResolvedImage,
        options: PullOptions,
    ) -> Result<()> {
        if self.context.stop.is_cancelled() {
            return Err(DockerPullError::Interrupted);
        }

        let normalized = reference.normalized();
        self.context.ui.begin_image(&normalized);

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

        let layers = pair_layers(resolved.layers.clone(), &config_bytes)?;
        let layer_digests = layers
            .iter()
            .map(|layer| layer.descriptor.digest.clone())
            .collect::<Vec<_>>();

        let layer_claim = self.context.layer_usage.claim(&layer_digests);
        if !options.no_load
            && load::finalize_existing_reference(
                &self.context,
                &reference,
                &stored_reference,
                &options,
                &layer_claim,
            )
            .await?
        {
            return Ok(());
        }

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
            load::load_reference(
                &self.context,
                &reference,
                &stored_reference,
                &options,
                &layer_claim,
            )
            .await?;
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
    use super::{BlobDownloadLocks, CurrentPullLayers};

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

    #[tokio::test]
    async fn blob_download_locks_remove_entries_after_waiter_cancellation() {
        let locks = std::sync::Arc::new(BlobDownloadLocks::default());
        let guard = locks.lock("sha256:abc").await;
        let waiter_locks = std::sync::Arc::clone(&locks);

        let waiter = tokio::spawn(async move {
            let _guard = waiter_locks.lock("sha256:abc").await;
        });
        tokio::task::yield_now().await;

        waiter.abort();
        let result = waiter.await;
        assert!(result.is_err_and(|error| error.is_cancelled()));

        drop(guard);
        assert_eq!(locks.len(), 0);
    }

    #[test]
    fn current_pull_layers_protects_shared_active_claims_until_last_release() {
        let usage = CurrentPullLayers::default();
        let first = vec!["sha256:shared".to_string(), "sha256:first".to_string()];
        let second = vec!["sha256:shared".to_string(), "sha256:second".to_string()];

        let first_claim = usage.claim(&first);
        let second_claim = usage.claim(&second);

        assert!(first_claim.protected_digests().contains("sha256:shared"));
        assert!(!first_claim.protected_digests().contains("sha256:first"));

        drop(first_claim);

        assert!(!second_claim.protected_digests().contains("sha256:shared"));

        drop(second_claim);
        assert_eq!(usage.len(), 0);
    }

    #[test]
    fn current_pull_layers_counts_duplicate_digests_once_per_image() {
        let usage = CurrentPullLayers::default();
        let digests = vec!["sha256:shared".to_string(), "sha256:shared".to_string()];

        let claim = usage.claim(&digests);

        assert!(!claim.protected_digests().contains("sha256:shared"));

        drop(claim);
        assert_eq!(usage.len(), 0);
    }

    #[test]
    fn current_pull_layers_can_preplan_images_before_they_start() {
        let first = vec!["sha256:shared".to_string(), "sha256:first".to_string()];
        let second = vec!["sha256:shared".to_string(), "sha256:second".to_string()];
        let usage = CurrentPullLayers::from_image_layers(&[first.clone(), second.clone()]);

        let first_claim = usage.claim(&first);

        assert!(first_claim.protected_digests().contains("sha256:shared"));

        drop(first_claim);

        let second_claim = usage.claim(&second);
        assert!(!second_claim.protected_digests().contains("sha256:shared"));

        drop(second_claim);
        assert_eq!(usage.len(), 0);
    }

    #[test]
    fn current_pull_layers_tracks_unplanned_layers_in_planned_pulls() {
        let planned = vec!["sha256:planned".to_string()];
        let unexpected = vec!["sha256:unexpected".to_string()];
        let usage = CurrentPullLayers::from_image_layers(std::slice::from_ref(&planned));

        let planned_claim = usage.claim(&planned);
        let first_unexpected_claim = usage.claim(&unexpected);
        let second_unexpected_claim = usage.claim(&unexpected);

        assert!(
            first_unexpected_claim
                .protected_digests()
                .contains("sha256:unexpected")
        );

        drop(first_unexpected_claim);

        assert!(
            !second_unexpected_claim
                .protected_digests()
                .contains("sha256:unexpected")
        );

        drop(second_unexpected_claim);
        drop(planned_claim);
        assert_eq!(usage.len(), 0);
    }
}
