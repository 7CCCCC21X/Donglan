"""How long this machine takes from a pushed book's frame to the order's request handed to its thread (python tests/bench_hotpath.py).

The part of the race that is this process's own: the frame parsed, the book kept, the window looked at, the order sized,
signed (or found signed beforehand), journaled and its request started. The network either side of it is not in here —
`python -m ruletaker latency` measures that."""
from __future__ import annotations

import asyncio
import json
import os
import statistics
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))


def main() -> None:
    import datetime as dt
    from fakes import make_taker, market_of
    from ruletaker.stream import TOPIC, _Conn
    start = int(dt.datetime(2026, 11, 18, 22, 0, tzinfo=dt.timezone.utc).timestamp() * 1000)

    history = int(os.environ.get("BENCH_RECORDS", "600"))  # 90 days of windows, as a long-running service holds them

    async def one(presign: bool) -> float:
        taker, api, chain, stream, clock = make_taker(start - 3_600_000, journal=True,
                                                      RULE_FAST_HOURS="00:00-24:00" if presign else "off")
        for i in range(history):  # settled windows of the days before (what usd, spent_on and day_pnl walk)
            at = start - (i + 1) * 3 * 3_600_000
            taker.recs[f"old{i}"] = {"slug": f"old{i}", "mode": "on", "rule": "gold_shut", "side": "up", "start_ms": at, "pnl": 1.0,
                                     "settled_day": "2026-01-01", "orders": [{"state": "final", "shares": 50, "filled": 50, "limit": 0.58,
                                                                              "avg": 0.57, "at": at - 3_600_000, "hash": f"h{i}"}]}
        slug = f"xau-updown-15m-{start // 1000}"
        api.categories[slug], api.markets["1"] = "1", market_of("1", slug, start)
        taker.plan(taker.now())
        taker.planned_ms = taker.now()
        await taker.turn()
        if presign:
            taker.presign(taker.now())
        conn = _Conn(stream, 0)
        conn.subscribed.add("1")
        sent_at: list[float] = []
        original = api.create_order

        async def timed(body, digest):
            sent_at.append(time.perf_counter())
            return await original(body, digest)
        api.create_order = timed
        frame = json.dumps({"type": "M", "topic": TOPIC + "1", "data": {"version": 9, "updateTimestampMs": int(time.time() * 1000),
                                                                       "asks": [["0.55", "100"], ["0.56", "50"]], "bids": [["0.40", "10"]]}})
        began = time.perf_counter()
        await conn.handle(None, frame)
        await asyncio.gather(*taker.sends)
        return (sent_at[0] - began) * 1000

    for presign in (False, True):
        times = [asyncio.run(one(presign)) for _ in range(30)]
        print(f"{'预先签好' if presign else '当场签名'}（记录里已有 {history} 场）：推送帧到达 → 下单请求交出 中位 {statistics.median(times):.3f} ms，"
              f"最快 {min(times):.3f} ms，最慢 {max(times):.3f} ms（{len(times)} 次）")


if __name__ == "__main__":
    main()
