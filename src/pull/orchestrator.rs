use std::path::Path;
use std::sync::Arc;

use futures_util::{StreamExt, stream};
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;
use tracing::warn;

use crate::auth::{AuthResolver, read_credentials};
use crate::cli::{ComposePullArgs, PullArgs, PullCommonArgs};
use crate::error::{DockerPullError, Result};
use crate::http::{build_http_client_with_connect_timeout, connect_timeout_from_seconds};
use crate::platform::Platform;
use crate::pull::{
    BlobDownloadLocks, CurrentPullLayers, DEFAULT_BLOB_RETRIES, LoadMode, PullContext, PullOptions,
    Puller,
};
use crate::reference::ImageReference;
use crate::registry::{DEFAULT_REQUEST_RETRIES, RegistryClient, ResolvedImage};
use crate::signal;
use crate::store::{ActiveStore, Store};
use crate::ui::UiGroup;

pub(crate) struct PullRequestOptions {
    download: PullDownloadConfig,
    image_concurrency: usize,
    retry: RetryConfig,
    import: ImportConfig,
    registry: RegistryConfig,
    auth: AuthConfig,
    quiet: bool,
    cache: CacheSourceConfig,
    registry_connection: RegistryConnectionConfig,
    no_animations: bool,
}

struct PullDownloadConfig {
    platform: Option<String>,
    concurrency: usize,
}

struct RetryConfig {
    blob_retry_limit: Option<u32>,
    request_retry_limit: Option<u32>,
}

struct ImportConfig {
    no_load: bool,
    keep_layer_blobs: bool,
    load_mode: LoadMode,
}

struct RegistryConfig {
    plain_http: bool,
    insecure_skip_tls_verify: bool,
    ca_file: Option<std::path::PathBuf>,
}

struct AuthConfig {
    username: Option<String>,
    password_stdin: bool,
}

struct CacheSourceConfig {
    cache_from: Option<url::Url>,
    cache_only: bool,
}

struct RegistryConnectionConfig {
    connect_timeout_seconds: i64,
}

impl PullRequestOptions {
    pub(crate) fn from_pull_args(args: PullArgs) -> (Vec<String>, Self) {
        (
            args.references,
            Self::from_args(args.common, args.no_animations),
        )
    }

    pub(crate) fn from_compose_pull_args(args: ComposePullArgs) -> Self {
        Self::from_args(args.common, true)
    }

    fn from_args(args: PullCommonArgs, no_animations: bool) -> Self {
        Self {
            download: PullDownloadConfig {
                platform: args.download.platform,
                concurrency: args.download.concurrency,
            },
            image_concurrency: args.image_parallel.image_concurrency,
            retry: RetryConfig {
                blob_retry_limit: retry_limit(
                    args.retry.blob_retries,
                    args.retry.retry_forever,
                    DEFAULT_BLOB_RETRIES,
                ),
                request_retry_limit: retry_limit(
                    args.retry.request_retries,
                    args.retry.retry_forever,
                    DEFAULT_REQUEST_RETRIES,
                ),
            },
            import: ImportConfig {
                no_load: args.import.no_load,
                keep_layer_blobs: args.import.keep_layer_blobs,
                load_mode: args.import.load_mode,
            },
            registry: RegistryConfig {
                plain_http: args.registry.plain_http,
                insecure_skip_tls_verify: args.registry.insecure_skip_tls_verify,
                ca_file: args.registry.ca_file,
            },
            auth: AuthConfig {
                username: args.auth.username,
                password_stdin: args.auth.password_stdin,
            },
            quiet: args.output.quiet,
            no_animations,
            cache: CacheSourceConfig {
                cache_from: args.cache.cache_from,
                cache_only: args.cache.cache_only,
            },
            registry_connection: RegistryConnectionConfig {
                connect_timeout_seconds: args.registry_connection.connect_timeout_seconds,
            },
        }
    }
}

pub(crate) fn retry_limit(
    retries: Option<u32>,
    retry_forever: bool,
    default_retries: u32,
) -> Option<u32> {
    match (retries, retry_forever) {
        (Some(retries), _) => Some(retries),
        (None, true) => None,
        (None, false) => Some(default_retries),
    }
}

struct PlannedPull {
    reference: ImageReference,
    resolved: ResolvedImage,
    layer_digests: Vec<String>,
}

struct PullPlan {
    pulls: Vec<PlannedPull>,
    image_layers: Vec<Vec<String>>,
}

