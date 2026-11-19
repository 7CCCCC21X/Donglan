"""The index the in-play rules (盘中) are judged by — a port of what huice's IndexUpDownMarket card gave its rule taker.

The windows settle on Chainlink's stream of the index price (field i) of Binance's USDⓈ-M futures mark price stream: the index
is pushed every second (<symbol>@markPrice@1s on wss://fstream.binance.com/market — Binance retired the bare /ws on 2026-04-23),
read over REST (premiumIndex) while the push is late, and each window's start line is the close of the index's 1-minute
candle that ends at the start (indexPriceKlines). An index is as fresh as its own event time (E), not its arrival."""
from __future__ import annotations

import asyncio
import contextlib
import math
import time
import urllib.parse
from decimal import Decimal as D
from typing import Any, Callable

from . import http
from .predict import READ_POOL
from .util import LOG, RemoteError, clean_error, loads

CANDLE_GRACE_MS = 1_500        # a boundary candle is read this long after its minute ended
TRADE_STALE_SHARE = 0.02       # an index older than this share of the time left to the end is not traded on...
TRADE_STALE_MIN_MS = 5_000     # ...in the last minutes one older than this
REST_EVERY_S = 1.0             # the REST read's pace while the push is late (and an in-play window needs the index)
PUSH_LATE_MS = 2_500           # ...the push counts as late past this


