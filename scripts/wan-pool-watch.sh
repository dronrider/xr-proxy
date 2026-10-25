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
SLOTS=0
for member in $MEMBERS; do SLOTS=$((SLOTS + 1)); done

apply_pool() (
    if ! mkdir "$STATE/lock" 2>/dev/null; then exit 1; fi
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

kernel_snapshot() {
    ip -4 route show table all > "$STATE/kernel.routes" || return 1
    ip -4 rule show > "$STATE/kernel.rules" || return 1
    awk -v slots="$SLOTS" '
        {for(i=1;i<NF;i++) if($i=="table" && (($(i+1)>=210 && $(i+1)<210+slots) || $(i+1)==218)) {print;break}}
    ' "$STATE/kernel.routes" | LC_ALL=C sort
    awk -v slots="$SLOTS" '
        {p=$1;sub(/:$/, "", p)}
        (p>=11000 && p<11000+slots) || (p>=11100 && p<11100+slots) || p==32763 || p==32764
    ' "$STATE/kernel.rules" | LC_ALL=C sort
}

route_present() {
    awk -v table="$1" -v kind="$2" -v dev="${3:-}" -v gateway="${4:-}" '
        {t="";d="";g="";m="";for(i=1;i<NF;i++) {
            if($i=="table")t=$(i+1);if($i=="dev")d=$(i+1);
            if($i=="via")g=$(i+1);if($i=="metric")m=$(i+1)
        }}
        t==table && ((kind=="unreachable" && $1=="unreachable" && $2=="default" && m==32767) ||
            (kind=="probe" && $1=="default" && d==dev && g==gateway && m==10)) {found=1}
        END {exit !found}
    ' "$STATE/kernel.candidate"
}

rule_present() {
    awk -v priority="$1:" -v selector="$2" -v table="$3" -v extra="${4:-}" '
        {$1=$1}
        $1==priority && index(" "$0" ", " "selector" ") &&
            (extra=="" || index(" "$0" ", " "extra" ")) {
            for(i=1;i<NF;i++) if($i=="lookup" && $(i+1)==table) found=1
        }
        END {exit !found}
    ' "$STATE/kernel.candidate"
}

kernel_valid() {
    slot=0
    fallback_dev=
    fallback_gateway=
    for member in $MEMBERS; do
        dev=${member%:*}
        table=$((210 + slot))
        mark=$(printf '0x%x/0xf000000' "$(((slot + 1) * 16777216))")
        route_present "$table" unreachable || return 1
        rule_present "$((11000 + slot))" "fwmark $mark" "$table" || return 1
        rule_present "$((11100 + slot))" "oif $dev" "$table" || return 1
        gateway=$(awk -v dev="$dev" '$1=="default" {d="";g="";own=0;for(i=1;i<=NF;i++){if($i=="dev")d=$(i+1);if($i=="via")g=$(i+1);if($i=="table" && $(i+1)>=210 && $(i+1)<=218)own=1}if(d==dev && !own && g!=""){print g;exit}}' "$STATE/kernel.routes")
        if ip link show dev "$dev" 2>/dev/null | grep -q LOWER_UP &&
           test -n "$gateway" && ip -o -4 addr show dev "$dev" | grep -q ' inet '; then
            route_present "$table" probe "$dev" "$gateway" || return 1
            if test -z "$fallback_dev" && ! test -f "$STATE/down.$dev"; then
                fallback_dev=$dev
                fallback_gateway=$gateway
            fi
        fi
        slot=$((slot + 1))
    done
    if test -n "$fallback_dev"; then
        route_present 218 probe "$fallback_dev" "$fallback_gateway" || return 1
    fi
    route_present 218 unreachable &&
        rule_present 32763 'not from all fwmark 0x200/0x200' main 'suppress_prefixlength 0' &&
        rule_present 32764 'not from all fwmark 0x200/0x200' 218
}

refresh_pool() {
    if apply_pool && kernel_snapshot > "$STATE/kernel.candidate" && kernel_valid; then
        mv "$STATE/kernel.candidate" "$STATE/kernel.applied"
    else
        logger -t xr-wan-pool "Failed to refresh WAN pool; retry on next check"
        return 1
    fi
}

while :; do
    kernel_snapshot > "$STATE/kernel.current"
    # netifd can flush our probe routes without changing the provider snapshot.
    # Repair them before probing, including routes of members already marked down.
    if test -f "$STATE/kernel.applied" && ! cmp -s "$STATE/kernel.current" "$STATE/kernel.applied"; then
        refresh_pool || true
    fi
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
    if ! cmp -s "$STATE/current" "$STATE/applied" || ! test -f "$STATE/kernel.applied" || ! nft list table ip xr_wan_pool >/dev/null 2>&1; then
        if refresh_pool; then
            cp "$STATE/current" "$STATE/applied"
        fi
    fi
    test "$ONCE" = 0 || exit 0
    sleep "$INTERVAL"
done
