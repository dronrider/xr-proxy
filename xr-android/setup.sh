#!/bin/bash
# Setup script for XR Proxy Android build environment.
# Works on macOS and Linux.
#
# Installs:
#   - Rust + cargo-ndk + Android targets
#   - JDK 17 (Temurin)
#   - Android SDK command-line tools
#   - Android SDK platform, build-tools, NDK
#
# Usage:
#   chmod +x setup.sh && ./setup.sh
#
# After setup, build with:
#   ./build.sh

set -e

# ── Detect OS ────────────────────────────────────────────────────────

OS="$(uname -s)"
ARCH="$(uname -m)"

case "$OS" in
    Darwin) PLATFORM="mac" ;;
    Linux)  PLATFORM="linux" ;;
    *)      echo "Unsupported OS: $OS"; exit 1 ;;
esac

case "$ARCH" in
    x86_64|amd64)   HOST_ARCH="x64" ;;
    arm64|aarch64)   HOST_ARCH="aarch64" ;;
    *)               echo "Unsupported arch: $ARCH"; exit 1 ;;
esac

echo "=== XR Proxy Android Build Setup ==="
echo "OS: $OS ($PLATFORM), Arch: $ARCH ($HOST_ARCH)"
echo ""

TOOLS_DIR="$HOME/android-tools"
mkdir -p "$TOOLS_DIR"

# ── 1. Rust ──────────────────────────────────────────────────────────

echo "--- [1/5] Rust ---"
if command -v rustup &>/dev/null; then
    echo "Rust already installed: $(rustc --version)"
else
    echo "Installing Rust..."
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
    source "$HOME/.cargo/env"
fi

source "$HOME/.cargo/env" 2>/dev/null || true

echo "Adding Android targets..."
rustup target add aarch64-linux-android x86_64-linux-android

if command -v cargo-ndk &>/dev/null; then
    echo "cargo-ndk already installed"
else
    echo "Installing cargo-ndk..."
    cargo install cargo-ndk
fi
echo ""

# ── 2. JDK 17 ───────────────────────────────────────────────────────

echo "--- [2/5] JDK 17 (Temurin) ---"
JDK_DIR="$TOOLS_DIR/jdk17"

if [ -x "$JDK_DIR/bin/java" ]; then
    echo "JDK 17 already installed: $($JDK_DIR/bin/java -version 2>&1 | head -1)"
else
    echo "Downloading JDK 17..."

    # Temurin API: pick the right binary for OS/arch
    case "$PLATFORM-$HOST_ARCH" in
        mac-aarch64) JDK_OS="mac"; JDK_ARCH="aarch64" ;;
        mac-x64)     JDK_OS="mac"; JDK_ARCH="x64" ;;
        linux-x64)   JDK_OS="linux"; JDK_ARCH="x64" ;;
        linux-aarch64) JDK_OS="linux"; JDK_ARCH="aarch64" ;;
        *) echo "No JDK binary for $PLATFORM-$HOST_ARCH"; exit 1 ;;
    esac

    JDK_URL="https://api.adoptium.net/v3/binary/latest/17/ga/$JDK_OS/$JDK_ARCH/jdk/hotspot/normal/eclipse"
    TMP_JDK="$TOOLS_DIR/jdk17-download.tar.gz"

    curl -L -o "$TMP_JDK" "$JDK_URL"

    # Extract to temp dir first, then find the actual JDK root.
    # On Linux: jdk-17.x.x+y/bin/java (strip 1)
    # On macOS: jdk-17.x.x+y/Contents/Home/bin/java (strip 3)
    TMP_JDK_EXTRACT="$TOOLS_DIR/jdk17-extract"
    rm -rf "$TMP_JDK_EXTRACT"
    mkdir -p "$TMP_JDK_EXTRACT"
    tar xzf "$TMP_JDK" -C "$TMP_JDK_EXTRACT"

    # Find the directory containing bin/java
    JDK_ROOT=$(find "$TMP_JDK_EXTRACT" -name "java" -path "*/bin/java" -type f | head -1 | sed 's|/bin/java$||')
    if [ -z "$JDK_ROOT" ]; then
        echo "ERROR: Could not find java binary in downloaded archive"
        rm -rf "$TMP_JDK_EXTRACT" "$TMP_JDK"
        exit 1
    fi

    rm -rf "$JDK_DIR"
    mv "$JDK_ROOT" "$JDK_DIR"
    rm -rf "$TMP_JDK_EXTRACT" "$TMP_JDK"

    echo "Installed: $($JDK_DIR/bin/java -version 2>&1 | head -1)"
fi
echo ""

# ── 3. Android SDK command-line tools ────────────────────────────────

echo "--- [3/5] Android SDK command-line tools ---"
SDK_DIR="$TOOLS_DIR/sdk"
CMDLINE_TOOLS="$SDK_DIR/cmdline-tools/latest/bin/sdkmanager"

