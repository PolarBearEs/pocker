use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use tokio::task::JoinSet;

use crate::auth::{AuthResolver, read_credentials};
use crate::cli::{ComposePullArgs, PullArgs, PullCommonArgs};
use crate::error::{DockerPullError, Result};
use crate::http::build_http_client;
use crate::platform::Platform;
use crate::pull::{
    BlobDownloadLocks, DEFAULT_BLOB_RETRIES, LoadMode, PullContext, PullOptions, Puller,
};
use crate::reference;
use crate::registry::{DEFAULT_REQUEST_RETRIES, RegistryClient};
use crate::signal;
use crate::store::Store;
use crate::ui::UiGroup;

pub(crate) struct PullRequestOptions {
    platform: Option<String>,
    concurrency: usize,
    image_concurrency: usize,
    blob_retry_limit: Option<u32>,
    request_retry_limit: Option<u32>,
    no_load: bool,
    keep_layer_blobs: bool,
    load_mode: LoadMode,
    plain_http: bool,
    insecure_skip_tls_verify: bool,
    ca_file: Option<std::path::PathBuf>,
    username: Option<String>,
    password_stdin: bool,
    quiet: bool,
    no_animations: bool,
    cache_from: Option<url::Url>,
    cache_only: bool,
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
            platform: args.download.platform,
            concurrency: args.download.concurrency,
            image_concurrency: args.image_parallel.image_concurrency,
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
            no_load: args.import.no_load,
            keep_layer_blobs: args.import.keep_layer_blobs,
            load_mode: args.import.load_mode,
            plain_http: args.registry.plain_http,
            insecure_skip_tls_verify: args.registry.insecure_skip_tls_verify,
            ca_file: args.registry.ca_file,
            username: args.auth.username,
            password_stdin: args.auth.password_stdin,
            quiet: args.output.quiet,
            no_animations,
            cache_from: args.cache.cache_from,
            cache_only: args.cache.cache_only,
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

#[derive(Clone)]
struct SharedPullState {
    store: Arc<Store>,
    registry: Arc<RegistryClient>,
    stop: Arc<AtomicBool>,
    blob_retry_limit: Option<u32>,
    blob_locks: Arc<BlobDownloadLocks>,
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
    let references = pocker_compose::unique_images(&references);
    let store = Arc::new(Store::open_active(cache_dir.to_path_buf()).await?);
    let quiet = global_quiet || request.quiet;
    let platform = request
        .platform
        .as_deref()
        .map(Platform::parse)
        .transpose()?
        .unwrap_or_else(Platform::host);
    let credentials = read_credentials(request.username, request.password_stdin)?;
    let auth = Arc::new(AuthResolver::new(credentials)?);
    let client = Arc::new(RegistryClient::new_with_cache_from(
        build_http_client(
            request.plain_http
                || request
                    .cache_from
                    .as_ref()
                    .is_some_and(|url| url.scheme() == "http"),
            request.insecure_skip_tls_verify,
            request.ca_file.as_deref(),
        )?,
        auth,
        request.plain_http,
        request.request_retry_limit,
        request.cache_from,
        request.cache_only,
    ));
    let stop = signal::install_handler();
    let options = PullOptions {
        platform,
        concurrency: request.concurrency.max(1),
        no_load: request.no_load,
        keep_layer_blobs: request.keep_layer_blobs,
        load_mode: request.load_mode,
    };
    let daemon_layer_cache = (options.load_mode == LoadMode::Stream)
        .then(|| Arc::new(crate::docker::DaemonLayerCache::new()));

    let state = SharedPullState {
        store,
        registry: client,
        stop,
        blob_retry_limit: request.blob_retry_limit,
        blob_locks: Arc::new(BlobDownloadLocks::default()),
        daemon_layer_cache,
        options,
        ui_group: UiGroup::new(quiet, !request.no_animations),
    };

    if request.image_concurrency <= 1 {
        for reference in references {
            pull_reference_with_group(state.clone(), reference).await?;
        }
        return Ok(());
    }

    pull_references_parallel(state, references, request.image_concurrency.max(1)).await
}

async fn pull_references_parallel(
    state: SharedPullState,
    references: Vec<String>,
    image_concurrency: usize,
) -> Result<()> {
    let mut pending = references.into_iter();
    let mut queue = JoinSet::new();

    while queue.len() < image_concurrency {
        let Some(reference) = pending.next() else {
            break;
        };
        spawn_pull_task(&mut queue, state.clone(), reference);
    }

    while let Some(result) = queue.join_next().await {
        match result {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                queue.abort_all();
                return Err(error);
            }
            Err(error) => {
                queue.abort_all();
                return Err(DockerPullError::CommandFailed(format!(
                    "pull task failed: {error}"
                )));
            }
        }

        if let Some(reference) = pending.next() {
            spawn_pull_task(&mut queue, state.clone(), reference);
        }
    }

    Ok(())
}

fn spawn_pull_task(queue: &mut JoinSet<Result<()>>, state: SharedPullState, reference: String) {
    queue.spawn(async move { pull_reference_with_group(state, reference).await });
}

async fn pull_reference_with_group(state: SharedPullState, reference: String) -> Result<()> {
    let reference = reference::ImageReference::parse(&reference)?;
    let label = reference.display_name();
    let context = PullContext {
        store: state.store,
        registry: state.registry,
        stop: state.stop,
        ui: Arc::new(state.ui_group.image_ui(label)),
        blob_retry_limit: state.blob_retry_limit,
        blob_locks: state.blob_locks,
        daemon_layer_cache: state.daemon_layer_cache,
    };
    Puller::new(context).pull(reference, state.options).await
}
