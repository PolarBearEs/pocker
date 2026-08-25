# pocker

`pocker` pulls OCI container images from registries with a local, resumable cache.

It is built for slow, flaky, or repeated pulls where downloading the same layer
again is wasted time. By default, `pocker pull` downloads the selected image
layers, stores them in pocker's cache, packages the image, and imports it into
Docker.

## What pocker does

- Resumes interrupted image layer downloads.
- Reuses cached layers across future pulls.
- Pulls images referenced by Compose projects.
- Serves the local cache as an OCI registry-compatible cache.
- Talks to Docker through its API; the Docker CLI and Docker Compose CLI are not
  required.

## Install

Install the latest prebuilt binary:

```bash
curl -fsSL https://github.com/PolarBearEs/pocker/releases/latest/download/install.sh | sh
```

Install a specific release or install somewhere else:

```bash
curl -fsSL https://github.com/PolarBearEs/pocker/releases/download/v0.2.3/install.sh | \
  POCKER_VERSION=v0.2.3 POCKER_INSTALL_DIR=/usr/local/bin sh
```

Build from source:

```bash
cargo install --path .
```

## Quick Start

Pull an image into Docker:

```bash
pocker pull alpine:latest
```

Pull the images referenced by a Compose project:

```bash
pocker compose pull
```

Enable optional Compose services with the same profile behavior as Docker Compose:

```bash
pocker compose --profile tools pull
```

## Documentation

- [Usage guide](https://github.com/PolarBearEs/pocker/wiki/Usage)
- [Runtime requirements](https://github.com/PolarBearEs/pocker/wiki/Runtime-Requirements)
- [Platform support](https://github.com/PolarBearEs/pocker/wiki/Platform-Support)
