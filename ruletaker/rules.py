"""The rules, their windows, the book's arithmetic and the records' sums (a port of huice rules.py 1.154.27, the parts that
are not the taker itself).

A few fixed windows of the gold and oil 15-minute cards have an outcome that is not the coin flip the book prices — gold's
index drifts while CME is shut (an end equal to the start settles Up), the first window after the daily reopen and gold's
05:15 New York window lean Down — and are bought on the rule's side whenever, before the window starts, the book offers
that side at or under the rule's price. 盘中 (RULE_GOLD_SHUT_LIVE_CENTS / RULE_GOLD_OPEN_LIVE_CENTS): gold's shut quarter hours
and the reopen's first window are bought in the window's course too, while the index stands ahead of the start on the
rule's side by Rule.lead."""
from __future__ import annotations

import dataclasses
import statistics
from decimal import Decimal as D
from typing import Any, Callable

from .cards import CARDS, WindowCard
from .clock import et_parts, market_shuts
from .util import DAY_MS, DUST, bj, bj_clock, day_of

PREFIX = "rule:"              # the records' keys in the store: rule:<window slug> (real), rule:paper:<slug> (paper)
FINAL_STATES = {"final", "rejected", "not_sent", "not_found"}
NEVER_PLACED = {"rejected", "not_sent", "not_found"}
OPEN_STATES = {"placing", "open", "unknown"}  # an order on its way or on the book: it may still take what the book offers
MODES = {"off": "关", "paper": "只记录（不下单）", "on": "真实吃单"}
MARKET_NAMES = {"gold": "黄金", "oil": "原油"}
RACES_SHOWN = 8


# --- the windows ------------------------------------------------------------------------------------------------------------
def daily_break(start_ms: int) -> bool:
    """A quarter hour of CME's daily one-hour break: 17:00–17:45 New York, Monday to Thursday (北京冬令时周二至周五 06:00–06:45，
    夏令时 05:00–05:45). The weekend's same hour is not one: the index moves there more."""
    weekday, hour, minute = et_parts(start_ms)
    return weekday <= 3 and hour == 17 and minute % 15 == 0


def daily_reopen(start_ms: int) -> bool:
    """The first quarter hour after CME's daily break: 18:00 New York, Monday to Thursday, the stretch shut before it the
    one-hour break itself — not the weekend's (Sunday's 18:00, 北京周一早上) nor a holiday's."""
    weekday, hour, minute = et_parts(start_ms)
    if weekday > 3 or (hour, minute) != (18, 0):
        return False
    ending = [s for s in market_shuts("cme", start_ms - 60_000, start_ms) if s[1] == start_ms]
    return bool(ending) and ending[0][1] - ending[0][0] <= 65 * 60_000


def ny_slot(hour: int, minute: int) -> Callable[[int], bool]:
    """The quarter hour from hour:minute New York, Monday to Friday, CME open through it."""
    def matches(start_ms: int) -> bool:
        weekday, h, mi = et_parts(start_ms)
        return weekday <= 4 and (h, mi) == (hour, minute) and not market_shuts("cme", start_ms, start_ms + 15 * 60_000)
    matches.__name__ = f"ny_{hour:02d}{minute:02d}"
    return matches


@dataclasses.dataclass(frozen=True)
class Rule:
    key: str                       # its price setting (RULE_*_CENTS) and the records' "rule"
    market: str                    # "gold" / "oil": the switch and the size it goes by (RULE_GOLD / RULE_OIL, RULE_*_SHARES)
    card: str                      # the card whose windows it buys
    side: str                      # "up" / "down"
    name: str                      # 黄金休市 4 档
    when: str                      # its windows, in New York and Beijing time
    matches: Callable[[int], bool]
    lead: D = D(0)                 # 盘中: bought in the window's course while the index is ahead of the start on the rule's side by
    #                                this much (relative); 0 — only before the window

    @property
    def word(self) -> str:
        return "涨" if self.side == "up" else "跌"

    @property
    def spec(self) -> WindowCard:
        return CARDS[self.card]


