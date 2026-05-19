//! Internal library backing the `pocker` binary and its tests.
//!
//! This crate is not published to crates.io and does not provide a stable
//! public API: module layout, items, and signatures may change between
//! releases without notice. The modules are `pub` only so the binary
//! target and future integration tests can reach them.

pub mod auth;
pub mod cli;
pub mod docker;
pub mod error;
pub mod export;
pub mod http;
pub mod image;
pub mod platform;
pub mod pull;
pub mod reference;
pub mod registry;
pub mod serve_registry;
pub mod store;
pub mod ui;
