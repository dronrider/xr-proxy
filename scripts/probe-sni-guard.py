#!/usr/bin/env python3
"""Пробник защиты разбора SNI (XR-205).

Шлёт с LAN-машины два кривых начала соединения, на которых xr-client раньше
падал: обрезанный TLS-рекорд и корректный ClientHello с именем длиннее предела
домена. Порт по умолчанию не 80 и не 443, там TLS-подобное начало уходит в
прокси само, без зависимости от правил маршрутизации.

Сам по себе пробник ничего не проверяет, он только стучится. Итог смотрят на
роутере: PID xr-client не менялся, в crash.log пусто, а в logread на каждый
стук легла своя строка отказа сниффера.

    ./scripts/probe-sni-guard.py 9.9.9.9
"""

import socket
import sys


def truncated_record() -> bytes:
    """Рекорд объявляет длину 1, а заголовок handshake это 4 байта."""
    return bytes([0x16, 0x03, 0x01, 0x00, 0x01, 0x01]) + b"\0" * 58


def client_hello(hostname: str) -> bytes:
    """Минимальный ClientHello с заданным именем в SNI."""
    host = hostname.encode()

    entry = b"\x00" + len(host).to_bytes(2, "big") + host
    sni_ext = len(entry).to_bytes(2, "big") + entry

    extensions = b"\x00\x00" + len(sni_ext).to_bytes(2, "big") + sni_ext

    body = b"\x03\x03" + b"\0" * 32  # версия + random
    body += b"\x00"  # session id
    body += (2).to_bytes(2, "big") + b"\x00\xff"  # один шифр
    body += b"\x01\x00"  # компрессия
    body += len(extensions).to_bytes(2, "big") + extensions

    handshake = b"\x01" + len(body).to_bytes(3, "big") + body
    return b"\x16\x03\x01" + len(handshake).to_bytes(2, "big") + handshake


def knock(dst: str, port: int, payload: bytes, what: str) -> None:
    try:
        s = socket.create_connection((dst, port), 5)
    except OSError as e:
        print(f"{what}: соединение не поднялось ({e})")
        return
    try:
        s.sendall(payload)
        print(f"{what}: отправлено {len(payload)} байт")
    finally:
        s.close()


def main() -> int:
    if len(sys.argv) < 2:
        print(__doc__)
        return 2
    dst = sys.argv[1]
    port = int(sys.argv[2]) if len(sys.argv) > 2 else 8080

    knock(dst, port, truncated_record(), "обрезанный рекорд")
    knock(dst, port, client_hello("a" * 300), "SNI в 300 байт")
    return 0


if __name__ == "__main__":
    sys.exit(main())
