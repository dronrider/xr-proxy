#!/bin/sh
# IPv4 pool and ordered failover for unbound router/bypass traffic.
# Usage: wan-pool-setup.sh --member wan:1 --member lan1:3 --bypass 192.168.1.10
# Reapply with the SAME member order after DHCP/interface changes. Weights may change.
# Requires existing firewall permissions LAN -> each WAN, without changing input rules.
# --down DEV excludes new flows after a failed health check but keeps probe routes.
# IPv6 and routed point-to-point WANs are outside this script.
set -eu
trap '' PIPE
STATE=${XR_WAN_POOL_STATE_DIR:-/var/run/xr-wan-pool}
LAN=br-lan
MEMBERS=
BYPASS=
DOWN=
fail() { echo "wan-pool: $*" >&2; exit 1; }
device_ok() { case "$1" in ''|*[!a-zA-Z0-9_.:-]*) return 1;; esac; [ ${#1} -le 15 ]; }
ip_ok() { printf '%s\n' "$1" | awk -F. 'NF != 4 {exit 1} {for(i=1;i<=4;i++) if($i !~ /^[0-9]+$/ || $i>255) exit 1}'; }
range_ok() {
    printf '%s\n' "$1" | awk '
    function ipv4(s, a,n,i) {n=split(s,a,".");if(n!=4)return 0;for(i=1;i<=4;i++)if(a[i]!~/^[0-9]+$/ || a[i]>255 || (length(a[i])>1 && substr(a[i],1,1)=="0"))return 0;return 1}
    {
        n=split($0,parts,"/"); if(n>2 || (n==2 && parts[2]!~/^[0-9]+$/))exit 1;
        s=parts[1]; v6=index(s,":")>0;
        if(n==2 && parts[2]>(v6?128:32))exit 1;
        if(!v6)exit !ipv4(s);
        if(s!~/^[0-9a-fA-F:.]+$/ || index(s,":::"))exit 1;
        if((substr(s,1,1)==":" && substr(s,1,2)!="::") || (substr(s,length(s),1)==":" && substr(s,length(s)-1)!="::"))exit 1;
        compressed=index(s,"::")>0; copy=s; if(gsub(/::/,"",copy)>1)exit 1;
        count=split(s,groups,":");units=0;
        for(i=1;i<=count;i++) {
            if(groups[i]==""){if(!compressed)exit 1;continue}
            if(index(groups[i],".")){if(i!=count || !ipv4(groups[i]))exit 1;units+=2}
            else {if(length(groups[i])>4 || groups[i]!~/^[0-9a-fA-F]+$/)exit 1;units++}
        }
        exit !(compressed?units<8:units==8);
    }'
}
while [ "$#" -gt 0 ]; do
    [ "$#" -ge 2 ] || fail 'missing argument'
    case "$1" in
        --lan) LAN=$2; device_ok "$LAN" || fail 'invalid LAN device';;
        --member)
            DEV=${2%:*}; WEIGHT=${2##*:}
            device_ok "$DEV" || fail 'invalid WAN device'
            case "$WEIGHT" in ''|*[!0-9]*) fail 'invalid weight';; esac
            [ "$WEIGHT" -ge 1 ] && [ "$WEIGHT" -le 4294967295 ] || fail 'weight outside 1..u32::MAX'
            WEIGHT=$(printf '%s\n' "$WEIGHT" | awk '{printf "%.0f",$0}')
            for SEEN in $MEMBERS; do [ "${SEEN%:*}" != "$DEV" ] || fail 'duplicate device'; done
            MEMBERS="$MEMBERS $DEV:$WEIGHT";;
        --down) device_ok "$2" || fail 'invalid down device'; DOWN="$DOWN $2";;
        --bypass) range_ok "$2" || fail 'invalid bypass IP/CIDR'; BYPASS="$BYPASS $2";;
        *) fail "unknown argument: $1";;
    esac
    shift 2
done
[ -n "$MEMBERS" ] || fail 'no pool members'
set -- $MEMBERS
[ "$#" -ge 1 ] && [ "$#" -le 8 ] || fail 'expected 1..8 pool members'
SUM=$(printf '%s\n' "$MEMBERS" | awk '{total=0;for(i=1;i<=NF;i++){n=split($i,a,":");total+=a[n]}printf "%.0f",total}')
[ "$SUM" -le 4294967295 ] || fail 'total weight overflow'
for MEMBER in $MEMBERS; do [ "${MEMBER%:*}" != "$LAN" ] || fail 'LAN cannot be a WAN'; done
mkdir -p "$STATE"
mkdir "$STATE/lock" 2>/dev/null || fail 'another pool operation is running'
trap 'rm -f "$STATE/routes" "$STATE/nft" "$STATE/active"; rmdir "$STATE/lock"' EXIT
trap 'exit 1' HUP INT TERM
DEVICES=$(for MEMBER in $MEMBERS; do printf '%s\n' "${MEMBER%:*}"; done)
if [ -f "$STATE/owned" ]; then
    [ "$(cat "$STATE/owned")" = "$DEVICES" ] || fail 'member order changed; cleanup first'
