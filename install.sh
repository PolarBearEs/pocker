#!/bin/sh
set -eu

repo="PolarBearEs/pocker"
bin_name="pocker"
version="${POCKER_VERSION:-latest}"

die() {
  printf 'pocker install: %s\n' "$1" >&2
  exit 1
}

need_cmd() {
  command -v "$1" >/dev/null 2>&1 || die "missing required command: $1"
}

detect_asset() {
  os="$(uname -s)"
  arch="$(uname -m)"

  case "$os" in
    Linux)
      case "$arch" in
        x86_64 | amd64) asset="pocker-linux-x86_64" ;;
        aarch64 | arm64) asset="pocker-linux-arm64" ;;
        armv7l | armv7*) asset="pocker-linux-armv7" ;;
        *) die "unsupported Linux architecture: $arch" ;;
      esac
      ;;
    Darwin)
      case "$arch" in
        x86_64 | amd64) asset="pocker-macos-x86_64" ;;
        aarch64 | arm64) asset="pocker-macos-arm64" ;;
        *) die "unsupported macOS architecture: $arch" ;;
      esac
      ;;
    *)
      die "unsupported operating system: $os"
      ;;
  esac
}

download() {
  url="$1"
  output="$2"

  if command -v curl >/dev/null 2>&1; then
    curl -fsSL "$url" -o "$output"
  elif command -v wget >/dev/null 2>&1; then
    wget -qO "$output" "$url"
  else
    die "missing required command: curl or wget"
  fi
}

verify_checksum() {
  checksum_file="$1"

  if command -v sha256sum >/dev/null 2>&1; then
    (cd "$tmp_dir" && sha256sum -c "$checksum_file")
  elif command -v shasum >/dev/null 2>&1; then
    (cd "$tmp_dir" && shasum -a 256 -c "$checksum_file")
  else
    die "missing required command: sha256sum or shasum"
  fi
}

path_contains() {
  case ":${PATH:-}:" in
    *":$1:"*) return 0 ;;
    *) return 1 ;;
  esac
}

if [ "${POCKER_INSTALL_DIR:-}" ]; then
  install_dir="$POCKER_INSTALL_DIR"
else
  [ "${HOME:-}" ] || die "HOME is not set; set POCKER_INSTALL_DIR to choose an install directory"
  install_dir="$HOME/.local/bin"
fi

need_cmd uname
need_cmd mktemp
need_cmd chmod
need_cmd mkdir

detect_asset

if [ "$version" = "latest" ]; then
  base_url="https://github.com/$repo/releases/latest/download"
else
  base_url="https://github.com/$repo/releases/download/$version"
fi

url="$base_url/$asset"
checksum_url="$base_url/$asset.sha256"

tmp_dir="$(mktemp -d)"
trap 'rm -rf "$tmp_dir"' EXIT HUP INT TERM

tmp_bin="$tmp_dir/$asset"
tmp_checksum="$tmp_dir/$asset.sha256"
dest="$install_dir/$bin_name"

printf 'Downloading %s from %s\n' "$bin_name" "$url"
download "$url" "$tmp_bin"
download "$checksum_url" "$tmp_checksum"
verify_checksum "$tmp_checksum"
chmod 0755 "$tmp_bin"

mkdir -p "$install_dir"
if command -v install >/dev/null 2>&1; then
  install -m 0755 "$tmp_bin" "$dest"
else
  cp "$tmp_bin" "$dest"
  chmod 0755 "$dest"
fi

printf 'Installed %s to %s\n' "$bin_name" "$dest"

if ! path_contains "$install_dir"; then
  printf 'Add %s to PATH to run %s from any directory.\n' "$install_dir" "$bin_name"
fi
