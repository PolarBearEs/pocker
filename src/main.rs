mod auth;
mod cli;
mod docker;
mod error;
mod export;
mod http;
mod image;
mod platform;
mod pull;
mod reference;
mod registry;
mod serve_registry;
mod store;
mod ui;

use std::io::Read;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use auth::{AuthResolver, Credentials};
use clap::Parser;
use cli::{CacheCommands, Cli, Commands, ComposeCommands, ImageCommands};
use error::{DockerPullError, Result};
use http::build_http_client;
use platform::Platform;
use pocker_compose as compose;
use pull::{PullContext, PullOptions, Puller};
use registry::RegistryClient;
use store::Store;
use tokio::task::JoinSet;
use tracing_subscriber::EnvFilter;
use ui::{Ui, UiGroup};

struct PullRequestOptions {
    platform: Option<String>,
    concurrency: usize,
    image_concurrency: usize,
    blob_retries: u32,
    request_retries: u32,
    no_load: bool,
    keep_layer_blobs: bool,
    load_mode: pull::LoadMode,
    plain_http: bool,
    insecure_skip_tls_verify: bool,
    ca_file: Option<std::path::PathBuf>,
    username: Option<String>,
    password_stdin: bool,
    quiet: bool,
    no_animations: bool,
    cache_from: Option<url::Url>,
}

#[derive(Clone)]
struct SharedPullState {
    store: Arc<Store>,
    registry: Arc<RegistryClient>,
    stop: Arc<AtomicBool>,
    blob_retry_limit: u32,
    options: PullOptions,
    ui_group: UiGroup,
}

#[tokio::main]
async fn main() {
    if let Err(error) = run().await {
        eprintln!("error: {error}");
        std::process::exit(1);
    }
}

async fn run() -> Result<()> {
    init_tracing();

    let cli = Cli::parse();

    match cli.command {
        Commands::Pull(args) => {
            let request = PullRequestOptions {
                platform: args.platform,
                concurrency: args.concurrency,
                image_concurrency: 1,
                blob_retries: args.blob_retries,
                request_retries: args.request_retries,
                no_load: args.no_load,
                keep_layer_blobs: args.keep_layer_blobs,
                load_mode: args.load_mode,
                plain_http: args.plain_http,
                insecure_skip_tls_verify: args.insecure_skip_tls_verify,
                ca_file: args.ca_file,
                username: args.username,
                password_stdin: args.password_stdin,
                quiet: args.quiet,
                no_animations: args.no_animations,
                cache_from: args.cache_from,
            };
            pull_references(
                &cli.global.cache_dir,
                cli.global.quiet,
                vec![args.reference],
                request,
            )
            .await?;
        }
        Commands::Compose(args) => match args.command {
            ComposeCommands::Config(config_args) => {
                let resolved = compose::resolve_images(&args.file, &std::env::current_dir()?)?;
                let resolved = compose::select_services(&resolved, &config_args.services)?;
                print_compose_config(&resolved, config_args.images, config_args.services_only);
            }
            ComposeCommands::Pull(pull_args) => {
                let resolved = compose::resolve_images(&args.file, &std::env::current_dir()?)?;
                let resolved = compose::select_services(&resolved, &pull_args.services)?;
                if !(resolved.skipped_build_only.is_empty() || cli.global.quiet || pull_args.quiet)
                {
                    eprintln!(
                        "warning: skipping build-only compose services without image: {}",
                        resolved.skipped_build_only.join(", ")
                    );
                }
                print_compose_pull_plan(&resolved, cli.global.quiet || pull_args.quiet);
                let images = compose::unique_images(&resolved.images);
                let request = PullRequestOptions {
                    platform: pull_args.platform,
                    concurrency: pull_args.concurrency,
                    image_concurrency: pull_args.image_concurrency,
                    blob_retries: pull_args.blob_retries,
                    request_retries: pull_args.request_retries,
                    no_load: pull_args.no_load,
                    keep_layer_blobs: pull_args.keep_layer_blobs,
                    load_mode: pull_args.load_mode,
                    plain_http: pull_args.plain_http,
                    insecure_skip_tls_verify: pull_args.insecure_skip_tls_verify,
                    ca_file: pull_args.ca_file,
                    username: pull_args.username,
                    password_stdin: pull_args.password_stdin,
                    quiet: pull_args.quiet,
                    no_animations: true,
                    cache_from: pull_args.cache_from,
                };
                pull_references(&cli.global.cache_dir, cli.global.quiet, images, request).await?;
            }
        },
        Commands::Serve(args) => {
            let store = Arc::new(Store::open(cli.global.cache_dir.clone()).await?);
            let password = if args.password_stdin {
                Some(read_password_stdin()?)
            } else {
                None
            };
            let credentials = match (args.username, password) {
                (Some(username), Some(password)) => Some(Credentials::Basic { username, password }),
                (None, None) => None,
                _ => {
                    return Err(DockerPullError::InvalidInput(
                        "`--username` requires `--password-stdin`".into(),
                    ));
                }
            };
            let auth = Arc::new(AuthResolver::new(credentials)?);
            let client = Arc::new(RegistryClient::new(
                build_http_client(
                    args.plain_http,
                    args.insecure_skip_tls_verify,
                    args.ca_file.as_deref(),
                )?,
                auth,
                args.plain_http,
                args.request_retries,
            ));
            let quiet = cli.global.quiet || args.quiet;
            if !quiet {
                eprintln!(
                    "Serving pocker cache on {} ({})",
                    args.listen,
                    if args.pull_missing {
                        "pull missing enabled"
                    } else {
                        "cache only"
                    }
                );
            }
            serve_registry::serve(serve_registry::ServeConfig {
                listen: args.listen,
                store,
                registry: client,
                pull_missing: args.pull_missing,
                blob_retry_limit: args.blob_retries,
                concurrency: args.concurrency.max(1),
                quiet,
            })
            .await?;
        }
        Commands::Cache(args) => {
            let store = Store::open(cli.global.cache_dir.clone()).await?;
            match args.command {
                CacheCommands::Clean(_) => clean_cache(&store, cli.global.quiet).await?,
            }
        }
        Commands::Image(args) => match args.command {
            ImageCommands::Ls(_) => print_image_list().await?,
            ImageCommands::Inspect(args) => print_image_inspect(&args.reference).await?,
            ImageCommands::Save(args) => docker::save_image(&args.reference, &args.output).await?,
            ImageCommands::Load(args) => docker::load_archive(&args.input).await?,
        },
        Commands::Images(_) => print_image_list().await?,
        Commands::Version => print_version(),
    }

    Ok(())
}

