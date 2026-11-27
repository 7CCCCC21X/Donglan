"""The push connection, the kept-alive HTTP, the index feed, the store, the Telegram text and the status page (offline)."""
from __future__ import annotations

import asyncio
import contextlib
import http.client
import json
import os
import sqlite3
import tempfile
import threading
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

    def getheaders(self) -> list[tuple[str, str]]:
        return list(self.headers.items())

    def read(self, n: int = -1) -> bytes:
        out, self.body = self.body, b""
        return out

    def isclosed(self) -> bool:
        return not self.body  # (read whole)


class FakeConn:
    made: list["FakeConn"] = []
    script: list = []  # per connection: what request() and getresponse() do ("ok" / "send-fail" / "answer-fail")

    def __init__(self, host: str, port: int, timeout: float = 0, context: object = None) -> None:
        self.host, self.port, self.sock = host, port, None
        self.requests: list[tuple] = []
        self.mode = FakeConn.script.pop(0) if FakeConn.script else "ok"
        FakeConn.made.append(self)

    def connect(self) -> None:
        if self.mode == "connect-fail":
            raise ConnectionRefusedError("refused")

    def request(self, method: str, path: str, body: bytes | None = None, headers: dict | None = None) -> None:
        self.requests.append((method, path, body, headers))
        if self.mode == "send-fail":
            raise ConnectionResetError("closed")

    def getresponse(self) -> FakeResponse:
        if self.mode == "answer-fail":
            raise http.client.RemoteDisconnected("gone")
        if self.mode == "400":
            return FakeResponse(400, b'{"success":false,"message":"Order value too small"}')
        return FakeResponse(body=b'{"ok": 1}')

    def close(self) -> None:
        pass


class SecretsTest(unittest.TestCase):
    def test_a_printed_configuration_never_shows_a_secret(self) -> None:
        from ruletaker.config import Config
        key = "0x" + "ab12" * 16
        config = Config.from_env({"RULE_TAKER": "paper", "PREDICT_PRIVATE_KEY": key, "PREDICT_API_KEY": "api-key-secret-123",
                                  "TELEGRAM_BOT_TOKEN": "123456:telegram-secret-token", "STATUS_KEY": "status-secret-key"})
        shown = repr(config) + str(config) + config.secrets_line()
        for secret in (key[2:], "api-key-secret-123", "telegram-secret-token", "status-secret-key"):
            self.assertNotIn(secret, shown)
        self.assertIn("predict_private_key=True", config.secrets_line())


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

    def test_an_order_goes_out_on_a_connection_kept_ready_with_no_handshake_of_its_own(self) -> None:
        self.assertTrue(nethttp.prewarm("api.predict.fun", 443, 2.0))  # opened and parked: no request over it
        self.assertFalse(nethttp.prewarm("api.predict.fun", 443, 2.0))  # one that fresh there already
        self.assertEqual((len(FakeConn.made), FakeConn.made[0].requests), (1, []))
        self.assertEqual(nethttp.request("https://api.predict.fun/v1/orders", b"{}"), b'{"ok": 1}')
        self.assertEqual(len(FakeConn.made), 1)  # the POST took the parked one
        self.assertEqual([r[0] for r in FakeConn.made[0].requests], ["POST"])

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

    def test_a_post_that_got_no_connection_is_not_sent(self) -> None:
        FakeConn.script = ["connect-fail", "connect-fail"]
        with self.assertRaises(NotSent) as caught:  # not a byte of it written: an order that never left
            nethttp.request("https://api.predict.fun/v1/orders", b"{}")
        self.assertTrue(str(caught.exception).startswith(nethttp.NOT_CONNECTED), caught.exception)
        with self.assertRaises(RemoteError) as caught:  # a read: as before
            nethttp.request("https://api.predict.fun/v1/x")
        self.assertNotIsInstance(caught.exception, NotSent)

    def test_post_on_a_stale_connection_gets_a_fresh_one(self) -> None:
        nethttp.request("https://api.predict.fun/v1/x")
        conn, _ = nethttp._POOL[("api.predict.fun", 443)][-1]
        nethttp._POOL[("api.predict.fun", 443)][-1] = (conn, time.monotonic() - nethttp.KEEPALIVE_POST_IDLE_SECONDS - 1)
        nethttp.request("https://api.predict.fun/v1/orders", b"{}")
        self.assertEqual(len(FakeConn.made), 2)
        self.assertEqual(FakeConn.made[1].requests[0][0], "POST")


    def test_an_order_s_way_out_is_timed(self) -> None:
        trace: dict = {}
        before = int(time.time() * 1000)
        nethttp.request("https://api.predict.fun/v1/orders", b"{}", trace=trace)
        self.assertIn("connect_ms", trace)  # (no warm one: a handshake)
        self.assertGreaterEqual(trace["wrote_wall"], before)
        self.assertIn("started", trace)
        again: dict = {}
        nethttp.request("https://api.predict.fun/v1/orders", b"{}", trace=again)
        self.assertNotIn("connect_ms", again)  # (the warm one reused)
        self.assertGreaterEqual(again["wrote_wall"], before)

    def test_an_error_answer_read_whole_keeps_its_connection(self) -> None:
        FakeConn.script = ["400"]
        with self.assertRaises(RemoteError) as caught:
            nethttp.request("https://api.predict.fun/v1/orders", b"{}")
        self.assertIn("Order value too small", str(caught.exception))
        FakeConn.made[0].mode = "ok"
        self.assertEqual(nethttp.request("https://api.predict.fun/v1/x"), b'{"ok": 1}')
        self.assertEqual(len(FakeConn.made), 1)  # the same connection carried the next request

    def test_a_connection_the_server_let_go_is_never_written(self) -> None:
        import socket
        ours, theirs = socket.socketpair()
        self.addCleanup(ours.close)
        theirs.close()  # (the server hung up while it sat in the pool)
        gone = FakeConn("api.predict.fun", 443)
        gone.sock = ours
        nethttp._POOL[("api.predict.fun", 443)] = [(gone, time.monotonic())]
        dead = nethttp.HTTP_STATS["dead"]
        self.assertEqual(nethttp.request("https://api.predict.fun/v1/orders", b"{}"), b'{"ok": 1}')
        self.assertEqual((gone.requests, len(FakeConn.made[-1].requests)), ([], 1))  # the order went on a fresh one
        self.assertEqual(nethttp.HTTP_STATS["dead"], dead + 1)


