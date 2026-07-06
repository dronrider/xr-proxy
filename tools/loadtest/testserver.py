#!/usr/bin/env python3
"""Нагрузочный тест-сервер стенда XR-087.

Ставится на aeza, слушает TLS на нестандартном порту (роутер проксирует такой
трафик автоматически по первому байту 0x16, без правил роутинга). Отдаёт разные
виды нагрузки, чтобы провоцировать зависания прокси-тракта (XR-086):

  GET /download?size=N   - сплошная выгрузка N байт (забивает mux writer сервер->клиент)
  GET /slow?size=N&rate=R- N байт со скоростью R байт/с (backpressure/idle)
  GET /small             - крошечный ответ (высокочастотный churn открытий)
  GET /burst?size=N      - отдать N байт и держать соединение открытым

Минимальный HTTP: читаем строку запроса, отвечаем. asyncio -> тысячи соединений.
TLS self-signed (клиент проверку отключает). Запуск:
  python3 testserver.py [port]     (по умолчанию 9443)
Сертификат ожидается рядом: cert.pem + key.pem (см. README).
"""
import asyncio, ssl, sys, os, urllib.parse

PORT = int(sys.argv[1]) if len(sys.argv) > 1 else 9443
HERE = os.path.dirname(os.path.abspath(__file__))
CERT = os.path.join(HERE, "cert.pem")
KEY = os.path.join(HERE, "key.pem")
CHUNK = 64 * 1024
stats = {"conn": 0, "download": 0, "slow": 0, "small": 0, "burst": 0}


def parse(reqline):
    # "GET /path?query HTTP/1.1"
    try:
        _, target, _ = reqline.split(" ", 2)
    except ValueError:
        return "/", {}
    u = urllib.parse.urlparse(target)
    q = {k: v[0] for k, v in urllib.parse.parse_qs(u.query).items()}
    return u.path, q


async def send_headers(writer, extra=b""):
    writer.write(b"HTTP/1.1 200 OK\r\nConnection: close\r\n"
                 b"Content-Type: application/octet-stream\r\n" + extra + b"\r\n")
    await writer.drain()


async def handle(reader, writer):
    stats["conn"] += 1
    try:
        line = await asyncio.wait_for(reader.readline(), timeout=15)
        # дочитать остальные заголовки (до пустой строки), не блокируясь долго
        while True:
            h = await asyncio.wait_for(reader.readline(), timeout=15)
            if h in (b"\r\n", b"", b"\n"):
                break
        path, q = parse(line.decode("latin1").strip())
        buf = b"x" * CHUNK

        if path.startswith("/download"):
            stats["download"] += 1
            size = int(q.get("size", 10 * 1024 * 1024))
            await send_headers(writer)
            sent = 0
            while sent < size:
                n = min(CHUNK, size - sent)
                writer.write(buf[:n])
                await writer.drain()
                sent += n
        elif path.startswith("/slow"):
            stats["slow"] += 1
            size = int(q.get("size", 1024 * 1024))
            rate = max(1024, int(q.get("rate", 32 * 1024)))
            await send_headers(writer)
            sent = 0
            step = max(1024, rate // 10)
            while sent < size:
                n = min(step, size - sent)
                writer.write(buf[:n])
                await writer.drain()
                sent += n
                await asyncio.sleep(step / rate)
        elif path.startswith("/burst"):
            stats["burst"] += 1
            size = int(q.get("size", 256 * 1024))
            await send_headers(writer)
            writer.write((buf * (size // CHUNK + 1))[:size])
            await writer.drain()
            await asyncio.sleep(30)  # держим открытым
        else:
            stats["small"] += 1
            body = b"ok"
            await send_headers(writer, b"Content-Length: 2\r\n")
            writer.write(body)
            await writer.drain()
    except Exception:
        pass
    finally:
        try:
            writer.close()
        except Exception:
            pass


async def reporter():
    last = dict(stats)
    while True:
        await asyncio.sleep(10)
        d = {k: stats[k] - last[k] for k in stats}
        last = dict(stats)
        print(f"[testserver] +10s conn={d['conn']} dl={d['download']} slow={d['slow']} "
              f"small={d['small']} burst={d['burst']} (total conn={stats['conn']})", flush=True)


async def main():
    ctx = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
    ctx.load_cert_chain(CERT, KEY)
    server = await asyncio.start_server(handle, "0.0.0.0", PORT, ssl=ctx)
    print(f"[testserver] TLS on 0.0.0.0:{PORT}", flush=True)
    asyncio.create_task(reporter())
    async with server:
        await server.serve_forever()


asyncio.run(main())
