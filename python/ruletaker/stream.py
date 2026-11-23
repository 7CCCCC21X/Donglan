"""Predict's WebSocket for the rule taker's windows (a port of huice rules.RuleStream): each listed window's book pushed the
moment it changes (predictOrderbook/<market id>), so an offer at a rule's price is bought at once — the taker is told of the
change in the very turn of the loop that brought it (on_change).

Protocol (dev.predict.fun): JSON text frames; the client sends {"method": "subscribe" | "unsubscribe", "requestId": n,
"params": [topic]}, answered by {"type": "R", "requestId": n, "success": bool, "error"}; the server pushes {"type": "M",
"topic", "data"}, a book as {"version", "marketId", "updateTimestampMs", "bids", "asks": [[price, size], ...]} (an older
version than the one in hand is dropped); its heartbeat (topic "heartbeat", every 15 s) is echoed back.

RULE_WS_CONNECTIONS (1–3, default 1): that many connections, each subscribed to every window; a book is taken from whichever
brings it first (the version decides between them) — a second connection can only make a push arrive sooner."""
from __future__ import annotations

import asyncio
import collections
import contextlib
import statistics
import time
from typing import Any, Callable

from .predict import predict_levels
from .util import LOG, clean_error, dumps, loads

PREDICT_WS_URL = "wss://ws.predict.fun/ws"
TOPIC = "predictOrderbook/"
HEARTBEAT = "heartbeat"


async def ws_connect(url: str, headers: dict[str, str]) -> Any:
    """A WebSocket connection (the websockets package): no compression (a frame is read the moment it lands), the library's
    own ping every 20 s."""
    try:
        from websockets.asyncio.client import connect
    except ImportError as error:  # pragma: no cover - depends on the environment
        raise RuntimeError("websockets 包未安装：pip install -r requirements.txt") from error
    return await connect(url, additional_headers=headers, open_timeout=15, close_timeout=1, ping_interval=20, ping_timeout=20,
                         max_size=4 * 1024 * 1024, compression=None)


