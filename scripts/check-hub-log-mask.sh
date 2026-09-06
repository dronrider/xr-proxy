#!/bin/sh
# Проверка маскировки токенов в собственном логе хаба живьём (XR-198).
# Поднимает xr-hub из свежей сборки под RUST_LOG=debug, ходит по инвайт-ссылке
# с токеном в пути и по ручке с токеном в query, потом судит лог: токена в нём
# нет, отметка <masked> есть. Судится кодом возврата, последняя строка успеха
# «маскировка лога хаба: ок». Порт 18777, чтобы не сталкиваться с соседними
# проверками (check-share-web.sh, check-git-share.sh).
set -eu
cd "$(dirname "$0")/.."

cargo build -p xr-hub

S=$(mktemp -d)
HUB=
cleanup() {
  [ -n "$HUB" ] && kill "$HUB" 2>/dev/null && wait "$HUB" 2>/dev/null
  rm -rf "$S"
}
trap cleanup EXIT

mkdir -p "$S/data"
cat > "$S/hub.toml" <<EOF
[server]
bind = "127.0.0.1:18777"
data_dir = "$S/data"
[admin]
users = []
EOF

RUST_LOG=debug ./target/debug/xr-hub --config "$S/hub.toml" > "$S/hub.log" 2>&1 &
HUB=$!
for _ in $(seq 1 40); do
  test "$(curl -s --max-time 1 http://127.0.0.1:18777/healthz)" = ok && break
  sleep 0.5
done
test "$(curl -s --max-time 1 http://127.0.0.1:18777/healthz)" = ok

T=XR198CHECK0000000000AA
curl -s -o /dev/null "http://127.0.0.1:18777/invite/$T"
curl -s -o /dev/null "http://127.0.0.1:18777/api/v1/invite/$T"
curl -s -o /dev/null "http://127.0.0.1:18777/api/v1/shares?token=$T"

kill "$HUB"; wait "$HUB" 2>/dev/null || true
HUB=

if grep -q "$T" "$S/hub.log"; then
  echo "токен уехал в лог хаба" >&2
  exit 1
fi
test "$(grep -c '<masked>' "$S/hub.log")" -ge 3 || {
  echo "в логе хаба нет отметок маскировки" >&2
  exit 1
}
# tracing красит поля лога ANSI-кодами, поэтому ищется значение поля, а не «uri=».
grep -q '/invite/<masked>' "$S/hub.log"
grep -q '/api/v1/shares?token=<masked>' "$S/hub.log"

echo "маскировка лога хаба: ок"
