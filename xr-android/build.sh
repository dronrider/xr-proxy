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

echo "=== Building Rust native libraries ==="
cd "$PROJECT_ROOT"
cargo ndk \
    -t aarch64-linux-android \
    -t x86_64-linux-android \
    -o "$SCRIPT_DIR/app/src/main/jniLibs" \
    build -p xr-android-jni --release

echo ""
echo "=== Building Android APK ==="
cd "$SCRIPT_DIR"
GRADLE_OPTS="-Xmx768m" sh ./gradlew assembleDebug --no-daemon

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
