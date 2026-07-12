#!/bin/sh
# xr-share installer (XR-028/029). Downloads the agent, verifies its SHA-256,
# and, given a registration token, installs it as a service with a long-lived
# hub mandate so you can then share any number of paths. One command:
#
#   curl -fsSL https://xr-hub.zoobr.top/share/install.sh \
#     | sudo sh -s -- --token <REG-TOKEN-FROM-HUB>
#
# Generate <REG-TOKEN> in the hub admin (Shares, "Команда установки"). Then,
# anytime:  sudo xr-share share /srv/photos   (a folder OR a single file).
# Pass --dir to also share one path right after install. Without --token it just
# installs the binary; set up later with `xr-share install --token <reg-token>`.
#
# One-command share (XR-127): a --setup token packs the reg-token and an invite,
# so a single line installs, mandates, and shares a folder on any OS, relay on by
# default and the invite attached with no extra flags:
#   curl -fsSL https://xr-hub.zoobr.top/share/install.sh | sudo sh -s -- \
#     --setup <SETUP-TOKEN> --dir /srv/photos
#
# Flags (or env): --token/XR_TOKEN, --setup/XR_SETUP, --hub/XR_HUB,
#                 --dir/XR_DIR (share now), --addr/XR_ADDR (advertised address;
#                 default = source IP), --name/XR_NAME (share name).
set -eu

BASE="${XR_SHARE_BASE:-https://xr-hub.zoobr.top/share}"
DIR="${XR_DIR:-}"
TOKEN="${XR_TOKEN:-}"
HUB="${XR_HUB:-https://xr-hub.zoobr.top}"
ADDR="${XR_ADDR:-}"
NAME="${XR_NAME:-}"
RELAY="${XR_RELAY:-}"
INVITE="${XR_INVITE:-}"
SETUP="${XR_SETUP:-}"
while [ $# -gt 0 ]; do
  case "$1" in
    --dir)    DIR="$2";    shift 2 ;;
    --token)  TOKEN="$2";  shift 2 ;;
    --setup)  SETUP="$2";  shift 2 ;;
    --hub)    HUB="$2";    shift 2 ;;
    --addr)   ADDR="$2";   shift 2 ;;
    --name)   NAME="$2";   shift 2 ;;
    --relay)  RELAY=1;     shift ;;
    --invite) INVITE="$2"; shift 2 ;;
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
  *) die "unsupported OS '$os' (on Windows use install.ps1 in PowerShell)" ;;
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

# No-hands: install the service with a hub mandate.
if [ -n "$SETUP" ] || [ -n "$TOKEN" ]; then
  if [ "$(id -u)" != 0 ]; then
    say ""
    say "Installing the service needs root, re-run as:"
    if [ -n "$SETUP" ]; then
      say "  curl -fsSL $BASE/install.sh | sudo sh -s -- --setup <setup-token> --dir <path>"
    else
      say "  curl -fsSL $BASE/install.sh | sudo sh -s -- --token <token>"
    fi
    exit 0
  fi
  say "Installing the service and exchanging the token for a hub mandate ..."
  if [ -n "$SETUP" ]; then
    # One setup token packs the reg-token and an invite (XR-127): the invite is
    # pinned as the default, so the share below needs no --invite and relay is on.
    "$dir/xr-share" install --non-interactive --hub "$HUB" --setup "$SETUP"
  else
    "$dir/xr-share" install --non-interactive --hub "$HUB" --token "$TOKEN"
  fi
  if [ -n "$DIR" ]; then
    say "Sharing $DIR ..."
    set -- share "$DIR"
    [ -n "$ADDR" ] && set -- "$@" --addr "$ADDR"
    [ -n "$NAME" ] && set -- "$@" --name "$NAME"
    # Relay is on by default once the mandate carries a relay descriptor (XR-127);
    # --relay only forces it and --invite is only needed without a --setup invite.
    [ -n "$RELAY" ]  && set -- "$@" --relay
    [ -n "$INVITE" ] && set -- "$@" --invite "$INVITE"
    "$dir/xr-share" "$@"
  fi
  say ""
  say "Done. Share any path anytime (folder or file):"
  say "  sudo xr-share share /srv/photos"
  say "  sudo xr-share list"
else
  # Binary-only run (no token). If the service is already installed, this was an
  # update, so restart it: the new binary (and its relay auto-config on start,
  # XR-123) takes effect now. Fresh installs fall through to the hint.
  if [ "$(id -u)" = 0 ] && systemctl list-unit-files xr-share.service >/dev/null 2>&1 \
     && systemctl cat xr-share.service >/dev/null 2>&1; then
    systemctl restart xr-share
    say ""
    say "Updated and restarted xr-share with the new binary."
  else
    say ""
    say "Next: install the service, then share paths"
    say "  sudo xr-share install --hub $HUB --token <reg-token-from-hub>"
    say "  sudo xr-share share /path/to/share"
  fi
fi
