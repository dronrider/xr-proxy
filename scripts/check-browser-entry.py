#!/usr/bin/env python3
"""Проверка браузерного входа xr-web: WebSocket насквозь и поведение при
выключенном агенте (LLD-38 фаза 3, XR-264).

Три подкоманды, каждая судится кодом возврата и последней строкой вывода:

  echo-service  синтетический сервис на машине агента: страница по GET и эхо по
                WebSocket. На него смотрит [[expose]]-запись публикации.
  ws            вход, апгрейд через фронт, кадры в обе стороны и ожидание
                штатного закрытия по лимиту жизни сплайса. Печатает
                `browser entry ws: ok`.
  offline       агент выключен: вход отдаёт 502 с названной причиной за
                секунды, а статус публикаций на хабе показывает её не на связи.
                Печатает `browser entry offline: ok`.

Зависимостей нет, только стандартная библиотека: скрипт гоняется и на VPS, и на
машине владельца, где ставить пакеты незачем.

Пример стенда (фронт на 18090, хаб на 18099, публикация dash):

  python3 scripts/check-browser-entry.py echo-service --port 18765 &
  python3 scripts/check-browser-entry.py ws \\
      --entry http://127.0.0.1:18090 --host dash.web.test \\
      --username owner --password "$PASS" --wait 120
  # остановить агента
  python3 scripts/check-browser-entry.py offline \\
      --entry http://127.0.0.1:18090 --host dash.web.test \\
      --username owner --password "$PASS" \\
      --hub http://127.0.0.1:18099 --secret "$WEB_SECRET" --publication dash

Лимит жизни сплайса на время проверки уменьшают конфигом relay
(`[relay] splice_lifetime_secs`, он же в конфиге хаба, откуда его берёт
маршрут): на боевом часе ждать штатного закрытия пришлось бы 59 минут.
"""

import argparse
import base64
import hashlib
import http.client
import json
import os
import socket
import ssl
import struct
import sys
import threading
import time
import urllib.parse

WS_GUID = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11"


def ws_accept(key):
    """Sec-WebSocket-Accept по ключу клиента by RFC 6455 (раздел 1.3):
    SHA-1 от ключа со склеенным магическим GUID, дальше base64."""
    return base64.b64encode(hashlib.sha1((key + WS_GUID).encode()).digest()).decode()


def fail(message):
    print(f"ОШИБКА: {message}", file=sys.stderr)
    sys.exit(1)


def parse_entry(entry):
    """Адрес фронта: схема, хост с портом."""
    url = urllib.parse.urlparse(entry)
    if url.scheme not in ("http", "https"):
        fail(f"адрес входа должен быть http или https, а не {entry}")
    port = url.port or (443 if url.scheme == "https" else 80)
    return url.scheme, url.hostname, port


def ssl_context(insecure):
    ctx = ssl.create_default_context()
    if insecure:
        ctx.check_hostname = False
        ctx.verify_mode = ssl.CERT_NONE
    return ctx


def connection(args):
    scheme, addr, port = parse_entry(args.entry)
    if scheme == "https":
        return http.client.HTTPSConnection(
            addr, port, timeout=args.timeout, context=ssl_context(args.insecure)
        )
    return http.client.HTTPConnection(addr, port, timeout=args.timeout)


def login(args):
    """Пройти вход и вернуть значение cookie сессии."""
    body = urllib.parse.urlencode({"username": args.username, "password": args.password})
    conn = connection(args)
    conn.request(
        "POST",
        "/.xr-web/login",
        body=body,
        headers={"Host": args.host, "Content-Type": "application/x-www-form-urlencoded"},
    )
    resp = conn.getresponse()
    raw = resp.getheader("set-cookie") or ""
    resp.read()
    conn.close()
    if resp.status != 303 or "xrweb=" not in raw:
        fail(f"вход не пустил: HTTP {resp.status}, Set-Cookie {raw!r}")
    if "domain" in raw.lower():
        fail(f"cookie сессии обязана быть host-only, а в ней есть Domain: {raw}")
    token = raw.split("xrweb=", 1)[1].split(";", 1)[0]
    return f"xrweb={token}"


