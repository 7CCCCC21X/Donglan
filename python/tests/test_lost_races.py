"""Why a race was lost, when it is not speed: an order Predict cancelled itself on arrival (never withdrawn by us) — its other
side this account's own resting order (huice's pre-posting), cancelled as the taker by self-trade prevention — is named in
the race's line; and another program on the same API key (Predict's rate-limit headers against our own count of the minute)
and the last day's 429s are named in the self-check."""
from __future__ import annotations

import asyncio
import time
import unittest

from fakes import book, make_taker, run
from ruletaker import http as nethttp
from ruletaker import rules
from ruletaker import taker as T
from test_taker import MID, SLUG, START, found

SHARED = 'ratelimit-remaining: 1236; ratelimit-limit: 5000; ratelimit-reset: 15; ratelimit-policy: "rpm";q=5000;w=60'


class ClosedByPredictTest(unittest.TestCase):
    def test_an_order_predict_cancelled_itself_is_said_so_one_we_withdrew_is_not(self) -> None:
        async def go(stp: bool) -> tuple[dict, str, list]:
            taker, api, chain, stream, clock = make_taker(START - 3_600_000)
            await found(taker, api)
            api.stp_cancel = stp  # (the offer was this account's own: Predict cancels the taker)
            stream.take(MID, book(asks=[("0.55", "100")]))
            await asyncio.sleep(0)
            await asyncio.gather(*taker.sends)
            order = taker.record(SLUG, "on")["orders"][0]
            clock.ms += T.FILL_READ_MS
            await taker.follow()
            clock.ms += 1_000
            await taker.follow()
            rows = rules.races(taker.recs, "on", 0)
            return order, rules.race_text(rows[0]), list(api.removed)

        order, text, removed = run(go(True))
        self.assertEqual((order.get("closed_as"), float(order["filled"])), ("CANCELLED", 0.0))
        self.assertEqual(removed, [])  # nothing of ours to withdraw
        self.assertIn("Predict 当场就撤了这张单（它记的状态 CANCELLED），不是我们撤的——多半是对面正是这个账户自己的挂单（比如 huice 的预挂），自成交保护撤掉吃单的一方", text)  # (word for word as the Rust version says it)
        order, text, removed = run(go(False))  # a rest that waited on the book, withdrawn by us
        self.assertTrue(order.get("removed") and not order.get("closed_as"))
        self.assertNotIn("当场就撤了", text)

    def test_a_record_from_before_the_status_was_kept_is_read_the_same_way(self) -> None:
        def order(**extra) -> dict:
            return {"state": "final", "id": "id1", "shares": 30.0, "filled": 0.0, **extra}
        self.assertEqual(rules.closed_by_predict(order()), "?")
        self.assertEqual(rules.closed_by_predict(order(state="closing")), "?")
        self.assertEqual(rules.closed_by_predict(order(closed_as="CANCELLED")), "CANCELLED")
        for not_so in (order(removed=5), order(filled=30.0), order(state="open"), order(paper=True), order(id=""),
                       order(error="到期后仍显示挂着：按已知成交记")):
            self.assertIsNone(rules.closed_by_predict(not_so), not_so)


class KeyBudgetTest(unittest.TestCase):
    def test_another_program_on_the_same_key_is_seen_in_predict_s_figures(self) -> None:
        self.assertEqual(nethttp.budget_of(SHARED), (5000, 1236))
        self.assertEqual(nethttp.budget_of("x-ratelimit-limit: 240; x-ratelimit-remaining: 200"), (240, 200))
        self.assertIsNone(nethttp.budget_of("ratelimit-limit: 5000"))
        self.assertIsNone(nethttp.budget_of(""))
        warn, line = T.key_budget_line(SHARED, 120)
        self.assertTrue(warn, line)
        self.assertIn("用了 3764 次（上限 5000），我们这一分钟发了 120 次", line)
        self.assertTrue("另外约 3644 次是别的程序" in line and "RULE_KEY_LEND=off" in line, line)
        warn, line = T.key_budget_line("ratelimit-limit: 5000; ratelimit-remaining: 4880", 110)
        self.assertFalse(warn or "别的程序" in line, line)
        self.assertIsNone(T.key_budget_line("", 3))

    def test_self_check_counts_the_rate_limits_of_the_last_day(self) -> None:
        async def go() -> tuple[str, str]:
            taker, api, chain, stream, clock = make_taker(START - 3_600_000)
            await found(taker, api)
            before = await taker.self_check()
            api.limited = [time.monotonic() - 3_600, time.monotonic() - 90_000]  # (the second more than a day ago)
            return before, await taker.self_check()
        before, after = run(go())
        self.assertNotIn("限流（429）", before)
        self.assertIn("⚠️ 最近一天被 Predict 限流（429）1 次", after)


class PushTimingTest(unittest.TestCase):
    def test_the_extra_push_connections_are_timed_against_the_first(self) -> None:
        from fakes import QuietStream
        from ruletaker.stream import BookStream

        async def go() -> tuple:
            s = BookStream("", 3, connect=QuietStream.never)
            s.wanted = {"m1"}
            book = lambda v: {"version": v, "updateTimestampMs": int(time.time() * 1000), "bids": [], "asks": [["0.55", "10"]]}
            s.take("m1", book(1), 0)
            time.sleep(0.005)
            s.take("m1", book(1), 1)  # the same book, 5 ms on, through connection 1
            s.take("m1", book(1), 1)  # (again through it: counted once)
            s.take("m1", book(2), 2)  # a newer one, first through connection 2
            s.take("m1", book(2), 0)
            return s.hedge(), list(s.conns[1].behind), BookStream("", 1, connect=QuietStream.never).hedge()
        (firsts, behind, copies), late, single = run(go())
        self.assertEqual((firsts, copies), ([1, 0, 1], 2))
        self.assertTrue(behind is not None and behind >= 0)
        self.assertGreaterEqual(late[0], 4.0, late)
        self.assertIsNone(single)  # (one: nothing to compare)

    def test_rest_is_judged_against_the_push_only_from_reads_dense_enough(self) -> None:
        from ruletaker.__main__ import rest_verdict
        sparse = rest_verdict(1.09, [41.0] * 5, [13.0], 7, 34)  # every 1.09 s: the push comes first nearly always
        self.assertTrue("太稀" in sparse and "rt.sh latency 4000" in sparse, sparse)
        self.assertIn("REST 的盘口比推送新", rest_verdict(0.06, [20.0] * 5, [60.0], 0, 0))
        self.assertIn("REST 不比推送新", rest_verdict(0.06, [80.0] * 5, [20.0], 1, 10))
        self.assertIn("样本太少", rest_verdict(0.06, [], [20.0], 1, 1))


if __name__ == "__main__":
    unittest.main()
