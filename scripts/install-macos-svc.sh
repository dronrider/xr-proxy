#!/bin/sh
# Установка сервиса macOS одной командой (XR-309): сборка xr-macos и вызов
# его же install с профилем. Профиль это тот же JSON, что получает
# приложение Android по QR или с хаба; ключ обфускации внутри, поэтому файл
# ляжет с правами 0600 под root.
#
# Пример:
#   sudo scripts/install-macos-svc.sh profile.json
#   sudo scripts/install-macos-svc.sh profile.json --config configs/macos.toml
#
# Дальнейшая жизнь сервиса: статус (sudo xr-macos status), остановка с
# откатом сети (sudo xr-macos stop), восстановление после аварии
# (sudo xr-macos recover), удаление (sudo xr-macos uninstall). Инструкция
# целиком в docs/MAC-UTUN.md.
set -eu
cd "$(dirname "$0")/.."

if [ "$(id -u)" -ne 0 ]; then
  echo "нужны права администратора: запустите через sudo" >&2
  exit 1
fi

if [ "$#" -lt 1 ]; then
  echo "использование: $0 <profile.json> [--config <macos.toml>]" >&2
  exit 1
fi
PROFILE=$1
shift

cargo build -p xr-macos
./target/debug/xr-macos install --profile "$PROFILE" "$@"
