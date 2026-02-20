#!/bin/bash
# Generate a random base64-encoded key for XR Proxy.
# The same key must be used in both client and server configs.
#
# Usage: ./generate-key.sh [length_bytes]
# Default: 64 bytes (512 bits) → 88 characters base64

set -euo pipefail

LENGTH=${1:-64}

if ! command -v openssl &>/dev/null && ! command -v base64 &>/dev/null; then
    echo "Error: need 'openssl' or 'base64' command" >&2
    exit 1
fi

KEY=$(openssl rand -base64 "$LENGTH" 2>/dev/null || head -c "$LENGTH" /dev/urandom | base64)

# Remove newlines from base64 output
KEY=$(echo "$KEY" | tr -d '\n')

echo ""
echo "═══════════════════════════════════════════════════════════"
echo "  XR Proxy — Generated Key ($LENGTH bytes)"
echo "═══════════════════════════════════════════════════════════"
echo ""
echo "  $KEY"
echo ""
echo "  Copy this key to BOTH config files:"
echo "    - Client: configs/client.toml → [obfuscation] key = \"...\""
echo "    - Server: configs/server.toml → [obfuscation] key = \"...\""
echo ""
echo "  ⚠  Keys MUST match on client and server!"
echo "═══════════════════════════════════════════════════════════"
echo ""
