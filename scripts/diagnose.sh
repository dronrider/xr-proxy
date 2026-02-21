#!/bin/sh
# XR Proxy — Диагностика на роутере
# Запустите: sh /tmp/xr-diagnose.sh
# Или: chmod +x /tmp/xr-diagnose.sh && /tmp/xr-diagnose.sh

CONFIG="/etc/xr-proxy/config.toml"

RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
NC='\033[0m'

ok()   { echo "${GREEN}[OK]${NC}    $1"; }
fail() { echo "${RED}[FAIL]${NC}  $1"; }
warn() { echo "${YELLOW}[WARN]${NC}  $1"; }
info() { echo "        $1"; }

echo "════════════════════════════════════════════════"
echo "  XR Proxy — Диагностика"
echo "════════════════════════════════════════════════"
echo ""

# 1. Бинарь
echo "── 1. Бинарь ──"
if [ -x /usr/bin/xr-client ]; then
    ok "xr-client найден: $(ls -lh /usr/bin/xr-client | awk '{print $5}')"
else
    fail "xr-client не найден или не исполняемый"
    info "Загрузите: scp -O xr-client root@ROUTER:/usr/bin/"
    info "Сделайте исполняемым: chmod +x /usr/bin/xr-client"
fi
echo ""

# 2. Конфиг
echo "── 2. Конфиг ──"
if [ -f "$CONFIG" ]; then
    ok "Конфиг найден: $CONFIG"

    # Проверить ключевые поля
    SERVER_IP=$(grep -E '^\s*address\s*=' "$CONFIG" | head -1 | sed 's/.*=\s*"\(.*\)".*/\1/')
    SERVER_PORT=$(grep -E '^\s*port\s*=' "$CONFIG" | head -1 | sed 's/.*=\s*\([0-9]*\).*/\1/')
    LISTEN_PORT=$(grep -E '^\s*listen_port\s*=' "$CONFIG" | head -1 | sed 's/.*=\s*\([0-9]*\).*/\1/')
    DEFAULT_ACTION=$(grep -E '^\s*default_action\s*=' "$CONFIG" | head -1 | sed 's/.*=\s*"\(.*\)".*/\1/')
    KEY=$(grep -E '^\s*key\s*=' "$CONFIG" | head -1 | sed 's/.*=\s*"\(.*\)".*/\1/')

    LISTEN_PORT=${LISTEN_PORT:-1080}

    if echo "$KEY" | grep -q "GENERATE\|YOUR\|REPLACE\|CHANGE"; then
        fail "Ключ не заменён! Сгенерируйте: openssl rand -base64 64"
    else
        ok "Ключ задан ($(echo "$KEY" | wc -c) символов)"
    fi

    if echo "$SERVER_IP" | grep -q "YOUR\|REPLACE\|CHANGE\|0\.0\.0\.0"; then
        fail "IP сервера не задан: address = \"$SERVER_IP\""
    else
        ok "Сервер: $SERVER_IP:$SERVER_PORT"
    fi

    info "Listen порт: $LISTEN_PORT"
    info "Default action: $DEFAULT_ACTION"
else
    fail "Конфиг не найден: $CONFIG"
    info "Скопируйте: scp -O configs/client.toml root@ROUTER:/etc/xr-proxy/config.toml"
fi
echo ""

# 3. Процесс
echo "── 3. Процесс ──"
PID=$(pgrep -x xr-client)
if [ -n "$PID" ]; then
    ok "xr-client запущен (PID: $PID)"
    MEM=$(grep -i vmrss /proc/$PID/status 2>/dev/null | awk '{print $2, $3}')
    [ -n "$MEM" ] && info "Память: $MEM"
else
    fail "xr-client НЕ запущен"
    info "Запустите: /etc/init.d/xr-proxy start"
    info "Или вручную: /usr/bin/xr-client -c $CONFIG -l debug"
fi
echo ""

# 4. Порт
echo "── 4. Слушающий порт ──"
if netstat -tlnp 2>/dev/null | grep -q ":${LISTEN_PORT:-1080} "; then
    ok "Порт ${LISTEN_PORT:-1080} слушается"
else
    fail "Порт ${LISTEN_PORT:-1080} НЕ слушается"
    info "xr-client либо не запущен, либо упал при старте"
fi
echo ""

