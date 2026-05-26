use clap::Parser;
use tracing_subscriber::EnvFilter;

use crate::cli::Cli;
use crate::error::Result;

pub async fn run() -> Result<()> {
    init_tracing();

    let cli = Cli::parse();
    crate::commands::execute(cli).await
}

fn init_tracing() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("warn"));
    let _ = tracing_subscriber::fmt().with_env_filter(filter).try_init();
}
