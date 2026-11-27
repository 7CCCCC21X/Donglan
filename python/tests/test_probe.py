"""The region probe (python -m ruletaker latency, rt.sh probe) against stand-ins of Predict's API and push connection: it times
every change of the pushed books (never the first book of a market: that is the book as it stands) and answers the
heartbeat, so the connection lasts the whole watch."""
from __future__ import annotations

import asyncio
import http.server
import json
import os
import threading
import time
import unittest

from fakes import make_config


# The stand-in book: the state Predict has now (its number, when it changed); the push of each state goes out 40 ms after it
# changed, so a REST read in between shows it first.
STATE = {"k": -1, "stamp": 0, "hits": 0}


def book(k: int, stamp: int) -> dict:
    return {"updateTimestampMs": stamp, "bids": [["0.40", "10"]], "asks": [[f"0.{50 + k % 40}", "5"]]}


class _Api(http.server.BaseHTTPRequestHandler):
    def log_message(self, *args) -> None:
        pass

    def do_GET(self) -> None:
        STATE["hits"] += 1
        if "/categories/" in self.path:
            quarter = int(self.path.rsplit("-", 1)[1])
            mid = f"m{quarter % 1800 // 900}"  # (this quarter hour's market, and the next one's)
            data = {"markets": [{"id": mid, "conditionId": "0x01", "title": "BTC Up or Down", "question": ""}]}
        else:
            data = book(STATE["k"], STATE["stamp"])
        body = json.dumps({"success": True, "data": data}).encode()
        self.send_response(200)
        self.send_header("Content-Length", str(len(body)))
        self.send_header("X-RateLimit-Limit", "600")
        # (the key's minute as Predict counts it: 500 of 600 gone — far more than the probe's own reads: another program's)
        self.send_header("X-RateLimit-Remaining", "100")
        self.end_headers()
        self.wfile.write(body)


class ProbeTest(unittest.TestCase):
    def test_the_probe_times_each_pushed_change_and_answers_the_heartbeat(self) -> None:
        from websockets.asyncio.server import serve
        from ruletaker.__main__ import latency
        api = http.server.HTTPServer(("127.0.0.1", 0), _Api)
        threading.Thread(target=api.serve_forever, daemon=True).start()
        self.addCleanup(api.server_close)
        self.addCleanup(api.shutdown)
        proxies = {k: os.environ.pop(k) for k in ("HTTPS_PROXY", "https_proxy", "HTTP_PROXY", "http_proxy") if k in os.environ}
        self.addCleanup(os.environ.update, proxies)
        echoed: list = []

        async def pushes(ws) -> None:
            topics: list = []
            async for raw in ws:  # both subscriptions: each answered, its book as it stands (5 s old), a heartbeat
                msg = json.loads(raw)
                if msg.get("method") == "subscribe":
                    topic = msg["params"][0]
                    await ws.send(json.dumps({"type": "R", "requestId": msg["requestId"], "success": True}))
                    await ws.send(json.dumps({"type": "M", "topic": topic, "data": book(-1, int(time.time() * 1000) - 5000)}))
                    await ws.send(json.dumps({"type": "M", "topic": "heartbeat", "data": 7}))
                    topics.append(topic)
                    if len(topics) == 2:
                        break
            for k in range(6):  # six changes: each pushed to both 40 ms after Predict has it
                stamp = int(time.time() * 1000)
                STATE.update(k=k, stamp=stamp)
                await asyncio.sleep(0.04)
                for topic in topics:
                    await ws.send(json.dumps({"type": "M", "topic": topic, "data": book(k, stamp)}))
                await asyncio.sleep(0.06)
            async for raw in ws:
                msg = json.loads(raw)
                if msg.get("method") == "heartbeat":
                    echoed.append(msg.get("data"))

        async def index(ws) -> None:  # Binance's index: a markPrice push every 50 ms, each 3 ms after its own event time
            while True:
                frame = {"stream": "xauusdt@markPrice@1s",
                         "data": {"e": "markPriceUpdate", "E": int(time.time() * 1000) - 3, "s": "XAUUSDT", "i": "4000.5"}}
                try:
                    await ws.send(json.dumps(frame))
                except Exception:
                    return
                await asyncio.sleep(0.05)

        async def go() -> dict:
            async with serve(pushes, "127.0.0.1", 0) as server, serve(index, "127.0.0.1", 0) as binance:
                port, binance_port = server.sockets[0].getsockname()[1], binance.sockets[0].getsockname()[1]
                config = make_config(RULE_TAKER="off", RULE_REGION="test", BINANCE_WS_URL=f"ws://127.0.0.1:{binance_port}",
                                     PREDICT_RATE_PER_MIN="6000")  # (REST read every 0.1 s)
                return await latency(config, rounds=2, push_seconds=1.0, say=said.append,
                                     base=f"http://127.0.0.1:{api.server_address[1]}/v1", ws_url=f"ws://127.0.0.1:{port}")
        said: list[str] = []
        found = asyncio.run(go())
        self.assertEqual(len(found["reads"]), 2)
        lags = found["lags"]
        self.assertEqual(len(lags), 12, lags)  # two markets × six changes; the books as they stood (5 s old) not counted
        self.assertTrue(all(35 <= lag < 400 for lag in lags), lags)
        # REST beside the pushes: the changes it caught in their 40 ms are ahead by at most that; the others behind
        self.assertGreaterEqual(len(found["rest_ahead"]) + len(found["push_ahead"]), 3, found)
        self.assertTrue(found["rest_ahead"] and all(lead <= 60 for lead in found["rest_ahead"]), found)
        self.assertTrue(found["rest_ages"] and all(0 <= a < 200 for a in found["rest_ages"]), found)
        from ruletaker import http as nethttp
        self.assertEqual(nethttp.HTTP_NET["127.0.0.1"]["limits"], "x-ratelimit-limit: 600; x-ratelimit-remaining: 100")
        # the key shared: said, with this measurement's own count, and the denser measurement held back
        text, hits = "\n".join(said), STATE["hits"]
        self.assertIn(f"用了 500 次（上限 600），我们这一分钟发了 {hits} 次：另外约 {500 - hits} 次是别的程序", text)  # (as the Rust version says it)
        self.assertIn("先别跑 rt.sh latency 4000", text)
        self.assertEqual(echoed, [7, 7])
        self.assertGreaterEqual(len(found["index"]), 5, found["index"])  # (the index watched over the same seconds)
        self.assertTrue(all(0 <= lag < 200 for lag in found["index"]), found["index"])