def get(args, path, cookie=None):
    """Обычный запрос к публикации. Возвращает (код, тело, секунды)."""
    conn = connection(args)
    headers = {"Host": args.host}
    if cookie:
        headers["Cookie"] = cookie
    started = time.monotonic()
    conn.request("GET", path, headers=headers)
    resp = conn.getresponse()
    body = resp.read().decode("utf-8", "replace")
    spent = time.monotonic() - started
    conn.close()
    return resp.status, body, spent


# WebSocket: клиент и сервис на одних лишь struct и socket.


def mask_frame(opcode, payload):
    """Кадр от клиента: маска обязательна."""
    mask = os.urandom(4)
    masked = bytes(b ^ mask[i % 4] for i, b in enumerate(payload))
    return struct.pack("!BB", 0x80 | opcode, 0x80 | len(payload)) + mask + masked


def server_frame(opcode, payload):
    """Кадр от сервера: без маски."""
    head = struct.pack("!BB", 0x80 | opcode, len(payload))
    return head + payload


def read_exactly(sock, count):
    buf = b""
    while len(buf) < count:
        chunk = sock.recv(count - len(buf))
        if not chunk:
            raise ConnectionError("соединение закрылось посреди кадра")
        buf += chunk
    return buf


def read_frame(sock):
    """Один кадр: (opcode, payload). Маску снимает, если она есть."""
    head = read_exactly(sock, 2)
    opcode = head[0] & 0x0F
    masked = head[1] & 0x80
    length = head[1] & 0x7F
    if length == 126:
        length = struct.unpack("!H", read_exactly(sock, 2))[0]
    elif length == 127:
        length = struct.unpack("!Q", read_exactly(sock, 8))[0]
    mask = read_exactly(sock, 4) if masked else None
    payload = read_exactly(sock, length) if length else b""
    if mask:
        payload = bytes(b ^ mask[i % 4] for i, b in enumerate(payload))
    return opcode, payload


def read_head(sock):
    """Заголовки HTTP-ответа до пустой строки."""
    head = b""
    while b"\r\n\r\n" not in head:
        chunk = sock.recv(1)
        if not chunk:
            raise ConnectionError(f"соединение закрылось на заголовках: {head!r}")
        head += chunk
    return head.decode("utf-8", "replace")


def ws_connect(args, cookie):
    """Апгрейд через вход. Возвращает открытый сокет после `101`."""
    scheme, addr, port = parse_entry(args.entry)
    sock = socket.create_connection((addr, port), timeout=args.timeout)
    if scheme == "https":
        sock = ssl_context(args.insecure).wrap_socket(sock, server_hostname=args.host)
    key = base64.b64encode(os.urandom(16)).decode()
    request = (
        f"GET {args.path} HTTP/1.1\r\n"
        f"Host: {args.host}\r\n"
        f"Cookie: {cookie}\r\n"
        "Connection: Upgrade\r\n"
        "Upgrade: websocket\r\n"
        "Sec-WebSocket-Version: 13\r\n"
        f"Sec-WebSocket-Key: {key}\r\n\r\n"
    )
    sock.sendall(request.encode())
    head = read_head(sock)
    status = head.split("\r\n", 1)[0]
    if "101" not in status:
        sock.close()
        fail(f"апгрейд не прошёл: {status}\n{head}")
    expected = ws_accept(key)
    if expected.lower() not in head.lower():
        sock.close()
        fail(f"рукопожатие не сошлось, Sec-WebSocket-Accept не тот:\n{head}")
    return sock


