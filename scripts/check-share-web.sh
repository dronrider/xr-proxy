#!/bin/bash
# Проверка web-контура шары (XR-190, LLD-33 п. 6 фаза 3) живьём: локальный хаб,
# агент из свежей сборки, браузер это curl по ссылке с токеном в query.
# Несовпадение любого ожидания роняет скрипт с ненулевым кодом. Процессы
# поднимает и убирает сам.
set -euo pipefail

S=$(mktemp -d)
cleanup() { kill $(jobs -p) 2>/dev/null || true; rm -rf "$S"; }
trap cleanup EXIT
# Дерево берём от места скрипта, а не от cwd: так прогон из чужого каталога
# не соберёт случайно основной чекаут вместо дерева задачи
cd "$(cd "$(dirname "$0")/.." && git rev-parse --show-toplevel)"
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

# Хаб: владелец owner/owner-secret, ключ подписи сгенерирован на месте.
# Порты 18199/18543, чтобы не сталкиваться с check-git-share.sh.
HASH=$(./target/debug/xr-hub hash-password 'owner-secret' | tail -1)
$SSL genpkey -algorithm ed25519 -out "$S/k.pem" 2>/dev/null
$SSL pkey -in "$S/k.pem" -outform DER | tail -c 32 | base64 > "$S/signing_key"
mkdir -p "$S/data"
cat > "$S/hub.toml" <<EOF
[server]
bind = "127.0.0.1:18199"
data_dir = "$S/data"
[admin]
[[admin.users]]
username = "owner"
password_hash = "$HASH"
[signing]
private_key = "$S/signing_key"
EOF
./target/debug/xr-hub --config "$S/hub.toml" >/dev/null 2>&1 &
wait_http http://127.0.0.1:18199/healthz ok
B=http://127.0.0.1:18199/api/v1
curl -s -H 'content-type: application/json' \
  -d '{"username":"owner","password":"owner-secret"}' "$B/auth/login" > "$S/login.json"
TOK=$(python3 -c "import json;print(json.load(open('$S/login.json'))['token'])")
AUTH="Authorization: Bearer $TOK"
INVITE=$(curl -s -H "$AUTH" -H 'content-type: application/json' -d '{"comment":"XR-190 check"}' "$B/admin/invites" | python3 -c "import json,sys;print(json.load(sys.stdin)['token'])")
REG=$(curl -s -H "$AUTH" -H 'content-type: application/json' -d '{}' "$B/admin/shares/reg-token" | python3 -c "import json,sys;print(json.load(sys.stdin)['token'])")

# Шара с git-контуром: два файла, один в подпапке, чтобы путь фильтра истории
# шёл percent-кодированным
mkdir -p "$S/notes/sub"
echo 'первая заметка' > "$S/notes/a.md"
echo 'заметка в подпапке' > "$S/notes/sub/notes.md"
./target/debug/xr-share -c "$S/agent.toml" install --hub http://127.0.0.1:18199 \
  --token "$REG" --listen 127.0.0.1:18543 --no-service --non-interactive >/dev/null
./target/debug/xr-share -c "$S/agent.toml" share "$S/notes" \
  --writable --git --invite "$INVITE" --name notes --addr 127.0.0.1 >/dev/null
