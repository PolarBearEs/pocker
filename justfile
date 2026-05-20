set shell := ["bash", "-eu", "-o", "pipefail", "-c"]

default:
  @just --list

install-cross:
  cargo install cross --locked

check:
  cargo check

precommit:
  cargo fmt
  cargo clippy --all-targets --all-features -- -D warnings
  cargo test --all-targets --all-features
  cargo build --locked

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
  cargo zigbuild --release --target x86_64-unknown-linux-gnu.2.17

build-arm64:
  cargo zigbuild --release --target aarch64-unknown-linux-gnu.2.17

build-armv7:
  cargo zigbuild --release --target armv7-unknown-linux-gnueabihf.2.17

build-all: build-amd64 build-arm64 build-armv7

artifacts:
  @echo target/x86_64-unknown-linux-gnu/release/pocker
  @echo target/aarch64-unknown-linux-gnu/release/pocker
  @echo target/armv7-unknown-linux-gnueabihf/release/pocker
