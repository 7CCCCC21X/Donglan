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


class FirstShowingTest(unittest.TestCase):
    def test_only_an_order_on_the_offer_s_first_showing_is_timed_against_the_fastest_hand(self) -> None:
        async def go() -> list[dict]:
            taker, api, _chain, stream, clock = make_taker(START - 3_600_000)
            await found(taker, api)
            v0 = int(time.time() * 1000)
            # the offer first shows on a book Predict stamped 5 s ago: the order goes out on that very book
            stream.take(MID, {**book(asks=[("0.55", "100")]), "version": v0, "updateTimestampMs": v0 - 5_000})
            await asyncio.sleep(0)
            await asyncio.gather(*taker.sends)
            clock.ms += T.FILL_READ_MS
            await taker.follow()
            clock.ms += 1_000
            await taker.follow()  # (it rested on the book: withdrawn)
            self.assertTrue(taker.record(SLUG, "on")["orders"][0].get("removed"))
            clock.ms += T.LATE_FILL_MS + 1_000
            await taker.follow()  # (no late fill: its shares free again)
            stream.take(MID, {**book(), "version": v0 + 1})
            stream.take(MID, {**book(asks=[("0.55", "100")]), "version": v0 + 2})  # back on a later book: a second order
            await asyncio.sleep(0)
            await asyncio.gather(*taker.sends)
            return rules.races(taker.recs, "on", 0)

        rows = run(go())
        orders = rows[0]["orders"]
        self.assertEqual(len(orders), 2)
        self.assertEqual((orders[0]["first"], orders[1]["first"]), (True, False))
        a0, a1 = orders[0]["after"], orders[1]["after"]
        self.assertTrue(a0 >= 5_000 and a1 >= a0, (a0, a1))
        text = rules.race_text(rows[0])
        self.assertIn(f"出现后 {a0} ms 发出", text)
        self.assertIn(f"首次出现 {rules.ago(a1)}后才发出（跟的是后来的盘口）", text)  # (word for word as the Rust version says it)
        second = text.split("｜第 2 张")[1]
        self.assertTrue("出现后" not in second and "首次出现" in second, text)
        # (the second order's hours-or-seconds wait kept out of the comparison: only the first is timed)
        self.assertIn(f"我们的单在出现后 中位 {a0} ms 发出（最快 {a0}）", rules.race_summary(rows))

    def test_a_stretch_of_time_reads_in_its_own_unit(self) -> None:
        for ms, said in ((370, "0.37 秒"), (59_994, "59.99 秒"), (60_000, "1 分钟"), (1_322_675, "22 分钟"), (3_600_000, "1 小时"),
                         (26_236_610, "7.3 小时"), (68_404_200, "19 小时")):
            self.assertEqual(rules.ago(ms), said)


class KeyBudgetTest(unittest.TestCase):
    def test_another_program_on_the_same_key_is_seen_in_predict_s_figures(self) -> None:
        self.assertEqual(nethttp.budget_of(SHARED), (5000, 1236))
        self.assertEqual(nethttp.budget_of("x-ratelimit-limit: 240; x-ratelimit-remaining: 200"), (240, 200))
        self.assertIsNone(nethttp.budget_of("ratelimit-limit: 5000"))
        self.assertIsNone(nethttp.budget_of(""))
        warn, line = T.key_budget_line(SHARED, 120)
        self.assertTrue(warn, line)
        self.assertIn("用了 3764 次（上限 5000），我们这一分钟发了 120 次", line)
        # (word for word as the Rust version says it — both cases huice's: its main key the same one, or its RULE_API_KEY lent out)
        self.assertEqual(line, "API Key 额度：Predict 记这一分钟用了 3764 次（上限 5000），我们这一分钟发了 120 次：另外约 3644 次是别的程序在用同一把 Key（多半是 Railway 上的 huice：它的主 Key 就是这把的话，这边换一把别的程序不用的——先 rt.sh set PREDICT_RATE_PER_MIN 220，再 rt.sh set RULE_API_KEY；是它的 RULE_API_KEY 借用这把的话，删掉那边的 RULE_API_KEY，或那边设 RULE_KEY_LEND=off）——一撞上限流，这里所有请求停 20 秒以上，赶上抢单时刻就下不了单")
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


