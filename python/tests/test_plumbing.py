"""The push connection, the kept-alive HTTP, the index feed, the store, the Telegram text and the status page (offline)."""
from __future__ import annotations

import asyncio
import http.client
import json
import os
import tempfile
import time
import unittest
from decimal import Decimal as D

import fakes  # noqa: F401
from ruletaker import http as nethttp
from ruletaker.index import IndexFeed
from ruletaker.notify import chunks
from ruletaker.store import Store
from ruletaker.stream import TOPIC, BookStream
from ruletaker.util import NotSent, RemoteError


class FakeWs:
    def __init__(self, frames: list) -> None:
        self.inbox: asyncio.Queue = asyncio.Queue()
        for f in frames:
            self.inbox.put_nowait(f)
        self.sent: list[dict] = []
        self.closed = False

    async def send(self, text: str) -> None:
        self.sent.append(json.loads(text))

    async def recv(self) -> str:
        return await self.inbox.get()

    async def close(self) -> None:
        self.closed = True


class StreamTest(unittest.TestCase):
    def test_subscribe_take_heartbeat_and_versions(self) -> None:
        async def go() -> None:
            ws = FakeWs([])
            told: list[str] = []

            async def connect(url: str, headers: dict) -> FakeWs:
                return ws
            s = BookStream("key", 1, connect=connect)
            s.on_change = lambda mid: told.append(mid) or True
            s.want({"9"})
            task = asyncio.create_task(s.run())
            await asyncio.sleep(0.05)
            self.assertEqual(ws.sent[0], {"method": "subscribe", "requestId": 1, "params": [TOPIC + "9"]})
            ws.inbox.put_nowait(json.dumps({"type": "R", "requestId": 1, "success": True}))
            frame = {"type": "M", "topic": TOPIC + "9", "data": {"version": 5, "updateTimestampMs": int(time.time() * 1000) - 20,
                                                                   "asks": [["0.55", "10"]], "bids": [["0.40", "3"]]}}
            ws.inbox.put_nowait(json.dumps(frame))
            await asyncio.sleep(0.02)
            self.assertEqual(told, ["9"])
            self.assertEqual(s.book("9"), (((D("0.40"), D("3")),), ((D("0.55"), D("10")),)))
            self.assertEqual(len(s.lags), 0)  # (the first book of a market: no change to time)
            old = {**frame, "data": {**frame["data"], "version": 4, "asks": [["0.50", "99"]]}}
            ws.inbox.put_nowait(json.dumps(old))  # an older version: dropped
            ws.inbox.put_nowait(json.dumps({"type": "M", "topic": "heartbeat", "data": 123}))
            await asyncio.sleep(0.02)
            self.assertEqual(s.book("9")[1], ((D("0.55"), D("10")),))
            self.assertEqual(ws.sent[-1], {"method": "heartbeat", "data": 123})
            s.want(set())
            await asyncio.sleep(0.05)
            self.assertEqual(ws.sent[-1]["method"], "unsubscribe")
            s.stop()
            await asyncio.wait_for(task, 3)
        asyncio.run(go())

    def test_merge_keeps_the_newer(self) -> None:
        s = BookStream("", 1)
        s.wanted = {"1"}
        asked = time.monotonic()
        s.take("1", {"version": 1, "asks": [["0.5", "1"]], "bids": []})
        self.assertFalse(s.merge("1", (), (), asked))  # asked before the push: the push stands
        self.assertEqual(s.books["1"][1], ((D("0.5"), D("1")),))
        self.assertTrue(s.merge("1", (), ((D("0.6"), D("2")),), time.monotonic()))
        self.assertEqual(s.books["1"][1], ((D("0.6"), D("2")),))


class FakeResponse:
    def __init__(self, status: int = 200, body: bytes = b"{}", close: bool = False) -> None:
        self.status, self.body, self.will_close = status, body, close
        self.headers = {"cf-ray": "abc-NRT"}

    def getheader(self, name: str, default: str | None = None) -> str | None:
        return self.headers.get(name.lower(), default)

    def read(self, n: int = -1) -> bytes:
        out, self.body = self.body, b""
        return out


class FakeConn:
    made: list["FakeConn"] = []
    script: list = []  # per connection: what request() and getresponse() do ("ok" / "send-fail" / "answer-fail")

    def __init__(self, host: str, port: int, timeout: float = 0, context: object = None) -> None:
        self.host, self.port, self.sock = host, port, None
        self.requests: list[tuple] = []
        self.mode = FakeConn.script.pop(0) if FakeConn.script else "ok"
        FakeConn.made.append(self)

    def connect(self) -> None:
        pass

    def request(self, method: str, path: str, body: bytes | None = None, headers: dict | None = None) -> None:
        self.requests.append((method, path, body, headers))
        if self.mode == "send-fail":
            raise ConnectionResetError("closed")

    def getresponse(self) -> FakeResponse:
        if self.mode == "answer-fail":
            raise http.client.RemoteDisconnected("gone")
        return FakeResponse(body=b'{"ok": 1}')

    def close(self) -> None:
        pass


