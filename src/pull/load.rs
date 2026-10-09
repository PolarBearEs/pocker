use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use futures_util::stream::{self, StreamExt, TryStreamExt};
use tokio::net::TcpListener;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

use crate::docker;
use crate::error::{DockerPullError, Result};
use crate::reference::{ImageReference, ReferenceTarget};
use crate::registry::{RegistryClient, cache_repository};
use crate::serve_registry::{self, ServeListenerConfig};
use crate::store::{CacheLayerClaimGuard, Store, StoredReference};

use super::{LayerClaimGuard, LoadMode, PullContext, PullOptions};

pub(super) async fn finalize_existing_reference(
    context: &PullContext,
    reference: &ImageReference,
    stored_reference: &StoredReference,
    options: &PullOptions,
    layer_claim: &LayerClaimGuard<'_>,
    cache_layer_claim: &CacheLayerClaimGuard,
) -> Result<bool> {
    let normalized = &stored_reference.reference;
    let already_loaded = docker::daemon_has_reference(reference, stored_reference)
        .await
        .unwrap_or(false);
    if !already_loaded {
        return Ok(false);
    }

    context.ui.set_image_status(normalized, "Already exists");
    context.ui.prepare_layers(&[]);
    context.store.save_reference(stored_reference).await?;
    if !options.keep_layer_blobs {
        context.ui.set_image_status(normalized, "Pruning cache");
        let protected_layer_digests = layer_claim.protected_digests();
        context
            .store
            .prune_reference_layer_blobs_except_claimed(
                stored_reference,
                &protected_layer_digests,
                Some(cache_layer_claim),
                &context.stop,
            )
            .await?;
    }
    context.ui.finish_image(normalized, "Already exists");
    Ok(true)
}

pub(super) async fn load_reference(
    context: &PullContext,
    reference: &ImageReference,
    stored_reference: &StoredReference,
    options: &PullOptions,
    layer_claim: &LayerClaimGuard<'_>,
    cache_layer_claim: &CacheLayerClaimGuard,
) -> Result<()> {
    let normalized = &stored_reference.reference;
    let waited_for_load = Arc::new(AtomicBool::new(false));
    let _load_lock = context
        .store
        .acquire_image_load_lock_with_wait_notice(
            normalized,
            &stored_reference.config_digest,
            &context.stop,
            {
                let ui = Arc::clone(&context.ui);
                let normalized = normalized.clone();
                let waited_for_load = Arc::clone(&waited_for_load);
                move || {
                    waited_for_load.store(true, Ordering::SeqCst);
                    ui.set_image_status(&normalized, "Waiting for another pocker load");
                }
            },
        )
        .await?;

    if docker::daemon_has_reference(reference, stored_reference)
        .await
        .unwrap_or(false)
    {
        let status = if waited_for_load.load(Ordering::SeqCst) {
            "Loaded by another pocker process"
        } else {
            "Already exists"
        };
        context.ui.begin_load(normalized);
        context.ui.set_image_status(normalized, status);
        prune_after_load_if_needed(
            context,
            normalized,
            stored_reference,
            options,
            layer_claim,
            cache_layer_claim,
        )
        .await?;
        context.ui.finish_image(normalized, status);
        return Ok(());
    }

    context.ui.begin_load(normalized);
    match options.load_mode {
        LoadMode::Stream => {
            stream_load_reference(context, reference, stored_reference, options).await?;
        }
        LoadMode::Registry => {
            load_reference_through_cache_registry(context, reference, stored_reference, options)
                .await?;
        }
    }
    prune_after_load_if_needed(
        context,
        normalized,
        stored_reference,
        options,
        layer_claim,
        cache_layer_claim,
    )
    .await?;
    context.ui.finish_image(normalized, "Ready");
    Ok(())
}

async fn prune_after_load_if_needed(
    context: &PullContext,
    normalized: &str,
    stored_reference: &StoredReference,
    options: &PullOptions,
    layer_claim: &LayerClaimGuard<'_>,
    cache_layer_claim: &CacheLayerClaimGuard,
) -> Result<()> {
    if options.keep_layer_blobs {
        return Ok(());
    }

    context.ui.set_image_status(normalized, "Pruning cache");
    let protected_layer_digests = layer_claim.protected_digests();
    context
        .store
        .prune_reference_layer_blobs_except_claimed(
            stored_reference,
            &protected_layer_digests,
            Some(cache_layer_claim),
            &context.stop,
        )
        .await?;
    Ok(())
}

