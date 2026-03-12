#!/bin/sh
# Setup nftables TPROXY rules for UDP relay.
# Run on the OpenWRT router.
#
# Usage:
#   udp-tproxy-setup.sh [tproxy_port]          — relay all LAN devices
#   udp-tproxy-setup.sh 192.168.1.187 [port]   — relay only this IP
#
# Examples:
#   udp-tproxy-setup.sh                  # all LAN, port 1081
#   udp-tproxy-setup.sh 1081             # all LAN, port 1081
#   udp-tproxy-setup.sh 192.168.1.187    # only Switch, port 1081
#   udp-tproxy-setup.sh 192.168.1.187 1081

set -e

TABLE="xr_udp_relay"
FWMARK="0x200"
ROUTE_TABLE="201"

# Parse arguments: detect if first arg is an IP or a port
SOURCE_FILTER=""
TPROXY_PORT="1081"

if [ -n "$1" ]; then
    case "$1" in
        *.*.*) # looks like an IP
            SOURCE_FILTER="$1"
            [ -n "$2" ] && TPROXY_PORT="$2"
            ;;
        *)     # assume it's a port
            TPROXY_PORT="$1"
            ;;
    esac
fi

NFT=""
for p in /usr/sbin/nft /sbin/nft /usr/bin/nft; do
    [ -x "$p" ] && NFT="$p" && break
done
[ -z "$NFT" ] && echo "ERROR: nft not found" && exit 1

# Get router's own LAN IP to exclude it
ROUTER_IP=$(ip -4 addr show br-lan 2>/dev/null | grep -o 'inet [0-9.]*' | awk '{print $2}')
[ -z "$ROUTER_IP" ] && ROUTER_IP=$(ip -4 route show default | awk '{print $7}' | head -1)

# Build source filter rule
if [ -n "$SOURCE_FILTER" ]; then
    SRC_RULE="ip saddr != $SOURCE_FILTER return"
    echo "Setting up UDP TPROXY for $SOURCE_FILTER -> port $TPROXY_PORT"
else
    SRC_RULE="# All LAN devices"
    echo "Setting up UDP TPROXY for all LAN devices -> port $TPROXY_PORT"
fi

# Clean up existing rules
"$NFT" delete table inet "$TABLE" 2>/dev/null || true

"$NFT" -f - <<EOF
table inet $TABLE {
    chain prerouting {
        type filter hook prerouting priority mangle; policy accept;

        # Only process UDP
        meta l4proto != udp return

        # Only process traffic from LAN (private IPs)
        ip saddr != { 10.0.0.0/8, 172.16.0.0/12, 192.168.0.0/16 } return

        # Exclude router's own traffic (avoid loops)
        ip saddr ${ROUTER_IP:-127.0.0.1} return

        # Per-device filter (empty = all LAN)
        $SRC_RULE

        # Skip DNS and DHCP
        udp dport { 53, 67, 68 } return

        # Skip traffic to LAN (local devices, router itself)
        ip daddr { 10.0.0.0/8, 172.16.0.0/12, 192.168.0.0/16, 127.0.0.0/8 } return

        # Mark and TPROXY
        meta mark set $FWMARK
        tproxy to :$TPROXY_PORT
    }
}
EOF

# Policy routing: marked packets go to local loopback
ip rule del fwmark "$FWMARK" table "$ROUTE_TABLE" 2>/dev/null || true
ip rule add fwmark "$FWMARK" table "$ROUTE_TABLE"
ip route replace local default dev lo table "$ROUTE_TABLE"

echo "Done."
[ -n "$ROUTER_IP" ] && echo "Router IP ($ROUTER_IP) excluded."
echo ""
echo "To verify:"
echo "  $NFT list table inet $TABLE"
echo ""
echo "To remove:"
echo "  $NFT delete table inet $TABLE"
echo "  ip rule del fwmark $FWMARK table $ROUTE_TABLE"
