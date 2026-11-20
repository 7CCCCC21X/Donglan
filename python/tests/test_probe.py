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


class _Api(http.server.BaseHTTPRequestHandler):
    def log_message(self, *args) -> None:
        pass

    def do_GET(self) -> None:
        if "/categories/" in self.path:
            quarter = int(self.path.rsplit("-", 1)[1])
            mid = f"m{quarter % 1800 // 900}"  # (this quarter hour's market, and the next one's)
            data = {"markets": [{"id": mid, "conditionId": "0x01", "title": "BTC Up or Down", "question": ""}]}
        else:
            data = {"bids": [], "asks": []}
        body = json.dumps({"success": True, "data": data}).encode()
        self.send_response(200)
        self.send_header("Content-Length", str(len(body)))
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
            async for raw in ws:
                msg = json.loads(raw)
                if msg.get("method") == "heartbeat":
                    echoed.append(msg.get("data"))
                elif msg.get("method") == "subscribe":
                    topic = msg["params"][0]
                    await ws.send(json.dumps({"type": "R", "requestId": msg["requestId"], "success": True}))
                    await ws.send(json.dumps({"type": "M", "topic": topic, "data": {"updateTimestampMs": time.time() * 1000 - 5000}}))
                    await ws.send(json.dumps({"type": "M", "topic": "heartbeat", "data": 7}))
                    for _ in range(4):  # four changes, each pushed 40 ms after Predict's own stamp
                        await asyncio.sleep(0.02)
                        await ws.send(json.dumps({"type": "M", "topic": topic, "data": {"updateTimestampMs": time.time() * 1000 - 40}}))

        async def go() -> dict:
            async with serve(pushes, "127.0.0.1", 0) as server:
                port = server.sockets[0].getsockname()[1]
                return await latency(make_config(RULE_TAKER="off", RULE_REGION="test"), rounds=2, push_seconds=0.8,
                                     base=f"http://127.0.0.1:{api.server_address[1]}/v1", ws_url=f"ws://127.0.0.1:{port}")
        found = asyncio.run(go())
        self.assertEqual(len(found["reads"]), 2)
        lags = found["lags"]
        self.assertEqual(len(lags), 8, lags)  # two markets × four changes; the books as they stood (5 s old) not counted
        self.assertTrue(all(35 <= lag < 400 for lag in lags), lags)
        self.assertEqual(echoed, [7, 7])


if __name__ == "__main__":
    unittest.main()
