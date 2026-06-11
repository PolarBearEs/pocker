use std::net::SocketAddr;
use std::path::PathBuf;

use clap::{Args, Parser, Subcommand, ValueEnum};
use url::Url;

use crate::http::{
    DEFAULT_EXTERNAL_CONNECT_TIMEOUT_SECONDS, parse_external_connect_timeout_seconds,
};
use crate::platform::Platform;
use crate::pull::LoadMode;

#[derive(Debug, Parser)]
#[command(name = "pocker")]
#[command(about = "Resumable OCI registry image puller")]
#[command(version)]
pub struct Cli {
    #[command(flatten)]
    pub global: GlobalArgs,
    #[command(subcommand)]
    pub command: Commands,
}

#[derive(Debug, Clone, Args)]
pub struct GlobalArgs {
    #[arg(
        long,
        default_value_os_t = default_cache_dir(),
        help = "Cache directory for blobs and partial downloads"
    )]
    pub cache_dir: PathBuf,
    #[arg(long, short = 'q')]
    pub quiet: bool,
}

#[derive(Debug, Subcommand)]
pub enum Commands {
    #[command(about = "Pull OCI images directly from registries")]
    Pull(PullArgs),
    #[command(about = "Serve the local cache as an OCI registry")]
    Serve(ServeArgs),
    #[command(about = "Run Docker Compose helper commands")]
    Compose(ComposeArgs),
    #[command(about = "Manage the local blob and partial-download cache")]
    Cache(CacheArgs),
    #[command(about = "Run Docker image helper commands")]
    Image(ImageArgs),
    #[command(about = "List Docker images")]
    Images(ImageLsArgs),
    #[command(about = "Print version information")]
    Version,
}

#[derive(Debug, Clone, Args)]
#[command(about = "Run Docker Compose helper commands")]
pub struct ComposeArgs {
    #[arg(
        long,
        short = 'f',
        value_name = "FILE",
        help = "Compose configuration file; can be used multiple times"
    )]
    pub file: Vec<PathBuf>,
    #[command(subcommand)]
    pub command: ComposeCommands,
}

#[derive(Debug, Clone, Subcommand)]
pub enum ComposeCommands {
    #[command(about = "Print resolved Compose configuration")]
    Config(ComposeConfigArgs),
    #[command(about = "Pull images referenced by Compose services")]
    Pull(Box<ComposePullArgs>),
}

#[derive(Debug, Clone, Args)]
pub struct ComposeConfigArgs {
    #[arg(value_name = "SERVICE", help = "Compose service to include")]
    pub services: Vec<String>,
    #[arg(
        long,
        conflicts_with_all = ["services_only", "pull_plan", "format"],
        help = "Print resolved service image references"
    )]
    pub images: bool,
    #[arg(
        long = "services",
        conflicts_with_all = ["images", "pull_plan", "format"],
        help = "Print resolved service names"
    )]
    pub services_only: bool,
    #[arg(
        long,
        conflicts_with_all = ["images", "services_only", "format"],
        help = "Print the Compose pull plan"
    )]
    pub pull_plan: bool,
    #[arg(
        long,
        value_enum,
        conflicts_with_all = ["images", "services_only", "pull_plan"],
        help = "Print pocker's resolved Compose pull model in the selected format"
    )]
    pub format: Option<ComposeConfigFormat>,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq, ValueEnum)]
pub enum ComposeConfigFormat {
    Json,
}

#[derive(Debug, Clone, Args)]
pub struct ComposePullArgs {
    #[arg(value_name = "SERVICE", help = "Compose service to pull")]
    pub services: Vec<String>,
    #[command(flatten)]
    pub common: PullCommonArgs,
}

