#!/usr/bin/env bash
# Manual build + publish of xr-share to the hub (XR-028).
#
# PRIMARY path is CI — .github/workflows/release-xr-share.yml — which builds
# musl + Windows on a current Rust and publishes on a `xr-share-v*` tag. Use
# THIS script only for a local one-off, and only on a host with a current Rust
# toolchain (the repo's `cross` image is too old for the icu/zerofrom deps).
#
#   ./scripts/release-xr-share.sh [path-to-windows-exe]
#
# Builds static musl via cargo-zigbuild (zig as the cross-linker → no musl-gcc
# pain), optionally bundles a pre-built Windows .exe, writes SHA256SUMS, and
# uploads binaries + install scripts to the hub's share-dist over SSH.
# Requires: cargo-zigbuild + zig  (cargo install cargo-zigbuild; and zig).
set -euo pipefail

HUB_HOST="${HUB_HOST:-201.51.16.159}"
HUB_PORT="${HUB_PORT:-8822}"
HUB_USER="${HUB_USER:-root}"
HUB_DIR="${HUB_DIR:-/var/lib/xr-hub/share-dist}"
WIN_EXE="${1:-}"

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
SSH="ssh -p $HUB_PORT $HUB_USER@$HUB_HOST"
SCP="scp -P $HUB_PORT"

stage="$(mktemp -d)"
trap 'rm -rf "$stage"' EXIT

echo "== build xr-share (x86_64-unknown-linux-musl, static) via zigbuild =="
rustup target add x86_64-unknown-linux-musl >/dev/null 2>&1 || true
cargo zigbuild --release --target x86_64-unknown-linux-musl -p xr-share
cp "$ROOT/target/x86_64-unknown-linux-musl/release/xr-share" "$stage/xr-share-linux-x86_64"

if [ "${BUILD_ARM64:-0}" = "1" ]; then
  echo "== build xr-share (aarch64-unknown-linux-musl) via zigbuild =="
  rustup target add aarch64-unknown-linux-musl >/dev/null 2>&1 || true
  cargo zigbuild --release --target aarch64-unknown-linux-musl -p xr-share \
    && cp "$ROOT/target/aarch64-unknown-linux-musl/release/xr-share" "$stage/xr-share-linux-aarch64" \
    || echo "  (aarch64 build skipped)"
fi

if [ -n "$WIN_EXE" ]; then
  echo "== bundle windows binary: $WIN_EXE =="
  cp "$WIN_EXE" "$stage/xr-share-windows-x86_64.exe"
else
  echo "== no windows .exe given — skipping (pass its path as \$1) =="
fi

cp "$ROOT/xr-share/dist/install.sh"  "$stage/install.sh"
cp "$ROOT/xr-share/dist/install.ps1" "$stage/install.ps1"

echo "== SHA256SUMS (binaries) =="
( cd "$stage" && sha256sum xr-share-* > SHA256SUMS && cat SHA256SUMS )

echo "== upload to hub $HUB_HOST:$HUB_DIR =="
$SSH "mkdir -p $HUB_DIR"
$SCP "$stage"/* "$HUB_USER@$HUB_HOST:$HUB_DIR/"

echo ""
echo "Published. Install command:"
echo "  curl -fsSL https://xr-hub.zoobr.top/share/install.sh | sh"
