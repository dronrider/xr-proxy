#!/bin/bash
# Сборка и обновление xr-server на VPS.
# Использование: ./scripts/deploy-server.sh [user@host]
set -euo pipefail

VPS="${1:-root@YOUR_VPS_IP}"

if [[ "$VPS" == *"YOUR_VPS_IP"* ]]; then
    echo "Использование: $0 user@host"
    echo "Пример:        $0 root@203.0.113.10"
    exit 1
fi

REMOTE_BIN="/usr/local/bin/xr-server"
LOCAL_BIN="target/release/xr-server"

echo "=== Сборка xr-server ==="
cargo build --release -p xr-server

echo "=== Загрузка на $VPS ==="
scp "$LOCAL_BIN" "$VPS":"${REMOTE_BIN}.new"

echo "=== Замена и перезапуск ==="
ssh "$VPS" "
    mv ${REMOTE_BIN}.new ${REMOTE_BIN} &&
    chmod +x ${REMOTE_BIN} &&
    systemctl restart xr-proxy-server &&
    sleep 1 &&
    systemctl status xr-proxy-server --no-pager
"

echo "=== Готово ==="
