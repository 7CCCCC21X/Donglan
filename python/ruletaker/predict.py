"""Predict.fun's REST API (api.predict.fun/v1) and the BNB-chain reads an order needs (a port of huice live.PredictApi,
preopen.safe_api / category_market, main.PredictPacer and the market parsers).

The API key goes in x-api-key on every request; the JWT from the sign-in flow (GET /auth/message → sign → POST /auth) in
Authorization, renewed before it expires or after a 401. Every key has a budget (PREDICT_RATE_PER_MIN, 220 a minute against
Predict's 240): a read waits for its token, an order or a cancel may borrow a burst ahead of the reads. A 429 pauses every
request on the client for its retry-after (20 s without one). Orders and cancels go out on threads of their own, never queued
behind a read."""
from __future__ import annotations

import asyncio
import base64
import collections
import concurrent.futures
import contextlib
import dataclasses
import decimal
import functools
import json
import re
import time
import urllib.parse
from concurrent.futures import ThreadPoolExecutor
from decimal import Decimal as D
from typing import Any, Callable

from . import http
from .util import NotSent, RemoteError, brief_error, clean_error, http_error_text, loads

PREDICT_REST = "https://api.predict.fun/v1"
API_BASES = {56: PREDICT_REST, 97: "https://api-testnet.predict.fun/v1"}  # the testnet API needs no API key
RPC_URLS = {56: "https://bsc-dataseed.bnbchain.org/", 97: "https://bsc-testnet-dataseed.bnbchain.org/"}
API_TIMEOUT = 12
ORDER_TIMEOUT = 20
RPC_TIMEOUT = 15
PREDICT_DEPTH = 20  # levels kept a side
UP_WORDS = {"yes", "up", "涨", "上涨", "higher", "above"}
DOWN_WORDS = {"no", "down", "跌", "下跌", "lower", "below"}
DUPLICATE_WORDS = ("already exist", "already placed", "duplicate", "exists")  # Predict has the order: not a refusal
LISTING_FIELDS = ("id", "conditionId", "status", "tradingStatus", "question", "outcomes")
LISTING_FULL_FIELDS = (*LISTING_FIELDS, "feeRateBps", "isNegRisk", "isYieldBearing")
PRICE_FIELDS = ("startPrice", "priceToBeat", "targetPrice", "strikePrice", "referencePrice")

READ_POOL = ThreadPoolExecutor(max_workers=8, thread_name_prefix="predict-read")
WRITE_POOL = ThreadPoolExecutor(max_workers=4, thread_name_prefix="predict-write")  # orders and cancels: never behind a read
GATE_WAIT_S = 2.0  # an order judged again on its request's thread (after a new connection): the loop's answer awaited this long


class OrderRejected(RemoteError):
    """Predict refused the order outright: nothing was placed."""


def http_status(error: BaseException) -> int:
    match = re.match(r"HTTP (\d{3})", str(error))
    return int(match.group(1)) if match else 0


def jwt_expiry(token: str) -> int:
    try:
        payload = token.split(".")[1]
        data = json.loads(base64.urlsafe_b64decode(payload + "=" * (-len(payload) % 4)))
        return int(data.get("exp") or 0)
    except (IndexError, ValueError, TypeError, AttributeError):
        return 0


def answer_shape(answer: Any) -> str:
    """An API answer's field names (one level into "data"), for an error that must not quote its values."""
    if not isinstance(answer, dict):
        return type(answer).__name__
    inner = answer.get("data")
    names = sorted(str(k) for k in answer)
    if isinstance(inner, dict):
        names.append("data{" + ",".join(sorted(str(k) for k in inner)) + "}")
    return ",".join(names)[:160]