# 5. Firewall redirect
echo "── 5. Правила перенаправления ──"
NFT_OK=0
IPT_OK=0

if command -v nft >/dev/null 2>&1; then
    if nft list table ip xr_proxy >/dev/null 2>&1; then
        ok "nftables таблица xr_proxy существует"
        NFT_OK=1
        nft list table ip xr_proxy 2>/dev/null | grep -E "redirect|return|dport" | while read -r line; do
            info "  $line"
        done
    else
        warn "nftables таблица xr_proxy НЕ найдена"
    fi
fi

if command -v iptables >/dev/null 2>&1; then
    if iptables -t nat -L XR_PROXY -n >/dev/null 2>&1; then
        ok "iptables цепочка XR_PROXY существует"
        IPT_OK=1
        iptables -t nat -L XR_PROXY -n 2>/dev/null | grep -v "^Chain\|^target" | while read -r line; do
            info "  $line"
        done
    else
        [ "$NFT_OK" = "0" ] && warn "iptables цепочка XR_PROXY НЕ найдена"
    fi
fi

if [ "$NFT_OK" = "0" ] && [ "$IPT_OK" = "0" ]; then
    fail "Нет правил перенаправления! Трафик не попадает в прокси"
    info "Проверьте auto_redirect = true в конфиге"
    info "Или настройте вручную"
fi
echo ""

# 6. Доступ к серверу
echo "── 6. Доступ к серверу ──"
if [ -n "$SERVER_IP" ] && [ -n "$SERVER_PORT" ]; then
    # Попробуем разные способы проверки
    if command -v nc >/dev/null 2>&1; then
        if nc -z -w3 "$SERVER_IP" "$SERVER_PORT" 2>/dev/null; then
            ok "Сервер $SERVER_IP:$SERVER_PORT доступен"
        else
            fail "Сервер $SERVER_IP:$SERVER_PORT НЕ доступен"
            info "Проверьте:"
            info "  - Сервер запущен? (ssh на VPS, systemctl status xr-proxy-server)"
            info "  - Порт открыт в файрволе VPS?"
            info "  - Security Group / панель VPS-провайдера?"
        fi
    elif command -v wget >/dev/null 2>&1; then
        if wget -q --spider --timeout=3 "http://$SERVER_IP:$SERVER_PORT" 2>/dev/null; then
            ok "Сервер $SERVER_IP:$SERVER_PORT отвечает (HTTP fallback)"
        else
            warn "Сервер $SERVER_IP:$SERVER_PORT не ответил (wget)"
            info "Это может быть нормально если fallback отключён"
        fi
    else
        warn "Нет nc/wget для проверки — установите: opkg install netcat"
    fi
else
    warn "Не удалось извлечь адрес сервера из конфига"
fi
echo ""

# 7. DNS
echo "── 7. DNS ──"
if nslookup google.com >/dev/null 2>&1; then
    ok "DNS работает (google.com резолвится)"
elif ping -c1 -W2 8.8.8.8 >/dev/null 2>&1; then
    warn "Ping до 8.8.8.8 работает, но DNS не резолвит"
    info "Проверьте настройки DNS на роутере"
else
    fail "Нет доступа к интернету с роутера"
fi
echo ""

# 8. Watchdog
echo "── 8. Watchdog ──"
if [ -x /usr/bin/xr-watchdog.sh ]; then
    ok "Watchdog скрипт установлен"
    if crontab -l 2>/dev/null | grep -q xr-watchdog; then
        ok "Watchdog cron активен"
    else
        warn "Watchdog cron НЕ активен"
        info "Он устанавливается автоматически при старте через init-скрипт"
    fi
else
    warn "Watchdog не установлен (/usr/bin/xr-watchdog.sh)"
fi
echo ""

# 9. Последние логи
echo "── 9. Последние логи xr-proxy ──"
LOGS=$(logread 2>/dev/null | grep -i "xr" | tail -10)
if [ -n "$LOGS" ]; then
    echo "$LOGS"
else
    info "(нет логов от xr в системном журнале)"
fi
echo ""

echo "════════════════════════════════════════════════"
echo "  Для подробной диагностики остановите сервис и"
echo "  запустите вручную с debug-логированием:"
echo ""
echo "  /etc/init.d/xr-proxy stop"
echo "  /usr/bin/xr-client -c $CONFIG -l debug"
echo "════════════════════════════════════════════════"
