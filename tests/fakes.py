"""Stand-ins for Predict's API, the chain and the push connection (offline tests: no network, no key)."""
from __future__ import annotations

import asyncio
import os
import sys
import tempfile
import time
from decimal import Decimal as D
from typing import Any

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))

from ruletaker.config import Config, RuleSettings  # noqa: E402
from ruletaker.predict import NotSent, PredictPacer, RemoteError  # noqa: E402
from ruletaker.stream import BookStream  # noqa: E402

KEY = "0x" + "4c" * 32       # a throwaway key (never funded)
TOKEN_UP, TOKEN_DOWN = "1111111111111111111111", "2222222222222222222222"


def market_of(mid: str, slug: str, start_ms: int, status: str = "OPEN", question: str = "") -> dict:
    return {"id": mid, "conditionId": "0x" + "ab" * 32, "status": "REGISTERED", "tradingStatus": status, "question": question,
            "title": question or slug, "feeRateBps": 200, "isNegRisk": False, "isYieldBearing": False, "categorySlug": slug,
            "outcomes": [{"name": "Up", "indexSet": 1, "onChainId": TOKEN_UP}, {"name": "Down", "indexSet": 2, "onChainId": TOKEN_DOWN}]}


class FakeApi:
    """What RuleTaker asks of PredictApi, answered from dicts; every request recorded."""

    def __init__(self) -> None:
        self.markets: dict[str, dict] = {}       # market id -> details
        self.categories: dict[str, str] = {}     # slug -> market id
        self.books: dict[str, tuple] = {}        # market id -> (bids, asks)
        self.orders: dict[str, dict] = {}        # hash -> the order's row as Predict would show it
        self.created: list[dict] = []
        self.removed: list[str] = []
        self.calls: list[tuple] = []
        self.before_send: Any = None
        self.blocked_until = 0.0
        self.pacer = PredictPacer(0)
        self.host = "api.predict.fun"
        self.trips: list = []
        self.create_error: BaseException | None = None  # raised by the next create_order (then cleared)
        self.fill_on_create = 0.0                       # shares the order fills as it lands (a taker)
        self.create_delay = 0.0
        self.jwt_ok = True
        self.stream: Any = None
        self.pre_send: Any = None

    def fresh(self) -> bool:
        return self.jwt_ok

    async def ensure_auth(self) -> None:
        self.calls.append(("auth",))

    async def call(self, method: str, path: str, payload: Any = None, *args: Any, **kwargs: Any) -> Any:
        self.calls.append((method, path))
        if path.startswith("/categories/"):
            slug = path.split("/categories/", 1)[1]
            mid = self.categories.get(slug)
            if mid is None:
                raise RemoteError("HTTP 404: not found")
            return {"success": True, "data": {"slug": slug, "markets": [{**self.markets[mid], "title": self.markets[mid]["title"]}]}}
        raise RemoteError(f"HTTP 404: {path}")

    async def market(self, mid: str, priority: bool = False) -> dict:
        self.calls.append(("market", mid))
        return dict(self.markets[mid])

    async def orderbook(self, mid: str) -> tuple:
        self.calls.append(("book", mid))
        if mid not in self.books and self.stream is not None and mid in self.stream.last:
            return self.stream.last[mid]  # (Predict's book is the one it last pushed)
        return self.books.get(mid, ((), ()))

    async def create_order(self, body: dict, digest: str) -> dict:
        self.calls.append(("create", digest))
        if self.pre_send is not None:
            await self.pre_send()  # (whatever the request waited for before its bytes left: a token, a slot, a sign-in)
        if self.before_send is not None and (why := self.before_send(digest)):
            raise NotSent(why)
        if isinstance(body, (bytes, bytearray)):
            import json
            body = json.loads(body)
        self.created.append(body)
        if self.create_delay:
            await asyncio.sleep(self.create_delay)
        if self.create_error is not None:
            error, self.create_error = self.create_error, None
            raise error
        order = body["data"]["order"]
        shares = int(order["takerAmount"])
        filled = min(int(self.fill_on_create * 10 ** 18), shares)
        oid = f"id{len(self.created)}"
        self.orders[digest.lower()] = {"id": oid, "status": "FILLED" if filled >= shares else "OPEN", "amount": str(shares),
                                       "amountFilled": str(filled), "order": {"hash": digest}}
        return {"order_id": oid, "hash": digest}

    async def remove_orders(self, ids: list[str]) -> dict:
        self.calls.append(("remove", tuple(ids)))
        self.removed.extend(ids)
        for row in self.orders.values():
            if row["id"] in ids and row["status"] == "OPEN":
                row["status"] = "CANCELLED"
        return {"removed": list(ids), "noop": []}

    async def order(self, oid: str, hash_: str = "") -> dict | None:
        self.calls.append(("order", hash_))
        row = self.orders.get(hash_.lower())
        return dict(row) if row else None


