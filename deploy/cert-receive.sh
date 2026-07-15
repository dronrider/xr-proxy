#!/bin/sh
# Приёмник серта на Aeza-хабе (XR-124). DNS xr-hub.zoobr.top смотрит только на
# Timeweb, сам Aeza продлиться не может, поэтому Timeweb после продления
# присылает свежую пару сюда (deploy/cert-sync-aeza.sh). Скрипт зажат в
# authorized_keys как forced command отдельного ключа cert-sync: принимает tar
# c fullchain.pem/privkey.pem на stdin, валидирует, бэкапит старые, кладёт в
# /etc/ssl/xr-hub/ и перезапускает nginx (reload новый серт не подхватывает).
set -eu

DEST=/etc/ssl/xr-hub
umask 077
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

tar -x -C "$tmp"
[ -s "$tmp/fullchain.pem" ] && [ -s "$tmp/privkey.pem" ] \
  || { echo "cert-receive: в tar нет fullchain.pem/privkey.pem" >&2; exit 1; }

# Серт парсится и приватный ключ от него же, иначе nginx не встанет.
openssl x509 -noout -in "$tmp/fullchain.pem" \
  || { echo "cert-receive: битый fullchain" >&2; exit 1; }
cert_pub=$(openssl x509 -pubkey -noout -in "$tmp/fullchain.pem")
key_pub=$(openssl pkey -pubout -in "$tmp/privkey.pem")
[ "$cert_pub" = "$key_pub" ] \
  || { echo "cert-receive: privkey не от этого fullchain" >&2; exit 1; }

ts=$(date +%s)
for f in fullchain.pem privkey.pem; do
  [ -f "$DEST/$f" ] && cp -a "$DEST/$f" "$DEST/$f.bak.$ts"
done
install -m 644 "$tmp/fullchain.pem" "$DEST/fullchain.pem"
install -m 600 "$tmp/privkey.pem"  "$DEST/privkey.pem"

systemctl restart nginx
echo "cert-receive: установлен, nginx перезапущен, $(openssl x509 -enddate -noout -in "$DEST/fullchain.pem")"
