//! Internal library backing the `pocker` binary. Not a stable public API.

mod auth;
mod cli;
mod commands;
mod digest;
mod docker;
mod error;
mod export;
mod http;
mod image;
mod image_view;
mod platform;
mod pull;
mod reference;
mod registry;
mod retry;
mod serve_registry;
mod signal;
mod store;
mod ui;
mod units;

pub use cli::Cli;
pub use error::{DockerPullError as Error, Result};

pub async fn run(cli: Cli) -> Result<()> {
    commands::execute(cli).await
}