def cmd_ws(args):
    cookie = login(args)
    print(f"вход пройден, cookie сессии получена ({args.host})")

    code, body, spent = get(args, "/.xr-web/healthz")
    if code != 200 or body.strip() != "ok":
        fail(f"/.xr-web/healthz отдал {code}: {body.strip()!r}")
    print(f"healthz: ok за {spent * 1000:.0f} мс")

    sock = ws_connect(args, cookie)
    print(f"апгрейд принят: 101 на {args.path}")

    started = time.monotonic()
    for i in range(args.frames):
        payload = f"кадр {i}".encode()
        sock.sendall(mask_frame(0x1, payload))
        while True:
            opcode, back = read_frame(sock)
            if opcode == 0x9:  # ping от сервиса
                sock.sendall(mask_frame(0xA, back))
                continue
            break
        if opcode != 0x1 or back != payload:
            fail(f"эхо не сошлось: opcode {opcode}, тело {back!r}")
    print(f"кадры ходят в обе стороны: {args.frames} шт.")

    # Штатное закрытие по лимиту жизни сплайса: его инициирует сам вход, до
    # того как relay обрубит сплайс молча.
    sock.settimeout(args.wait)
    try:
        while True:
            opcode, payload = read_frame(sock)
            if opcode == 0x9:
                sock.sendall(mask_frame(0xA, payload))
                continue
            if opcode == 0x8:
                break
            print(f"по пути пришёл кадр opcode {opcode}, {len(payload)} байт")
    except socket.timeout:
        fail(
            f"штатного закрытия не дождались за {args.wait} с: уменьши "
            "[relay] splice_lifetime_secs (и то же значение в конфиге хаба), "
            "иначе ждать придётся почти час"
        )
    except ConnectionError as e:
        fail(f"сплайс оборвался без штатного закрытия ({e}): это и есть тот обрыв, "
             "который фронт обязан опередить")
    waited = time.monotonic() - started
    code = struct.unpack("!H", payload[:2])[0] if len(payload) >= 2 else 0
    if code != 1001:
        fail(f"закрытие пришло с кодом {code}, а штатное это 1001 (going away)")
    # Ответное закрытие с нашей стороны, как велит протокол.
    try:
        sock.sendall(mask_frame(0x8, struct.pack("!H", 1000)))
    except OSError:
        pass
    sock.close()
    print(f"штатное закрытие 1001 пришло само через {waited:.1f} с после апгрейда")
    print("browser entry ws: ok")


def cmd_offline(args):
    cookie = login(args)

    code, body, spent = get(args, "/", cookie)
    if code != 502:
        fail(f"при выключенном агенте вход обязан отдать 502, а отдал {code}")
    if "не на связи" not in body:
        fail(f"причина не названа в ответе: {body[:400]}")
    if spent > args.deadline:
        fail(f"отказ пришёл за {spent:.1f} с, а обязан за секунды, не таймаутом")
    print(f"обычный запрос: 502 «машина не на связи» за {spent * 1000:.0f} мс")

    # Тот же случай на пути апгрейда: рукопожатие отвергают, а не подвешивают.
    scheme, addr, port = parse_entry(args.entry)
    sock = socket.create_connection((addr, port), timeout=args.timeout)
    if scheme == "https":
        sock = ssl_context(args.insecure).wrap_socket(sock, server_hostname=args.host)
    started = time.monotonic()
    sock.sendall(
        (
            f"GET {args.path} HTTP/1.1\r\nHost: {args.host}\r\nCookie: {cookie}\r\n"
            "Connection: Upgrade\r\nUpgrade: websocket\r\nSec-WebSocket-Version: 13\r\n"
            f"Sec-WebSocket-Key: {base64.b64encode(os.urandom(16)).decode()}\r\n\r\n"
        ).encode()
    )
    head = read_head(sock)
    sock.close()
    spent = time.monotonic() - started
    if "502" not in head.split("\r\n", 1)[0]:
        fail(f"апгрейд к выключенной машине обязан получить 502: {head.splitlines()[0]}")
    if spent > args.deadline:
        fail(f"отказ апгрейда пришёл за {spent:.1f} с, а обязан за секунды")
    print(f"апгрейд: 502 за {spent * 1000:.0f} мс, без ожидания таймаута")

    if args.hub:
        if not args.secret:
            fail("для проверки статуса нужен --secret (общий секрет [web])")
        url = urllib.parse.urlparse(args.hub)
        if url.scheme == "https":
            conn = http.client.HTTPSConnection(
                url.hostname,
                url.port or 443,
                timeout=args.timeout,
                context=ssl_context(args.insecure),
            )
        else:
            conn = http.client.HTTPConnection(url.hostname, url.port or 80, timeout=args.timeout)
        conn.request(
            "GET", "/api/v1/web/status", headers={"Authorization": f"Bearer {args.secret}"}
        )
        resp = conn.getresponse()
        body = resp.read().decode("utf-8", "replace")
        conn.close()
        if resp.status != 200:
            fail(f"GET /api/v1/web/status отдал {resp.status}: {body[:200]}")
        rows = json.loads(body)
        row = next((r for r in rows if r.get("name") == args.publication), None)
        if row is None:
            fail(f"публикации {args.publication} нет в статусе хаба: {body[:200]}")
        if row.get("online") is not False:
            fail(
                f"статус публикации {args.publication} обязан быть «не на связи», "
                f"а хаб отдал online={row.get('online')} (probe_error={row.get('probe_error')})"
            )
        print(f"статус хаба: публикация {args.publication} не на связи, хост {row.get('host')}")

    print("browser entry offline: ok")


