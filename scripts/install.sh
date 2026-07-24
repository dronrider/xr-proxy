#!/bin/sh
# Тонкая обёртка автоустановки (XR-015, LLD-13 п. 3.3): качает xr-setup с
# хаба, сверяет SHA-256 и передаёт ему управление. Вся логика установки
# живёт в самом xr-setup, здесь только доставка. Ручной сценарий владельца:
#
#   curl -fsSL https://xr-hub.zoobr.top/api/v1/setup/install.sh \
#     | sh -s -- server --with-hub --hub-domain xr-hub.example.com
#
# Без аргументов ставит бинарь и печатает подсказку. База раздачи по
# умолчанию это основной хаб проекта; при установке с другого хаба задать
# его явно: XR_SETUP_BASE=https://<хаб>/api/v1/setup.
set -eu

BASE="${XR_SETUP_BASE:-https://xr-hub.zoobr.top/api/v1/setup}"

say() { printf '%s\n' "$*"; }
die() { printf 'error: %s\n' "$*" >&2; exit 1; }
have() { command -v "$1" >/dev/null 2>&1; }

[ "$(uname -s 2>/dev/null || echo unknown)" = Linux ] || die "поддерживается только Linux"
arch=$(uname -m 2>/dev/null || echo unknown)
case "$arch" in
  x86_64|amd64)  arch=x86_64 ;;
  aarch64|arm64) arch=aarch64 ;;
  *) die "архитектура '$arch' не поддерживается" ;;
esac
bin="xr-setup-linux-${arch}"

fetch()    { if have curl; then curl -fsSL "$1"; elif have wget; then wget -qO- "$1"; else die "нужен curl или wget"; fi; }
fetch_to() { if have curl; then curl -fsSL "$1" -o "$2"; elif have wget; then wget -qO "$2" "$1"; else die "нужен curl или wget"; fi; }

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

say "Скачиваю $bin ..."
fetch_to "$BASE/$bin" "$tmp/$bin" || die "не скачался $BASE/$bin"

say "Проверяю контрольную сумму ..."
fetch "$BASE/SHA256SUMS" > "$tmp/SHA256SUMS" || die "не скачался SHA256SUMS"
expected=$(grep " ${bin}\$" "$tmp/SHA256SUMS" | awk '{print $1}' | head -1)
[ -n "$expected" ] || die "в SHA256SUMS нет записи для $bin"
if have sha256sum; then actual=$(sha256sum "$tmp/$bin" | awk '{print $1}')
else die "нужен sha256sum"; fi
[ "$expected" = "$actual" ] || die "хеш не совпал (ожидался $expected, получен $actual)"
say "  ok ($actual)"

chmod +x "$tmp/$bin"
[ "$(id -u)" = 0 ] || die "нужен root: перезапусти через sudo sh"
# На OpenWRT нет /usr/local/bin (и его нет в PATH), там дом это /usr/bin.
dest=/usr/local/bin
[ -d "$dest" ] || dest=/usr/bin
mv "$tmp/$bin" "$dest/xr-setup"
say "Установлен $dest/xr-setup"

if [ $# -gt 0 ]; then
  rm -rf "$tmp"
  # Бинари компонентов установщик возьмёт с той же раздачи, если источник
  # не задан явно (обе формы флага: с пробелом и с =).
  case " $* " in
    *" --dist-url"*|*" --from-dir"*) exec "$dest/xr-setup" "$@" ;;
    *) exec "$dest/xr-setup" "$@" --dist-url "$BASE" ;;
  esac
fi
say ""
say "Дальше, например:"
say "  xr-setup server --with-hub --hub-domain <домен> --dist-url $BASE"