#[derive(Debug, Clone, Args)]
pub struct PullCommonArgs {
    #[command(flatten)]
    pub download: PullDownloadArgs,
    #[command(flatten)]
    pub image_parallel: ImageParallelArgs,
    #[command(flatten)]
    pub retry: RetryArgs,
    #[command(flatten)]
    pub import: ImportArgs,
    #[command(flatten)]
    pub registry: RegistryArgs,
    #[command(flatten)]
    pub auth: AuthArgs,
    #[command(flatten)]
    pub output: PullOutputArgs,
    #[command(flatten)]
    pub cache: CacheSourceArgs,
    #[command(flatten)]
    pub external_registry_connection: ExternalRegistryConnectionArgs,
}

#[derive(Debug, Clone, Args)]
pub struct PullDownloadArgs {
    #[arg(long, help_heading = "Download options", help = platform_help())]
    pub platform: Option<String>,
    #[arg(
        long = "max-parallel-downloads",
        visible_alias = "concurrency",
        default_value_t = 4,
        help_heading = "Download options",
        help = "Maximum concurrent layer downloads"
    )]
    pub concurrency: usize,
}

#[derive(Debug, Clone, Args)]
pub struct ImageParallelArgs {
    #[arg(
        long = "max-parallel-images",
        default_value_t = 4,
        help_heading = "Download options",
        help = "Maximum concurrent image pulls"
    )]
    pub image_concurrency: usize,
}

#[derive(Debug, Clone, Args)]
pub struct RetryArgs {
    #[arg(
        long = "blob-retries",
        value_name = "N",
        allow_hyphen_values = true,
        value_parser = parse_retry_count,
        help_heading = "Retry options",
        help = "Maximum retries for interrupted blob downloads; use -1 for unlimited or 0 to disable [default: 8]"
    )]
    pub blob_retries: Option<i64>,
    #[arg(
        long = "request-retries",
        value_name = "N",
        allow_hyphen_values = true,
        value_parser = parse_retry_count,
        help_heading = "Retry options",
        help = "Maximum retries for registry requests before any response or on retryable HTTP status; use -1 for unlimited or 0 to disable [default: 5]"
    )]
    pub request_retries: Option<i64>,
}

#[derive(Debug, Clone, Args)]
pub struct ImportArgs {
    #[arg(
        long,
        help_heading = "Import options",
        help = "Download into the local cache without importing into Docker"
    )]
    pub no_load: bool,
    #[arg(
        long,
        help_heading = "Import options",
        help = "Keep downloaded layer blobs in the cache after packaging/loading"
    )]
    pub keep_layer_blobs: bool,
    #[arg(
        long = "load-mode",
        value_enum,
        default_value = "stream",
        help_heading = "Import options",
        help = "Docker import mode to use after downloading; registry is experimental and local-daemon only"
    )]
    pub load_mode: LoadMode,
}

#[derive(Debug, Clone, Args)]
pub struct RegistryArgs {
    #[arg(
        long,
        help_heading = "Registry options",
        help = "Use plain HTTP instead of HTTPS for registry requests"
    )]
    pub plain_http: bool,
    #[arg(
        long,
        help_heading = "Registry options",
        help = "Disable TLS certificate verification for registry requests"
    )]
    pub insecure_skip_tls_verify: bool,
    #[arg(
        long,
        help_heading = "Registry options",
        help = "Additional CA certificate bundle in PEM format"
    )]
    pub ca_file: Option<PathBuf>,
}

#[derive(Debug, Clone, Args)]
pub struct AuthArgs {
    #[arg(
        long,
        help_heading = "Registry options",
        help = "Registry username; requires --password-stdin"
    )]
    pub username: Option<String>,
    #[arg(
        long,
        help_heading = "Registry options",
        help = "Read the registry password from stdin"
    )]
    pub password_stdin: bool,
}

#[derive(Debug, Clone, Args)]
pub struct PullOutputArgs {
    #[arg(
        long,
        short = 'q',
        help_heading = "Output options",
        help = "Suppress progress and status output"
    )]
    pub quiet: bool,
}

#[derive(Debug, Clone, Args)]
pub struct CacheSourceArgs {
    #[arg(
        long,
        value_name = "URL",
        help_heading = "Cache options",
        help = "Prefer a pocker cache registry, falling back to upstream on cache misses"
    )]
    pub cache_from: Option<Url>,
    #[arg(
        long,
        requires = "cache_from",
        help_heading = "Cache options",
        help = "Require --cache-from content and do not fall back to upstream"
    )]
    pub cache_only: bool,
}

