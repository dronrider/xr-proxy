#!/bin/sh
# Fail-closed kill-switch (XR-077).
#
# Проблема: когда xr-client не перехватывает (упал, рестартится, а watchdog или
# сам клиент сняли nft-redirect), LAN-трафик идёт обычной маршрутизацией через
# fw4 forward (там accept lan->wan) напрямую в WAN с реальным IP. `block` в
# конфиге это app-level и отсутствие приложения не покрывает.
#
# Решение: отдельная ПЕРСИСТЕНТНАЯ таблица `xr_killswitch` с forward-хуком,
# которая роняет ровно то, что забирает redirect (весь LAN TCP, кроме
# явно-Direct портов, плюс QUIC), но пропускает локалки, адреса VPS и bypass_ips.
# Пока xr-client жив, его redirect уводит трафик в INPUT (DNAT-to-local), до
# forward он не доходит, и правило спит. Как только redirect снят, трафик идёт
# через forward и режется вместо утечки. Таблицей управляет init-скрипт, а не
# xr-client, поэтому она переживает падения/рестарты приложения.
#
# priority -100: раньше fw4 (filter=0), поэтому наш drop срабатывает до его
# accept lan->wan. verdict `accept` в нашей цепочке не финальный между base-
# цепочками, так что легитимный не-web forward дальше решает fw4.

export PATH="/usr/sbin:/usr/bin:/sbin:/bin"

CONFIG="${1:-/etc/xr-proxy/config.toml}"
LAN=br-lan

NFT=""
for p in /usr/sbin/nft /sbin/nft /usr/bin/nft; do
    [ -x "$p" ] && NFT="$p" && break
done
[ -z "$NFT" ] && { echo "nft not found" >&2; exit 1; }

# Порты, которые redirect НЕ забирает (идут Direct и при живом прокси): их
# kill-switch тоже не трогает. Держать в синхроне с redirect.rs.
EXCLUDE_PORTS="22, 25, 53, 110, 143, 465, 587, 853, 993, 995, 1080, 3478, 5060, 5061"

# Адреса VPS из [[servers]] (и legacy [server]). Это туннельные эндпоинты, не резать.
SERVERS=$(grep -E '^address = ' "$CONFIG" 2>/dev/null | grep -oE '[0-9]+\.[0-9]+\.[0-9]+\.[0-9]+')

# bypass_ips = ["a","b"] это устройства, всегда идущие Direct, их не резать.
BYPASS=$(grep -E '^bypass_ips' "$CONFIG" 2>/dev/null | grep -oE '[0-9]+\.[0-9]+\.[0-9]+\.[0-9]+')

"$NFT" delete table ip xr_killswitch 2>/dev/null

"$NFT" add table ip xr_killswitch
"$NFT" add chain ip xr_killswitch forward '{ type filter hook forward priority -100 ; policy accept ; }'

# Управляем только форвардом из LAN.
"$NFT" add rule ip xr_killswitch forward iifname != "$LAN" accept
# Локальные назначения (LAN-to-LAN, инфраструктура) не трогаем.
"$NFT" add rule ip xr_killswitch forward ip daddr '{ 192.168.0.0/16, 10.0.0.0/8, 172.16.0.0/12, 127.0.0.0/8, 169.254.0.0/16 }' accept
# Туннельные эндпоинты.
for s in $SERVERS; do
    "$NFT" add rule ip xr_killswitch forward ip daddr "$s" accept
done
# Явно исключённые из проксирования устройства.
for b in $BYPASS; do
    "$NFT" add rule ip xr_killswitch forward ip saddr "$b" accept
done
# Всё, что забрал бы redirect (весь TCP кроме Direct-портов), плюс QUIC, режем.
"$NFT" add rule ip xr_killswitch forward meta l4proto tcp tcp dport != "{ $EXCLUDE_PORTS }" drop
"$NFT" add rule ip xr_killswitch forward meta l4proto udp udp dport 443 drop

logger -t xr-killswitch "kill-switch installed (fail-closed forward drop)"
