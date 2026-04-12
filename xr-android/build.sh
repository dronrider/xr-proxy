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
#   cd xr-android && ./build.sh
#   APK will be at: app/build/outputs/apk/debug/app-debug.apk

set -e

export JAVA_HOME="${JAVA_HOME:-$HOME/android-tools/jdk17}"
export ANDROID_HOME="${ANDROID_HOME:-$HOME/android-tools/sdk}"
export ANDROID_NDK_HOME="${ANDROID_NDK_HOME:-$ANDROID_HOME/ndk/27.0.12077973}"
export PATH="$JAVA_HOME/bin:$HOME/.cargo/bin:$PATH"

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

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
echo "=== Building Rust native libraries ==="
cargo ndk \
    -t aarch64-linux-android \
    -t x86_64-linux-android \
    -o "$SCRIPT_DIR/app/src/main/jniLibs" \
    build -p xr-android-jni --release

echo ""
echo "=== Building Android APK ==="
cd "$SCRIPT_DIR"

# Generate local.properties with correct SDK path for this machine.
echo "sdk.dir=$ANDROID_HOME" > local.properties

sh ./gradlew assembleDebug --no-daemon

echo ""
echo "=== Done ==="
APK="app/build/outputs/apk/debug/app-debug.apk"
if [ -f "$APK" ]; then
    echo "APK: $(realpath $APK)"
    echo "Size: $(du -h $APK | cut -f1)"
    echo ""
    echo "Install: adb install $APK"
else
    echo "ERROR: APK not found at $APK"
    exit 1
fi
