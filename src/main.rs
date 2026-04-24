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
mod store;
mod ui;

use std::io::Read;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use auth::{AuthResolver, Credentials};
use clap::Parser;
use cli::{Cli, Commands, ImageCommands};
use error::{DockerPullError, Result};
use http::build_http_client;
use platform::Platform;
use pull::{PullContext, PullOptions, Puller};
use registry::RegistryClient;
use store::Store;
use tracing_subscriber::EnvFilter;
use ui::Ui;

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
            let cache_dir = cli.global.cache_dir.clone();
            let store = Arc::new(Store::open(cache_dir).await?);
            let reference = reference::ImageReference::parse(&args.reference)?;
            let quiet = cli.global.quiet || args.quiet;
            let platform = args
                .platform
                .as_deref()
                .map(Platform::parse)
                .transpose()?
                .unwrap_or_else(Platform::host);
            let ui = Arc::new(Ui::new(quiet));
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
            ));
            let stop = install_signal_handler();
            let options = PullOptions {
                platform,
                concurrency: args.concurrency.max(1),
                no_load: args.no_load,
                keep_layer_blobs: args.keep_layer_blobs,
            };
            let context = PullContext {
                store,
                registry: client,
                stop,
                ui,
            };
            Puller::new(context).pull(reference, options).await?;
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
