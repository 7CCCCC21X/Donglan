"""The second review's holes, held shut: the day's loss stop holds after a later profit until /resume; the balance keeps
aside what an unconfirmed order may still take; an order that got no connection holds nothing and is tried again soon; an
empty answer is no word on an order Predict gave an id to; the order test keeps the fills an earlier step read; a reset of
the wrong shape resets nothing; a connection coming back never lets an older book in; paper records nothing past the line."""
from __future__ import annotations

import asyncio
import json
import os
import tempfile
import unittest
from decimal import Decimal as D

from fakes import book, make_taker, run
from ruletaker import http as nethttp
from ruletaker import taker as T
from ruletaker.predict import PredictApi
from ruletaker.rules import filled, held
from ruletaker.status import StatusServer
from ruletaker.store import Store
from ruletaker.stream import BookStream
from ruletaker.util import RemoteError, day_of
from test_taker import MID, SLUG, START, found


def settle(taker, key: str, pnl: float) -> None:
    """A real record settled today at ``pnl`` (as settle() leaves it), and the stop latched as a settlement does."""
    taker.recs[key] = {"slug": key, "mode": "on", "rule": "gold_shut", "side": "up", "pnl": pnl, "orders": [],
                       "settled_day": day_of(taker.now()), "start_ms": taker.now()}
    taker.hold_loss_stop()


class LossStopTest(unittest.TestCase):
    def test_the_day_s_loss_stop_holds_after_a_later_profit_until_resume(self) -> None:
        async def go() -> None:
            path = os.path.join(tempfile.mkdtemp(prefix="ruletaker-holds-"), "h.sqlite3")
            taker, api, chain, stream, clock = make_taker(START - 3_600_000, store=Store(path), RULE_MAX_DAILY_LOSS_USD="10")
            await found(taker, api)
            settle(taker, "a", -12.0)  # -$12 settled: the $10 line passed
            self.assertIn("停机线", taker.block(0.0))
            settle(taker, "b", 5.0)  # a profit later the same day (-$7: over the line again) — the stop holds all the same
            self.assertIn("之后的盈利不解除", taker.block(0.0))
            self.assertTrue(taker.halted())
            stream.take(MID, book(asks=[("0.55", "100")]))
            await asyncio.sleep(0)
            self.assertEqual(api.created, [])
            self.assertIn("今天已停机", taker.status())
            # ... over a restart too
            self.assertTrue(await asyncio.to_thread(taker.store.flush, 10))
            again, *_ = make_taker(clock.ms, store=Store(path), RULE_MAX_DAILY_LOSS_USD="10")
            self.assertIn("之后的盈利不解除", again.block(0.0))
            # /resume lifts it: another $10 from the P&L now (-$7 -> the line at -$17)
            reply = await taker.control("resume", "")
            self.assertIn("停机线移到 −$17.00", reply)
            self.assertEqual(taker.block(0.0), "")
            await taker.turn()
            self.assertEqual(taker.block(0.0), "")
            # the next day starts free
            fresh, *_ = make_taker(START - 3_600_000, RULE_MAX_DAILY_LOSS_USD="10")
            settle(fresh, "a", -12.0)
            fresh.clock.ms += 86_400_000
            self.assertEqual(fresh.loss_stop_why(), "")
        run(go())


class LossFloorTest(unittest.TestCase):
    def test_a_resumed_day_s_floor_follows_the_cap_and_may_be_zero(self) -> None:
        async def go() -> None:
            taker, api, _chain, _stream, _clock = make_taker(START - 3_600_000, RULE_MAX_DAILY_LOSS_USD="10")
            await found(taker, api)
            settle(taker, "a", -12.0)
            settle(taker, "b", 22.0)  # +$10 now: the stop held all the same
            self.assertIn("停机线", taker.state()["loss_stop"])  # (the console's resume)
            # resumed at exactly the cap: the line at $0 — a line, not a missing one
            reply = await taker.control("resume", "")
            self.assertIn("停机线移到 +$0.00", reply)
            self.assertEqual(taker.loss_floor(), 0.0)
            self.assertEqual(taker.state()["loss_stop"], "")
            settle(taker, "c", -10.5)  # -$0.50: under it
            self.assertIn("停机线", taker.block(0.0))

            taker, api, _chain, _stream, _clock = make_taker(START - 3_600_000, RULE_MAX_DAILY_LOSS_USD="10")
            await found(taker, api)
            settle(taker, "a", -12.0)
            self.assertIn("停机线移到 −$22.00", await taker.control("resume", ""))  # -$12 -> the line at -$22
            settle(taker, "b", -7.0)  # -$19: over the line
            self.assertEqual(taker.block(0.0), "")
            # the cap tightened to $5 the same day: counted from the P&L at the resume, -$17 — under it now, stopped at once
            await taker.save_settings({"RULE_MAX_DAILY_LOSS_USD": "5"})
            self.assertEqual(taker.loss_floor(), -17.0)
            self.assertIn("停机线", taker.block(0.0))
            # and back to the deploy's $10: -$22 again (the stop reached meanwhile holds until /resume)
            await taker.reset_settings(None)
            self.assertEqual(taker.loss_floor(), -22.0)
        run(go())

    def test_the_console_button_resumes_a_loss_stop_as_well_as_a_pause(self) -> None:
        with open(os.path.join(os.path.dirname(__file__), "..", "ruletaker", "web", "console.html"), encoding="utf-8") as f:
            page = f.read()
        self.assertIn("held=!!(s.paused||s.loss_stop)", page)
        self.assertIn("if(s.loss_stop){if(confirm(", page)


