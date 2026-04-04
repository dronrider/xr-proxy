#!/bin/bash
# Кросс-сборка xr-client и обновление на OpenWRT роутере.
# Использование: ./scripts/deploy-router.sh user@host [-p port] [-t target]
#
# Примеры:
#   ./scripts/deploy-router.sh root@192.168.1.1
#   ./scripts/deploy-router.sh root@192.168.1.1 -p 22 -t aarch64-unknown-linux-musl
set -euo pipefail

usage() {
    echo "Использование: $0 user@host [-p port] [-t target]"
    echo ""
    echo "  -p port    SSH-порт (по умолчанию 22)"
    echo "  -t target  Rust target (по умолчанию aarch64-unknown-linux-musl)"
    echo ""
    echo "Примеры:"
    echo "  $0 root@192.168.1.1"
    echo "  $0 root@192.168.1.1 -p 2222 -t mips-unknown-linux-musl"
    exit 1
}

ROUTER="${1:-}"
[ -z "$ROUTER" ] && usage
shift

SSH_PORT="22"
TARGET="aarch64-unknown-linux-musl"

while getopts "p:t:" opt; do
    case $opt in
        p) SSH_PORT="$OPTARG" ;;
        t) TARGET="$OPTARG" ;;
        *) usage ;;
    esac
done

REMOTE_BIN="/usr/bin/xr-client"
LOCAL_BIN="target/${TARGET}/release/xr-client"
SSH_CMD="ssh -p ${SSH_PORT} ${ROUTER}"

echo "=== Сборка xr-client (${TARGET}) ==="
cross build --release --target "$TARGET" -p xr-client

echo "=== Остановка службы ==="
$SSH_CMD "/etc/init.d/xr-proxy stop" 2>/dev/null || true

echo "=== Загрузка на ${ROUTER} ==="
scp -O -P "$SSH_PORT" "$LOCAL_BIN" "${ROUTER}:${REMOTE_BIN}.new"

echo "=== Замена и запуск ==="
$SSH_CMD "
    mv ${REMOTE_BIN}.new ${REMOTE_BIN} &&
    chmod +x ${REMOTE_BIN} &&
    /etc/init.d/xr-proxy start
"

echo "=== Проверка ==="
sleep 2
$SSH_CMD "pgrep -f xr-client >/dev/null && echo 'OK: xr-client запущен' || echo 'ОШИБКА: xr-client не запустился'"

echo "=== Готово ==="