RULES: tuple[Rule, ...] = (
    Rule("gold_shut", "gold", "XAU-15M", "up", "黄金休市 4 档", "纽约 17:00–17:45，周一至周四（北京冬令时 06:00–06:45、夏令时 05:00–05:45）", daily_break, D("0.00005")),
    Rule("gold_open", "gold", "XAU-15M", "down", "黄金开盘第一档", "纽约 18:00，周一至周四（北京冬令时 07:00、夏令时 06:00；周一早上那场不做）", daily_reopen, D("0.0002")),
    Rule("gold_0515", "gold", "XAU-15M", "down", "黄金纽约 05:15", "纽约 05:15，周一至周五（北京冬令时 18:15、夏令时 17:15）", ny_slot(5, 15)),
    Rule("gold_0500", "gold", "XAU-15M", "up", "黄金纽约 05:00", "纽约 05:00，周一至周五（北京冬令时 18:00、夏令时 17:00）", ny_slot(5, 0)),
    Rule("gold_1645", "gold", "XAU-15M", "up", "黄金休市前一档", "纽约 16:45，周一至周五（北京冬令时 05:45、夏令时 04:45）", ny_slot(16, 45)),
    Rule("oil_shut", "oil", "CL-15M", "down", "原油休市 4 档", "纽约 17:00–17:45，周一至周四（北京冬令时 06:00–06:45、夏令时 05:00–05:45）", daily_break),
    Rule("oil_0045", "oil", "CL-15M", "down", "原油纽约 00:45", "纽约 00:45，周一至周五（北京冬令时 13:45、夏令时 12:45）", ny_slot(0, 45)),
    Rule("oil_open", "oil", "CL-15M", "down", "原油开盘第一档", "纽约 18:00，周一至周四（北京冬令时 07:00、夏令时 06:00；周一早上那场不做）", daily_reopen),
)
RULE_OF = {r.key: r for r in RULES}


def windows(rule: Rule, from_ms: int, to_ms: int) -> list[int]:
    """The starts of the rule's windows in [from_ms, to_ms)."""
    card = rule.spec
    out, start = [], card.first_start(from_ms)
    while start < to_ms:
        if rule.matches(start):
            out.append(start)
        start = card.next_start(start)
    return out


def live_rule(settings: Any, rule: Rule) -> bool:
    """Whether a rule buys before its windows by the settings: its market switched on and a price set."""
    return settings.on(rule.market) and settings.price(rule.key) > 0


def live_price_of(settings: Any, rule: Rule) -> int:
    """The rule's price in its window's course (RULE_*_LIVE_CENTS), 0 = none."""
    if not rule.lead > 0 or not settings.on(rule.market):
        return 0
    return int(settings.live_price(rule.key))


def lead_note(rule: Rule) -> str:
    return f"指数比起点{'高' if rule.side == 'up' else '低'} {float(rule.lead) * 100:g}% 以上才买"


# --- the book -----------------------------------------------------------------------------------------------------------------
def offered(bids: tuple, asks: tuple, side: str, limit: D) -> list[tuple[D, D]]:
    """What a buy of ``side`` at ``limit`` or under takes, the cheapest first: (price, shares). Predict's book is quoted for 涨
    (Up): its asks are the Up shares for sale; a bid for Up at p is Down for sale at 1 − p (the two buys mint a pair)."""
    if side == "up":
        return [(p, q) for p, q in sorted(asks) if p <= limit]
    return [(1 - p, q) for p, q in sorted(bids, reverse=True) if 1 - p <= limit]


def without(levels: list[tuple[D, D]], shares: D) -> list[tuple[D, D]]:
    """``levels`` (the cheapest first) less ``shares`` taken from the cheapest."""
    out, left = [], shares
    for price, size in levels:
        took = min(size, left)
        left -= took
        if size > took:
            out.append((price, size - took))
    return out


def walk(levels: list[tuple[D, D]], shares: D) -> D:
    """The average price of ``shares`` bought from ``levels`` (cheapest first)."""
    left, total = shares, D(0)
    for price, size in levels:
        took = min(size, left)
        total += price * took
        left -= took
        if left <= 0:
            break
    return total / (shares - left) if shares > left else D(0)


# --- the records ----------------------------------------------------------------------------------------------------------------
def rid(slug: str, mode: str) -> str:
    """A record's id (its store key after PREFIX): the window's slug for a real one, ``paper:<slug>`` for a paper one."""
    return slug if mode == "on" else f"{mode}:{slug}"


def fee_share(avg: float, bps: int) -> float:
    """The part of a taker's shares Predict keeps as its fee: rate × min(p, 1 − p) a share, taken in shares, not USDT."""
    return bps / 10_000 * min(avg, 1 - avg) / avg if avg > 0 else 0.0