class FakeChain:
    def __init__(self, usdt: float = 1000.0) -> None:
        self.usdt = int(usdt * 10 ** 18)
        self.allowed = 2 ** 256 - 1

    async def usdt_balance(self) -> int:
        return self.usdt

    async def bnb_balance(self) -> int:
        return 10 ** 17

    async def allowance(self, key: str) -> int:
        return self.allowed

    async def account_owner(self, account: str) -> str:
        return "0x" + "0" * 40


class QuietStream(BookStream):
    """A BookStream whose connections never open: the tests push books into it with take()."""

    def __init__(self) -> None:
        super().__init__("", 1, connect=self.never)
        self.fake_alive = True
        self.last: dict[str, tuple] = {}

    def take(self, mid: str, data: Any) -> None:
        from ruletaker.predict import predict_levels
        self.last[mid] = (predict_levels(data.get("bids"), True), predict_levels(data.get("asks"), False))
        super().take(mid, data)

    async def never(self, url: str, headers: dict) -> Any:
        await asyncio.sleep(3600)

    def alive(self) -> bool:
        return self.fake_alive

    @property
    def connected(self) -> bool:
        return self.fake_alive


class Clock:
    def __init__(self, ms: int) -> None:
        self.ms = ms

    def __call__(self) -> int:
        return self.ms


def make_config(**env: str) -> Config:
    base = {"RULE_TAKER": "on", "PREDICT_API_KEY": "k", "PREDICT_PRIVATE_KEY": KEY, "STATUS_PORT": "0",
            "RULE_FAST_HOURS": "off", "RULE_LEAD_MINUTES": "0"}
    base.update(env)
    return Config.from_env(base)


def make_taker(clock_ms: int, store: Any = None, journal: bool = False, **env: str):
    from ruletaker.store import Store
    from ruletaker.taker import RuleTaker
    config = make_config(**env)
    if store is None:
        if journal:
            folder = tempfile.mkdtemp(prefix="ruletaker-test-")
            store = Store(os.path.join(folder, "t.sqlite3"))
        else:
            store = Store(":memory:")
    api, chain, stream, clock = FakeApi(), FakeChain(), QuietStream(), Clock(clock_ms)
    api.stream = stream
    taker = RuleTaker(config, store, api=api, chain=chain, stream=stream, clock=clock)
    taker.ready_error = ""
    taker.usdt, taker.balance_asked_ms = chain.usdt, clock_ms - 60_000  # (the balance read a minute before the test begins)
    taker.running = True
    return taker, api, chain, stream, clock


def book(asks: list[tuple[str, str]] = (), bids: list[tuple[str, str]] = ()) -> dict:
    """A pushed frame's data: asks / bids as [[price, size], ...] (the book quoted for Up)."""
    return {"version": int(time.time() * 1000), "updateTimestampMs": int(time.time() * 1000) - 5,
            "asks": [[p, q] for p, q in asks], "bids": [[p, q] for p, q in bids]}


def run(coro: Any) -> Any:
    return asyncio.run(coro)


__all__ = ["FakeApi", "FakeChain", "QuietStream", "Clock", "make_config", "make_taker", "book", "run", "market_of", "D", "RuleSettings",
           "TOKEN_UP", "TOKEN_DOWN", "KEY"]