else
    nft list table ip xr_wan_pool >/dev/null 2>&1 && fail 'nft table already exists without ownership'
    SLOT=0
    for MEMBER in $MEMBERS; do
        TABLE=$((210 + SLOT)); PRIORITY=$((11000 + SLOT)); OIF_PRIORITY=$((11100 + SLOT))
        [ -z "$(ip -4 route show table "$TABLE" 2>/dev/null)" ] || fail "route table $TABLE is occupied"
        ip -4 rule show | awk -v a="$PRIORITY:" -v b="$OIF_PRIORITY:" '$1==a || $1==b {found=1} END {exit !found}' && fail 'rule priority is occupied'
        SLOT=$((SLOT + 1))
    done
    printf '%s\n' "$DEVICES" > "$STATE/owned"
fi
# A separate lookup after bound-WAN rules preserves netifd-owned routes. Look
# through main's specific prefixes first, but select default only from healthy WANs.
if [ ! -f "$STATE/fallback-owned" ]; then
    [ -z "$(ip -4 route show table 218 2>/dev/null)" ] || fail 'route table 218 is occupied'
    ip -4 rule show | awk '$1=="32763:" || $1=="32764:" {f=1} END {exit !f}' && fail 'fallback rule priority is occupied'
    : > "$STATE/fallback-owned"
fi
# Reserved tables terminate failed lookups: a dead bound/marked WAN must not fall
# through to the main table and leak traffic through another provider.
SLOT=0
TOTAL=0
FALLBACK_DEV=
FALLBACK_GATEWAY=
: > "$STATE/active"
ip -4 route show table all > "$STATE/routes"
for MEMBER in $MEMBERS; do
    DEV=${MEMBER%:*}; WEIGHT=${MEMBER##*:}
    TABLE=$((210 + SLOT)); PRIORITY=$((11000 + SLOT)); OIF_PRIORITY=$((11100 + SLOT)); MARK=$(((SLOT + 1) * 16777216))
    ip -4 route replace unreachable default metric 32767 table "$TABLE"
    if ! ip -4 rule show | awk -v p="$PRIORITY:" '$1==p {f=1} END {exit !f}'; then
        ip -4 rule add priority "$PRIORITY" fwmark "$MARK/0x0f000000" lookup "$TABLE"
    fi
    if ! ip -4 rule show | awk -v p="$OIF_PRIORITY:" '$1==p {f=1} END {exit !f}'; then
        ip -4 rule add priority "$OIF_PRIORITY" oif "$DEV" lookup "$TABLE"
    fi
    GATEWAY=$(awk -v dev="$DEV" '$1=="default" {d="";g="";own=0;for(i=1;i<=NF;i++){if($i=="dev")d=$(i+1);if($i=="via")g=$(i+1);if($i=="table" && $(i+1)>=210 && $(i+1)<=218)own=1}if(d==dev && !own && g!=""){print g;exit}}' "$STATE/routes")
    if ip link show dev "$DEV" 2>/dev/null | grep -q 'LOWER_UP' && [ -n "$GATEWAY" ] && ip -o -4 addr show dev "$DEV" | grep -q ' inet '; then
        ip_ok "$GATEWAY" || fail 'invalid gateway from kernel'
        ip -4 route replace default via "$GATEWAY" dev "$DEV" onlink metric 10 table "$TABLE"
        case " $DOWN " in
            *" $DEV "*) ;;
            *)
                printf '%s %s %s\n' "$DEV" "$MARK" "$WEIGHT" >> "$STATE/active"
                TOTAL=$((TOTAL + WEIGHT))
                if [ -z "$FALLBACK_DEV" ]; then FALLBACK_DEV=$DEV; FALLBACK_GATEWAY=$GATEWAY; fi
                ;;
        esac
    else
        ip -4 route del default metric 10 table "$TABLE" 2>/dev/null || true
    fi
    SLOT=$((SLOT + 1))
done
ip -4 route replace unreachable default metric 32767 table 218
if [ -n "$FALLBACK_DEV" ]; then
    ip -4 route replace default via "$FALLBACK_GATEWAY" dev "$FALLBACK_DEV" onlink metric 10 table 218
else
    ip -4 route del default metric 10 table 218 2>/dev/null || true
fi
if ! ip -4 rule show | awk '$1=="32763:" {f=1} END {exit !f}'; then
    ip -4 rule add priority 32763 not fwmark 0x200/0x200 lookup main suppress_prefixlength 0
