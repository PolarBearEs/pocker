set shell := ["bash", "-eu", "-o", "pipefail", "-c"]

default:
  @just --list

install-cross:
  cargo install cross --locked

check:
  cargo check

test:
  cargo test

build:
  cargo build

build-release:
  cargo build --release

smoke-registry:
  cargo build
  ./ci/smoke-registry-behavior.sh target/debug/pocker

smoke-docker:
  cargo build
  ./ci/smoke-docker.sh target/debug/pocker

run *args:
  cargo run -- {{args}}

build-amd64:
  cross build --release --target x86_64-unknown-linux-gnu

build-arm64:
  cross build --release --target aarch64-unknown-linux-gnu

build-armv7:
  cross build --release --target armv7-unknown-linux-gnueabihf

build-all: build-amd64 build-arm64 build-armv7

artifacts:
  @echo target/x86_64-unknown-linux-gnu/release/pocker
  @echo target/aarch64-unknown-linux-gnu/release/pocker
  @echo target/armv7-unknown-linux-gnueabihf/release/pocker