# A throwaway CA and its "localhost" certificate (P-256, valid to 2126), for a TLS server on this machine alone (the Rust
# tests use the same pair).
TEST_CA = """-----BEGIN CERTIFICATE-----
MIIBnjCCAUWgAwIBAgIUOqfX6aXo6NlIiI/DvptlFIhyX8AwCgYIKoZIzj0EAwIw
HDEaMBgGA1UEAwwRcnVsZXRha2VyIHRlc3QgQ0EwIBcNMjYxMTIwMDYwMzE5WhgP
MjEyNjEwMjcwNjAzMTlaMBwxGjAYBgNVBAMMEXJ1bGV0YWtlciB0ZXN0IENBMFkw
EwYHKoZIzj0CAQYIKoZIzj0DAQcDQgAE/TgO2tFcIpiObsvu2LeLK3aDmIxodKHl
dxbZ1Xt361EpsH7nozeyK3UkZ583ZnFBjBR6+cMrLoHl7oN5K8PyDqNjMGEwHQYD
VR0OBBYEFPrSzeGNxhuXPgc3eXAVfcJa3gKfMB8GA1UdIwQYMBaAFPrSzeGNxhuX
Pgc3eXAVfcJa3gKfMA8GA1UdEwEB/wQFMAMBAf8wDgYDVR0PAQH/BAQDAgIEMAoG
CCqGSM49BAMCA0cAMEQCIB/IFr5n3t9cDDMD9NAFADRRORHd7fJh9xXdIYhKt4uo
AiAXQoqBxdZ2fkVarobJM3UVSJBVCOmjnE55H+iX0IMtxA==
-----END CERTIFICATE-----"""
TEST_LEAF = """-----BEGIN CERTIFICATE-----
MIIBvjCCAWSgAwIBAgIUQm5Da2Rjc+E1VJICNND50UBi67UwCgYIKoZIzj0EAwIw
HDEaMBgGA1UEAwwRcnVsZXRha2VyIHRlc3QgQ0EwIBcNMjYxMTIwMDYwMzIwWhgP
MjEyNjEwMjcwNjAzMjBaMBQxEjAQBgNVBAMMCWxvY2FsaG9zdDBZMBMGByqGSM49
AgEGCCqGSM49AwEHA0IABALYsYucdVHYBFFJXS9LHP2O1Mm4KFeQfiq5/0l273H2
5fZEsXC8Nlv5piHCFNh1pTelRddDNyazSuol9UqmjlKjgYkwgYYwFAYDVR0RBA0w
C4IJbG9jYWxob3N0MAkGA1UdEwQCMAAwDgYDVR0PAQH/BAQDAgeAMBMGA1UdJQQM
MAoGCCsGAQUFBwMBMB0GA1UdDgQWBBTbb1YBSPPUQ29NVnOswtnhTRfzvzAfBgNV
HSMEGDAWgBT60s3hjcYblz4HN3lwFX3CWt4CnzAKBggqhkjOPQQDAgNIADBFAiEA
mAh6FOKPfIgRbiy6DyVU2OGedMKZCjcpOrhvgEmFgf8CIDouLX/N6Gejt0V/kN3N
WjuUKhV0CK7XVYafsCk/NzIt
-----END CERTIFICATE-----"""
TEST_LEAF_KEY = """-----BEGIN PRIVATE KEY-----
MIGHAgEAMBMGByqGSM49AgEGCCqGSM49AwEHBG0wawIBAQQg1l5p9ecwhLZ8+qiE
ugon6QeKhziJ6kufs07JYpM//KihRANCAAQC2LGLnHVR2ARRSV0vSxz9jtTJuChX
kH4quf9Jdu9x9uX2RLFwvDZb+aYhwhTYdaU3pUXXQzcms0rqJfVKpo5S
-----END PRIVATE KEY-----"""


