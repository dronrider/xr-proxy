#!/bin/sh
# XR Proxy Watchdog — страховка от падений:
# 1. Фиксирует факт и причину краша в /etc/xr-proxy/crash.log
# 2. Чистит firewall-правила (чтобы интернет не пропал)
# 3. Перезапускает через procd
# Запускается из cron каждую минуту.

export PATH="/usr/sbin:/usr/bin:/sbin:/bin"

CRASHLOG=/etc/xr-proxy/crash.log

is_running() {
    if command -v pidof >/dev/null 2>&1; then
        pidof xr-client >/dev/null 2>&1 && return 0
    fi
    if command -v pgrep >/dev/null 2>&1; then
        pgrep -f "xr-client" >/dev/null 2>&1 && return 0
    fi
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
        logger -t xr-watchdog "nftables rules removed"
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
        logger -t xr-watchdog "iptables rules removed"
    }
}

log_crash() {
    {
        echo "=== CRASH $(date '+%Y-%m-%d %H:%M:%S') ==="
        echo "--- dmesg (OOM/kill) ---"
        dmesg | grep -iE "oom|killed|xr-client" | tail -5
        echo "--- logread (last xr entries) ---"
        logread 2>/dev/null | grep -i "xr" | tail -10
        echo "--- memory ---"
        free -m 2>/dev/null || cat /proc/meminfo | head -5
        echo ""
    } >> "$CRASHLOG"

    # Ограничить размер лога (~50 КБ)
    if [ -f "$CRASHLOG" ] && [ "$(wc -c < "$CRASHLOG")" -gt 50000 ]; then
        tail -200 "$CRASHLOG" > "${CRASHLOG}.tmp"
        mv "${CRASHLOG}.tmp" "$CRASHLOG"
    fi
}

if ! is_running; then
    logger -t xr-watchdog "xr-client not running, logging crash and restarting"
    log_crash
    cleanup_nftables
    cleanup_iptables

    # Перезапуск через procd
    if [ -x /etc/init.d/xr-proxy ]; then
        /etc/init.d/xr-proxy start
    fi
else
    # Процесс жив — убедиться что OOM-защита установлена
    pid=$(pidof xr-client 2>/dev/null)
    if [ -n "$pid" ] && [ -f "/proc/$pid/oom_score_adj" ]; then
        current=$(cat "/proc/$pid/oom_score_adj" 2>/dev/null)
        if [ "$current" != "-900" ]; then
            echo -900 > "/proc/$pid/oom_score_adj" 2>/dev/null
            logger -t xr-watchdog "OOM protection set for PID $pid"
        fi
    fi
fi