def order_avg(o: dict) -> float:
    return float(o.get("avg") or o.get("limit") or 0)


def order_bps(o: dict) -> int:
    return int(o.get("fee_bps") if o.get("fee_bps") is not None else 200)


def filled(rec: dict) -> float:
    return sum(float(o.get("filled") or 0) for o in rec.get("orders") or [])


def cost(rec: dict) -> float:
    """What the fills cost in USDT: each order's shares at its average price (its limit while Predict states none)."""
    return sum(float(o.get("filled") or 0) * order_avg(o) for o in rec.get("orders") or [])


def net(rec: dict) -> float:
    """The shares the account holds of the fills: those bought less the fee Predict takes in shares."""
    return sum(float(o.get("filled") or 0) * (1 - fee_share(order_avg(o), order_bps(o))) for o in rec.get("orders") or [])


def avg_price(rec: dict) -> float:
    shares = filled(rec)
    return sum(float(o.get("filled") or 0) * float(o.get("avg") or o.get("limit") or 0) for o in rec.get("orders") or []) / shares if shares > DUST else 0.0


def in_flight(rec: dict) -> bool:
    return any(o.get("state") not in FINAL_STATES for o in rec.get("orders") or [])


def held(rec: dict) -> float:
    """The window's shares bought or being bought: the fills, and the whole of an order whose word is not final."""
    return sum(float(o.get("shares") or 0) if o.get("state") not in FINAL_STATES else float(o.get("filled") or 0) for o in rec.get("orders") or [])


def open_orders(rec: dict) -> list[dict]:
    """The window's orders that may still take from the book: on their way, or on it and not withdrawn."""
    return [o for o in rec.get("orders") or [] if o.get("state") in OPEN_STATES and not o.get("removed")]


def claims(rec: dict) -> float:
    """The shares those orders may still take: asked for and not known filled."""
    return sum(max(float(o.get("shares") or 0) - float(o.get("filled") or 0), 0.0) for o in open_orders(rec))


def spent_on(recs: dict[str, dict], day: str, mode: str) -> float:
    """What the orders placed on a Beijing day cost in USDT, the mode's own records: the fills at their price, and the whole of
    an order whose word is not final at its limit (RULE_MAX_DAY_USD)."""
    total = 0.0
    for rec in recs.values():
        if rec.get("mode") != mode:
            continue
        for o in rec.get("orders") or []:
            if day_of(int(o.get("at") or 0)) != day:
                continue
            if o.get("state") in FINAL_STATES:
                total += float(o.get("filled") or 0) * order_avg(o)
            else:
                total += float(o.get("shares") or 0) * float(o.get("limit") or 0)
    return total


def day_pnl(recs: dict[str, dict], day: str) -> float:
    """The real records' P&L settled on a Beijing day (RULE_MAX_DAILY_LOSS_USD)."""
    return sum(float(r.get("pnl") or 0) for r in recs.values() if r.get("mode") == "on" and r.get("settled_day") == day)


def open_cost(recs: dict[str, dict]) -> float:
    """The real records' money not yet settled: fills unsettled at their cost, orders not final at their limit (status)."""
    total = 0.0
    for rec in recs.values():
        if rec.get("mode") != "on" or rec.get("pnl") is not None:
            continue
        for o in rec.get("orders") or []:
            total += (float(o.get("filled") or 0) * order_avg(o) if o.get("state") in FINAL_STATES
                      else float(o.get("shares") or 0) * float(o.get("limit") or 0))
    return total


# A window's watch (RuleTaker.watch): the lowest offer of the rule's side at any price, the most on offer at the rule's price at once,
# when that first showed, when the window was listed and matching first seen open; Predict's own stamps of the offer's coming and
# going, and of when the amount on offer first went down (someone began to take it)
WATCH = {"best": None, "offer_max": 0.0, "offer_ms": 0, "listed_ms": 0, "open_ms": 0, "offer_ts": 0, "gone_ms": 0, "gone_ts": 0,
         "cut_ms": 0, "cut_ts": 0}


