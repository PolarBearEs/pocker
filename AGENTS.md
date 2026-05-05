# AGENTS.md

## Before Commit

Before committing changes in this repo:

1. Run `cargo fmt`.
2. Run `cargo clippy --all-targets --all-features -- -D warnings`.
3. Run `cargo test --all-targets --all-features`.
4. Run `cargo build --locked`.

Notes:

- CI also runs Docker and registry-behavior smoke tests, but they are not part of the default local pre-commit checklist unless your change affects that behavior directly.
- If `cargo build --locked` fails because `Cargo.lock` is stale, regenerate it before committing.
- If you changed the crate version in `Cargo.toml`, make sure the root package version in `Cargo.lock` is updated too.