# --- the request budget of a key ---------------------------------------------------------------------------------------------
class PredictPacer:
    """A key's request budget: so many tokens a minute, a burst of BURST; a read waits for a whole token, an order or a cancel
    (``write``) may borrow up to a burst ahead of the reads, so it never queues behind them. Empty at first: a process just
    started does not know how much of Predict's minute the one before it used."""
    BURST = 8.0

    def __init__(self, per_min: float):
        self.per_min = float(per_min)
        self.tokens = 0.0
        self.at = time.monotonic()
        self.waited = 0.0
        self.waits: collections.deque = collections.deque(maxlen=4096)
        self.sent: collections.deque = collections.deque(maxlen=4096)
        self.limits: collections.deque = collections.deque(maxlen=200)

    def refill(self) -> None:
        now = time.monotonic()
        self.tokens = min(self.BURST, self.tokens + (now - self.at) * self.per_min / 60.0)
        self.at = now

    def ready(self, write: bool = False) -> bool:
        """Whether a request would get its token this instant (no wait)."""
        if self.per_min <= 0:
            return True
        self.refill()
        return self.tokens >= (1.0 - self.BURST if write else 1.0) - 1e-9

    async def take(self, write: bool = False) -> None:
        if self.per_min > 0:
            need = 1.0 - self.BURST if write else 1.0
            began = 0.0
            while True:
                self.refill()
                if self.tokens >= need - 1e-9:
                    self.tokens -= 1.0
                    break
                wait = (need - self.tokens) * 60.0 / self.per_min
                self.waited += wait
                began = began or time.monotonic()
                await asyncio.sleep(wait)
            if began:
                self.waits.append((time.monotonic(), time.monotonic() - began))
        self.sent.append(time.monotonic())

    def last_minute(self) -> int:
        cut = time.monotonic() - 60.0
        return sum(1 for t in self.sent if t >= cut)

    def waited_last(self, seconds: float = 60.0) -> tuple[int, float]:
        cut = time.monotonic() - seconds
        waits = [w for t, w in self.waits if t >= cut]
        return len(waits), sum(waits)

    def note_429(self) -> None:
        self.limits.append((time.monotonic(), self.last_minute()))


