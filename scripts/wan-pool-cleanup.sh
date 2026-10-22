#!/bin/sh
set -eu
STATE=${XR_WAN_POOL_STATE_DIR:-/var/run/xr-wan-pool}
[ -f "$STATE/owned" ] || exit 0
mkdir "$STATE/lock" 2>/dev/null || { echo 'wan-pool: another operation is running' >&2; exit 1; }
trap 'rmdir "$STATE/lock"' EXIT
trap 'exit 1' HUP INT TERM
nft delete table ip xr_wan_pool 2>/dev/null || true
SLOT=0
while read -r DEV; do
    TABLE=$((210 + SLOT)); PRIORITY=$((11000 + SLOT)); OIF_PRIORITY=$((11100 + SLOT))
    ip -4 rule del priority "$PRIORITY" 2>/dev/null || true
    ip -4 rule del priority "$OIF_PRIORITY" 2>/dev/null || true
    ip -4 route flush table "$TABLE"
    SLOT=$((SLOT + 1))
done < "$STATE/owned"
rm -f "$STATE/owned"
echo 'wan-pool: removed; main route unchanged' >&2 || true
