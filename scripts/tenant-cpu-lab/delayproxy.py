#!/usr/bin/env python3
"""delayproxy.py: TCP の中間サーバ。方向ごとに固定遅延 + ジッターを加えて中継する（ネットワーク遅延の演出）。
   使い方: delayproxy.py <listen_port> <target_port> <one_way_ms> [jitter_ms]
   RTT = one_way × 2。1 接続内の順序は保つ（遅延は単調に積む）。試作: 本番の caplab は Rust (tokio) で作る。"""
import asyncio, random, sys, time

LISTEN, TARGET, ONEWAY = int(sys.argv[1]), int(sys.argv[2]), float(sys.argv[3]) / 1000
JITTER = float(sys.argv[4]) / 1000 if len(sys.argv) > 4 else 0.0

async def pipe(r, w):
    loop = asyncio.get_running_loop()
    last = 0.0
    try:
        while data := await r.read(65536):
            at = max(last, loop.time() + ONEWAY + random.uniform(0, JITTER))   # 順序を崩さない
            last = at
            loop.call_at(at, w.write, data)
    finally:
        loop.call_later(ONEWAY + JITTER + 0.001, w.close)

async def handle(cr, cw):
    sr, sw = await asyncio.open_connection('127.0.0.1', TARGET)
    await asyncio.sleep(ONEWAY * 2)    # 接続確立の往復（SYN/ACK 相当）を 1 RTT 分だけ演出
    await asyncio.gather(pipe(cr, sw), pipe(sr, cw))

async def main():
    srv = await asyncio.start_server(handle, '127.0.0.1', LISTEN)
    async with srv: await srv.serve_forever()
asyncio.run(main())
