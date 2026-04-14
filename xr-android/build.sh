#!/bin/bash
# Build script for XR Proxy Android app.
# Run this outside of Claude Code session for maximum available RAM.
#
# Prerequisites:
#   - Rust + cargo-ndk installed
#   - Android SDK + NDK at ~/android-tools/sdk
#   - JDK 17 at ~/android-tools/jdk17
#
# Usage:
#   cd xr-android && ./build.sh              # debug APK
#   cd xr-android && ./build.sh --release    # release APK (debug-signed)
#   cd xr-android && ./build.sh -r           # то же короче
#
# Rust native libraries (libxr_proxy.so) всегда собираются в release-профиле
# — дебажный Rust-слой на мобильном замедляет даже direct-трафик в 5-10x
# из-за smoltcp и per-packet обработки. Отдельный режим для Rust-debug мы
# не делаем: если нужно отладить нативу, её проще прогнать под cargo test
# на хосте.
#
# APK paths:
#   debug:   app/build/outputs/apk/debug/app-debug.apk
#   release: app/build/outputs/apk/release/app-release.apk

set -e

# ── Parse args ──────────────────────────────────────────────────────
RELEASE=0
for arg in "$@"; do
    case "$arg" in
        -r|--release)
            RELEASE=1
            ;;
        -h|--help)
            sed -n '3,20p' "$0"
            exit 0
            ;;
        *)
            echo "Unknown argument: $arg" >&2
            echo "Use --help for usage." >&2
            exit 1
            ;;
    esac
done

if [ "$RELEASE" = "1" ]; then
    GRADLE_TASK="assembleRelease"
    APK_SUBDIR="release"
    APK_NAME="app-release.apk"
    BUILD_LABEL="RELEASE"
else
    GRADLE_TASK="assembleDebug"
    APK_SUBDIR="debug"
    APK_NAME="app-debug.apk"
    BUILD_LABEL="DEBUG"
fi

# ── Environment ────────────────────────────────────────────────────
export JAVA_HOME="${JAVA_HOME:-$HOME/android-tools/jdk17}"
export ANDROID_HOME="${ANDROID_HOME:-$HOME/android-tools/sdk}"
export ANDROID_NDK_HOME="${ANDROID_NDK_HOME:-$ANDROID_HOME/ndk/27.0.12077973}"
export PATH="$JAVA_HOME/bin:$HOME/.cargo/bin:$PATH"

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

echo "=== Build mode: $BUILD_LABEL ==="
echo ""

echo "=== Cleaning workspace crates (keeps dep cache) ==="
cd "$PROJECT_ROOT"
# Чистим только наши крейты, чтобы гарантированно пересобрать native .so
# даже после rsync/checkout с чужими mtime. Тяжёлые deps (tokio, serde, ...)
# остаются кэшированными, поэтому пересборка занимает ~30-40 секунд.
#
# ВАЖНО: нужны --target *-linux-android И --release. Без --target
# cargo clean трогает только host (target/debug, target/release), не
# трогая target/<triple>/release/, где лежат наши JNI .so. Без
# --release чистится только debug-профиль того же target'а.
cargo clean -p xr-proto -p xr-core -p xr-android-jni \
    --target aarch64-linux-android --release
cargo clean -p xr-proto -p xr-core -p xr-android-jni \
    --target x86_64-linux-android --release

echo ""
echo "=== Building Rust native libraries (release) ==="
cargo ndk \
    -t aarch64-linux-android \
    -t x86_64-linux-android \
    -o "$SCRIPT_DIR/app/src/main/jniLibs" \
    build -p xr-android-jni --release

echo ""
echo "=== Building Android APK ($BUILD_LABEL) ==="
cd "$SCRIPT_DIR"

# Generate local.properties with correct SDK path for this machine.
echo "sdk.dir=$ANDROID_HOME" > local.properties

sh ./gradlew "$GRADLE_TASK" --no-daemon

echo ""
echo "=== Done ==="
APK="app/build/outputs/apk/$APK_SUBDIR/$APK_NAME"
if [ -f "$APK" ]; then
    echo "APK: $(realpath $APK)"
    echo "Size: $(du -h $APK | cut -f1)"
    echo "Mode: $BUILD_LABEL"
    echo ""
    echo "Install: adb install $APK"
else
    echo "ERROR: APK not found at $APK"
    exit 1
fi
