#!/bin/sh
# XR Proxy Watchdog — удаляет правила перенаправления, если xr-client не работает.
# Устанавливается в cron и проверяет каждую минуту.
#
# Это страховка: если клиент упал или завис, интернет через роутер
# восстанавливается автоматически в течение 1 минуту.

# Полный PATH — cron на OpenWRT имеет минимальный PATH
export PATH="/usr/sbin:/usr/bin:/sbin:/bin"

is_running() {
    # Несколько способов проверки — busybox на OpenWRT может не иметь pgrep -x
    if command -v pidof >/dev/null 2>&1; then
        pidof xr-client >/dev/null 2>&1 && return 0
    fi
    if command -v pgrep >/dev/null 2>&1; then
        pgrep -f "xr-client" >/dev/null 2>&1 && return 0
    fi
    # Fallback: проверка через /proc
    for pid_dir in /proc/[0-9]*; do
        [ -f "$pid_dir/cmdline" ] || continue
        if grep -q "xr-client" "$pid_dir/cmdline" 2>/dev/null; then
            return 0
        fi
    done
    return 1
}

cleanup_nftables() {
    local nft=""
    for p in /usr/sbin/nft /sbin/nft /usr/bin/nft; do
        [ -x "$p" ] && nft="$p" && break
    done
    [ -z "$nft" ] && return

    "$nft" list table ip xr_proxy >/dev/null 2>&1 && {
        "$nft" delete table ip xr_proxy
        logger -t xr-watchdog "nftables rules removed (xr-client not running)"
    }
}

cleanup_iptables() {
    local ipt=""
    for p in /usr/sbin/iptables /sbin/iptables /usr/bin/iptables; do
        [ -x "$p" ] && ipt="$p" && break
    done
    [ -z "$ipt" ] && return

    "$ipt" -t nat -L XR_PROXY -n >/dev/null 2>&1 && {
        "$ipt" -t nat -D PREROUTING -j XR_PROXY 2>/dev/null
        "$ipt" -t nat -F XR_PROXY 2>/dev/null
        "$ipt" -t nat -X XR_PROXY 2>/dev/null
        logger -t xr-watchdog "iptables rules removed (xr-client not running)"
    }
}

if ! is_running; then
    cleanup_nftables
    cleanup_iptables
fi
