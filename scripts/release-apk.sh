#!/usr/bin/env bash
# Релиз Android APK одной командой (XR-109): сборка, подпись манифеста
# офлайн-ключом, заливка на оба хаба и проверка, что оба отдают новый latest.
#
#   ./scripts/release-apk.sh --version 1.0.0 --version-code 100
#   ./scripts/release-apk.sh --bump --notes "Проводник по группам"
#   ./scripts/release-apk.sh --only-upload --version 1.0.0 --version-code 100
#
# Скрипт не решает, что релизить: номер версии, строка доски и проверка на
# устройстве остаются за владельцем. Всё, что знает адреса и доступы, лежит в
# гитигнорнутом local-docs/release.env (путь переопределяется RELEASE_ENV);
# запуск без него печатает готовый шаблон и останавливается.
#
# Флаги:
#   --version X.Y.Z      имя версии (versionName и имя файла APK на хабе)
#   --version-code N     versionCode, строго больше живого на хабе
#   --bump               взять код живого релиза, прибавить единицу и собрать
#                        имя версии по нашему соглашению 0.<код>.0
#   --notes "текст"      строка релиза в баннере приложения
#   --skip-build         не собирать, подписать уже собранный APK
#   --only-upload        не собирать и не подписывать, залить готовый каталог
#                        подписи (перезапуск после обрыва заливки)
#   --primary-only       не трогать второй хаб
#   --dry-run            напечатать шаги, ничего не делая
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
RELEASE_ENV="${RELEASE_ENV:-$ROOT/local-docs/release.env}"

env_template() {
  cat <<'TEMPLATE'
# Адреса и доступы релиза APK. Файл гитигнорен (local-docs/), в репозиторий
# не едет. Значения подставить свои.

# Офлайн-ключ подписи манифеста и его публичная половина, вшитая в приложение.
RELEASE_KEY=/path/to/release.key
RELEASE_PUBKEY=<base64 публичной половины>

# Публичный адрес первичного хаба: уходит в манифест как база ссылки на APK
# и по нему же проверяется latest после заливки.
HUB_URL=https://hub.example.com

# Первичный хаб.
PRIMARY_HOST=203.0.113.10
PRIMARY_PORT=22
PRIMARY_USER=root
PRIMARY_SSH_KEY=$HOME/.ssh/id_ed25519
PRIMARY_RELEASES_DIR=/var/lib/xr-hub/releases

# Второй хаб (резерв с тем же именем сайта). Его latest проверяется изнутри
# самой машины, поэтому нужен Host для запроса на 127.0.0.1.
SECONDARY_HOST=198.51.100.20
SECONDARY_PORT=22
SECONDARY_USER=root
SECONDARY_SSH_KEY=$HOME/.ssh/id_ed25519
SECONDARY_RELEASES_DIR=/var/lib/xr-hub/releases
SECONDARY_HOST_HEADER=hub.example.com
SECONDARY_LATEST_URL=https://127.0.0.1/api/v1/app/latest
TEMPLATE
}

for a in "$@"; do
  case "$a" in -h|--help) sed -n '2,25p' "$0"; exit 0 ;; esac
done

if [ ! -f "$RELEASE_ENV" ]; then
  echo "error: нет $RELEASE_ENV. Завести его по шаблону:" >&2
  env_template >&2
  exit 1
fi
# shellcheck disable=SC1090
. "$RELEASE_ENV"

VERSION=""
VERSION_CODE=""
NOTES="${RELEASE_NOTES:-}"
BUMP=0
DO_BUILD=1
DO_SIGN=1
PRIMARY_ONLY=0
DRY_RUN=0

while [ $# -gt 0 ]; do
  case "$1" in
    --version)       VERSION="$2"; shift 2 ;;
    --version-code)  VERSION_CODE="$2"; shift 2 ;;
    --notes)         NOTES="$2"; shift 2 ;;
    --bump)          BUMP=1; shift ;;
    --skip-build)    DO_BUILD=0; shift ;;
    --only-upload)   DO_BUILD=0; DO_SIGN=0; shift ;;
    --primary-only)  PRIMARY_ONLY=1; shift ;;
    --dry-run)       DRY_RUN=1; shift ;;
    -h|--help)       sed -n '2,25p' "$0"; exit 0 ;;
    *) echo "error: неизвестный аргумент $1 (см. --help)" >&2; exit 1 ;;
  esac
done