class ReadyTest(unittest.TestCase):
    def test_a_failed_renewal_with_the_token_still_good_holds_nothing_up(self) -> None:
        async def go() -> None:
            taker, api, _chain, _stream, _clock = make_taker(START - 3_600_000)
            await found(taker, api)
            await taker.prepare()
            self.assertTrue(taker.ready)

            async def down() -> None:
                raise RemoteError("HTTP 502: Bad Gateway")
            api.renew_auth = down
            api.usable = lambda: True  # (good 200 s more: renewed ahead, still good for an order)
            await taker.prepare()
            self.assertTrue(taker.ready, taker.not_ready_why)
            self.assertEqual(taker.block(0.0), "")
            api.usable = lambda: False  # run out with the sign-in still down: that holds the orders
            await taker.prepare()
            self.assertIn("502", taker.not_ready_why)
        run(go())

    def test_a_chain_node_s_empty_answer_is_an_error_never_a_zero(self) -> None:
        import types

        from ruletaker.predict import Chain
        answer: dict = {"result": None}

        def fake_request(url, payload=None, timeout=15, headers=None, gate=None, trace=None):
            return json.dumps({"jsonrpc": "2.0", "id": 1, "result": answer["result"]}).encode()
        wallet = types.SimpleNamespace(addresses={"USDT": "0x" + "11" * 20, "ECDSA_VALIDATOR": "0x" + "22" * 20,
                                                  "CTF_EXCHANGE": "0x" + "33" * 20}, maker="0x" + "44" * 20, signer="0x" + "55" * 20)
        account = "0x" + "ab" * 20

        async def go() -> None:
            chain = Chain("https://bsc-rpc.test/", wallet)
            saved = nethttp.request
            nethttp.request = fake_request
            try:
                for slip in ("0x", None, "0x123", "0xzz", "0x" + "00" * 31):
                    answer["result"] = slip
                    for read in (chain.usdt_balance, lambda: chain.allowance("CTF_EXCHANGE"), lambda: chain.account_owner(account)):
                        with self.assertRaises(RemoteError, msg=repr(slip)):
                            await read()
                answer["result"] = "0x"  # BNB's balance: a quantity, "0x0" for none
                with self.assertRaises(RemoteError):
                    await chain.bnb_balance()
                answer["result"] = "0x0"
                self.assertEqual(await chain.bnb_balance(), 0)
                answer["result"] = "0x" + "00" * 32  # a whole word of zeros is a zero: an account the validator does not know
                self.assertEqual(await chain.usdt_balance(), 0)
                self.assertEqual(await chain.account_owner(account), "0x" + "0" * 40)
            finally:
                nethttp.request = saved
        run(go())

    def test_a_failed_balance_read_backs_off_on_its_own_clock_in_a_busy_stretch(self) -> None:
        async def go() -> None:
            taker, api, chain, _stream, clock = make_taker(START - 50_000)  # (busy: the balance read every 20 s)
            await found(taker, api)
            await taker.refresh_balance()

            def due() -> bool:
                return taker.balance_due(taker.now(), T.BALANCE_BUSY_MS)
            clock.ms += T.BALANCE_BUSY_MS
            self.assertTrue(due())

            async def slip() -> int:
                raise RemoteError("链上节点的应答只有 0 字节（应有 32）")
            good, chain.usdt_balance = chain.usdt_balance, slip
            await taker.refresh_balance()
            # its last good read 20 s and more old, yet no read again before 15 s are out — then 30, then every minute
            for wait in (T.BALANCE_RETRY_MS, 2 * T.BALANCE_RETRY_MS, T.BALANCE_RETRY_MAX_MS, T.BALANCE_RETRY_MAX_MS):
                clock.ms += 2_000
                self.assertFalse(due())
                clock.ms += wait - 2_000
                self.assertTrue(due(), wait)
                await taker.refresh_balance()
            chain.usdt_balance = good
            clock.ms += T.BALANCE_RETRY_MAX_MS
            await taker.refresh_balance()
            self.assertEqual(taker.balance_error, "")
            clock.ms += T.BALANCE_BUSY_MS
            self.assertTrue(due())
        run(go())