class BookStream:
    FRESH_S = 20.0               # a pushed book stands for this long while nothing is heard of it...
    ALIVE_S = 25.0               # ...and the connection vouches for the books it holds while a frame (the heartbeat: 15 s) is this recent
    SILENT_S = 45.0              # no frame for this long: the connection is made anew
    BACKOFF = (2.0, 120.0)
    SYNC_S = 30.0

    def __init__(self, key: str = "", connections: int = 1, connect: Any = None, url: str = PREDICT_WS_URL):
        self.key, self.url = key, url
        self.connect = connect or ws_connect
        self.count = max(1, int(connections))
        self.wanted: set[str] = set()
        self.books: dict[str, tuple] = {}     # market id -> (bids, asks, monotonic time of the push)
        # market id -> (version, the server's update time — updateTimestampMs, 0 when absent) of the book applied last: shared
        # by the connections — a frame older than it is dropped, whichever connection brings it
        self.versions: dict[str, tuple[int, int]] = {}
        # market id -> [the version brought last, when (monotonic), by which connection, the connections that have brought it
        # (a bit each)]: the copies the other connections bring later say what the extra connections save (RULE_WS_CONNECTIONS)
        self.arrived: dict[str, list] = {}
        self.changed: set[str] = set()       # markets whose book changed since the taker last looked
        self.arrivals = 0
        self.pushed = asyncio.Event()
        self.on_change: Callable[[str], bool] | None = None  # told of a change on the spot; True: it took everything there was
        self.stamps: dict[str, tuple[int, int]] = {}  # market id -> (Predict's stamp on the book in hand — 0 for a REST read —, its arrival, wall ms)
        self.lags: collections.deque = collections.deque(maxlen=300)
        self.conns: list[_Conn] = [_Conn(self, i) for i in range(self.count)]
        self.stopping = asyncio.Event()

    # -- what the taker asks --
    def want(self, ids: set[str]) -> None:
        if ids != self.wanted:
            self.wanted = set(ids)
            for c in self.conns:
                c.wake.set()

    def alive(self) -> bool:
        return any(c.alive() for c in self.conns)

    @property
    def connected(self) -> bool:
        return any(c.ws is not None for c in self.conns)

    @property
    def error(self) -> str:
        return next((c.error for c in self.conns if c.error), "")

    @property
    def connects(self) -> int:
        return sum(c.connects for c in self.conns)

    def book(self, mid: str) -> tuple | None:
        """(bids, asks) as last pushed while a connection that has it lives, None when there is none fresh."""
        got = self.books.get(mid)
        if got is None or not self.connected or time.monotonic() - got[2] > self.FRESH_S:
            return None
        return got[0], got[1]

    def held(self, mid: str) -> tuple | None:
        return self.books.get(mid)

    def nudge(self, *mids: str) -> None:
        """The taker looks at these markets now though their books have not changed (matching just seen open with an offer in
        hand; the index a window's course is judged by just moved)."""
        for mid in mids:
            self.changed.add(mid)
            self.arrivals += 1
            self.told(mid)

    def told(self, mid: str) -> None:
        if not (self.on_change is not None and self.on_change(mid)):
            self.pushed.set()

    def merge(self, mid: str, bids: tuple, asks: tuple, asked: float) -> bool:
        """A REST read of a window's book beside the pushes: dropped when a push came after the read was asked for; else the
        book in hand. True when it differed from a book held — a change no push brought."""
        old = self.books.get(mid)
        if old is not None and old[2] > asked:
            return False
        self.books[mid] = (bids, asks, time.monotonic())
        if old is None or (bids, asks) != (old[0], old[1]):
            self.stamps[mid] = (0, int(time.time() * 1000))
            self.changed.add(mid)
            self.arrivals += 1
            self.told(mid)
            return old is not None
        return False

    def take(self, mid: str, data: Any, n: int = 0) -> None:
        """A pushed book (as connection ``n`` brought it): kept (an older version dropped, a side left out keeps its own), the
        taker told when it changed."""
        if mid not in self.wanted or not isinstance(data, dict):
            return
        version = data.get("version")
        if isinstance(version, (int, float)) and not isinstance(version, bool):
            stamp, wall = data.get("updateTimestampMs"), int(time.time() * 1000)
            stamp = int(stamp) if isinstance(stamp, (int, float)) and not isinstance(stamp, bool) and 0 < stamp <= wall + 60_000 else 0
            held = self.versions.get(mid)
            # older than the book in hand: dropped — unless the server's own clock says it is the newer (its counter started
            # over: a restarted book), which a version alone cannot tell
            if held is not None and version < held[0] and not (stamp and held[1] and stamp > held[1]):
                return
            self.versions[mid] = (int(version), stamp)
            if len(self.conns) > 1:
                self.clock_arrival(n, mid, int(version))
        old = self.books.get(mid)
        bids = predict_levels(data.get("bids"), True) if isinstance(data.get("bids"), list) else (old[0] if old else ())
        asks = predict_levels(data.get("asks"), False) if isinstance(data.get("asks"), list) else (old[1] if old else ())
        self.books[mid] = (bids, asks, time.monotonic())
        if old is None or (bids, asks) != (old[0], old[1]):
            stamp, wall = data.get("updateTimestampMs"), int(time.time() * 1000)
            stamp = int(stamp) if isinstance(stamp, (int, float)) and not isinstance(stamp, bool) and 0 < stamp <= wall + 60_000 else 0
            self.stamps[mid] = (stamp, wall)
            if stamp and old is not None:
                self.lags.append(wall - stamp)
            self.changed.add(mid)
            self.arrivals += 1
            self.told(mid)

    def clock_arrival(self, n: int, mid: str, version: int) -> None:
        """Which connection brought this version first; a later copy of it from another one, how much later."""
        now, bit = time.monotonic(), 1 << min(n, 7)
        a = self.arrived.get(mid)
        if a is not None and a[0] == version:
            if not a[3] & bit:
                a[3] |= bit
                if n < len(self.conns):
                    self.conns[n].behind.append((now - a[1]) * 1000)
            return
        self.arrived[mid] = [version, now, n, bit]
        if n < len(self.conns):
            self.conns[n].firsts += 1

    def hedge(self) -> tuple[list[int], float | None, int] | None:
        """With more than one connection: how many books each brought first, and how much later the others brought the same book
        (ms, the median; None before any) with how many such copies — what the extra connections save. None with one."""
        if len(self.conns) < 2:
            return None
        behind = [b for c in self.conns for b in c.behind]
        return [c.firsts for c in self.conns], (statistics.median(behind) if behind else None), len(behind)

    def forget(self, mid: str) -> None:
        if all(mid not in c.subscribed for c in self.conns):
            self.arrived.pop(mid, None)
            self.books.pop(mid, None)
            self.versions.pop(mid, None)
            self.stamps.pop(mid, None)
            self.changed.discard(mid)

    def headers(self) -> dict[str, str]:
        return {"x-api-key": self.key} if self.key else {}

    async def run(self) -> None:
        tasks = [asyncio.create_task(c.run(), name=f"predict-ws-{c.n}") for c in self.conns]
        try:
            await self.stopping.wait()
        finally:
            for t in tasks:
                t.cancel()
            await asyncio.gather(*tasks, return_exceptions=True)

    def stop(self) -> None:
        self.stopping.set()