def watch_stats(recs: dict[str, dict], rule: str, mode: str, since_ms: int) -> dict:
    """What the watches of a rule's windows saw since ``since_ms``: the windows watched, those that offered the rule's side at its
    price, the shares offered at once on average there, the median lowest offer, the median lead of matching's opening over the
    start (seconds), the makers' lead, and the life of the offers lost and won."""
    seen = [r["seen"] for r in recs.values() if r.get("rule") == rule and r.get("mode") == mode and isinstance(r.get("seen"), dict)
            and int(r["seen"].get("start_ms") or 0) >= since_ms and not r["seen"].get("open")]
    offers = [x for x in seen if float(x.get("offer_max") or 0) > 0]
    mid = lambda xs: statistics.median(xs) if xs else None
    lost = [x for x in offers if x.get("gone_ms") and float(x.get("bought") or 0) <= DUST]
    won = [x for x in offers if x.get("gone_ts") and x.get("offer_ts") and float(x.get("bought") or 0) > DUST]
    stood = lambda x: (int(x["gone_ts"]) - int(x["offer_ts"])) if x.get("gone_ts") and x.get("offer_ts") else int(x["gone_ms"]) - int(x["offer_ms"])
    return {"watched": len(seen), "offered": len(offers),
            "offer_avg": round(sum(float(x["offer_max"]) for x in offers) / len(offers), 2) if offers else None,
            "best_mid": mid([float(x["best"]) for x in seen if x.get("best") is not None]),
            "open_lead": mid([(int(x["start_ms"]) - int(x["open_ms"])) / 1000 for x in seen if x.get("open_ms")]),
            "offer_lead": mid([(int(x["start_ms"]) - int(x["offer_ms"])) / 60_000 for x in offers if x.get("offer_ms")]),
            "lost": len(lost), "life": mid([max(0, stood(x)) for x in lost]),
            "won": len(won), "won_life": mid([max(0, stood(x)) for x in won])}


def races(recs: dict[str, dict], mode: str, since_ms: int) -> list[dict]:
    """The windows' races since ``since_ms``, the latest start first: when the offer showed (Predict's stamp, else the look's
    time) and how long before the start, the most on offer, how long until it was first taken from and until none was left,
    what this taker asked for and bought, and each order against the clock."""
    rows = []
    for r in recs.values():
        if r.get("mode") != mode or int(r.get("start_ms") or 0) < since_ms:
            continue
        seen = r["seen"] if isinstance(r.get("seen"), dict) else {}
        orders = [o for o in r.get("orders") or [] if not o.get("live")]
        if not seen.get("offer_ms") and not orders:
            continue
        shown = int(seen.get("offer_ts") or 0)

        def gap(kind: str) -> int | None:
            if not seen.get(kind + "_ms"):
                return None
            if seen.get(kind + "_ts") and shown:
                return max(0, int(seen[kind + "_ts"]) - shown)
            return max(0, int(seen[kind + "_ms"]) - int(seen["offer_ms"]))
        rule = RULE_OF.get(r.get("rule"))
        rows.append({
            "slug": r.get("slug"), "rule": r.get("rule"), "name": rule.name if rule else str(r.get("rule")), "window": r.get("name"),
            "start_ms": int(r.get("start_ms") or 0), "cents": int(seen.get("cents") or r.get("cents") or 0),
            "offer_at": shown or int(seen.get("offer_ms") or 0), "stamped": bool(shown),
            "lead_min": round((int(r.get("start_ms") or 0) - int(seen["offer_ms"])) / 60_000, 1) if seen.get("offer_ms") else None,
            "peak": float(seen.get("offer_max") or 0), "cut_ms": gap("cut"), "gone_ms": gap("gone"), "open": bool(seen.get("open")),
            "asked": round(sum(float(o.get("shares") or 0) for o in orders if o.get("state") not in NEVER_PLACED), 4),
            "bought": round(sum(float(o.get("filled") or 0) for o in orders), 4),
            "orders": [{"shares": float(o.get("shares") or 0), "filled": float(o.get("filled") or 0), "paper": bool(o.get("paper")),
                        "seen": o.get("seen"), "fresh": o.get("fresh"), "state": o.get("state"),
                        "error": str(o.get("error") or "") if o.get("state") in NEVER_PLACED else "",
                        "after": int(o["sent_wall"]) - shown if o.get("sent_wall") and shown else None,
                        "lag": int(o["seen_wall"]) - int(o["book_ts"]) if o.get("book_ts") and o.get("seen_wall") else None,
                        "own": int(o["sent_wall"]) - int(o["seen_wall"]) if o.get("sent_wall") and o.get("seen_wall") else None,
                        "wake": int(o["look_wall"]) - int(o["seen_wall"]) if o.get("look_wall") and o.get("seen_wall") and int(o["look_wall"]) >= int(o["seen_wall"]) else None,
                        "prep": int(o["signed_wall"]) - max(int(o["look_wall"]), int(o.get("seen_wall") or 0)) if o.get("signed_wall") and o.get("look_wall") else None,
                        "out": int(o["sent_wall"]) - int(o["signed_wall"]) if o.get("sent_wall") and o.get("signed_wall") else None,
                        "presigned": bool(o.get("presigned")),
                        "open_wait": o.get("open_wait"), "open_reads": o.get("open_reads"),
                        "trip": int(o["answered_wall"]) - int(o["sent_wall"]) if o.get("answered_wall") and o.get("sent_wall") else None}
                       for o in orders]})
    return sorted(rows, key=lambda x: -x["start_ms"])


