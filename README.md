# pocker

`pocker` is a resumable OCI image puller written in Rust.

It pulls images directly from registries, resumes interrupted downloads from a local cache, and imports the final image into Docker.

## Features

- Resumable layer downloads
- Docker credential helper and `config.json` auth support
- Docker image list, inspect, save, and load commands
- Configurable retry behavior for retryable registry failures

## Install

```bash
cargo install --path .
```

Or build a local release binary:

```bash
cargo build --release
./target/release/pocker version
```

## Usage

Pull an image into Docker:

```bash
pocker pull alpine:latest
```

Pull a specific platform:

```bash
pocker pull --platform linux/arm64 ghcr.io/example/app:latest
```

Increase blob retries on unstable links:

```bash
pocker pull --blob-retries 32 ghcr.io/example/app:latest
```

Retry indefinitely on unstable links:

```bash
pocker pull --blob-retries 0 ghcr.io/example/app:latest
```

Increase registry request retries for flaky pre-response failures:

```bash
pocker pull --request-retries 16 ghcr.io/example/app:latest
```

Pull a private image:

```bash
printf '%s' "$REGISTRY_PASSWORD" | \
  pocker pull \
    --username my-user \
    --password-stdin \
    ghcr.io/example/private-image:latest
```

Docker image helpers:

```bash
pocker cache clean
pocker image ls
pocker image inspect alpine:latest
pocker image save alpine:latest --output alpine.tar
pocker image load --input alpine.tar
```

See full help:

```bash
pocker --help
pocker cache --help
pocker pull --help
pocker image --help
```

## Notes

- Docker access uses `DOCKER_HOST` if set, otherwise `/var/run/docker.sock`
- Registry auth is reused from Docker config when available
- Use `--cache-dir` to override the default local cache location
- Use `pocker cache clean` to wipe and recreate the local cache directory
- Use `--blob-retries` to raise the retry budget for unstable connections; `0` means unlimited retries
- Use `--request-retries` to raise the retry budget for request/connect/503-style failures; `0` means unlimited retries
- Registry retries are bounded by default; setting a retry flag to `0` makes that retry path unlimited

## Support Matrix

| Environment | Status | Notes |
| --- | --- | --- |
| Linux + Docker | Supported | Validated in CI with end-to-end smoke tests |
| macOS | Not validated | No release claim for 0.1.0 |
| Windows | Not validated | No release claim for 0.1.0 |

## Runtime Requirements

- Docker image workflows require access to a Docker daemon socket
- Private registry pulls require either `--username` with `--password-stdin` or Docker config-based auth
- Plain HTTP registries require `--plain-http`
