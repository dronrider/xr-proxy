#!/bin/sh
# Remove UDP TPROXY rules.
# Usage: udp-tproxy-cleanup.sh

TABLE="xr_udp_relay"
FWMARK="0x200"
ROUTE_TABLE="201"

NFT=""
for p in /usr/sbin/nft /sbin/nft /usr/bin/nft; do
    [ -x "$p" ] && NFT="$p" && break
done

[ -n "$NFT" ] && "$NFT" delete table ip "$TABLE" 2>/dev/null && echo "nftables ip rules removed"
[ -n "$NFT" ] && "$NFT" delete table inet "$TABLE" 2>/dev/null && echo "nftables inet rules removed"
ip rule del fwmark "$FWMARK" table "$ROUTE_TABLE" 2>/dev/null && echo "Policy route removed"

echo "UDP TPROXY cleanup done"
