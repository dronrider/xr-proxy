#!/bin/sh
# Сторож crash-loop сервисов на VPS (XR-226): юниты стоят с Restart=always,
# поэтому упавший сервис systemd крутит бесконечно и молча, клиенты уходят
# на резервный VPS, а оператор узнаёт о падении от пользователей. Раз в минуту
# cron сверяет факты systemd (NRestarts и ActiveState), цикл рестартов шлёт
# алерт в Telegram, восстановление отмечается одним сообщением. Судит по
# systemd, а не по самочувствию самих сервисов: у упавшего оно отсутствует.
# Токен и чат в env-файле (в git не попадает, chmod 600), тот же, что у
# cert-alert.sh:
#   TG_TOKEN=123456:ABC...
#   TG_CHAT=123456789
set -eu

SERVICES="${SERVICES:-xr-proxy-server xr-relay xr-hub}"
ENV_FILE="${ENV_FILE:-/etc/xr-proxy/alert.env}"
STATE_DIR="${STATE_DIR:-/var/lib/xr-service-alert}"
# RestartSec у юнитов 5 с, порог набирается за первые минуты цикла; редкие
# одиночные рестарты алертом не становятся.
THRESHOLD="${THRESHOLD:-10}"
REPEAT_SEC="${REPEAT_SEC:-3600}"

[ -f "$ENV_FILE" ] || { echo "xr-service-alert: нет $ENV_FILE, алерт не настроен" >&2; exit 0; }

send() {
  . "$ENV_FILE"
  echo "xr-service-alert: $1 (хост $(hostname))" >&2
  curl -fsS -m 20 "https://api.telegram.org/bot${TG_TOKEN}/sendMessage" \
    -d chat_id="${TG_CHAT}" \
    -d text="xr-server: $1 (хост $(hostname)). XR-226" >/dev/null || true
}

mkdir -p "$STATE_DIR"
for svc in $SERVICES; do
  load=$(systemctl show "$svc" -p LoadState --value 2>/dev/null) || continue
  [ "$load" = loaded ] || continue
  active=$(systemctl show "$svc" -p ActiveState --value 2>/dev/null) || continue
  n=$(systemctl show "$svc" -p NRestarts --value 2>/dev/null) || continue
  stamp="$STATE_DIR/$svc.alert"

  if [ "$active" = active ]; then
    if [ -f "$stamp" ]; then
      rm -f "$stamp"
      send "сервис $svc восстановился после crash-loop"
    fi
    continue
  fi

  # Остановленный руками сервис это обслуживание, а не падение.
  [ "$active" = inactive ] && continue
  [ "$n" -ge "$THRESHOLD" ] 2>/dev/null || continue

  now=$(date +%s)
  last=$(cat "$stamp" 2>/dev/null || echo 0)
  [ $(( now - last )) -ge "$REPEAT_SEC" ] || continue
  echo "$now" > "$stamp"
  send "сервис $svc в crash-loop: NRestarts=$n, состояние $active"
done