fn print_compose_config(resolved: &compose::ComposeImages, images: bool, services_only: bool) {
    if images {
        for image in &compose::unique_images(&resolved.images) {
            println!("{image}");
        }
        return;
    }

    if services_only {
        for service in &resolved.services {
            println!("{}", service.service);
        }
        return;
    }

    print_compose_pull_plan(resolved, false);
}

fn print_compose_pull_plan(resolved: &compose::ComposeImages, quiet: bool) {
    if quiet {
        return;
    }

    let images = compose::unique_images(&resolved.images);
    eprintln!(
        "{} {} service(s), {} unique image(s)",
        color("Compose pull plan:", Color::Cyan),
        resolved.services.len(),
        images.len()
    );

    let mut first_service_by_image = std::collections::HashMap::new();
    for service in &resolved.services {
        if let Some(image) = &service.image {
            if let Some(first_service) = first_service_by_image.get(image) {
                eprintln!(
                    "  {} {} {} {}",
                    color(&service.service, Color::Green),
                    color("->", Color::Dim),
                    image,
                    color(&format!("(shared with {first_service})"), Color::Dim)
                );
            } else {
                first_service_by_image.insert(image.clone(), service.service.clone());
                eprintln!(
                    "  {} {} {}",
                    color(&service.service, Color::Green),
                    color("->", Color::Dim),
                    image
                );
            }
        } else if service.build_only {
            eprintln!(
                "  {} {} {}",
                color(&service.service, Color::Green),
                color("->", Color::Dim),
                color("build-only (skipped)", Color::Yellow)
            );
        }
    }
}

#[derive(Copy, Clone)]
enum Color {
    Green,
    Yellow,
    Cyan,
    Dim,
}

fn color(value: &str, color: Color) -> String {
    if !ui::should_color_stderr() {
        return value.to_string();
    }
    let code = match color {
        Color::Green => "32",
        Color::Yellow => "33",
        Color::Cyan => "36",
        Color::Dim => "2",
    };
    format!("\x1b[{code}m{value}\x1b[0m")
}

