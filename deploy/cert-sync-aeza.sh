#!/bin/sh
# Deploy-hook certbot на Timeweb-хабе (XR-124): после успешного продления
# отправляет свежую пару на Aeza, где её принимает deploy/cert-receive.sh
# (forced command ключа cert-sync, бэкап + рестарт nginx на той стороне).
# Ставится в /etc/letsencrypt/renewal-hooks/deploy/, certbot зовёт его только
# при реально обновлённом серте; для ручного прогона запустить как есть.
set -eu

AEZA_HOST="${AEZA_HOST:-85.192.38.29}"
AEZA_KEY="${AEZA_KEY:-/root/.ssh/aeza-cert-sync}"
LIVE="${RENEWED_LINEAGE:-/etc/letsencrypt/live/xr-hub.zoobr.top}"

# live/*.pem это симлинки в archive, поэтому tar с -h (разыменовать).
cd "$LIVE"
tar -ch fullchain.pem privkey.pem | ssh -i "$AEZA_KEY" \
  -o StrictHostKeyChecking=accept-new -o ConnectTimeout=15 -o BatchMode=yes \
  "root@${AEZA_HOST}"
