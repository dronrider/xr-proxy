#!/bin/sh
# xr-share installer (XR-028). Downloads the agent, verifies its SHA-256, and
# — given a registration token — configures it and starts the service. One
# command, no hands:
#
#   curl -fsSL https://xr-hub.zoobr.top/share/install.sh \
#     | sudo sh -s -- --dir /srv/share --token <REG-TOKEN-FROM-HUB>
#
# Generate <REG-TOKEN> in the hub admin (Shares → "Команда установки"). Without
# --dir/--token it just installs the binary; configure later with `xr-share init`.
#
# Flags (or env): --dir/XR_DIR, --token/XR_TOKEN, --hub/XR_HUB,
#                 --addr/XR_ADDR (advertised address; default = source IP),
#                 --name/XR_NAME (share name; default = hostname).
set -eu

BASE="${XR_SHARE_BASE:-https://xr-hub.zoobr.top/share}"
DIR="${XR_DIR:-}"
TOKEN="${XR_TOKEN:-}"
HUB="${XR_HUB:-https://xr-hub.zoobr.top}"
ADDR="${XR_ADDR:-}"
NAME="${XR_NAME:-}"
while [ $# -gt 0 ]; do
  case "$1" in
    --dir)   DIR="$2";   shift 2 ;;
    --token) TOKEN="$2"; shift 2 ;;
    --hub)   HUB="$2";   shift 2 ;;
    --addr)  ADDR="$2";  shift 2 ;;
    --name)  NAME="$2";  shift 2 ;;
    *) printf 'warning: ignoring unknown arg %s\n' "$1" >&2; shift ;;
  esac
done

say()  { printf '%s\n' "$*"; }
die()  { printf 'error: %s\n' "$*" >&2; exit 1; }
have() { command -v "$1" >/dev/null 2>&1; }

os=$(uname -s 2>/dev/null || echo unknown)
arch=$(uname -m 2>/dev/null || echo unknown)
case "$os" in
  Linux)  os=linux ;;
  Darwin) os=macos ;;
  *) die "unsupported OS '$os' — on Windows use install.ps1 in PowerShell" ;;
esac
case "$arch" in
  x86_64|amd64)  arch=x86_64 ;;
  aarch64|arm64) arch=aarch64 ;;
  *) die "unsupported architecture '$arch'" ;;
esac
bin="xr-share-${os}-${arch}"

fetch()    { if have curl; then curl -fsSL "$1"; elif have wget; then wget -qO- "$1"; else die "need curl or wget"; fi; }
fetch_to() { if have curl; then curl -fsSL "$1" -o "$2"; elif have wget; then wget -qO "$2" "$1"; else die "need curl or wget"; fi; }

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

say "Downloading $bin ..."
fetch_to "$BASE/$bin" "$tmp/$bin" || die "download failed: $BASE/$bin"

say "Verifying checksum ..."
fetch "$BASE/SHA256SUMS" > "$tmp/SHA256SUMS" || die "cannot fetch SHA256SUMS"
expected=$(grep " ${bin}\$" "$tmp/SHA256SUMS" | awk '{print $1}' | head -1)
[ -n "$expected" ] || die "no checksum listed for $bin"
if   have sha256sum; then actual=$(sha256sum "$tmp/$bin" | awk '{print $1}')
elif have shasum;    then actual=$(shasum -a 256 "$tmp/$bin" | awk '{print $1}')
else die "need sha256sum or shasum"; fi
[ "$expected" = "$actual" ] || die "checksum mismatch (expected $expected, got $actual)"
say "  ok ($actual)"

chmod +x "$tmp/$bin"
if [ -w /usr/local/bin ]; then dir=/usr/local/bin; else dir="$HOME/.local/bin"; mkdir -p "$dir"; fi
mv "$tmp/$bin" "$dir/xr-share"
say "Installed: $dir/xr-share"

# ── No-hands: configure + start the service ─────────────────────────
if [ -n "$DIR" ] && [ -n "$TOKEN" ]; then
  if [ "$(id -u)" != 0 ]; then
    say ""
    say "To self-register + start the service, re-run as root, e.g.:"
    say "  curl -fsSL $BASE/install.sh | sudo sh -s -- --dir \"$DIR\" --token <token>"
    exit 0
  fi
  say "Registering with the hub and starting the service ..."
  set -- init --non-interactive --dir "$DIR" --hub "$HUB" --token "$TOKEN"
  [ -n "$ADDR" ] && set -- "$@" --addr "$ADDR"
  [ -n "$NAME" ] && set -- "$@" --name "$NAME"
  "$dir/xr-share" "$@"
  "$dir/xr-share" service install
  say ""
  say "Done — xr-share is running and registered. Files in $DIR are now shareable."
else
  say ""
  say "Next: configure + enable the service"
  say "  sudo xr-share init --dir /path/to/share --token <reg-token-from-hub>"
  say "  sudo xr-share service install"
fi