class HttpTest(unittest.TestCase):
    def setUp(self) -> None:
        self.saved = nethttp.HTTPS_CONNECTION
        nethttp.HTTPS_CONNECTION = FakeConn
        nethttp._POOL.clear()
        FakeConn.made, FakeConn.script = [], []
        self.proxy = {k: os.environ.pop(k) for k in ("HTTPS_PROXY", "https_proxy", "HTTP_PROXY", "http_proxy") if k in os.environ}

    def tearDown(self) -> None:
        nethttp.HTTPS_CONNECTION = self.saved
        nethttp._POOL.clear()
        os.environ.update(self.proxy)

    def test_reuse_and_post_never_sent_twice(self) -> None:
        self.assertEqual(nethttp.request("https://api.predict.fun/v1/x"), b'{"ok": 1}')
        self.assertEqual(nethttp.request("https://api.predict.fun/v1/x"), b'{"ok": 1}')
        self.assertEqual(len(FakeConn.made), 1)  # kept alive and reused
        self.assertEqual(nethttp.HTTP_NET["api.predict.fun"]["edge"], "abc-NRT")
        # a reused connection that swallows a POST's answer: an error (result unknown), never a second send
        FakeConn.made[0].mode = "answer-fail"
        with self.assertRaises(RemoteError):
            nethttp.request("https://api.predict.fun/v1/orders", b"{}")
        self.assertEqual(sum(1 for c in FakeConn.made for r in c.requests if r[0] == "POST"), 1)

    def test_get_retried_on_a_closed_connection(self) -> None:
        nethttp.request("https://api.predict.fun/v1/x")
        FakeConn.made[0].mode = "answer-fail"
        self.assertEqual(nethttp.request("https://api.predict.fun/v1/x"), b'{"ok": 1}')  # again, on a fresh one
        self.assertEqual(len(FakeConn.made), 2)

    def test_an_order_is_judged_again_after_a_new_connection(self) -> None:
        # no warm connection: the handshake, then the order's gate once more — a pause meanwhile holds it, nothing written
        with self.assertRaises(NotSent) as caught:
            nethttp.request("https://api.predict.fun/v1/orders", b"{}", gate=lambda: "已暂停（握手时按的）")
        self.assertIn("已暂停", str(caught.exception))
        self.assertEqual((len(FakeConn.made), FakeConn.made[0].requests), (1, []))
        # the connection, untouched, kept: a warm one is written at once (no wait, so no second judgment)
        judged: list = []
        self.assertEqual(nethttp.request("https://api.predict.fun/v1/orders", b"{}", gate=lambda: judged.append(1) or ""), b'{"ok": 1}')
        self.assertEqual((len(FakeConn.made), judged), (1, []))
        # a dead connection replaced by a fresh one: judged again before the fresh one is written
        FakeConn.made[0].mode = "send-fail"
        with self.assertRaises(NotSent):
            nethttp.request("https://api.predict.fun/v1/orders", b"{}", gate=lambda: "服务正在停止，不再发单")
        self.assertEqual((len(FakeConn.made), FakeConn.made[1].requests), (2, []))
        # a dead pooled connection over a warm one: the warm one is written only after the order is judged again
        warm, dead = FakeConn("api.predict.fun", 443), FakeConn("api.predict.fun", 443)
        dead.mode = "send-fail"
        nethttp._POOL[("api.predict.fun", 443)] = [(warm, time.monotonic()), (dead, time.monotonic())]
        with self.assertRaises(NotSent):
            nethttp.request("https://api.predict.fun/v1/orders", b"{}", gate=lambda: "已暂停（换连接时按的）")
        self.assertEqual((len(dead.requests), warm.requests), (1, []))
        # a GET has no gate: as before
        self.assertEqual(nethttp.request("https://api.predict.fun/v1/x"), b'{"ok": 1}')

    def test_post_on_a_stale_connection_gets_a_fresh_one(self) -> None:
        nethttp.request("https://api.predict.fun/v1/x")
        conn, _ = nethttp._POOL[("api.predict.fun", 443)][-1]
        nethttp._POOL[("api.predict.fun", 443)][-1] = (conn, time.monotonic() - nethttp.KEEPALIVE_POST_IDLE_SECONDS - 1)
        nethttp.request("https://api.predict.fun/v1/orders", b"{}")
        self.assertEqual(len(FakeConn.made), 2)
        self.assertEqual(FakeConn.made[1].requests[0][0], "POST")