/// Stream-load an image, reusing layers exported from the Docker daemon.
///
/// The pull skipped downloading layers the daemon reported in an image's
/// `RootFS`, but the daemon cannot always export them (containerd may keep
/// only the unpacked snapshot). Any layer it fails to export is downloaded
/// from the registry here rather than failing the whole load.
async fn stream_load_reference(
    context: &PullContext,
    reference: &ImageReference,
    stored_reference: &StoredReference,
    options: &PullOptions,
) -> Result<()> {
    let normalized = &stored_reference.reference;
    let prepared = docker::prepare_reference_archive(&context.store, stored_reference).await?;
    let unresolved = prepared.unresolved_layers();
    if !unresolved.is_empty() {
        context.ui.warn(&format!(
            "Docker could not export {} reused layer(s) for {normalized}; downloading them from the registry",
            unresolved.len()
        ));
        context
            .ui
            .set_image_status(normalized, "Downloading layers");
        let digests = unresolved
            .iter()
            .map(|descriptor| descriptor.digest.clone())
            .collect::<Vec<_>>();
        context.ui.prepare_layers(&digests);
        stream::iter(unresolved)
            .map(|descriptor| super::download::download_blob(context, reference, descriptor))
            .buffer_unordered(options.concurrency.max(1))
            .try_collect::<()>()
            .await?;
        context.ui.begin_load(normalized);
    }
    docker::load_prepared_reference_archive_stream(&context.store, stored_reference, prepared).await
}

async fn load_reference_through_cache_registry(
    context: &PullContext,
    reference: &ImageReference,
    stored_reference: &StoredReference,
    options: &PullOptions,
) -> Result<()> {
    if matches!(reference.target, ReferenceTarget::Digest(_)) {
        context.ui.warn(
            "registry load mode does not support digest references yet; falling back to stream load",
        );
        return stream_load_reference(context, reference, stored_reference, options).await;
    }

    let registry =
        TemporaryCacheRegistry::start(context.store.clone(), context.registry.clone(), reference)
            .await?;
    let synthetic = registry.synthetic_reference();
    let load_result = async {
        docker::pull_image(&synthetic).await?;
        let tag_result = docker::tag_image(&synthetic, &reference.display_name()).await;
        let _ = docker::remove_image_tag(&synthetic).await;
        tag_result
    }
    .await;
    let shutdown_result = registry.shutdown().await;
    load_result?;
    shutdown_result?;
    Ok(())
}

struct TemporaryCacheRegistry {
    address: String,
    repository: String,
    tag: String,
    task: Option<JoinHandle<Result<()>>>,
    shutdown: Option<oneshot::Sender<()>>,
}

impl TemporaryCacheRegistry {
    async fn start(
        store: Arc<Store>,
        registry: Arc<RegistryClient>,
        reference: &ImageReference,
    ) -> Result<Self> {
        let tag = match &reference.target {
            ReferenceTarget::Tag(tag) => tag.clone(),
            ReferenceTarget::Digest(_) => {
                return Err(DockerPullError::InvalidInput(
                    "temporary cache registry requires a tag reference".into(),
                ));
            }
        };
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?.to_string();
        let repository = cache_repository(&reference.registry, &reference.repository);
        let (shutdown, shutdown_rx) = oneshot::channel();
        let task = tokio::spawn(async move {
            serve_registry::serve_listener(
                listener,
                ServeListenerConfig {
                    store,
                    registry,
                    pull_missing: false,
                    blob_retry_limit: Some(1),
                    blob_idle_timeout: None,
                    concurrency: 1,
                    quiet: true,
                },
                Some(shutdown_rx),
            )
            .await
        });

        Ok(Self {
            address,
            repository,
            tag,
            task: Some(task),
            shutdown: Some(shutdown),
        })
    }

    fn synthetic_reference(&self) -> String {
        format!("{}/{}:{}", self.address, self.repository, self.tag)
    }

    async fn shutdown(mut self) -> Result<()> {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        if let Some(task) = self.task.take() {
            task.await.map_err(|error| {
                DockerPullError::CommandFailed(format!(
                    "temporary cache registry task failed: {error}"
                ))
            })??;
        }
        Ok(())
    }
}

impl Drop for TemporaryCacheRegistry {
    fn drop(&mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
    }
}
