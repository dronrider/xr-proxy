#!/bin/sh
# XR hang-watch: ловит ЖИВОЕ состояние зависания проксирования (XR-084/рецидивы).
#
# Проблема диагностики: прокси иногда встаёт намертво (процесс жив, лечит только
# kill), но эпизод интермиттентный, и вручную застать зависший процесс не выходит
# (его убивают раньше). Скрипт раз в минуту снимает компактный срез состояния
# единственного потока рантайма и очередей mux-сокетов. Зависание держится
# минутами, поэтому минутного разрешения хватает, чтобы записать стойкое стоячее
# состояние.
#
# Что различаем по срезу:
# - dcpu ~0 и очередь mux стоит высоко и не сливается несколько срезов подряд
#   -> дедлок на async-уровне (таски запаркованы, исполнитель спит);
# - dcpu высокий (сотни тиков) при стоячем проксировании -> livelock (spin);
# - Recv-Q сокета пухнет -> reader перестал читать сокет (не сливает kernel-буфер);
# - Send-Q сокета пухнет -> writer застрял (сервер под backpressure не читает).
# Скрипт только читает, ничего не чинит и не рестартит.

export PATH="/usr/sbin:/usr/bin:/sbin:/bin"

LOG=/etc/xr-proxy/hangwatch.log
DUMP=/etc/xr-proxy/hang-dump.log
TS=$(date '+%Y-%m-%d %H:%M:%S')

pid=$(pidof xr-client 2>/dev/null) || exit 0
[ -n "$pid" ] || exit 0

st=$(awk '{print $3}' "/proc/$pid/stat" 2>/dev/null)
wchan=$(cat "/proc/$pid/wchan" 2>/dev/null)
cur=$(awk '{print $14+$15}' "/proc/$pid/stat" 2>/dev/null)
prev=$(cat /tmp/.xrhw_cpu 2>/dev/null || echo "$cur")
echo "$cur" > /tmp/.xrhw_cpu
dcpu=$((cur - prev))
fd=$(ls "/proc/$pid/fd" 2>/dev/null | wc -l)
thr=$(ls "/proc/$pid/task" 2>/dev/null | wc -l)

# Очереди mux-сокетов к серверам (порт 8443 в foreign-колонке). $2=Recv-Q, $3=Send-Q.
mux=$(netstat -tn 2>/dev/null | awk '/:8443/{c++; if($2+0>rq)rq=$2; if($3+0>sq)sq=$3} END{printf "n=%d maxRecvQ=%d maxSendQ=%d", c+0, rq+0, sq+0}')
maxq=$(netstat -tn 2>/dev/null | awk '/:8443/{if($2+0>m)m=$2; if($3+0>m)m=$3} END{print m+0}')
ct=$(wc -l < /proc/net/nf_conntrack 2>/dev/null)

echo "$TS st=$st wchan=${wchan:-.} dcpu=${dcpu}t fd=$fd thr=$thr mux[$mux] ct=$ct" >> "$LOG"

# Подозрение на зависание: какая-то очередь mux заметно застряла. Снимаем полный
# контекст (состояния всех потоков, netstat, логи) до того, как процесс убьют.
if [ "${maxq:-0}" -gt 50000 ]; then
    {
        echo "=== HANG-SUSPECT $TS st=$st dcpu=${dcpu}t fd=$fd mux[$mux] ct=$ct ==="
        echo "-- состояния потоков --"
        for t in "/proc/$pid/task"/*; do
            [ -f "$t/stat" ] || continue
            echo "  tid $(basename "$t"): state=$(awk '{print $3}' "$t/stat" 2>/dev/null) wchan=$(cat "$t/wchan" 2>/dev/null)"
        done
        echo "-- netstat :8443 (Recv-Q Send-Q) --"
        netstat -tnp 2>/dev/null | awk 'NR==1 || /:8443/'
        echo "-- logread xr tail --"
        logread 2>/dev/null | grep xr-client | tail -25
        echo ""
    } >> "$DUMP"
fi

# Ротация по размеру.
for f in "$LOG" "$DUMP"; do
    if [ -f "$f" ] && [ "$(wc -c < "$f")" -gt 262144 ]; then
        tail -1000 "$f" > "$f.tmp" && mv "$f.tmp" "$f"
    fi
done