./target/debug/xr-share -c "$S/agent.toml" >/dev/null 2>&1 &
wait_http http://127.0.0.1:18543/healthz ok
curl -s "$B/invite/$INVITE/shares" > "$S/grants.json"
read SID WTOK <<EOF
$(python3 -c "
import json
g = [g for g in json.load(open('$S/grants.json')) if g['name'] == 'notes'][0]
print(g['share_id'], g['token'])
")
EOF

# Коллега: клон и пуш, в истории два коммита
git -c http.extraHeader="Authorization: Bearer $WTOK" clone -q "http://127.0.0.1:18543/$SID/git" "$S/clone"
echo 'правка коллеги' >> "$S/clone/a.md"
git -C "$S/clone" add -A
git -C "$S/clone" -c user.name=co -c user.email=co@example.com commit -qm 'правка коллеги'
git -C "$S/clone" -c http.extraHeader="Authorization: Bearer $WTOK" push -q origin main

# Read-токен через хабовый минт по креду агента
CRED=$(sed -n 's/^agent_credential = "\(.*\)"$/\1/p' "$S/agent.toml")
RTOK=$(curl -s -H 'content-type: application/json' \
  -d "{\"credential\":\"$CRED\",\"share_id\":\"$SID\"}" "$B/share/mint" \
  | python3 -c "import json,sys;print(json.load(sys.stdin)['token'])")

# Шаг 1: страница отдаётся по токену в query, без токена отказ, наружу ничего
test "$(curl -s -o "$S/page.html" -w '%{http_code}' "http://127.0.0.1:18543/$SID/web?token=$RTOK")" = 200
grep -q '<title>Шара</title>' "$S/page.html"
if grep -qE 'https?://' "$S/page.html"; then echo 'страница тянет внешние адреса' >&2; exit 1; fi
test "$(curl -s -o /dev/null -w '%{http_code}' "http://127.0.0.1:18543/$SID/web")" = 401

# Шаг 2: read-токену история закрыта
test "$(curl -s -o /dev/null -w '%{http_code}' -H "Authorization: Bearer $RTOK" \
  "http://127.0.0.1:18543/$SID/git/log")" = 403

# Шаг 3: weblink печатает ссылку с write-токеном и предупреждение. Хаб минтит
# токен заново на каждый запрос списка шар, поэтому сверяется адрес, скоуп
# токена в блобе и живое открытие ссылки, а не совпадение строки с WTOK
./target/debug/xr-share weblink --hub http://127.0.0.1:18199 \
  --invite "$INVITE" --share notes > "$S/weblink.txt"
LINK=$(sed -n 1p "$S/weblink.txt")
test "${LINK%%\?*}" = "http://127.0.0.1:18543/$SID/web"
BLOB=${LINK##*token=}
python3 - "$BLOB" <<'PY'
import base64, json, sys
raw = sys.argv[1] + '=' * (-len(sys.argv[1]) % 4)
t = json.loads(base64.urlsafe_b64decode(raw))
assert 'share:write' in t['scope'], t
PY
sed -n 3p "$S/weblink.txt" | grep -q 'история браузера'
test "$(curl -s -o /dev/null -w '%{http_code}' "$LINK")" = 200

# Шаг 4: история по write-токену, путь в подпапке percent-кодированием
curl -s -H "Authorization: Bearer $WTOK" "http://127.0.0.1:18543/$SID/git/log" > "$S/log.json"
python3 - "$S/log.json" <<'PY'
import json, sys
rows = json.load(open(sys.argv[1]))
assert len(rows) >= 2, rows
assert rows[0]['subject'] == 'правка коллеги', rows[0]
for k in ('sha', 'author', 'date', 'subject'):
    assert rows[0][k], (k, rows[0])
PY
test "$(curl -s -H "Authorization: Bearer $WTOK" \
  "http://127.0.0.1:18543/$SID/git/log?path=sub%2Fnotes.md" | python3 -c "import json,sys;print(len(json.load(sys.stdin)))")" = 1

# Шаг 5: дифф между соседними коммитами
FROM=$(python3 -c "import json;print(json.load(open('$S/log.json'))[1]['sha'])")
TO=$(python3 -c "import json;print(json.load(open('$S/log.json'))[0]['sha'])")
curl -s -H "Authorization: Bearer $WTOK" \
  "http://127.0.0.1:18543/$SID/git/diff?from=$FROM&to=$TO" > "$S/diff.txt"
grep -q 'diff --git' "$S/diff.txt"
grep -q '^+правка коллеги' "$S/diff.txt"

# Шаг 6: правка PUT'ом с If-Match коммитится и доезжает до клона
curl -s -H "Authorization: Bearer $WTOK" "http://127.0.0.1:18543/$SID/manifest" > "$S/manifest.json"
SHA=$(python3 -c "import json;print([e['sha256'] for e in json.load(open('$S/manifest.json'))['entries'] if e['path'] == 'a.md'][0])")
test -n "$SHA"
test "$(curl -s -o /dev/null -w '%{http_code}' -X PUT -H "Authorization: Bearer $WTOK" \
  -H "If-Match: $SHA" --data-binary $'первая заметка\nправка коллеги\nправка из браузера\n' \
  "http://127.0.0.1:18543/$SID/file/a.md")" = 204
WEBEDIT=""
for _ in $(seq 1 15); do
  if ! test "$(git --git-dir "$S/git/$SID" log --format=%s -1)" = 'правка коллеги'; then WEBEDIT=1; break; fi
  sleep 1
done
test -n "$WEBEDIT"
git --git-dir "$S/git/$SID" log --format=%s -1 | grep -q 'a.md'
FETCHED=""
for _ in $(seq 1 15); do
  git -C "$S/clone" -c http.extraHeader="Authorization: Bearer $WTOK" fetch -q origin
  if git -C "$S/clone" log --format=%s -1 origin/main | grep -q 'a.md'; then FETCHED=1; break; fi
  sleep 1
done
test -n "$FETCHED"
grep -q 'правка из браузера' "$S/notes/a.md"

# Шаг 7: протухший If-Match это 412, содержимое не тронуто
test "$(curl -s -o /dev/null -w '%{http_code}' -X PUT -H "Authorization: Bearer $WTOK" \
  -H "If-Match: $SHA" --data-binary 'конфликт' \
  "http://127.0.0.1:18543/$SID/file/a.md")" = 412
! grep -q 'конфликт' "$S/notes/a.md"
echo 'web-контур шары: ок'
