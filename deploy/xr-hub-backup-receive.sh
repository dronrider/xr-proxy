#!/bin/sh
# Приёмник бэкапа хаба (XR-224) на втором VPS. Зажат в authorized_keys как
# forced command отдельного ключа: принимает tar.gz на stdin, кладёт под
# именем с меткой времени, оставляет KEEP поколений и обновляет метку
# последнего успешного приёма. Больше этим ключом сделать нечего.
# Строка в /root/.ssh/authorized_keys приёмника:
#   command="/usr/local/bin/xr-hub-backup-receive.sh",no-port-forwarding,\
#   no-agent-forwarding,no-pty ssh-ed25519 AAAA... xr-hub-backup
set -eu

DEST="${DEST:-/var/backups/xr-hub}"
KEEP="${KEEP:-14}"
STAMP="${STAMP:-$DEST/.last-received}"

umask 077
mkdir -p "$DEST"
tmp=$(mktemp "$DEST/.incoming.XXXXXX")
trap 'rm -f "$tmp"' EXIT

cat > "$tmp"
[ -s "$tmp" ] || { echo "xr-hub-backup-receive: пустой поток" >&2; exit 1; }

# Архив обязан быть целым и нашим: оборванная передача и чужой tar до
# каталога поколений не доходят.
gzip -t "$tmp" || { echo "xr-hub-backup-receive: битый gzip" >&2; exit 1; }
tar -tzf "$tmp" | grep -qx MANIFEST.json \
  || { echo "xr-hub-backup-receive: в архиве нет MANIFEST.json" >&2; exit 1; }

name="xr-hub-backup-$(date -u +%Y%m%dT%H%M%SZ).tar.gz"
mv "$tmp" "$DEST/$name"
trap - EXIT
chmod 600 "$DEST/$name"

# Ротация по имени: метка времени в нём делает сортировку хронологической.
total=$(ls -1 "$DEST"/xr-hub-backup-*.tar.gz | wc -l)
if [ "$KEEP" -gt 0 ] && [ "$total" -gt "$KEEP" ]; then
  ls -1 "$DEST"/xr-hub-backup-*.tar.gz | sort | head -n "$((total - KEEP))" \
    | while read -r old; do rm -f "$old"; done
fi

date +%s > "$STAMP"
echo "xr-hub-backup-receive: принят $name ($(wc -c < "$DEST/$name") байт)"