impl PullPlan {
    fn new(pulls: Vec<PlannedPull>) -> Self {
        let image_layers = pulls
            .iter()
            .map(|planned| planned.layer_digests.clone())
            .collect();
        Self {
            pulls,
            image_layers,
        }
    }

    fn layer_usage(&self) -> CurrentPullLayers {
        CurrentPullLayers::from_image_layers(&self.image_layers)
    }

    fn into_pulls(self) -> Vec<PlannedPull> {
        self.pulls
    }
}

async fn plan_pull_references(
    references: &[String],
    registry: Arc<RegistryClient>,
    platform: &Platform,
    concurrency: usize,
) -> Result<PullPlan> {
    let mut planned = stream::iter(references.iter().cloned().enumerate())
        .map(|(index, raw_reference)| {
            let registry = Arc::clone(&registry);
            let platform = platform.clone();
            async move {
                let reference = ImageReference::parse(&raw_reference)?;
                let resolved = registry
                    .resolve_image(&reference, &platform)
                    .await
                    .map_err(|source| DockerPullError::ImageResolutionFailed {
                        reference: reference.display_name(),
                        source: Box::new(source),
                    })?;
                let layer_digests = resolved
                    .layers
                    .iter()
                    .map(|layer| layer.digest.clone())
                    .collect();
                Result::Ok((
                    index,
                    PlannedPull {
                        reference,
                        resolved,
                        layer_digests,
                    },
                ))
            }
        })
        .buffer_unordered(concurrency.max(1));

    let mut ordered = Vec::with_capacity(references.len());
    ordered.resize_with(references.len(), || None);
    while let Some(result) = planned.next().await {
        let (index, planned_pull) = result?;
        ordered[index] = Some(planned_pull);
    }

    Ok(PullPlan::new(ordered.into_iter().flatten().collect()))
}

#[derive(Clone)]
struct SharedPullState {
    store: Arc<Store>,
    registry: Arc<RegistryClient>,
    stop: CancellationToken,
    blob_retry_limit: Option<u32>,
    blob_locks: Arc<BlobDownloadLocks>,
    layer_usage: Arc<CurrentPullLayers>,
    daemon_layer_cache: Option<Arc<crate::docker::DaemonLayerCache>>,
    options: PullOptions,
    ui_group: UiGroup,
}

pub(crate) async fn pull_references(
    cache_dir: &Path,
    global_quiet: bool,
    references: Vec<String>,
    request: PullRequestOptions,
) -> Result<()> {
    let registry_connect_timeout =
        connect_timeout_from_seconds(request.registry_connection.connect_timeout_seconds)?;
    let references = pocker_compose::unique_images(&references);
    let store = Arc::new(
        ActiveStore::open(cache_dir.to_path_buf())
            .await?
            .into_store(),
    );
    let quiet = global_quiet || request.quiet;
    let platform = request
        .download
        .platform
        .as_deref()
        .map(Platform::parse)
        .transpose()?
        .unwrap_or_else(Platform::host);
    let credentials = read_credentials(request.auth.username, request.auth.password_stdin)?;
    let auth = Arc::new(AuthResolver::new_async(credentials).await?);
    let client = Arc::new(RegistryClient::new_with_cache_from(
        build_http_client_with_connect_timeout(
            request.registry.plain_http
                || request
                    .cache
                    .cache_from
                    .as_ref()
                    .is_some_and(|url| url.scheme() == "http"),
            request.registry.insecure_skip_tls_verify,
            request.registry.ca_file.as_deref(),
            registry_connect_timeout,
        )?,
        auth,
        request.registry.plain_http,
        request.retry.request_retry_limit,
        request.cache.cache_from,
        request.cache.cache_only,
    ));
    let image_concurrency = request.image_concurrency.max(1);
    let pull_plan = plan_pull_references(
        &references,
        Arc::clone(&client),
        &platform,
        image_concurrency,
    )
    .await?;
    let layer_usage = Arc::new(pull_plan.layer_usage());
    let planned_pulls = pull_plan.into_pulls();
    let stop = signal::install_handler();
    let options = PullOptions {
        concurrency: request.download.concurrency.max(1),
        no_load: request.import.no_load,
        keep_layer_blobs: request.import.keep_layer_blobs,
        load_mode: request.import.load_mode,
    };
    let daemon_layer_cache = (options.load_mode == LoadMode::Stream)
        .then(|| Arc::new(crate::docker::DaemonLayerCache::new()));

    let state = SharedPullState {
        store,
        registry: client,
        stop,
        blob_retry_limit: request.retry.blob_retry_limit,
        blob_locks: Arc::new(BlobDownloadLocks::default()),
        layer_usage,
        daemon_layer_cache,
        options,
        ui_group: UiGroup::new(quiet, !request.no_animations),
    };

    if image_concurrency <= 1 {
        for planned_pull in planned_pulls {
            pull_reference_with_group(state.clone(), planned_pull).await?;
        }
        return Ok(());
    }

    pull_references_parallel(state, planned_pulls, image_concurrency).await
}

