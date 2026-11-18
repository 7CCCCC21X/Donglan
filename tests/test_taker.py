"""The rule taker end to end against stand-ins of Predict: a push brings an offer, the order goes out in the same turn, its fills
are read, its rest withdrawn, the window settled — and the guards (the cap, the pause, the loss stop, a refusal, a lost answer,
a restart, the settings changed on the order's way)."""
from __future__ import annotations

import asyncio
import datetime as dt
import json
import unittest
from decimal import Decimal as D

from fakes import FakeApi, book, make_taker, market_of, run
from ruletaker import taker as T
from ruletaker.predict import RemoteError
from ruletaker.rules import held

UTC = dt.timezone.utc
START = int(dt.datetime(2026, 11, 18, 22, 0, tzinfo=UTC).timestamp() * 1000)  # 17:00 EST = 06:00 Beijing: gold's first shut window
SLUG = f"xau-updown-15m-{START // 1000}"
MID = "77001"


def listed(api: FakeApi, mid: str = MID, slug: str = SLUG, start: int = START, status: str = "OPEN") -> None:
    api.categories[slug] = mid
    api.markets[mid] = market_of(mid, slug, start, status=status)


async def found(taker, api: FakeApi) -> dict:
    """The plan made and the window's market found (its first look)."""
    taker.plan(taker.now())
    taker.planned_ms = taker.now()
    listed(api)
    await taker.turn()
    w = taker.track[SLUG]
    assert w["market"] is not None, w["word"]
    return w