# --- the API ---------------------------------------------------------------------------------------------------------------
class PredictApi:
    PARALLEL = 6           # requests in flight at most...
    READ_PARALLEL = 4      # ...of which the reads may hold this many: slots always stay free for an order or a cancel
    BACKOFF_SECONDS = 20

    def __init__(self, base: str, api_key: str, wallet: Any, per_min: float = 220.0,
                 before_send: Callable[[str], str] | None = None):
        self.base, self.api_key, self.wallet = base.rstrip("/"), api_key, wallet
        self.host = urllib.parse.urlsplit(self.base).hostname or ""
        self.pacer = PredictPacer(per_min)
        self.before_send = before_send  # judged right before an order's bytes go out: "" sends it, a reason refuses it (NotSent)
        self.jwt, self.jwt_exp, self.jwt_at = "", 0, 0.0
        self.auth_error = ""
        self._auth_lock: tuple[Any, asyncio.Lock] | None = None
        self._gates: tuple[Any, asyncio.Semaphore, asyncio.Semaphore] | None = None
        self.writes_inflight = 0
        self.blocked_until = 0.0
        self.calls = 0
        self.limited: collections.deque = collections.deque(maxlen=500)
        self.trips: collections.deque = collections.deque(maxlen=300)  # (method, ms): the request alone, after the pace and the gate
        self.last_ok = 0.0  # monotonic: the last answered request (a warm connection)

    # -- plumbing --
    def gates(self) -> tuple[asyncio.Semaphore, asyncio.Semaphore]:
        loop = asyncio.get_running_loop()
        if self._gates is None or self._gates[0] is not loop:
            self._gates = (loop, asyncio.Semaphore(self.PARALLEL), asyncio.Semaphore(self.READ_PARALLEL))
        return self._gates[1], self._gates[2]

    def headers(self) -> dict[str, str]:
        out = {}
        if self.api_key:
            out["x-api-key"] = self.api_key
        if self.jwt:
            out["Authorization"] = f"Bearer {self.jwt}"
        return out

    def cooling(self) -> float:
        return self.blocked_until - time.monotonic()

    async def call(self, method: str, path: str, payload: Any = None, timeout: float = API_TIMEOUT, priority: bool = False,
                   check: str = "") -> Any:
        """One request: the key's token, a slot (a read takes one of the reads' slots first), then the request on a thread
        (an order's or a cancel's own, ``priority``). ``check``: the order hash before_send judges inside the slot, right
        before the bytes go out. An explicit success=false is a refusal whatever the status."""
        if (left := self.cooling()) > 0:
            raise RemoteError(f"Predict 接口限流冷却中（{int(left) + 1} 秒后重试）", int(left) + 1)
        if method == "POST" and payload is None:
            payload = {}
        await self.pacer.take(write=priority)
        self.calls += 1
        gate, read_gate = self.gates()
        if priority:
            self.writes_inflight += 1
            try:
                async with gate:
                    raw, began = await self.send(method, path, payload, timeout, True, check)
            finally:
                self.writes_inflight -= 1
        else:
            async with read_gate, gate:
                raw, began = await self.send(method, path, payload, timeout, False, check)
        self.trips.append((method, (time.perf_counter() - began) * 1000))
        self.last_ok = time.monotonic()
        try:
            answer = loads(raw) if raw else {}
        except (ValueError, UnicodeDecodeError):
            raise RemoteError("Predict 接口未返回有效 JSON") from None
        if not isinstance(answer, dict) or answer.get("success", True) is not True:
            raise RemoteError(f"Predict 应答不是已确认成功的数据（{method} {path.split('?')[0]}）："
                              + (http_error_text(answer) if isinstance(answer, dict) else type(answer).__name__))
        return answer

    async def send(self, method: str, path: str, payload: Any, timeout: float, write: bool, check: str) -> tuple[bytes, float]:
        if (left := self.cooling()) > 0:  # a cooldown that began while this request waited for its slot
            raise RemoteError(f"Predict 接口限流冷却中（{int(left) + 1} 秒后重试）", int(left) + 1)
        if check and self.before_send is not None and (why := self.before_send(check)):
            raise NotSent(why)
        loop = asyncio.get_running_loop()
        gate = self.thread_gate(loop, check) if check and self.before_send is not None else None
        began = time.perf_counter()
        try:
            raw = await loop.run_in_executor(WRITE_POOL if write else READ_POOL, functools.partial(
                http.request, self.base + path, payload, timeout, self.headers(), **({"gate": gate} if gate else {})))
        except RemoteError as error:
            if error.retry_after or http_status(error) == 429:
                self.blocked_until = max(self.blocked_until, time.monotonic() + max(error.retry_after, self.BACKOFF_SECONDS))
                if http_status(error) == 429:
                    self.limited.append(time.monotonic())
                    self.pacer.note_429()
            raise
        return raw, began

    def thread_gate(self, loop: asyncio.AbstractEventLoop, check: str) -> Callable[[], str]:
        """before_send for the request's own thread: judged again when the request had to wait there — a new connection's TCP +
        TLS handshake, a dead one replaced — right before the bytes go out (a pause, a stop, a time line crossed meanwhile holds
        it). Judged on the loop (the taker's state is the loop's), the thread waiting for the answer: no answer, no send."""
        judge = self.before_send

        def gate() -> str:
            answer: concurrent.futures.Future = concurrent.futures.Future()

            def run() -> None:
                try:
                    answer.set_result(judge(check))
                except BaseException as error:  # noqa: BLE001 — whatever it was, the order is not sent on it
                    answer.set_exception(error)
            try:
                loop.call_soon_threadsafe(run)
            except RuntimeError:  # the loop is closed: the service stops
                return "服务正在停止，不再发单"
            try:
                return str(answer.result(timeout=GATE_WAIT_S) or "")
            except BaseException as error:  # noqa: BLE001
                return f"发单前的复核没有答复（{type(error).__name__}），这张单不发"
        return gate

    async def authed(self, method: str, path: str, payload: Any = None, timeout: float = API_TIMEOUT, priority: bool = False,
                     check: str = "") -> Any:
        await self.ensure_auth()
        used = self.jwt
        try:
            return await self.call(method, path, payload, timeout, priority, check)
        except RemoteError as error:
            if http_status(error) != 401 or not self.wallet:
                raise
            if self.jwt == used:  # only the token that was refused is dropped (another request may have signed in meanwhile)
                self.jwt = ""
            await self.ensure_auth()
            return await self.call(method, path, payload, timeout, priority, check)

    def fresh(self) -> bool:
        return bool(self.jwt) and (not self.jwt_exp or time.time() < self.jwt_exp - 300)

    @property
    def auth_lock(self) -> asyncio.Lock:
        loop = asyncio.get_running_loop()
        if self._auth_lock is None or self._auth_lock[0] is not loop:
            self._auth_lock = (loop, asyncio.Lock())
        return self._auth_lock[1]

    async def ensure_auth(self) -> None:
        if self.fresh():
            return
        async with self.auth_lock:
            if self.fresh():
                return
            await self._sign_in()

    async def _sign_in(self) -> str:
        if self.wallet is None:
            raise RemoteError("没有签名钱包，无法登录 Predict")
        try:
            data = await self.call("GET", "/auth/message")
            message = str(((data or {}).get("data") or {}).get("message") or "")
            if not message:
                raise RemoteError("登录消息格式异常（应答字段：" + answer_shape(data) + "）")
            answer = await self.call("POST", "/auth", {"signer": self.wallet.maker, "signature": self.wallet.sign_text(message),
                                                      "message": message})
            token = str(((answer or {}).get("data") or {}).get("token") or "")
            if not token:
                raise RemoteError("登录未返回 token（应答字段：" + answer_shape(answer) + "）")
        except RemoteError as error:
            self.auth_error = clean_error(error)
            raise
        self.jwt, self.jwt_exp, self.jwt_at, self.auth_error = token, jwt_expiry(token), time.time(), ""
        return token

    # -- the endpoints --
    async def market(self, market_id: str, priority: bool = False) -> dict:
        data = await self.call("GET", f"/markets/{urllib.parse.quote(str(market_id))}", priority=priority)
        market = data.get("data", data) if isinstance(data, dict) else {}
        if not isinstance(market, dict) or not market:
            raise RemoteError("Predict 市场详情格式异常")
        return market

    async def orderbook(self, market_id: str) -> tuple[tuple, tuple]:
        answer = await self.call("GET", f"/markets/{urllib.parse.quote(str(market_id))}/orderbook")
        data = answer.get("data", answer) if isinstance(answer, dict) else {}
        if not isinstance(data, dict) or ("bids" not in data and "asks" not in data):
            raise RemoteError("订单簿格式异常")
        return predict_levels(data.get("bids"), True), predict_levels(data.get("asks"), False)

    async def create_order(self, body: Any, digest: str, checked: bool = True, check: str = "") -> dict:
        """POST /orders -> {"order_id", "hash"}; ``body`` the request (a dict, or its JSON bytes made beforehand), ``digest`` the
        order's hash (before_send's check; ``check`` another one for it — the operator's order test's "test:…"; ``checked``
        False: none). A 2xx answer without an order id is no refusal: the order may be there."""
        answer = await self.authed("POST", "/orders", body, timeout=ORDER_TIMEOUT, priority=True,
                                   check=check or (digest if checked else ""))
        data = answer.get("data") if isinstance(answer, dict) else None
        if not isinstance(data, dict):
            raise RemoteError("下单应答没有可读的内容（订单可能已被接受，按哈希核对）：" + brief_error(json.dumps(answer, ensure_ascii=False), 120))
        order_id = str(data.get("orderId") or data.get("id") or data.get("order_id") or "")
        if not order_id:
            raise RemoteError("下单应答没有订单号：" + brief_error(json.dumps(answer, ensure_ascii=False), 120))
        return {"order_id": order_id, "hash": str(data.get("orderHash") or data.get("hash") or "")}

    async def remove_orders(self, ids: list[str]) -> dict:
        answer = await self.authed("POST", "/orders/remove", {"data": {"ids": [str(i) for i in ids]}}, priority=True)
        data = answer.get("data") if isinstance(answer.get("data"), dict) else answer
        return {"removed": [str(i) for i in (data.get("removed") or [])], "noop": [str(i) for i in (data.get("noop") or [])]}

    async def order(self, order_id: str, hash_: str = "") -> dict | None:
        """One order, open or closed, by its hash (GET /orders/{hash}); None when Predict knows nothing of it (404). An answer
        with no order in it (an empty body, ``{}``) is None only for an order Predict never gave an id to: one it did is there,
        and such an answer says nothing of it — an error, read again later (its fills are never closed on it)."""
        if not hash_:
            return None
        try:
            data = await self.authed("GET", f"/orders/{urllib.parse.quote(str(hash_))}")
        except RemoteError as error:
            if http_status(error) == 404:
                return None
            raise
        row = data.get("data", data) if isinstance(data, dict) else None
        if isinstance(row, list):
            row = next((r for r in row if isinstance(r, dict) and (order_hash_of(r) == hash_.lower() or (order_id and str(r.get("id")) == str(order_id)))), None)
        if isinstance(row, dict) and (row.get("id") is not None or row.get("order") or row.get("status")):
            return row
        if order_id:
            raise RemoteError(f"查单应答里没有订单 #{order_id} 的内容（不是 404）：过一会儿再查")
        return None


