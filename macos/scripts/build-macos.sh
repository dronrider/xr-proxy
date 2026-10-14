#!/bin/sh
# Сборка NE-приложения macOS: staticlib ядра, генерация проекта, xcodebuild.
# Подпись и нотаризация: отдельный шаг --sign, требует учётки Apple Developer.
#
# Требования: полный Xcode (не Command Line Tools), xcodegen, Rust (cargo).
# Запуск: scripts/build-macos.sh [--release] [--sign] [--notarize]
# Без флагов собирает debug без подписи.

set -eu

ROOT=$(cd "$(dirname "$0")/../.." && pwd)
MACOS_DIR="$ROOT/macos/XrMac"
TARGET_DIR="$ROOT/target"
CONFIG="${CONFIG:-debug}"
SIGN=0
NOTARIZE=0

for arg in "$@"; do
    case "$arg" in
        --release) CONFIG=release ;;
        --sign) SIGN=1 ;;
        --notarize) SIGN=1; NOTARIZE=1 ;;
        *) echo "неизвестный флаг: $arg" >&2; exit 2 ;;
    esac
done

echo "==> Rust staticlib ($CONFIG)"
if [ "$CONFIG" = "release" ]; then
    cargo build --manifest-path "$ROOT/Cargo.toml" -p xr-ios-ffi --release
else
    cargo build --manifest-path "$ROOT/Cargo.toml" -p xr-ios-ffi
fi

echo "==> Swift-юниты чистой логики"
(cd "$ROOT/macos/XrCore" && swift run xrcore-tests)

if ! command -v xcodegen >/dev/null 2>&1; then
    echo "xcodegen не найден. Установи: brew install xcodegen" >&2
    exit 1
fi

echo "==> Генерация Xcode-проекта"
(cd "$MACOS_DIR" && xcodegen generate)

echo "==> xcodebuild ($CONFIG)"
XCODE_ARGS="-project $MACOS_DIR/XrMac.xcodeproj -scheme XrMac -configuration $(echo "$CONFIG" | sed 's/^./\U&/') -destination 'platform=macOS' build"

# shellcheck disable=SC2086
xcodebuild $XCODE_ARGS

APP_PATH=$(find "$MACOS_DIR/build" -name "XR Proxy.app" -maxdepth 5 | head -1)
if [ -z "$APP_PATH" ]; then
    # Xcode кладёт в DerivedData; ищем оттуда.
    APP_PATH=$(find ~/Library/Developer/Xcode/DerivedData -name "XR Proxy.app" -path "*XrMac*" | head -1)
fi
echo "==> Сборка: $APP_PATH"

if [ "$SIGN" -eq 0 ]; then
    echo "==> Подпись пропущена (без --sign). Приложение локально, для чужих машин нужна подпись."
    exit 0
fi

if [ -z "${DEVELOPER_ID:-}" ]; then
    echo "DEVELOPER_ID не задан (пример: 'Developer ID Application: Name (TEAMID)')." >&2
    echo "Нотаризация требует APPLE_ID, APPLE_TEAM_ID и APPLE_APP_PASSWORD." >&2
    exit 1
fi

echo "==> Подпись Developer ID: $DEVELOPER_ID"
codesign --force --options runtime --timestamp \
    --sign "$DEVELOPER_ID" \
    --entitlements "$MACOS_DIR/XrMacTunnel/XrMacTunnel.entitlements" \
    "$APP_PATH/Contents/PlugIns/XR Tunnel.appex"
codesign --force --options runtime --timestamp \
    --sign "$DEVELOPER_ID" \
    --entitlements "$MACOS_DIR/XrMac/XrMac.entitlements" \
    "$APP_PATH"

codesign --verify --deep --strict "$APP_PATH"
echo "==> Подпись проверена"

if [ "$NOTARIZE" -eq 0 ]; then
    echo "==> Нотаризация пропущена (без --notarize)."
    exit 0
fi

if [ -z "${APPLE_ID:-}" ] || [ -z "${APPLE_TEAM_ID:-}" ] || [ -z "${APPLE_APP_PASSWORD:-}" ]; then
    echo "Для нотаризации нужны APPLE_ID, APPLE_TEAM_ID и APPLE_APP_PASSWORD (app-specific password)." >&2
    exit 1
fi

echo "==> Упаковка для нотаризации"
NOTARIZE_ZIP=$(mktemp -d)/app.zip
ditto -c -k --keepParent "$APP_PATH" "$NOTARIZE_ZIP"

echo "==> Отправка в нотаризацию (ждать, бывает несколько минут)"
xcrun notarytool submit "$NOTARIZE_ZIP" \
    --apple-id "$APPLE_ID" \
    --team-id "$APPLE_TEAM_ID" \
    --password "$APPLE_APP_PASSWORD" \
    --wait

echo "==> Прикрепление staple"
xcrun stapler staple "$APP_PATH"
spctl --assess --type execute --verbose "$APP_PATH"
echo "==> Нотаризация завершена: $APP_PATH"