# Синтетический сервис: страница по GET и эхо по WebSocket.


def serve_client(sock):
    try:
        head = read_head(sock)
    except (ConnectionError, OSError):
        return
    lowered = head.lower()
    if "upgrade: websocket" not in lowered:
        page = b"synthetic service ok\n"
        sock.sendall(
            b"HTTP/1.1 200 OK\r\ncontent-type: text/plain; charset=utf-8\r\n"
            + f"content-length: {len(page)}\r\n\r\n".encode()
            + page
        )
        sock.close()
        return
    key = ""
    for line in head.split("\r\n"):
        if line.lower().startswith("sec-websocket-key:"):
            key = line.split(":", 1)[1].strip()
    accept = ws_accept(key)
    sock.sendall(
        (
            "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\n"
            f"Connection: Upgrade\r\nSec-WebSocket-Accept: {accept}\r\n\r\n"
        ).encode()
    )
    try:
        while True:
            opcode, payload = read_frame(sock)
            if opcode == 0x8:
                sock.sendall(server_frame(0x8, payload[:2]))
                break
            if opcode == 0x9:
                sock.sendall(server_frame(0xA, payload))
                continue
            if opcode == 0xA:
                continue
            sock.sendall(server_frame(opcode, payload))
    except (ConnectionError, OSError):
        pass
    finally:
        sock.close()


def cmd_echo_service(args):
    srv = socket.socket()
    srv.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    srv.bind((args.bind, args.port))
    srv.listen(16)
    print(f"синтетический сервис на http://{args.bind}:{args.port} (GET и WebSocket-эхо)")
    sys.stdout.flush()
    while True:
        client, _ = srv.accept()
        threading.Thread(target=serve_client, args=(client,), daemon=True).start()


def main():
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = p.add_subparsers(dest="cmd", required=True)

    def entry_args(sp):
        sp.add_argument("--entry", required=True, help="адрес фронта, например https://dash.web.zoobr.top")
        sp.add_argument("--host", required=True, help="имя публикации в Host: <имя>.<web-домен>")
        sp.add_argument("--username", required=True)
        sp.add_argument("--password", required=True)
        sp.add_argument("--path", default="/live", help="путь запроса апгрейда")
        sp.add_argument("--timeout", type=float, default=15.0)
        sp.add_argument("--insecure", action="store_true", help="не проверять сертификат фронта")

    ws = sub.add_parser("ws", help="WebSocket через вход и штатное закрытие по лимиту")
    entry_args(ws)
    ws.add_argument("--frames", type=int, default=3)
    ws.add_argument("--wait", type=float, default=300.0, help="сколько ждать штатного закрытия")
    ws.set_defaults(func=cmd_ws)

    off = sub.add_parser("offline", help="агент выключен: 502 с причиной и статус на хабе")
    entry_args(off)
    off.add_argument("--deadline", type=float, default=5.0, help="за сколько секунд обязан прийти отказ")
    off.add_argument("--hub", help="адрес хаба для GET /api/v1/web/status")
    off.add_argument("--secret", help="общий секрет [web] хаба")
    off.add_argument("--publication", default="dash")
    off.set_defaults(func=cmd_offline)

    echo = sub.add_parser("echo-service", help="синтетический сервис на машине агента")
    echo.add_argument("--port", type=int, default=18765)
    echo.add_argument("--bind", default="127.0.0.1")
    echo.set_defaults(func=cmd_echo_service)

    args = p.parse_args()
    args.func(args)


if __name__ == "__main__":
    main()
