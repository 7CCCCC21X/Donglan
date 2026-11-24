"""The rule taker, in a process of its own (a port of huice rules.RuleTaker 1.154.27).

One loop works out the rules' windows of the next day, finds each one's market as Predict lists it, watches its matching
status and book before the start, buys what the rule allows, follows its orders to Predict's final word and settles each
window on Predict's result. What is kept from huice as it was: the windows and the prices, the sizes and the caps, the order
itself (a LIMIT at the rule's price, for all the window may still buy when enough is on offer; its rest withdrawn
FILL_READ_MS after its answer; signed to expire EXPIRY_BEFORE_S before the start — in the last two minutes, when Predict
would refuse that, two minutes past it: expiry_for), the hash on disk before the request
leaves, an answer gone missing (or a 409) looked for by the hash and never sent again, the late fills, the fee taken in
shares, the race's clock (Predict's stamps on the books), the per-second hours, the makers' moment, the wait for matching to
open, the in-play rules.

What is new, for speed (README.md: 提速):
- the process does nothing else: no model trader, no pre-open mode, no review page sharing its loop (huice measured that
  loop's lag up to seconds: the σ recalibration, the review page's recomputing, a 24,000-order snapshot);
- a pushed book is looked at in the very turn that brought it, by a look of that window alone (a look of another window
  waiting for a read no longer holds it up: each window looks by itself, and the day's cap and the balance are counted
  again in the same step that records the order);
- the order a window would place when the makers' ladder shows is signed beforehand (the same salt, the same amounts: the
  same order), its request handed to a thread of the orders' own the moment the book shows the offer;
- the connection to Predict is kept warm through the busy stretches (a read every second or two: the order never waits for a
  TCP + TLS handshake), automatic garbage collection is held off around the race's seconds;
- and it runs where Predict is (Google Cloud Tokyo: deploy/gcp)."""
from __future__ import annotations

import asyncio
import collections
import contextlib
import dataclasses
import datetime as dt
import gc
import hashlib
import os
import queue
import statistics
import sys
import threading
import time
import urllib.parse
from decimal import Decimal as D
from typing import Any

from . import VERSION
from . import http as nethttp
from .clock import CALENDAR, day_spans_text
from . import console
from .config import Config, RuleSettings
from .predict import (API_BASES, PREDICT_REST, RPC_URLS, Chain, NotSent, OrderRejected, PredictApi, book_up_first,
                      category_market, http_status, market_start_price, order_fill, order_final, order_price,
                      outcome_for_side, parse_market, predict_resolution, won_index_set, delay_fields, DUPLICATE_WORDS, DOWN_WORDS,
                      UP_WORDS)  # noqa: F401
from .rules import (FINAL_STATES, MARKET_NAMES, MODES, NEVER_PLACED, PREFIX, RACES_SHOWN, RULE_OF, RULES, WATCH, Rule, avg_price,
                    claims, cost, course_part, course_speed, course_stats, filled, held, in_flight, lead_note, live_price_of,
                    live_rule, net, offered, open_cost, open_orders, race_summary, race_text, races, rid, speed_stats, spent_on,
                    walk, watch_stats, windows, without, day_pnl)
from .util import DAY_MS, DUST, LOG, NotOnDisk, bj, clean_error, day_of, dumps_bytes, fmt, money
from .wallet import MAX_SALT, Wallet, build_order, exchange_key, from_wei, limit_amounts, native_signing, to_wei

TICK_S = 1.0                  # the loop's pace
PLAN_EVERY_MS = 60_000        # the rules' windows of the next day worked out again this often (and at once after a settings change)
HORIZON_MS = 24 * 3_600_000 + 10 * 60_000  # windows followed this far ahead: Predict lists a window a day before its start
LISTED_AHEAD_MS = 24 * 3_600_000  # ...about 85 s after it enters that day (11-06): its category is looked for from then
PROBE_EARLY_MS = 120_000      # the listing is looked for from this long before it is expected
PROBE_FAST_S, PROBE_SLOW_S = 2.0, 30.0  # ...every PROBE_FAST_S until PROBE_LATE_MS past the moment, then every PROBE_SLOW_S
PROBE_LATE_MS = 5 * 60_000
NEAR_MS = 15 * 60_000         # the last stretch before the start: the status and the book read every FAST_S (matching opens then)
FAST_S, SLOW_S, CLOSED_SLOW_S = 2.0, 30.0, 60.0
OFFER_STATUS_S = 10.0         # an offer at the rule's price on a market not yet open for matching: its status read this often...
STATUS_RUSH_S, STATUS_RUSH_MS, RUSH_READS_MAX = 0.25, 20_000, 240  # ...and every STATUS_RUSH_S beside the looks for 20 s (rush)
QUIET_BEFORE_MS, QUIET_AFTER_MS = 1_000, 15_000  # the race's seconds around a makers' moment (no garbage collection then)
GC_BEFORE_MS = 5_000
COURSE_QUIET_MS = 12_000      # ...and around the beginning of a window's course
RUSH_QUIET_MS = 3_000
INDEX_FRESH_MS = 1_500        # an index older than this as it comes brings no look of its own
STOP_MS = 15_000              # nothing placed this close to the start: the rule is about the window before it begins
EXPIRY_BEFORE_S = 5           # an order's signed expiry: this long before the start (in the last two minutes: expiry_for)
PREDICT_EXPIRY_MIN_S = 120    # Predict takes no order signed to expire sooner than this after it arrives ("expiry is too soon")
EXPIRY_SLACK_S = 10           # ...this much more for the clocks and the way there
SEND_MIN_LEFT_MS = (PREDICT_EXPIRY_MIN_S + 2) * 1000  # none sent with less than this left to its expiry: Predict would refuse it
FILL_READ_MS = 1_500          # an order is read this long after its answer; what has not filled then is withdrawn
LATE_FILL_MS = 60_000         # an order off the book is final only by a read this long after it left
REST_MIN_S = 0.1
REST_SOON_S = 0.005
FOLLOW_EVERY_MS = 1_000
UNKNOWN_EVERY_MS = 10_000
GIVE_UP_MS = 15 * 60_000
REJECT_PAUSE_MS = 30_000
RACE_WAIT_S = 30.0            # the status text and the CSV wait out the race's seconds this long at the most
CONNECT_RETRY_MS = 2_000      # an order that got no connection (nothing sent): the window looks again this soon
MAX_REJECTS = 5
SETTLE_AFTER_MS = 120_000
SETTLE_EVERY_MS = 60_000
KEEP_DAYS = 90
WARM_MS = 60_000              # the per-second hours are prepared for this long before they begin...
WARM_EVERY_MS = 30_000        # ...and the preparation renewed this often through them
FAST_LOOK_S = 1.0
FAST_STALE_S = 20.0
VERIFY_AGAIN_S = 1.0  # the REST reads beside the pushes: a window not again within a second (verify_every(): how often at all)
LIVE_START_MS = 3_000         # 盘中: not in the window's first moments (the start line is read then)...
LIVE_FULL_MS = 60_000         # ...in its first minute no dearer than before the window...
LIVE_STOP_MS = 15_000         # ...nothing placed this close to its end
SWEEP_SHARES = 20             # this much on offer at the rule's price (and a quarter of what the window may still buy)...
SWEEP_PART = 4                # ...and the order is for all the window may still buy
MAX_OPEN_ORDERS = 3
JOURNAL_SUFFIX = ".rule-orders"
LAG_EVERY_S = 0.1
SPRINT_MS = 15 * 60_000       # a window sprints from WARM_MS before the makers' moment to this long after it...
HOT_BEFORE_MS, HOT_AFTER_MS = 10_000, 120_000  # ...and around the moment itself its book is the one read over REST
ALLOWANCE_MS = 60_000
ALLOWANCE_UNLIMITED, ALLOWANCE_LONG_MS = 10 ** 40, 3_600_000
RECHECKED = ("规则吃单的设置刚改过", "盘中：")
HELD_BACK_MAX = 3
CONFLICT_GRACE_MS = 90_000
CONFLICT_MISSES = 6
LAST_WITHDRAW_S = 4.0
BALANCE_BUSY_MS, BALANCE_IDLE_MS = 20_000, 120_000  # the USDT balance read again this often (busy: an order may go out soon)
BALANCE_LAG_MS = 10_000       # ...and an order placed this long before a read began is taken as not in it yet (usd)
RECENT_MS = 2 * DAY_MS        # the records an order's checks sum over (usd, the day's spend): those of the last two days' windows
SEND_WAIT_S = 25.0            # at a stop: the orders on their way given this long for their answers (an order's request: 20 s at most)
READY_RETRY_MS = 60_000
STRIKE_EVERY_MS, STRIKE_FOR_MS = 5_000, 180_000  # 盘中: Predict's start price asked for this often, this long after the start
WARM_CONN_S = 2.0             # through the busy stretches a pooled connection to Predict is kept used within this
SIGN_CACHE = 64
EAGER = sys.version_info >= (3, 12)


def start_now(coro: Any, name: str) -> "asyncio.Task":
    """A task whose coroutine runs now, in this turn of the loop, up to its first real wait (Python 3.12's eager start)."""
    loop = asyncio.get_running_loop()
    if EAGER:
        return asyncio.Task(coro, loop=loop, name=name, eager_start=True)
    return loop.create_task(coro, name=name)


MODE_RANK = {"off": 0, "paper": 1, "on": 2}
TEST_LOOKUPS, TEST_LOOKUP_S = 4, 1.5   # an order test whose answer was lost: looked for by its hash this often, this far apart


CALENDAR_WARN_DAYS = 30  # the CME holiday calendar's last days, told in /check, the status and once a day
EXCHANGES = [exchange_key(n, y) for y in (False, True) for n in (False, True)]
EXCHANGE_NAMES = {"CTF_EXCHANGE": "标准交易所", "NEG_RISK_CTF_EXCHANGE": "多结果交易所", "YIELD_BEARING_CTF_EXCHANGE": "收益型交易所",
                  "YIELD_BEARING_NEG_RISK_CTF_EXCHANGE": "收益型多结果交易所"}
_SYNCS: queue.SimpleQueue | None = None


def journal_sync(fd: int, taker: Any) -> None:
    """A journal's lines made durable (fdatasync) on a thread of its own — never in the order's path: asked right after the
    line is written, done while the request is on its way."""
    global _SYNCS
    if _SYNCS is None:
        _SYNCS = queue.SimpleQueue()
        threading.Thread(target=_sync_journals, args=(_SYNCS,), name="rt-journal-sync", daemon=True).start()
    _SYNCS.put((fd, taker))


def _sync_journals(asked: queue.SimpleQueue) -> None:
    while True:
        fd, taker = asked.get()
        with contextlib.suppress(OSError):
            os.fdatasync(fd)
        taker.journal_syncs += 1


def settle_every(since_end_ms: int) -> int:
    """How often a window's settlement is read: every minute at first; still unsettled an hour past its end, every ten minutes;
    a day past it, every hour (a market Predict is slow to settle, or settles in words not understood, costs a read an hour)."""
    if since_end_ms < 3_600_000:
        return SETTLE_EVERY_MS
    return 10 * 60_000 if since_end_ms < DAY_MS else 3_600_000


def paid(rec: dict, resolution: dict, won: int | None) -> float | None:
    """What a share the record holds paid at the settlement: by the outcome it bought (its indexSet) — the winning outcome's
    name read only for a record without one (written before it was kept); a split pays half. None when it cannot be told."""
    if resolution.get("split"):
        return 0.5
    held = int(rec.get("index_set") or 0)
    if held > 0 and won is not None:
        return 1.0 if won == held else 0.0
    name = str(resolution.get("name") or "").strip().lower()
    up = 1.0 if name in UP_WORDS else 0.0 if name in DOWN_WORDS else None
    if up is None:
        return None
    return up if rec.get("side") == "up" else 1 - up


# How close to a race (a per-second stretch, the makers' moment, a window's last minutes) /speed still runs: it takes a quarter of
# the key's budget for about a minute.
SPEED_CLEAR_MS = 3 * 60_000


def key_tail(key: str) -> str:
    """The last four characters of a key: enough to tell which one is meant, too few to use."""
    return str(key)[-4:]