fi
if ! ip -4 rule show | awk '$1=="32764:" {f=1} END {exit !f}'; then
    ip -4 rule add priority 32764 not fwmark 0x200/0x200 lookup 218
fi
SELECTED=${FALLBACK_DEV:-none}
if [ "$(cat "$STATE/fallback-selected" 2>/dev/null || true)" != "$SELECTED" ]; then
    echo "wan-pool: router/bypass default WAN $SELECTED" >&2 || true
    printf '%s\n' "$SELECTED" > "$STATE/fallback-selected"
fi
NFT="$STATE/nft"
: > "$NFT"
if nft list table ip xr_wan_pool >/dev/null 2>&1; then echo 'delete table ip xr_wan_pool' >> "$NFT"; fi
cat >> "$NFT" <<EOF
add table ip xr_wan_pool
add chain ip xr_wan_pool select { type filter hook prerouting priority -90; policy accept; }
add chain ip xr_wan_pool nat { type nat hook postrouting priority 101; policy accept; }
add rule ip xr_wan_pool select iifname != "$LAN" return
add rule ip xr_wan_pool select ct direction reply return
add rule ip xr_wan_pool select meta mark & 0x200 != 0 return
add rule ip xr_wan_pool select fib daddr type local return
add rule ip xr_wan_pool select ct status dnat return
add rule ip xr_wan_pool select ip daddr { 0.0.0.0/8, 10.0.0.0/8, 100.64.0.0/10, 127.0.0.0/8, 169.254.0.0/16, 172.16.0.0/12, 192.168.0.0/16, 224.0.0.0/4, 240.0.0.0/4 } return
EOF
for IP in $BYPASS; do
    case "$IP" in *:*) continue;; esac
    case "$IP" in */*)
        IP=$(printf '%s\n' "$IP" | awk -F'[./]' '{p=$5;for(i=1;i<=4;i++){b=p-8*(i-1);if(b<0)b=0;if(b>8)b=8;unit=2^(8-b);printf "%s%d",(i>1?".":""),int($i/unit)*unit}printf "/%d",p}')
        ;;
    esac
    echo "add rule ip xr_wan_pool select ip saddr $IP return" >> "$NFT"
done
# Preserve explicitly routed VPN/public subnets too; only default-route traffic
# belongs to the pool. Kernel-provided prefixes are validated before interpolation.
ip -4 route show table main | awk '$1!="default" {print $1}' | while read -r PREFIX; do
    case "$PREFIX" in *[!0-9./]*|'') continue;; esac
    echo "add rule ip xr_wan_pool select ip daddr $PREFIX return" >> "$NFT"
done
SLOT=0
for MEMBER in $MEMBERS; do
    MARK=$(((SLOT + 1) * 16777216))
    echo "add rule ip xr_wan_pool select ct mark & 0x0f000000 == $MARK meta mark set (meta mark & 0xf0ffffff) | $MARK return" >> "$NFT"
    SLOT=$((SLOT + 1))
done
echo 'add rule ip xr_wan_pool select ct mark & 0x0f000000 != 0 drop' >> "$NFT"
echo 'add rule ip xr_wan_pool select ct state established,related return' >> "$NFT"
if [ "$TOTAL" -gt 0 ]; then
    INDEX=0
    REMAINING=$TOTAL
    while read -r DEV MARK WEIGHT; do
        END=$((INDEX + WEIGHT - 1))
        echo "add rule ip xr_wan_pool select meta l4proto udp jhash ip saddr . udp sport mod $TOTAL seed 0x58525031 $INDEX-$END ct mark set (ct mark & 0xf0ffffff) | $MARK meta mark set (meta mark & 0xf0ffffff) | $MARK return" >> "$NFT"
        INDEX=$((END + 1))
        if [ "$WEIGHT" -eq "$REMAINING" ]; then
            CHOICE=
        else
            CHOICE="numgen random mod $REMAINING < $WEIGHT"
        fi
        echo "add rule ip xr_wan_pool select meta l4proto != udp $CHOICE ct mark set (ct mark & 0xf0ffffff) | $MARK meta mark set (meta mark & 0xf0ffffff) | $MARK return" >> "$NFT"
        REMAINING=$((REMAINING - WEIGHT))
    done < "$STATE/active"
else
    echo 'add rule ip xr_wan_pool select drop' >> "$NFT"
fi
for MEMBER in $MEMBERS; do
    DEV=${MEMBER%:*}
    echo "add rule ip xr_wan_pool nat iifname \"$LAN\" oifname \"$DEV\" meta mark & 0x0f000000 != 0 masquerade" >> "$NFT"
done
nft -c -f "$NFT"
nft -f "$NFT"
echo "wan-pool: applied, active weight $TOTAL; router/bypass WAN $SELECTED" >&2 || true
