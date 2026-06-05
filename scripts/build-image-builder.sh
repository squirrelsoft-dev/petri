#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'USAGE'
usage: scripts/build-image-builder.sh [options] [-- <extra petri image build args>]

Build the reusable Petri macOS builder VM image from a fresh checkout.

Options:
  --builder-image <path>              Output builder bundle
                                     (default: PETRI_BUILDER_IMAGE or target/petri-builder)
  --builder-source <url-or-path>      Raw Debian ARM64 NoCloud image source
                                     (default: Petri CLI default)
  --builder-source-sha256 <hex>       Expected SHA-256 for --builder-source
  --builder-source-checksums <path>   Checksum file path or URL for --builder-source
  --builder-cache-dir <path>          Download/cache directory
                                     (default: target/petri-builder-cache)
  --disk-size <size>                  Builder root disk size, such as 16G
  --target <triple>                   petri-guest Linux Rust target
                                     (default: image config target)
  --release                           Build the petri host CLI in release mode
  --help                              Show this help

Environment overrides:
  PETRI_BUILDER_IMAGE
  PETRI_BUILDER_SOURCE
  PETRI_BUILDER_SOURCE_SHA256
  PETRI_BUILDER_SOURCE_CHECKSUMS
  PETRI_BUILDER_CACHE_DIR
  PETRI_BUILDER_DISK_SIZE
  PETRI_GUEST_TARGET

This script builds crates/petri-vz, builds the petri CLI, then runs:
  petri image build --prepare-builder --builder-image <path>
USAGE
}

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
builder_image="${PETRI_BUILDER_IMAGE:-$repo_root/target/petri-builder}"
builder_source="${PETRI_BUILDER_SOURCE:-}"
builder_source_sha256="${PETRI_BUILDER_SOURCE_SHA256:-}"
builder_source_checksums="${PETRI_BUILDER_SOURCE_CHECKSUMS:-}"
builder_cache_dir="${PETRI_BUILDER_CACHE_DIR:-}"
disk_size="${PETRI_BUILDER_DISK_SIZE:-}"
target="${PETRI_GUEST_TARGET:-}"
release=0
extra_args=()

need_tool() {
  if ! command -v "$1" >/dev/null 2>&1; then
    echo "missing required tool: $1" >&2
    exit 1
  fi
}

absolute_path() {
  local path="$1"
  case "$path" in
    /*) printf '%s\n' "$path" ;;
    *) printf '%s\n' "$repo_root/$path" ;;
  esac
}

while [ "$#" -gt 0 ]; do
  case "$1" in
    --builder-image)
      builder_image="$(absolute_path "$2")"
      shift 2
      ;;
    --builder-source)
      builder_source="$2"
      shift 2
      ;;
    --builder-source-sha256)
      builder_source_sha256="$2"
      shift 2
      ;;
    --builder-source-checksums)
      builder_source_checksums="$2"
      shift 2
      ;;
    --builder-cache-dir)
      builder_cache_dir="$(absolute_path "$2")"
      shift 2
      ;;
    --disk-size)
      disk_size="$2"
      shift 2
      ;;
    --target)
      target="$2"
      shift 2
      ;;
    --release)
      release=1
      shift
      ;;
    --help|-h)
      usage
      exit 0
      ;;
    --)
      shift
      extra_args+=("$@")
      break
      ;;
    *)
      echo "unknown argument: $1" >&2
      usage >&2
      exit 2
      ;;
  esac
done

if [ "$(uname -s)" != "Darwin" ]; then
  echo "builder image bootstrap requires macOS Virtualization.framework" >&2
  exit 1
fi

need_tool cargo
need_tool codesign
need_tool curl
need_tool git
need_tool hdiutil
need_tool rustup
need_tool shasum
need_tool swift

cd "$repo_root"

echo "building petri-vz helper"
swift build --package-path crates/petri-vz
petri_vz_bin="$repo_root/crates/petri-vz/.build/debug/petri-vz"
codesign --force --sign - --entitlements "$repo_root/crates/petri-vz/petri-vz.entitlements" "$petri_vz_bin"

cargo_args=(build -p petri)
petri_bin="$repo_root/target/debug/petri"
if [ "$release" -eq 1 ]; then
  cargo_args+=(--release)
  petri_bin="$repo_root/target/release/petri"
fi

echo "building petri CLI"
cargo "${cargo_args[@]}"

if [ "${target:-aarch64-unknown-linux-musl}" = "aarch64-unknown-linux-musl" ] &&
  [ -z "${CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER:-}" ]; then
  export CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER=rust-lld
fi

petri_args=(
  image build
  --prepare-builder
  --builder-image "$builder_image"
)

if [ -n "$builder_source" ]; then
  petri_args+=(--builder-source "$builder_source")
fi
if [ -n "$builder_source_sha256" ]; then
  petri_args+=(--builder-source-sha256 "$builder_source_sha256")
fi
if [ -n "$builder_source_checksums" ]; then
  petri_args+=(--builder-source-checksums "$builder_source_checksums")
fi
if [ -n "$builder_cache_dir" ]; then
  petri_args+=(--builder-cache-dir "$builder_cache_dir")
fi
if [ -n "$disk_size" ]; then
  petri_args+=(--disk-size "$disk_size")
fi
if [ -n "$target" ]; then
  petri_args+=(--target "$target")
fi
if [ "${#extra_args[@]}" -gt 0 ]; then
  petri_args+=("${extra_args[@]}")
fi

echo "preparing builder image: $builder_image"
PETRI_VZ_BIN="$petri_vz_bin" "$petri_bin" "${petri_args[@]}"