class OrderTest(unittest.TestCase):
    def test_push_places_the_whole_window_at_once(self) -> None:
        async def go() -> None:
            taker, api, chain, stream, clock = make_taker(START - 3_600_000)
            w = await found(taker, api)
            self.assertIn(MID, stream.wanted)
            self.assertEqual(api.created, [])
            # an offer of 100 Up at 55¢ (the rule's 58¢): bought in the push's own turn — the window's 50 shares, at 58¢
            stream.take(MID, book(asks=[("0.55", "100")]))
            await asyncio.sleep(0)
            self.assertEqual(len(api.created), 1)
            order = api.created[0]["data"]["order"]
            self.assertEqual(api.created[0]["data"]["strategy"], "LIMIT")
            self.assertEqual(api.created[0]["data"]["pricePerShare"], str(58 * 10 ** 16))
            self.assertEqual(int(order["takerAmount"]), 50 * 10 ** 18)
            self.assertEqual(int(order["makerAmount"]), 29 * 10 ** 18)
            self.assertEqual(order["tokenId"], "1111111111111111111111")
            self.assertEqual(int(order["expiration"]), START // 1000 - T.EXPIRY_BEFORE_S)
            await asyncio.gather(*taker.sends)
            rec = taker.record(SLUG, "on")
            self.assertEqual(rec["orders"][0]["state"], "open")
            self.assertEqual(rec["orders"][0]["hash"], order["hash"].lower())
            self.assertTrue(rec["orders"][0]["fresh"])
            self.assertIn("已下单", w["word"])
            # the signature is the wallet's over the order's hash
            from eth_account import Account
            signer = Account._recover_hash(bytes.fromhex(order["hash"][2:]), signature=bytes.fromhex(order["signature"][2:]))
            self.assertEqual(signer, taker.wallet.signer)
        run(go())

    def test_small_offer_is_bought_as_shown_and_a_second_order_beside_it(self) -> None:
        async def go() -> None:
            taker, api, chain, stream, clock = make_taker(START - 3_600_000)
            await found(taker, api)
            api.create_delay = 0.05  # the first order's answer still on its way...
            stream.take(MID, book(asks=[("0.57", "8")]))
            await asyncio.sleep(0)
            self.assertEqual(int(api.created[0]["data"]["order"]["takerAmount"]), 8 * 10 ** 18)
            stream.take(MID, book(asks=[("0.57", "8"), ("0.58", "30")]))  # ...when more is posted: ordered beside it
            await asyncio.sleep(0.01)
            self.assertEqual(len(api.created), 2)
            # 38 offered, 8 of them the first order's: 30 fresh — at least 20 and a quarter of the 42 the window may still buy: all 42
            self.assertEqual(int(api.created[1]["data"]["order"]["takerAmount"]), 42 * 10 ** 18)
            self.assertNotEqual(api.created[0]["data"]["order"]["salt"], api.created[1]["data"]["order"]["salt"])
            await asyncio.gather(*taker.sends)
            self.assertEqual(held(taker.record(SLUG, "on")), 50)
        run(go())

    def test_follow_reads_fills_withdraws_and_ends(self) -> None:
        async def go() -> None:
            taker, api, chain, stream, clock = make_taker(START - 3_600_000)
            await found(taker, api)
            api.fill_on_create = 30
            stream.take(MID, book(asks=[("0.55", "100")]))
            await asyncio.sleep(0)
            await asyncio.gather(*taker.sends)
            order = taker.record(SLUG, "on")["orders"][0]
            clock.ms += T.FILL_READ_MS
            await taker.follow()
            self.assertEqual(order["filled"], 30)
            self.assertEqual(order["state"], "cancelling")
            self.assertEqual(api.removed, ["id1"])
            clock.ms += 1_000
            await taker.follow()
            self.assertEqual(order["state"], "closing")  # off the book: final only a minute on (a late fill)
            clock.ms += T.LATE_FILL_MS
            await taker.follow()
            self.assertEqual(order["state"], "final")
            self.assertEqual(held(taker.record(SLUG, "on")), 30)
            # settled on Predict's result: Up won
            api.markets[MID]["outcomes"][0]["status"] = "WON"
            clock.ms = START + 15 * 60_000 + T.SETTLE_AFTER_MS
            await taker.settle()
            rec = taker.record(SLUG, "on")
            self.assertEqual(rec["result"], "up")
            fee = 0.02 * 0.42 / 0.58
            self.assertAlmostEqual(rec["pnl"], 30 * (1 - fee) - 30 * 0.58, places=6)
        run(go())

    def test_day_cap_counts_every_window(self) -> None:
        async def go() -> None:
            taker, api, chain, stream, clock = make_taker(START - 3_600_000, RULE_MAX_DAY_USD="40")
            await found(taker, api)
            stream.take(MID, book(asks=[("0.55", "100")]))
            await asyncio.sleep(0)
            await asyncio.gather(*taker.sends)
            # $40 a day at 58¢: 68 shares at the most — the first window takes 50 ($29)
            self.assertEqual(int(api.created[0]["data"]["order"]["takerAmount"]), 50 * 10 ** 18)
            second = START + 15 * 60_000
            slug2 = f"xau-updown-15m-{second // 1000}"
            listed(api, "77002", slug2, second)
            clock.ms += T.PROBE_SLOW_S * 1000 + 1  # (its listing looked for again)
            await taker.turn()
            stream.take("77002", book(asks=[("0.55", "100")]))
            await asyncio.sleep(0)
            await asyncio.gather(*taker.sends)
            self.assertEqual(int(api.created[1]["data"]["order"]["takerAmount"]), 18 * 10 ** 18)  # $11 left → 18 shares
        run(go())

    def test_balance_caps_the_order(self) -> None:
        async def go() -> None:
            taker, api, chain, stream, clock = make_taker(START - 3_600_000)
            taker.usdt = 10 * 10 ** 18  # $10: 17 shares at 58¢
            await found(taker, api)
            stream.take(MID, book(asks=[("0.55", "100")]))
            await asyncio.sleep(0)
            self.assertEqual(int(api.created[0]["data"]["order"]["takerAmount"]), 17 * 10 ** 18)
            await asyncio.gather(*taker.sends)
        run(go())

    def test_nothing_while_paused_or_stopped_for_the_day(self) -> None:
        async def go() -> None:
            taker, api, chain, stream, clock = make_taker(START - 3_600_000, RULE_MAX_DAILY_LOSS_USD="20")
            await found(taker, api)
            await taker.control("pause", "test")
            stream.take(MID, book(asks=[("0.55", "100")]))
            await asyncio.sleep(0)
            self.assertEqual(api.created, [])
            self.assertIn("暂停", taker.track[SLUG]["word"])
            await taker.control("resume", "")
            # a settled loss of $25 today: the loss stop holds every new order
            day = taker.now()
            taker.recs["old"] = {"slug": "old", "mode": "on", "rule": "gold_shut", "side": "up", "pnl": -25.0, "orders": [],
                                 "settled_day": __import__("ruletaker.util", fromlist=["day_of"]).day_of(day), "start_ms": day}
            stream.take(MID, book(asks=[("0.54", "100")]))
            await asyncio.sleep(0)
            self.assertEqual(api.created, [])
            self.assertIn("停机线", taker.block(0.0))
            # /resume moves the line: from the P&L now, another $20
            reply = await taker.control("resume", "")
            self.assertIn("停机线移到", reply)
            self.assertEqual(taker.block(0.0), "")
        run(go())

    def test_refusal_pauses_the_window_and_counts(self) -> None:
        async def go() -> None:
            taker, api, chain, stream, clock = make_taker(START - 3_600_000)
            await found(taker, api)
            api.create_error = RemoteError("HTTP 400: order value too small")
            stream.take(MID, book(asks=[("0.55", "100")]))
            await asyncio.sleep(0)
            await asyncio.gather(*taker.sends)
            order = taker.record(SLUG, "on")["orders"][0]
            self.assertEqual(order["state"], "rejected")
            self.assertGreater(taker.track[SLUG]["look_ms"], clock.ms)  # not again for half a minute
            stream.take(MID, book(asks=[("0.54", "100")]))
            await asyncio.sleep(0)
            self.assertEqual(len(api.created), 1)
        run(go())

    def test_lost_answer_is_found_by_hash_never_sent_again(self) -> None:
        async def go() -> None:
            taker, api, chain, stream, clock = make_taker(START - 3_600_000)
            await found(taker, api)
            api.create_error = RemoteError("网络错误 (TimeoutError)")
            stream.take(MID, book(asks=[("0.55", "100")]))
            await asyncio.sleep(0)
            await asyncio.gather(*taker.sends)
            order = taker.record(SLUG, "on")["orders"][0]
            self.assertEqual(order["state"], "unknown")
            # the window holds: its 50 shares are counted whole while the order may exist
            stream.take(MID, book(asks=[("0.54", "100")]))
            await asyncio.sleep(0)
            self.assertEqual(len(api.created), 1)
            # Predict did take it (the answer was lost on the way): found by its hash
            api.orders[order["hash"]] = {"id": "x9", "status": "FILLED", "amount": str(50 * 10 ** 18), "amountFilled": str(50 * 10 ** 18)}
            clock.ms += T.UNKNOWN_EVERY_MS
            await taker.follow()
            self.assertEqual(order["id"], "x9")
            self.assertEqual(order["state"], "final")
            self.assertEqual(order["filled"], 50)
        run(go())

    def test_conflict_not_found_frees_the_window(self) -> None:
        async def go() -> None:
            taker, api, chain, stream, clock = make_taker(START - 3_600_000)
            await found(taker, api)
            api.create_error = RemoteError("HTTP 409: Conflict")
            stream.take(MID, book(asks=[("0.55", "100")]))
            await asyncio.sleep(0)
            await asyncio.gather(*taker.sends)
            order = taker.record(SLUG, "on")["orders"][0]
            self.assertEqual((order["state"], order.get("conflict")), ("unknown", True))
            api.orders.pop(order["hash"], None)
            for _ in range(T.CONFLICT_MISSES + 1):
                clock.ms += T.CONFLICT_GRACE_MS // T.CONFLICT_MISSES + 1
                order["next_ms"] = 0
                await taker.follow()
            self.assertEqual(order["state"], "not_found")
            self.assertEqual(held(taker.record(SLUG, "on")), 0)
        run(go())

    def test_settings_changed_on_the_way_hold_the_order(self) -> None:
        async def go() -> None:
            taker, api, chain, stream, clock = make_taker(START - 3_600_000)
            await found(taker, api)
            original = api.create_order

            async def changed_first(body, digest):
                taker.configure(taker.settings.with_mode("paper"))  # switched while the request waited for its slot
                return await original(body, digest)
            api.create_order = changed_first
            stream.take(MID, book(asks=[("0.55", "100")]))
            await asyncio.sleep(0)
            await asyncio.gather(*taker.sends)
            order = taker.record(SLUG, "on")["orders"][0]
            self.assertEqual(order["state"], "not_sent")
            self.assertEqual(api.created, [])
        run(go())

    def test_matching_not_open_waits_then_buys_the_moment_it_opens(self) -> None:
        async def go() -> None:
            taker, api, chain, stream, clock = make_taker(START - 3_600_000)
            taker.plan(taker.now())
            taker.planned_ms = taker.now()
            listed(api, status="MATCHING_NOT_ENABLED")
            await taker.turn()
            stream.take(MID, book(asks=[("0.55", "100")]))
            await asyncio.sleep(0)
            self.assertEqual(api.created, [])
            self.assertIn("等撮合开启", taker.track[SLUG]["word"])
            api.markets[MID]["tradingStatus"] = "OPEN"
            await asyncio.sleep(0.4)  # the rush's next read of the status sees it open, and the look buys
            self.assertEqual(len(api.created), 1)
            order = taker.record(SLUG, "on")["orders"][0]
            self.assertIn("open_wait", order)
            await asyncio.gather(*taker.sends)
        run(go())

    def test_presigned_order_is_the_one_sent(self) -> None:
        async def go() -> None:
            taker, api, chain, stream, clock = make_taker(START - 3_600_000, RULE_FAST_HOURS="00:00-24:00")
            await found(taker, api)
            taker.pace(taker.now())
            self.assertTrue(taker.fast)
            taker.presign(taker.now())
            self.assertEqual(taker.presigned, 1)
            stream.take(MID, book(asks=[("0.55", "100")]))
            await asyncio.sleep(0)
            await asyncio.gather(*taker.sends)
            self.assertEqual(taker.presign_hits, 1)
            self.assertTrue(taker.record(SLUG, "on")["orders"][0]["presigned"])
        run(go())

    def test_journal_recovers_an_order_the_store_never_got(self) -> None:
        async def go() -> None:
            taker, api, chain, stream, clock = make_taker(START - 3_600_000, journal=True)
            await found(taker, api)
            stream.take(MID, book(asks=[("0.55", "100")]))
            await asyncio.sleep(0)
            await asyncio.gather(*taker.sends)
            order = taker.record(SLUG, "on")["orders"][0]
            path = taker.journal_path()
            with open(path, encoding="utf-8") as f:
                lines = [json.loads(line) for line in f if line.strip()]
            self.assertEqual(lines[0]["order"]["hash"], order["hash"])
            # the store lost the record (a crash before its writer got to it): the journal brings the order back as unknown
            from ruletaker.store import Store
            from ruletaker.taker import RuleTaker
            taker.store.flush()
            store = Store(taker.store.path)
            store.delete_keys([k for k, _ in list(store.items("rule:"))])
            store.flush()
            again = RuleTaker(taker.config, store, api=api, chain=chain, stream=stream, clock=clock)
            rec = again.record(SLUG, "on")
            self.assertIsNotNone(rec)
            self.assertEqual(rec["orders"][0]["hash"], order["hash"])
            self.assertEqual(rec["orders"][0]["state"], "unknown")
        run(go())

    def test_paper_records_once_and_sends_nothing(self) -> None:
        async def go() -> None:
            taker, api, chain, stream, clock = make_taker(START - 3_600_000, RULE_TAKER="paper")
            await found(taker, api)
            stream.take(MID, book(asks=[("0.55", "20"), ("0.57", "40")]))
            await asyncio.sleep(0)
            stream.take(MID, book(asks=[("0.55", "25"), ("0.57", "40")]))
            await asyncio.sleep(0)
            self.assertEqual(api.created, [])
            rec = taker.record(SLUG, "paper")
            self.assertEqual(len(rec["orders"]), 1)
            self.assertEqual(rec["orders"][0]["filled"], 50)
            self.assertAlmostEqual(rec["orders"][0]["avg"], (0.55 * 20 + 0.57 * 30) / 50)
        run(go())

    def test_nothing_in_the_last_seconds_before_the_start(self) -> None:
        async def go() -> None:
            taker, api, chain, stream, clock = make_taker(START - 3_600_000)
            await found(taker, api)
            clock.ms = START - T.STOP_MS + 1
            stream.take(MID, book(asks=[("0.55", "100")]))
            await asyncio.sleep(0)
            self.assertEqual(api.created, [])
        run(go())

    def test_withdraw_all_on_pause(self) -> None:
        async def go() -> None:
            taker, api, chain, stream, clock = make_taker(START - 3_600_000)
            await found(taker, api)
            stream.take(MID, book(asks=[("0.55", "100")]))
            await asyncio.sleep(0)
            await asyncio.gather(*taker.sends)
            reply = await taker.control("pause", "test")
            self.assertIn("已请求撤回", reply)
            self.assertEqual(api.removed, ["id1"])
        run(go())

    def test_status_text(self) -> None:
        async def go() -> None:
            taker, api, chain, stream, clock = make_taker(START - 3_600_000)
            await found(taker, api)
            stream.take(MID, book(asks=[("0.55", "100")]))
            await asyncio.sleep(0)
            await asyncio.gather(*taker.sends)
            text = taker.status()
            self.assertIn("规则吃单", text)
            self.assertIn("黄金休市 4 档", text)
            self.assertIn("抢单记录", text)
            self.assertIsInstance(taker.summary(), dict)
        run(go())


class InPlayTest(unittest.TestCase):
    def test_course_buys_only_with_the_index_ahead(self) -> None:
        from ruletaker.index import IndexFeed

        async def go() -> None:
            taker, api, chain, stream, clock = make_taker(START - 60_000, RULE_GOLD_SHUT_LIVE_CENTS="80")
            taker.index = feed = IndexFeed(["XAUUSDT"])
            feed.on_change = taker.on_index
            await found(taker, api)
            clock.ms = START + 90_000  # a minute and a half into the window
            taker.track[SLUG]["look_ms"] = 0
            feed.candles[("XAUUSDT", START - 60_000)] = D("4000")
            feed.push_index("XAUUSDT", D("4000.1"), clock.ms - 100, clock.ms)  # +0.0025%: not the 0.005% the rule asks
            stream.take(MID, book(asks=[("0.70", "100")]))
            await asyncio.sleep(0)
            self.assertEqual(api.created, [])
            self.assertIn("还没领先", taker.track[SLUG]["word"])
            feed.push_index("XAUUSDT", D("4000.4"), clock.ms - 50, clock.ms)  # +0.01%: ahead — the index's arrival is the look
            await asyncio.sleep(0)
            self.assertEqual(len(api.created), 1)
            order = api.created[0]["data"]["order"]
            self.assertEqual(api.created[0]["data"]["pricePerShare"], str(80 * 10 ** 16))
            self.assertEqual(int(order["expiration"]), (START + 15 * 60_000) // 1000 - T.EXPIRY_BEFORE_S)
            await asyncio.gather(*taker.sends)
            self.assertTrue(taker.record(SLUG, "on")["orders"][0]["live"])
        run(go())

    def test_first_minute_no_dearer_than_before(self) -> None:
        from ruletaker.index import IndexFeed

        async def go() -> None:
            taker, api, chain, stream, clock = make_taker(START - 60_000, RULE_GOLD_SHUT_LIVE_CENTS="80")
            taker.index = feed = IndexFeed(["XAUUSDT"])
            await found(taker, api)
            clock.ms = START + 20_000
            taker.track[SLUG]["look_ms"] = 0
            feed.candles[("XAUUSDT", START - 60_000)] = D("4000")
            feed.push_index("XAUUSDT", D("4001"), clock.ms - 100, clock.ms)
            stream.take(MID, book(asks=[("0.70", "100")]))
            await asyncio.sleep(0)
            self.assertEqual(api.created, [])  # 70¢ is above the 58¢ before the window: not in the first minute
            stream.take(MID, book(asks=[("0.57", "10")]))
            await asyncio.sleep(0)
            self.assertEqual(api.created[0]["data"]["pricePerShare"], str(58 * 10 ** 16))
            await asyncio.gather(*taker.sends)
        run(go())


if __name__ == "__main__":
    unittest.main()


class LoopTest(unittest.TestCase):
    def test_the_loop_runs_reads_fills_and_stops_cleanly(self) -> None:
        async def go() -> None:
            taker, api, chain, stream, clock = make_taker(START - 3_600_000)
            taker.running = False
            listed(api)
            api.fill_on_create = 50  # filled in full as it lands
            task = asyncio.create_task(taker.run())
            for _ in range(100):  # the plan, the listing found, the book subscribed
                await asyncio.sleep(0.01)
                if MID in stream.wanted:
                    break
            self.assertIn(MID, stream.wanted)
            stream.take(MID, book(asks=[("0.55", "100")]))
            await asyncio.sleep(0.01)
            self.assertEqual(len(api.created), 1)
            clock.ms += T.FILL_READ_MS + 10
            for _ in range(300):  # woken FILL_READ_MS after the answer: the order read, filled in full
                await asyncio.sleep(0.01)
                if taker.record(SLUG, "on")["orders"][0]["state"] == "final":
                    break
            order = taker.record(SLUG, "on")["orders"][0]
            self.assertEqual((order["state"], order["filled"]), ("final", 50))
            taker.stop.set()
            await asyncio.wait_for(task, 10)
            self.assertFalse(taker.running)
        run(go())
