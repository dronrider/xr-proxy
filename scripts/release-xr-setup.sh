#!/usr/bin/env bash
# Сборка и выкладка setup-dist на хаб (XR-015, LLD-13): статические musl
# бинари xr-setup, xr-server и xr-hub под обе арки плюс install.sh и
# SHA256SUMS. С этой раздачи ставится чистый VPS одной командой:
#
#   curl -fsSL https://<хаб>/api/v1/setup/install.sh | sh -s -- server ...
#
# musl-сборщик тот же, что у release-xr-share.sh: cargo-zigbuild, если
# установлен, иначе cross (нужен Docker).
set -euo pipefail

HUB_HOST="${HUB_HOST:-201.51.16.159}"
HUB_PORT="${HUB_PORT:-8822}"
HUB_USER="${HUB_USER:-root}"
HUB_DIR="${HUB_DIR:-/var/lib/xr-hub/setup-dist}"

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
SSH="ssh -p $HUB_PORT $HUB_USER@$HUB_HOST"
SCP="scp -P $HUB_PORT"
CRATES=(xr-setup xr-server xr-hub)

stage="$(mktemp -d)"
trap 'rm -rf "$stage"' EXIT

have() { command -v "$1" >/dev/null 2>&1; }
sums() { if have sha256sum; then sha256sum "$@"; else shasum -a 256 "$@"; fi; }

build_musl() {
  local target="$1" crate="$2"
  if have cargo-zigbuild; then
    rustup target add "$target" >/dev/null 2>&1 || true
    cargo zigbuild --release --target "$target" -p "$crate"
  elif have cross; then
    cross build --release --target "$target" -p "$crate"
  else
    echo "error: нужен cargo-zigbuild (плюс zig) или cross" >&2
    exit 1
  fi
}

for target in x86_64-unknown-linux-musl aarch64-unknown-linux-musl; do
  suffix="linux-${target%%-*}"
  for crate in "${CRATES[@]}"; do
    echo "== build $crate ($target) =="
    build_musl "$target" "$crate"
    cp "$ROOT/target/$target/release/$crate" "$stage/$crate-$suffix"
  done
done

cp "$ROOT/scripts/install.sh" "$stage/install.sh"

echo "== SHA256SUMS =="
( cd "$stage" && sums xr-setup-* xr-server-* xr-hub-* > SHA256SUMS && cat SHA256SUMS )

echo "== upload to hub $HUB_HOST:$HUB_DIR =="
$SSH "mkdir -p $HUB_DIR"
$SCP "$stage"/* "$HUB_USER@$HUB_HOST:$HUB_DIR/"

echo ""
echo "Готово. Установка с чистого VPS:"
echo "  curl -fsSL https://xr-hub.zoobr.top/api/v1/setup/install.sh | sh -s -- server --with-hub --hub-domain <домен>"
