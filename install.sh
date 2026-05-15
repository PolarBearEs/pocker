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

extract_asset_digest() {
  metadata_file="$1"

  awk -v wanted="$asset" '
    $0 ~ "\"name\"[[:space:]]*:" {
      in_asset = ($0 ~ "\"name\"[[:space:]]*:[[:space:]]*\"" wanted "\"")
    }
    in_asset && $0 ~ "\"digest\"[[:space:]]*:" {
      if (match($0, /"digest"[[:space:]]*:[[:space:]]*"sha256:[0-9A-Fa-f]+"/)) {
        digest = substr($0, RSTART, RLENGTH)
        sub(/^.*sha256:/, "", digest)
        sub(/"$/, "", digest)
        print tolower(digest)
        exit
      }
    }
  ' "$metadata_file"
}

file_sha256() {
  file="$1"

  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$file" | awk '{ print $1 }'
  elif command -v shasum >/dev/null 2>&1; then
    shasum -a 256 "$file" | awk '{ print $1 }'
  else
    die "missing required command: sha256sum or shasum"
  fi
}

verify_digest() {
  expected="$1"
  actual="$(file_sha256 "$tmp_bin")"

  [ "$actual" = "$expected" ] || die "checksum mismatch for $asset"
  printf 'Verified SHA256 %s\n' "$expected"
}

path_contains() {
  case ":${PATH:-}:" in
    *":$1:"*) return 0 ;;
    *) return 1 ;;
  esac
}

default_install_dir() {
  [ "${HOME:-}" ] || die "HOME is not set; set POCKER_INSTALL_DIR to choose an install directory"

  if path_contains "$HOME/.local/bin"; then
    printf '%s\n' "$HOME/.local/bin"
  else
    printf '%s\n' /usr/local/bin
  fi
}

need_cmd uname
need_cmd mktemp
need_cmd chmod
need_cmd mkdir
need_cmd awk

if [ "${POCKER_INSTALL_DIR:-}" ]; then
  install_dir="$POCKER_INSTALL_DIR"
else
  install_dir="$(default_install_dir)"
fi

detect_asset

if [ "$version" = "latest" ]; then
  base_url="https://github.com/$repo/releases/latest/download"
  metadata_url="https://api.github.com/repos/$repo/releases/latest"
else
  base_url="https://github.com/$repo/releases/download/$version"
  metadata_url="https://api.github.com/repos/$repo/releases/tags/$version"
fi

url="$base_url/$asset"

tmp_dir="$(mktemp -d)"
trap 'rm -rf "$tmp_dir"' EXIT HUP INT TERM

tmp_bin="$tmp_dir/$asset"
tmp_metadata="$tmp_dir/release.json"
dest="$install_dir/$bin_name"

printf 'Downloading %s from %s\n' "$bin_name" "$url"
download "$url" "$tmp_bin"
download "$metadata_url" "$tmp_metadata"
expected_digest="$(extract_asset_digest "$tmp_metadata")"
[ "$expected_digest" ] || die "GitHub release metadata did not include a SHA256 digest for $asset"
verify_digest "$expected_digest"
chmod 0755 "$tmp_bin"

mkdir -p "$install_dir"
[ -w "$install_dir" ] || die "cannot write to $install_dir; run with sudo or set POCKER_INSTALL_DIR"

if command -v install >/dev/null 2>&1; then
  install -m 0755 "$tmp_bin" "$dest"
else
  cp "$tmp_bin" "$dest"
  chmod 0755 "$dest"
fi

printf 'Installed %s to %s\n' "$bin_name" "$dest"
