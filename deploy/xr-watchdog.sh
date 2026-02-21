#!/bin/sh
# XR Proxy Watchdog — удаляет правила перенаправления, если xr-client не работает.
# Устанавливается в cron и проверяет каждую минуту.
#
# Это страховка: если клиент упал или завис, интернет через роутер
# восстанавливается автоматически в течение 1 минуты.

PROCESS_NAME="xr-client"

is_running() {
    pgrep -x "$PROCESS_NAME" >/dev/null 2>&1
}

cleanup_nftables() {
    nft list table ip xr_proxy >/dev/null 2>&1 && {
        nft delete table ip xr_proxy
        logger -t xr-watchdog "nftables rules removed (xr-client not running)"
    }
}

cleanup_iptables() {
    iptables -t nat -L XR_PROXY >/dev/null 2>&1 && {
        iptables -t nat -D PREROUTING -j XR_PROXY 2>/dev/null
        iptables -t nat -F XR_PROXY 2>/dev/null
        iptables -t nat -X XR_PROXY 2>/dev/null
        logger -t xr-watchdog "iptables rules removed (xr-client not running)"
    }
}

if ! is_running; then
    cleanup_nftables
    cleanup_iptables
fi
