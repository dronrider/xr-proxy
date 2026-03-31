#!/bin/sh
# Setup nftables TPROXY rules for UDP relay.
# Only intercepts UDP from specified devices — all other traffic passes through untouched.
#
# Usage:
#   udp-tproxy-setup.sh                          — read IPs from config.toml
#   udp-tproxy-setup.sh 192.168.1.188            — one device
#   udp-tproxy-setup.sh 192.168.1.188 192.168.1.240  — multiple devices
#
# If no IPs are given and config has empty source_ips, the script refuses to run
# (to prevent accidentally intercepting all LAN UDP including games/VoIP).

set -e

TABLE="xr_udp_relay"
FWMARK="0x200"
ROUTE_TABLE="201"
CONFIG="/etc/xr-proxy/config.toml"
TPROXY_PORT="1081"

# ── Collect source IPs ──────────────────────────────────────────────

SOURCE_IPS=""

if [ $# -gt 0 ]; then
    # IPs from command line arguments
    for arg in "$@"; do
        case "$arg" in
            *.*.*.*)  SOURCE_IPS="$SOURCE_IPS $arg" ;;
            *)        TPROXY_PORT="$arg" ;;
        esac
    done
else
    # Read from config.toml
    if [ -f "$CONFIG" ]; then
        # Parse listen_port
        port=$(grep -A20 '^\[udp_relay\]' "$CONFIG" 2>/dev/null | grep -v '^ *#' | grep 'listen_port' | head -1 | grep -o '[0-9]*')
        [ -n "$port" ] && TPROXY_PORT="$port"

        # Parse source_ips = ["192.168.1.188", "192.168.1.240"]
        # Skip commented lines (starting with #)
        SOURCE_IPS=$(grep -A20 '^\[udp_relay\]' "$CONFIG" 2>/dev/null | grep -v '^ *#' | grep 'source_ips' | head -1 | \
            sed 's/.*\[//; s/\].*//; s/"//g; s/,/ /g; s/^ *//; s/ *$//')
    fi
fi

# Trim whitespace
SOURCE_IPS=$(echo "$SOURCE_IPS" | xargs)

# ── Safety check ────────────────────────────────────────────────────

if [ -z "$SOURCE_IPS" ]; then
    echo "ERROR: No source IPs specified."
    echo ""
    echo "Without source IPs, ALL LAN UDP traffic (games, VoIP, video calls)"
    echo "would be intercepted and broken when the proxy is stopped."
    echo ""
    echo "Fix: add source_ips to config.toml:"
    echo '  source_ips = ["192.168.1.188"]'
    echo ""
    echo "Or pass IPs as arguments:"
    echo "  $0 192.168.1.188 192.168.1.240"
    exit 1
fi

# ── Find nft ────────────────────────────────────────────────────────

NFT=""
for p in /usr/sbin/nft /sbin/nft /usr/bin/nft; do
    [ -x "$p" ] && NFT="$p" && break
done
[ -z "$NFT" ] && echo "ERROR: nft not found" && exit 1

# ── Build nftables rules ───────────────────────────────────────────

# Get router's own LAN IP to exclude
ROUTER_IP=$(ip -4 addr show br-lan 2>/dev/null | grep -o 'inet [0-9.]*' | awk '{print $2}')
[ -z "$ROUTER_IP" ] && ROUTER_IP=$(ip -4 route show default | awk '{print $7}' | head -1)

# Build nft set of source IPs
# Single IP: "ip saddr 192.168.1.188"
# Multiple:  "ip saddr { 192.168.1.188, 192.168.1.240 }"
ip_count=$(echo "$SOURCE_IPS" | wc -w)
if [ "$ip_count" -eq 1 ]; then
    NFT_SADDR="ip saddr $SOURCE_IPS"
else
    NFT_SET=$(echo "$SOURCE_IPS" | tr ' ' ',')
    NFT_SADDR="ip saddr { $NFT_SET }"
fi

echo "Setting up UDP TPROXY for [$SOURCE_IPS] -> port $TPROXY_PORT"

# Clean up existing rules
"$NFT" delete table ip "$TABLE" 2>/dev/null || true
"$NFT" delete table inet "$TABLE" 2>/dev/null || true

"$NFT" -f - <<EOF
add table ip $TABLE
add chain ip $TABLE prerouting { type filter hook prerouting priority mangle; policy accept; }
add rule ip $TABLE prerouting meta l4proto != udp return
add rule ip $TABLE prerouting $NFT_SADDR udp dport != { 53, 67, 68 } meta mark set $FWMARK
add rule ip $TABLE prerouting $NFT_SADDR udp dport != { 53, 67, 68 } tproxy to :$TPROXY_PORT
EOF

# Policy routing: marked packets go to local loopback
ip rule del fwmark "$FWMARK" table "$ROUTE_TABLE" 2>/dev/null || true
ip rule add fwmark "$FWMARK" table "$ROUTE_TABLE"
ip route replace local default dev lo table "$ROUTE_TABLE"

echo "Done."
echo "Only intercepting UDP from: $SOURCE_IPS"
[ -n "$ROUTER_IP" ] && echo "Router IP: $ROUTER_IP (not affected)"
echo ""
echo "To verify:  $NFT list table ip $TABLE"
echo "To remove:  $NFT delete table ip $TABLE && ip rule del fwmark $FWMARK table $ROUTE_TABLE"