class SpeedCommandTest(unittest.TestCase):
    def test_speed_test_keeps_out_of_the_races_and_runs_one_at_a_time(self) -> None:
        async def go() -> list:
            out = []
            # a per-second stretch all day: never measured in it
            taker, *_ = make_taker(START - 10 * 3_600_000, RULE_FAST_HOURS="00:00-23:59")
            out.append(await taker.speed_test(""))
            out.append(taker.speeding)
            # clear of the races: a rate that is not one is refused before anything is measured
            taker, *_ = make_taker(START - 10 * 3_600_000)
            for bad in ("abc", "0", "9000"):
                out.append(await taker.speed_test(bad))
            # one at a time: asked while one is under way, told so (nothing measured)
            taker.speeding = True
            out.append(await taker.speed_test("4000"))
            out.append(await taker.command("/help", ""))
            return out
        busy, speeding, *bad, again, help_text = run(go())
        self.assertTrue("正在抢单时段" in busy and "/speed" in busy, busy)
        self.assertFalse(speeding)
        for said in bad:
            self.assertIn("用法：/speed", said)
        self.assertIn("测速正在进行", again)
        self.assertIn("/speed 测速", help_text)

    def test_the_menu_has_the_speed_test(self) -> None:
        from ruletaker.notify import Telegram
        _, keyboard = Telegram("", (), admin=42, commands=True, command_token="CMD").menu_card()
        buttons = [b["callback_data"] for row in keyboard["inline_keyboard"] for b in row]
        self.assertIn("c:/speed", buttons)
        self.assertTrue("c:/test" in buttons and "m:" in buttons)


class SpareKeyTest(unittest.TestCase):
    def test_a_429_on_the_key_in_use_sends_the_request_again_through_a_spare(self) -> None:
        from ruletaker.predict import PredictApi
        from ruletaker.util import RemoteError
        limited: set = {"k"}
        created: list = []

        def fake_request(url, payload=None, timeout=15, headers=None, gate=None, trace=None):
            if (headers or {}).get("x-api-key") in limited:
                raise RemoteError("HTTP 429: Rate limit exceeded", 30)  # (refused before anything is done)
            if payload is not None:
                created.append(payload)
            return b'{"success": true, "data": {}}'

        async def go() -> tuple:
            api = PredictApi("https://api.predict.fun/v1", "k", None, 0).with_spares(["B", " ", "k", "C"])
            saved = nethttp.request
            nethttp.request = fake_request
            try:
                out = [list(api.keys)]
                await api.call("GET", "/markets/1/orderbook")  # a read: answered through the first spare, no pause
                out += [api.active, api.cooling() <= 0, api.key_cooling(0) > 0, len(api.switches), len(api.limited)]
                limited.add("B")
                await api.call("POST", "/orders", b'{"data": {"order": 1}}', priority=True)  # an order: placed once, through C
                out += [len(created), api.active]
                limited.add("C")  # every key spent: the pause as before
                try:
                    await api.call("GET", "/markets/1/orderbook")
                    out.append("answered")
                except RemoteError:
                    out.append("refused")
                out.append(api.cooling() > 0)
                return out
            finally:
                nethttp.request = saved
        keys, active, free, cooling0, switches, limits, placed, active2, last, all_cooling = run(go())
        self.assertEqual(keys, ["k", "B", "C"])
        self.assertEqual((active, free, cooling0, switches, limits), (1, True, True, 1, 1))
        self.assertEqual((placed, active2), (1, 2))
        self.assertEqual((last, all_cooling), ("refused", True))

    def test_the_self_check_tries_each_spare_key(self) -> None:
        import collections
        from ruletaker.util import RemoteError

        async def go() -> str:
            taker, api, chain, stream, clock = make_taker(START - 3_600_000)
            await found(taker, api)
            api.keys, api.active, api.switches = ["k", "B", "WRONGKEY"], 0, collections.deque()
            api.key_cooling = lambda k: 0.0

            async def try_key(k: int, path: str) -> float:
                if api.keys[k] == "WRONGKEY":
                    raise RemoteError("HTTP 401: Invalid API key")
                return 12.0
            api.try_key = try_key
            return await taker.self_check()
        text = run(go())
        self.assertIn("API Key：主用 + 备用 2 把，现在用第 1 把｜第 2 把（…B）可用 12 ms", text)
        self.assertIn("❌ 备用 API Key 被 Predict 拒绝：第 3 把（…GKEY）", text)
        self.assertNotIn("WRONGKEY", text)  # never the whole key

    def test_the_spare_keys_are_read_from_the_settings(self) -> None:
        from ruletaker.config import Config
        c = Config.from_env({"PREDICT_API_KEY": "k", "RULE_API_KEY_2": " B ", "RULE_API_KEY_3": "", "RULE_API_KEY_4": "k",
                             "RULE_API_KEY_5": "B", "RULE_API_KEY_9": "C"})
        self.assertEqual(c.api_spares, ("B", "C"))
        self.assertIn("spare_api_keys=2", c.secrets_line())
        self.assertNotIn("'B'", repr(c))


if __name__ == "__main__":
    unittest.main()