#[derive(Debug, Clone, Args)]
pub struct ExternalRegistryConnectionArgs {
    #[arg(
        long = "connect-timeout-seconds",
        value_name = "SECONDS",
        default_value_t = DEFAULT_EXTERNAL_CONNECT_TIMEOUT_SECONDS,
        allow_hyphen_values = true,
        value_parser = parse_external_connect_timeout_seconds,
        help_heading = "External connection options",
        help = "Timeout for opening retryable registry/cache HTTP connections; use -1 to disable"
    )]
    pub connect_timeout_seconds: i64,
}

#[derive(Debug, Clone, Args)]
pub struct ServeArgs {
    #[arg(
        long,
        default_value = "127.0.0.1:5000",
        help_heading = "Serve options",
        help = "Address to listen on"
    )]
    pub listen: SocketAddr,
    #[arg(
        long,
        help_heading = "Serve options",
        help = "Fetch missing manifests and blobs from upstream registries"
    )]
    pub pull_missing: bool,
    #[arg(
        long = "max-parallel-downloads",
        visible_alias = "concurrency",
        default_value_t = 4,
        help_heading = "Serve options",
        help = "Maximum concurrent layer downloads"
    )]
    pub concurrency: usize,
    #[command(flatten)]
    pub retry: RetryArgs,
    #[command(flatten)]
    pub registry: RegistryArgs,
    #[command(flatten)]
    pub auth: AuthArgs,
    #[arg(
        long,
        short = 'q',
        help_heading = "Output options",
        help = "Suppress status output"
    )]
    pub quiet: bool,
}

#[derive(Debug, Clone, Args)]
#[command(about = "Manage the local blob and partial-download cache")]
pub struct CacheArgs {
    #[command(subcommand)]
    pub command: CacheCommands,
}

#[derive(Debug, Clone, Subcommand)]
pub enum CacheCommands {
    #[command(about = "Delete cached blobs and partial downloads")]
    Clean(CacheCleanArgs),
}

#[derive(Debug, Clone, Args, Default)]
#[command(about = "Delete cached blobs and partial downloads")]
pub struct CacheCleanArgs {
    #[arg(
        long,
        short = 'v',
        help_heading = "Output options",
        help = "Print each deleted cache file unless --quiet is set"
    )]
    pub verbose: bool,
}

#[derive(Debug, Clone, Args)]
#[command(about = "Run Docker image helper commands")]
pub struct ImageArgs {
    #[command(subcommand)]
    pub command: ImageCommands,
}

#[derive(Debug, Clone, Subcommand)]
pub enum ImageCommands {
    #[command(about = "List Docker images")]
    Ls(ImageLsArgs),
    #[command(about = "Print detailed metadata for a Docker image")]
    Inspect(ImageInspectArgs),
    #[command(about = "Export a Docker image to a tar archive")]
    Save(ImageSaveArgs),
    #[command(about = "Import a Docker image tar archive into Docker")]
    Load(ImageLoadArgs),
}

#[derive(Debug, Clone, Args, Default)]
#[command(about = "List Docker images")]
pub struct ImageLsArgs {}

#[derive(Debug, Clone, Args)]
pub struct ImageInspectArgs {
    #[arg(help = "Image reference to inspect")]
    pub reference: String,
}

#[derive(Debug, Clone, Args)]
pub struct ImageSaveArgs {
    #[arg(help = "Image reference to export")]
    pub reference: String,
    #[arg(long, short = 'o', help = "Destination tar archive path")]
    pub output: PathBuf,
}

#[derive(Debug, Clone, Args)]
pub struct ImageLoadArgs {
    #[arg(long, short = 'i', help = "Source tar archive path")]
    pub input: PathBuf,
}

