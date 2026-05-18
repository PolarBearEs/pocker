# pocker

`pocker` is a resumable OCI image puller written in Rust.

It pulls images directly from registries, resumes interrupted downloads from a local cache, and imports the final image into Docker.

## Features

- Resumable layer downloads
- Docker credential helper and `config.json` auth support
- Native Docker Compose image discovery and pull support
- Docker image list, inspect, save, and load commands
- Configurable retry behavior for retryable registry failures

## Install

Install the latest prebuilt binary:

```bash
curl -fsSL https://github.com/PolarBearEs/pocker/releases/latest/download/install.sh | sh
```

To install a specific release or install somewhere else:

```bash
curl -fsSL https://github.com/PolarBearEs/pocker/releases/download/v0.1.5/install.sh | \
  POCKER_VERSION=v0.1.5 POCKER_INSTALL_DIR=/usr/local/bin sh
```

Or install from source:

```bash
cargo install --path .
```

Or build a local release binary:

```bash
cargo build --release
./target/release/pocker version
```

Prebuilt release binaries are published for the targets in the support matrix.

## Usage

Pull an image into Docker:

```bash
pocker pull alpine:latest
```

Pull a specific platform:

```bash
pocker pull --platform linux/arm64 ghcr.io/example/app:latest
```

Pull through another pocker cache server:

```bash
pocker pull --cache-from http://cache.example:5000 alpine:latest
```

The cache server sees normalized upstream image paths, so `alpine:latest` is served
through a path like `cache.example:5000/registry-1.docker.io/library/alpine:latest`.
By default, cache misses fall back to the upstream registry. To require that all
content comes from the cache server:

```bash
pocker pull --cache-from http://cache.example:5000 --cache-only alpine:latest
```

Import through a temporary local registry instead of Docker's archive load API:

```bash
pocker pull --load-mode registry ghcr.io/example/app:latest
```

Increase blob retries on unstable links:

```bash
pocker pull --blob-retries 32 ghcr.io/example/app:latest
```

Retry indefinitely on unstable links:

```bash
pocker pull --retry-forever ghcr.io/example/app:latest
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

Inspect images referenced by a Compose project:

```bash
pocker compose config --images
pocker compose -f docker-compose.prod.yml config --images api worker
```

Pull Compose service images:

```bash
pocker compose pull
pocker compose -f docker-compose.prod.yml pull api worker
pocker compose pull --cache-from http://cache.example:5000
```

Pull more than one Compose image at a time:

```bash
pocker compose pull --max-parallel-images 4
```

Serve the local pocker cache as an OCI registry-compatible cache:

```bash
pocker serve --listen 0.0.0.0:5000
```

By default, `pocker serve` is cache-only and returns `404` for missing content. To let
the serving instance fetch missing manifests and blobs from upstream registries:

```bash
pocker serve --listen 0.0.0.0:5000 --pull-missing
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
pocker serve --help
pocker compose --help
pocker image --help
```

## Notes

- Docker access uses `DOCKER_HOST` if set, otherwise `/var/run/docker.sock`
- Registry auth is reused from Docker config when available
- Use `--cache-dir` to override the default local cache location
- Use `pocker serve` with `pocker pull --cache-from` to pull through another pocker cache
- Use `pocker cache clean` to wipe and recreate the local cache directory
- Use `--blob-retries` to set the retry budget for unstable blob downloads; `0` disables blob retries
- Use `--request-retries` to set the retry budget for request/connect/503-style failures; `0` disables request retries
- Use `--retry-forever` to retry retryable blob downloads and registry requests indefinitely
- `pocker serve` is cache-only by default; `--pull-missing` explicitly allows upstream registry requests
- Upstream auth for `pocker serve --pull-missing` is resolved on the serving instance, not forwarded by clients
- `--load-mode registry` is experimental and requires the Docker daemon to reach pocker on `127.0.0.1`; use the default `stream` mode for remote Docker daemons or unsupported environments
- `pocker compose` parses Compose files itself and does not require the Docker Compose CLI
- Compose file selection supports default compose file discovery and repeated `-f/--file`
- Compose image discovery supports service `image`, `.env` interpolation, `include`, and `extends`; build-only services are reported as skipped because there is no registry image to pull
- `pocker compose pull [SERVICE]...` pulls only the selected services when service names are provided

## Support Matrix

| Environment | Status | Notes |
| --- | --- | --- |
| Linux x86_64 + Docker | Supported | Built, unit-tested, and smoke-tested in CI |
| Linux arm64 + Docker | Supported | Built, unit-tested, and smoke-tested in CI |
| Linux armv7 | Build-checked | Built in CI with `cross`, but not native runtime-validated |
| macOS arm64 | CI-checked | Built and unit-tested in CI, but not runtime-validated |
| macOS x86_64 | CI-checked | Built and unit-tested in CI, but not runtime-validated |
| Windows x64 | CI-checked | Built and unit-tested in CI, but not runtime-validated |
| Windows arm64 | CI-checked | Built and unit-tested in CI, but not runtime-validated |

## Runtime Requirements

- Docker image workflows require access to a Docker daemon socket
- Private registry pulls require either `--username` with `--password-stdin` or Docker config-based auth
- Plain HTTP registries require `--plain-http`
- Plain HTTP pocker cache servers can be used with `--cache-from http://host:port`