class _Limited(http.server.BaseHTTPRequestHandler):
    def log_message(self, *args) -> None:
        pass

    def do_GET(self) -> None:
        body = b'{"success":false,"message":"Rate limit exceeded"}'
        self.send_response(429)
        self.send_header("Content-Length", str(len(body)))
        self.send_header("Retry-After", "30")
        self.end_headers()
        self.wfile.write(body)


class ProbeLimitedTest(unittest.TestCase):
    def test_the_probe_counts_the_429s_of_its_own_requests(self) -> None:
        from ruletaker.__main__ import latency
        api = http.server.HTTPServer(("127.0.0.1", 0), _Limited)
        threading.Thread(target=api.serve_forever, daemon=True).start()
        try:
            config = make_config(RULE_TAKER="off", RULE_REGION="test", BINANCE_WS_URL="ws://127.0.0.1:9")
            found = asyncio.run(latency(config, rounds=2, push_seconds=0.2, say=lambda line: None,
                                        base=f"http://127.0.0.1:{api.server_address[1]}/v1", ws_url="ws://127.0.0.1:9"))
        finally:
            api.shutdown()
        self.assertGreaterEqual(found["limited"], 3, found)  # (the markets' read and the two rounds)


class VerifyPaceTest(unittest.TestCase):
    def test_the_rest_reads_at_the_moment_keep_within_the_key_s_budget(self) -> None:
        from fakes import make_taker

        def every(**env: str) -> float:
            return make_taker(0, **env)[0].verify_every()
        self.assertAlmostEqual(every(), 60 / (0.8 * 220))  # three a second on the standard key, as before
        self.assertAlmostEqual(every(RULE_VERIFY_MS="100"), 60 / (0.8 * 220))  # (asked faster: the budget holds it)
        self.assertAlmostEqual(every(RULE_VERIFY_MS="100", PREDICT_RATE_PER_MIN="1100"), 0.1)
        self.assertAlmostEqual(every(PREDICT_RATE_PER_MIN="1100"), 0.34)  # (a bigger budget alone changes nothing)


if __name__ == "__main__":
    unittest.main()
