# AGENTS.md

## Design Guidelines

- Assume pocker is used on unreliable, slow, or constrained devices and networks. Avoid total-duration timeouts for blob transfers or other operations that can legitimately take a long time while still making progress.
- Prefer idle/progress timeouts over absolute timeouts for network and file streaming paths.
- Be careful with cache coordination: multiple images, tasks, or processes may request the same layer at the same time. Preserve deduplication, resumability, and cache integrity.
- Do not optimize for the happy path at the expense of flaky-network behavior. Retries, cancellation, and partial progress should remain first-class.
- When adding limits, concurrency controls, or timeouts, document what the limit protects against and why the default is appropriate for slow environments.

## Before Commit

Before committing changes in this repo:

1. Run `cargo fmt`.
2. Run `cargo clippy --workspace --all-targets --all-features -- -D warnings`.
3. Run `cargo test --workspace --all-targets --all-features`.
4. Run `cargo build --workspace --locked`.

Notes:

- Registry behavior is covered by Rust tests. CI also runs Docker smoke tests, but they are not part of the default local pre-commit checklist unless your change affects that behavior directly.
- If `cargo build --locked` fails because `Cargo.lock` is stale, regenerate it before committing.
- If you changed the crate version in `Cargo.toml`, make sure the root package version in `Cargo.lock` is updated too.

## Commit History

- Do not amend, squash, rebase, or otherwise rewrite existing commits unless the user explicitly asks for that history change.
- When a follow-up change could be either a new commit or an amendment, ask before choosing.