async def category_market(api: PredictApi, slug: str) -> dict | None:
    """A window's market by its category (GET /categories/<slug>), then its details unless the category's own market object
    carries every field an order needs; the slug put on it; None when the category is not listed (404)."""
    try:
        answer = await api.call("GET", f"/categories/{urllib.parse.quote(slug)}")
    except RemoteError as error:
        if "HTTP 404" in str(error):
            return None
        raise
    found = predict_markets(answer)
    if not found:
        return None
    node = category_node(answer)
    if node is not None and str(node.get("id")) == found[0]["id"] and listing_complete(node):
        return {**node, "categorySlug": slug, "id": str(node["id"]), "_from": "category"}
    market = await api.market(found[0]["id"])
    return {**market, "categorySlug": slug, "id": market.get("id") or found[0]["id"], "_from": "details"}


# --- what the answers say ----------------------------------------------------------------------------------------------------------
def predict_levels(rows: Any, bids: bool) -> tuple[tuple[D, D], ...]:
    """[[price, size], ...] or [{price, size}, ...] -> sorted (price, size), empty levels dropped, PREDICT_DEPTH a side."""
    out = []
    for row in rows if isinstance(rows, list) else []:
        try:
            price, size = (row[0], row[1]) if isinstance(row, (list, tuple)) else (row.get("price"), row.get("size", row.get("quantity")))
            price, size = D(str(price)), D(str(size))
        except (IndexError, TypeError, AttributeError, decimal.InvalidOperation):
            continue
        if price.is_finite() and size.is_finite() and 0 < price < 1 and size > 0:
            out.append((price, size))
    out.sort(key=lambda level: level[0], reverse=bids)
    return tuple(out[:PREDICT_DEPTH])


