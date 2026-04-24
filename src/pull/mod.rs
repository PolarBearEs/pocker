mod download;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use futures_util::stream::{FuturesUnordered, StreamExt};

use crate::docker;
use crate::error::{DockerPullError, Result};
use crate::image::pair_layers;
use crate::platform::Platform;
use crate::reference::ImageReference;
use crate::registry::RegistryClient;
use crate::store::{Store, StoredReference};
use crate::ui::Ui;

#[derive(Clone)]
pub struct PullContext {
    pub store: Arc<Store>,
    pub registry: Arc<RegistryClient>,
    pub stop: Arc<AtomicBool>,
    pub ui: Arc<Ui>,
}

#[derive(Debug, Clone)]
pub struct PullOptions {
    pub platform: Platform,
    pub concurrency: usize,
    pub no_load: bool,
    pub keep_layer_blobs: bool,
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
        let config_bytes = self
            .context
            .registry
            .get_blob_bytes(&reference, &resolved.config.digest)
            .await?;
        self.context
            .store
            .save_blob_bytes(&resolved.config, &config_bytes)
            .await?;

        let layers = pair_layers(resolved.layers.clone(), &config_bytes)?;
        let layer_digests = layers
            .iter()
            .map(|layer| layer.descriptor.digest.clone())
            .collect::<Vec<_>>();
        self.context.ui.prepare_layers(&layer_digests);
        let daemon_layers = docker::daemon_layer_coverage(&layers)
            .await
            .unwrap_or_default();
        let mut downloads = Vec::new();

        for layer in layers {
            if self
                .context
                .store
                .ensure_blob_complete(&layer.descriptor.digest, layer.descriptor.size)
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

        let stored_reference = StoredReference {
            reference: normalized.clone(),
            manifest: resolved.manifest.clone(),
            config_digest: resolved.config.digest.clone(),
        };

        if options.no_load {
            self.context.ui.finish_image(&normalized, "Pulled");
        } else {
            finalize_reference(&self.context, &reference, &stored_reference, &options).await?;
        }

        Ok(())
    }
}

async fn finalize_reference(
    context: &PullContext,
    reference: &ImageReference,
    stored_reference: &StoredReference,
    options: &PullOptions,
) -> Result<()> {
    let normalized = &stored_reference.reference;
    let already_loaded = docker::daemon_has_reference(reference, &stored_reference.config_digest)
        .await
        .unwrap_or(false);
    if already_loaded {
        context.ui.set_image_status(normalized, "Already exists");
        if !options.keep_layer_blobs {
            context.ui.set_image_status(normalized, "Pruning cache");
            context
                .store
                .prune_reference_layer_blobs(stored_reference)
                .await?;
        }
    } else {
        context.ui.set_image_status(normalized, "Packaging archive");
        let archive = docker::write_reference_archive(&context.store, stored_reference).await?;
        if !options.keep_layer_blobs {
            context.ui.set_image_status(normalized, "Pruning cache");
            context
                .store
                .prune_reference_layer_blobs(stored_reference)
                .await?;
        }
        context.ui.begin_load(normalized);
        docker::load_archive(archive.path()).await?;
    }
    context.ui.finish_image(
        normalized,
        if already_loaded {
            "Already exists"
        } else {
            "Ready"
        },
    );
    Ok(())
}