async fn pull_references(
    cache_dir: &std::path::Path,
    global_quiet: bool,
    references: Vec<String>,
    request: PullRequestOptions,
) -> Result<()> {
    let store = Arc::new(Store::open(cache_dir.to_path_buf()).await?);
    let quiet = global_quiet || request.quiet;
    let platform = request
        .platform
        .as_deref()
        .map(Platform::parse)
        .transpose()?
        .unwrap_or_else(Platform::host);
    let ui = Arc::new(Ui::new(quiet, !request.no_animations));
    let password = if request.password_stdin {
        Some(read_password_stdin()?)
    } else {
        None
    };
    let credentials = match (request.username, password) {
        (Some(username), Some(password)) => Some(Credentials::Basic { username, password }),
        (None, None) => None,
        _ => {
            return Err(DockerPullError::InvalidInput(
                "`--username` requires `--password-stdin`".into(),
            ));
        }
    };
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
        request.request_retries,
        request.cache_from,
    ));
    let stop = install_signal_handler();
    let options = PullOptions {
        platform,
        concurrency: request.concurrency.max(1),
        no_load: request.no_load,
        keep_layer_blobs: request.keep_layer_blobs,
        load_mode: request.load_mode,
    };

    if references.len() <= 1 || request.image_concurrency <= 1 {
        let context = PullContext {
            store,
            registry: client,
            stop,
            ui,
            blob_retry_limit: request.blob_retries,
        };
        let puller = Puller::new(context);
        for reference in references {
            let reference = reference::ImageReference::parse(&reference)?;
            puller.pull(reference, options.clone()).await?;
        }
        return Ok(());
    }

    let state = SharedPullState {
        store,
        registry: client,
        stop,
        blob_retry_limit: request.blob_retries,
        options,
        ui_group: UiGroup::new(quiet, false),
    };
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
                    "compose pull task failed: {error}"
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
    queue.spawn(async move {
        let reference = reference::ImageReference::parse(&reference)?;
        let label = reference.display_name();
        let context = PullContext {
            store: state.store,
            registry: state.registry,
            stop: state.stop,
            ui: Arc::new(state.ui_group.image_ui(label)),
            blob_retry_limit: state.blob_retry_limit,
        };
        Puller::new(context).pull(reference, state.options).await
    });
}

fn init_tracing() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("warn"));
    let _ = tracing_subscriber::fmt().with_env_filter(filter).try_init();
}

fn read_password_stdin() -> Result<String> {
    let mut password = String::new();
    std::io::stdin().read_to_string(&mut password)?;
    Ok(password.trim_end_matches(['\n', '\r']).to_string())
}

fn install_signal_handler() -> Arc<AtomicBool> {
    let stop = Arc::new(AtomicBool::new(false));
    let signal = Arc::clone(&stop);
    tokio::spawn(async move {
        #[cfg(unix)]
        {
            use tokio::signal::unix::{SignalKind, signal};

            let mut sigint = signal(SignalKind::interrupt()).ok();
            let mut sigterm = signal(SignalKind::terminate()).ok();
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {}
                _ = async {
                    if let Some(stream) = sigint.as_mut() {
                        stream.recv().await;
                    }
                } => {}
                _ = async {
                    if let Some(stream) = sigterm.as_mut() {
                        stream.recv().await;
                    }
                } => {}
            }
        }
        #[cfg(not(unix))]
        {
            let _ = tokio::signal::ctrl_c().await;
        }
        signal.store(true, Ordering::SeqCst);
    });
    stop
}