def predict_markets(data: Any) -> list[dict]:
    found: list[dict] = []

    def walk(node: Any, depth: int = 0) -> None:
        if depth > 8:
            return
        if isinstance(node, dict):
            if node.get("id") is not None and ("conditionId" in node or "question" in node) and "title" in node:
                found.append({"id": str(node["id"]), "conditionId": str(node.get("conditionId") or ""),
                              "title": str(node.get("title") or node.get("question") or ""), "question": str(node.get("question") or "")})
                return
            for value in node.values():
                walk(value, depth + 1)
        elif isinstance(node, list):
            for value in node:
                walk(value, depth + 1)
    walk(data)
    return list({m["id"]: m for m in found}.values())


def category_node(answer: Any) -> dict | None:
    def walk(node: Any, depth: int = 0) -> dict | None:
        if depth > 8:
            return None
        if isinstance(node, dict):
            if node.get("id") is not None and "conditionId" in node and isinstance(node.get("outcomes"), list):
                return node
            for value in node.values():
                if (hit := walk(value, depth + 1)) is not None:
                    return hit
        elif isinstance(node, list):
            for value in node:
                if (hit := walk(value, depth + 1)) is not None:
                    return hit
        return None
    return walk(answer)


def listing_complete(node: dict) -> bool:
    outcomes = node.get("outcomes")
    return (all(k in node for k in LISTING_FULL_FIELDS) and isinstance(outcomes, list) and len(outcomes) >= 2
            and all(isinstance(o, dict) and o.get("onChainId") and o.get("indexSet") is not None for o in outcomes))