if [ -x "$CMDLINE_TOOLS" ]; then
    echo "Command-line tools already installed"
else
    echo "Downloading Android command-line tools..."

    case "$PLATFORM" in
        mac)   CMDLINE_ZIP_OS="mac" ;;
        linux) CMDLINE_ZIP_OS="linux" ;;
    esac

    CMDLINE_URL="https://dl.google.com/android/repository/commandlinetools-${CMDLINE_ZIP_OS}-11076708_latest.zip"
    TMP_CMDLINE="$TOOLS_DIR/cmdline-tools.zip"

    curl -L -o "$TMP_CMDLINE" "$CMDLINE_URL"
    TMP_UNZIP="$TOOLS_DIR/cmdline-tools-tmp"
    rm -rf "$TMP_UNZIP"
    unzip -q "$TMP_CMDLINE" -d "$TMP_UNZIP"
    mkdir -p "$SDK_DIR/cmdline-tools"
    rm -rf "$SDK_DIR/cmdline-tools/latest"
    mv "$TMP_UNZIP/cmdline-tools" "$SDK_DIR/cmdline-tools/latest"
    rm -rf "$TMP_CMDLINE" "$TMP_UNZIP"

    echo "Installed command-line tools"
fi
echo ""

# ── 4. Android SDK packages ─────────────────────────────────────────

echo "--- [4/5] Android SDK packages (platform, build-tools, NDK) ---"
export JAVA_HOME="$JDK_DIR"
export ANDROID_HOME="$SDK_DIR"
export PATH="$JDK_DIR/bin:$SDK_DIR/cmdline-tools/latest/bin:$PATH"

# Accept licenses silently
yes 2>/dev/null | sdkmanager --licenses > /dev/null 2>&1 || true

# Install required packages
PACKAGES=(
    "platform-tools"
    "platforms;android-34"
    "build-tools;34.0.0"
    "ndk;27.0.12077973"
)

for pkg in "${PACKAGES[@]}"; do
    pkg_dir=$(echo "$pkg" | tr ';' '/')
    if [ -d "$SDK_DIR/$pkg_dir" ]; then
        echo "  Already installed: $pkg"
    else
        echo "  Installing: $pkg ..."
        if ! sdkmanager --install "$pkg" 2>&1 | tail -5; then
            echo "  WARNING: Failed to install $pkg"
        fi
    fi
done
echo ""

# ── 5. Verify ───────────────────────────────────────────────────────

echo "--- [5/5] Verification ---"

NDK_DIR=$(ls -d "$SDK_DIR/ndk/"* 2>/dev/null | head -1)

ERRORS=0

check() {
    if [ "$2" = "ok" ]; then
        echo "  ✓ $1"
    else
        echo "  ✗ $1 — MISSING"
        ERRORS=$((ERRORS + 1))
    fi
}

command -v rustc &>/dev/null && check "Rust $(rustc --version | cut -d' ' -f2)" "ok" || check "Rust" "missing"
command -v cargo-ndk &>/dev/null && check "cargo-ndk" "ok" || check "cargo-ndk" "missing"
rustup target list --installed | grep -q aarch64-linux-android && check "target aarch64-linux-android" "ok" || check "target aarch64-linux-android" "missing"
rustup target list --installed | grep -q x86_64-linux-android && check "target x86_64-linux-android" "ok" || check "target x86_64-linux-android" "missing"
[ -x "$JDK_DIR/bin/java" ] && check "JDK 17" "ok" || check "JDK 17" "missing"
[ -x "$CMDLINE_TOOLS" ] && check "Android SDK cmdline-tools" "ok" || check "Android SDK cmdline-tools" "missing"
[ -d "$SDK_DIR/platforms/android-34" ] && check "Android platform 34" "ok" || check "Android platform 34" "missing"
[ -d "$SDK_DIR/build-tools/34.0.0" ] && check "Build tools 34.0.0" "ok" || check "Build tools 34.0.0" "missing"
[ -n "$NDK_DIR" ] && check "NDK $(basename $NDK_DIR)" "ok" || check "NDK" "missing"

echo ""

if [ "$ERRORS" -gt 0 ]; then
    echo "=== Setup incomplete: $ERRORS errors ==="
    exit 1
fi

# ── Print environment ────────────────────────────────────────────────

echo "=== Setup complete ==="
echo ""
echo "Add to your shell profile (~/.bashrc, ~/.zshrc, etc.):"
echo ""
echo "  export JAVA_HOME=\"$JDK_DIR\""
echo "  export ANDROID_HOME=\"$SDK_DIR\""
echo "  export ANDROID_NDK_HOME=\"$NDK_DIR\""
echo "  export PATH=\"\$JAVA_HOME/bin:\$ANDROID_HOME/cmdline-tools/latest/bin:\$ANDROID_HOME/platform-tools:\$PATH\""
echo ""
echo "Then build:"
echo "  cd xr-android && ./build.sh"
