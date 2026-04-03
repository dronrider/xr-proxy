#!/bin/bash
# Сборка и обновление xr-server на VPS.
# Использование: ./scripts/deploy-server.sh user@host [-p port]
set -euo pipefail

VPS="${1:-}"
SSH_PORT="22"

shift || true
while getopts "p:" opt 2>/dev/null; do
    case $opt in
        p) SSH_PORT="$OPTARG" ;;
    esac
done

if [[ -z "$VPS" ]]; then
    echo "Использование: $0 user@host [-p port]"
    echo "Пример:        $0 root@203.0.113.10 -p 8822"
    exit 1
fi

REMOTE_BIN="/usr/local/bin/xr-server"
LOCAL_BIN="target/release/xr-server"

echo "=== Сборка xr-server ==="
cargo build --release -p xr-server

echo "=== Загрузка на $VPS (порт $SSH_PORT) ==="
scp -P "$SSH_PORT" "$LOCAL_BIN" "$VPS":"${REMOTE_BIN}.new"

echo "=== Замена и перезапуск ==="
ssh -p "$SSH_PORT" "$VPS" "
    mv ${REMOTE_BIN}.new ${REMOTE_BIN} &&
    chmod +x ${REMOTE_BIN} &&
    systemctl restart xr-proxy-server &&
    sleep 1 &&
    systemctl status xr-proxy-server --no-pager
"

echo "=== Готово ==="