def market_start_price(market: Any) -> D | None:
    """The target price (price to beat) a market's details may carry, under whichever name; None when none."""
    if not isinstance(market, dict):
        return None
    rows = [market, *(market.get(k) for k in ("marketData", "metadata") if isinstance(market.get(k), dict))]
    for row in rows:
        for name in PRICE_FIELDS:
            value = row.get(name)
            if value in (None, "", 0, False):
                continue
            with contextlib.suppress(decimal.InvalidOperation, ValueError, TypeError):
                found = D(str(value))
                if found.is_finite() and found > 0:
                    return found
    return None


def predict_resolution(market: dict) -> dict | None:
    """What a market object says it settled on: {"index", "name", "split", "how"}; None while open or unreadable."""
    rows = sorted((o for o in market.get("outcomes") or [] if isinstance(o, dict)), key=lambda o: int(o.get("indexSet") or 0))
    names = [str(o.get("name") or "") for o in rows]
    won = [i for i, o in enumerate(rows) if str(o.get("status") or "").strip().upper() in {"WON", "WIN", "WINNER"}]
    if len(won) == 1:
        return {"index": won[0], "name": names[won[0]], "split": False, "how": "outcomes.status"}
    if len(won) == len(rows) == 2:
        return {"index": None, "name": "50/50", "split": True, "how": "outcomes.status"}
    pays = []
    for o in rows:
        value = next((o[k] for k in ("payout", "payoutNumerator", "payoutPerShare") if o.get(k) not in (None, "")), None)
        with contextlib.suppress(TypeError, ValueError):
            pays.append(float(value))
    if rows and len(pays) == len(rows) and sum(pays) > 0:
        top = max(pays)
        if all(abs(p - top) < 1e-9 for p in pays):
            return {"index": None, "name": "50/50", "split": True, "how": "outcomes.payout"}
        if sum(1 for p in pays if p > 0) == 1:
            i = pays.index(top)
            return {"index": i, "name": names[i], "split": False, "how": "outcomes.payout"}
    status = str(market.get("status") or "").upper()
    if not any(word in status for word in ("RESOLV", "SETTL", "FINAL")):
        return None
    res = next((market.get(k) for k in ("resolution", "resolvedOutcome", "winningOutcome", "winner") if market.get(k)), None)
    name = str(res.get("name") or res.get("outcome") or "") if isinstance(res, dict) else res if isinstance(res, str) else ""
    index = None
    if isinstance(res, dict) and str(res.get("indexSet") or "").isdigit():
        index = next((i for i, o in enumerate(rows) if str(o.get("indexSet")) == str(res["indexSet"])), None)
    if index is None and name:
        index = next((i for i, n in enumerate(names) if n.strip().lower() == str(name).strip().lower()), None)
    if index is None:
        return None
    return {"index": index, "name": names[index], "split": False, "how": "resolution"}