class _Conn:
    def __init__(self, stream: BookStream, n: int):
        self.stream, self.n = stream, n
        self.subscribed: set[str] = set()
        self.pending: dict[int, tuple[str, str]] = {}
        self.request_id = 0
        self.ws: Any = None
        self.connects = 0
        self.firsts = 0                                      # books this connection brought first (with more than one)
        self.behind: collections.deque = collections.deque(maxlen=300)  # how much later (ms) it brought one another had brought first
        self.frames = 0
        self.last_frame = 0.0
        self.error = ""
        self.backoff = BookStream.BACKOFF[0]
        self.wake = asyncio.Event()

    def alive(self) -> bool:
        return self.ws is not None and time.monotonic() - self.last_frame <= BookStream.ALIVE_S

    def connected_anew(self) -> None:
        """This connection (re)opened: its own subscriptions start over. The versions stay — another connection may be serving
        the same markets, and a frame older than the book it brought must never replace it (a market no connection serves any
        more was forgotten when its last one went)."""
        self.subscribed.clear()
        self.pending.clear()

    async def pause(self, seconds: float) -> None:
        with contextlib.suppress(asyncio.TimeoutError):
            await asyncio.wait_for(self.stream.stopping.wait(), seconds)

    async def run(self) -> None:
        s = self.stream
        while not s.stopping.is_set():
            if not s.wanted:
                self.wake.clear()
                with contextlib.suppress(asyncio.TimeoutError):
                    await asyncio.wait_for(self.wake.wait(), 5.0)
                continue
            try:
                ws = await s.connect(s.url, s.headers())
            except asyncio.CancelledError:
                raise
            except Exception as error:
                self.error = clean_error(error) or type(error).__name__
                await self.pause(self.backoff)
                self.backoff = min(self.backoff * 2, BookStream.BACKOFF[1])
                continue
            since = self.last_frame = time.monotonic()
            self.ws, self.connects = ws, self.connects + 1
            self.connected_anew()
            why = ""
            try:
                why = await self.session(ws)
            except asyncio.CancelledError:
                raise
            except Exception as error:
                why = clean_error(error) or type(error).__name__
            finally:
                self.ws = None
                for mid in list(self.subscribed):
                    self.subscribed.discard(mid)
                    s.forget(mid)
                with contextlib.suppress(Exception):
                    await asyncio.wait_for(ws.close(), 1.0)
            if s.stopping.is_set():
                return
            self.error = why or "连接结束"
            LOG.warning("Predict 推送连接 %d 断开：%s", self.n, self.error)
            self.backoff = BookStream.BACKOFF[0] if time.monotonic() - since >= 60 else min(self.backoff * 2, BookStream.BACKOFF[1])
            await self.pause(self.backoff)

    async def session(self, ws: Any) -> str:
        tasks = {asyncio.create_task(self.read(ws)), asyncio.create_task(self.sync_loop(ws)),
                 asyncio.create_task(self.stream.stopping.wait())}
        try:
            done, _ = await asyncio.wait(tasks, return_when=asyncio.FIRST_COMPLETED)
            task = next(iter(done))
            if self.stream.stopping.is_set():
                return ""
            error = task.exception()
            return (clean_error(error) or type(error).__name__) if error else (task.result() or "连接结束")
        finally:
            for task in tasks:
                task.cancel()
            await asyncio.gather(*tasks, return_exceptions=True)

    async def read(self, ws: Any) -> str:
        while True:
            try:
                raw = await asyncio.wait_for(ws.recv(), timeout=BookStream.SILENT_S)
            except asyncio.TimeoutError:
                return f"{BookStream.SILENT_S:.0f} 秒没有收到任何帧"
            self.frames += 1
            self.last_frame = time.monotonic()
            await self.handle(ws, raw)

    async def sync_loop(self, ws: Any) -> str:
        s = self.stream
        while True:
            self.wake.clear()
            for mid in sorted(s.wanted - self.subscribed):
                await self.request(ws, "subscribe", mid)
                self.subscribed.add(mid)
            for mid in sorted(self.subscribed - s.wanted):
                await self.request(ws, "unsubscribe", mid)
                self.subscribed.discard(mid)
                s.forget(mid)
            with contextlib.suppress(asyncio.TimeoutError):
                await asyncio.wait_for(self.wake.wait(), BookStream.SYNC_S)

    async def request(self, ws: Any, method: str, mid: str) -> None:
        self.request_id += 1
        self.pending[self.request_id] = (method, mid)
        await ws.send(dumps({"method": method, "requestId": self.request_id, "params": [TOPIC + mid]}))

    async def handle(self, ws: Any, raw: Any) -> None:
        try:
            msg = loads(raw)
        except (TypeError, ValueError):
            return
        if not isinstance(msg, dict):
            return
        kind = msg.get("type")
        if kind == "M":
            topic = str(msg.get("topic") or "")
            if topic.startswith(TOPIC):
                if (mid := topic[len(TOPIC):]) in self.subscribed:
                    self.stream.take(mid, msg.get("data"), self.n)
            elif topic == HEARTBEAT:
                await ws.send(dumps({"method": "heartbeat", "data": msg.get("data")}))
        elif kind == "R":
            with contextlib.suppress(TypeError, ValueError):
                method, mid = self.pending.pop(int(msg.get("requestId")), ("", ""))
                if mid and (msg.get("success") is False or (msg.get("success") is None and msg.get("error"))):
                    if method == "subscribe":
                        self.subscribed.discard(mid)  # asked for again at the next sync
                    self.error = f"{method} {mid}：{msg.get('error') or '拒绝'}"
