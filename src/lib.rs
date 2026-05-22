//! Internal library backing the `pocker` binary. Not a stable public API.

mod auth;
mod cli;
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
mod runtime;
mod serve_registry;
mod signal;
mod store;
mod ui;

pub use error::{DockerPullError as Error, Result};
pub use runtime::run;
