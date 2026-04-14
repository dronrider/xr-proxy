#!/bin/bash
# Кросс-сборка xr-client и обновление на OpenWRT роутере.
# Использование: ./scripts/deploy-router.sh user@host [опции]
#
# Флаги:
#   -p port       SSH-порт (по умолчанию 22)
#   -t target     Rust target (если не указан — определяется по `uname -m` роутера)
#   -u hub_url    URL xr-hub. Если задан, в /etc/xr-proxy/config.toml будет
#                 добавлена секция [hub], если её там ещё нет.
#   -n preset     Имя пресета (используется вместе с -u, default: russia)
#
# Примеры:
#   # обычное обновление (auto-detect arch)
#   ./scripts/deploy-router.sh root@192.168.1.1
#
#   # явный target + SSH-порт
#   ./scripts/deploy-router.sh root@192.168.1.1 -p 2222 -t mips-unknown-linux-musl
#
#   # с подключением к xr-hub
#   ./scripts/deploy-router.sh root@192.168.1.1 -u https://xr-hub.example.com -n russia
set -euo pipefail

usage() {
    sed -n '2,21p' "$0" | sed 's/^# \{0,1\}//'
    exit 1
}

ROUTER="${1:-}"
[ -z "$ROUTER" ] && usage
shift

SSH_PORT="22"
TARGET=""
HUB_URL=""
PRESET_NAME="russia"

while getopts "p:t:u:n:" opt; do
    case $opt in
        p) SSH_PORT="$OPTARG" ;;
        t) TARGET="$OPTARG" ;;
        u) HUB_URL="$OPTARG" ;;
        n) PRESET_NAME="$OPTARG" ;;
        *) usage ;;
    esac
done

SSH_CMD="ssh -p ${SSH_PORT} ${ROUTER}"

# ── Auto-detect target if not provided ─────────────────────────────
if [ -z "$TARGET" ]; then
    echo "=== Определение архитектуры роутера ==="
    arch=$($SSH_CMD 'uname -m' 2>/dev/null || true)
    case "$arch" in
        aarch64)  TARGET="aarch64-unknown-linux-musl" ;;
        armv7l)   TARGET="armv7-unknown-linux-musleabihf" ;;
        armv6l)   TARGET="arm-unknown-linux-musleabihf" ;;
        mips)     TARGET="mips-unknown-linux-musl" ;;
        mipsel)   TARGET="mipsel-unknown-linux-musl" ;;
        x86_64)   TARGET="x86_64-unknown-linux-musl" ;;
        "")
            echo "Не удалось подключиться к ${ROUTER} (ssh не ответил)." >&2
            echo "Проверьте доступность и SSH-ключи, либо укажите target вручную через -t." >&2
            exit 1
            ;;
        *)
            echo "Неизвестная архитектура '${arch}'. Укажите target вручную через -t." >&2
            exit 1
            ;;
    esac
    echo "    uname -m: ${arch} → ${TARGET}"
fi

REMOTE_BIN="/usr/bin/xr-client"
LOCAL_BIN="target/${TARGET}/release/xr-client"

# ── Build ──────────────────────────────────────────────────────────
echo "=== Сборка xr-client (${TARGET}) ==="
cross build --release --target "$TARGET" -p xr-client

# ── Inject [hub] section into config.toml if requested ─────────────
# Делаем ДО рестарта, чтобы новый бинарь сразу поднялся с hub-настройками.
if [ -n "$HUB_URL" ]; then
    echo "=== Настройка секции [hub] ==="
    if $SSH_CMD "grep -q '^\[hub\]' /etc/xr-proxy/config.toml"; then
        echo "    [hub] уже присутствует в /etc/xr-proxy/config.toml — оставляем как есть"
        echo "    (если нужно сменить url/preset — отредактируйте руками)"
    else
        $SSH_CMD "cat >> /etc/xr-proxy/config.toml" <<EOF

[hub]
url = "${HUB_URL}"
preset = "${PRESET_NAME}"
refresh_interval_secs = 300
EOF
        echo "    добавили [hub]: url=${HUB_URL}, preset=${PRESET_NAME}"
    fi
fi

# ── Deploy ─────────────────────────────────────────────────────────
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

# ── Verify ─────────────────────────────────────────────────────────
echo "=== Проверка ==="
sleep 2
$SSH_CMD "pgrep -f xr-client >/dev/null && echo 'OK: xr-client запущен' || echo 'ОШИБКА: xr-client не запустился'"

# Показать последние xr-* строки в syslog — там должно быть видно
# загрузку пресета ("fetched preset 'X' vN" / "preset 'X' loaded, merging ...").
echo ""
echo "=== Последние логи (grep xr) ==="
$SSH_CMD "logread | grep -Ei 'xr|preset' | tail -30" || true

echo ""
echo "=== Готово ==="
if [ -n "$HUB_URL" ]; then
    echo "Чтобы посмотреть hot-swap в живом режиме, в отдельном терминале:"
    echo "  ssh -p ${SSH_PORT} ${ROUTER} 'logread -f | grep -Ei preset'"
    echo ""
    echo "Затем отредактируй пресет '${PRESET_NAME}' в ${HUB_URL}/admin —"
    echo "через ≤5 минут в логе появится 'preset ${PRESET_NAME} hot-swapped'."
fi