def key_budget_line(limits: str, ours: int) -> tuple[bool, str] | None:
    """The key's use as Predict counts it (its rate-limit headers) against our own requests of the same minute: another program
    on the same key shows as the difference (Railway's huice lending it out: RULE_KEY_LEND). (a warning?, the line); None
    without Predict's figures."""
    budget = nethttp.budget_of(limits)
    if budget is None:
        return None
    limit, left = budget
    used = max(limit - left, 0)
    others = used - ours
    text = f"API Key 额度：Predict 记这一分钟用了 {used} 次（上限 {limit}），我们这一分钟发了 {ours} 次"
    if others > max(limit // 10, 30):
        return True, (f"{text}：另外约 {others} 次是别的程序在用同一把 Key（多半是 Railway 上的 huice：它的主 Key 就是这把的话，"
                      "这边换一把别的程序不用的——先 rt.sh set PREDICT_RATE_PER_MIN 220，再 rt.sh set RULE_API_KEY；"
                      "是它的 RULE_API_KEY 借用这把的话，删掉那边的 RULE_API_KEY，或那边设 RULE_KEY_LEND=off）"
                      "——一撞上限流，这里所有请求停 20 秒以上，赶上抢单时刻就下不了单")
    return False, text


def placed_or_not(error: BaseException) -> str:
    """What a failed order request tells about the order: "not_sent" (a local gate, the key's cooldown, a 429), "rejected"
    (Predict refused it outright: a 4xx that is no conflict nor a duplicate), "unknown" (a timeout, a lost answer, a 5xx, a
    409 — it may be there: looked for by its hash, never sent again)."""
    if isinstance(error, NotSent):
        return "not_sent"
    text = clean_error(error) or type(error).__name__
    status = http_status(error)
    if isinstance(error, RuntimeError) and "shutdown" in str(error):
        return "not_sent"
    if status == 429 or "限流冷却中" in text:
        return "not_sent"
    definite = isinstance(error, OrderRejected) or 400 <= status < 500
    if definite and status != 409 and not any(word in text.lower() for word in DUPLICATE_WORDS):
        return "rejected"
    return "unknown"


def settings_text(view: list) -> str:
    """/settings as text (where no buttons can be shown): each field's value and its name for /set."""
    lines = ["⚙️ 设置（改：/set 设置名 数值；回到设置文件的值：/reset 设置名）"]
    for group in view:
        lines.append(group["group"])
        lines += [f"· {f['label']}：{f['value']}（{f['key']}）" + (" •" if f.get("saved") else "") for f in group["fields"]]
    return "\n".join(lines)


def review_text(data: dict, days: int) -> str:
    """The review page's numbers as a message: the totals, each rule, the last days, the last windows."""
    if data.get("busy"):
        return f"📈 {data['busy']}"
    lines = [f"📈 复盘（{'全部' if not days else f'近 {days} 天'}，按开场时间算）"]
    shown_any = False
    for mode, name in (("on", "真实"), ("paper", "只记录")):
        t = (data.get("totals") or {}).get(mode) or {}
        if not t.get("windows"):
            continue
        shown_any = True
        roi = f"，收益率 {t['roi'] * 100:+.1f}%" if t.get("roi") is not None else ""
        still = f"；还有 {t['open']} 场没结算" if t.get("open") else ""
        lines.append(f"{name}：买了 {t['windows']} 场 {fmt(t['shares'])} 份，花 ${t['cost']:.2f}；已结算 {t['settled']} 场（赢 {t['wins']} 输 "
                     f"{t['losses']}），盈亏 {money(t['pnl'])}{roi}{still}")
    if not shown_any:
        lines.append("这段时间还没有买到过（真实和只记录都没有）")
    rules = [r for r in data.get("rules") or [] if r.get("windows")]
    if rules:
        lines.append("按规则：")
        lines += [f"· {r['name']}（{'真实' if r['mode'] == 'on' else '只记录'}）：{r['windows']} 场，赢 {r['wins']} 输 {r['losses']}，"
                  f"盈亏 {money(r['pnl'])}" for r in rules]
    daily = list(reversed((data.get("daily") or {}).get("on") or []))[:7]
    if daily:
        lines.append("最近几天（真实）：")
        lines += [f"  {d['day']}：{d['windows']} 场 {money(d['pnl'])}（累计 {money(d['cum'])}）" for d in daily]
    recent = (data.get("recent") or [])[:5]
    if recent:
        lines.append("最近几场：")
        for r in recent:
            result = {"up": "涨", "down": "跌", "split": "平"}.get(r.get("result") or "", "待结算")
            pnl = f" {money(r['pnl'])}" if r.get("pnl") is not None else ""
            lines.append(f"  {bj(int(r.get('start_ms') or 0))} {r.get('window') or ''}{'（只记录）' if r.get('mode') == 'paper' else ''} "
                         f"买{'涨' if r.get('side') == 'up' else '跌'} {fmt(r.get('shares') or 0)} 份 均价 {float(r.get('avg') or 0) * 100:.1f}¢ → {result}{pnl}")
    lines.append("其他天数：/review 7、/review all；图表和 CSV 在网页控制台")
    return "\n".join(lines)


def expiry_for(line_ms: int, stop_ms: int, now_ms: int) -> int:
    """An order's signed expiry (s) for a window's line — its start before it, its end in its course: EXPIRY_BEFORE_S before
    the line; once that is too soon for Predict (under PREDICT_EXPIRY_MIN_S + EXPIRY_SLACK_S from now: the last two minutes),
    late enough for the last order that may still go out (``stop_ms`` before the line) — one value for all of them, so an
    order signed beforehand is still the one the look signs. It then outlasts the line; what does not fill at once is
    withdrawn at the follow, FILL_READ_MS after the answer, as always."""
    line = line_ms // 1000
    early = line - EXPIRY_BEFORE_S
    if early * 1000 - now_ms >= (PREDICT_EXPIRY_MIN_S + EXPIRY_SLACK_S) * 1000:
        return early
    return line - stop_ms // 1000 + PREDICT_EXPIRY_MIN_S + EXPIRY_SLACK_S


class RuleTaker:
    def __init__(self, config: Config, store: Any, *, api: Any = None, wallet: Any = None, chain: Any = None, stream: Any = None,
                 index: Any = None, telegram: Any = None, clock: Any = None, base_env: dict | None = None):
        from .stream import BookStream
        self.config = config
        self.store = store
        self.clock = clock
        self.env_mode = config.rules.mode
        saved = str(store.get("control:mode") or "")
        mode = saved if saved in MODE_RANK and MODE_RANK[saved] < MODE_RANK[self.env_mode] else self.env_mode
        # the control page's saved settings over the settings file's (console.py); one that no longer parses is dropped and told
        self.base_env = {k: str(v) for k, v in (base_env if base_env is not None else console.current(config.rules, config.max_daily_loss_usd)).items()
                         if k.startswith("RULE_") and k != "RULE_TAKER"}
        settings, self.loss_cap, self.saved_settings, self.settings_dropped = console.settle_saved(
            self.base_env, dict(store.get(console.SAVED) or {}), mode)
        if self.settings_dropped:
            store.put(console.SAVED, self.saved_settings)
        self.settings: RuleSettings = settings
        self.review_held: dict = {}
        self.flush_wait_s = 10.0    # how long an operator's change (or an order with no journal line) waits for the disk
        self.lookup_s = TEST_LOOKUP_S
        self.wallet = wallet if wallet is not None else (Wallet(config.private_key, config.chain_id, config.account) if config.private_key else None)
        base = API_BASES.get(config.chain_id, PREDICT_REST)
        self.api = api if api is not None else PredictApi(base, config.api_key, self.wallet, config.rate_per_min).with_spares(config.api_spares)
        self.api.before_send = self.before_send
        self.chain = chain if chain is not None else (Chain(config.rpc_url or RPC_URLS.get(config.chain_id, RPC_URLS[56]), self.wallet)
                                                     if self.wallet is not None else None)
        self.stream = stream if stream is not None else BookStream(config.api_key, config.ws_connections)
        self.index = index
        self.telegram = telegram
        self.paused = str(store.get("control:paused") or "")
        floor = store.get("control:loss_floor")
        self.loss_floor_at = floor if isinstance(floor, dict) else {}
        stop = store.get("control:loss_stop")
        self.loss_stop = stop if isinstance(stop, dict) else {}  # the day the loss stop was reached: it holds that whole day
        # readiness (real orders): the sign-in, the Predict account's owner on-chain, the balance
        self.ready_error = "尚未登录 Predict"
        self.account_checked = not (self.wallet is not None and self.wallet.predict_account)
        self.account_error = ""
        self.ready_ms = 0
        self.usdt: int | None = None
        self.bnb: int | None = None
        self.balance_ms = 0
        self.balance_asked_ms = 0
        self.balance_error = ""
        self.allowance: dict[str, tuple[int, int]] = {}
        # the loop
        self.fast = False
        self.warm_ms = 0
        self.warmed = False
        self.warming: asyncio.Task | None = None
        self.verifying: asyncio.Task | None = None
        self.verify_at = 0.0
        self.verifies = self.verify_diffs = self.fast_looks = self.status_reads = 0
        self.first_ticks = self.index_looks = self.index_late = 0
        self.verify_error = ""
        self.verify_ms: collections.deque = collections.deque(maxlen=120)
        self.loop_lags: collections.deque = collections.deque(maxlen=int(60 / LAG_EVERY_S))
        self.side_tasks: set = set()
        self.sends: set = set()
        self.gc_off = False
        self.frozen_ms = 0
        self.running = False
        self.started_ms = 0
        self.ticks = 0
        self.looked = 0
        self.track: dict[str, dict] = {}
        self.by_market: dict[str, str] = {}  # market id -> window slug
        self.planned_ms = 0
        self.sending: dict[str, str] = {}    # hash -> slug while its request is out
        self.decided: dict[str, Any] = {}    # ...and the settings its look decided it under
        self.signed: collections.OrderedDict = collections.OrderedDict()  # the orders signed beforehand (sign)
        self.presigned = self.presign_hits = 0
        self.journal_fd: int | None = None
        self.journal_cut = False
        self.journal_syncs = 0       # the journal made durable behind the orders (journal_sync), so many times
        self.testing = False         # an order test under way (one at a time: a second asked meanwhile is told so, not run)
        self.speeding = False        # a /speed measurement under way (one at a time)
        # the last day of the CME holiday calendar (CME_CLOSED_DAYS / CME_EARLY_DAYS, as set at the start), and the Beijing day
        # its running out was last told
        self.cal_last = CALENDAR.last_day()
        self.cal_told = ""
        self.errors: collections.deque = collections.deque(maxlen=20)
        self.error = ""
        self.stop = asyncio.Event()
        self.wakeup = asyncio.Event()
        self.reads = 0
        self.region = config.region
        self.recs: dict[str, dict] = {k.removeprefix(PREFIX): v for k, v in store.items(PREFIX) if isinstance(v, dict)}
        self.recover()
        # the records the hot path sums (the balance spent, the day's spend) are a few: the windows of the last two days — kept
        # apart from the 90 days the store holds, so an order's checks walk a dozen records, not hundreds
        self.recent_keys: set[str] = {k for k, r in self.recs.items() if int(r.get("start_ms") or 0) >= self.now() - RECENT_MS}
        self.pnl_epoch = 0          # bumped by each settlement: the day's P&L (the loss stop) summed again only then
        self.pnl_held: tuple = ()
        self.stream.on_change = self.on_change
        if self.index is not None:
            self.index.on_change = self.on_index
            self.index.needed = self.index_needed
            self.index.wanted = self.index_wanted

    # -- plumbing --
    def now(self) -> int:
        return int(self.clock()) if self.clock is not None else int(time.time() * 1000)

    def record(self, slug: str, mode: str | None = None) -> dict | None:
        return self.recs.get(rid(slug, mode or self.settings.mode))

    def save(self, rec: dict) -> None:
        key = rid(rec["slug"], rec["mode"])
        self.recs[key] = rec
        if int(rec.get("start_ms") or 0) >= self.now() - RECENT_MS:
            self.recent_keys.add(key)
        self.store.put(PREFIX + key, rec)

    def recent(self) -> dict[str, dict]:
        """The records of the last two days' windows (every order placed today or yesterday is on one of them)."""
        return {k: r for k in list(self.recent_keys) if (r := self.recs.get(k)) is not None}

    def today_pnl(self) -> float:
        """The real records' P&L settled today (Beijing): summed again after a settlement, a new record, a new day."""
        mark = (day_of(self.now()), self.pnl_epoch, len(self.recs))
        if not self.pnl_held or self.pnl_held[0] != mark:
            self.pnl_held = (mark, day_pnl(self.recs, mark[0]))
        return self.pnl_held[1]

    def note(self, text: str) -> None:
        self.error = text
        if text:
            self.errors.append((self.now(), text))
            LOG.warning("规则吃单：%s", text)

    def notify(self, text: str) -> None:
        LOG.info("%s", text)
        if self.telegram is not None and self.config.notify:
            with contextlib.suppress(Exception):
                self.telegram.send(text)

    def alarm(self, text: str) -> None:
        """A problem the operator must see: pushed whatever RULE_NOTIFY says."""
        LOG.warning("%s", text)
        if self.telegram is not None:
            with contextlib.suppress(Exception):
                self.telegram.send(text)

    def cooling(self) -> float:
        return float(getattr(self.api, "blocked_until", 0.0)) - time.monotonic()

    def usd(self, excluding: dict | None = None) -> float | None:
        """The USDT this taker may still spend: the balance last read on-chain, less what that read could not have seen (a fill
        reaches the chain a few seconds after it is matched: BALANCE_LAG_MS) — an order placed, or filled, since just before
        the read began: its fills at their price once its word is final, its limit in full while not; any other order not
        final yet: its unfilled rest at its limit (resting, or its answer lost: it may still take the money) — until a read
        begun GIVE_UP_MS past the order's signed expiry (at its window's start, or its end: Predict matches nothing after it,
        so that read has all it ever took, however long its answer stays lost). A window a day older than the read holds no
        order that may take anything: not looked at. None before a read. ``excluding``: an order not to count (before_send
        judges an order against what is left)."""
        if self.usdt is None:
            return None
        since = self.balance_asked_ms - BALANCE_LAG_MS
        spent = 0.0
        for rec in self.recent().values():
            if rec.get("mode") != "on" or int(rec.get("start_ms") or 0) < since - DAY_MS:
                continue
            for o in rec.get("orders") or []:
                if o is excluding or o.get("paper") or o.get("state") in NEVER_PLACED:
                    continue
                unseen = int(o.get("at") or 0) >= since or int(o.get("filled_ms") or 0) >= since
                expires = int(o.get("expires") or 0)
                expired = expires > 0 and since >= expires * 1000 + GIVE_UP_MS
                shares, done, limit = float(o.get("shares") or 0), float(o.get("filled") or 0), float(o.get("limit") or 0)
                if o.get("state") in FINAL_STATES or expired:
                    spent += done * float(o.get("avg") or o.get("limit") or 0) if unseen else 0.0
                else:
                    spent += shares * limit if unseen else max(shares - done, 0.0) * limit
        return from_wei(self.usdt) - spent

    @property
    def ready(self) -> bool:
        return self.wallet is not None and not self.ready_error and not self.account_error and self.account_checked

    @property
    def not_ready_why(self) -> str:
        if self.wallet is None:
            return "没有设置 PREDICT_PRIVATE_KEY"
        return self.ready_error or self.account_error or ("Predict 账户的链上核对还没完成" if not self.account_checked else "")

    def healthy(self) -> bool:
        return self.running and not self.stop.is_set()

    def configure(self, settings: RuleSettings) -> None:
        self.settings = settings
        self.planned_ms = 0
        self.signed.clear()

    def index_market(self, slug: str, w: dict) -> None:
        if w.get("market") is not None:
            self.by_market[str(w["market"]["id"])] = slug

    # -- the gates --
    def loss_floor(self) -> float | None:
        """The day's settled P&L at which no new order goes out (RULE_MAX_DAILY_LOSS_USD below zero, or below the P&L at which
        the operator resumed after a stop that day); None with no cap."""
        cap = self.loss_cap
        if not cap:
            return None
        today = day_of(self.now())
        if self.loss_floor_at.get("day") == today:
            return float(self.loss_floor_at.get("floor") or -cap)
        return -cap

    def hold_loss_stop(self) -> None:
        """The day's loss stop, once reached, holds for the rest of the (Beijing) day: a profit settled afterwards does not lift
        it — /resume does (another cap from the P&L then), and the next day. Latched and saved here: where the P&L or the
        floor changes (a settlement, a saved setting) and at every turn."""
        floor = self.loss_floor()
        if floor is None:
            return
        today = day_of(self.now())
        if self.loss_stop.get("day") == today:
            return
        pnl = self.today_pnl()
        if pnl <= floor:
            self.loss_stop = {"day": today, "pnl": pnl, "floor": floor, "at": self.now()}
            self.store.put("control:loss_stop", self.loss_stop)

    def loss_stop_why(self) -> str:
        """Why the day's loss stop holds now ("" when it does not): the P&L at the floor, or there earlier today."""
        floor = self.loss_floor()
        if floor is None:
            return ""
        pnl = self.today_pnl()
        if pnl <= floor:
            return f"今日已结算 {money(pnl)} 已到 RULE_MAX_DAILY_LOSS_USD 的停机线（{money(floor)}），今天不再开新单"
        if self.loss_stop.get("day") == day_of(self.now()):
            return (f"今日已结算到过 RULE_MAX_DAILY_LOSS_USD 的停机线（{money(floor)}，当时 {money(float(self.loss_stop.get('pnl') or 0))}）："
                    f"之后的盈利不解除，今天不再开新单（现在 {money(pnl)}；/resume 从现在的盈亏起再给一个额度）")
        return ""

    def block(self, usd: float, excluding: dict | None = None) -> str:
        """Why no real order goes out now ("" to send): the mode, the deploy's switch, the pause, the day's loss stop, the
        readiness, the records' store, the API's cooldown, the balance (spendable beside ``excluding``)."""
        if self.settings.mode != "on":
            return f"规则吃单是“{MODES.get(self.settings.mode, self.settings.mode)}”，不下单"
        if not self.config.trading_allowed:
            return "部署变量 LIVE_TRADING_ALLOWED 不是 on，不下单"
        if self.paused:
            return f"已暂停（{self.paused}）"
        if stop := self.loss_stop_why():
            return stop
        if why := self.calendar_why(self.now()):
            return why
        if not self.ready:
            return f"真实交易未就绪：{self.not_ready_why}"
        if error := str(getattr(self.store, "error", "") or ""):
            return f"记录写不进数据库（{error}），不开新单"  # (an order nothing could find after a restart is not sent)
        if (left := self.cooling()) > 0:
            return f"Predict 接口限流冷却中（{int(left) + 1} 秒后重试）"
        usd_left = self.usd(excluding)
        if usd_left is not None and usd_left < usd - 1e-9:
            return f"USDT 余额 ${usd_left:,.2f} 不足本单 ${usd:,.2f}"
        return ""

    def calendar_left(self, now: int) -> int | None:
        """Days the CME holiday calendar still covers from ``now`` (0 once its last listed day is over); None without one."""
        if self.cal_last is None:
            return None
        return max((self.cal_last - dt.date(1970, 1, 1)).days + 1 - now // DAY_MS, 0)

    def calendar_why(self, now: int) -> str:
        """Why no real order goes out for the calendar: its last listed day over, a holiday after it is not known (a day
        missing trades a standing index). "" while it covers today."""
        if self.calendar_left(now) == 0:
            return (f"CME 休市日历只排到 {self.cal_last.isoformat()}，之后哪天休市不知道（错过一天就会在不动的指数上下单）："
                    "在设置文件补上之后的 CME_CLOSED_DAYS、CME_EARLY_DAYS 再重启")
        return ""

    def halted(self) -> bool:
        """No real order may be opened now by the operator's word or the service's stop: the mode, the deploy's switch, the
        pause, the loss stop, the process ending."""
        if self.settings.mode != "on" or not self.config.trading_allowed or self.paused or self.stop.is_set():
            return True
        return bool(self.loss_stop_why())

    def before_send(self, digest: str) -> str:
        """Judged right before an order's bytes go out: the gates again, the settings the look decided under, the window's start
        not passed, in a window's course the index still on the rule's side."""
        digest = str(digest or "").lower()
        if digest.startswith("test:"):  # the operator's order test: the hard switch, the pause, the stop
            return self.test_gate()
        slug = self.sending.get(digest)
        if slug is None:
            return "不是规则吃单这一刻要发的订单"
        if self.stop.is_set():
            return "服务正在停止，不再发单"
        decided = self.decided.get(digest)
        if decided is not None and decided != self.settings:
            return "规则吃单的设置刚改过，这张单不发了（下一轮按新设置重新判断）"
        rec = self.record(slug, "on") or {}
        order = next((o for o in rec.get("orders") or [] if o.get("hash") == digest), None)
        if order is None:
            return "订单记录不见了"
        now = self.now()
        if order.get("live"):
            if now > int(rec.get("end_ms") or 0) - LIVE_STOP_MS:
                return "离这一场结束不足 15 秒，盘中的单不再发送"
        elif now > int(rec.get("start_ms") or 0) - STOP_MS:
            return "离开场不足 15 秒，这张单不再发送（它在路上等了太久）"
        if int(order.get("expires") or 0) * 1000 - now < SEND_MIN_LEFT_MS:
            return "距订单签的到期时间不足 2 分钟（Predict 不收这样的单）：这张不发了，下一轮重新签"
        if why := self.block(float(order.get("shares") or 0) * float(order.get("limit") or 0), excluding=order):
            return why
        if order.get("live") and (rule := RULE_OF.get(rec.get("rule"))) is not None:
            try:
                why = self.live_guard(slug, rule, self.now())
            except Exception as error:
                why = f"发单前复核指数出错（{type(error).__name__}）"
            if why:
                return f"盘中：{why}"
        return ""

    # -- readiness --
    async def prepare(self) -> None:
        """Real orders' readiness: the sign-in, the Predict account checked on-chain (its key must be the account's owner)."""
        if self.wallet is None:
            return
        try:
            await self.api.ensure_auth()
            self.ready_error, self.ready_ms = "", self.now()
        except asyncio.CancelledError:
            raise
        except Exception as error:
            self.ready_error = clean_error(error) or type(error).__name__
            if http_status(error) in {401, 403} and not self.config.api_key:
                self.ready_error += "（可能需要 PREDICT_API_KEY）"
        if self.wallet.predict_account and self.chain is not None:
            try:
                owner = await self.chain.account_owner(self.wallet.predict_account)
            except asyncio.CancelledError:
                raise
            except Exception as error:
                if not self.account_checked:
                    self.account_error = f"无法在链上核对 PREDICT_ACCOUNT 的控制钥匙：{clean_error(error) or type(error).__name__}"
                return
            if int(owner, 16) == 0:
                self.account_error = "链上没有这个 Predict 账户的签名记录：地址填错，或账户还没在链上激活（先在网站交易一次）"
            elif owner.lower() != self.wallet.signer.lower():
                self.account_error = "PREDICT_PRIVATE_KEY 不是 PREDICT_ACCOUNT 这个 Predict 账户的控制钥匙：请导出该账户的 Privy 钱包私钥"
            else:
                self.account_error = ""
            self.account_checked = not self.account_error

    async def refresh_balance(self) -> None:
        if self.chain is None:
            return
        asked = self.now()
        try:
            self.usdt, self.bnb = await asyncio.gather(self.chain.usdt_balance(), self.chain.bnb_balance())
            self.balance_ms, self.balance_asked_ms, self.balance_error = self.now(), asked, ""
        except asyncio.CancelledError:
            raise
        except Exception as error:
            self.balance_error = clean_error(error) or type(error).__name__
            self.balance_ms = self.now() - BALANCE_IDLE_MS + 15_000  # (again in 15 s)

    async def ready_loop(self) -> None:
        """Signed in, checked, the balance read — and kept so: retried while not ready, the balance read again every
        BALANCE_BUSY_MS while an order may go out soon (BALANCE_IDLE_MS otherwise)."""
        while not self.stop.is_set():
            if self.settings.mode == "on" and self.wallet is not None:
                now = self.now()
                if (not self.ready or not self.api.fresh()) and now - self.ready_ms >= (READY_RETRY_MS if self.ready_ms else 0):
                    self.ready_ms = now
                    await self.prepare()
                busy = self.busy_in(now)
                every = BALANCE_BUSY_MS if busy is not None and busy <= WARM_MS else BALANCE_IDLE_MS
                if now - self.balance_ms >= every:
                    await self.refresh_balance()
            with contextlib.suppress(asyncio.TimeoutError):
                await asyncio.wait_for(self.stop.wait(), 2.0)

    # -- the loop --
    async def run(self) -> None:
        self.stream.on_change = self.on_change
        if self.index is not None:
            self.index.on_change = self.on_index
            self.index.needed = self.index_needed
            self.index.wanted = self.index_wanted
        side = [asyncio.create_task(self.stream.run(), name="rules-stream"), asyncio.create_task(self.lag_loop(), name="rules-lag"),
                asyncio.create_task(self.ready_loop(), name="rules-ready")]
        if self.index is not None:
            side.append(asyncio.create_task(self.index.run(), name="rules-index"))
        self.running, self.started_ms = True, self.now()
        with contextlib.suppress(Exception):
            gc.collect()
            gc.freeze()  # the records and the modules as loaded: never walked by a collection again
            self.frozen_ms = self.now()
        try:
            while not self.stop.is_set():
                try:
                    await self.turn()
                except asyncio.CancelledError:
                    raise
                except Exception as error:
                    self.note(clean_error(error) or type(error).__name__)
                await self.rest()
            self.running = False
            pending = [t for t in self.sends if not t.done()]
            if pending:  # an order's answer still on its way: its record made whole (and its rest withdrawn: halted) before the process goes
                await asyncio.wait(pending, timeout=SEND_WAIT_S)
            with contextlib.suppress(Exception):
                await asyncio.wait_for(self.withdraw_all("服务停止"), LAST_WITHDRAW_S)
            with contextlib.suppress(Exception):
                await asyncio.to_thread(self.store.flush, 10.0)
        finally:
            self.running = False
            if self.gc_off:
                self.gc_off = False
                gc.enable()
            self.stream.stop()
            if self.index is not None:
                self.index.stop()
            extra = [t for t in (self.warming, self.verifying) if t is not None]
            for task in [*side, *self.side_tasks, *extra]:
                task.cancel()
            await asyncio.gather(*side, *self.side_tasks, *extra, return_exceptions=True)

    async def lag_loop(self) -> None:
        while True:
            began = time.monotonic()
            await asyncio.sleep(LAG_EVERY_S)
            self.loop_lags.append(max(0.0, (time.monotonic() - began - LAG_EVERY_S) * 1000))

    def rest_s(self) -> float:
        return TICK_S if not self.stream.changed else REST_SOON_S if self.stream.arrivals != self.looked else REST_MIN_S

    async def rest(self) -> None:
        self.stream.pushed.clear()
        if self.wakeup.is_set():  # (set while the turn ran — an order's first read is due: the next turn at once)
            self.wakeup.clear()
            await asyncio.sleep(0)
            return
        waits = {asyncio.ensure_future(self.stop.wait()), asyncio.ensure_future(self.stream.pushed.wait()),
                 asyncio.ensure_future(self.wakeup.wait())}
        try:
            await asyncio.wait(waits, timeout=self.rest_s(), return_when=asyncio.FIRST_COMPLETED)
        finally:
            for w in waits:
                w.cancel()
            await asyncio.gather(*waits, return_exceptions=True)
            self.wakeup.clear()

    def wake_in(self, seconds: float) -> None:
        """The loop woken in ``seconds`` (an order's first read, FILL_READ_MS after its answer: not up to a tick later)."""
        with contextlib.suppress(RuntimeError):
            asyncio.get_running_loop().call_later(max(0.0, seconds), self.wakeup.set)

    def on_change(self, mid: str) -> bool:
        """A followed window's book just changed (a push, a REST read that differed, matching seen open with an offer in hand, the
        index of a window in its course): that window is looked at now, in this very turn of the loop — its order signed and its
        request handed to its thread before anything else gets a turn. True when the change was taken (the loop need not wake)."""
        if not self.running or self.stop.is_set() or self.settings.mode == "off":
            return False
        slug = self.by_market.get(mid)
        w = self.track.get(slug) if slug is not None else None
        if w is None:
            return False
        if w.get("busy"):
            w["again"] = True  # looked at again the moment the look under way ends
            return True
        self.first_ticks += 1
        task = start_now(self.look_now(slug, w), "rules-look")
        if not task.done():
            self.side_tasks.add(task)
            task.add_done_callback(self.side_tasks.discard)
        return mid not in self.stream.changed

    async def look_now(self, slug: str, w: dict) -> None:
        """A window's look, one at a time a window (another change while it runs: looked at again right after)."""
        if w.get("busy"):
            w["again"] = True
            return
        w["busy"] = True
        try:
            for _ in range(5):
                w["again"] = False
                try:
                    await self.look(slug, w)
                except asyncio.CancelledError:
                    raise
                except Exception as error:
                    w["word"] = f"读取失败：{clean_error(error) or type(error).__name__}"
                    self.note(f"{slug}：{w['word']}")
                if not w.get("again") or self.stop.is_set() or self.track.get(slug) is not w:
                    break
        finally:
            w["busy"] = False

    def index_wanted(self) -> bool:
        """Whether any rule buys in its windows' course by the settings in force (a RULE_*_LIVE_CENTS set, its market on, the
        mode not off): the index feed keeps its connection only then."""
        return self.settings.mode != "off" and any(self.live_cents(r) > 0 for r in RULES)

    def index_needed(self) -> bool:
        """Whether a window in (or about to begin) its course needs the index now: the REST read's pace (IndexFeed.rest_loop)."""
        if self.settings.mode == "off":
            return False
        now = self.now()
        return any(self.live_cents(RULE_OF[w["rule"]]) > 0 and w["start"] - WARM_MS <= now < self.end_of(w) for w in self.track.values())

    def on_index(self, symbol: str, at_ms: int) -> None:
        """The index of a card just moved (or the start line of its window was read): the window in its course is looked at as
        the index comes — not at the next second's look."""
        if not self.running or self.settings.mode == "off":
            return
        now = self.now()
        hit = []
        for slug, w in self.track.items():
            rule = RULE_OF[w["rule"]]
            if (w["market"] is None or rule.spec.symbol != symbol or self.phase(w, now) != "live" or now < w["look_ms"]
                    or self.finished(slug, rule, True)):
                continue
            if at_ms and now - at_ms > INDEX_FRESH_MS:
                self.index_late += 1
                continue
            hit.append(w)
        if not hit or (self.settings.mode == "on" and self.block(0.0)) or self.cooling() > 0:
            return
        if not self.fast:
            self.pace(now)
        for w in hit:
            w["indexed"] = int(time.time() * 1000)
        self.index_looks += 1
        self.stream.nudge(*(str(w["market"]["id"]) for w in hit))

    def gc_guard(self, now: int) -> None:
        """No automatic garbage collection through the race's seconds (quiet_ms) — a collection of the oldest generation stops
        the whole process while it walks every object; refrozen now and then outside them."""
        want = self.running and (self.quiet_ms(now) > 0 or self.quiet_ms(now + GC_BEFORE_MS) > 0)
        if want and not self.gc_off:
            self.gc_off = True
            gc.disable()
        elif not want and self.gc_off:
            self.gc_off = False
            gc.enable()
        elif not want and now - self.frozen_ms >= 3_600_000:
            self.frozen_ms = now
            with contextlib.suppress(Exception):
                gc.collect()
                gc.freeze()

    def quiet_ms(self, now: int) -> int:
        """The ms left of the race's seconds: around the makers' moment of a window this taker can still buy before its start,
        the first seconds of a window's course for a rule that buys there, while an offer in hand waits for matching to open."""
        left = 0
        if self.settings.mode != "on":
            return 0
        for slug, w in self.track.items():
            rule = RULE_OF[w["rule"]]
            at, begins = self.moment(w), w["start"] + LIVE_START_MS
            if at and at - QUIET_BEFORE_MS <= now < at + QUIET_AFTER_MS and now < w["start"] - STOP_MS and self.settings.price(rule.key) > 0:
                until, course = at + QUIET_AFTER_MS, False
            elif begins - QUIET_BEFORE_MS <= now < begins + COURSE_QUIET_MS and self.live_cents(rule) > 0 and self.settings.price(rule.key) > 0:
                until, course = begins + COURSE_QUIET_MS, True
            elif self.rushing(w):
                until, course = now + RUSH_QUIET_MS, False
            else:
                continue
            if not self.finished(slug, rule, course):
                left = max(left, until - now)
        return left

    async def turn(self) -> None:
        """One tick's work: the plan, the pace, the looks that are due, a REST read beside the pushes, the orders signed
        beforehand, the follow-ups and the results."""
        self.ticks += 1
        self.hold_loss_stop()
        now = self.now()
        if now - self.planned_ms >= PLAN_EVERY_MS:
            self.plan(now)
            self.planned_ms = now
        self.pace(now)
        self.gc_guard(now)
        self.stream.changed &= set(self.by_market)
        if self.settings.mode == "off" or self.cooling() > 0:
            self.stream.changed.clear()
            for w in self.track.values():
                w.pop("indexed", None)
        if self.settings.mode != "off":
            changed, self.looked = set(self.stream.changed), self.stream.arrivals
            for slug, w in sorted(list(self.track.items()), key=lambda kv: (
                    kv[1]["market"] is None or str(kv[1]["market"]["id"]) not in changed, kv[1]["start"])):
                if self.stop.is_set() or self.cooling() > 0:
                    break
                if w.get("busy") or self.track.get(slug) is not w:
                    continue
                await self.look_now(slug, w)
            if self.fast:
                self.verify_tick()
            self.keep_warm(self.now())
            self.course_reads(self.now())
            self.presign(self.now())
        self.stream.want(set(self.by_market))
        await self.follow()
        await self.settle()

    def pace(self, now: int) -> None:
        """The per-second hours (RULE_FAST_HOURS), a window at the makers' moment, a window in its course: looked at each second
        and prepared (warm) from WARM_MS before."""
        on = self.settings.mode != "off"
        wait = self.settings.fast_in(now) if on else None
        sprint = on and any(self.sprinting(w, now) for w in self.track.values())
        course = on and any(self.phase(w, now) == "live" for w in self.track.values())
        fast = wait == 0 or sprint or course
        if fast != self.fast:
            self.fast = fast
            LOG.info("规则吃单：每秒检查%s", "开始" if fast else "结束")
        near = on and any(now >= w["start"] - WARM_MS and self.live_cents(RULE_OF[w["rule"]]) > 0 for w in self.track.values())
        soon = on and any(w["market"] is not None and 0 <= w["start"] - now <= NEAR_MS and self.settings.price(RULE_OF[w["rule"]].key) > 0
                          for w in self.track.values())
        first = on and not self.warmed and any(w["market"] is not None for w in self.track.values())
        if (((wait is not None and wait <= WARM_MS) or sprint or near or soon or first)
                and now - self.warm_ms >= WARM_EVERY_MS and (self.warming is None or self.warming.done())):
            self.warm_ms, self.warmed = now, True
            self.warming = asyncio.create_task(self.warm(), name="rules-warm")

    def busy_in(self, now: int) -> int | None:
        """How long until the taker needs speed, ms: 0 — it does now; None — not for any window it follows."""
        if self.settings.mode == "off":
            return None
        soonest = self.settings.fast_in(now)
        if soonest == 0 or self.fast:
            return 0
        for w in self.track.values():
            spans = [(w["start"] - NEAR_MS, self.end_of(w) - LIVE_STOP_MS if self.live_cents(RULE_OF[w["rule"]]) > 0 else w["start"] - STOP_MS)]
            if at := self.moment(w):
                spans.append((at - WARM_MS, at + SPRINT_MS))
            for begin, end in spans:
                if now >= end:
                    continue
                if now >= begin:
                    return 0
                soonest = begin - now if soonest is None else min(soonest, begin - now)
        return soonest

    def moment(self, w: dict) -> int:
        lead = int(getattr(self.settings, "lead_minutes", 0) or 0) * 60_000
        return w["start"] - lead if lead > 0 else 0

    def rushing(self, w: dict) -> bool:
        task = w.get("rush_task")
        return task is not None and not task.done()

    def rush_pace(self) -> float:
        per_min = float(getattr(self.api.pacer, "per_min", 0) or 0) if hasattr(self.api, "pacer") else 0.0
        pace = max(STATUS_RUSH_S, 66.0 / per_min) if per_min > 0 else STATUS_RUSH_S
        return pace * max(1, sum(1 for w in self.track.values() if self.rushing(w)))

    def rush(self, slug: str, w: dict) -> None:
        """An offer at the rule's price in hand on a market whose matching has not been seen open: its status asked every
        rush_pace() seconds for STATUS_RUSH_MS, ahead of the key's other reads; the moment it says OPEN the window is looked at."""
        if self.settings.mode != "on" or self.rushing(w) or int(w.get("rush_reads") or 0) >= RUSH_READS_MAX:
            return
        w["rush_wall"] = int(time.time() * 1000)
        task = w["rush_task"] = asyncio.ensure_future(self.rush_run(slug, w))
        self.side_tasks.add(task)
        task.add_done_callback(self.side_tasks.discard)

    async def rush_run(self, slug: str, w: dict) -> None:
        began = time.monotonic()
        while (not w["open_ms"] and w.get("offer", True) and self.track.get(slug) is w and w["market"] is not None and not self.stop.is_set()
               and self.settings.mode == "on" and time.monotonic() - began < STATUS_RUSH_MS / 1000 and int(w.get("rush_reads") or 0) < RUSH_READS_MAX):
            asked = time.monotonic()
            if self.cooling() <= 0:
                mid = str(w["market"]["id"])
                self.reads += 1
                self.status_reads += 1
                w["rush_reads"] = int(w.get("rush_reads") or 0) + 1
                try:
                    market = await self.api.market(mid, priority=True)
                except asyncio.CancelledError:
                    raise
                except Exception as error:
                    w["rush_error"] = clean_error(error) or type(error).__name__
                else:
                    if self.track.get(slug) is not w or w["market"] is None:
                        return
                    w["market"] = {**market, "categorySlug": slug}
                    if str(market.get("tradingStatus") or "").upper() == "OPEN":
                        if not w["open_ms"]:
                            w["open_ms"], w["open_wall"] = self.now(), int(time.time() * 1000)
                            self.watch(w)["open_ms"] = self.watch(w)["open_ms"] or w["open_ms"]
                        self.stream.nudge(mid)  # looked at now: the offer in hand is bought
                        return
            await asyncio.sleep(max(0.0, self.rush_pace() - (time.monotonic() - asked)))

    def sprinting(self, w: dict, now: int) -> bool:
        at = self.moment(w)
        return bool(at) and -WARM_MS <= now - at < SPRINT_MS

    def hot(self, w: dict, now: int) -> bool:
        at = self.moment(w)
        return bool(at) and -HOT_BEFORE_MS <= now - at < HOT_AFTER_MS

    async def warm(self) -> None:
        """Ready before an offer shows: every window not yet found looked for at the next tick, a market not yet seen open for
        matching read once more, and — real orders — the sign-in, the exchange allowances and the balance in hand."""
        try:
            for w in self.track.values():
                if w["market"] is None and w["probe_ms"] <= self.now():
                    w["probe_ms"] = 0
            if self.cooling() > 0:
                return
            for slug, w in sorted(list(self.track.items()), key=lambda kv: kv[1]["start"]):
                if w["market"] is None or w["open_ms"] or self.stop.is_set() or slug not in self.track:
                    continue
                now = self.now()
                self.reads += 1
                w["market"], w["status_ms"] = {**await self.api.market(str(w["market"]["id"])), "categorySlug": slug}, now
                if str(w["market"].get("tradingStatus") or "").upper() == "OPEN":
                    w["open_ms"], w["open_wall"] = now, int(time.time() * 1000)
                    self.watch(w)["open_ms"] = self.watch(w)["open_ms"] or now
            if self.settings.mode != "on" or self.block(0.0):
                return
            await self.api.ensure_auth()
            exchanges = set()
            for w in list(self.track.values()):
                if w["market"] is not None:
                    with contextlib.suppress(Exception):
                        info = parse_market(w["market"], self.config.fee_bps)
                        exchanges.add(exchange_key(info.neg_risk, info.yield_bearing))
            for exchange in sorted(exchanges):
                if self.chain is not None:
                    self.allowance[exchange] = (self.now(), int(await self.chain.allowance(exchange)))
            if self.now() - self.balance_ms >= BALANCE_BUSY_MS:
                await self.refresh_balance()
        except asyncio.CancelledError:
            raise
        except Exception as error:
            self.note(f"每秒检查的准备没做完：{clean_error(error) or type(error).__name__}")

    def finished(self, slug: str, rule: Rule, course: bool = False) -> str:
        """Why nothing more is bought in this window, "" while something may be."""
        rec = self.record(slug)
        if rec is None:
            return ""
        size = self.settings.shares_of(rule.market)
        if held(rec) >= size - DUST:
            return f"这一场已买满 {size:g} 份" if not in_flight(rec) else f"这一场的 {size:g} 份已全部下单，等结果（撤回的部分 {LATE_FILL_MS // 1000} 秒后确认没有晚到的成交才算空出来）"
        if self.settings.mode == "paper" and any(o.get("paper") and bool(o.get("live")) == course for o in rec["orders"]):
            return "只记录：这一场" + ("盘中" if course else "") + "已记过（只记一次：没真买，盘口上的单还在，再记就重复了）"
        if sum(1 for o in rec["orders"] if o.get("state") == "rejected" or (o.get("state") == "not_found" and o.get("conflict"))) >= MAX_REJECTS:
            return f"Predict 拒绝了 {MAX_REJECTS} 次，这一场不再下单"
        return ""

    def verify_tick(self) -> None:
        """The REST read beside the pushes (in the per-second stretches): one window's book at a time, the longest unread
        first, three a second at most and a window not again within a second — a change the push did not bring is seen within
        seconds, the push down the looks go on on these reads, and the connection to Predict stays warm. At the makers'
        moment the reads are that window's; a market not yet seen open for matching is asked its status instead."""
        if self.verifying is not None and not self.verifying.done():
            return
        mono = time.monotonic()
        every = self.verify_every()
        if mono - self.verify_at < every or self.cooling() > 0 or (self.settings.mode == "on" and self.block(0.0)):
            return
        now = self.now()
        open_to = [(slug, w) for slug, w in self.track.items()
                   if w["market"] is not None and (phase := self.phase(w, now)) and now >= w["look_ms"]
                   and (self.live_cents(RULE_OF[w["rule"]]) if phase == "live" else self.settings.price(w["rule"])) > 0
                   and not self.finished(slug, RULE_OF[w["rule"]], phase == "live")
                   and not (phase == "live" and self.live_guard(slug, RULE_OF[w["rule"]], now))]
        hot = [(slug, w) for slug, w in open_to if self.hot(w, now)]
        often = every if hot else max(VERIFY_AGAIN_S, every)
        due = [(w.get("rest_at", 0.0), slug) for slug, w in (hot or open_to) if mono - w.get("rest_at", 0.0) >= often]
        if not due:
            return
        slug = min(due)[1]
        w = self.track[slug]
        w["rest_at"] = self.verify_at = mono
        if hot and not w["open_ms"] and self.stream.alive() and self.stream.held(str(w["market"]["id"])) is not None:
            self.verifying = asyncio.create_task(self.verify_status(slug, w), name="rules-verify-status")
        else:
            self.verifying = asyncio.create_task(self.verify(str(w["market"]["id"])), name="rules-verify")

    def keep_warm(self, now: int) -> None:
        """Through every busy stretch (real orders: the last NEAR_MS before a start, the makers' moment, a course, the
        per-second hours) a pooled connection to Predict is kept used within WARM_CONN_S: an order may reuse a connection
        only that fresh (http.KEEPALIVE_POST_IDLE_SECONDS), and on a cold one it waits for a TCP + TLS handshake. While the
        pushes bring every book nothing else reads — so a book of the soonest window is read (merged as verify merges it)."""
        if self.settings.mode != "on" or (self.verifying is not None and not self.verifying.done()) or self.cooling() > 0:
            return
        host = getattr(self.api, "host", "")
        fresh = nethttp.pool_fresh(host) if host else 0.0
        if fresh is not None and fresh < WARM_CONN_S:
            return
        if self.busy_in(now) != 0 or self.block(0.0):
            return
        soon = sorted((w["start"], slug) for slug, w in self.track.items() if w["market"] is not None and self.phase(w, now))
        if not soon:
            return
        w = self.track[soon[0][1]]
        w["rest_at"] = self.verify_at = time.monotonic()
        self.verifying = asyncio.create_task(self.verify(str(w["market"]["id"])), name="rules-keep-warm")

    async def verify_status(self, slug: str, w: dict) -> None:
        try:
            self.reads += 1
            self.status_reads += 1
            now = self.now()
            market = await self.api.market(str(w["market"]["id"]))
            if slug not in self.track or self.track[slug] is not w or w["market"] is None:
                return
            w["market"], w["status_ms"] = {**market, "categorySlug": slug}, max(now, int(w.get("status_ms") or 0))
            if str(market.get("tradingStatus") or "").upper() == "OPEN" and not w["open_ms"]:
                w["open_ms"], w["open_wall"] = self.now(), int(time.time() * 1000)
                self.watch(w)["open_ms"] = self.watch(w)["open_ms"] or w["open_ms"]
                if w.get("offer"):
                    self.stream.nudge(str(w["market"]["id"]))
        except asyncio.CancelledError:
            raise
        except Exception as error:
            self.note(f"撮合状态没读到：{clean_error(error) or type(error).__name__}")

    async def verify(self, mid: str) -> None:
        asked = time.monotonic()
        try:
            self.reads += 1
            self.verifies += 1
            bids, asks = await self.book(mid)
        except asyncio.CancelledError:
            raise
        except Exception as error:
            self.verify_error = f"{bj(self.now(), False)} {clean_error(error) or type(error).__name__}"
            return
        self.verify_error = ""
        self.verify_ms.append(int((time.monotonic() - asked) * 1000))
        if self.stream.merge(mid, bids, asks, asked):
            self.verify_diffs += 1

    def verify_every(self) -> float:
        """How often the REST read beside the pushes may run (s): RULE_VERIFY_MS, never more than 80% of the key's budget
        (PREDICT_RATE_PER_MIN; the default 340 ms is three a second, within the standard key's 220 a minute)."""
        budget = 60 / (0.8 * self.config.rate_per_min) if self.config.rate_per_min > 0 else 0.0
        return max(self.config.verify_ms / 1000, budget)

    def plan(self, now: int) -> None:
        """The windows of the enabled rules from now to a day on; a window of a rule switched off is dropped, and so is one this
        close to its start (or, for a rule that buys in its windows' course too, to its end). Old finished records removed."""
        # the holiday calendar running out: told once a day for its last CALENDAR_WARN_DAYS (then no real order goes out)
        left = self.calendar_left(now)
        if left is not None and left <= CALENDAR_WARN_DAYS and self.cal_told != day_of(now) and self.settings.mode == "on":
            self.cal_told = day_of(now)
            self.alarm(f"⛔ 规则吃单：{self.calendar_why(now)}" if left == 0 else
                       f"⚠️ 规则吃单：CME 休市日历只排到 {self.cal_last.isoformat()}（还剩 {left} 天）。到期后不再下单：在设置文件补上之后的"
                       " CME_CLOSED_DAYS、CME_EARLY_DAYS（bash deploy/gcp/rt.sh edit）")
        want: dict[str, tuple[str, int]] = {}
        if self.settings.mode != "off":
            for rule in RULES:
                course = self.live_cents(rule) > 0
                if live_rule(self.settings, rule) or course:
                    back = rule.spec.window_ms - 1 if course else -STOP_MS
                    for start in windows(rule, now - back, now + HORIZON_MS):
                        if not self.over({"rule": rule.key, "start": start}, now):
                            want[rule.spec.slug_of(start)] = (rule.key, start)
        for slug in list(self.track):
            if slug not in want:
                w = self.track.pop(slug)
                if w.get("market") is not None:
                    self.by_market.pop(str(w["market"]["id"]), None)
                if w["start"] - now <= STOP_MS and not w.get("kept"):
                    self.keep_watch(slug, w)
        for slug, (key, start) in want.items():
            fresh = slug not in self.track
            w = self.track.setdefault(slug, {"rule": key, "start": start, "market": None, "listed_ms": 0, "open_ms": 0, "probe_ms": 0,
                                             "look_ms": 0, "status_ms": 0, "offer": False, "word": ""})
            kept = (self.record(slug) or {}).get("seen") if fresh else None
            if isinstance(kept, dict) and kept.get("open"):
                w["seen"] = {k: kept.get(k, v) for k, v in WATCH.items()}
        self.recent_keys = {k for k in self.recent_keys if int((self.recs.get(k) or {}).get("start_ms") or 0) >= now - RECENT_MS}
        old = [key for key, r in self.recs.items()
               if int(r.get("start_ms") or 0) < now - KEEP_DAYS * DAY_MS and not in_flight(r) and (r.get("pnl") is not None or filled(r) <= DUST)]
        if old:
            self.store.delete_keys([PREFIX + key for key in old])
            for key in old:
                self.recs.pop(key, None)

    def live_cents(self, rule: Rule) -> int:
        """The rule's highest price in its window's course (RULE_*_LIVE_CENTS), cents; 0 = it buys before the window alone."""
        return live_price_of(self.settings, rule)

    def end_of(self, w: dict) -> int:
        return RULE_OF[w["rule"]].spec.end_of(w["start"])

    def phase(self, w: dict, now: int) -> str:
        """"before" — ahead of the start by more than STOP_MS; "live" — in its course (from LIVE_START_MS after the start to
        LIVE_STOP_MS before the end), for a rule that buys there; "" — neither."""
        if w["start"] - now > STOP_MS:
            return "before"
        if self.live_cents(RULE_OF[w["rule"]]) > 0 and now - w["start"] >= LIVE_START_MS and self.end_of(w) - now > LIVE_STOP_MS:
            return "live"
        return ""

    def over(self, w: dict, now: int) -> bool:
        if w["start"] - now > STOP_MS:
            return False
        return not (self.live_cents(RULE_OF[w["rule"]]) > 0 and self.end_of(w) - now > LIVE_STOP_MS)

    def live_guard(self, slug: str, rule: Rule, now: int) -> str:
        """Why the rule's side is not bought in the window's course right now ("" while it may be): the index must stand ahead
        of the window's start on the rule's side by Rule.lead — ahead of both start lines when Predict shows a start price
        that agrees with the index candle (within 0.1%), of whichever is the less kind to the side."""
        feed = self.index
        if feed is None:
            return "没有币安指数行情（盘中吃单要用）"
        w = self.track.get(slug)
        if w is None:
            return "这一场不在跟踪"
        symbol = rule.spec.symbol
        candle = feed.close_of(symbol, w["start"])
        strike = w.get("strike")
        line_used = candle
        if candle is not None and strike is not None and strike > 0 and abs(strike - candle) <= candle * D("0.001"):
            line_used = strike
        lines = [x for x in {candle, line_used} if x is not None and x > 0]
        price = feed.price(symbol)
        if not lines:
            return "还没读到这一场的起点价"
        if price is None or not price > 0:
            return "还没有实时指数"
        if why := feed.price_problem(symbol, self.end_of(w), now):
            return why
        up = rule.side == "up"
        line = max(lines) if up else min(lines)
        if ((price - line) if up else (line - price)) / line >= rule.lead:
            return ""
        return f"{rule.word}还没领先（指数 {fmt(price)}，起点 {fmt(line)}，要{'高' if up else '低'}出 {float(rule.lead) * 100:g}% 以上），不买"

    def course_reads(self, now: int) -> None:
        """盘中: the start line of a window in (or entering) its course read once its minute is over, and Predict's own start
        price asked for every STRIKE_EVERY_MS through the course's first minutes — beside the looks, never in their way."""
        if self.index is None:
            return
        for slug, w in self.track.items():
            rule = RULE_OF[w["rule"]]
            if self.live_cents(rule) <= 0 or w["market"] is None or not w["start"] <= now < self.end_of(w):
                continue
            if self.index.close_of(rule.spec.symbol, w["start"]) is None and not self.busy_task(w, "candle_task"):
                w["candle_task"] = self.spawn(self.index.ensure_start(rule.spec.symbol, w["start"], now), "rules-candle")
            if (w.get("strike") is None and now - w["start"] < STRIKE_FOR_MS and now - int(w.get("strike_ms") or 0) >= STRIKE_EVERY_MS
                    and not self.busy_task(w, "strike_task") and self.cooling() <= 0):
                w["strike_ms"] = now
                w["strike_task"] = self.spawn(self.read_strike(slug, w), "rules-strike")

    async def read_allowance(self, exchange: str) -> tuple[int, int]:
        """The account's USDT allowance for an exchange, read on-chain: an unlimited one is not spent by an order (read again
        hourly, beside the orders), a limited one every ALLOWANCE_MS (on the order's way: it shrinks as orders fill)."""
        got = (self.now(), int(await self.chain.allowance(exchange)))
        self.allowance[exchange] = got
        return got

    async def read_allowance_quietly(self, exchange: str) -> None:
        try:
            await self.read_allowance(exchange)
        except asyncio.CancelledError:
            raise
        except Exception as error:
            self.note(f"交易所授权额度没读到（沿用上一次的）：{clean_error(error) or type(error).__name__}")

    def busy_task(self, w: dict, name: str) -> bool:
        task = w.get(name)
        return task is not None and not task.done()

    def spawn(self, coro: Any, name: str) -> asyncio.Task:
        task = asyncio.ensure_future(coro)
        self.side_tasks.add(task)
        task.add_done_callback(self.side_tasks.discard)
        return task

    async def read_strike(self, slug: str, w: dict) -> None:
        with contextlib.suppress(Exception):
            self.reads += 1
            market = await self.api.market(str(w["market"]["id"]))
            if self.track.get(slug) is w:
                w["strike"] = market_start_price(market)
                if w["strike"] is not None and self.index is not None:
                    self.on_index(RULE_OF[w["rule"]].spec.symbol, 0)

    def due(self, w: dict, now: int) -> bool:
        start, near = w["start"], w["start"] - now <= NEAR_MS
        if w["market"] is None:
            expected = start - LISTED_AHEAD_MS
            if now < expected - PROBE_EARLY_MS:
                return False
            every = PROBE_FAST_S if now <= expected + PROBE_LATE_MS or near else PROBE_SLOW_S
            return now - w["probe_ms"] >= every * 1000
        mid = str(w["market"]["id"])
        if now < w["look_ms"]:
            self.stream.changed.discard(mid)
            w.pop("indexed", None)
            return False
        if mid in self.stream.changed:
            return True
        every = (FAST_LOOK_S if self.fast else FAST_S if near else OFFER_STATUS_S if (w["offer"] and not w["open_ms"])
                 else SLOW_S if w["open_ms"] else CLOSED_SLOW_S)
        return now - w["look_ms"] >= every * 1000

    def clamp(self, take: D, limit: D, now: int) -> D:
        """``take`` within the USDT at hand and the day's cap left (RULE_MAX_DAY_USD), whole shares — as look sizes an order
        (place counts them again as it records it; presign signs the order a sweep would make)."""
        usd_left = self.usd()
        if usd_left is not None and limit > 0:
            take = min(take, (D(str(max(usd_left, 0.0))) / limit).to_integral_value(rounding="ROUND_FLOOR"))
        if self.settings.max_day_usd and limit > 0:
            left = self.settings.max_day_usd - spent_on(self.recent(), day_of(now), self.settings.mode)
            take = min(take, D(str(max(left, 0.0))) / limit)
        return max(D(0), take.to_integral_value(rounding="ROUND_FLOOR"))

    async def look(self, slug: str, w: dict) -> None:
        """One look at a window: its market found as Predict lists it; its book (pushed, or read); the offers at the rule's
        price; matching's status read when there is something to buy and it has not opened; then the take."""
        now = self.now()
        rule = RULE_OF[w["rule"]]
        phase = self.phase(w, now)
        course = phase == "live"
        if phase != "before" and not w.get("kept"):
            w["kept"] = True
            self.keep_watch(slug, w)
        if not phase:
            if self.over(w, now):
                self.track.pop(slug, None)
                if w.get("market") is not None:
                    self.by_market.pop(str(w["market"]["id"]), None)
            else:
                w["word"] = "开场前后这几秒不下单（等这一场的起点价）"
            return
        if not self.due(w, now):
            return
        w["look_wall"], w["fresh"] = int(time.time() * 1000), False
        indexed = int(w.pop("indexed", 0) or 0)
        w["index_wall"] = indexed if course and 0 <= w["look_wall"] - indexed < 2_000 else 0
        if w["market"] is not None:
            w["fresh"] = str(w["market"]["id"]) in self.stream.changed and not indexed
            self.stream.changed.discard(str(w["market"]["id"]))
            if not course and self.observe(w, rule, now):
                self.save_watch(slug, w, rule)
        settings = self.settings
        rec = self.record(slug)
        cents = self.live_cents(rule) if course else self.settings.price(rule.key)
        early = course and now - w["start"] < LIVE_FULL_MS
        if early:
            cents = min(cents, self.settings.price(rule.key))
        if done := self.finished(slug, rule, course):
            w["word"] = done
            return
        if self.settings.mode == "on" and (why := self.block(0.0)):
            w["word"] = why
            return
        if w["market"] is None:
            w["probe_ms"] = now
            self.reads += 1
            market = await category_market(self.api, slug)
            if market is None:
                w["word"] = "还没发布"
                return
            question = str(market.get("question") or market.get("title") or "")
            if problem := rule.spec.title_problem(question, w["start"]):
                w["word"] = f"市场标题和场次对不上：{problem}"
                w["probe_ms"] = now + 10 * 60_000
                return
            if not book_up_first(market):
                w["word"] = "这个市场把“跌”列在第一个结果，盘口方向未确认，不做"
                w["probe_ms"] = now + 10 * 60_000
                return
            if self.track.get(slug) is not w:
                return
            w["market"], w["listed_ms"], w["status_ms"] = market, now, now
            self.index_market(slug, w)
            if str(market.get("tradingStatus") or "").upper() == "OPEN":
                w["open_ms"] = now
            self.watch(w)["listed_ms"] = self.watch(w)["listed_ms"] or now
            self.stream.want(set(self.by_market))  # subscribed now, not at the tick's end
        w["look_ms"] = now
        if early and self.settings.mode == "paper":
            w["word"] = "盘中：只记录模式从开场 1 分钟后按盘中的价格记一次（第 1 分钟的价格和盘前一样）"
            return
        if cents <= 0:
            w["word"] = ("盘中：开场后第 1 分钟最高按盘前的价格买（盘前价格是 0，不买），1 分钟后按盘中的价格" if early
                         else "盘前不买这条（盘前价格是 0），开场 1 分钟后按盘中的价格买")
            return
        if course and (why := self.live_guard(slug, rule, now)):
            w["word"] = f"盘中：{why}"
            return
        mid = str(w["market"]["id"])
        pushed = self.stream.book(mid)
        asked = time.monotonic()
        if self.fast:
            self.fast_looks += 1
            if pushed is None:
                got = self.stream.held(mid)
                if got is not None and (self.stream.alive() or asked - got[2] <= FAST_STALE_S):
                    pushed = got[:2]
                elif asked - w.get("rest_at", -1e9) < FAST_STALE_S:
                    w["word"] = "每秒检查：等盘口（推送还没送到，REST 轮流读取中）"
                    return
                else:
                    w["rest_at"] = asked
        if pushed is None:
            self.reads += 1
        bids, asks = pushed if pushed is not None else await self.book(mid)
        if pushed is None:
            got = self.stream.held(mid)
            if got is not None and got[2] > asked:
                bids, asks = got[0], got[1]  # a book was pushed while this read was out: the push is the newer
                pushed = (bids, asks)
            else:
                self.stream.merge(mid, bids, asks, asked)
            self.stream.changed.discard(mid)
        w["book_at"] = self.stream.stamps.get(mid, (0, 0)) if pushed is not None else (0, int(time.time() * 1000))
        limit = D(cents) / 100
        levels = offered(bids, asks, rule.side, limit) if limit > 0 else []
        if course and self.settings.mode == "paper" and rec and levels:
            levels = without(levels, D(str(sum(float(o.get("filled") or 0) for o in rec["orders"] if o.get("paper") and not o.get("live")))))
        w["offer"] = bool(levels)
        if not levels:
            w.pop("rushed", None)
        best = offered(bids, asks, rule.side, D("0.99"))[:1]
        if not course and self.note_offers(w, levels, best, now):
            self.save_watch(slug, w, rule)
        if not levels:
            w["word"] = ("盘中：" if course else "") + f"盘口没有 ≤{int(limit * 100)}¢ 的{rule.word}" + (f"（最低 {float(best[0][0]) * 100:g}¢）" if best else "（没人卖）")
            return
        if not w["open_ms"]:
            near = w["start"] - now <= NEAR_MS
            if not w.get("rushed"):
                w["rushed"] = True
                self.rush(slug, w)
            if now - w["status_ms"] >= (FAST_S if near or self.fast else OFFER_STATUS_S) * 1000:
                w["status_ms"] = now
                self.reads += 1
                w["market"] = {**await self.api.market(mid), "categorySlug": slug}
                if str(w["market"].get("tradingStatus") or "").upper() == "OPEN" and not w["open_ms"]:
                    w["open_ms"], w["open_wall"] = now, int(time.time() * 1000)
                    self.watch(w)["open_ms"] = self.watch(w)["open_ms"] or now
            if not w["open_ms"]:
                w["word"] = (f"盘口已有 ≤{int(limit * 100)}¢ 的{rule.word} {sum(q for _, q in levels):g} 份，等撮合开启就买"
                             f"（现在 {w['market'].get('tradingStatus') or '?'}" + (f"，每 {self.rush_pace():g} 秒读一次状态" if self.rushing(w) else "") + "）")
                return
        if self.phase(w, self.now()) != ("live" if course else "before"):
            # (the book's read took that long: past the line no real order goes out, so no paper one is recorded either)
            w["word"] = "开场前 15 秒（盘中：结束前 15 秒）已到，这一轮不下单也不记录"
            return
        size = self.settings.shares_of(rule.market)
        room = D(str(size)) - D(str(held(rec) if rec else 0.0))
        there = sum(q for _, q in levels)
        take = min(there, room)
        if self.settings.mode == "on":
            # the window bought at once: the offers in hand are first those of the window's orders already on their way; with
            # enough beyond them the order is for all the window may still buy (it takes what stands at the rule's price when it
            # arrives — a ladder still being posted has more by then than the book in hand showed); within the USDT at hand
            out = open_orders(rec) if rec else []
            if len(out) >= MAX_OPEN_ORDERS:
                w["word"] = f"这一场已有 {len(out)} 张单在途，等它们的结果"
                return
            fresh = there - D(str(claims(rec) if rec else 0.0))
            if fresh < 1:
                w["word"] = f"盘口 ≤{int(limit * 100)}¢ 的{rule.word} {there:g} 份，在途的单正在吃"
                return
            take = room if fresh >= SWEEP_SHARES and (fresh >= room / SWEEP_PART or self.sprinting(w, now)) else min(fresh, room)
            usd_left = self.usd()
            if usd_left is not None:
                afford = (D(str(max(usd_left, 0.0))) / limit).to_integral_value(rounding="ROUND_FLOOR")
                if afford < 1 or float(afford * limit) < 1.0:
                    w["word"] = f"USDT 余额 ${usd_left:,.2f} 不够一张最小订单"
                    return
                take = min(take, afford)
        if self.settings.max_day_usd:
            left = self.settings.max_day_usd - spent_on(self.recent(), day_of(now), self.settings.mode)
            take = min(take, D(str(max(left, 0.0))) / limit)
            if left < 1.0:
                w["word"] = f"今天已花 ${self.settings.max_day_usd - left:,.2f}，到了每天上限 ${self.settings.max_day_usd:g}（RULE_MAX_DAY_USD）"
                return
        take = take.to_integral_value(rounding="ROUND_FLOOR")
        if take < 1 or float(take * limit) < 1.0:
            w["word"] = f"盘口 ≤{int(limit * 100)}¢ 的{rule.word}只有 {there:g} 份，不够一张最小订单（或额度、余额剩得不够）"
            return
        if self.settings != settings:
            w["word"] = "设置刚改过：这一轮不下单，下一轮按新设置重新看"
            w["look_ms"] = 0
            return
        if self.settings.mode == "paper":
            self.paper(slug, w, rule, levels, take, limit, rec, course)
        else:
            await self.place(slug, w, rule, levels, take, limit, rec, course)

    def observe(self, w: dict, rule: Rule, now: int) -> bool:
        seen = w.get("seen")
        if not seen or not seen.get("offer_ms") or seen.get("gone_ms"):
            return False
        mid = str(w["market"]["id"])
        got, stamp = self.stream.held(mid), self.stream.stamps.get(mid)
        limit = D(self.settings.price(rule.key)) / 100
        if got is None or stamp is None or limit <= 0:
            return False
        return self.clock_offers(w, float(sum(q for _, q in offered(got[0], got[1], rule.side, limit))), now, int(stamp[0]))

    @staticmethod
    def watch(w: dict) -> dict:
        return w.setdefault("seen", dict(WATCH))

    def note_offers(self, w: dict, levels: list, best: list, now: int) -> bool:
        seen = self.watch(w)
        if best:
            seen["best"] = min(float(best[0][0]), seen["best"] if seen["best"] is not None else 1.0)
        noted = self.clock_offers(w, float(sum(q for _, q in levels)), now, int((w.get("book_at") or (0, 0))[0]))
        if w["open_ms"] and not seen["open_ms"]:
            seen["open_ms"] = w["open_ms"]
        return noted

    def clock_offers(self, w: dict, total: float, now: int, stamp: int) -> bool:
        seen, noted = self.watch(w), False
        if total > seen["offer_max"]:
            seen["offer_max"], noted = total, True
        if total > 0 and not seen["offer_ms"]:
            seen["offer_ms"], seen["offer_ts"], noted = now, stamp, True
        elif seen["offer_ms"] and not seen.get("cut_ms") and total < float(w.get("there") or 0.0) - DUST:
            seen["cut_ms"], seen["cut_ts"], noted = now, stamp, True
        if total <= 0 and seen["offer_ms"] and not seen.get("gone_ms"):
            seen["gone_ms"], seen["gone_ts"], noted = now, stamp, True
        w["there"] = total
        return noted

    def save_watch(self, slug: str, w: dict, rule: Rule) -> None:
        seen = w.get("seen")
        if not seen or w["market"] is None or self.settings.mode == "off" or self.settings.price(rule.key) <= 0:
            return
        rec = self.record(slug) or self.new_record(slug, w, rule, 0)
        rec["seen"] = {**seen, "start_ms": w["start"], "cents": self.settings.price(rule.key), "open": True}
        self.save(rec)

    def keep_watch(self, slug: str, w: dict) -> None:
        seen = w.get("seen")
        rule = RULE_OF[w["rule"]]
        if not seen or not seen.get("listed_ms") or w["market"] is None or self.settings.mode == "off" or self.settings.price(rule.key) <= 0:
            return
        rec = self.record(slug) or self.new_record(slug, w, rule, 0)
        rec["seen"] = {**seen, "start_ms": w["start"], "cents": self.settings.price(rule.key), "bought": round(filled(rec), 4)}
        self.save(rec)

    async def book(self, market_id: str) -> tuple[tuple, tuple]:
        return await self.api.orderbook(market_id)

    def new_record(self, slug: str, w: dict, rule: Rule, index_set: int, mode: str | None = None) -> dict:
        card, start = rule.spec, w["start"]
        return {"slug": slug, "rule": rule.key, "card": rule.card, "side": rule.side, "mode": mode or self.settings.mode,
                "name": card.name_of(start), "start_ms": start, "end_ms": card.end_of(start), "market_id": str(w["market"]["id"]),
                "index_set": index_set, "cents": self.settings.price(rule.key), "orders": [], "created_ms": self.now(), "result": "",
                "pnl": None}

    def paper(self, slug: str, w: dict, rule: Rule, levels: list, take: D, limit: D, rec: dict | None, course: bool = False) -> None:
        """只记录: what the order would have bought, at the book's prices — no order."""
        info = parse_market(w["market"], self.config.fee_bps)
        outcome, _ = outcome_for_side(rule.side, info.outcomes)
        if rec is not None and rec.get("mode") != "paper":
            rec = self.record(slug, "paper")
        rec = rec or self.new_record(slug, w, rule, int((outcome or {}).get("index_set") or 0), mode="paper")
        rec["index_set"] = int(rec.get("index_set") or 0) or int((outcome or {}).get("index_set") or 0)
        avg = walk(levels, take)
        rec["orders"].append({"paper": True, "state": "final", "shares": float(take), "filled": float(take), "limit": float(limit),
                              "avg": float(avg), "fee_bps": info.fee_bps, "at": self.now(), **({"live": True} if course else {}),
                              "book": [[float(p), float(q)] for p, q in levels[:5]]})
        self.save(rec)
        w["word"] = ("盘中 " if course else "") + f"只记录：{rule.word} {float(avg) * 100:.1f}¢ × {take:g} 份"
        self.notify(f"🧪 规则吃单{'·盘中' if course else ''}（只记录，没下单）：{rec['name']} 买{rule.word} 均价 {float(avg) * 100:.1f}¢ × {take:g} 份"
                    f"（{rule.name}{'盘中' if course else ''} ≤{int(limit * 100)}¢）")

    # -- the order --
    def order_for(self, slug: str, w: dict, rule: Rule, rec: dict | None, take: D, limit: D, course: bool) -> tuple | None:
        """The order a look would send now, unsigned: (signed fields, amounts, info, outcome, expires); None when the market's
        tokens cannot be told apart."""
        info = parse_market(w["market"], self.config.fee_bps)
        outcome, assumed = outcome_for_side(rule.side, info.outcomes)
        if outcome is None or assumed:
            return None
        expires = expiry_for(self.end_of(w), LIVE_STOP_MS, self.now()) if course else expiry_for(w["start"], STOP_MS, self.now())
        amounts = limit_amounts(True, to_wei(limit), to_wei(take))
        count = len((rec or {}).get("orders") or []) if (rec or {}).get("mode") == "on" else 0
        identity = f"rule-v1|{self.config.chain_id}|{self.wallet.maker.lower()}|{info.market_id}|{outcome['token']}|{w['start']}|{count}"
        salt = int.from_bytes(hashlib.sha256(identity.encode()).digest()[:8], "big") % MAX_SALT
        fields = build_order(self.wallet, outcome["token"], amounts["maker"], amounts["taker"], info.fee_bps, expires, salt=salt)
        return fields, amounts, info, outcome, expires

    def sign(self, fields: dict, neg_risk: bool, yield_bearing: bool) -> tuple[str, str, bool]:
        """(hash, signature, signed beforehand) — the same fields always make the same order and the same signature, so an order
        signed ahead of the offer (presign) is the one the look would sign itself."""
        key = (neg_risk, yield_bearing, *(str(fields[k]) for k in ("salt", "tokenId", "makerAmount", "takerAmount", "feeRateBps", "expiration", "maker")))
        hit = self.signed.get(key)
        if hit is not None:
            return hit[0], hit[1], True
        digest, signature = self.wallet.sign_order(fields, neg_risk, yield_bearing)
        self.signed[key] = (digest, signature)
        while len(self.signed) > SIGN_CACHE:
            self.signed.popitem(last=False)
        return digest, signature, False

    def presign(self, now: int) -> None:
        """The order a window would send the moment the makers' ladder shows — all the window may still buy, at the rule's
        price — signed ahead (real orders; the windows at the makers' moment, in the per-second hours or near their start)."""
        if self.settings.mode != "on" or self.wallet is None or not self.fast or self.block(0.0):
            return
        for slug, w in self.track.items():
            if w["market"] is None or self.phase(w, now) != "before" or w.get("busy"):
                continue
            rule = RULE_OF[w["rule"]]
            limit = D(self.settings.price(rule.key)) / 100
            if limit <= 0 or self.finished(slug, rule):
                continue
            rec = self.record(slug, "on")
            if len(open_orders(rec) if rec else []) >= MAX_OPEN_ORDERS:
                continue
            room = D(str(self.settings.shares_of(rule.market))) - D(str(held(rec) if rec else 0.0))
            take = self.clamp(room, limit, now)
            if take < 1 or float(take * limit) < 1.0:
                continue
            with contextlib.suppress(Exception):
                made = self.order_for(slug, w, rule, rec, take, limit, False)
                if made is not None:
                    info = made[2]
                    key_before = len(self.signed)
                    self.sign(made[0], info.neg_risk, info.yield_bearing)
                    if len(self.signed) > key_before:
                        self.presigned += 1

    async def place(self, slug: str, w: dict, rule: Rule, levels: list, take: D, limit: D, rec: dict | None, course: bool = False) -> None:
        usd = float(take * limit)
        if why := self.block(usd):
            w["word"] = why
            return
        settings = self.settings
        info = parse_market(w["market"], self.config.fee_bps)
        outcome, assumed = outcome_for_side(rule.side, info.outcomes)
        if outcome is None or assumed:
            w["word"] = f"认不出{rule.word}的代币：{assumed or '缺字段'}"
            return
        exchange = exchange_key(info.neg_risk, info.yield_bearing)
        cached = self.allowance.get(exchange)
        if self.chain is not None and cached is not None and cached[1] >= ALLOWANCE_UNLIMITED:
            if self.now() - cached[0] > ALLOWANCE_LONG_MS and not self.busy_task(self.__dict__, "allowance_task"):
                self.allowance_task = self.spawn(self.read_allowance_quietly(exchange), "rules-allowance")  # (not on this order's way)
        elif self.chain is not None and (cached is None or self.now() - cached[0] > ALLOWANCE_MS):
            cached = await self.read_allowance(exchange)
        if cached is not None and cached[1] < to_wei(usd):
            w["word"] = "交易所授权额度不足（在 Predict 网站用这个账户交易一次，或运行 huice 的 python main.py --approve）"
            return
        await self.api.ensure_auth()
        if self.settings != settings:
            w["word"] = "设置刚改过：这张单不下了，下一轮按新设置重新看"
            return
        # -- from here to the request handed to its thread nothing is waited for: the day's cap, the balance and the window's
        # shares are counted again on the records as they stand now (another window's order may have gone out meanwhile)
        now = self.now()
        if self.phase(w, now) != ("live" if course else "before") or self.track.get(slug) is not w:
            w["word"] = "开场前 15 秒（盘中：结束前 15 秒）已到，这张单不下了"  # (the look's reads took it past the line)
            return
        rec = (self.record(slug, "on") or (rec if rec is not None and rec.get("mode") == "on" else None)
               or self.new_record(slug, w, rule, int(outcome.get("index_set") or 0), mode="on"))
        rec["index_set"] = int(rec.get("index_set") or 0) or int(outcome.get("index_set") or 0)
        room = D(str(self.settings.shares_of(rule.market))) - D(str(held(rec)))
        take = self.clamp(min(take, room), limit, now)
        if take < 1 or float(take * limit) < 1.0:
            return
        if why := self.block(float(take * limit)):
            w["word"] = why
            return
        made = self.order_for(slug, w, rule, rec, take, limit, course)
        if made is None:
            w["word"] = f"认不出{rule.word}的代币"
            return
        fields, amounts, info, outcome, expires = made
        # the record's shares are the ones signed: Predict keeps 5 significant digits of a quantity (a whole number up to the
        # settings' 100 000 keeps every one; a longer one is cut, and its fills are read against what was cut)
        take = D(int(amounts["taker"])) / D(10 ** 18)
        digest, signature, ahead = self.sign(fields, info.neg_risk, info.yield_bearing)
        if ahead:
            self.presign_hits += 1
        book_ts, seen_wall = w.get("book_at") or (0, 0)
        order = {"hash": digest.lower(), "id": "", "state": "placing", "shares": float(take), "limit": float(limit), "filled": 0.0,
                 "avg": None, "fee_bps": info.fee_bps, "at": now, "expires": expires,
                 "book": [[float(p), float(q)] for p, q in levels[:5]],
                 "book_ts": int(book_ts), "seen_wall": int(seen_wall),
                 "fresh": bool(w.get("fresh")), "look_wall": int(w.get("look_wall") or 0), "signed_wall": int(time.time() * 1000),
                 **({"presigned": True} if ahead else {}),
                 **({"open_wait": int(w["open_wall"]) - int(w["rush_wall"]), "open_reads": int(w.get("rush_reads") or 0)}
                    if w.get("open_wall") and w.get("rush_wall") and not w.get("open_told") else {}),
                 "seen": float(sum(q for _, q in levels)),
                 **({"live": True, **({"index_wall": int(w["index_wall"])} if w.get("index_wall") else {})} if course else {})}
        w["open_told"] = True
        rec["orders"].append(order)
        self.sending[order["hash"]] = slug  # (on its way from here: follow() leaves it to send(), which ends it)
        self.save(rec)
        self.decided[order["hash"]] = settings
        body = dumps_bytes({"data": {"order": {**fields, "signature": signature, "hash": digest}, "strategy": "LIMIT",
                                     "pricePerShare": str(amounts["price_per_share"])}})  # (the request's bytes made here, once)
        # the request handed to its thread within this very call (eager start); the look does not wait for the answer — another
        # change of this window's book is looked at at once, the offers beyond this order's claim ordered beside it
        task = start_now(self.send(slug, w, rule, rec, order, body, take, limit), "rules-send")
        if task.done():
            self.sent_done(task)
        else:
            self.sends.add(task)
            task.add_done_callback(self.sent_done)

    def sent_done(self, task: asyncio.Task) -> None:
        self.sends.discard(task)
        if not task.cancelled() and (error := task.exception()) is not None:
            self.note(f"下单过程出错：{clean_error(error) or type(error).__name__}")

    async def send(self, slug: str, w: dict, rule: Rule, rec: dict, order: dict, body: bytes, take: D, limit: D) -> None:
        """One prepared order out: its hash on the disk first (a crash in between is found by it), then the request; Predict's
        answer onto the record. The order is "on its way" (self.sending — follow() leaves it alone) from the moment place()
        recorded it until this is wholly done, its rest withdrawn when real trading stopped meanwhile."""
        try:
            await self.send_inner(slug, w, rule, rec, order, body, take, limit)
        finally:
            self.sending.pop(order["hash"], None)
            self.decided.pop(order["hash"], None)

    async def send_inner(self, slug: str, w: dict, rule: Rule, rec: dict, order: dict, body: bytes, take: D, limit: D) -> None:
        if not self.journal(rec, order) and not (self.store.writer is None or await asyncio.to_thread(self.store.flush, self.flush_wait_s)):
            order.update(state="not_sent", error="数据库写入未完成，没有发单")
            self.save(rec)
            w["word"] = f"没下成：{order['error']}"
            self.note(f"{rec['name']} 没下成：{order['error']}")
            return
        self.sending[order["hash"]] = slug
        order["sent_wall"] = int(time.time() * 1000)
        try:
            result = await self.api.create_order(body, order["hash"])
            order.update(id=str(result.get("order_id") or ""), state="open", next_ms=self.now() + FILL_READ_MS)
            # its way out, in parts: the wait for the key's budget, a slot and a thread, a new connection (if one was
            # needed), and the moment its bytes were all written
            order.update({k: result[k] for k in ("queued_ms", "connect_ms", "wrote_wall") if k in result})
        except NotSent as error:
            order.update(state="not_sent", error=clean_error(error))
            order.pop("sent_wall", None)
        except Exception as error:
            text = clean_error(error) or type(error).__name__
            status = http_status(error)
            placed = placed_or_not(error)
            if isinstance(error, RuntimeError) and "shutdown" in str(error):
                order.update(state="not_sent", error="进程正在停止，请求没有发出")
            elif placed == "not_sent":
                order.update(state="not_sent", error=text)
            elif placed == "rejected":
                order.update(state="rejected", error=text)
            else:  # a timeout, a lost answer, a bare 409: it may exist — looked for by its hash, never sent again
                order.update(state="unknown", error=text, next_ms=self.now() + (FILL_READ_MS if status == 409 else UNKNOWN_EVERY_MS),
                             **({"conflict": True} if status == 409 else {}))
        finally:
            self.decided.pop(order["hash"], None)
            order["answered_wall"] = int(time.time() * 1000)
        if order["state"] == "open" and order.get("id") and self.halted():
            # real trading stopped while the request was out (a pause, the mode, the loss stop, the service stopping): its
            # rest withdrawn now, by the id this answer brought — not at its first read (a stop would leave it resting)
            await self.withdraw_one(order)
        self.save(rec)
        if order["state"] in {"open", "unknown"}:
            self.wake_in(FILL_READ_MS / 1000 + 0.01)
        w["word"] = ("盘中 " if order.get("live") else "") + {
            "open": f"已下单：{rule.word} ≤{int(limit * 100)}¢ × {take:g} 份", "unknown": f"下单结果未知（{order.get('error')}），按哈希核对"}.get(
            order["state"], f"没下成：{order.get('error') or order['state']}")
        if order["state"] in NEVER_PLACED:
            self.note(f"{rec['name']} 没下成：{order.get('error') or order['state']}")
            held_back = order["state"] == "not_sent" and str(order.get("error") or "").startswith(RECHECKED)
            w["held_back"] = int(w.get("held_back") or 0) + 1 if held_back else 0
            if order["state"] == "not_sent" and str(order.get("error") or "").startswith(nethttp.NOT_CONNECTED):
                w["look_ms"] = self.now() + CONNECT_RETRY_MS  # (the network: tried again in a moment, never hammered)
            elif not held_back or w["held_back"] >= HELD_BACK_MAX:
                w["held_back"] = 0
                w["look_ms"] = self.now() + REJECT_PAUSE_MS
            else:
                w["look_ms"] = 0
        else:
            w.pop("held_back", None)
            LOG.info("规则吃单下单：%s %s ≤%d¢ × %s 份 → %s（往返 %d ms）", rec["name"], rule.word, int(limit * 100), take, order["state"],
                     int(order["answered_wall"]) - int(order.get("sent_wall") or order["answered_wall"]))

    # -- the journal --
    def journal_path(self) -> str:
        path = str(getattr(self.store, "path", "") or "")
        return "" if not path or path == ":memory:" else path + JOURNAL_SUFFIX

    def journal(self, rec: dict, order: dict) -> bool:
        """A real order's record in a line of the journal file, before its request leaves: what a restart needs to look for it by
        its hash should the process end before the store has it. False without such a file (the caller flushes the store)."""
        if self.journal_fd is None:
            return False
        line = (b"\n" if self.journal_cut else b"") + dumps_bytes(
            {"key": rid(rec["slug"], rec["mode"]), "rec": {k: v for k, v in rec.items() if k != "orders"}, "order": order}) + b"\n"
        try:
            wrote = os.write(self.journal_fd, line)
        except OSError as error:
            self.note(f"订单日志写入失败（{type(error).__name__}），改为等数据库写完再发单")
            return False
        if wrote != len(line):
            self.journal_cut = True
            with contextlib.suppress(OSError):
                if os.write(self.journal_fd, b"\n") == 1:
                    self.journal_cut = False
            self.note(f"订单日志只写进 {wrote}/{len(line)} 字节（磁盘满了？），改为等数据库写完再发单")
            return False
        self.journal_cut = False
        journal_sync(self.journal_fd, self)  # (on the disk itself a moment after the request leaves, a power cut or not)
        return True

    def recover(self) -> None:
        """At the start: the journal's orders the records do not hold onto their records as orders whose answer is unknown
        (looked for by their hash, never sent again); then, the store written, the journal begun anew."""
        path = self.journal_path()
        if not path:
            return
        import json
        try:
            if os.path.exists(path):
                with open(path, encoding="utf-8", errors="replace") as lines:
                    for line in lines:
                        try:
                            entry = json.loads(line)
                            key, order = entry.get("key"), entry.get("order") or {}
                            kept = self.recs.get(key)
                            rec = kept or {**(entry.get("rec") or {}), "orders": []}
                            old = kept is None and int(rec.get("start_ms") or 0) < self.now() - KEEP_DAYS * DAY_MS
                            if not key or not order.get("hash") or not rec.get("slug") or any(o.get("hash") == order["hash"] for o in rec["orders"]):
                                continue
                        except (ValueError, TypeError, AttributeError, KeyError):
                            continue
                        if old:
                            continue
                        rec["orders"].append({**order, "state": "unknown", "error": "重启前发出的单，记录里还没有：按哈希核对", "next_ms": 0})
                        self.recs[key] = rec
                        self.store.put(PREFIX + key, rec)
                        LOG.warning("规则吃单：订单日志里有一张记录里没有的单（%s），按哈希核对", order["hash"][:12])
            written = self.store.flush()
            self.journal_fd = os.open(path, os.O_WRONLY | os.O_APPEND | os.O_CREAT | (os.O_TRUNC if written else 0), 0o600)
            # the file and its folder's entry on the disk now (once, at the start), its lines after each order
            with contextlib.suppress(OSError):
                os.fsync(self.journal_fd)
                folder = os.open(os.path.dirname(os.path.abspath(path)), os.O_RDONLY)
                try:
                    os.fsync(folder)
                finally:
                    os.close(folder)
        except OSError as error:
            LOG.warning("规则吃单：订单日志打不开（%s），发单前改为等数据库写完", type(error).__name__)

    # -- the orders --
    async def follow(self) -> None:
        now = self.now()
        for slug, rec in list(self.recs.items()):
            if rec.get("mode") != "on" or not in_flight(rec):
                continue
            for order in rec["orders"]:
                if (order.get("paper") or order.get("state") in FINAL_STATES or now < int(order.get("next_ms") or 0)
                        or self.cooling() > 0 or order.get("hash") in self.sending):
                    continue
                try:
                    await self.follow_order(rec, order, now)
                except Exception as error:
                    order["error"] = clean_error(error) or type(error).__name__
                    order["next_ms"] = now + UNKNOWN_EVERY_MS
                self.save(rec)

    async def follow_order(self, rec: dict, order: dict, now: int) -> None:
        state = order["state"]
        if state == "placing":  # a request that left with the process gone before its answer: looked for by its hash
            order.update(state="unknown", next_ms=now)
            state = "unknown"
        late = now >= int(order.get("expires") or 0) * 1000 + GIVE_UP_MS
        self.reads += 1
        row = await self.api.order(order.get("id") or "", order["hash"])
        if row is None:
            if late:
                if state == "unknown" and not order.get("id"):
                    order.update(state="not_found", error="按哈希一直查不到：视为没下成")
                else:
                    order.update(state="final", error=f"到期后 {GIVE_UP_MS // 60_000} 分钟仍查不到这张单：按已知成交 {float(order.get('filled') or 0):g} 份记")
            elif state == "unknown" and not order.get("id") and order.get("conflict"):
                order["misses"] = int(order.get("misses") or 0) + 1
                if order["misses"] >= CONFLICT_MISSES and now - int(order.get("at") or now) >= CONFLICT_GRACE_MS:
                    order.update(state="not_found", error=f"Predict 答复 409，之后按哈希查了 {order['misses']} 次都没有这张单：视为被拒、没下成")
                    self.note(f"{rec.get('name') or rec.get('slug')} 那张答复 409 的单按哈希查不到，视为没下成（这一场可以再买）")
                else:
                    order["next_ms"] = now + UNKNOWN_EVERY_MS
            else:
                order["next_ms"] = now + (UNKNOWN_EVERY_MS if state == "unknown" else FOLLOW_EVERY_MS)
            return
        order["id"] = str(row.get("id") or order.get("id") or "")
        self.take_fills(rec, order, row)
        if float(order.get("filled") or 0) >= float(order["shares"]) - DUST:
            order["state"] = "final"
            return
        if order_final(row, float(order["shares"])) and not order.get("removed") and not order.get("closed_as"):
            # closed on Predict's side, never by us (a rest is withdrawn only here, at a follow): its word kept — an order
            # meeting the account's own resting one is cancelled as the taker (self-trade prevention), nothing bought
            order["closed_as"] = str(row.get("status") or "") or "?"
        if order_final(row, float(order["shares"])) or order.get("removed"):
            order["left_ms"] = int(order.get("left_ms") or now)
            if now - order["left_ms"] >= LATE_FILL_MS or late:
                order["state"] = "final"
            else:
                order.update(state="closing", next_ms=order["left_ms"] + LATE_FILL_MS)
            return
        if late:
            order.update(state="final", error="到期后仍显示挂着：按已知成交记")
            return
        if order.get("id"):  # still open: the rest withdrawn (a taker never waits on the book)
            result = await self.api.remove_orders([order["id"]])
            if order["id"] in (result.get("removed") or []) or order["id"] in (result.get("noop") or []):
                order.update(removed=now, left_ms=now)
            order.update(state="cancelling", next_ms=now + FOLLOW_EVERY_MS)
        else:
            order["next_ms"] = now + UNKNOWN_EVERY_MS

    async def withdraw_one(self, order: dict) -> None:
        now = self.now()
        try:
            result = await self.api.remove_orders([order["id"]])
        except asyncio.CancelledError:
            raise
        except Exception as error:
            order.update(error=clean_error(error) or type(error).__name__, next_ms=0)
            return
        # (noop: nothing left to withdraw — filled or expired already; either way off the book, as follow_order takes it)
        gone = {str(i) for i in (result.get("removed") or [])} | {str(i) for i in (result.get("noop") or [])}
        if str(order["id"]) in gone:
            order.update(removed=now, left_ms=now, state="cancelling", next_ms=now + FOLLOW_EVERY_MS)
        else:
            order["next_ms"] = 0

    async def withdraw_all(self, why: str) -> str:
        """Every real order of this taker still on the book asked to be withdrawn now (the pause, a stop); one whose answer went
        missing looked for by its hash first. The line for the reply; "" when there is no real order out."""
        out = [(rec, o) for rec in self.recs.values() if rec.get("mode") == "on"
               for o in rec.get("orders") or [] if not o.get("paper") and o.get("state") not in FINAL_STATES]
        if not out:
            return ""
        lost = [(rec, o) for rec, o in out if not o.get("id") and o.get("state") == "unknown" and o.get("hash")]
        if lost:
            self.reads += len(lost)
            rows = await asyncio.gather(*(self.api.order("", o["hash"]) for _, o in lost), return_exceptions=True)
            for (rec, o), row in zip(lost, rows):
                if isinstance(row, asyncio.CancelledError):
                    raise row
                if isinstance(row, dict) and row.get("id") not in (None, "") and not o.get("id"):
                    o["id"] = str(row["id"])
        resting = [(rec, o) for rec, o in out if o.get("id") and not o.get("removed") and o.get("state") in {"open", "cancelling", "unknown"}]
        done: set[str] = set()
        error = ""
        if resting:
            try:
                result = await self.api.remove_orders([str(o["id"]) for _, o in resting])
                done = {str(i) for i in (result.get("removed") or [])} | {str(i) for i in (result.get("noop") or [])}
            except asyncio.CancelledError:
                raise
            except Exception as failed:
                error = clean_error(failed) or type(failed).__name__
        now, off = self.now(), 0
        for rec, o in resting:
            if o.get("state") in FINAL_STATES or o.get("removed") or o.get("state") == "closing":
                off += 1
                continue
            if str(o["id"]) in done:
                o.update(removed=now, left_ms=int(o.get("left_ms") or now), state="cancelling", next_ms=now + FOLLOW_EVERY_MS)
                off += 1
            else:
                o["next_ms"] = 0
            self.save(rec)
        unknown = [(rec, o) for rec, o in out if not o.get("id") and o.get("state") == "unknown"]
        flying = [(rec, o) for rec, o in out if not o.get("id") and o.get("state") == "placing"]
        for rec, o in unknown:
            o["next_ms"] = 0
            self.save(rec)
        parts = []
        if resting:
            missed = len(resting) - off
            parts.append(f"{len(resting)} 张挂着的单已请求撤回，Predict 确认 {off} 张"
                         + (f"，{missed} 张没撤成（{error or 'Predict 的应答里没有它'}），每秒再撤" if missed else ""))
        if unknown:
            parts.append(f"{len(unknown)} 张下单结果未知，按哈希现在查不到：接着核对，一查到就撤")
        if flying:
            parts.append(f"{len(flying)} 张还在发送途中，应答一到就撤")
        if not parts:
            return ""
        LOG.warning("规则吃单：%s——%s", why, "；".join(parts))
        return "规则吃单：" + "；".join(parts) + "。"

    def take_fills(self, rec: dict, order: dict, row: dict) -> None:
        shares, _ = order_fill(row, float(order["shares"]))
        price = order_price(row)
        if price is not None:
            order.update(avg=price, avg_stated=True)
        before = float(order.get("filled") or 0)
        if shares > before + DUST:
            order["filled"] = shares
            order["filled_ms"] = self.now()  # (the balance read before this cannot have seen it)
            self.balance_ms = 0  # (the balance read again at once: the fill spent it)
            avg = float(order.get("avg") or order["limit"])
            rule = RULE_OF.get(rec.get("rule"))
            self.notify(f"🎯 规则吃单{'·盘中' if order.get('live') else ''}成交：{rec['name']} 买{rule.word if rule else rec.get('side')} {avg * 100:.1f}¢ × {shares - before:g} 份"
                        f"（累计 {shares:g}/{order['shares']:g}；{rule.name if rule else ''}{'盘中' if order.get('live') else ''} ≤{round(float(order.get('limit') or 0) * 100)}¢）")

    # -- the results --
    async def settle(self) -> None:
        now = self.now()
        for slug, rec in list(self.recs.items()):
            if rec.get("pnl") is not None or in_flight(rec) or filled(rec) <= DUST:
                continue
            end = int(rec.get("end_ms") or 0)
            if now < end + SETTLE_AFTER_MS or now - int(rec.get("settle_checked") or 0) < settle_every(now - end):
                continue
            if self.cooling() > 0:
                return
            rec["settle_checked"] = now
            try:
                self.reads += 1
                market = await self.api.market(str(rec["market_id"]))
                resolution = predict_resolution(market)
            except Exception as error:
                rec["settle_error"] = clean_error(error) or type(error).__name__
                self.save(rec)
                continue
            if not resolution:
                self.save(rec)
                continue
            pay = paid(rec, resolution, won_index_set(market, resolution))
            if pay is None:
                first = not rec.get("settle_alarmed")
                rec["settle_error"] = f"结果名称认不出：{resolution.get('name')}"
                rec["settle_alarmed"] = True
                self.save(rec)
                if first:
                    self.alarm(f"⚠️ 规则吃单：{rec.get('name') or slug} 已结算，但认不出结果（{resolution.get('name')}），"
                               "这一场的盈亏算不出、不计入今天的亏损停机线：请到 Predict 网站核对")
                continue
            up = pay if resolution.get("split") or rec["side"] == "up" else 1 - pay
            pnl = net(rec) * pay - cost(rec)
            rec.update(result="split" if up == 0.5 else "up" if up == 1.0 else "down", pnl=round(pnl, 6), settled_ms=now, settled_day=day_of(now))
            self.pnl_epoch += 1
            self.save(rec)
            word = "赢" if pnl > DUST else "输" if pnl < -DUST else "平"
            rule = RULE_OF.get(rec.get("rule"))
            self.notify(f"{'✅' if pnl > DUST else '❌' if pnl < -DUST else '➖'} 规则吃单结算{'（只记录）' if rec.get('mode') == 'paper' else ''}："
                        f"{rec['name']} 买{rule.word if rule else rec['side']} {filled(rec):g} 份，结果{'涨' if up == 1.0 else '跌' if up == 0.0 else '平'}，"
                        f"{word} {money(pnl)}" + ("（赢的份额要在 Predict 领取：同一账户上 Railway 机器人开着自动领取会领，否则到网站上领）"
                                                   if rec.get("mode") == "on" and pay > 0 else ""))
            floor = self.loss_floor()
            if rec.get("mode") == "on" and floor is not None and day_pnl(self.recs, day_of(now)) <= floor:
                self.hold_loss_stop()
                self.alarm(f"⛔ 规则吃单今日已结算 {money(day_pnl(self.recs, day_of(now)))}，到了 RULE_MAX_DAILY_LOSS_USD 的停机线：今天不再开新单（明天自动恢复；/resume 从现在的盈亏起再给一个额度）")

    # -- the operator --
    async def control(self, what: str, arg: str) -> str:
        if what == "check":
            return await self.self_check()
        if what == "test":
            return await self.order_test()
        if what == "pause":
            self.paused = arg or "手动暂停"
            self.store.put("control:paused", self.paused)
            line = await self.withdraw_all("暂停")
            late = await self.on_disk("暂停", "恢复成没暂停（接着开新单）")
            return f"⏸ 规则吃单已暂停：不再开新单（{self.paused}）" + "".join(f"\n{x}" for x in (line, late) if x)
        if what == "resume":
            parts = []
            if self.paused:
                self.paused = ""
                self.store.put("control:paused", "")
                parts.append("暂停已解除")
            floor = self.loss_floor()
            pnl = day_pnl(self.recs, day_of(self.now()))
            held = self.loss_stop.get("day") == day_of(self.now())
            if held:  # (the day's stop, held since it was reached: lifted — another cap from the P&L now)
                self.loss_stop = {}
                self.store.put("control:loss_stop", {})
            if floor is not None and (pnl <= floor or held):
                self.loss_floor_at = {"day": day_of(self.now()), "floor": pnl - self.loss_cap}
                self.store.put("control:loss_floor", self.loss_floor_at)
                parts.append(f"今日停机线移到 {money(self.loss_floor_at['floor'])}（从现在的 {money(pnl)} 起再亏 ${self.loss_cap:g} 再停）")
            reply = "▶️ " + ("；".join(parts) if parts else "本来就没有暂停") + f"｜现在：{self.mode_text()}"
            late = await self.on_disk("恢复", "仍是暂停")
            return reply + (f"\n{late}" if late else "")
        if what == "mode":
            want = arg.strip().lower()
            if want not in MODE_RANK:
                return "用法：/mode off | paper | on（不会高于部署变量 RULE_TAKER）"
            if MODE_RANK[want] > MODE_RANK[self.env_mode]:
                return f"部署变量 RULE_TAKER={self.env_mode}，这里最多调到 {self.env_mode}（要真实吃单先改部署变量再重启）"
            before = self.settings.mode
            self.configure(self.settings.with_mode(want))
            self.store.put("control:mode", want if want != self.env_mode else "")
            line = await self.withdraw_all("改成" + MODES[want]) if want != "on" else ""
            late = await self.on_disk("模式", f"回到“{MODES.get(before, before)}”")
            return f"规则吃单改成：{MODES[want]}" + "".join(f"\n{x}" for x in (line, late) if x)
        return "？"

    async def set_command(self, args: str) -> str:
        """/set KEY VALUE: one setting changed, as the control page changes it (checked whole, in force at once, kept on disk)."""
        key, _, value = args.strip().partition(" ")
        if not key or not value.strip():
            return "用法：/set 设置名 数值（例：/set RULE_MAX_DAILY_LOSS_USD 30）；设置名在 /settings 里点一项就能看到"
        try:
            return "✅ " + await self.save_settings({key.upper(): value.strip()})
        except NotOnDisk as late:
            return f"⚠️ {late}"
        except ValueError as error:
            return f"❌ 没改：{error}"

    async def reset_command(self, args: str) -> str:
        """/reset KEY: one setting back to the settings file's value."""
        key = args.strip().upper()
        if key not in console.FIELDS:
            return "用法：/reset 设置名（回到设置文件里的值；设置名在 /settings 里点一项就能看到）"
        try:
            return "✅ " + await self.reset_settings([key])
        except NotOnDisk as late:
            return f"⚠️ {late}"
        except ValueError as error:
            return f"❌ 没改：{error}"

    async def after_the_race(self) -> None:
        """Waits out the race's seconds (RACE_WAIT_S at the most): what is worked out after it — the status text (milliseconds
        over a long record), the CSV — holds the loop the orders go out on, never while an offer may be there to take."""
        began = time.monotonic()
        while (left := self.quiet_ms(self.now())) > 0 and time.monotonic() - began < RACE_WAIT_S:
            await asyncio.sleep(min(left / 1000, 0.25))

    async def command(self, command: str, args: str) -> str:
        """The admin's Telegram commands."""
        if command in {"/status", "/rules", "/start"}:
            await self.after_the_race()
            return self.status()
        if command == "/pause":
            return await self.control("pause", args or "Telegram /pause")
        if command == "/resume":
            return await self.control("resume", "")
        if command == "/mode":
            return await self.control("mode", args)
        if command in {"/check", "/selfcheck"}:
            return await self.self_check()
        if command == "/set":
            return await self.set_command(args)
        if command == "/reset":
            return await self.reset_command(args)
        if command == "/review":
            arg = args.strip().lower()
            days = 30 if not arg else 0 if arg in {"all", "全部"} else max(0, min(3650, int(arg))) if arg.isdigit() else 30
            return review_text(self.review_data(days), days)
        if command == "/settings":
            return settings_text(self.settings_view())
        if command == "/test":
            return await self.order_test()
        if command == "/speed":
            return await self.speed_test(args)
        return ("命令：/menu 按钮菜单｜/status 状态｜/check 自检｜/test 挂单测试｜/speed 测速｜/pause 暂停｜/resume 恢复｜/mode off|paper|on｜"
                "/settings 设置｜/set 设置名 数值｜/review 复盘｜/id")

    # -- the self-check and the order test (Telegram /check /test, the control page's buttons, rt.sh check / test) --
    def exchanges_in_use(self) -> set[str]:
        """The exchanges (EXCHANGES keys) the markets followed now settle on."""
        out = set()
        for w in self.track.values():
            if w.get("market") is not None:
                info = parse_market(w["market"], self.config.fee_bps)
                out.add(exchange_key(info.neg_risk, info.yield_bearing))
        return out

    async def self_check(self) -> str:
        """Everything a real order needs, looked at now and said in one message (huice's /live check, and more): the mode and its
        stops, the sign-in, the Predict account's owner on-chain, the balance, the exchange allowances, whether Predict's order
        endpoint takes orders from this region (an empty order: refused as invalid, never placed), a read's round trip, the push
        connection, the index (in-play rules), the records' store, the windows found, the last problems — and a conclusion."""
        now, s = self.now(), self.settings
        lines = [f"🔎 规则吃单自检 {bj(now)}（{self.region or '未知区域'}，版本 {VERSION}）"]
        bad: list[str] = []

        def ok(text: str) -> None:
            lines.append("✅ " + text)

        def no(text: str) -> None:
            lines.append("❌ " + text)
            bad.append(text)

        def warn(text: str) -> None:
            lines.append("⚠️ " + text)
        floor = self.loss_floor()
        lines.append(f"模式：{MODES.get(s.mode, s.mode)}（RULE_TAKER={self.env_mode}）｜LIVE_TRADING_ALLOWED "
                     f"{'允许' if self.config.trading_allowed else '不是 on（不会真实下单）'}｜{('已暂停：' + self.paused) if self.paused else '没暂停'}"
                     f"｜今天已结算 {money(self.today_pnl())}" + (f"（停机线 {money(floor)}）" if floor is not None else "（不设日亏损停机）"))
        live = s.mode == "on"
        if live and not self.config.trading_allowed:
            no("LIVE_TRADING_ALLOWED 不是 on：真实吃单模式也不会下单")
        if live and floor is not None and self.loss_stop_why():
            no(f"今天已到过日亏损停机线 {money(floor)}：今天不再开新单（/resume 从现在的盈亏起再给一个额度）")
        left = self.calendar_left(now)
        if left == 0:
            (no if live else warn)(self.calendar_why(now))
        elif left is not None and left <= CALENDAR_WARN_DAYS:
            warn(f"CME 休市日历只排到 {self.cal_last.isoformat()}（还剩 {left} 天）：到期前在设置文件补上之后的 CME_CLOSED_DAYS、"
                 "CME_EARLY_DAYS，到期后不下单")
        elif left is not None:
            ok(f"CME 休市日历排到 {self.cal_last.isoformat()}")
        else:
            warn("CME 休市日历是空的（CME_CLOSED_DAYS、CME_EARLY_DAYS 都没写）：节假日也会照常下单")
        # where the key is kept: encrypted on this machine (deploy/gcp/vm-key.sh), or plain in the settings file
        kept, store = self.config.key_kept, self.config.key_store
        if self.wallet is None:
            (no if live else warn)("没有设置 PREDICT_PRIVATE_KEY：只能 off / paper" + ("（设定私钥：bash deploy/gcp/rt.sh key）" if store else ""))
        else:
            try:
                await self.api.ensure_auth()
                exp = int(getattr(self.api, "jwt_exp", 0) or 0)
                self.ready_error, self.ready_ms = "", now
                ok("Predict 登录：成功" + (f"（token 到 {bj(exp * 1000)}）" if exp else ""))
            except Exception as error:
                self.ready_error = clean_error(error) or type(error).__name__
                no(f"Predict 登录失败：{self.ready_error}")
            if self.wallet.predict_account:
                await self.prepare()  # (the owner's check, as at a start)
                if self.account_error:
                    no(f"账户：{self.account_error}")
                elif self.account_checked:
                    ok(f"账户：Predict 账户 {self.wallet.maker[:6]}…{self.wallet.maker[-4:]}，私钥是它的控制钥匙（链上核对）")
                else:
                    no("账户：链上核对还没完成")
            else:
                ok(f"账户：普通钱包 {self.wallet.maker[:6]}…{self.wallet.maker[-4:]}")
            if kept == "stored":
                ok("私钥：加密保存在这台机器上，只在服务自己的内存里解开（设置文件里没有明文）")
            elif kept == "both":
                warn("私钥：已加密保存，但设置文件里还留着同一把的明文：运行 bash deploy/gcp/rt.sh update（或 edit 存一次）就会收走")
            elif store:
                warn("私钥：在设置文件里是明文：运行 bash deploy/gcp/rt.sh update 改成加密保存（只有这台机器能解开）")
            # (not run by systemd: a laptop, a test — nothing to keep it in)
            await self.refresh_balance()
            if self.usdt is None:
                no(f"余额没读到：{self.balance_error or '链上节点没回答'}")
            else:
                need = max((s.shares_of(r.market) * s.price(r.key) / 100 for r in RULES if live_rule(s, r)), default=0.0)
                text = f"余额：USDT {from_wei(self.usdt):,.2f}（可用 {self.usd():,.2f}）" + (f"｜签名钱包 BNB {from_wei(self.bnb):.4f}" if self.bnb is not None else "")
                (warn if need and (self.usd() or 0) < need else ok)(text + (f"｜不够买满一场（最多要 ${need:,.2f}）" if need and (self.usd() or 0) < need else ""))
            if self.chain is not None:
                # the exchanges the markets followed now settle on (each market's isNegRisk × isYieldBearing, as an order checks
                # it): each must have the USDT allowance, or its orders are refused the moment they are due; with no market
                # followed yet, all four told (none of them failing the check)
                used = self.exchanges_in_use()
                for key in EXCHANGES:
                    if used and key not in used:
                        continue
                    name = EXCHANGE_NAMES[key]
                    try:
                        got = await self.read_allowance(key)
                        if got[1] >= 10 ** 20:
                            ok(f"授权：{name} USDT 额度已授权")
                        elif not used:
                            warn(f"授权：{name} 没有 USDT 额度（现在没有跟踪的市场，用到它的市场才需要）")
                        else:
                            no(f"授权：{name} 没有 USDT 额度（跟踪的市场要用：在 Predict 网站用这个账户交易一次）")
                    except Exception as error:
                        warn(f"授权：{name} 没读到（{clean_error(error) or type(error).__name__}）")
            # a speed bump Predict puts on the takers of a market followed: the race is then decided at Predict, not here
            delays = sorted({f"{RULE_OF[w['rule']].name}：{'、'.join(bump)}" for w in self.track.values()
                             if w.get("market") is not None and (bump := delay_fields(w["market"]))})
            if delays:
                warn("Predict 对这些市场的吃单有延迟设置（吃单的单要在 Predict 那边等这么久才撮合）：" + "；".join(delays))
            else:
                ok("吃单延迟：跟踪的市场没有这类设置（speedBump / delay）")
            if self.api.fresh() if hasattr(self.api, "fresh") else False:
                try:
                    await self.api.authed("POST", "/orders", {"data": {}}, priority=True)
                    warn("下单接口对空订单没有报错（意外），请用 /test 挂单测试确认")
                except Exception as error:
                    status = http_status(error)
                    if status == 403:
                        no(f"下单接口 403：这个区域可能不允许交易（{clean_error(error)}）")
                    elif status == 401:
                        no(f"下单接口 401：登录被拒（{clean_error(error)}）")
                    elif 400 <= status < 500:
                        ok(f"下单接口在这个区域可用（空订单被拒为无效：HTTP {status}，这是预期的；要连签名一起验证用 /test）")
                    else:
                        warn(f"下单接口没回答清楚：{clean_error(error) or type(error).__name__}")
        if self.config.api_key:
            mid = next((str(w["market"]["id"]) for w in sorted(self.track.values(), key=lambda w: w["start"]) if w["market"] is not None), "")
            began = time.perf_counter()
            try:
                if mid:
                    await self.api.orderbook(mid)
                else:
                    await category_market(self.api, f"btc-updown-15m-{(now - now % 900_000) // 1000}")
                took = (time.perf_counter() - began) * 1000
                net = nethttp.HTTP_NET.get(getattr(self.api, "host", "")) or {}
                (ok if took < 150 else warn)(f"Predict 读取：一次往返 {took:.0f} ms" + (f"｜入口 {net['edge']}" if net.get("edge") else "")
                                              + ("" if took < 150 else "（偏慢：rt.sh probe 量一下别的区域）"))
                # (that read's answer brought the key's figures: is anyone else on it?)
                line = key_budget_line(net.get("limits") or "", self.api.pacer.last_minute())
                if line:
                    (warn if line[0] else ok)(line[1])
            except Exception as error:
                no(f"Predict 读取失败：{clean_error(error) or type(error).__name__}")
        elif s.mode != "off":
            no("没有 API Key（RULE_API_KEY / PREDICT_API_KEY）")
        # the spare keys: each asked once now (a read through it alone), so one Predict refuses is found here, not in a race
        keys = list(getattr(self.api, "keys", ()))
        if len(keys) > 1:
            path = f"/categories/btc-updown-15m-{(now - now % 900_000) // 1000}"
            fine, refused = [], []
            for k in range(1, len(keys)):
                name = f"第 {k + 1} 把（…{key_tail(keys[k])}）"
                try:
                    fine.append(f"{name}可用 {await self.api.try_key(k, path):.0f} ms")
                except Exception as error:
                    if http_status(error) == 404:
                        fine.append(f"{name}可用")  # (that quarter not listed yet: the key passed)
                    else:
                        refused.append(f"{name}：{clean_error(error) or type(error).__name__}")
            switched = sum(1 for at, _, _ in list(self.api.switches) if time.monotonic() - at < 86_400)
            cooling = [f"第 {k + 1} 把还要冷却 {int(self.api.key_cooling(k)) + 1} 秒" for k in range(len(keys)) if self.api.key_cooling(k) > 0]
            text = (f"API Key：主用 + 备用 {len(keys) - 1} 把，现在用第 {self.api.active + 1} 把" + (f"｜{'、'.join(fine)}" if fine else "")
                    + (f"｜{'、'.join(cooling)}" if cooling else "") + (f"｜最近一天被限流换了 {switched} 次 Key" if switched else ""))
            if refused:
                warn(text)
                no(f"备用 API Key 被 Predict 拒绝：{'；'.join(refused)}（rt.sh edit 改正或删掉这一行）")
            else:
                ok(text)
        limited = sum(1 for at in list(getattr(self.api, "limited", ())) if time.monotonic() - at < 86_400)
        if limited:
            warn(f"最近一天被 Predict 限流（429）{limited} 次：每次这里所有请求停 {PredictApi.BACKOFF_SECONDS} 秒以上，期间不下新单")
        if self.by_market:
            if self.stream.connected and self.stream.alive():
                lag = statistics.median(self.stream.lags) if self.stream.lags else None
                ok(f"推送：已连接（共连接过 {self.stream.connects} 次）" + (f"，推送延迟中位 {lag:.0f} ms" if lag is not None else ""))
            else:
                (no if s.mode != "off" else warn)(f"推送没连上：{self.stream.error or '连接中'}（推送断开时靠 REST 轮流读，慢很多）")
        elif s.mode != "off":
            warn("推送：还没有已发布的规则场次要订阅（场次在开场前约 24 小时发布）")
        if self.telegram is not None and hasattr(self.telegram, "health"):
            fine, line = self.telegram.health()
            (ok if fine else warn)(line)
        if any(self.live_cents(r) > 0 for r in RULES):
            feed = self.index
            if feed is None or feed.ws is None:
                no("币安指数没连上：盘中吃单不会买" + (f"（{feed.error}）" if feed is not None and feed.error else ""))
            else:
                ages = [f"{sym} {fmt(feed.price(sym))}（{(now - int(feed.priced_ms.get(sym) or 0)) / 1000:.1f} 秒前）" for sym in feed.symbols if feed.price(sym)]
                ok("币安指数：已连接" + (f"，{'、'.join(ages)}" if ages else ""))
        if error := str(getattr(self.store, "error", "") or ""):
            no(f"记录：{error}（数据库写不进去时不开新单）")
        else:
            ok("记录：数据库正常" + ("，订单日志已打开" if self.journal_fd is not None else "（订单日志没打开：发单前改为等数据库写完，慢一点）"))
        listed = [w for w in self.track.values() if w["market"] is not None]
        soonest = min(self.track.values(), key=lambda w: w["start"], default=None)
        if s.mode == "off":
            warn("规则场次：模式是关，不跟踪场次")
        elif soonest is None:
            warn("规则场次：接下来一天没有开着的规则场次（价格都是 0、市场关了，或者是周末 / 节假日）")
        else:
            ok(f"规则场次：接下来一天 {len(self.track)} 场，已找到市场 {len(listed)} 场；下一场 {bj(soonest['start'])} {RULE_OF[soonest['rule']].name}"
               + (f"（{soonest['word']}）" if soonest.get("word") else ""))
        if self.settings_dropped:
            warn("启动时删掉了和设置文件冲突的网页设置：" + "、".join(self.settings_dropped))
        if self.errors:
            lines.append("最近问题：")
            lines.extend(f"  {bj(at)} {text}" for at, text in list(self.errors)[-5:])
        if bad:
            lines.append("结论：❌ 先解决上面 ❌ 的几项")
        elif s.mode == "on":
            lines.append("结论：✅ 可以真实吃单" + ("（但现在暂停着）" if self.paused else ""))
        else:
            lines.append(f"结论：✅ 没发现问题（现在是{MODES.get(s.mode, s.mode)}；真实吃单前可以发 /test 做一次挂单测试）")
        return "\n".join(lines)

    TEST_MAX_PRICE = D("0.02")   # the order test's bid: 2¢ at the most, and a cent under the best bid — never at or above the best ask
    TEST_USD = 1.0               # ...for about a dollar (Predict's least order)
    TEST_SECONDS = 600           # ...signed to expire ten minutes on, whatever happens to the cancel

    async def speed_test(self, args: str = "") -> str:
        """/speed: the measurement ``python -m ruletaker latency`` makes, run here (read-only, about a minute) and its report sent
        back — the push delay, the REST reads against the pushes, Binance's index, the key's figures — and the service's own push
        connections beside it. With a rate (/speed 4000) the REST reads take a quarter of that, this once. Never in a race or
        within SPEED_CLEAR_MS of one (the key's budget is the race's); one at a time."""
        busy = self.busy_in(self.now())
        if busy is not None and busy < SPEED_CLEAR_MS:
            return ("⏳ 正在抢单时段：不测速（它要占这把 Key 四分之一的额度），过了再发 /speed" if busy <= 0 else
                    f"⏳ {busy // 60_000 + 1} 分钟后就是抢单时段：不测速（它要占这把 Key 四分之一的额度），过了再发 /speed")
        config = self.config
        if args.strip():
            try:
                rate = float(args.strip())
            except ValueError:
                rate = 0.0
            if not 1 <= rate <= 6000:
                return "用法：/speed，或 /speed 4000（4000 = 这次测量用的每分钟请求数，1～6000；设置不动）"
            config = dataclasses.replace(config, rate_per_min=rate)
        if self.speeding:
            return "⏳ 测速正在进行，等它的结果（同一时间只测一次）"
        self.speeding = True
        lines: list[str] = []
        try:
            import importlib  # (the command line's own measurement: one code for both)
            await importlib.import_module("ruletaker.__main__").latency(config, say=lines.append)
        finally:
            self.speeding = False
        hedge = self.stream.hedge() if hasattr(self.stream, "hedge") else None
        if hedge is None:
            lines.append("服务自己的推送：1 条连接（RULE_WS_CONNECTIONS=3 开 3 条，谁先送到用谁的；开了以后这里写省了多少）")
        elif hedge[1] is None:
            lines.append(f"服务自己的推送：{len(hedge[0])} 条连接，还没有可比的盘口")
        else:
            firsts, behind, copies = hedge
            lines.append(f"服务自己的推送：{len(firsts)} 条连接，各自先到 {'/'.join(str(f) for f in firsts)} 次，同一份盘口后到的比先到的晚 中位 "
                         f"{behind:.1f} ms（{copies} 次；这就是多开连接省下的）")
        return "📶 测速结果\n" + "\n".join(lines)

    async def order_test(self) -> str:
        """The real order path end to end with about a dollar for a few seconds (huice's /live test): a LIMIT buy of Up far under
        the market (2¢ at the most, a cent under the best bid, never at or above the best ask: it rests), on a rule window whose
        matching is open — else the BTC quarter hour running now; read back by its hash, withdrawn, read again. Each step with
        Predict's answer and its round trip. The order is signed to expire in ten minutes, and is no rule order (not recorded as
        one, nothing follows it but this). One at a time: asked again while one is under way (Telegram, the console, both),
        the second is told so and places nothing."""
        if self.testing:
            return "⏳ 挂单测试正在进行，等它的结果（同一时间只做一张，这次没有再挂）"
        self.testing = True
        try:
            return await self._run_order_test()
        finally:
            self.testing = False

    async def _run_order_test(self) -> str:
        import math
        now = self.now()
        if self.wallet is None:
            return "❌ 没有设置 PREDICT_PRIVATE_KEY，做不了挂单测试"
        if self.stop.is_set():
            return "服务正在停止"
        if why := self.test_gate():  # the test is a real order: the hard switch and the pause hold it as they hold every order
            return f"❌ 挂单测试没做：{why}"
        try:
            await self.api.ensure_auth()
        except Exception as error:
            return f"❌ Predict 登录失败：{clean_error(error) or type(error).__name__}"
        markets = [(slug, w["market"]) for slug, w in sorted(self.track.items(), key=lambda kv: kv[1]["start"]) if w["market"] is not None]
        btc = f"btc-updown-15m-{(now - now % 900_000) // 1000}"
        try:
            found = await category_market(self.api, btc)
            if found is not None:
                markets.append((btc, found))
        except Exception:
            pass
        tried = []
        for slug, market in markets[:4]:
            try:
                market = {**market, **(await self.api.market(str(market["id"])))}
            except Exception as error:
                tried.append(f"{slug}：读不到详情（{clean_error(error) or type(error).__name__}）")
                continue
            if str(market.get("tradingStatus") or "").upper() != "OPEN":
                tried.append(f"{slug}：撮合没开启（{market.get('tradingStatus') or '?'}）")
                continue
            info = parse_market(market, self.config.fee_bps)
            outcome, assumed = outcome_for_side("up", info.outcomes)
            if outcome is None or assumed:
                tried.append(f"{slug}：认不出涨的代币")
                continue
            bids, asks = await self.api.orderbook(info.market_id)
            best_bid = bids[0][0] if bids else D(0)
            best_ask = asks[0][0] if asks else D(1)
            price = max(D("0.01"), min(self.TEST_MAX_PRICE, best_bid - D("0.01")))
            if price >= best_ask:
                tried.append(f"{slug}：最低卖价 {float(best_ask) * 100:g}¢，放不下一张不成交的测试单")
                continue
            shares = D(math.ceil(self.TEST_USD / float(price)))
            amounts = limit_amounts(True, to_wei(price), to_wei(shares))
            fields = build_order(self.wallet, outcome["token"], amounts["maker"], amounts["taker"], info.fee_bps, now // 1000 + self.TEST_SECONDS)
            digest, signature = self.wallet.sign_order(fields, info.neg_risk, info.yield_bearing)
            body = dumps_bytes({"data": {"order": {**fields, "signature": signature, "hash": digest}, "strategy": "LIMIT",
                                         "pricePerShare": str(amounts["price_per_share"])}})
            name = market.get("question") or market.get("title") or slug
            lines = [f"🧪 挂单测试 {bj(now)}：{name}（市场 {info.market_id}）买涨 {float(price) * 100:g}¢ × {shares:g} 份，"
                     f"最多花 ${from_wei(amounts['maker']):.2f}，{self.TEST_SECONDS // 60} 分钟后自动过期；盘口最高买价 {float(best_bid) * 100:g}¢、最低卖价 {float(best_ask) * 100:g}¢"]
            record = {"at": now, "slug": slug, "market_id": info.market_id, "price": float(price), "shares": float(shares), "hash": digest.lower(), "steps": lines}
            self.store.put(f"test:{digest.lower()}", record)
            began = time.perf_counter()
            try:  # (judged again right before its bytes leave, as a rule order is: the switch, the pause, the stop)
                result = await self.api.create_order(body, digest, checked=False, check="test:" + digest.lower())
                oid = str(result.get("order_id") or "")
                lines.append(f"1/4 下单成功：订单 #{oid}（往返 {(time.perf_counter() - began) * 1000:.0f} ms）")
            except Exception as error:
                if placed_or_not(error) != "unknown":
                    lines.append(f"1/4 下单失败（{(time.perf_counter() - began) * 1000:.0f} ms）：{clean_error(error) or type(error).__name__}")
                    self.store.put(f"test:{digest.lower()}", {**record, "steps": lines})
                    lines.append("结论：❌ 这张测试单没下成：看上面 Predict 的说法" + ("（403 = 这个区域不能交易）" if http_status(error) == 403 else ""))
                    return "\n".join(lines)
                # the answer lost on the way (a timeout, a 5xx, a 409): Predict may hold the order — looked for by its hash, and
                # withdrawn by the id it shows, as a rule order is
                lines.append(f"1/4 下单的应答没收到（{(time.perf_counter() - began) * 1000:.0f} ms）：{clean_error(error) or type(error).__name__}"
                             "——Predict 可能已经收下了，按哈希核对")
                oid = ""
                for attempt in range(TEST_LOOKUPS):
                    if attempt:
                        await asyncio.sleep(self.lookup_s)
                    with contextlib.suppress(Exception):
                        row = await self.api.order("", digest)
                        if row and row.get("id") not in (None, ""):
                            oid = str(row["id"])
                            break
                if not oid:
                    lines.append(f"结论：⚠️ 下单结果未知：按哈希查了 {TEST_LOOKUPS} 次都没有这张单（多半没下成）；它签的是 {self.TEST_SECONDS // 60} 分钟后过期，"
                                 "万一挂上了也会自己失效——到 Predict 网站的订单里核对一下")
                    self.store.put(f"test:{digest.lower()}", {**record, "steps": lines, "unknown": True})
                    return "\n".join(lines)
                lines.append(f"1/4 按哈希找到了：订单 #{oid}（Predict 收下了，只是应答没送到）")
            began = time.perf_counter()
            filled_seen = 0.0  # (fills only grow: the most any step read is the order's)
            try:
                row = await self.api.order(oid, digest)
                filled_now, status = order_fill(row, float(shares)) if row else (0.0, "")
                filled_seen = filled_now
                lines.append(f"2/4 按哈希查到：{status or '?'}，成交 {filled_now:g}/{shares:g}（往返 {(time.perf_counter() - began) * 1000:.0f} ms）" if row
                             else f"2/4 按哈希暂时查不到（往返 {(time.perf_counter() - began) * 1000:.0f} ms）：Predict 的记录可能晚一点出现")
            except Exception as error:
                lines.append(f"2/4 查询失败：{clean_error(error) or type(error).__name__}")
            began = time.perf_counter()
            removed = False
            try:
                answer = await self.api.remove_orders([oid])
                removed = oid in (answer.get("removed") or []) or oid in (answer.get("noop") or [])
                lines.append(f"3/4 撤单成功（往返 {(time.perf_counter() - began) * 1000:.0f} ms）" if removed else
                             f"3/4 撤单请求发出，Predict 的应答里没有它（往返 {(time.perf_counter() - began) * 1000:.0f} ms；"
                             f"应答：{getattr(self.api, 'last_remove', '')}）")
            except Exception as error:
                lines.append(f"3/4 撤单失败：{clean_error(error) or type(error).__name__}（它 {self.TEST_SECONDS // 60} 分钟后自己过期）")
            if not removed:
                # the answer did not name it: the order itself says whether it is gone (a cancel Predict carries out a moment
                # later); still open after a few reads, asked once more
                for attempt in range(TEST_LOOKUPS + 1):
                    await asyncio.sleep(self.lookup_s / 3)
                    with contextlib.suppress(Exception):
                        row = await self.api.order(oid, digest)
                        if row:
                            got, status = order_fill(row, float(shares))
                            filled_seen = max(filled_seen, got)
                            if status and status != "OPEN":
                                removed = True
                                lines.append(f"3/4 再查：已经是 {status}，撤单生效了")
                                break
                    if attempt == TEST_LOOKUPS // 2:
                        with contextlib.suppress(Exception):
                            again = await self.api.remove_orders([oid])
                            if oid in (again.get("removed") or []) or oid in (again.get("noop") or []):
                                removed = True
                                lines.append("3/4 再撤一次：撤单成功")
                                break
                if not removed:
                    lines.append(f"3/4 再查了几次仍是挂着（最后一次撤单应答：{getattr(self.api, 'last_remove', '')}）")
            def spent(n: float) -> str:
                return f"（花了约 ${n * float(price):.2f}，这几份照常结算）" if n > DUST else ""
            final_read = False
            try:
                row = await self.api.order(oid, digest)
                if row:
                    got, status = order_fill(row, float(shares))
                    filled_seen, final_read = max(filled_seen, got), True
                    lines.append(f"4/4 最终状态：{status or '?'}，成交 {filled_seen:g} 份" + spent(filled_seen))
                else:
                    lines.append(f"4/4 最终状态暂时查不到：成交按前面查到的 {filled_seen:g} 份记" + spent(filled_seen))
            except Exception as error:
                lines.append(f"4/4 最终状态没读到：{clean_error(error) or type(error).__name__}——成交按前面查到的 {filled_seen:g} 份记" + spent(filled_seen))
            lines.append("结论：" + ("⚠️ 下单成功但撤单没确认：到 Predict 网站看一下这张单（10 分钟后自己过期）" if not removed
                                  else "⚠️ 下单、撤单都正常，但最后一次查询没读到这张单的最终状态：成交数以 Predict 网站为准，核对一下" if not final_read
                                  else "✅ 下单、查询、撤单都正常：签名、授权、这个区域都没问题，可以真实吃单"
                                  + (f"（这张测试单撤掉前成交了 {filled_seen:g} 份）" if filled_seen > DUST else "")))
            self.store.put(f"test:{digest.lower()}", {**record, "steps": lines, "order_id": oid, "removed": removed, "filled": filled_seen,
                                                      "final_read": final_read})
            return "\n".join(lines)
        return "❌ 没找到能做挂单测试的市场（需要撮合已开启、最低卖价高于 1¢）：\n" + "\n".join(tried[:6])

    # -- the control page and the review page --
    async def on_disk(self, what: str, back: str) -> str:
        """The wait for an operator's change to reach the disk; the line to add when it did not."""
        if self.store.writer is None or await asyncio.to_thread(self.store.flush, self.flush_wait_s):
            return ""
        error = str(getattr(self.store, "error", "") or "") or "数据库忙，等了也没写完"
        return f"⚠️ {what}已经生效，但还没写进数据库（{error}）：写完之前服务重启的话会{back}；数据库恢复后会自动补写"

    async def save_settings(self, values: dict) -> str:
        """The control page's save: checked whole against the settings file with the other saved values (a wrong one changes
        nothing), in force at once — in the same step as the check, so nothing (a mode switched meanwhile, another save) can
        come between them — and saved; the windows planned again with it at the next tick. The reply waits for the disk:
        NotOnDisk when the write did not get there."""
        values = console.clean(values)
        saved = {**self.saved_settings, **values}
        settings, cap = console.build(self.base_env, saved, self.settings.mode)
        self.saved_settings, self.loss_cap = saved, cap
        self.store.put(console.SAVED, saved)
        self.configure(settings)
        self.hold_loss_stop()
        names = "、".join(console.FIELDS[k][1] + "＝" + (v or "（空）") for k, v in values.items())
        late = await self.on_disk("设置", "回到改之前的值")
        if late:
            self.alarm(f"⚙️ 规则吃单设置已改（控制台）：{names}\n{late}")
            raise NotOnDisk(f"{names}：{late}")
        self.notify(f"⚙️ 规则吃单设置已改（控制台）：{names}")
        return f"已保存：{names}"

    async def reset_settings(self, keys: list | None) -> str:
        """Back to the settings file's value for these fields (``None``: all of them)."""
        keys = [k for k in (list(self.saved_settings) if keys is None else keys) if k in self.saved_settings]
        saved = {k: v for k, v in self.saved_settings.items() if k not in keys}
        settings, cap = console.build(self.base_env, saved, self.settings.mode)
        self.saved_settings, self.loss_cap = saved, cap
        self.store.put(console.SAVED, saved)
        self.configure(settings)  # (in force at once, as a save is)
        self.hold_loss_stop()
        reply = "已恢复部署设置：" + ("、".join(console.FIELDS[k][1] for k in keys if k in console.FIELDS) or "（没有网页保存的值）")
        late = await self.on_disk("恢复部署设置", "回到网页保存的值")
        if late:
            raise NotOnDisk(f"{reply}\n{late}")
        return reply

    def menu_head(self) -> tuple[str, str]:
        """The Telegram menu card's head: what is in force now (no status text worked out), and the mode."""
        paused = f"已暂停（{self.paused}）" if self.paused else "没暂停"
        usd = self.usd()
        return (f"模式：{MODES.get(self.settings.mode, self.settings.mode)}｜{paused}｜今日已结算 {money(self.today_pnl())}"
                + (f"｜可用 ${usd:,.2f}" if usd is not None else "") + ("｜今日已到停机线" if self.loss_stop_why() else ""),
                self.settings.mode)

    def test_gate(self) -> str:
        """Why the operator's order test may not go out now ("" when it may): the deploy's hard switch, the pause, the stop."""
        if self.stop.is_set():
            return "服务正在停止，不再发单"
        if not self.config.trading_allowed:
            return "部署变量 LIVE_TRADING_ALLOWED 不是 on——这个硬开关关着时一张真实的单都不发，挂单测试也是真实的单"
        if self.paused:
            return (f"已暂停（{self.paused}）：暂停时一张新单都不开，挂单测试也不做。"
                    "要测：先 /mode paper（只记录，不会自动下单），再 /resume，再 /test")
        return ""

    def settings_view(self) -> list:
        return console.view(self.settings, self.loss_cap, self.saved_settings)

    def review_data(self, days: int = 30) -> dict:
        """The review page's numbers (review.review), kept 30 s; not worked out in the race's seconds — the one kept is given
        then (a few ms with hundreds of records, but never on an order's way)."""
        from .review import review
        now = self.now()
        held = self.review_held.get(days)
        if held is not None and (now - held[0] < 30_000 or self.quiet_ms(now) > 0):
            return {**held[1], "computed_ms": held[0]}
        if self.quiet_ms(now) > 0:
            return {"busy": "抢单时刻前后这几秒不算复盘数据，稍后再刷新", "computed_ms": now}
        data = review(self.recs, self.settings, now, days)
        self.review_held[days] = (now, data)
        return {**data, "computed_ms": now}

    def state(self) -> dict:
        """The control page's view of now: the mode and why no order goes out, the account, the connections, the per-second
        hours and the makers' moments ahead, each rule's next window and what its last look said, the speed, the network,
        the problems, the settings."""
        now, s = self.now(), self.settings
        lags = list(self.loop_lags)
        host = getattr(self.api, "host", "")
        net = nethttp.HTTP_NET.get(host) or {}
        trips = list(getattr(self.api, "trips", ()) or ())
        mid = lambda xs: round(statistics.median(xs), 1) if xs else None
        reads, writes = [ms for m, ms in trips if m == "GET"], [ms for m, ms in trips if m != "GET"]
        rules = []
        for rule in RULES:
            nxt = min((w for w in self.track.values() if w["rule"] == rule.key), key=lambda w: w["start"], default=None)
            rules.append({"key": rule.key, "name": rule.name, "market": rule.market, "side": rule.side, "word": rule.word, "when": rule.when,
                          "cents": s.price(rule.key) if s.on(rule.market) else 0, "live_cents": self.live_cents(rule),
                          "next_ms": nxt["start"] if nxt else None, "moment_ms": self.moment(nxt) if nxt else None,
                          "listed": bool(nxt and nxt["market"] is not None), "state": (nxt or {}).get("word", ""),
                          "phase": self.phase(nxt, now) if nxt else ""})
        wait = s.fast_in(now)
        floor = self.loss_floor()
        return {"version": VERSION, "now_ms": now, "region": self.region, "mode": s.mode, "env_mode": self.env_mode, "modes": MODES,
                "paused": self.paused, "blocked": self.block(0.0) if s.mode == "on" else "", "ready": self.ready,
                "not_ready": self.not_ready_why if s.mode == "on" and not self.ready else "", "trading_allowed": self.config.trading_allowed,
                "account": {"kind": self.wallet.kind if self.wallet else "", "maker": self.wallet.maker if self.wallet else "",
                            "usdt": from_wei(self.usdt) if self.usdt is not None else None, "spendable": self.usd(),
                            "bnb": from_wei(self.bnb) if self.bnb is not None else None, "balance_ms": self.balance_ms,
                            "balance_error": self.balance_error, "signed_in": bool(self.api.fresh()) if hasattr(self.api, "fresh") else False},
                "stream": {"connected": self.stream.connected, "alive": self.stream.alive(), "error": self.stream.error,
                           "connects": self.stream.connects, "lag_ms": mid(list(self.stream.lags))},
                "index": ({"connected": self.index.ws is not None, "error": self.index.error, "pushes": self.index.pushes}
                          if self.index is not None and self.index_wanted() else None),
                "fast": {"hours": day_spans_text(s.fast), "active": self.fast, "next_ms": now + wait if wait else None,
                         "looks": self.fast_looks, "verifies": self.verifies, "diffs": self.verify_diffs},
                "lead_minutes": s.lead_minutes, "plan": self.first_plan(now),
                "today": {"spent": round(spent_on(self.recent(), day_of(now), s.mode), 2), "pnl": round(self.today_pnl(), 4),
                          "max_day_usd": s.max_day_usd, "loss_cap": self.loss_cap, "loss_floor": floor,
                          "open_cost": round(open_cost(self.recs), 2)},
                "rules": rules, "speed": speed_stats(self.recs, now - 30 * DAY_MS),
                "net": {"region": self.region, "host": host, "edge": net.get("edge", ""), "connect_ms": mid(list(net.get("connect") or ())),
                        "read_ms": mid(reads), "read_best_ms": round(min(reads), 1) if reads else None,
                        "write_ms": mid(writes), "write_best_ms": round(min(writes), 1) if writes else None},
                "process": {"loop_lag_ms": mid(lags), "loop_lag_max_ms": round(max(lags), 1) if lags else None,
                            "native_signing": native_signing(), "presigned": self.presigned, "presign_hits": self.presign_hits,
                            "first_ticks": self.first_ticks, "eager": EAGER, "python": f"{sys.version_info[0]}.{sys.version_info[1]}"},
                "errors": [{"at": at, "text": text} for at, text in list(self.errors)[-12:]][::-1],
                "settings": self.settings_view(), "settings_dropped": self.settings_dropped,
                "fast_line": self.fast_line(now)}

    def mode_text(self) -> str:
        why = self.block(0.0) if self.settings.mode == "on" else ""
        return MODES.get(self.settings.mode, self.settings.mode) + (f"（现在不下单：{why}）" if why else "")

    # -- what it says --
    def fast_line(self, now: int) -> str:
        s = self.settings
        lead = int(getattr(s, "lead_minutes", 0) or 0)
        ahead = sorted((self.moment(w), w["start"]) for w in self.track.values() if self.moment(w) and now - self.moment(w) < SPRINT_MS)
        moment = (f"｜对手提前 {lead} 分钟挂单的时刻前后也每秒检查（RULE_LEAD_MINUTES）"
                  + (f"，下一个 {bj(ahead[0][0], False)}（{bj(ahead[0][1], False)} 的场）" if ahead else "")) if lead else ""
        if not s.fast:
            text, wait = "每秒检查时段：关（RULE_FAST_HOURS=off）", None
        else:
            text, wait = f"每秒检查时段（北京时间）：{day_spans_text(s.fast)}", s.fast_in(now)
        if self.fast:
            listed = sum(1 for w in self.track.values() if w["market"] is not None)
            lag, trip = (statistics.median(self.stream.lags) if self.stream.lags else None), (statistics.median(self.verify_ms) if self.verify_ms else None)
            text += (f"｜进行中{'（有场次在盘中）' if any(self.phase(w, now) == 'live' for w in self.track.values()) else ''}：每秒看 {listed} 场（已看 {self.fast_looks} 次）；"
                     + ("推送正常" if self.stream.alive() else "推送断开，靠 REST 轮流读")
                     + (f"，推送延迟中位 {lag:.0f} ms" if lag is not None else "")
                     + f"；REST 核对 {self.verifies} 次" + (f"，{self.verify_diffs} 次发现推送没送到的变化" if self.verify_diffs else "")
                     + (f"，一次往返中位 {trip:.0f} ms" if trip is not None else "")
                     + (f"；等撮合开启时读状态 {self.status_reads} 次" if self.status_reads else "")
                     + (f"；核对读取失败：{self.verify_error}" if self.verify_error else ""))
        elif wait is not None:
            text += f"｜下一次 {bj(now + wait, False)} 开始" + ("，正在准备（登录、授权额度、撮合状态、推送订阅）" if wait <= WARM_MS else "，提前 1 分钟准备")
        else:
            text += "：推送一有变化就看，另外每 30 秒读一次、开场前 15 分钟每 2 秒"
        return text + moment

    def first_plan(self, now: int) -> str:
        s, moments, spans = self.settings, set(), []
        for w in sorted(self.track.values(), key=lambda w: w["start"]):
            rule = RULE_OF[w["rule"]]
            at, end = self.moment(w), self.end_of(w)
            if at and now < at + QUIET_AFTER_MS and s.price(rule.key) > 0:
                moments.add(at)
            if self.live_cents(rule) > 0 and now < end - LIVE_STOP_MS:
                if spans and spans[-1][1] >= w["start"]:
                    spans[-1][1] = max(spans[-1][1], end)
                else:
                    spans.append([w["start"], end])

        def told(items: list) -> str:
            out, day = [], day_of(now)
            for at, text in items:
                out.append((bj(at)[:6] if day_of(at) != day else "") + text)
                day = day_of(at)
            return "、".join(out)
        parts = []
        if moments:
            shown = sorted(moments)[:8]
            parts.append("盘前抢单时刻 " + told([(at, bj(at, False)) for at in shown]) + (f" 等 {len(moments)} 个" if len(moments) > len(shown) else ""))
        if spans:
            whole = (self.planned_ms or now) + HORIZON_MS
            parts.append("盘中 " + told([(a, f"{bj(a, False)}–{bj(b, False)}" if b < whole else f"{bj(a, False)} 起") for a, b in spans[:4]])
                         + (f" 等 {len(spans)} 段" if len(spans) > 4 else ""))
        return "接下来一天：" + "；".join(parts) if parts else ""

    def speed_text(self) -> str:
        """How the process itself is doing (the race is the network's and this): the loop's delay, the signing, the orders
        signed beforehand, the garbage collector."""
        lags = list(self.loop_lags)
        parts = [f"独立进程（Python {sys.version_info[0]}.{sys.version_info[1]}" + ("，盘口一到当场处理" if EAGER else "，需要 3.12 才能当场处理") + "）"]
        if lags:
            parts.append(f"主循环延迟近 1 分钟中位 {statistics.median(lags):.1f} ms、最长 {max(lags):.0f} ms")
        parts.append("签名：" + ("原生 secp256k1（coincurve）" if native_signing() else "纯 Python（装 coincurve 更快）"))
        if self.presigned or self.presign_hits:
            parts.append(f"预先签好 {self.presigned} 张，下单时用上 {self.presign_hits} 张")
        if self.first_ticks:
            parts.append(f"推送当场处理 {self.first_ticks} 次")
        hedge = self.stream.hedge() if hasattr(self.stream, "hedge") else None
        if hedge:
            firsts, behind, copies = hedge
            parts.append(f"推送 {len(firsts)} 条连接：各自先到 {'/'.join(str(f) for f in firsts)} 次"
                         + (f"，同一份盘口后到的比先到的晚 中位 {behind:.1f} ms（{copies} 次；这就是多开连接省下的）" if behind is not None else ""))
        if self.index_looks or self.index_late:
            parts.append(f"盘中由指数带来的检查 {self.index_looks} 次" + (f"（另有 {self.index_late} 次指数到达时已超过 {INDEX_FRESH_MS / 1000:g} 秒）" if self.index_late else ""))
        return "速度：" + "｜".join(parts)

    def net_line(self) -> str:
        host = self.api.host if hasattr(self.api, "host") else urllib.parse.urlsplit(PREDICT_REST).hostname or ""
        net = nethttp.HTTP_NET.get(host) or {}
        trips = list(getattr(self.api, "trips", ()) or ())
        reads, writes = [ms for method, ms in trips if method == "GET"], [ms for method, ms in trips if method != "GET"]
        spread = lambda xs: f"中位 {statistics.median(xs):.0f} ms（最快 {min(xs):.0f}）"
        connect = list(net.get("connect") or ())
        parts = [f"服务在 {self.region or '未知区域'}"]
        if connect:
            parts.append(f"到 {host} 新建连接 {spread(connect)}（TCP+TLS，2～3 个来回）")
        if net.get("edge"):
            parts.append(f"入口 {net['edge']}")
        keys = list(getattr(self.api, "keys", ()))
        if len(keys) > 1:
            parts.append(f"API Key 在用第 {self.api.active + 1} 把（共 {len(keys)} 把，被限流就换下一把）")
        if reads:
            parts.append(f"读一次 {spread(reads)}")
        if writes:
            parts.append(f"下单 / 撤单一次 {spread(writes)}")
        if not reads and not writes and not connect:
            return "网络：还没有请求"
        hint = ""
        if reads and statistics.median(reads) >= 60:
            hint = "｜一次读取超过 60 ms：这个区域离 Predict 的服务器不近，可以用 python -m ruletaker latency 在别的区域量一下"
        return "网络（不含排队）：" + "｜".join(parts) + hint

    def speed_line(self, now: int) -> str:
        sp = speed_stats(self.recs, now - 30 * DAY_MS)
        if not sp:
            return ""
        best = sp.get("best") or {}
        ms = lambda key: "—" if sp.get(key) is None else f"{sp[key]:.0f} ms" + (f"（最快 {best[key]:.0f}）" if best.get(key) is not None else "")
        cs = course_speed(self.recs, now - 30 * DAY_MS)
        return (f"下单速度（近 30 天真实订单 {sp['orders']} 张，{sp['filled']} 张有成交）：盘口变化→推送到达 中位 {ms('lag')}、"
                f"到达→发出 {ms('own')}" + (f"［其中到达→开始处理 {sp['wake']:.0f} ms］" if sp.get("wake") is not None else "")
                + f"、发出→应答 {ms('trip')}"
                + (f"｜盘中由指数带来的 {cs['orders']} 张：指数到达→发出 中位 {cs['own']:.0f} ms（最快 {cs['best']:.0f}）" if cs else ""))

    def account_line(self) -> str:
        if self.wallet is None:
            return "账户：没有设置 PREDICT_PRIVATE_KEY（只能 off / paper）"
        bal = f"USDT {from_wei(self.usdt):,.2f}" if self.usdt is not None else f"USDT 未读到（{self.balance_error or '尚未读取'}）"
        bnb = f"｜BNB {from_wei(self.bnb):.4f}" if self.bnb is not None else ""
        return (f"账户：{self.wallet.kind} {self.wallet.maker[:6]}…{self.wallet.maker[-4:]}｜{bal}{bnb}"
                + f"｜{'已登录' if self.api.fresh() else '未登录'}" + (f"｜未就绪：{self.not_ready_why}" if self.settings.mode == "on" and not self.ready else "")
                + (f"｜在途和未结算 ${open_cost(self.recs):,.2f}" if open_cost(self.recs) > 0 else ""))

    def calendar_note(self, now: int) -> str:
        """The status's word on the holiday calendar in its last CALENDAR_WARN_DAYS ("" before)."""
        left = self.calendar_left(now)
        if left is None or left > CALENDAR_WARN_DAYS:
            return ""
        last = self.cal_last.isoformat()
        if left == 0:
            return f"｜⛔ CME 休市日历只排到 {last}，不再下单：补上 CME_CLOSED_DAYS、CME_EARLY_DAYS 再重启"
        return f"｜⚠️ CME 休市日历只排到 {last}（还剩 {left} 天）"

    def status(self) -> str:
        now, s = self.now(), self.settings
        recs = self.recs
        in_course = any(self.live_cents(r) > 0 for r in RULES)
        lines = [f"规则吃单 {VERSION}（独立服务）：{MODES.get(s.mode, s.mode)}（RULE_TAKER={self.env_mode}"
                 + (f"，运行中调成 {s.mode}" if s.mode != self.env_mode else "") + f"）｜{'开场前吃' if in_course else '只在开场前吃'}，盘口一有就买，到开场前 {STOP_MS // 1000} 秒"
                 + (f"；设了盘中价格的规则开场后接着买（开场 {LIVE_START_MS // 1000} 秒后到结束前 {LIVE_STOP_MS // 1000} 秒，第 1 分钟最高按盘前的价格）" if in_course else "")
                 + self.calendar_note(now)
                 + (f"｜每天最多花 ${s.max_day_usd:g}（今天已花 ${spent_on(recs, day_of(now), s.mode):,.2f}）" if s.max_day_usd else "")
                 + (f"｜日亏损停机线 {money(self.loss_floor() or 0)}（今天已结算 {money(day_pnl(recs, day_of(now)))}"
                    + ("，今天已停机：/resume 解除" if self.loss_stop_why() else "") + "）" if self.loss_floor() is not None else "")
                 + f"｜推送 {'已连接' if self.stream.connected else '未连接' + ('（' + self.stream.error + '）' if self.stream.error else '')}"
                 + (f"｜币安指数 {'已连接' if self.index.ws is not None else '未连接' + ('（' + self.index.error + '）' if self.index.error else '')}" if self.index is not None and in_course else "")]
        if s.mode == "on" and (why := self.block(0.0)):
            lines.append(f"⏸ 现在不下单：{why}")
        lines.append(self.account_line())
        if s.mode != "off":
            lines.append(self.fast_line(now))
            if plan := self.first_plan(now):
                lines.append(plan)
            lines.append(self.speed_text())
            if speed := self.speed_line(now):
                lines.append(speed)
            lines.append(self.net_line())
        for market, name in MARKET_NAMES.items():
            lines.append(f"{name}：{'开' if s.on(market) else '关'}" + (f"｜每场最多 {s.shares_of(market):g} 份" if s.on(market) else "（这个市场的规则都不做）"))
            if not s.on(market):
                continue
            for rule in (r for r in RULES if r.market == market):
                cents, during = s.price(rule.key), self.live_cents(rule)
                if cents <= 0 and during <= 0:
                    lines.append(f"· {rule.name}：不做（价格设为 0）")
                    continue
                nxt = min((w for w in self.track.values() if w["rule"] == rule.key), key=lambda w: w["start"], default=None)
                st = watch_stats(recs, rule.key, s.mode, now - 30 * DAY_MS) if s.mode != "off" and cents > 0 else {"watched": 0}
                cs = course_stats(recs, rule.key, s.mode, now - 30 * DAY_MS) if s.mode != "off" else {"windows": 0}
                lines.append(f"· {rule.name} 买{rule.word} " + (f"≤{cents}¢" if cents > 0 else "盘前不买")
                             + (f"｜盘中 ≤{during}¢（{lead_note(rule)}）" if during > 0 else "") + f"：{rule.when}"
                             + (f"｜{'这一场' if nxt['start'] <= now else '下一场'} {bj(nxt['start'])}{('：' + nxt['word']) if nxt['word'] else ''}" if nxt else "")
                             + (f"｜近 30 天看了 {st['watched']} 场，{st['offered']} 场有 ≤{cents}¢ 的卖单"
                                + (f"（平均 {st['offer_avg']:g} 份）" if st["offer_avg"] else "")
                                + (f"，最低卖价中位 {st['best_mid'] * 100:.0f}¢" if st["best_mid"] is not None else "")
                                + (f"，撮合开启在开场前中位 {st['open_lead']:.0f} 秒" if st["open_lead"] is not None else "")
                                + (f"，卖单首次出现在开场前中位 {st['offer_lead']:.0f} 分钟" if st.get("offer_lead") is not None else "")
                                + (f"，{st['lost']} 场的卖单没买到就没了（存活中位 {st['life'] / 1000:.1f} 秒）" if st.get("lost") else "")
                                + (f"，买到的 {st['won']} 场从卖单出现到被吃掉中位 {st['won_life'] / 1000:.1f} 秒" if st.get("won") else "") if st["watched"] else "")
                             + (f"｜盘中近 30 天买到 {cs['windows']} 场 {cs['shares']:g} 份（均价 {cs['cost'] / cs['shares'] * 100:.1f}¢）"
                                + (f"，已结算 {cs['settled']} 场：赢 {cs['wins']} 输 {cs['losses']}，{money(cs['pnl'])}" if cs["settled"] else "") if cs["windows"] else ""))
        if s.mode != "off" and (rows := races(recs, s.mode, now - 30 * DAY_MS)[:RACES_SHOWN]):
            lines.append(f"抢单记录（最近 {len(rows)} 场，北京时间；“开始被吃 / 被吃光”是卖单出现之后多久，按 Predict 盘口自己的时间戳）：")
            if summary := race_summary(races(recs, s.mode, now - 30 * DAY_MS)):
                lines.append(f"  近 30 天：{summary}")
            lines.extend("  " + race_text(row) for row in rows)
        today = day_of(now)
        for mode, word in (("on", "今天"), ("paper", "今天（只记录，没真买）")):
            mine = [r for r in recs.values() if r.get("mode") == mode and day_of(int(r.get("start_ms") or 0)) == today and filled(r) > DUST]
            if mine:
                done = [r for r in mine if r.get("pnl") is not None]
                lines.append(f"{word}：{len(mine)} 场买到 {sum(filled(r) for r in mine):g} 份，成本 ${sum(cost(r) for r in mine):,.2f}"
                             + (f"；已结算 {len(done)} 场 {money(sum(float(r['pnl']) for r in done))}" if done else ""))
        last = sorted((r for r in recs.values() if filled(r) > DUST), key=lambda r: -int(r.get("start_ms") or 0))[:5]
        for r in last:
            lines.append(f"  {bj(int(r['start_ms']))} {(RULE_OF.get(r['rule']) or RULES[0]).name} 买{'涨' if r['side'] == 'up' else '跌'} "
                         f"{avg_price(r) * 100:.1f}¢×{filled(r):g}" + (f"（其中盘中 {course_part(r)[0]:g} 份）" if course_part(r)[0] > DUST else "")
                         + ("（只记录）" if r.get("mode") == "paper" else "")
                         + (f" → {money(float(r['pnl']))}" if r.get("pnl") is not None else "（待结算）"))
        if self.error:
            lines.append(f"最近问题：{self.error}")
        return "\n".join(lines)

    def summary(self) -> dict:
        now = self.now()
        sp = speed_stats(self.recs, now - 30 * DAY_MS)
        return {"version": VERSION, "mode": self.settings.mode, "env_mode": self.env_mode, "paused": self.paused,
                "blocked": self.block(0.0) if self.settings.mode == "on" else "", "ready": self.ready, "region": self.region,
                "usdt": self.usd(), "stream_connected": self.stream.connected, "fast": self.fast, "records": len(self.recs),
                "today_spent": spent_on(self.recs, day_of(now), self.settings.mode), "today_pnl": day_pnl(self.recs, day_of(now)),
                "speed_30d": sp, "loop_lag_ms": statistics.median(self.loop_lags) if self.loop_lags else None,
                "windows": [{"slug": slug, "rule": w["rule"], "start_ms": w["start"], "market": (w["market"] or {}).get("id"), "word": w["word"]}
                            for slug, w in sorted(self.track.items(), key=lambda kv: kv[1]["start"])[:20]],
                "error": self.error}