class IndexTest(unittest.TestCase):
    def test_push_and_staleness(self) -> None:
        feed = IndexFeed(["XAUUSDT"])
        seen: list = []
        feed.on_change = lambda s, at: seen.append((s, at))
        now = int(time.time() * 1000)
        feed.take(json.dumps({"stream": "xauusdt@markPrice@1s", "data": {"e": "markPriceUpdate", "E": now - 200, "s": "XAUUSDT",
                                                                          "p": "4001", "i": "4000.5"}}))
        self.assertEqual(feed.price("XAUUSDT"), D("4000.5"))
        self.assertEqual(seen, [("XAUUSDT", now - 200)])
        feed.take(json.dumps({"e": "markPriceUpdate", "E": now - 500, "s": "XAUUSDT", "i": "3990"}))  # older: dropped
        self.assertEqual(feed.price("XAUUSDT"), D("4000.5"))
        end = now + 10 * 60_000
        self.assertEqual(feed.price_problem("XAUUSDT", end, now), "")
        self.assertIn("未更新", feed.price_problem("XAUUSDT", end, now + 20_000))  # 12 s allowed with 10 minutes left

    def test_start_candle(self) -> None:
        async def go() -> None:
            start = 1_795_039_200_000

            async def getter(path: str, params: dict) -> list:
                self.assertEqual(path, "indexPriceKlines")
                o = params["startTime"]
                return [[o, "1", "2", "0.5", "4000.25", "0", o + 59_999]]
            feed = IndexFeed(["XAUUSDT"], getter=getter)
            self.assertIsNone(await feed.ensure_start("XAUUSDT", start, start + 500))  # its minute not read before the grace
            self.assertEqual(await feed.ensure_start("XAUUSDT", start, start + 2_000), D("4000.25"))
            self.assertEqual(feed.close_of("XAUUSDT", start), D("4000.25"))
        asyncio.run(go())


class StoreTest(unittest.TestCase):
    def test_write_behind_and_reload(self) -> None:
        folder = tempfile.mkdtemp()
        path = os.path.join(folder, "s.sqlite3")
        s = Store(path)
        rec = {"a": 1, "orders": [{"x": 2.5}]}
        s.put("rule:x", rec)
        rec["a"] = 99  # changed after put: the write has the value as it was
        s.put("other", "text")
        self.assertTrue(s.flush())
        again = Store(path)
        self.assertEqual(again.get("rule:x"), {"a": 1, "orders": [{"x": 2.5}]})
        self.assertEqual(dict(again.items("rule:")), {"rule:x": {"a": 1, "orders": [{"x": 2.5}]}})
        again.delete_keys(["rule:x"])
        again.flush()
        self.assertIsNone(Store(path).get("rule:x"))


class TextTest(unittest.TestCase):
    def test_chunks(self) -> None:
        text = "\n".join("x" * 100 for _ in range(100))
        parts = chunks(text, 1000)
        self.assertTrue(all(len(p) <= 1000 for p in parts))
        self.assertEqual("\n".join(parts), text)


class StatusTest(unittest.TestCase):
    def test_routes_and_key(self) -> None:
        from ruletaker.status import StatusServer

        class T:
            def healthy(self) -> bool:
                return True

            def status(self) -> str:
                return "状态"

            def summary(self) -> dict:
                return {"mode": "paper"}

            async def control(self, what: str, arg: str) -> str:
                return f"{what}:{arg}"

        async def go() -> None:
            server = StatusServer(T(), "127.0.0.1", 0, key="k" * 12)
            self.assertEqual(await server.route("GET", "/healthz", {}, {}), (200, "text/plain; charset=utf-8", "ok"))
            self.assertEqual((await server.route("GET", "/status", {}, {}))[0], 403)
            self.assertEqual((await server.route("GET", "/status", {"key": ["k" * 12]}, {}))[2], "状态")
            self.assertEqual((await server.route("POST", "/pause", {}, {"x-status-key": "k" * 12}))[0], 403)  # no X-RT: refused
            self.assertEqual((await server.route("POST", "/pause", {}, {"x-status-key": "k" * 12, "x-rt": "1"}))[2], "pause:状态页")
            self.assertEqual((await server.route("GET", "/pause", {"key": ["k" * 12]}, {}))[0], 405)
        asyncio.run(go())


if __name__ == "__main__":
    unittest.main()


class IndexWantedTest(unittest.TestCase):
    def test_connected_only_while_an_in_play_price_is_set(self) -> None:
        async def go() -> None:
            opened: list = []

            class Ws:
                def __init__(self) -> None:
                    self.closed = False

                async def recv(self) -> str:
                    await asyncio.sleep(0.01)
                    return json.dumps({"data": {"e": "markPriceUpdate", "E": int(time.time() * 1000), "s": "XAUUSDT", "i": "4000.5"}})

                async def close(self) -> None:
                    self.closed = True

            async def connect(url: str, headers: dict) -> Ws:
                opened.append(Ws())
                return opened[-1]
            feed = IndexFeed(["XAUUSDT"], connect=connect)
            want = {"on": False}
            feed.wanted = lambda: want["on"]
            feed.needed = lambda: False
            task = asyncio.create_task(feed.run())
            await asyncio.sleep(0.1)
            self.assertEqual(opened, [])  # no price set: no connection made
            want["on"] = True              # set on the control page while running...
            for _ in range(400):
                await asyncio.sleep(0.01)
                if feed.price("XAUUSDT") is not None:
                    break
            self.assertEqual(feed.price("XAUUSDT"), D("4000.5"))  # ...connected, the index in hand
            want["on"] = False
            for _ in range(200):
                await asyncio.sleep(0.01)
                if opened[0].closed:
                    break
            self.assertTrue(opened[0].closed)  # the price taken away again: the connection let go
            self.assertIsNone(feed.ws)
            feed.stop()
            await asyncio.wait_for(task, 5)
        asyncio.run(go())
