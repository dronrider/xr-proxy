#!/bin/bash
# Сборка и перезапуск xr-server. Запускать из корня проекта на VPS.
set -euo pipefail

REMOTE_BIN="/usr/local/bin/xr-server"

echo "=== Сборка ==="
cargo build --release -p xr-server

echo "=== Установка ==="
cp target/release/xr-server "${REMOTE_BIN}.new"
mv "${REMOTE_BIN}.new" "${REMOTE_BIN}"

echo "=== Перезапуск ==="
systemctl restart xr-proxy-server
sleep 1
systemctl status xr-proxy-server --no-pager

echo "=== Готово ==="
