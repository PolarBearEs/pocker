use std::io::Write;
use std::sync::Arc;

use pocker_compose as compose;

use crate::auth::{AuthResolver, read_credentials};
use crate::cli::{
    CacheCommands, Cli, Commands, ComposeCommands, ComposeConfigFormat, ImageCommands,
};
use crate::error::{DockerPullError, Result};
use crate::http::{
    blob_idle_timeout_from_seconds, build_http_client_with_external_connect_timeout,
    external_connect_timeout_from_seconds,
};
use crate::image_view::{format_size, print_image_inspect, print_image_list};
use crate::pull::orchestrator::{PullRequestOptions, pull_references, retry_limit};
use crate::registry::{DEFAULT_REQUEST_RETRIES, RegistryClient};
use crate::store::{ActiveStore, MaintenanceStore};
use crate::{docker, pull, serve_registry, signal, ui};

pub(crate) async fn execute(cli: Cli) -> Result<()> {
    match cli.command {
        Commands::Pull(args) => {
            let (references, request) = PullRequestOptions::from_pull_args(args);
            pull_references(
                &cli.global.cache_dir,
                cli.global.quiet,
                references,
                request,
                "pull",
            )
            .await?;
        }
        Commands::Compose(args) => match args.command {
            ComposeCommands::Config(config_args) => {
                let resolved = resolve_compose_images(
                    args.file,
                    compose::ComposeOptions {
                        profiles: args.profile,
                        services: config_args.services.clone(),
                    },
                )
                .await?;
                print_compose_config(
                    &resolved,
                    config_args.images,
                    config_args.services_only,
                    config_args.pull_plan,
                    config_args.format,
                )?;
            }
            ComposeCommands::Pull(pull_args) => {
                let resolved = resolve_compose_images(
                    args.file,
                    compose::ComposeOptions {
                        profiles: args.profile,
                        services: pull_args.services.clone(),
                    },
                )
                .await?;
                if !(resolved.skipped_build_only.is_empty()
                    || cli.global.quiet
                    || pull_args.common.output.quiet)
                {
                    eprintln!(
                        "{} skipping build-only compose services without image: {}",
                        ui::paint("warning:", ui::WARNING),
                        resolved.skipped_build_only.join(", ")
                    );
                }
                print_compose_pull_plan(
                    &resolved,
                    cli.global.quiet || pull_args.common.output.quiet,
                )?;
                let images = compose::unique_images(&resolved.images);
                let request = PullRequestOptions::from_compose_pull_args(*pull_args);
                pull_references(
                    &cli.global.cache_dir,
                    cli.global.quiet,
                    images,
                    request,
                    "compose pull",
                )
                .await?;
            }
        },
        Commands::Serve(args) => {
            let store = Arc::new(
                ActiveStore::open(cli.global.cache_dir.clone(), "serve")
                    .await?
                    .into_store(),
            );
            let credentials = read_credentials(args.auth.username, args.auth.password_stdin)?;
            let auth = Arc::new(AuthResolver::new_async(credentials).await?);
            let quiet = cli.global.quiet || args.quiet;
            let client = Arc::new(
                RegistryClient::new(
                    build_http_client_with_external_connect_timeout(
                        args.registry.plain_http,
                        args.registry.insecure_skip_tls_verify,
                        args.registry.ca_file.as_deref(),
                        external_connect_timeout_from_seconds(
                            args.external_registry_connection.connect_timeout_seconds,
                        )?,
                    )?,
                    auth,
                    args.registry.plain_http,
                    retry_limit(args.retry.request_retries, DEFAULT_REQUEST_RETRIES),
                )
                .with_retry_warning_sink(Arc::new(move |warning| {
                    if !quiet {
                        eprintln!("{} {warning}", ui::paint("warning:", ui::WARNING));
                    }
                })),
            );
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
            let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
            tokio::spawn(async move {
                signal::wait_for_shutdown_signal().await;
                let _ = shutdown_tx.send(());
            });
            serve_registry::serve(serve_registry::ServeConfig {
                listen: args.listen,
                store,
                registry: client,
                pull_missing: args.pull_missing,
                blob_retry_limit: retry_limit(args.retry.blob_retries, pull::DEFAULT_BLOB_RETRIES),
                blob_idle_timeout: blob_idle_timeout_from_seconds(
                    args.external_registry_connection.blob_idle_timeout_seconds,
                )?,
                concurrency: args.concurrency.max(1),
                quiet,
                shutdown: Some(shutdown_rx),
            })
            .await?;
        }
        Commands::Cache(args) => {
            let store = MaintenanceStore::open(cli.global.cache_dir.clone()).await?;
            match args.command {
                CacheCommands::Clean(args) => {
                    clean_cache(&store, cli.global.quiet, args.verbose).await?
                }
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
    if pull_plan {
        return print_compose_pull_plan(resolved, false);
    }

    write_compose_config(std::io::stdout(), resolved, images, services_only, format)
}

fn write_compose_config(
    mut out: impl Write,
    resolved: &compose::ComposeImages,
    images: bool,
    services_only: bool,
    format: Option<ComposeConfigFormat>,
) -> Result<()> {
    if images {
        for image in &compose::unique_images(&resolved.images) {
            writeln!(out, "{image}")?;
        }
        return Ok(());
    }

    if services_only {
        for service in &resolved.services {
            writeln!(out, "{}", service.service)?;
        }
        return Ok(());
    }

    match format.unwrap_or(ComposeConfigFormat::Json) {
        ComposeConfigFormat::Json => write_compose_config_json(out, resolved)?,
    }
    Ok(())
}

fn write_compose_config_json(mut out: impl Write, resolved: &compose::ComposeImages) -> Result<()> {
    let mut resolved = resolved.clone();
    resolved.images = compose::unique_images(&resolved.images);
    serde_json::to_writer_pretty(&mut out, &resolved)?;
    writeln!(out)?;
    Ok(())
}

fn print_compose_pull_plan(resolved: &compose::ComposeImages, quiet: bool) -> Result<()> {
    write_compose_pull_plan(std::io::stderr(), resolved, quiet)
}

fn write_compose_pull_plan(
    mut err: impl Write,
    resolved: &compose::ComposeImages,
    quiet: bool,
) -> Result<()> {
    if quiet {
        return Ok(());
    }

    let images = compose::unique_images(&resolved.images);
    writeln!(
        err,
        "{} {} service(s), {} unique image(s)",
        ui::paint("Compose pull plan:", ui::CYAN),
        resolved.services.len(),
        images.len()
    )?;

    let mut first_service_by_image = std::collections::HashMap::new();
    for service in &resolved.services {
        if let Some(image) = &service.image {
            if let Some(first_service) = first_service_by_image.get(image) {
                writeln!(
                    err,
                    "  {} {} {} {}",
                    ui::paint(&service.service, ui::GREEN),
                    ui::paint("->", ui::DIM),
                    image,
                    ui::paint(&format!("(shared with {first_service})"), ui::DIM)
                )?;
            } else {
                first_service_by_image.insert(image.clone(), service.service.clone());
                writeln!(
                    err,
                    "  {} {} {}",
                    ui::paint(&service.service, ui::GREEN),
                    ui::paint("->", ui::DIM),
                    image
                )?;
            }
        } else if service.build_only {
            writeln!(
                err,
                "  {} {} {}",
                ui::paint(&service.service, ui::GREEN),
                ui::paint("->", ui::DIM),
                ui::paint("build-only (skipped)", ui::YELLOW)
            )?;
        }
    }
    Ok(())
}

async fn resolve_compose_images(
    files: Vec<std::path::PathBuf>,
    options: compose::ComposeOptions,
) -> Result<compose::ComposeImages> {
    let working_dir = std::env::current_dir()?;
    tokio::task::spawn_blocking(move || compose::resolve_images(&files, &working_dir, &options))
        .await
        .map_err(|error| {
            DockerPullError::InvalidInput(format!("compose resolver task panicked: {error}"))
        })?
        .map_err(Into::into)
}

async fn clean_cache(store: &MaintenanceStore, quiet: bool, verbose: bool) -> Result<()> {
    if quiet {
        store.clear_quiet().await?;
        return Ok(());
    }

    let cleared = store.clear().await?;
    print_cleared_cache(store, cleared, verbose);
    Ok(())
}

fn print_cleared_cache(
    store: &MaintenanceStore,
    cleared: crate::store::ClearedCache,
    verbose: bool,
) {
    println!("Cleared cache at {}", store.root().display());
    if cleared.files.is_empty() {
        println!("Deleted: nothing");
    } else if verbose {
        println!("Deleted:");
        for file in &cleared.files {
            println!(
                "  {} ({})",
                file.path.display(),
                format_size(Some(file.size))
            );
        }
    } else {
        print_deleted_cache_summary(&cleared.files);
    }
    println!(
        "Reclaimed space: {}",
        format_size(Some(cleared.reclaimed_bytes))
    );
}

fn print_deleted_cache_summary(files: &[crate::store::ClearedCacheFile]) {
    println!("Deleted:");
    let mut cache_files = 0usize;
    let mut cache_bytes = 0u64;
    let mut coordination_files = 0usize;
    let mut coordination_bytes = 0u64;

    for file in files {
        if file.path.starts_with("locks") {
            coordination_files += 1;
            coordination_bytes = coordination_bytes.saturating_add(file.size);
        } else {
            cache_files += 1;
            cache_bytes = cache_bytes.saturating_add(file.size);
        }
    }

    if cache_files > 0 {
        println!(
            "  Cached files/layers: {} ({})",
            format_file_count(cache_files),
            format_size(Some(cache_bytes))
        );
    }
    if coordination_files > 0 {
        println!(
            "  Coordination files: {} ({})",
            format_file_count(coordination_files),
            format_size(Some(coordination_bytes))
        );
    }
}

fn format_file_count(count: usize) -> String {
    let noun = if count == 1 { "file" } else { "files" };
    format!("{count} {noun}")
}

fn print_version() {
    println!("{} {}", env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION"));
}

#[cfg(test)]
mod tests {
    use super::{write_compose_config, write_compose_pull_plan};
    use crate::cli::ComposeConfigFormat;

    #[test]
    fn compose_config_images_writes_unique_images_to_output() {
        let resolved = compose_images();
        let mut out = Vec::new();

        write_compose_config(
            &mut out,
            &resolved,
            true,
            false,
            Some(ComposeConfigFormat::Json),
        )
        .expect("config output should write");

        assert_eq!(String::from_utf8(out).expect("utf8"), "nginx:alpine\n");
    }

    #[test]
    fn compose_pull_plan_can_be_rendered_without_stderr_side_effects() {
        let resolved = compose_images();
        let mut err = Vec::new();

        write_compose_pull_plan(&mut err, &resolved, false).expect("pull plan should write");
        let output = String::from_utf8(err).expect("utf8");

        assert!(output.contains("Compose pull plan:"));
        assert!(output.contains("web"));
        assert!(output.contains("api"));
        assert!(output.contains("shared with web"));
    }

    fn compose_images() -> pocker_compose::ComposeImages {
        pocker_compose::ComposeImages {
            services: vec![
                pocker_compose::ComposeServiceImage {
                    service: "web".to_string(),
                    image: Some("nginx:alpine".to_string()),
                    build_only: false,
                    labels: None,
                },
                pocker_compose::ComposeServiceImage {
                    service: "api".to_string(),
                    image: Some("nginx:alpine".to_string()),
                    build_only: false,
                    labels: None,
                },
            ],
            images: vec!["nginx:alpine".to_string(), "nginx:alpine".to_string()],
            skipped_build_only: Vec::new(),
        }
    }
}