async fn print_image_list() -> Result<()> {
    let images = docker::list_images().await?;
    let mut rows = Vec::new();
    for image in images {
        if image.repo_tags.is_empty() {
            rows.push((
                "<none>".to_string(),
                "<none>".to_string(),
                short_image_id(&image.id).to_string(),
                format_created(image.created),
                format_size(image.size),
            ));
            continue;
        }

        for tag in image.repo_tags {
            let (repository, tag) = split_repo_tag(&tag);
            rows.push((
                repository.to_string(),
                tag.to_string(),
                short_image_id(&image.id).to_string(),
                format_created(image.created),
                format_size(image.size),
            ));
        }
    }

    let repo_width = rows
        .iter()
        .map(|(repository, _, _, _, _)| repository.len())
        .max()
        .unwrap_or(0)
        .max("REPOSITORY".len());
    let tag_width = rows
        .iter()
        .map(|(_, tag, _, _, _)| tag.len())
        .max()
        .unwrap_or(0)
        .max("TAG".len());
    let id_width = rows
        .iter()
        .map(|(_, _, image_id, _, _)| image_id.len())
        .max()
        .unwrap_or(0)
        .max("IMAGE ID".len());
    let created_width = rows
        .iter()
        .map(|(_, _, _, created, _)| created.len())
        .max()
        .unwrap_or(0)
        .max("CREATED".len());

    println!(
        "{:<repo_width$}  {:<tag_width$}  {:<id_width$}  {:<created_width$}  SIZE",
        "REPOSITORY", "TAG", "IMAGE ID", "CREATED",
    );
    for (repository, tag, image_id, created, size) in rows {
        println!(
            "{:<repo_width$}  {:<tag_width$}  {:<id_width$}  {:<created_width$}  {}",
            repository, tag, image_id, created, size,
        );
    }
    Ok(())
}

async fn print_image_inspect(reference: &str) -> Result<()> {
    let Some(image) = docker::inspect_image(reference).await? else {
        return Err(DockerPullError::CommandFailed(format!(
            "docker image inspect failed: image `{reference}` not found"
        )));
    };
    println!("{}", serde_json::to_string_pretty(&image)?);
    Ok(())
}

async fn clean_cache(store: &Store, quiet: bool) -> Result<()> {
    let cleared = store.clear().await?;
    if !quiet {
        println!("Cleared cache at {}", store.root().display());
        if cleared.files.is_empty() {
            println!("Deleted: nothing");
        } else {
            println!("Deleted:");
            for file in cleared.files {
                println!(
                    "  {} ({})",
                    file.path.display(),
                    format_size(Some(file.size))
                );
            }
        }
        println!(
            "Reclaimed space: {}",
            format_size(Some(cleared.reclaimed_bytes))
        );
    }
    Ok(())
}

fn print_version() {
    println!("{} {}", env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION"));
}

fn split_repo_tag(value: &str) -> (&str, &str) {
    match value.rsplit_once(':') {
        Some((repository, tag)) if !tag.contains('/') => (repository, tag),
        _ => (value, "<none>"),
    }
}

fn short_image_id(value: &str) -> &str {
    let value = value.strip_prefix("sha256:").unwrap_or(value);
    let end = value
        .char_indices()
        .nth(12)
        .map(|(index, _)| index)
        .unwrap_or(value.len());
    &value[..end]
}

fn format_size(size: Option<u64>) -> String {
    let Some(size) = size else {
        return "<unknown>".into();
    };
    const UNITS: [&str; 5] = ["B", "kB", "MB", "GB", "TB"];
    let mut value = size as f64;
    let mut unit = 0usize;
    while value >= 1000.0 && unit + 1 < UNITS.len() {
        value /= 1000.0;
        unit += 1;
    }

    if unit == 0 {
        format!("{size} {}", UNITS[unit])
    } else if value >= 10.0 {
        format!("{value:.0} {}", UNITS[unit])
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

fn format_created(created: Option<i64>) -> String {
    let Some(created) = created else {
        return "<unknown>".into();
    };
    let Ok(now) = SystemTime::now().duration_since(UNIX_EPOCH) else {
        return "<unknown>".into();
    };
    let now = now.as_secs();
    let created = if created < 0 { 0 } else { created as u64 };
    if created > now {
        return "just now".into();
    }

    let delta = now - created;
    const MINUTE: u64 = 60;
    const HOUR: u64 = 60 * MINUTE;
    const DAY: u64 = 24 * HOUR;
    const WEEK: u64 = 7 * DAY;
    const MONTH: u64 = 30 * DAY;
    const YEAR: u64 = 365 * DAY;

    let (value, unit) = if delta < MINUTE {
        (delta, "second")
    } else if delta < HOUR {
        (delta / MINUTE, "minute")
    } else if delta < DAY {
        (delta / HOUR, "hour")
    } else if delta < WEEK {
        (delta / DAY, "day")
    } else if delta < MONTH {
        (delta / WEEK, "week")
    } else if delta < YEAR {
        (delta / MONTH, "month")
    } else {
        (delta / YEAR, "year")
    };

    if value == 1 {
        format!("1 {unit} ago")
    } else {
        format!("{value} {unit}s ago")
    }
}