def race_text(row: dict) -> str:
    sec = lambda ms: f"{ms / 1000:.2f} 秒"
    text = f"{bj(row['start_ms'])} {row['name']}："
    if row["offer_at"]:
        text += (f"≤{row['cents']}¢ 的卖单 {bj_clock(row['offer_at'])} 出现" + (f"（开场前 {row['lead_min']:g} 分钟）" if row["lead_min"] is not None else "")
                 + f"，最多 {row['peak']:g} 份；"
                 + ("、".join(([f"{sec(row['cut_ms'])}后开始被吃"] if row["cut_ms"] is not None else [])
                             + ([f"{sec(row['gone_ms'])}后被吃光"] if row["gone_ms"] is not None else ["还没吃光" if row["open"] else "到开场前没吃光"] if row["cut_ms"] is not None else []))
                    or ("还没有被吃" if row["open"] else "到开场前没人吃")))
    else:
        text += "卖单出现的时刻没有记到（重启前的，或没看到规则价以内的卖单）"
    orders = row["orders"]
    if not orders:
        return text + "；没下单" + ("（这一场还在看）" if row["open"] else "")
    text += "；" + ("只记录 " if all(o["paper"] for o in orders) else "") + (
        f"买到 {row['bought']:g}/{row['asked']:g} 份" if row["bought"] > DUST else f"下了 {row['asked']:g} 份，没买到" if row["asked"] > DUST else "单没下成")
    for i, o in enumerate(orders, 1):
        if o["paper"]:
            continue
        if o.get("state") in NEVER_PLACED:
            text += f"｜第 {i} 张 要 {o['shares']:g}：没下成" + (f"（{o['error']}）" if o.get("error") else "")
            continue
        parts = ([f"出现后 {o['after']} ms 发出"] if o["after"] is not None else []) + ([f"推送 {o['lag']} ms"] if o["lag"] is not None else [])
        if o["own"] is not None:
            split = [f"{name} {o[key]} ms" for name, key in (("到达→开始处理", "wake"), ("处理→签好", "prep"), ("签好→发出", "out")) if o.get(key) is not None]
            if o.get("presigned"):
                split.append("预先签好")
            if o.get("open_wait") is not None:
                split.append(f"卖单先到、等撮合开启 {o['open_wait']} ms（读了 {o.get('open_reads') or 0} 次状态）")
            parts.append(f"到达→发出 {o['own']} ms" + (f"［{'、'.join(split)}］" if split else "") + ("（不是盘口一到就下的）" if o["fresh"] is False else ""))
        if o["trip"] is not None:
            parts.append(f"往返 {o['trip']} ms")
        text += (f"｜第 {i} 张 要 {o['shares']:g} 成交 {o['filled']:g}" + (f"（当时盘口上 {o['seen']:g} 份）" if o["seen"] else "")
                 + ("：" + "、".join(parts) if parts else ""))
    return text