async fn pull_references_parallel(
    state: SharedPullState,
    planned_pulls: Vec<PlannedPull>,
    image_concurrency: usize,
) -> Result<()> {
    let mut pending = planned_pulls.into_iter();
    let mut queue = JoinSet::new();

    while queue.len() < image_concurrency {
        let Some(planned_pull) = pending.next() else {
            break;
        };
        spawn_pull_task(&mut queue, state.clone(), planned_pull);
    }

    while let Some(result) = queue.join_next().await {
        match result {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                abort_pull_tasks(&mut queue).await;
                return Err(error);
            }
            Err(error) => {
                abort_pull_tasks(&mut queue).await;
                return Err(DockerPullError::CommandFailed(format!(
                    "pull task failed: {error}"
                )));
            }
        }

        if let Some(planned_pull) = pending.next() {
            spawn_pull_task(&mut queue, state.clone(), planned_pull);
        }
    }

    Ok(())
}

fn spawn_pull_task(
    queue: &mut JoinSet<Result<()>>,
    state: SharedPullState,
    planned_pull: PlannedPull,
) {
    queue.spawn(async move { pull_reference_with_group(state, planned_pull).await });
}

async fn abort_pull_tasks(queue: &mut JoinSet<Result<()>>) {
    queue.abort_all();
    while let Some(result) = queue.join_next().await {
        if let Err(error) = result
            && !error.is_cancelled()
        {
            warn!("pull task failed while aborting: {error}");
        }
    }
}

async fn pull_reference_with_group(
    state: SharedPullState,
    planned_pull: PlannedPull,
) -> Result<()> {
    let PlannedPull {
        reference,
        resolved,
        layer_digests: _,
    } = planned_pull;
    let label = reference.display_name();
    let context = PullContext {
        store: state.store,
        registry: state.registry,
        stop: state.stop,
        ui: Arc::new(state.ui_group.image_ui(label)),
        blob_retry_limit: state.blob_retry_limit,
        blob_locks: state.blob_locks,
        layer_usage: state.layer_usage,
        daemon_layer_cache: state.daemon_layer_cache,
    };
    Puller::new(context)
        .pull_resolved(reference, resolved, state.options)
        .await
}

#[cfg(test)]
mod tests {
    use super::{PlannedPull, PullPlan};
    use crate::reference::ImageReference;
    use crate::registry::{Descriptor, ResolvedImage};

    #[test]
    fn pull_plan_builds_layer_usage_from_resolved_images() {
        let first_layers = vec!["sha256:shared".to_string(), "sha256:first".to_string()];
        let second_layers = vec!["sha256:shared".to_string(), "sha256:second".to_string()];
        let plan = PullPlan::new(vec![
            planned_pull("example.com/library/first:latest", first_layers.clone()),
            planned_pull("example.com/library/second:latest", second_layers),
        ]);
        let usage = plan.layer_usage();

        let first_claim = usage.claim(&first_layers);

        assert!(first_claim.protected_digests().contains("sha256:shared"));
        assert!(!first_claim.protected_digests().contains("sha256:first"));
    }

    fn planned_pull(raw_reference: &str, layer_digests: Vec<String>) -> PlannedPull {
        let layers = layer_digests
            .iter()
            .map(|digest| descriptor(digest))
            .collect::<Vec<_>>();
        PlannedPull {
            reference: ImageReference::parse(raw_reference).expect("reference should parse"),
            resolved: ResolvedImage {
                manifest: descriptor("sha256:manifest"),
                manifest_bytes: Vec::new(),
                config: descriptor("sha256:config"),
                layers,
            },
            layer_digests,
        }
    }

    fn descriptor(digest: &str) -> Descriptor {
        Descriptor {
            media_type: String::new(),
            digest: digest.to_string(),
            size: 0,
            platform: None,
            annotations: None,
        }
    }
}
