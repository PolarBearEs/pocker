pub mod download;
pub(crate) mod orchestrator;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use futures_util::stream::{FuturesUnordered, StreamExt};
use tracing::warn;

use crate::docker;
use crate::error::{DockerPullError, Result};
use crate::image::pair_layers;
use crate::platform::Platform;
use crate::reference::{ImageReference, ReferenceTarget};
use crate::registry::{Descriptor, RegistryClient};
use crate::serve_registry;
use crate::store::{Store, StoredReference};
use crate::ui::Ui;

pub const DEFAULT_BLOB_RETRIES: u32 = 8;

#[derive(Clone)]
pub struct PullContext {
    pub store: Arc<Store>,
    pub registry: Arc<RegistryClient>,
    pub stop: Arc<AtomicBool>,
    pub ui: Arc<Ui>,
    pub blob_retry_limit: Option<u32>,
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

impl Puller {
    pub fn new(context: PullContext) -> Self {
        Self { context }
    }

    pub async fn pull(&self, reference: ImageReference, options: PullOptions) -> Result<()> {
        if self.context.stop.load(Ordering::SeqCst) {
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
            && finalize_existing_reference(&self.context, &reference, &stored_reference, &options)
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
            match docker::daemon_layer_coverage(&layers).await {
                Ok(coverage) => coverage,
                Err(error) => {
                    warn!("failed to inspect Docker daemon layer coverage: {error}");
                    self.context.ui.warn(format!(
                        "could not inspect existing Docker layers; downloading all missing cache layers: {error}"
                    ));
                    Default::default()
                }
            }
        } else {
            Default::default()
        };
        let mut downloads = Vec::new();

        for layer in layers {
            if self
                .context
                .store
                .ensure_blob_complete(&layer.descriptor.digest, layer.descriptor.expected_size()?)
                .await?
            {
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
                    match result {
                        Ok(inner) => inner?,
                        Err(error) => {
                            return Err(DockerPullError::InvalidInput(format!(
                                "download worker failed: {error}"
                            )));
                        }
                    }
                }
            }
            let context = self.context.clone();
            let reference = reference.clone();
            let normalized = normalized.clone();
            queue.push(tokio::spawn(async move {
                download::download_blob(&context, &reference, &normalized, descriptor).await
            }));
        }

        while let Some(result) = queue.next().await {
            result.map_err(|error| {
                DockerPullError::InvalidInput(format!("download worker failed: {error}"))
            })??;
        }

        self.context.store.save_reference(&stored_reference).await?;

        if options.no_load {
            self.context.ui.finish_image(&normalized, "Pulled");
        } else {
            load_reference(&self.context, &reference, &stored_reference, &options).await?;
        }

        Ok(())
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

async fn finalize_existing_reference(
    context: &PullContext,
    reference: &ImageReference,
    stored_reference: &StoredReference,
    options: &PullOptions,
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
        context
            .store
            .prune_reference_layer_blobs(stored_reference)
            .await?;
    }
    context.ui.finish_image(normalized, "Already exists");
    Ok(true)
}

async fn load_reference(
    context: &PullContext,
    reference: &ImageReference,
    stored_reference: &StoredReference,
    options: &PullOptions,
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
        context
            .store
            .prune_reference_layer_blobs(stored_reference)
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

    let registry = serve_registry::TemporaryCacheRegistry::start(
        context.store.clone(),
        context.registry.clone(),
        reference,
    )
    .await?;
    let synthetic = registry.synthetic_reference();
    docker::pull_image(&synthetic).await?;
    let tag_result = docker::tag_image(&synthetic, &reference.display_name()).await;
    let _ = docker::remove_image_tag(&synthetic).await;
    tag_result?;
    Ok(())
}
