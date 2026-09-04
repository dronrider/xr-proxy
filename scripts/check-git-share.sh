#!/bin/bash
# Проверка git-контура шары (XR-188 и XR-189, LLD-33 п. 6 фазы 1 и 2) живьём:
# локальный хаб, агент из свежей сборки, коллега сперва штатный git (шаги 1-7),
# потом две машины на харнессе `xr-share sync` (шаг 8). Несовпадение любого
# ожидания роняет скрипт с ненулевым кодом. Процессы поднимает и убирает сам.
set -euo pipefail

S=$(mktemp -d)
cleanup() { kill $(jobs -p) 2>/dev/null || true; rm -rf "$S"; }
trap cleanup EXIT
cd "$(git rev-parse --show-toplevel)"
cargo build -p xr-hub -p xr-share

# LibreSSL из macOS не умеет ed25519 rawin, ищем OpenSSL 3
SSL=openssl
for c in /opt/homebrew/bin/openssl /usr/local/bin/openssl; do
  if command -v "$c" >/dev/null && "$c" version 2>/dev/null | grep -q 'OpenSSL 3'; then SSL=$c; break; fi
done

wait_http() { # wait_http <url> <ожидаемое тело>
  for _ in $(seq 1 40); do
    test "$(curl -s --max-time 1 "$1")" = "$2" && return 0
    sleep 0.5
  done
  echo "не дождались '$2' от $1" >&2
  return 1
}

# Хаб: владелец owner/owner-secret, ключ подписи сгенерирован на месте
HASH=$(./target/debug/xr-hub hash-password 'owner-secret' | tail -1)
$SSL genpkey -algorithm ed25519 -out "$S/k.pem" 2>/dev/null
$SSL pkey -in "$S/k.pem" -outform DER | tail -c 32 | base64 > "$S/signing_key"
mkdir -p "$S/data"
cat > "$S/hub.toml" <<EOF
[server]
bind = "127.0.0.1:18099"
data_dir = "$S/data"
[admin]
[[admin.users]]
username = "owner"
password_hash = "$HASH"
[signing]
private_key = "$S/signing_key"
EOF
./target/debug/xr-hub --config "$S/hub.toml" >/dev/null 2>&1 &
wait_http http://127.0.0.1:18099/healthz ok
B=http://127.0.0.1:18099/api/v1
curl -s -H 'content-type: application/json' \
  -d '{"username":"owner","password":"owner-secret"}' "$B/auth/login" > "$S/login.json"
TOK=$(python3 -c "import json;print(json.load(open('$S/login.json'))['token'])")
AUTH="Authorization: Bearer $TOK"
INVITE=$(curl -s -H "$AUTH" -H 'content-type: application/json' -d '{"comment":"XR-188 check"}' "$B/admin/invites" | python3 -c "import json,sys;print(json.load(sys.stdin)['token'])")
REG=$(curl -s -H "$AUTH" -H 'content-type: application/json' -d '{}' "$B/admin/shares/reg-token" | python3 -c "import json,sys;print(json.load(sys.stdin)['token'])")

# Шаг 1: шара с git-контуром, репозиторий вне папки, первый коммит из её файлов
mkdir -p "$S/notes" && echo 'первая заметка' > "$S/notes/a.md"
./target/debug/xr-share -c "$S/agent.toml" install --hub http://127.0.0.1:18099 \
  --token "$REG" --listen 127.0.0.1:18443 --no-service --non-interactive >/dev/null
./target/debug/xr-share -c "$S/agent.toml" share "$S/notes" \
  --writable --git --invite "$INVITE" --name notes --addr 127.0.0.1 >/dev/null
