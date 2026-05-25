use std::sync::Arc;
use tokio::net::TcpListener;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

use crate::docker;
use crate::error::{DockerPullError, Result};
use crate::reference::{ImageReference, ReferenceTarget};
use crate::registry::{RegistryClient, cache_repository};
use crate::serve_registry::{self, ServeListenerConfig};
use crate::store::{Store, StoredReference};

use super::{LayerClaimGuard, LoadMode, PullContext, PullOptions};

pub(super) async fn finalize_existing_reference(
    context: &PullContext,
    reference: &ImageReference,
    stored_reference: &StoredReference,
    options: &PullOptions,
    layer_claim: &LayerClaimGuard<'_>,
) -> Result<bool> {
    let normalized = &stored_reference.reference;
    let already_loaded = docker::daemon_has_reference(reference, &stored_reference.config_digest)
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
            .prune_reference_layer_blobs_except(stored_reference, &protected_layer_digests)
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
) -> Result<()> {
    let normalized = &stored_reference.reference;
    context.ui.begin_load(normalized);
    match options.load_mode {
        LoadMode::Stream => {
            docker::load_reference_archive_stream(&context.store, stored_reference).await?;
        }
        LoadMode::Registry => {
            load_reference_through_cache_registry(context, reference, stored_reference).await?;
        }
    }
    if !options.keep_layer_blobs {
        context.ui.set_image_status(normalized, "Pruning cache");
        let protected_layer_digests = layer_claim.protected_digests();
        context
            .store
            .prune_reference_layer_blobs_except(stored_reference, &protected_layer_digests)
            .await?;
    }
    context.ui.finish_image(normalized, "Ready");
    Ok(())
}

async fn load_reference_through_cache_registry(
    context: &PullContext,
    reference: &ImageReference,
    stored_reference: &StoredReference,
) -> Result<()> {
    if matches!(reference.target, ReferenceTarget::Digest(_)) {
        context.ui.warn(
            "registry load mode does not support digest references yet; falling back to stream load",
        );
        docker::load_reference_archive_stream(&context.store, stored_reference).await?;
        return Ok(());
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
