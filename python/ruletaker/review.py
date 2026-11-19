"""The review page's numbers (复盘): a port of huice rules.review — each rule's totals, the real and the paper apart, what the
watches saw, what was bought in the windows' course, the races, the orders against the clock, the last windows — and, new
here, the result day by day with its running total, and the records as CSV."""
from __future__ import annotations

import csv
import io
from typing import Any

from .rules import (MARKET_NAMES, RULE_OF, RULES, avg_price, cost, course_part, course_stats, filled, live_price_of, live_rule,
                    races, speed_stats, watch_stats)
from .clock import day_spans_text
from .util import DAY_MS, DUST, bj, day_of

RANGES = {"7": 7, "30": 30, "90": 90, "all": 0}


def since_of(now_ms: int, days: int) -> int:
    return now_ms - days * DAY_MS if days else 0


def review(recs: dict[str, dict], settings: Any, now_ms: int, days: int = 30) -> dict:
    """Everything the review page shows for the last ``days`` days (0 = all), the windows counted by their start."""
    since = since_of(now_ms, days)
    mine = {k: r for k, r in recs.items() if int(r.get("start_ms") or 0) >= since}
    rows = []
    for rule in RULES:
        for mode in ("on", "paper"):
            bought = [r for r in mine.values() if r.get("rule") == rule.key and r.get("mode") == mode and filled(r) > DUST]
            stats = watch_stats(mine, rule.key, mode, since)
            if not bought and not stats["watched"] and (mode == "paper" or not (live_rule(settings, rule) or live_price_of(settings, rule))):
                continue
            done = [r for r in bought if r.get("pnl") is not None]
            rows.append({"rule": rule.key, "name": rule.name, "side": rule.side, "market": rule.market,
                         "cents": settings.price(rule.key) if live_rule(settings, rule) else 0, "when": rule.when,
                         "live_cents": live_price_of(settings, rule), "course": course_stats(mine, rule.key, mode, since),
                         "mode": mode, "windows": len(bought), "shares": round(sum(filled(r) for r in bought), 4),
                         "cost": round(sum(cost(r) for r in bought), 4), "settled": len(done),
                         "wins": sum(1 for r in done if float(r["pnl"]) > DUST), "losses": sum(1 for r in done if float(r["pnl"]) < -DUST),
                         "pnl": round(sum(float(r["pnl"]) for r in done), 4), "open": len(bought) - len(done), "watch": stats})
    last = sorted((r for r in mine.values() if filled(r) > DUST), key=lambda r: -int(r.get("start_ms") or 0))[:100]
    return {"mode": settings.mode, "days": days, "since_ms": since,
            "markets": {m: {"on": settings.on(m), "shares": settings.shares_of(m), "name": n} for m, n in MARKET_NAMES.items()},
            "max_day_usd": settings.max_day_usd, "fast": day_spans_text(getattr(settings, "fast", ())),
            "lead_minutes": int(getattr(settings, "lead_minutes", 0) or 0),
            "totals": {mode: totals(mine, mode) for mode in ("on", "paper")},
            "rules": rows, "speed": speed_stats(mine, since),
            "races": races(mine, "on", since)[:80] + [r for r in races(mine, "paper", since)[:40]],
            "daily": {mode: daily(mine, mode) for mode in ("on", "paper")},
            "recent": [{"slug": r.get("slug"), "rule": r.get("rule"), "name": (RULE_OF.get(r.get("rule")) or RULES[0]).name,
                        "window": r.get("name"), "start_ms": r.get("start_ms"), "side": r.get("side"), "mode": r.get("mode"),
                        "shares": round(filled(r), 4), "avg": round(avg_price(r), 4), "cost": round(cost(r), 4),
                        "live": round(course_part(r)[0], 4), "result": r.get("result") or "", "pnl": r.get("pnl")} for r in last]}


def totals(recs: dict[str, dict], mode: str) -> dict:
    """One mode's windows bought, shares, cost, settled P&L, wins and losses, and the windows still to settle."""
    bought = [r for r in recs.values() if r.get("mode") == mode and filled(r) > DUST]
    done = [r for r in bought if r.get("pnl") is not None]
    spent = sum(cost(r) for r in done)
    pnl = sum(float(r["pnl"]) for r in done)
    return {"windows": len(bought), "shares": round(sum(filled(r) for r in bought), 4), "cost": round(sum(cost(r) for r in bought), 4),
            "settled": len(done), "wins": sum(1 for r in done if float(r["pnl"]) > DUST), "losses": sum(1 for r in done if float(r["pnl"]) < -DUST),
            "pnl": round(pnl, 4), "roi": round(pnl / spent, 4) if spent > DUST else None, "open": len(bought) - len(done)}


def daily(recs: dict[str, dict], mode: str) -> list[dict]:
    """The settled windows day by day (the Beijing date of their start), the oldest first, with the running total."""
    days: dict[str, dict] = {}
    for r in recs.values():
        if r.get("mode") != mode or filled(r) <= DUST or r.get("pnl") is None:
            continue
        day = day_of(int(r.get("start_ms") or 0))
        d = days.setdefault(day, {"day": day, "windows": 0, "wins": 0, "losses": 0, "shares": 0.0, "cost": 0.0, "pnl": 0.0})
        d["windows"] += 1
        d["wins"] += float(r["pnl"]) > DUST
        d["losses"] += float(r["pnl"]) < -DUST
        d["shares"] += filled(r)
        d["cost"] += cost(r)
        d["pnl"] += float(r["pnl"])
    out, run = [], 0.0
    for day in sorted(days):
        d = days[day]
        run += d["pnl"]
        out.append({**d, "shares": round(d["shares"], 4), "cost": round(d["cost"], 4), "pnl": round(d["pnl"], 4), "cum": round(run, 4)})
    return out


def records_csv(recs: dict[str, dict]) -> str:
    """Every order of every window, one line each (the paper ones too), for a spreadsheet."""
    out = io.StringIO()
    w = csv.writer(out)
    w.writerow(["开场（北京）", "场次", "规则", "模式", "买", "订单哈希", "状态", "限价", "份数", "成交", "均价", "手续费 bps", "盘中",
                "下单时刻（北京）", "推送延迟 ms", "到达→发出 ms", "往返 ms", "结果", "这一场盈亏"])
    for r in sorted(recs.values(), key=lambda r: int(r.get("start_ms") or 0)):
        rule = RULE_OF.get(r.get("rule"))
        for o in r.get("orders") or []:
            lag = int(o["seen_wall"]) - int(o["book_ts"]) if o.get("book_ts") and o.get("seen_wall") else ""
            own = int(o["sent_wall"]) - int(o["seen_wall"]) if o.get("sent_wall") and o.get("seen_wall") else ""
            trip = int(o["answered_wall"]) - int(o["sent_wall"]) if o.get("answered_wall") and o.get("sent_wall") else ""
            w.writerow([bj(int(r.get("start_ms") or 0)), r.get("name") or r.get("slug"), rule.name if rule else r.get("rule"),
                        "真实" if r.get("mode") == "on" else "只记录", "涨" if r.get("side") == "up" else "跌", o.get("hash") or "",
                        o.get("state") or "", o.get("limit") or "", o.get("shares") or "", o.get("filled") or 0, o.get("avg") or "",
                        o.get("fee_bps") if o.get("fee_bps") is not None else "", "是" if o.get("live") else "",
                        bj(int(o.get("at") or 0)) if o.get("at") else "", lag, own, trip, r.get("result") or "",
                        r.get("pnl") if r.get("pnl") is not None else ""])
    return out.getvalue()
