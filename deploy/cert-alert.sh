#!/bin/sh
# Сторож срока серта xr-hub.zoobr.top (XR-124): продление однажды месяц молча
# падало, поэтому ежедневный cron на обоих хабах сверяет дни до истечения и
# ниже порога шлёт алерт в Telegram. Certbot продлевает за 30 дней, значит
# 14 дней до конца это уже две недели сломанного продления.
# Токен и чат в env-файле (в git не попадает, chmod 600):
#   TG_TOKEN=123456:ABC...
#   TG_CHAT=123456789
set -eu

CERT="${CERT:-/etc/letsencrypt/live/xr-hub.zoobr.top/fullchain.pem}"
WARN_DAYS="${WARN_DAYS:-14}"
ENV_FILE="${ENV_FILE:-/etc/xr-proxy/alert.env}"

[ -f "$ENV_FILE" ] || { echo "cert-alert: нет $ENV_FILE, алерт не настроен" >&2; exit 0; }
. "$ENV_FILE"

end=$(openssl x509 -enddate -noout -in "$CERT" | cut -d= -f2)
end_ts=$(date -d "$end" +%s)
days=$(( (end_ts - $(date +%s)) / 86400 ))
[ "$days" -lt "$WARN_DAYS" ] || exit 0

curl -fsS -m 20 "https://api.telegram.org/bot${TG_TOKEN}/sendMessage" \
  -d chat_id="${TG_CHAT}" \
  -d text="xr-hub: серту осталось ${days} дн. (хост $(hostname)), похоже продление снова сломалось. XR-124" \
  >/dev/null
