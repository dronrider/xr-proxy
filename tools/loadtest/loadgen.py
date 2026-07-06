#!/usr/bin/env python3
"""Нагрузчик стенда XR-087 (запуск с ноутбука за роутером).

Бьёт по тест-серверу на aeza (TLS на нестандартном порту -> роутер проксирует
автоматически). Ключевой сценарий под гипотезу XR-086: медленно читаемые крупные
загрузки забивают серверный mux writer_tx, а параллельный шквал мелких открытий
требует ConnectAck через переполненный канал -> должен застопорить mux_handler.

Запуск:
  python3 loadgen.py <aeza_ip> [port] [duration] [slow_workers] [churn_workers]
  напр.: python3 loadgen.py 85.192.38.29 9443 300 40 400
"""
import asyncio, ssl, sys, time

IP = sys.argv[1]
PORT = int(sys.argv[2]) if len(sys.argv) > 2 else 9443
DUR = int(sys.argv[3]) if len(sys.argv) > 3 else 300
SLOW = int(sys.argv[4]) if len(sys.argv) > 4 else 40
CHURN = int(sys.argv[5]) if len(sys.argv) > 5 else 400
CONNECT_TIMEOUT = 8.0

ctx = ssl.create_default_context()
ctx.check_hostname = False
ctx.verify_mode = ssl.CERT_NONE

stats = {"churn_ok": 0, "churn_to": 0, "churn_err": 0,
         "slow_ok": 0, "slow_to": 0, "slow_err": 0, "slow_bytes": 0}
stop_at = time.time() + DUR


async def req(path):
    fut = asyncio.open_connection(IP, PORT, ssl=ctx, server_hostname="loadtest.local")
    reader, writer = await asyncio.wait_for(fut, timeout=CONNECT_TIMEOUT)
    writer.write(f"GET {path} HTTP/1.1\r\nHost: loadtest.local\r\nConnection: close\r\n\r\n".encode())
    await asyncio.wait_for(writer.drain(), timeout=CONNECT_TIMEOUT)
    return reader, writer


async def churn_worker():
    # Быстрые короткие проксируемые открытия (нужен ConnectAck на каждое).
    while time.time() < stop_at:
        try:
            reader, writer = await req("/small")
            await asyncio.wait_for(reader.read(256), timeout=CONNECT_TIMEOUT)
            writer.close()
            try:
                await asyncio.wait_for(writer.wait_closed(), timeout=2)
            except Exception:
                pass
            stats["churn_ok"] += 1
        except asyncio.TimeoutError:
            stats["churn_to"] += 1
        except Exception:
            stats["churn_err"] += 1


async def slow_worker():
    # Крупная загрузка, читаемая МЕДЛЕННО: забивает серверный writer_tx.
    while time.time() < stop_at:
        try:
            reader, writer = await req("/download?size=52428800")  # 50 МБ
            while time.time() < stop_at:
                chunk = await asyncio.wait_for(reader.read(16384), timeout=CONNECT_TIMEOUT)
                if not chunk:
                    break
                stats["slow_bytes"] += len(chunk)
                await asyncio.sleep(0.2)  # медленное потребление
            writer.close()
            stats["slow_ok"] += 1
        except asyncio.TimeoutError:
            stats["slow_to"] += 1
        except Exception:
            stats["slow_err"] += 1


async def reporter():
    last = dict(stats)
    while time.time() < stop_at:
        await asyncio.sleep(5)
        d = {k: stats[k] - last[k] for k in stats}
        last = dict(stats)
        el = int(time.time() - (stop_at - DUR))
        print(f"[{el}s] churn ok={d['churn_ok']} TIMEOUT={d['churn_to']} err={d['churn_err']} | "
              f"slow ok={d['slow_ok']} TIMEOUT={d['slow_to']} MB={d['slow_bytes']//1048576}", flush=True)


async def main():
    print(f"loadgen -> {IP}:{PORT} slow={SLOW} churn={CHURN} dur={DUR}s", flush=True)
    tasks = [asyncio.create_task(slow_worker()) for _ in range(SLOW)]
    tasks += [asyncio.create_task(churn_worker()) for _ in range(CHURN)]
    r = asyncio.create_task(reporter())
    await asyncio.gather(*tasks, return_exceptions=True)
    r.cancel()
    print(f"ИТОГО {stats}", flush=True)


asyncio.run(main())