class BalanceTest(unittest.TestCase):
    def test_the_balance_keeps_what_an_unconfirmed_order_may_still_take(self) -> None:
        async def go() -> None:
            taker, api, chain, stream, clock = make_taker(START - 3_600_000)
            await found(taker, api)
            taker.usdt = 100 * 10 ** 18
            api.create_error = RemoteError("网络错误 (TimeoutError: timed out)")
            stream.take(MID, book(asks=[("0.55", "100")]))
            await asyncio.sleep(0)
            await asyncio.gather(*taker.sends)
            order = taker.record(SLUG, "on")["orders"][0]
            self.assertEqual(order["state"], "unknown")
            # read again 20 s on, the chain still at $100 (nothing taken yet): the $29 the order may take stays aside
            clock.ms += 20_000
            taker.balance_asked_ms = clock.ms
            self.assertAlmostEqual(taker.usd(), 71.0)
            # resting with 20 of its 50 matched (the chain has those): the unfilled 30 at 58¢ aside
            order.update(state="open", filled=20.0, filled_ms=clock.ms - 60_000)
            self.assertAlmostEqual(taker.usd(), 100 - 30 * 0.58)
            # filled since the read began (the chain may not have it yet): the whole order aside
            order.update(filled_ms=clock.ms - 1_000)
            self.assertAlmostEqual(taker.usd(), 71.0)
            # done, its fills older than the read: nothing aside
            order.update(state="final", filled=50.0, filled_ms=clock.ms - 60_000)
            self.assertAlmostEqual(taker.usd(), 100.0)
        run(go())


class NoConnectionTest(unittest.TestCase):
    def test_an_order_that_got_no_connection_holds_nothing_and_is_tried_again_soon(self) -> None:
        async def go() -> None:
            taker, api, chain, stream, clock = make_taker(START - 3_600_000)
            await found(taker, api)
            api.refuse_connect = True
            stream.take(MID, book(asks=[("0.55", "100")]))
            await asyncio.sleep(0)
            await asyncio.gather(*taker.sends)
            rec = taker.record(SLUG, "on")
            order = rec["orders"][0]
            self.assertEqual(order["state"], "not_sent", order.get("error"))
            self.assertTrue(order["error"].startswith(nethttp.NOT_CONNECTED))
            self.assertEqual(held(rec), 0)  # the window's shares all still free
            self.assertEqual(taker.track[SLUG]["look_ms"], clock.ms + T.CONNECT_RETRY_MS)  # again in a moment
            api.refuse_connect = False
            clock.ms += T.CONNECT_RETRY_MS + 1
            stream.take(MID, book(asks=[("0.54", "100")]))
            await asyncio.sleep(0)
            await asyncio.gather(*taker.sends)
            self.assertEqual(len(api.created), 1)
            self.assertEqual(int(api.created[0]["data"]["order"]["takerAmount"]), 50 * 10 ** 18)
        run(go())


class EmptyAnswerTest(unittest.TestCase):
    def test_an_empty_answer_is_no_word_on_an_order_with_an_id(self) -> None:
        async def go() -> None:
            def fake_request(url, payload=None, timeout=15, headers=None, gate=None, trace=None):
                return b'{"success": true, "data": {}}'
            api = PredictApi("https://api.predict.fun/v1", "k", None)
            api.jwt, api.jwt_exp = "t", 0
            api.ensure_auth = lambda: asyncio.sleep(0)  # (signed in)
            saved = nethttp.request
            nethttp.request = fake_request
            try:
                with self.assertRaises(RemoteError) as caught:  # Predict gave it an id: it is there — read again later
                    await api.order("id1", "0xab")
                self.assertIn("查单应答里没有", str(caught.exception))
                self.assertIsNone(await api.order("", "0xab"))  # no id: as good as its 404
            finally:
                nethttp.request = saved
        run(go())


class OrderTestFillsTest(unittest.TestCase):
    def test_an_order_test_keeps_the_fills_an_earlier_step_read(self) -> None:
        async def go() -> None:
            taker, api, chain, stream, clock = make_taker(START - 3_600_000)
            await found(taker, api)
            api.books[MID] = (((D("0.30"), D("10")),), ((D("0.55"), D("100")),))
            api.fill_on_create = 10.0  # 10 of its 50 shares matched before the cancel
            api.order_reads = ["", "timeout"]  # the last read lost
            text = await taker.order_test()
            self.assertIn("2/4 按哈希查到：OPEN，成交 10/50", text)
            self.assertIn("3/4 撤单成功", text)
            self.assertIn("4/4 最终状态没读到", text)
            self.assertIn("成交按前面查到的 10 份记", text)
            self.assertNotIn("结论：✅", text)
            self.assertIn("最后一次查询没读到", text)
            saved = [v for _, v in taker.store.items("test:")]
            self.assertEqual((saved[0]["filled"], saved[0]["final_read"]), (10.0, False))
        run(go())


