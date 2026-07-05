#!/bin/sh
# Снять fail-closed kill-switch (XR-077). Вызывается только при ПОЛНОЙ остановке
# сервиса (init stop): администратор осознанно выключает проксирование, значит
# LAN должен вернуться к обычному интернету. При крашах/рестартах xr-client эту
# таблицу НЕ трогаем, иначе снова появится окно Direct-утечки.

export PATH="/usr/sbin:/usr/bin:/sbin:/bin"

NFT=""
for p in /usr/sbin/nft /sbin/nft /usr/bin/nft; do
    [ -x "$p" ] && NFT="$p" && break
done
[ -z "$NFT" ] && exit 0

"$NFT" delete table ip xr_killswitch 2>/dev/null && \
    logger -t xr-killswitch "kill-switch removed (service stop)"
exit 0