@dataclasses.dataclass
class MarketInfo:
    market_id: str
    outcomes: list[dict]  # [{"name", "index_set", "token"}] in index order
    fee_bps: int
    neg_risk: bool
    yield_bearing: bool
    condition_id: str
    status: str


def parse_market(market: dict, default_fee: int) -> MarketInfo:
    rows = sorted((o for o in market.get("outcomes") or [] if isinstance(o, dict)), key=lambda o: int(o.get("indexSet") or 0))
    outcomes = [{"name": str(o.get("name") or ""), "index_set": int(o.get("indexSet") or i + 1),
                 "token": str(o.get("onChainId") or o.get("tokenId") or o.get("token_id") or "")} for i, o in enumerate(rows)]
    fee = None
    with contextlib.suppress(TypeError, ValueError):
        stated = int(market.get("feeRateBps"))
        fee = stated if 0 <= stated <= 10_000 else None
    return MarketInfo(str(market.get("id") or ""), outcomes, fee if fee is not None else default_fee,
                      bool(market.get("isNegRisk")), bool(market.get("isYieldBearing")), str(market.get("conditionId") or ""),
                      str(market.get("status") or ""))


def outcome_for_side(side: str, outcomes: list[dict]) -> tuple[dict | None, str]:
    """The outcome (name, index set, token) an up/down window's side buys, and a note when it was assumed from the order of
    the outcomes rather than read from their names."""
    if len(outcomes) != 2 or not all(o.get("token") for o in outcomes):
        return None, "市场不是两个结果，或缺少结果代币编号"
    names = [str(o.get("name") or "").strip().lower() for o in outcomes]
    ups = [o for o, n in zip(outcomes, names) if n in UP_WORDS]
    downs = [o for o, n in zip(outcomes, names) if n in DOWN_WORDS]
    hit = None
    if side == "up":
        if len(ups) == 1:
            hit = ups[0]
        elif len(downs) == 1:
            hit = next(o for o in outcomes if o is not downs[0])
    else:
        if len(downs) == 1:
            hit = downs[0]
        elif len(ups) == 1:
            hit = next(o for o in outcomes if o is not ups[0])
    if hit is not None:
        return hit, ""
    return outcomes[0 if side == "up" else 1], f"按结果顺序推定（结果名称：{'、'.join(names)}）"


def book_up_first(market: dict) -> bool:
    """Whether the market lists its Up outcome first (its book is then quoted for Up, as offered() reads it)."""
    rows = sorted((o for o in market.get("outcomes") or [] if isinstance(o, dict)), key=lambda o: int(o.get("indexSet") or 0))
    first = str(rows[0].get("name") or "").strip().lower() if rows else ""
    return first not in DOWN_WORDS


def order_hash_of(row: dict) -> str:
    inner = row.get("order") if isinstance(row.get("order"), dict) else {}
    return str(inner.get("hash") or row.get("hash") or row.get("orderHash") or "").lower()


FILLED_STATUSES = {"FILLED", "MATCHED", "COMPLETED", "DONE"}
FINAL_STATUSES = {"FILLED", "CANCELLED", "CANCELED", "EXPIRED", "INVALIDATED", "REJECTED", "FAILED", "MATCHED", "CLOSED",
                  "COMPLETED", "DONE", "SETTLED", "RESOLVED", "UNKNOWN"}
OPEN_STATUSES = {"OPEN", "PENDING", "ACTIVE", "LIVE", "NEW", "PARTIAL", "PARTIALLY_FILLED", "PARTIALLY FILLED", "RESTING"}


def order_fill(row: dict, ordered_shares: float) -> tuple[float, str]:
    """(shares filled, status) of an order record: amountFilled / amount of the shares ordered, the status upper-cased."""
    status = str(row.get("status") or "").upper()
    try:
        amount, got = int(str(row.get("amount") or "0")), int(str(row.get("amountFilled") or row.get("filled") or "0"))
    except ValueError:
        try:
            amount, got = int(float(row.get("amount") or 0)), int(float(row.get("amountFilled") or 0))
        except (TypeError, ValueError):
            return 0.0, status
    if amount <= 0:
        return (ordered_shares if status in FILLED_STATUSES else 0.0), status
    if got <= 0 and status in FILLED_STATUSES:
        got = amount
    return min(ordered_shares, ordered_shares * got / amount), status


