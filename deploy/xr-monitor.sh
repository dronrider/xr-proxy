#!/bin/sh
# XR Proxy Monitor. Снимает слепое пятно диагностики (XR-077).
#
# logread на OpenWRT это кольцевой буфер в RAM: ребут его стирает, и разобрать
# инцидент постфактум нечем. Раз в минуту (cron) пишем компактную строку
# состояния в персистентный файл, а при аномалии (xr-client не запущен ИЛИ
# nft-redirect пропал, то есть условие Direct-утечки) снимаем полный контекст
# (logread + dmesg) в отдельный персистентный файл ДО того, как его сотрёт
# ребут. Скрипт только читает, ничего не чинит.

export PATH="/usr/sbin:/usr/bin:/sbin:/bin"

STATUS_LOG=/etc/xr-proxy/monitor.log
INCIDENT_LOG=/etc/xr-proxy/incident.log
CONFIG=/etc/xr-proxy/config.toml
TS=$(date '+%Y-%m-%d %H:%M:%S')

PID=$(pidof xr-client 2>/dev/null)
if [ -n "$PID" ]; then
    ALIVE=up
    FD=$(ls "/proc/$PID/fd" 2>/dev/null | wc -l)
else
    ALIVE=DOWN
    FD=0
fi

nft list table ip xr_proxy >/dev/null 2>&1 && PROXY=yes || PROXY=NO
nft list table ip xr_killswitch >/dev/null 2>&1 && KS=yes || KS=-

# ESTABLISHED mux к каждому адресу из [[servers]] (порт 8443).
MUX=""
for s in $(grep -E '^address = ' "$CONFIG" 2>/dev/null | grep -oE '[0-9]+\.[0-9]+\.[0-9]+\.[0-9]+'); do
    n=$(grep "dst=$s " /proc/net/nf_conntrack 2>/dev/null | grep ESTABLISHED | grep -c 'dport=8443')
    MUX="$MUX$s=$n "
done

echo "$TS xr=$ALIVE fd=$FD xr_proxy=$PROXY killswitch=$KS mux[ $MUX]" >> "$STATUS_LOG"

# Аномалия: приложение лежит или redirect снят при живом хосте. Оба состояния
# это либо текущая утечка (redirect снят, трафик идёт напрямую), либо её канун.
if [ "$ALIVE" = DOWN ] || [ "$PROXY" = NO ]; then
    {
        echo "=== ANOMALY $TS xr=$ALIVE xr_proxy=$PROXY killswitch=$KS ==="
        echo "--- logread (xr) ---"
        logread 2>/dev/null | grep -iE 'xr-client|xr-watchdog|xr-monitor' | tail -50
        echo "--- dmesg tail ---"
        dmesg 2>/dev/null | tail -20
        echo "--- nft tables ---"
        nft list tables ip 2>/dev/null
        echo ""
    } >> "$INCIDENT_LOG"
fi

# Ротация по размеру (~200 КБ статус, ~200 КБ инцидентов).
for f in "$STATUS_LOG" "$INCIDENT_LOG"; do
    if [ -f "$f" ] && [ "$(wc -c < "$f")" -gt 204800 ]; then
        tail -1000 "$f" > "$f.tmp" && mv "$f.tmp" "$f"
    fi
done