class IndexFeed:
    def __init__(self, symbols: list[str], fapi: str = "https://fapi.binance.com", ws_base: str = "wss://fstream.binance.com/market",
                 connect: Any = None, getter: Any = None):
        self.symbols = sorted({s.upper() for s in symbols})
        self.fapi, self.ws_base = fapi.rstrip("/"), ws_base.rstrip("/")
        self.connect = connect
        self.getter = getter  # (path, params) -> JSON (the tests' stand-in for Binance's REST)
        self.prices: dict[str, D] = {}
        self.index_ms: dict[str, int] = {}   # the newest index's own time
        self.priced_ms: dict[str, int] = {}  # min(its arrival, its own time): what its age is counted from
        self.candles: dict[tuple[str, int], D] = {}  # (symbol, open ms) -> close of that 1-minute index candle
        self.candle_error: dict[tuple[str, int], str] = {}
        self.on_change: Callable[[str, int], None] | None = None  # (symbol, the index's own time) — the in-play looks
        self.needed: Callable[[], bool] = lambda: True  # whether an in-play window needs the index now (the REST read's pace)
        self.wanted: Callable[[], bool] = lambda: True  # whether any rule buys in its windows' course at all (a price set): connected only then
        self.ws: Any = None
        self.error = ""
        self.pushes = 0
        self.reads = 0
        self.stopping = asyncio.Event()

    # -- what the taker asks --
    def price(self, symbol: str) -> D | None:
        return self.prices.get(symbol.upper())

    def price_problem(self, symbol: str, end_ms: int, now_ms: int) -> str:
        """Why the index in hand is too old to trade on ("" while it is not): older than TRADE_STALE_SHARE of the time left to
        the window's end, or than TRADE_STALE_MIN_MS in the last minutes."""
        left = end_ms - now_ms
        limit = max(TRADE_STALE_MIN_MS, TRADE_STALE_SHARE * left)
        age = now_ms - int(self.priced_ms.get(symbol.upper()) or 0)
        if age <= limit:
            return ""
        return (f"指数 {math.ceil(age / 1000)} 秒未更新，超过离结束 {max(left, 0) / 60_000:.1f} 分钟时的上限 "
                f"{math.floor(limit / 100 + 1e-6) / 10:g} 秒")

    def close_of(self, symbol: str, start_ms: int) -> D | None:
        """The start line of the window starting at ``start_ms``: the close of the index's 1-minute candle ending there."""
        return self.candles.get((symbol.upper(), start_ms - 60_000))

    def push_index(self, symbol: str, price: D, at_ms: int, now_ms: int) -> bool:
        """An index (pushed, or read): taken when newer than the one held; True when it moved."""
        symbol = symbol.upper()
        if at_ms <= self.index_ms.get(symbol, 0) or not price > 0:
            return False
        moved = price != self.prices.get(symbol)
        self.index_ms[symbol], self.prices[symbol], self.priced_ms[symbol] = at_ms, price, min(now_ms, at_ms)
        if moved and self.on_change is not None:
            with contextlib.suppress(Exception):
                self.on_change(symbol, at_ms)
        return moved

    async def get(self, path: str, **params: Any) -> Any:
        if self.getter is not None:
            return await self.getter(path, params)
        url = f"{self.fapi}/fapi/v1/{path}?{urllib.parse.urlencode(params)}"
        loop = asyncio.get_running_loop()
        return await loop.run_in_executor(READ_POOL, http.request_json, url, None, 8, {})

    async def read_candle(self, symbol: str, open_ms: int) -> D:
        """The close of exactly the index's 1-minute candle that opens at open_ms (a neighbouring minute is never taken)."""
        rows = await self.get("indexPriceKlines", pair=symbol, interval="1m", startTime=open_ms, endTime=open_ms + 59_999, limit=1)
        row = next((r for r in rows or [] if isinstance(r, list) and len(r) > 6 and int(r[0]) == open_ms), None)
        if row is None or int(row[6]) != open_ms + 59_999:
            raise RemoteError(f"币安没有返回 {symbol} 这根 1 分钟指数 K 线")
        close = D(str(row[4]))
        if not close > 0:
            raise RemoteError(f"{symbol} 指数 K 线收盘价无效")
        return close

    async def ensure_start(self, symbol: str, start_ms: int, now_ms: int) -> D | None:
        """The window's start line, read once its minute is over (and CANDLE_GRACE_MS more); None until then or while it fails."""
        key = (symbol.upper(), start_ms - 60_000)
        if key in self.candles:
            return self.candles[key]
        if now_ms < start_ms + CANDLE_GRACE_MS:
            return None
        try:
            self.candles[key] = await self.read_candle(key[0], key[1])
            self.candle_error.pop(key, None)
            if len(self.candles) > 400:
                for old in sorted(self.candles)[:100]:
                    self.candles.pop(old, None)
            if self.on_change is not None:
                with contextlib.suppress(Exception):
                    self.on_change(key[0], 0)  # (the first look of a course waits for it: looked at now, whatever the index's age)
            return self.candles[key]
        except asyncio.CancelledError:
            raise
        except Exception as error:
            self.candle_error[key] = clean_error(error) or type(error).__name__
            return None

    async def read_price(self, symbol: str) -> None:
        data = await self.get("premiumIndex", symbol=symbol)
        now = int(time.time() * 1000)
        at = int((data or {}).get("time") or now)
        self.reads += 1
        self.push_index(symbol, D(str(data["indexPrice"])), at, now)

    # -- the loops --
    async def run(self) -> None:
        tasks = [asyncio.create_task(self.stream_loop(), name="binance-ws"), asyncio.create_task(self.rest_loop(), name="binance-rest")]
        try:
            await self.stopping.wait()
        finally:
            for t in tasks:
                t.cancel()
            await asyncio.gather(*tasks, return_exceptions=True)

    def stop(self) -> None:
        self.stopping.set()

    async def rest_loop(self) -> None:
        while not self.stopping.is_set():
            now = int(time.time() * 1000)
            if self.needed():
                for symbol in self.symbols:
                    if now - int(self.priced_ms.get(symbol) or 0) > PUSH_LATE_MS:
                        try:
                            await self.read_price(symbol)
                        except asyncio.CancelledError:
                            raise
                        except Exception as error:
                            self.error = f"REST 指数：{clean_error(error) or type(error).__name__}"
            with contextlib.suppress(asyncio.TimeoutError):
                await asyncio.wait_for(self.stopping.wait(), REST_EVERY_S if self.needed() else 15.0)

    async def stream_loop(self) -> None:
        from .stream import ws_connect
        connect = self.connect or ws_connect
        backoff = 2.0
        url = f"{self.ws_base}/stream?streams=" + "/".join(f"{s.lower()}@markPrice@1s" for s in self.symbols)
        while not self.stopping.is_set():
            if not self.wanted():  # no in-play price set (it may be set on the control page any time): no connection kept meanwhile
                with contextlib.suppress(asyncio.TimeoutError):
                    await asyncio.wait_for(self.stopping.wait(), 3.0)
                continue
            try:
                ws = await connect(url, {})
            except asyncio.CancelledError:
                raise
            except Exception as error:
                self.error = f"币安推送连不上：{clean_error(error) or type(error).__name__}"
                with contextlib.suppress(asyncio.TimeoutError):
                    await asyncio.wait_for(self.stopping.wait(), backoff)
                backoff = min(backoff * 2, 120.0)
                continue
            self.ws, began, self.error = ws, time.monotonic(), ""
            try:
                while not self.stopping.is_set() and self.wanted():
                    raw = await asyncio.wait_for(ws.recv(), timeout=30)
                    self.take(raw)
            except asyncio.CancelledError:
                raise
            except Exception as error:
                self.error = f"币安推送断开：{clean_error(error) or type(error).__name__}"
                LOG.warning("%s", self.error)
            finally:
                self.ws = None
                with contextlib.suppress(Exception):
                    await asyncio.wait_for(ws.close(), 1.0)
            backoff = 2.0 if time.monotonic() - began > 60 else min(backoff * 2, 120.0)
            with contextlib.suppress(asyncio.TimeoutError):
                await asyncio.wait_for(self.stopping.wait(), backoff)

    def take(self, raw: Any) -> None:
        try:
            msg = loads(raw)
        except (TypeError, ValueError):
            return
        data = msg.get("data") if isinstance(msg, dict) and isinstance(msg.get("data"), dict) else msg
        if not isinstance(data, dict) or data.get("e") != "markPriceUpdate":
            return
        with contextlib.suppress(TypeError, ValueError, ArithmeticError, KeyError):
            self.pushes += 1
            self.push_index(str(data["s"]), D(str(data["i"])), int(data["E"]), int(time.time() * 1000))