def order_final(row: dict, ordered_shares: float) -> bool:
    status = str(row.get("status") or "").upper()
    if status in OPEN_STATUSES:
        return False
    if status in FINAL_STATUSES:
        return True
    got, _ = order_fill(row, ordered_shares)
    return ordered_shares > 0 and got >= ordered_shares - 1e-9


def order_price(row: dict) -> float | None:
    for key in ("averagePrice", "avgPrice", "priceFilled", "filledPrice", "executedPrice", "averageFillPrice"):
        value = row.get(key)
        if value in (None, "", 0, "0"):
            continue
        with contextlib.suppress(TypeError, ValueError, decimal.InvalidOperation):
            price = D(str(value))
            if price > 1:
                price = price / 10 ** 18
            if 0 < price <= 1:
                return float(price)
    return None


# --- BNB chain (JSON-RPC): the balance, the allowance, the account's owner --------------------------------------------------------
class Chain:
    def __init__(self, rpc_url: str, wallet: Any):
        self.rpc_url, self.wallet = rpc_url, wallet
        self.addresses = wallet.addresses
        self.error = ""

    async def rpc(self, method: str, params: list) -> Any:
        loop = asyncio.get_running_loop()
        try:
            raw = await loop.run_in_executor(READ_POOL, http.request, self.rpc_url,
                                             {"jsonrpc": "2.0", "id": 1, "method": method, "params": params}, RPC_TIMEOUT, {})
            data = json.loads(raw)
        except RemoteError as error:
            self.error = clean_error(error)
            raise
        except (json.JSONDecodeError, UnicodeDecodeError):
            self.error = "RPC 未返回有效 JSON"
            raise RemoteError(self.error) from None
        if isinstance(data, dict) and data.get("error"):
            said = data["error"].get("message") if isinstance(data["error"], dict) else data["error"]
            self.error = f"RPC 错误：{brief_error(str(said or ''), 120)}"
            raise RemoteError(self.error)
        self.error = ""
        return data.get("result") if isinstance(data, dict) else None

    @staticmethod
    def selector(signature: str) -> bytes:
        from .wallet import keccak
        return keccak(signature.encode())[:4]

    async def call_uint(self, to: str, signature: str, *addresses: str) -> int:
        data = self.selector(signature) + b"".join(bytes(12) + bytes.fromhex(a[2:]) for a in addresses)
        result = await self.rpc("eth_call", [{"to": to, "data": "0x" + data.hex()}, "latest"])
        out = bytes.fromhex(str(result or "0x").removeprefix("0x"))
        return int.from_bytes(out[:32], "big") if out else 0

    async def usdt_balance(self) -> int:
        return await self.call_uint(self.addresses["USDT"], "balanceOf(address)", self.wallet.maker)

    async def bnb_balance(self) -> int:
        return int(str(await self.rpc("eth_getBalance", [self.wallet.signer, "latest"]) or "0x0"), 16)

    async def allowance(self, spender_key: str) -> int:
        return await self.call_uint(self.addresses["USDT"], "allowance(address,address)", self.wallet.maker, self.addresses[spender_key])

    async def account_owner(self, account: str) -> str:
        """The signer the ECDSA validator holds for a Predict account (0x000… when the account is unknown to it)."""
        data = self.selector("ecdsaValidatorStorage(address)") + bytes(12) + bytes.fromhex(account[2:])
        result = await self.rpc("eth_call", [{"to": self.addresses["ECDSA_VALIDATOR"], "data": "0x" + data.hex()}, "latest"])
        out = bytes.fromhex(str(result or "0x").removeprefix("0x"))
        return "0x" + out[12:32].hex() if len(out) >= 32 else "0x" + "0" * 40