# Пути и инструменты, у каждого разумный дефолт; тесты подменяют их через env.
APK_PATH="${APK_PATH:-$ROOT/xr-android/app/build/outputs/apk/release/app-release.apk}"
STAGING_ROOT="${STAGING_ROOT:-$ROOT/release-staging}"
BUILD_LOG="${BUILD_LOG:-$STAGING_ROOT/build.log}"
ANDROID_BUILD="${ANDROID_BUILD:-$ROOT/xr-android/build.sh}"
XR_HUB_BIN="${XR_HUB_BIN:-}"
PRIMARY_PORT="${PRIMARY_PORT:-22}"
PRIMARY_USER="${PRIMARY_USER:-root}"
PRIMARY_RELEASES_DIR="${PRIMARY_RELEASES_DIR:-/var/lib/xr-hub/releases}"
SECONDARY_PORT="${SECONDARY_PORT:-22}"
SECONDARY_USER="${SECONDARY_USER:-root}"
SECONDARY_RELEASES_DIR="${SECONDARY_RELEASES_DIR:-/var/lib/xr-hub/releases}"
SECONDARY_LATEST_URL="${SECONDARY_LATEST_URL:-https://127.0.0.1/api/v1/app/latest}"

need_var() {
  local name="$1"
  if [ -z "${!name:-}" ]; then
    echo "error: в $RELEASE_ENV не задан $name" >&2
    exit 1
  fi
}
for v in RELEASE_KEY RELEASE_PUBKEY HUB_URL PRIMARY_HOST; do need_var "$v"; done
if [ "$PRIMARY_ONLY" = "0" ]; then
  for v in SECONDARY_HOST SECONDARY_HOST_HEADER; do need_var "$v"; done
fi
[ -f "$RELEASE_KEY" ] || { echo "error: ключа подписи нет: $RELEASE_KEY" >&2; exit 1; }

say()  { printf '%s\n' "$*"; }
step() { printf '\n== %s ==\n' "$*"; }

# Всё, что меняет мир (сборка, ssh, заливка), идёт через run: сухой прогон
# печатает команду вместо запуска.
run() {
  if [ "$DRY_RUN" = "1" ]; then
    printf '  [dry-run] %s\n' "$*"
    return 0
  fi
  "$@"
}

sums() { if command -v sha256sum >/dev/null 2>&1; then sha256sum "$@"; else shasum -a 256 "$@"; fi; }

# CLI подписи: готовый бинарь из env, иначе собранный отладочный (релизный
# требует собранного admin-ui, а нам нужен только sign-release/verify-release).
if [ -z "$XR_HUB_BIN" ]; then
  step "сборка xr-hub CLI (нужен для подписи и проверки)"
  cargo build -q -p xr-hub --bin xr-hub
  XR_HUB_BIN="$ROOT/target/debug/xr-hub"
fi
xr_hub() { "$XR_HUB_BIN" "$@"; }

PRIMARY_SSH=(ssh -p "$PRIMARY_PORT")
if [ -n "${PRIMARY_SSH_KEY:-}" ]; then PRIMARY_SSH+=(-i "$PRIMARY_SSH_KEY"); fi
PRIMARY_SSH+=("$PRIMARY_USER@$PRIMARY_HOST")
SECONDARY_SSH=(ssh -p "$SECONDARY_PORT")
if [ -n "${SECONDARY_SSH_KEY:-}" ]; then SECONDARY_SSH+=(-i "$SECONDARY_SSH_KEY"); fi
SECONDARY_SSH+=("$SECONDARY_USER@${SECONDARY_HOST:-}")

primary_ssh() { run "${PRIMARY_SSH[@]}" "$@"; }
secondary_ssh() { run "${SECONDARY_SSH[@]}" "$@"; }

# Ответ /api/v1/app/latest с первичного хаба; пустая строка, если релиза там
# ещё нет (свежий хаб отвечает 404).
fetch_primary_latest() {
  curl -fsS --max-time 30 "$HUB_URL/api/v1/app/latest" 2>/dev/null || true
}

# versionCode из ответа: манифест внутри JSON-эскейпнут, поэтому не парсим
# его как JSON, а вынимаем число по имени поля.
manifest_code() {
  printf '%s' "$1" | grep -o 'version_code[^0-9]*[0-9][0-9]*' | head -1 | grep -o '[0-9][0-9]*$'
}

TS="$(date +%Y%m%d-%H%M%S)"

step "префлайт"
live="$(fetch_primary_latest)"
live_code=""
if [ -n "$live" ]; then
  live_code="$(manifest_code "$live" || true)"
  say "живой релиз на первичном хабе: code ${live_code:-?}"
  # Единственная доступная проверка, что ключ подписи тот самый: им должна
  # сходиться подпись уже отданного манифеста. Ошибиться ключом значит
  # выкатить обновление, которое установленные приложения отвергнут.
  printf '%s' "$live" | xr_hub verify-release --signed - --key "$RELEASE_KEY" \
    || { echo "error: живой манифест не проверяется ключом $RELEASE_KEY, это не тот ключ" >&2; exit 1; }
