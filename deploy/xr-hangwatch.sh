#!/bin/sh
# XR hang-watch: ловит ЖИВОЕ состояние зависания проксирования.
#
# Прошлый эпизод показал, что при зависании xr-client по метрикам ВЫГЛЯДИТ здоровым
# (поток спит, CPU ~1%, очереди mux-сокетов нулевые, mux-таймаутов в логе нет), но
# проксирование стоит. Значит очереди сокетов не тот сигнал. Ключевой сигнал это
# идут ли РЕАЛЬНО данные через туннель: считаем байты на conntrack-флоу к IP
# серверов (dport=8443) и смотрим дельту между срезами. Плюс активность сети (fd,
# новые conntrack). Туннель не двигает байты при активной сети = подозрение на
# зависание, снимаем полный контекст.
#
# Скрипт только читает, ничего не чинит и не рестартит.

export PATH="/usr/sbin:/usr/bin:/sbin:/bin"

LOG=/etc/xr-proxy/hangwatch.log
DUMP=/etc/xr-proxy/hang-dump.log
CONFIG=/etc/xr-proxy/config.toml
STAMP=/tmp/.xrhw
TS=$(date '+%Y-%m-%d %H:%M:%S')

pid=$(pidof xr-client 2>/dev/null) || exit 0
[ -n "$pid" ] || exit 0

st=$(awk '{print $3}' "/proc/$pid/stat" 2>/dev/null)
cur=$(awk '{print $14+$15}' "/proc/$pid/stat" 2>/dev/null)
prevc=$(cat "$STAMP.cpu" 2>/dev/null || echo "$cur"); echo "$cur" > "$STAMP.cpu"
dcpu=$((cur - prevc))
fd=$(ls "/proc/$pid/fd" 2>/dev/null | wc -l)

# IP серверов из конфига.
srv=$(grep -E '^address = ' "$CONFIG" 2>/dev/null | grep -oE '[0-9]+\.[0-9]+\.[0-9]+\.[0-9]+')

# МОНОТОННЫЙ счётчик тунельных байт через nft (conntrack-сумма немонотонна:
# записи закрываются между срезами и сумма падает в 0 даже при активном трафике,
# из-за чего сигнал врал про «сухой туннель»). Отдельная таблица только со
# счётчиками (policy accept, трафик не трогаем): output к серверу:8443 (upload) и
# input от сервера:8443 (download).
if ! nft list table ip xr_hwcount >/dev/null 2>&1; then
    nft add table ip xr_hwcount 2>/dev/null
    nft add chain ip xr_hwcount mon_out '{ type filter hook output priority 300; policy accept; }' 2>/dev/null
    nft add chain ip xr_hwcount mon_in '{ type filter hook input priority 300; policy accept; }' 2>/dev/null
    for s in $srv; do
        nft add rule ip xr_hwcount mon_out ip daddr "$s" tcp dport 8443 counter 2>/dev/null
        nft add rule ip xr_hwcount mon_in ip saddr "$s" tcp sport 8443 counter 2>/dev/null
    done
fi
muxbytes=$(nft list table ip xr_hwcount 2>/dev/null | grep -oE 'bytes [0-9]+' | awk '{s+=$2} END{print s+0}')
muxn=0
for s in $srv; do
    muxn=$((muxn + $(grep "dst=$s " /proc/net/nf_conntrack 2>/dev/null | grep -c 'dport=8443')))
done
prevb=$(cat "$STAMP.mux" 2>/dev/null || echo "$muxbytes"); echo "$muxbytes" > "$STAMP.mux"
dmux=$((muxbytes - prevb))
# nft-счётчик сбрасывается только с таблицей (рестарт роутера) -> нормализуем.
[ "$dmux" -lt 0 ] && dmux=0

ct=$(wc -l < /proc/net/nf_conntrack 2>/dev/null)

# Очереди на mux-сокетах к серверам (Recv-Q/Send-Q через netstat). Ключевой сигнал
# per-slot затыка ConnectAck (XR-086): при зависании reader клиента застрял ->
# Recv-Q пухнет (сервер шлёт, клиент не читает); если Recv-Q=0 при живом сервере
# -> сервер молчит именно в этот сокет. Берём максимум по всем сокетам к серверам.
rq=0; sq=0
for s in $srv; do
    eval "$(netstat -tn 2>/dev/null | awk -v s="$s" '$0 ~ (s":8443") {if($2+0>rq)rq=$2; if($3+0>sq)sq=$3} END{print "lrq="rq+0"; lsq="sq+0}')"
    [ "${lrq:-0}" -gt "$rq" ] && rq=$lrq
    [ "${lsq:-0}" -gt "$sq" ] && sq=$lsq
done

echo "$TS st=$st dcpu=${dcpu}t fd=$fd mux[n=$muxn dBytes=${dmux} rq=$rq sq=$sq] ct=$ct" >> "$LOG"

# Подозрение на зависание: туннель НЕ двигает байты, но сеть активна (fd высокий,
# то есть кто-то ломится). Считаем подряд идущие "сухие" срезы, дампим на 3-м.
DRY_FD_MIN=200
if [ "$dmux" -lt 4096 ] && [ "$fd" -gt "$DRY_FD_MIN" ]; then
    dry=$(cat "$STAMP.dry" 2>/dev/null || echo 0); dry=$((dry + 1)); echo "$dry" > "$STAMP.dry"
else
    echo 0 > "$STAMP.dry"; dry=0
fi

if [ "$dry" -ge 3 ]; then
    echo 0 > "$STAMP.dry"
    {
        echo "=== HANG-SUSPECT $TS st=$st dcpu=${dcpu}t fd=$fd mux[n=$muxn dBytes=$dmux] ct=$ct ==="
        echo "-- состояния потоков --"
        for t in "/proc/$pid/task"/*; do
            [ -f "$t/stat" ] || continue
            echo "  tid $(basename "$t"): state=$(awk '{print $3}' "$t/stat" 2>/dev/null) wchan=$(cat "$t/wchan" 2>/dev/null)"
        done
        echo "-- mux-флоу к серверам --"
        for s in $srv; do echo "  $s:"; grep "dst=$s " /proc/net/nf_conntrack 2>/dev/null | grep dport=8443 | head -14; done
        echo "-- nft xr_proxy/killswitch присутствуют --"
        nft list tables ip 2>/dev/null | grep -E 'xr_proxy|xr_killswitch|xr_udp'
        echo "-- logread xr tail (решения + ошибки) --"
        logread 2>/dev/null | grep xr-client | tail -40
        echo ""
    } >> "$DUMP"
fi

for f in "$LOG" "$DUMP"; do
    if [ -f "$f" ] && [ "$(wc -c < "$f")" -gt 262144 ]; then
        tail -1000 "$f" > "$f.tmp" && mv "$f.tmp" "$f"
    fi
done