./target/debug/xr-share -c "$S/agent.toml" >/dev/null 2>&1 &
wait_http http://127.0.0.1:18443/healthz ok
curl -s "$B/invite/$INVITE/shares" > "$S/grants.json"
read SID WTOK PUB <<EOF
$(python3 -c "
import json
g = [g for g in json.load(open('$S/grants.json')) if g['name'] == 'notes'][0]
print(g['share_id'], g['token'], g['agent_pubkey'])
")
EOF
test -f "$S/git/$SID/HEAD"
test ! -e "$S/notes/.git"
git --git-dir "$S/git/$SID" log --format=%s -1 | grep -qx 'a.md'

# Шаг 2: клон и пуш штатным git, правка материализуется в папке и в манифесте
git -c http.extraHeader="Authorization: Bearer $WTOK" clone -q "http://127.0.0.1:18443/$SID/git" "$S/clone"
echo 'правка коллеги' >> "$S/clone/a.md"
git -C "$S/clone" add -A
git -C "$S/clone" -c user.name=co -c user.email=co@example.com commit -qm 'правка коллеги'
git -C "$S/clone" -c http.extraHeader="Authorization: Bearer $WTOK" push -q origin main
grep -q 'правка коллеги' "$S/notes/a.md"
curl -s -H "Authorization: Bearer $WTOK" "http://127.0.0.1:18443/$SID/manifest" | grep -q a.md

# Шаг 3: правка в папке агента доезжает авто-коммитом, subject из пути
echo 'заметка с агента' > "$S/notes/b.md"
FETCHED=""
for _ in $(seq 1 15); do
  git -C "$S/clone" -c http.extraHeader="Authorization: Bearer $WTOK" fetch -q origin
  if git -C "$S/clone" log --format=%s -1 origin/main | grep -q 'b.md'; then FETCHED=1; break; fi
  sleep 1
done
test -n "$FETCHED"

# Шаг 4: гейты (read-only токен 403, шара без git 403) и подпись head
CRED=$(sed -n 's/^agent_credential = "\(.*\)"$/\1/p' "$S/agent.toml")
RTOK=$(curl -s -H 'content-type: application/json' \
  -d "{\"credential\":\"$CRED\",\"share_id\":\"$SID\"}" "$B/share/mint" \
  | python3 -c "import json,sys;print(json.load(sys.stdin)['token'])")
test "$(curl -s -o /dev/null -w '%{http_code}' -H "Authorization: Bearer $RTOK" \
  "http://127.0.0.1:18443/$SID/git/info/refs?service=git-upload-pack")" = 403
mkdir -p "$S/plain" && echo x > "$S/plain/x.txt"
./target/debug/xr-share -c "$S/agent.toml" share "$S/plain" --writable --invite "$INVITE" --name plain --addr 127.0.0.1 >/dev/null
code=""
for _ in $(seq 1 20); do
  curl -s "$B/invite/$INVITE/shares" > "$S/grants2.json"
  PSID=$(python3 -c "
import json
g = [g for g in json.load(open('$S/grants2.json')) if g['name'] == 'plain']
print(g[0]['share_id'] if g else '')
")
  if test -n "$PSID"; then break; fi
  sleep 0.5
done
PTOK=$(python3 -c "
import json
g = [g for g in json.load(open('$S/grants2.json')) if g['name'] == 'plain'][0]
print(g['token'])
")
for _ in $(seq 1 20); do
  code=$(curl -s -o /dev/null -w '%{http_code}' -H "Authorization: Bearer $PTOK" \
    "http://127.0.0.1:18443/$PSID/git/info/refs?service=git-upload-pack")
  test "$code" = 403 && break
  sleep 0.5
done
test "$code" = 403
curl -s -H "Authorization: Bearer $WTOK" "http://127.0.0.1:18443/$SID/git/head" > "$S/head.json"
H1=$(python3 -c "import json;print(json.load(open('$S/head.json'))['head'])")
test -n "$H1"
python3 - "$S/head.json" "$PUB" "$SID" <<'PY'
import base64, json, sys
h = json.load(open(sys.argv[1]))
der = bytes.fromhex('302a300506032b6570032100') + base64.b64decode(sys.argv[2])
open(sys.argv[1] + '.der', 'wb').write(der)
open(sys.argv[1] + '.sig', 'wb').write(base64.b64decode(h['sig']))
msg = 'xr-share-git-head\nv1\n%s\n%s\n%s' % (sys.argv[3], h['signed_at'], h['head'])
open(sys.argv[1] + '.msg', 'wb').write(msg.encode())
PY
$SSL pkeyutl -verify -pubin -keyform DER -inkey "$S/head.json.der" \
  -rawin -in "$S/head.json.msg" -sigfile "$S/head.json.sig"

# Шаг 5: первый пуш в шару, пустовавшую при --git, проходит (unborn main)
mkdir -p "$S/empty"
./target/debug/xr-share -c "$S/agent.toml" share "$S/empty" \
  --writable --git --invite "$INVITE" --name emptygit --addr 127.0.0.1 >/dev/null
ESID=""
for _ in $(seq 1 20); do
  curl -s "$B/invite/$INVITE/shares" > "$S/grants3.json"
  ESID=$(python3 -c "
import json
g = [g for g in json.load(open('$S/grants3.json')) if g['name'] == 'emptygit']
print(g[0]['share_id'] if g else '')
")
  test -n "$ESID" && break
  sleep 0.5
done
ETOK=$(python3 -c "
import json
g = [g for g in json.load(open('$S/grants3.json')) if g['name'] == 'emptygit'][0]
print(g['token'])
")
CLONED=""
for _ in $(seq 1 20); do
  rm -rf "$S/eclone"
  if git -c http.extraHeader="Authorization: Bearer $ETOK" clone -q \
      "http://127.0.0.1:18443/$ESID/git" "$S/eclone" 2>/dev/null; then CLONED=1; break; fi
  sleep 1
done
test -n "$CLONED"
echo 'первый файл' > "$S/eclone/first.md"
git -C "$S/eclone" add -A
git -C "$S/eclone" -c user.name=co -c user.email=co@example.com commit -qm first
git -C "$S/eclone" -c http.extraHeader="Authorization: Bearer $ETOK" push -q origin main
test "$(cat "$S/empty/first.md")" = 'первый файл'

# Шаг 6: long-poll head просыпается на авто-коммите
curl -s --max-time 30 -o "$S/lp.json" -H "Authorization: Bearer $WTOK" \
  "http://127.0.0.1:18443/$SID/git/head?since=$H1&wait=20" &
LP=$!
sleep 1
echo 'ещё заметка' > "$S/notes/c.md"
wait $LP
python3 -c "
import json
h = json.load(open('$S/lp.json'))['head']
assert h and h != '$H1', h
"

# Шаг 7: файл в 100 МБ живёт манифест-контуром, в истории его нет
dd if=/dev/urandom of="$S/notes/big.bin" bs=1048576 count=100 2>/dev/null
sleep 6
curl -s -H "Authorization: Bearer $WTOK" "http://127.0.0.1:18443/$SID/manifest" | grep -q big.bin
if git --git-dir "$S/git/$SID" ls-tree -r main --name-only | grep -q big.bin; then
  echo 'big.bin попал в историю' >&2
  exit 1
fi
# Шаг 8: харнесс sync двумя машинами (фаза 2). Клон, встречные правки, а на
# пересечении строк конфликт-копия, доезжающая до обеих сторон.
SYNC="./target/debug/xr-share sync --hub http://127.0.0.1:18099 --invite $INVITE --share notes --once"
$SYNC --name машина-A "$S/A" >/dev/null
$SYNC --name машина-B "$S/B" >/dev/null
test -f "$S/A/a.md" || { echo 'клон на A не приехал' >&2; exit 1; }
test -f "$S/B/a.md" || { echo 'клон на B не приехал' >&2; exit 1; }
# Крупный файл живёт манифест-контуром и в клон соавтора не едет.
if [ -f "$S/A/big.bin" ]; then echo 'big.bin приехал в клон' >&2; exit 1; fi

printf 'строка один\nстрока два\nстрока три\n' > "$S/A/sync.md"
$SYNC --name машина-A "$S/A" >/dev/null
grep -q 'строка два' "$S/notes/sync.md" || { echo 'правка A не доехала до агента' >&2; exit 1; }
$SYNC --name машина-B "$S/B" >/dev/null
grep -q 'строка два' "$S/B/sync.md" || { echo 'правка A не доехала до B' >&2; exit 1; }

# Обе стороны правят одну строку, A успевает первым.
printf 'строка один\nверсия A\nстрока три\n' > "$S/A/sync.md"
$SYNC --name машина-A "$S/A" >/dev/null
printf 'строка один\nверсия B\nстрока три\n' > "$S/B/sync.md"
$SYNC --name машина-B "$S/B" >/dev/null
COPY=$(cd "$S/B" && ls | grep 'конфликт' | head -1)
test -n "$COPY" || { echo 'конфликт-копия не появилась' >&2; exit 1; }
grep -q 'версия B' "$S/B/sync.md" || { echo 'на B пропала своя версия' >&2; exit 1; }
grep -q 'версия A' "$S/B/$COPY" || { echo 'в конфликт-копии не версия A' >&2; exit 1; }
if grep -q '<<<<<<<' "$S/B/sync.md"; then echo 'маркеры merge в папке' >&2; exit 1; fi
# Разбор уехал наверх: у первой стороны то же состояние и та же копия.
$SYNC --name машина-A "$S/A" >/dev/null
grep -q 'версия B' "$S/A/sync.md" || { echo 'разбор не доехал до A' >&2; exit 1; }
grep -q 'версия A' "$S/A/$COPY" || { echo 'конфликт-копия не доехала до A' >&2; exit 1; }

echo 'git-контур шары: ок'
