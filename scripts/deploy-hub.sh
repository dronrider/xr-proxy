#!/bin/bash
# Deploy xr-hub on VPS.
# Run from the repo root: ./scripts/deploy-hub.sh
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$REPO_ROOT"

echo "==> Pulling latest changes..."
git pull

echo "==> Building Admin UI..."
cd xr-hub/admin-ui
npm ci --no-audit --no-fund
npm run build
cd "$REPO_ROOT"

echo "==> Building xr-hub (release)..."
cargo build --release -p xr-hub

echo "==> Stopping xr-hub..."
systemctl stop xr-hub || true

echo "==> Installing binary..."
cp target/release/xr-hub /usr/local/bin/xr-hub

echo "==> Starting xr-hub..."
systemctl start xr-hub

echo "==> Done. Status:"
systemctl status xr-hub --no-pager
