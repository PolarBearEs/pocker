use std::sync::Arc;

use clap::Parser;
use pocker_compose as compose;
use tracing_subscriber::EnvFilter;

use crate::auth::{AuthResolver, read_credentials};
use crate::cli::{
    CacheCommands, Cli, Commands, ComposeCommands, ComposeConfigFormat, ImageCommands,
};
use crate::error::Result;
use crate::http::build_http_client;
use crate::image_view::{format_size, print_image_inspect, print_image_list};
use crate::pull::orchestrator::{PullRequestOptions, pull_references, retry_limit};
use crate::registry::{DEFAULT_REQUEST_RETRIES, RegistryClient};
use crate::store::Store;
use crate::{docker, pull, serve_registry, ui};

pub async fn run() -> Result<()> {
    init_tracing();

    let cli = Cli::parse();

    match cli.command {
        Commands::Pull(args) => {
            let (references, request) = PullRequestOptions::from_pull_args(args);
            pull_references(&cli.global.cache_dir, cli.global.quiet, references, request).await?;
        }
        Commands::Compose(args) => match args.command {
            ComposeCommands::Config(config_args) => {
                let resolved = compose::resolve_images(&args.file, &std::env::current_dir()?)?;
                let resolved = compose::select_services(&resolved, &config_args.services)?;
                print_compose_config(
                    &resolved,
                    config_args.images,
                    config_args.services_only,
                    config_args.pull_plan,
                    config_args.format,
                )?;
            }
            ComposeCommands::Pull(pull_args) => {
                let resolved = compose::resolve_images(&args.file, &std::env::current_dir()?)?;
                let resolved = compose::select_services(&resolved, &pull_args.services)?;
                if !(resolved.skipped_build_only.is_empty()
                    || cli.global.quiet
                    || pull_args.output.quiet)
                {
                    eprintln!(
                        "warning: skipping build-only compose services without image: {}",
                        resolved.skipped_build_only.join(", ")
                    );
                }
                print_compose_pull_plan(&resolved, cli.global.quiet || pull_args.output.quiet);
                let images = compose::unique_images(&resolved.images);
                let request = PullRequestOptions::from_compose_pull_args(*pull_args);
                pull_references(&cli.global.cache_dir, cli.global.quiet, images, request).await?;
            }
        },
        Commands::Serve(args) => {
            let store = Arc::new(Store::open_active(cli.global.cache_dir.clone()).await?);
            let credentials = read_credentials(args.username, args.password_stdin)?;
            let auth = Arc::new(AuthResolver::new(credentials)?);
            let client = Arc::new(RegistryClient::new(
                build_http_client(
                    args.plain_http,
                    args.insecure_skip_tls_verify,
                    args.ca_file.as_deref(),
                )?,
                auth,
                args.plain_http,
                retry_limit(
                    args.request_retries,
                    args.retry_forever,
                    DEFAULT_REQUEST_RETRIES,
                ),
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
                blob_retry_limit: retry_limit(
                    args.blob_retries,
                    args.retry_forever,
                    pull::DEFAULT_BLOB_RETRIES,
                ),
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

fn print_compose_config(
    resolved: &compose::ComposeImages,
    images: bool,
    services_only: bool,
    pull_plan: bool,
    format: Option<ComposeConfigFormat>,
) -> Result<()> {
    if images {
        for image in &compose::unique_images(&resolved.images) {
            println!("{image}");
        }
        return Ok(());
    }

    if services_only {
        for service in &resolved.services {
            println!("{}", service.service);
        }
        return Ok(());
    }

    if pull_plan {
        print_compose_pull_plan(resolved, false);
        return Ok(());
    }

    match format.unwrap_or(ComposeConfigFormat::Json) {
        ComposeConfigFormat::Json => print_compose_config_json(resolved)?,
    }
    Ok(())
}

fn print_compose_config_json(resolved: &compose::ComposeImages) -> Result<()> {
    let mut resolved = resolved.clone();
    resolved.images = compose::unique_images(&resolved.images);
    serde_json::to_writer_pretty(std::io::stdout(), &resolved)?;
    println!();
    Ok(())
}

fn print_compose_pull_plan(resolved: &compose::ComposeImages, quiet: bool) {
    if quiet {
        return;
    }

    let images = compose::unique_images(&resolved.images);
    eprintln!(
        "{} {} service(s), {} unique image(s)",
        ui::paint("Compose pull plan:", ui::CYAN),
        resolved.services.len(),
        images.len()
    );

    let mut first_service_by_image = std::collections::HashMap::new();
    for service in &resolved.services {
        if let Some(image) = &service.image {
            if let Some(first_service) = first_service_by_image.get(image) {
                eprintln!(
                    "  {} {} {} {}",
                    ui::paint(&service.service, ui::GREEN),
                    ui::paint("->", ui::DIM),
                    image,
                    ui::paint(&format!("(shared with {first_service})"), ui::DIM)
                );
            } else {
                first_service_by_image.insert(image.clone(), service.service.clone());
                eprintln!(
                    "  {} {} {}",
                    ui::paint(&service.service, ui::GREEN),
                    ui::paint("->", ui::DIM),
                    image
                );
            }
        } else if service.build_only {
            eprintln!(
                "  {} {} {}",
                ui::paint(&service.service, ui::GREEN),
                ui::paint("->", ui::DIM),
                ui::paint("build-only (skipped)", ui::YELLOW)
            );
        }
    }
}

fn init_tracing() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("warn"));
    let _ = tracing_subscriber::fmt().with_env_filter(filter).try_init();
}

async fn clean_cache(store: &Store, quiet: bool) -> Result<()> {
    if quiet {
        store.clear_quiet().await?;
        return Ok(());
    }

    let cleared = store.clear().await?;
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
    Ok(())
}

fn print_version() {
    println!("{} {}", env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION"));
}