#[derive(Debug, Clone, Args)]
pub struct PullArgs {
    #[arg(
        value_name = "IMAGE",
        required = true,
        num_args = 1..,
        help = "Image reference(s) to pull"
    )]
    pub references: Vec<String>,
    #[command(flatten)]
    pub common: PullCommonArgs,
    #[arg(
        long,
        help_heading = "Output options",
        help = "Disable animated progress output during pull"
    )]
    pub no_animations: bool,
}

impl ValueEnum for LoadMode {
    fn value_variants<'a>() -> &'a [Self] {
        &[Self::Stream, Self::Registry]
    }

    fn to_possible_value(&self) -> Option<clap::builder::PossibleValue> {
        match self {
            Self::Stream => Some(clap::builder::PossibleValue::new("stream")),
            Self::Registry => Some(clap::builder::PossibleValue::new("registry")),
        }
    }
}

fn default_cache_dir() -> PathBuf {
    directories::ProjectDirs::from("", "", "pocker")
        .map(|dirs| dirs.data_local_dir().to_path_buf())
        .unwrap_or_else(|| PathBuf::from(".pocker"))
}

fn platform_help() -> String {
    format!(
        "Target platform in os/arch[/variant] form, for example {}",
        Platform::host().as_string()
    )
}

fn parse_retry_count(value: &str) -> std::result::Result<i64, String> {
    let retries = value
        .parse::<i64>()
        .map_err(|error| format!("invalid retry count: {error}"))?;
    if retries < -1 {
        return Err("retry count must be -1 or a non-negative integer".into());
    }
    if retries > u32::MAX as i64 {
        return Err(format!("retry count must be no greater than {}", u32::MAX));
    }
    Ok(retries)
}

#[cfg(test)]
mod tests {
    use clap::{CommandFactory, Parser};

    use super::{
        CacheCommands, Cli, Commands, ComposeCommands, ComposeConfigFormat, LoadMode, platform_help,
    };
    use crate::http::DEFAULT_EXTERNAL_CONNECT_TIMEOUT_SECONDS;

    #[test]
    fn pull_external_connect_timeout_defaults_to_20_seconds() {
        let cli = Cli::parse_from(["pocker", "pull", "alpine:latest"]);
        let Commands::Pull(args) = cli.command else {
            panic!("expected pull command");
        };

        assert_eq!(
            args.common
                .external_registry_connection
                .connect_timeout_seconds,
            DEFAULT_EXTERNAL_CONNECT_TIMEOUT_SECONDS
        );
    }

    #[test]
    fn pull_external_connect_timeout_accepts_minus_one_to_disable() {
        let cli = Cli::parse_from([
            "pocker",
            "pull",
            "--connect-timeout-seconds",
            "-1",
            "alpine:latest",
        ]);
        let Commands::Pull(args) = cli.command else {
            panic!("expected pull command");
        };

        assert_eq!(
            args.common
                .external_registry_connection
                .connect_timeout_seconds,
            -1
        );
    }

    #[test]
    fn pull_external_connect_timeout_rejects_lower_negative_values() {
        let error = Cli::try_parse_from([
            "pocker",
            "pull",
            "--connect-timeout-seconds=-2",
            "alpine:latest",
        ])
        .expect_err("external connect timeout should only accept -1 or non-negative values");

        assert_eq!(error.kind(), clap::error::ErrorKind::ValueValidation);
    }

    #[test]
    fn compose_pull_accepts_external_connect_timeout() {
        let cli = Cli::parse_from([
            "pocker",
            "compose",
            "pull",
            "--connect-timeout-seconds",
            "-1",
        ]);
        let Commands::Compose(args) = cli.command else {
            panic!("expected compose command");
        };
        let ComposeCommands::Pull(pull) = args.command else {
            panic!("expected compose pull command");
        };

        assert_eq!(
            pull.common
                .external_registry_connection
                .connect_timeout_seconds,
            -1
        );
    }

