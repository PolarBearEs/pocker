# pocker

`pocker` is a resumable OCI image puller written in Rust.

It pulls images directly from registries, resumes interrupted downloads from a local cache, and imports the final image into Docker.

## Features

- Resumable layer downloads
- Docker credential helper and `config.json` auth support
- Docker image list, inspect, save, and load commands
- Bounded retry behavior for retryable registry failures

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
pocker image ls
pocker image inspect alpine:latest
pocker image save alpine:latest --output alpine.tar
pocker image load --input alpine.tar
```

See full help:

```bash
pocker --help
pocker pull --help
pocker image --help
```

## Notes

- Docker access uses `DOCKER_HOST` if set, otherwise `/var/run/docker.sock`
- Registry auth is reused from Docker config when available
- Use `--cache-dir` to override the default local cache location
- Registry retries are bounded; persistent retryable failures exit with a non-zero status

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

## Known Limitations

- Release artifacts are built for Linux targets only
- Registry retry warnings are surfaced through the CLI and tracing output, but the retry budget is fixed in 0.1.0
