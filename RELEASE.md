# pocker v0.1.4

## Highlights

- Added Docker Compose image discovery and pull support.
- Added cache serving so one pocker instance can expose its local cache as an OCI registry-compatible pull-through cache.
- Added an experimental local registry load mode for importing pulled images into Docker without using Docker's archive load path.
- Switched Docker image loads to stream archives instead of buffering the full image archive first.
- Added a `curl | sh` installer for prebuilt Linux and macOS release binaries.
- Linux release binaries now target a glibc 2.17 baseline with `cargo-zigbuild` to avoid depending on the GitHub runner's host glibc version.

## New Commands and Flags

- `pocker compose config --images` prints the registry images referenced by a Compose project.
- `pocker compose pull [SERVICE]...` pulls Compose service images, with repeated `-f/--file`, service selection, `.env` interpolation, `include`, and `extends` support.
- `pocker compose pull --max-parallel-images N` pulls multiple Compose images concurrently.
- `pocker serve --listen HOST:PORT` serves the local pocker cache as an OCI registry-compatible cache.
- `pocker serve --pull-missing` lets the cache server fetch missing manifests and blobs from upstream registries.
- `pocker pull --cache-from URL` pulls through another pocker cache server.
- `pocker pull --cache-only` requires all content to come from the configured cache server.
- `pocker pull --load-mode registry` imports via a temporary local registry instead of Docker's archive load API.

## Improvements

- Pulls now check whether the target image is already loaded in Docker before planning layer downloads.
- Registry request handling was refactored to make manifest and blob fallback behavior clearer and more reliable.
- Cache misses can now fall back to upstream registries when configured, while cache-only paths return misses without upstream requests.
- Docker image archive loading now streams data to Docker, reducing temporary memory and disk pressure for large images.
- OCI archive export handles digest references and fallback manifest media types more consistently.
- Non-TTY UI output was expanded for Compose and multi-image pull progress.
- CI now reuses Linux smoke-test builds and includes registry-behavior smoke coverage.

## Installer

The installer detects the local OS and CPU architecture, downloads the matching release asset, and installs `pocker` to `~/.local/bin` by default:

```bash
curl -fsSL https://github.com/PolarBearEs/pocker/releases/latest/download/install.sh | sh
```

To install this exact release into a custom directory:

```bash
curl -fsSL https://github.com/PolarBearEs/pocker/releases/download/v0.1.4/install.sh | \
  POCKER_VERSION=v0.1.4 POCKER_INSTALL_DIR=/usr/local/bin sh
```

Installer downloads are verified against the SHA256 digest GitHub publishes for each release asset before the binary is installed. The installer queries GitHub release metadata for the selected asset, reads the asset's `sha256` digest, computes the local SHA256 of the downloaded binary, and installs only when the digests match.

Linux assets are built with `cargo-zigbuild` against a glibc 2.17 baseline to avoid glibc version mismatches caused by native GitHub runner builds on newer distributions.

## Release Assets

- `pocker-linux-x86_64`
- `pocker-linux-arm64`
- `pocker-linux-armv7`
- `pocker-macos-arm64`
- `pocker-macos-x86_64`
- `pocker-windows-x86_64.exe`
- `pocker-windows-arm64.exe`
- `install.sh`

## Changes Since v0.1.3

- `bd822ed` Reuse Linux CI build for smoke tests (#15)
- `c12d4d5` Compose pull (#16)
- `cf673a2` Stream Docker image loads (#17)
- `6979242` Add local registry load mode (#18)
- `70d8bef` Feature/pocker cache serve (#19)
- `37ca5b7` Check loaded images before layer planning (#20)
- `0d0c37a` docs: add commit history guidance
- `d82175b` Refactor pull and registry helpers (#21)
- `ee9aa4f` Add release install script (#22)