    #[test]
    fn pull_accepts_no_animations_flag() {
        let cli = Cli::parse_from(["pocker", "pull", "--no-animations", "alpine:latest"]);
        let Commands::Pull(args) = cli.command else {
            panic!("expected pull command");
        };

        assert!(args.no_animations);
        assert_eq!(args.references, vec!["alpine:latest"]);
    }

    #[test]
    fn pull_accepts_multiple_references() {
        let cli = Cli::parse_from(["pocker", "pull", "alpine:latest", "busybox:latest"]);
        let Commands::Pull(args) = cli.command else {
            panic!("expected pull command");
        };

        assert_eq!(args.references, vec!["alpine:latest", "busybox:latest"]);
        assert_eq!(args.common.image_parallel.image_concurrency, 4);
    }

    #[test]
    fn pull_rejects_missing_reference() {
        let error = Cli::try_parse_from(["pocker", "pull"])
            .expect_err("pull should require at least one image");

        assert_eq!(
            error.kind(),
            clap::error::ErrorKind::MissingRequiredArgument
        );
    }

    #[test]
    fn pull_accepts_max_parallel_images() {
        let cli = Cli::parse_from([
            "pocker",
            "pull",
            "--max-parallel-images",
            "4",
            "alpine:latest",
            "busybox:latest",
        ]);
        let Commands::Pull(args) = cli.command else {
            panic!("expected pull command");
        };

        assert_eq!(args.common.image_parallel.image_concurrency, 4);
        assert_eq!(args.references, vec!["alpine:latest", "busybox:latest"]);
    }

    #[test]
    fn pull_accepts_quiet_short_flag() {
        let cli = Cli::parse_from(["pocker", "pull", "-q", "alpine:latest"]);
        let Commands::Pull(args) = cli.command else {
            panic!("expected pull command");
        };

        assert!(args.common.output.quiet);
    }

    #[test]
    fn pull_accepts_registry_load_mode() {
        let cli = Cli::parse_from(["pocker", "pull", "--load-mode", "registry", "alpine:latest"]);
        let Commands::Pull(args) = cli.command else {
            panic!("expected pull command");
        };

        assert_eq!(args.common.import.load_mode, LoadMode::Registry);
    }

    #[test]
    fn pull_accepts_blob_retries_flag() {
        let cli = Cli::parse_from(["pocker", "pull", "--blob-retries", "32", "alpine:latest"]);
        let Commands::Pull(args) = cli.command else {
            panic!("expected pull command");
        };

        assert_eq!(args.common.retry.blob_retries, Some(32));
    }

    #[test]
    fn pull_accepts_zero_blob_retries() {
        let cli = Cli::parse_from(["pocker", "pull", "--blob-retries", "0", "alpine:latest"]);
        let Commands::Pull(args) = cli.command else {
            panic!("expected pull command");
        };

        assert_eq!(args.common.retry.blob_retries, Some(0));
    }

    #[test]
    fn pull_accepts_unlimited_blob_retries() {
        let cli = Cli::parse_from(["pocker", "pull", "--blob-retries", "-1", "alpine:latest"]);
        let Commands::Pull(args) = cli.command else {
            panic!("expected pull command");
        };

        assert_eq!(args.common.retry.blob_retries, Some(-1));
    }

    #[test]
    fn pull_rejects_retry_values_below_minus_one() {
        let error = Cli::try_parse_from(["pocker", "pull", "--blob-retries=-2", "alpine:latest"])
            .expect_err("retry count should only accept -1 or non-negative values");

        assert_eq!(error.kind(), clap::error::ErrorKind::ValueValidation);
    }

    #[test]
    fn pull_rejects_removed_retry_forever_flag() {
        let error = Cli::try_parse_from(["pocker", "pull", "--retry-forever", "alpine:latest"])
            .expect_err("retry-forever should be removed");

        assert_eq!(error.kind(), clap::error::ErrorKind::UnknownArgument);
    }

    #[test]
    fn pull_accepts_request_retries_flag() {
        let cli = Cli::parse_from(["pocker", "pull", "--request-retries", "12", "alpine:latest"]);
        let Commands::Pull(args) = cli.command else {
            panic!("expected pull command");
        };

        assert_eq!(args.common.retry.request_retries, Some(12));
    }