class ResetShapeTest(unittest.TestCase):
    def test_a_reset_of_the_wrong_shape_resets_nothing(self) -> None:
        async def go() -> None:
            taker, *_ = make_taker(START - 3_600_000)
            await taker.save_settings({"RULE_GOLD_SHUT_CENTS": "61", "RULE_MAX_DAY_USD": "100"})
            server = StatusServer(taker, "127.0.0.1", 0, key="")
            for body in (b'{"reset":"RULE_GOLD_SHUT_CENTS"}', b'{"reset":[]}', b'{"reset":["NOPE"]}', b'{"reset":[1]}',
                         b'{"reset":null}', b'{"reset":{"RULE_GOLD_SHUT_CENTS":true}}'):
                status, _, answer = await server.route("POST", "/api/settings", {}, {"x-rt": "1"}, body)
                self.assertEqual(status, 400, (body, answer))
                self.assertEqual((len(taker.saved_settings), taker.settings.max_day_usd), (2, 100), body)
            status, _, answer = await server.route("POST", "/api/settings", {}, {"x-rt": "1"}, b'{"reset":["RULE_GOLD_SHUT_CENTS"]}')
            self.assertEqual((status, json.loads(answer)["ok"]), (200, True))
            self.assertEqual((len(taker.saved_settings), taker.settings.max_day_usd), (1, 100))
            status, _, answer = await server.route("POST", "/api/settings", {}, {"x-rt": "1"}, b'{"reset":"all"}')
            self.assertEqual((status, json.loads(answer)["ok"]), (200, True))
            self.assertEqual(taker.saved_settings, {})
        run(go())


class StreamVersionsTest(unittest.TestCase):
    def test_a_connection_coming_back_never_lets_an_older_book_in(self) -> None:
        import time
        s = BookStream("", 2, connect=None)
        s.wanted = {"7"}
        s.conns[0].subscribed.add("7")  # connection 0 serves it
        now = int(time.time() * 1000)
        s.take("7", {"version": 102, "asks": [[0.60, 10]], "updateTimestampMs": now - 5})
        s.conns[1].connected_anew()  # connection 1 comes back...
        s.take("7", {"version": 101, "asks": [[0.55, 10]], "updateTimestampMs": now - 50})  # ...an older book arrives
        self.assertEqual(s.books["7"][1][0][0], D("0.6"))
        # a book whose counter started over (restarted): the server's own later clock lets it in
        s.take("7", {"version": 3, "asks": [[0.62, 10]], "updateTimestampMs": now + 10})
        self.assertEqual(s.books["7"][1][0][0], D("0.62"))
        s.take("7", {"version": 2, "asks": [[0.50, 10]]})  # without a stamp the version alone decides
        self.assertEqual(s.books["7"][1][0][0], D("0.62"))


class PaperLineTest(unittest.TestCase):
    def test_paper_records_nothing_past_the_15_second_line(self) -> None:
        async def go(book_ms: int) -> float:
            taker, api, chain, stream, clock = make_taker(START - 3_600_000, RULE_TAKER="paper")
            await found(taker, api)
            api.books[MID] = ((), ((D("0.55"), D("100")),))
            stream.fake_alive = False  # (no push to go by: the book read from REST)
            clock.ms = START - 16_000  # the look begins 16 s before the start...
            api.book_ms = book_ms      # ...and its book read takes this long
            await taker.turn()
            rec = taker.record(SLUG, "paper")
            return filled(rec) if rec else 0.0
        self.assertEqual(run(go(2_000)), 0.0)  # past the line when it came back: nothing recorded
        self.assertEqual(run(go(0)), 50.0)     # read at once: recorded


class UnnamedCancelTest(unittest.TestCase):
    def test_an_order_test_whose_cancel_answer_names_nothing_reads_the_order_itself(self) -> None:
        async def go(mode: str) -> tuple:
            taker, api, chain, stream, clock = make_taker(START - 3_600_000)
            await found(taker, api)
            api.books[MID] = (((D("0.30"), D("10")),), ((D("0.55"), D("100")),))
            api.remove_mode, taker.lookup_s = mode, 0.03
            return await taker.order_test(), api
        text, _ = run(go("silent"))
        self.assertIn("应答里没有它", text)
        self.assertIn('"removed":[]', text)
        self.assertIn("已经是 CANCELLED，撤单生效了", text)
        self.assertIn("结论：✅", text)
        text, api = run(go("ignored"))
        self.assertIn("仍是挂着", text)
        self.assertIn("撤单没确认", text)
        self.assertEqual(len(api.removed), 2)  # (asked once more)


if __name__ == "__main__":
    unittest.main()
