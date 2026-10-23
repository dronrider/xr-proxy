#!/bin/sh
set -eu

CONFIG=${XR_WAN_POOL_CONFIG:-/etc/xr-proxy/wan-pool.conf}
SETUP=${XR_WAN_POOL_SETUP:-/usr/bin/wan-pool-setup.sh}
STATE=${XR_WAN_POOL_STATE:-/var/run/xr-wan-pool-watch}
SYSFS=${XR_WAN_POOL_SYSFS:-/sys/class/net}
ONCE=0
case "${1:-}" in --once) ONCE=1 ;; "") ;; *) echo "usage: $0 [--once]" >&2; exit 2 ;; esac
test -r "$CONFIG" || exit 0
. "$CONFIG"
: "${LAN:=br-lan}" "${MEMBERS:?MEMBERS is required}" "${BYPASS_IPS:=}"
: "${PROBE_TARGETS:=1.1.1.1 9.9.9.9}" "${INTERVAL:=5}"
case "$INTERVAL" in ''|*[!0-9]*) exit 2 ;; esac
test "$INTERVAL" -ge 1 || exit 2
mkdir -p "$STATE"

apply_pool() (
    if ! mkdir "$STATE/lock" 2>/dev/null; then exit 0; fi
    trap 'rmdir "$STATE/lock" 2>/dev/null || true' EXIT HUP INT TERM
    set -- --lan "$LAN"
    for member in $MEMBERS; do
        set -- "$@" --member "$member"
        dev=${member%:*}
        case "$dev" in ''|*[!a-zA-Z0-9_.:-]*) exit 2 ;; esac
        if test -f "$STATE/down.$dev"; then set -- "$@" --down "$dev"; fi
    done
    for address in $BYPASS_IPS; do set -- "$@" --bypass "$address"; done
    "$SETUP" "$@"
)

while :; do
    for member in $MEMBERS; do
        dev=${member%:*}
        case "$dev" in ''|*[!a-zA-Z0-9_.:-]*) exit 2 ;; esac
        good=0
        carrier=$(cat "$SYSFS/$dev/carrier" 2>/dev/null || echo 0)
        if test "$carrier" = 1; then
            for target in $PROBE_TARGETS; do
                if ping -4 -n -I "$dev" -c 1 -W 1 -w 2 "$target" >/dev/null 2>&1; then
                    good=1
                    break
                fi
            done
        fi
        if test "$good" = 1; then
            echo 0 > "$STATE/fail.$dev"
            count=$(cat "$STATE/pass.$dev" 2>/dev/null || echo 0)
            count=$((count + 1))
            test "$count" -le 3 || count=3
            echo "$count" > "$STATE/pass.$dev"
            if test "$count" -ge 3 && test -f "$STATE/down.$dev"; then
                mv "$STATE/down.$dev" "$STATE/recovered.$dev"
                logger -t xr-wan-pool "WAN $dev restored"
            fi
        else
            echo 0 > "$STATE/pass.$dev"
            count=$(cat "$STATE/fail.$dev" 2>/dev/null || echo 0)
            count=$((count + 1))
            test "$count" -le 3 || count=3
            echo "$count" > "$STATE/fail.$dev"
            if { test "$count" -ge 3 || test "$carrier" != 1; } && ! test -f "$STATE/down.$dev"; then
                : > "$STATE/down.$dev"
                logger -t xr-wan-pool "WAN $dev unavailable"
            fi
        fi
    done
    {
        printf '%s\n' "$LAN" "$MEMBERS" "$BYPASS_IPS"
        for member in $MEMBERS; do
            dev=${member%:*}
            ip -4 -o addr show dev "$dev" 2>/dev/null || true
            ip -4 route show table all default dev "$dev" 2>/dev/null |
                awk '{own=0; for(i=1;i<=NF;i++) if($i=="table" && $(i+1)>=210 && $(i+1)<=218) own=1; if(!own) print}'
            if test -f "$STATE/down.$dev"; then echo "down $dev"; fi
        done
    } > "$STATE/current"
    if ! cmp -s "$STATE/current" "$STATE/applied" || ! nft list table ip xr_wan_pool >/dev/null 2>&1; then
        if apply_pool; then
            cp "$STATE/current" "$STATE/applied"
        else
            logger -t xr-wan-pool "Failed to refresh WAN pool; retry on next check"
        fi
    fi
    test "$ONCE" = 0 || exit 0
    sleep "$INTERVAL"
done