else
  say "на первичном хабе релиза ещё нет, проверку ключа по живому манифесту пропускаем"
fi

if [ "$BUMP" = "1" ]; then
  [ -n "$live_code" ] || { echo "error: --bump некуда считать, живого релиза нет" >&2; exit 1; }
  VERSION_CODE="$((live_code + 1))"
  VERSION="0.$VERSION_CODE.0"
  say "--bump: версия $VERSION (code $VERSION_CODE)"
fi
[ -n "$VERSION" ] && [ -n "$VERSION_CODE" ] \
  || { echo "error: нужны --version и --version-code (или --bump)" >&2; exit 1; }
case "$VERSION_CODE" in *[!0-9]*) echo "error: --version-code это число" >&2; exit 1 ;; esac
# Новый релиз обязан расти по коду, иначе приложение его не предложит. При
# --only-upload догоняется уже выпущенная версия (обрыв заливки на втором
# хабе), там равенство это норма.
if [ -n "$live_code" ] && [ "$VERSION_CODE" -le "$live_code" ]; then
  if [ "$DO_SIGN" = "1" ] || [ "$VERSION_CODE" -lt "$live_code" ]; then
    echo "error: code $VERSION_CODE не больше живого $live_code, приложение такое обновление не предложит" >&2
    exit 1
  fi
  say "докатываем уже выпущенный code $VERSION_CODE"
fi

STAGING="$STAGING_ROOT/$VERSION"
say "каталог подписи: $STAGING"

if [ "$DO_BUILD" = "1" ]; then
  step "сборка APK $VERSION (code $VERSION_CODE)"
  mkdir -p "$STAGING_ROOT"
  say "лог сборки: $BUILD_LOG"
  if [ "$DRY_RUN" = "1" ]; then
    printf '  [dry-run] %s --release\n' "$ANDROID_BUILD"
  elif ! ORG_GRADLE_PROJECT_xrVersionName="$VERSION" \
        ORG_GRADLE_PROJECT_xrVersionCode="$VERSION_CODE" \
        ORG_GRADLE_PROJECT_xrReleasePublicKey="$RELEASE_PUBKEY" \
        "$ANDROID_BUILD" --release >"$BUILD_LOG" 2>&1; then
    echo "error: сборка упала, хвост $BUILD_LOG:" >&2
    tail -n 30 "$BUILD_LOG" >&2
    exit 1
  fi
  say "APK собран"
else
  say "сборка пропущена"
fi

if [ "$DO_SIGN" = "1" ]; then
  step "подпись манифеста"
  [ "$DRY_RUN" = "1" ] || [ -f "$APK_PATH" ] \
    || { echo "error: нет APK $APK_PATH" >&2; exit 1; }
  if [ "$DRY_RUN" = "1" ]; then
    printf '  [dry-run] xr-hub sign-release --apk %s --version %s --out %s\n' "$APK_PATH" "$VERSION" "$STAGING"
  else
    mkdir -p "$STAGING"
    xr_hub sign-release \
      --apk "$APK_PATH" \
      --version "$VERSION" \
      --version-code "$VERSION_CODE" \
      --key "$RELEASE_KEY" \
      --base-url "$HUB_URL" \
      --notes "$NOTES" \
      --out "$STAGING" >/dev/null
    # Подпись сверяем той же публичной половиной, что вшита в приложение:
    # собрали одним ключом, а подписали другим значит OTA не поедет.
    xr_hub verify-release \
      --manifest "$STAGING/manifest.json" --sig "$STAGING/manifest.sig" \
      --pubkey "$RELEASE_PUBKEY" \
      --expect-version "$VERSION" --expect-version-code "$VERSION_CODE"
  fi
else
  say "подпись пропущена, заливаем готовый $STAGING"
fi

APK_SHA=""
if [ "$DRY_RUN" = "0" ]; then
  for f in manifest.json manifest.sig "$VERSION.apk"; do
    [ -f "$STAGING/$f" ] || { echo "error: в $STAGING нет $f" >&2; exit 1; }
  done
  APK_SHA="$(sums "$STAGING/$VERSION.apk" | awk '{print $1}')"
fi

