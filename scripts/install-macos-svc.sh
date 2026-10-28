#!/bin/sh
# Установка сервиса macOS одной командой (XR-309): сборка xr-macos и вызов
# его же install с профилем. Сборка идёт от вызывающего пользователя:
# rustup-шим под sudo ищет тулчейн в HOME=/var/root и падает, а сборка от
# root оставила бы в target/ файлы своего владельца. Права администратора
# скрипт поднимает сам, и только на шаге install. Профиль это тот же JSON,
# что получает приложение Android по QR или с хаба; ключ обфускации внутри,
# поэтому файл ляжет с правами 0600 под root.
#
# Пример:
#   scripts/install-macos-svc.sh profile.json
#   scripts/install-macos-svc.sh profile.json --config configs/macos.toml
#
# Дальнейшая жизнь сервиса: статус (sudo xr-macos status), остановка с
# откатом сети (sudo xr-macos stop), восстановление после аварии
# (sudo xr-macos recover), удаление (sudo xr-macos uninstall). Инструкция
# целиком в docs/MAC-UTUN.md.
set -eu
cd "$(dirname "$0")/.."

if [ "$(id -u)" -eq 0 ]; then
  echo "sudo не нужен: сборка идёт от вашего пользователя, права скрипт поднимет сам на шаге установки" >&2
  exit 1
fi

if [ "$#" -lt 1 ]; then
  echo "использование: $0 <profile.json> [--config <macos.toml>]" >&2
  exit 1
fi
PROFILE=$1
shift

cargo build -p xr-macos
BIN="$(pwd)/target/debug/xr-macos"
exec sudo "$BIN" install --profile "$PROFILE" "$@"
