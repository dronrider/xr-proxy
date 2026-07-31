#!/bin/sh
# Ежедневный бэкап состояния хаба на второй VPS (XR-224). Собирает архив
# командой `xr-hub backup`, отправляет свежий по ssh на приёмник, где ключ
# зажат forced command на deploy/xr-hub-backup-receive.sh, и сторожит метку
# последнего успеха. Молчание не должно быть неотличимо от работы, поэтому
# и провал отправки, и ненастроенный env уходят алертом в Telegram (тот же
# /etc/xr-proxy/alert.env, что у cert-alert.sh).
# Адрес приёмника и ключ в env-файле (в git не попадает, chmod 600):
#   BACKUP_HOST=203.0.113.7
#   BACKUP_KEY=/root/.ssh/xr-hub-backup
set -eu

BIN="${BIN:-/usr/local/bin/xr-hub}"
CONFIG="${CONFIG:-/etc/xr-hub/config.toml}"
OUT="${OUT:-/var/backups/xr-hub}"
KEEP="${KEEP:-14}"
ENV_FILE="${ENV_FILE:-/etc/xr-hub/backup.env}"
ALERT_ENV="${ALERT_ENV:-/etc/xr-proxy/alert.env}"
STAMP="${STAMP:-/var/backups/xr-hub/.last-sent}"
MAX_AGE_DAYS="${MAX_AGE_DAYS:-2}"

# Без alert.env слать алерт некуда: тогда громко ругаемся в stderr (cron
# принесёт это письмом) и выходим нулём, как cert-alert.sh.
alert() {
  if [ ! -f "$ALERT_ENV" ]; then
    echo "xr-hub-backup: $1 (и нет $ALERT_ENV, алерт слать некуда)" >&2
    exit 0
  fi
  . "$ALERT_ENV"
  curl -fsS -m 20 "https://api.telegram.org/bot${TG_TOKEN}/sendMessage" \
    -d chat_id="${TG_CHAT}" \
    -d text="xr-hub: $1 (хост $(hostname)). XR-224" >/dev/null || true
  echo "xr-hub-backup: $1" >&2
  exit 1
}

"$BIN" --config "$CONFIG" backup --out "$OUT" --keep "$KEEP" \
  || alert "бэкап не собрался"

[ -f "$ENV_FILE" ] || alert "нет $ENV_FILE, архив никуда не уезжает"
. "$ENV_FILE"
[ -n "${BACKUP_HOST:-}" ] && [ -n "${BACKUP_KEY:-}" ] \
  || alert "в $ENV_FILE не заполнены BACKUP_HOST и BACKUP_KEY"

latest=$(ls -1 "$OUT"/xr-hub-backup-*.tar.gz 2>/dev/null | sort | tail -1)
[ -n "$latest" ] || alert "в $OUT нет ни одного архива"

ssh -i "$BACKUP_KEY" -o StrictHostKeyChecking=accept-new -o ConnectTimeout=15 \
  -o BatchMode=yes "root@${BACKUP_HOST}" < "$latest" \
  || alert "архив $(basename "$latest") не уехал на приёмник"

mkdir -p "$(dirname "$STAMP")"
date +%s > "$STAMP"
echo "xr-hub-backup: отправлен $(basename "$latest") ($(wc -c < "$latest") байт)"

# Сторож свежести. После удачной отправки метке ноль дней, так что порог
# стреляет, когда она не обновилась (нет прав, полный диск): иначе провал
# записи метки был бы неотличим от успеха. Порог занижают руками, чтобы
# проверить, что алерт доходит.
age_days=$(( ( $(date +%s) - $(cat "$STAMP") ) / 86400 ))
[ "$age_days" -lt "$MAX_AGE_DAYS" ] \
  || alert "последний успешный бэкап был $age_days дн. назад"