class PooledConnectionTest(unittest.TestCase):
    """A pooled connection judged by what the server sent while it sat there (a real TLS 1.3 server here: its session tickets
    come after the client's side of the handshake is done)."""

    def serve(self, then: str) -> int:
        import socket
        import ssl
        import tempfile
        import threading
        folder = tempfile.mkdtemp()
        self.addCleanup(lambda: __import__("shutil").rmtree(folder, ignore_errors=True))
        cert, key = os.path.join(folder, "leaf.pem"), os.path.join(folder, "leaf.key")
        with open(cert, "w") as f:
            f.write(TEST_LEAF + "\n")
        with open(key, "w") as f:
            f.write(TEST_LEAF_KEY + "\n")
        context = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
        context.minimum_version = ssl.TLSVersion.TLSv1_3
        context.load_cert_chain(cert, key)
        listener = socket.create_server(("127.0.0.1", 0))
        done = threading.Event()
        self.addCleanup(done.set)

        def run() -> None:
            raw, _ = listener.accept()
            with context.wrap_socket(raw, server_side=True) as tls:
                if then == "close_notify":
                    tls.settimeout(0.3)
                    with contextlib.suppress(OSError):
                        tls.unwrap()  # (sends its close_notify; the TCP connection left open)
                elif then == "data":
                    tls.sendall(b"HTTP/1.1 408 Request Timeout\r\nContent-Length: 0\r\n\r\n")
                done.wait(5)
            listener.close()
        threading.Thread(target=run, daemon=True).start()
        return listener.getsockname()[1]

    def pooled(self, then: str) -> object:
        import socket
        import ssl
        port = self.serve(then)
        tls = ssl.create_default_context(cadata=TEST_CA).wrap_socket(socket.create_connection(("127.0.0.1", port)),
                                                                     server_hostname="localhost")
        self.addCleanup(tls.close)
        time.sleep(0.2)  # (what the server sent is in)
        return type("Pooled", (), {"sock": tls})()

    def test_tls_records_of_its_own_leave_it_good_the_end_or_data_do_not(self) -> None:
        good = self.pooled("")
        self.assertFalse(nethttp._closed(good))  # the session tickets read, nothing behind them
        self.assertFalse(nethttp._closed(good))
        self.assertTrue(nethttp._closed(self.pooled("close_notify")))
        self.assertTrue(nethttp._closed(self.pooled("data")))

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

    def test_a_start_line_that_never_reads_is_kept_as_a_few_errors_only(self) -> None:
        async def go() -> dict:
            async def getter(path: str, params: dict) -> list:
                raise RemoteError("HTTP 503: down")
            feed = IndexFeed(["XAUUSDT"], getter=getter)
            start = 1_795_039_200_000
            for i in range(450):
                at = start + i * 900_000
                self.assertIsNone(await feed.ensure_start("XAUUSDT", at, at + 2_000))
            return feed.candle_error
        errors = asyncio.run(go())
        self.assertEqual(len(errors), 400)
        self.assertTrue(all(minute >= 1_795_039_200_000 + 50 * 900_000 - 60_000 for _, minute in errors))  # (the oldest let go)

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


    def test_the_writer_s_first_open_is_tried_again_and_said_meanwhile(self) -> None:
        folder = tempfile.mkdtemp()
        path = os.path.join(folder, "s.sqlite3")
        opened: list = []

        class Slow(Store):
            CONNECT_RETRY_S = 0.05

            def connect(self):  # (the reader's open in __init__ passes; the writer's first two are refused — a lock held)
                opened.append(threading.current_thread().name)
                if opened.count("store-writer") <= 2 and threading.current_thread().name == "store-writer":
                    raise sqlite3.OperationalError("database is locked")
                return super().connect()
        s = Slow(path)
        s.put("rule:x", {"a": 1})
        self.assertTrue(s.flush(5.0), s.error)  # (written once the writer got its connection)
        self.assertEqual(s.error, "")
        self.assertEqual(opened.count("store-writer"), 3)
        self.assertTrue(s.writer.is_alive())
        self.assertEqual(Store(path).get("rule:x"), {"a": 1})


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
