#!/usr/bin/env bash
# Manual build + publish of xr-share to the hub (XR-028).
#
# PRIMARY path is CI (.github/workflows/release-xr-share.yml), which builds
# musl + Windows + macOS on a current Rust and publishes on a `xr-share-v*`
# tag. Use THIS script only for a local one-off.
#
#   ./scripts/release-xr-share.sh [path-to-windows-exe]
#
# Builds static musl binaries, on a mac also the native darwin ones, optionally
# bundles a pre-built Windows .exe, writes SHA256SUMS, and uploads binaries +
# install scripts to the hub's share-dist over SSH. Everything is built with the
# relay feature: the hub hands out relay by default (XR-127), so a relay-less
# agent silently loses shares behind NAT. The musl builder is cargo-zigbuild
# (zig as the cross-linker, no musl-gcc pain) when installed, otherwise `cross`
# (needs Docker running).
set -euo pipefail

HUB_HOST="${HUB_HOST:-201.51.16.159}"
HUB_PORT="${HUB_PORT:-8822}"
HUB_USER="${HUB_USER:-root}"
HUB_DIR="${HUB_DIR:-/var/lib/xr-hub/share-dist}"
WIN_EXE="${1:-}"

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
SSH="ssh -p $HUB_PORT $HUB_USER@$HUB_HOST"
SCP="scp -P $HUB_PORT"
FEATURES="--features relay"

stage="$(mktemp -d)"
trap 'rm -rf "$stage"' EXIT

have() { command -v "$1" >/dev/null 2>&1; }
sums() { if have sha256sum; then sha256sum "$@"; else shasum -a 256 "$@"; fi; }

build_musl() {
  local target="$1"
  if have cargo-zigbuild; then
    rustup target add "$target" >/dev/null 2>&1 || true
    cargo zigbuild --release --target "$target" -p xr-share $FEATURES
  elif have cross; then
    cross build --release --target "$target" -p xr-share $FEATURES
  else
    echo "error: need cargo-zigbuild (cargo install cargo-zigbuild; plus zig) or cross" >&2
    exit 1
  fi
}

echo "== build xr-share (x86_64-unknown-linux-musl, static) =="
build_musl x86_64-unknown-linux-musl
cp "$ROOT/target/x86_64-unknown-linux-musl/release/xr-share" "$stage/xr-share-linux-x86_64"

if [ "${BUILD_ARM64:-0}" = "1" ]; then
  echo "== build xr-share (aarch64-unknown-linux-musl) =="
  build_musl aarch64-unknown-linux-musl \
    && cp "$ROOT/target/aarch64-unknown-linux-musl/release/xr-share" "$stage/xr-share-linux-aarch64" \
    || echo "  (aarch64 build skipped)"
fi

# launchd support shipped with XR-127, so the mac one-liner needs its binaries
# in share-dist too (XR-126); darwin builds only work on a darwin host.
if [ "$(uname -s)" = "Darwin" ]; then
  for target in aarch64-apple-darwin x86_64-apple-darwin; do
    echo "== build xr-share ($target) =="
    rustup target add "$target" >/dev/null 2>&1 || true
    cargo build --release --target "$target" -p xr-share $FEATURES
    cp "$ROOT/target/$target/release/xr-share" "$stage/xr-share-macos-${target%%-*}"
  done
fi

if [ -n "$WIN_EXE" ]; then
  echo "== bundle windows binary: $WIN_EXE =="
  cp "$WIN_EXE" "$stage/xr-share-windows-x86_64.exe"
else
  echo "== no windows .exe given, skipping (pass its path as \$1) =="
fi

# The relay uplink leaves this literal in the binary; a binary without it was
# built with the default feature set and must not reach the hub (XR-133).
echo "== relay guard =="
for b in "$stage"/xr-share-*; do
  grep -qa "relay reverse tunnel" "$b" \
    || { echo "error: $b is built without the relay feature" >&2; exit 1; }
done

cp "$ROOT/xr-share/dist/install.sh"  "$stage/install.sh"
cp "$ROOT/xr-share/dist/install.ps1" "$stage/install.ps1"

echo "== SHA256SUMS (binaries) =="
( cd "$stage" && sums xr-share-* > SHA256SUMS && cat SHA256SUMS )

echo "== upload to hub $HUB_HOST:$HUB_DIR =="
$SSH "mkdir -p $HUB_DIR"
$SCP "$stage"/* "$HUB_USER@$HUB_HOST:$HUB_DIR/"

echo ""
echo "Published. Install command:"
echo "  curl -fsSL https://xr-hub.zoobr.top/share/install.sh | sh"