    #[test]
    fn pull_accepts_cache_from_url() {
        let cli = Cli::parse_from([
            "pocker",
            "pull",
            "--cache-from",
            "http://127.0.0.1:5000",
            "alpine:latest",
        ]);
        let Commands::Pull(args) = cli.command else {
            panic!("expected pull command");
        };

        assert_eq!(
            args.common
                .cache
                .cache_from
                .as_ref()
                .map(|url| url.as_str()),
            Some("http://127.0.0.1:5000/")
        );
    }

    #[test]
    fn pull_accepts_cache_only_with_cache_from() {
        let cli = Cli::parse_from([
            "pocker",
            "pull",
            "--cache-from",
            "http://127.0.0.1:5000",
            "--cache-only",
            "alpine:latest",
        ]);
        let Commands::Pull(args) = cli.command else {
            panic!("expected pull command");
        };

        assert!(args.common.cache.cache_only);
    }

    #[test]
    fn pull_rejects_cache_only_without_cache_from() {
        let error = Cli::try_parse_from(["pocker", "pull", "--cache-only", "alpine:latest"])
            .expect_err("cache-only requires cache-from");

        assert_eq!(
            error.kind(),
            clap::error::ErrorKind::MissingRequiredArgument
        );
    }

    #[test]
    fn pull_rejects_invalid_cache_from_url() {
        let error = Cli::try_parse_from([
            "pocker",
            "pull",
            "--cache-from",
            "not-a-url",
            "alpine:latest",
        ])
        .expect_err("invalid cache URL should be rejected");

        assert_eq!(error.kind(), clap::error::ErrorKind::ValueValidation);
    }

    #[test]
    fn serve_parses_defaults() {
        let cli = Cli::parse_from(["pocker", "serve"]);
        let Commands::Serve(args) = cli.command else {
            panic!("expected serve command");
        };

        assert_eq!(args.listen.to_string(), "127.0.0.1:5000");
        assert!(!args.pull_missing);
    }

    #[test]
    fn serve_accepts_pull_missing_and_listen() {
        let cli = Cli::parse_from([
            "pocker",
            "serve",
            "--pull-missing",
            "--listen",
            "0.0.0.0:5000",
        ]);
        let Commands::Serve(args) = cli.command else {
            panic!("expected serve command");
        };

        assert_eq!(args.listen.to_string(), "0.0.0.0:5000");
        assert!(args.pull_missing);
    }

    #[test]
    fn serve_accepts_quiet_short_flag() {
        let cli = Cli::parse_from(["pocker", "serve", "-q"]);
        let Commands::Serve(args) = cli.command else {
            panic!("expected serve command");
        };

        assert!(args.quiet);
    }

    #[test]
    fn serve_accepts_unlimited_request_retries() {
        let cli = Cli::parse_from(["pocker", "serve", "--request-retries", "-1"]);
        let Commands::Serve(args) = cli.command else {
            panic!("expected serve command");
        };

        assert_eq!(args.retry.request_retries, Some(-1));
    }

    #[test]
    fn pull_accepts_zero_request_retries() {
        let cli = Cli::parse_from(["pocker", "pull", "--request-retries", "0", "alpine:latest"]);
        let Commands::Pull(args) = cli.command else {
            panic!("expected pull command");
        };

        assert_eq!(args.common.retry.request_retries, Some(0));
    }

    #[test]
    fn cache_clean_parses() {
        for verbose_flag in [None, Some("--verbose"), Some("-v")] {
            let mut command = vec!["pocker", "cache", "clean"];
            if let Some(flag) = verbose_flag {
                command.push(flag);
            }

            let cli = Cli::parse_from(command);
            let Commands::Cache(args) = cli.command else {
                panic!("expected cache command");
            };
            let CacheCommands::Clean(args) = args.command;

            assert_eq!(args.verbose, verbose_flag.is_some());
        }
    }