# Заливка на хаб: файлы едут в свой staging-каталог и лишь оттуда переезжают
# в releases/ переименованием. Так хаб ни секунды не отдаёт недокачанный APK,
# а старый манифест перед перезаписью ложится рядом бэкапом.
upload() {
  local role="$1" dir="$2" ssh_fn="$3" rsh="$4" target="$5"
  local remote_stage="$dir/.staging-$VERSION"

  step "$role: заливка $VERSION"
  $ssh_fn "mkdir -p '$remote_stage'"
  run rsync -e "$rsh" --partial --inplace \
    "$STAGING/manifest.json" "$STAGING/manifest.sig" "$STAGING/$VERSION.apk" \
    "$target:$remote_stage/"
  # Бэкап и переезд одной командой: APK раньше манифеста, чтобы манифест
  # никогда не ссылался на ещё не доехавший файл.
  $ssh_fn "set -e
    cd '$dir'
    for f in manifest.json manifest.sig; do
      if [ -f \"\$f\" ]; then cp -p \"\$f\" \"\$f.bak.$TS\"; fi
    done
    mv '$remote_stage/$VERSION.apk' '$dir/$VERSION.apk'
    mv '$remote_stage/manifest.json' '$dir/manifest.json'
    mv '$remote_stage/manifest.sig' '$dir/manifest.sig'
    rmdir '$remote_stage'"
}

PRIMARY_RSH="ssh -p $PRIMARY_PORT${PRIMARY_SSH_KEY:+ -i $PRIMARY_SSH_KEY}"
SECONDARY_RSH="ssh -p $SECONDARY_PORT${SECONDARY_SSH_KEY:+ -i $SECONDARY_SSH_KEY}"

upload "первичный хаб" "$PRIMARY_RELEASES_DIR" primary_ssh "$PRIMARY_RSH" \
  "$PRIMARY_USER@$PRIMARY_HOST"
[ "$PRIMARY_ONLY" = "1" ] || upload "второй хаб" "$SECONDARY_RELEASES_DIR" secondary_ssh \
  "$SECONDARY_RSH" "$SECONDARY_USER@${SECONDARY_HOST:-}"

# Проверка после заливки: подпись сходится вшитым ключом, версия та самая.
# HTTP 200 сам по себе ничего не доказывает, хаб отдаёт файлы с диска.
verify_served() {
  local role="$1" body="$2"
  local -a expect=(--expect-version "$VERSION" --expect-version-code "$VERSION_CODE")
  if [ -n "$APK_SHA" ]; then expect+=(--expect-sha256 "$APK_SHA"); fi
  [ -n "$body" ] || { echo "error: $role не отдал latest" >&2; exit 1; }
  printf '%s' "$body" | xr_hub verify-release --signed - \
    --pubkey "$RELEASE_PUBKEY" "${expect[@]}" \
    || { echo "error: $role отдаёт не тот релиз" >&2; exit 1; }
}

PRIMARY_LATEST="(сухой прогон)"
SECONDARY_LATEST="(сухой прогон)"
if [ "$DRY_RUN" = "0" ]; then
  step "проверка latest на первичном хабе"
  PRIMARY_LATEST="$(verify_served "первичный хаб" "$(fetch_primary_latest)")"
  say "$PRIMARY_LATEST"
  # HEAD, а не кусок тела: диапазоны хаб не поддерживает и на -r 0-0 отдаёт
  # весь APK, полсотни мегабайт ради кода ответа.
  dl_code="$(curl -fsS -I -o /dev/null -w '%{http_code}' --max-time 60 \
    "$HUB_URL/api/v1/app/download/$VERSION" || true)"
  case "$dl_code" in
    200) say "download отдаётся ($dl_code)" ;;
    *) echo "error: download отвечает $dl_code" >&2; exit 1 ;;
  esac

  if [ "$PRIMARY_ONLY" = "0" ]; then
    step "проверка latest на втором хабе"
    # Второй хаб живёт под тем же именем сайта, публично на него не попасть,
    # поэтому спрашиваем его изнутри машины и подставляем Host.
    body="$(secondary_ssh "curl -sk --max-time 30 -H 'Host: $SECONDARY_HOST_HEADER' '$SECONDARY_LATEST_URL'")"
    SECONDARY_LATEST="$(verify_served "второй хаб" "$body")"
    say "$SECONDARY_LATEST"
  else
    SECONDARY_LATEST="(пропущен)"
  fi
fi

step "итог"
say "версия:     $VERSION (code $VERSION_CODE)"
say "sha256 APK: ${APK_SHA:-(сухой прогон)}"
say "каталог:    $STAGING"
say "первичный:  $PRIMARY_LATEST"
say "второй:     $SECONDARY_LATEST"
say "бэкапы старых манифестов: manifest.{json,sig}.bak.$TS"
say ""
say "Осталось: строка в доске и проверка обновления на устройстве."