def race_summary(rows: list[dict]) -> str:
    mid = statistics.median
    marks = [(r["offer_at"] % 900_000) / 1000 for r in rows if r.get("stamped")]
    cuts = [r["cut_ms"] / 1000 for r in rows if r.get("cut_ms") is not None and r.get("stamped")]
    ours = [o["after"] for r in rows for o in r["orders"] if o.get("after") is not None and o.get("fresh") is not False]
    parts = []
    if marks:
        parts.append(f"卖单出现在整刻钟后 中位 {mid(marks):.2f} 秒（{min(marks):.2f}～{max(marks):.2f}，{len(marks)} 场）")
    if cuts:
        parts.append(f"最快的人在出现后 中位 {mid(cuts):.2f} 秒下手（最快 {min(cuts):.2f}）")
    if ours:
        parts.append(f"我们的单在出现后 中位 {mid(ours):.0f} ms 发出（最快 {min(ours):.0f}），再加半个往返才到")
    return "；".join(parts)


def course_part(rec: dict) -> tuple[float, float, float | None]:
    """A record's fills bought in the window's course (盘中): (shares, what they cost, their P&L — None until settled)."""
    orders = [o for o in rec.get("orders") or [] if o.get("live")]
    shares = sum(float(o.get("filled") or 0) for o in orders)
    spent = sum(float(o.get("filled") or 0) * order_avg(o) for o in orders)
    up = {"up": 1.0, "down": 0.0, "split": 0.5}.get(str(rec.get("result") or ""))
    if rec.get("pnl") is None or up is None:
        return shares, spent, None
    kept = sum(float(o.get("filled") or 0) * (1 - fee_share(order_avg(o), order_bps(o))) for o in orders)
    return shares, spent, kept * (up if rec.get("side") == "up" else 1 - up) - spent


def course_stats(recs: dict[str, dict], rule: str, mode: str, since_ms: int = 0) -> dict:
    rows = [course_part(r) for r in recs.values() if r.get("rule") == rule and r.get("mode") == mode and int(r.get("start_ms") or 0) >= since_ms]
    rows = [x for x in rows if x[0] > DUST]
    done = [x[2] for x in rows if x[2] is not None]
    return {"windows": len(rows), "shares": round(sum(x[0] for x in rows), 4), "cost": round(sum(x[1] for x in rows), 4), "settled": len(done),
            "wins": sum(1 for x in done if x > DUST), "losses": sum(1 for x in done if x < -DUST), "pnl": round(sum(done), 4)}


def speed_stats(recs: dict[str, dict], since_ms: int) -> dict | None:
    """The real orders against the clock since ``since_ms``, None for none: how many went out and how many filled anything; in
    ms the medians (and the fastest) of the push's lag (Predict's stamp on the book the order was sized on to its arrival), the
    taker's own time (that arrival to the request leaving) over the orders a book that had just come brought (``fresh``), and
    the request's round trip."""
    rows = [o for r in recs.values() if r.get("mode") == "on" and int(r.get("start_ms") or 0) >= since_ms
            for o in r.get("orders") or [] if o.get("sent_wall")]
    if not rows:
        return None
    marked = any("fresh" in o for o in rows)
    quick = [o for o in rows if o.get("fresh")] if marked else rows
    mid = lambda xs: statistics.median(xs) if xs else None
    lag = [int(o["seen_wall"]) - int(o["book_ts"]) for o in rows if o.get("book_ts") and o.get("seen_wall")]
    own = [int(o["sent_wall"]) - int(o["seen_wall"]) for o in quick if o.get("seen_wall")]
    wake = [int(o["look_wall"]) - int(o["seen_wall"]) for o in quick if o.get("seen_wall") and o.get("look_wall")]
    trip = [int(o["answered_wall"]) - int(o["sent_wall"]) for o in rows if o.get("answered_wall")]
    return {"orders": len(rows), "filled": sum(1 for o in rows if float(o.get("filled") or 0) > DUST),
            "lag": mid(lag), "own": mid(own), "trip": mid(trip), "wake": mid(wake), "timed": len(own),
            "best": {"lag": min(lag, default=None), "own": min(own, default=None), "trip": min(trip, default=None)}}


def course_speed(recs: dict[str, dict], since_ms: int) -> dict | None:
    took = [int(o["sent_wall"]) - int(o["index_wall"]) for r in recs.values() if r.get("mode") == "on" and int(r.get("start_ms") or 0) >= since_ms
            for o in r.get("orders") or [] if o.get("live") and o.get("index_wall") and o.get("sent_wall")]
    return {"orders": len(took), "own": statistics.median(took), "best": min(took)} if took else None


def since_days(now_ms: int, days: int) -> int:
    return now_ms - days * DAY_MS