    #[test]
    fn compose_pull_accepts_repeated_file_flags() {
        let cli = Cli::parse_from([
            "pocker",
            "compose",
            "-f",
            "compose.yml",
            "-f",
            "compose.override.yml",
            "pull",
            "app",
        ]);
        let Commands::Compose(args) = cli.command else {
            panic!("expected compose command");
        };
        let ComposeCommands::Pull(pull) = args.command else {
            panic!("expected compose pull command");
        };

        assert_eq!(args.file.len(), 2);
        assert_eq!(pull.services, vec!["app"]);
    }

    #[test]
    fn compose_pull_accepts_quiet_short_flag() {
        let cli = Cli::parse_from(["pocker", "compose", "pull", "-q"]);
        let Commands::Compose(args) = cli.command else {
            panic!("expected compose command");
        };
        let ComposeCommands::Pull(pull) = args.command else {
            panic!("expected compose pull command");
        };

        assert!(pull.common.output.quiet);
    }

    #[test]
    fn compose_pull_accepts_unlimited_blob_retries() {
        let cli = Cli::parse_from(["pocker", "compose", "pull", "--blob-retries", "-1"]);
        let Commands::Compose(args) = cli.command else {
            panic!("expected compose command");
        };
        let ComposeCommands::Pull(pull) = args.command else {
            panic!("expected compose pull command");
        };

        assert_eq!(pull.common.retry.blob_retries, Some(-1));
    }

    #[test]
    fn compose_pull_accepts_cache_only_with_cache_from() {
        let cli = Cli::parse_from([
            "pocker",
            "compose",
            "pull",
            "--cache-from",
            "http://127.0.0.1:5000",
            "--cache-only",
        ]);
        let Commands::Compose(args) = cli.command else {
            panic!("expected compose command");
        };
        let ComposeCommands::Pull(pull) = args.command else {
            panic!("expected compose pull command");
        };

        assert!(pull.common.cache.cache_only);
    }

    #[test]
    fn compose_config_accepts_images_and_service_names() {
        let cli = Cli::parse_from([
            "pocker",
            "compose",
            "-f",
            "compose.yml",
            "config",
            "--images",
            "app",
        ]);
        let Commands::Compose(args) = cli.command else {
            panic!("expected compose command");
        };
        let ComposeCommands::Config(config) = args.command else {
            panic!("expected compose config command");
        };

        assert!(config.images);
        assert_eq!(config.services, vec!["app"]);
    }

    #[test]
    fn compose_config_accepts_json_format() {
        let cli = Cli::parse_from([
            "pocker",
            "compose",
            "-f",
            "compose.yml",
            "config",
            "--format",
            "json",
        ]);
        let Commands::Compose(args) = cli.command else {
            panic!("expected compose command");
        };
        let ComposeCommands::Config(config) = args.command else {
            panic!("expected compose config command");
        };

        assert_eq!(config.format, Some(ComposeConfigFormat::Json));
    }

    #[test]
    fn compose_config_rejects_conflicting_output_modes() {
        for args in [
            ["pocker", "compose", "config", "--images", "--pull-plan"].as_slice(),
            [
                "pocker", "compose", "config", "--images", "--format", "json",
            ]
            .as_slice(),
        ] {
            let error = Cli::try_parse_from(args).expect_err("output modes should conflict");
            assert_eq!(error.kind(), clap::error::ErrorKind::ArgumentConflict);
        }
    }

    #[test]
    fn pull_help_shows_host_platform_example() {
        let mut help = Vec::new();
        Cli::command()
            .find_subcommand("pull")
            .expect("pull command should exist")
            .clone()
            .write_long_help(&mut help)
            .expect("help should render");
        let help = String::from_utf8(help).expect("help should be utf8");

        assert!(help.contains(&platform_help()));
        assert!(help.contains("Download options"));
        assert!(help.contains("Retry options"));
        assert!(help.contains("use -1 for unlimited"));
        assert!(!help.contains("--retry-forever"));
    }
}
