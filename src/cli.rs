use std::net::SocketAddr;
use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(name = "pocker")]
#[command(about = "Resumable OCI registry image puller")]
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
    Pull(PullArgs),
    Serve(ServeArgs),
    Image(ImageArgs),
    Images(ImageLsArgs),
    Version,
}

#[derive(Debug, Clone, Args)]
pub struct ImageArgs {
    #[command(subcommand)]
    pub command: ImageCommands,
}

#[derive(Debug, Clone, Subcommand)]
pub enum ImageCommands {
    Ls(ImageLsArgs),
    Inspect(ImageInspectArgs),
    Save(ImageSaveArgs),
    Load(ImageLoadArgs),
}

#[derive(Debug, Clone, Args, Default)]
pub struct ImageLsArgs {}

#[derive(Debug, Clone, Args)]
pub struct ImageInspectArgs {
    pub reference: String,
}

#[derive(Debug, Clone, Args)]
pub struct ImageSaveArgs {
    pub reference: String,
    #[arg(long, short = 'o')]
    pub output: PathBuf,
}

#[derive(Debug, Clone, Args)]
pub struct ImageLoadArgs {
    #[arg(long, short = 'i')]
    pub input: PathBuf,
}

#[derive(Debug, Clone, Args)]
pub struct PullArgs {
    pub reference: String,
    #[arg(long)]
    pub platform: Option<String>,
    #[arg(
        long = "max-parallel-downloads",
        visible_alias = "concurrency",
        default_value_t = 4
    )]
    pub concurrency: usize,
    #[arg(long)]
    pub no_load: bool,
    #[arg(long)]
    pub keep_layer_blobs: bool,
    #[arg(long)]
    pub plain_http: bool,
    #[arg(long)]
    pub insecure_skip_tls_verify: bool,
    #[arg(long)]
    pub ca_file: Option<PathBuf>,
    #[arg(long)]
    pub username: Option<String>,
    #[arg(long)]
    pub password_stdin: bool,
    #[arg(long)]
    pub quiet: bool,
}

#[derive(Debug, Clone, Args)]
pub struct ServeArgs {
    #[arg(
        long,
        default_value = "127.0.0.1:5100",
        help = "Bind address for the local cache registry; use 127.0.0.1 for Docker pulls"
    )]
    pub bind: SocketAddr,
}

fn default_cache_dir() -> PathBuf {
    directories::ProjectDirs::from("", "", "pocker")
        .map(|dirs| dirs.data_local_dir().to_path_buf())
        .unwrap_or_else(|| PathBuf::from(".pocker"))
}
