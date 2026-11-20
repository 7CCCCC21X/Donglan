"""The third review's holes, held shut: a window settles on the outcome it bought whatever its name, and one Predict is slow
to settle is read less and less often; an order past its expiry holds nothing once a read has seen it; one order test at a
time, and a confirm card's yes runs once; a cancel answered "noop" is a cancel; the record keeps the shares as signed; the
check asks for the exchange the markets settle on; a calendar run out holds every real order; the journal is made durable
behind the order; the console refuses on the head and takes no flood."""
from __future__ import annotations

import asyncio
import contextlib
import datetime as dt
import time
import unittest

from fakes import book, make_taker, market_of, run
from ruletaker import taker as T
from ruletaker.status import MAX_CONNECTIONS, StatusServer
from ruletaker.util import RemoteError, day_of
from test_taker import MID, SLUG, START, found


def bought(taker, key: str, side: str, index_set: int, end_ms: int, mid: str) -> None:
    """A real record of a finished window, its shares bought and its order final: ``index_set`` the outcome it bought."""
    taker.recs[key] = {"slug": key, "name": key, "mode": "on", "rule": "gold_shut", "side": side, "index_set": index_set,
                       "market_id": mid, "start_ms": end_ms - 900_000, "end_ms": end_ms,
                       "orders": [{"hash": f"0x{key}", "id": "", "state": "final", "shares": 10.0, "filled": 10.0, "limit": 0.55,
                                   "avg": 0.55, "fee_bps": 0, "at": end_ms - 1_000_000}]}


def settled(mid: str, names: tuple[str, str], won: int) -> dict:
    """A market settled: the outcome with ``won`` its indexSet marked WON, under the names given."""
    return {"id": mid, "status": "RESOLVED", "outcomes": [
        {"name": names[i - 1], "indexSet": i, "onChainId": str(i) * 3, "status": "WON" if i == won else "LOST"} for i in (1, 2)]}


class SettleTest(unittest.TestCase):
    def test_a_window_settles_on_the_outcome_it_bought_whatever_its_name(self) -> None:
        async def go() -> None:
            taker, api, *_ = make_taker(START)
            end = START - 3_600_000
            bought(taker, "up-won", "up", 1, end, "m1")  # "Up or Flat" / "Down": outcome 1 bought for Up, 2 for Down
            bought(taker, "down-lost", "down", 2, end, "m1")
            bought(taker, "old-unnamed", "up", 0, end, "m1")  # (written before records kept the outcome)
            api.markets["m1"] = settled("m1", ("Up or Flat", "Down"), 1)
            await taker.settle()
            won, lost, old = taker.recs["up-won"], taker.recs["down-lost"], taker.recs["old-unnamed"]
            self.assertEqual((won["pnl"], won["result"]), (4.5, "up"))
            self.assertEqual((lost["pnl"], lost["result"]), (-5.5, "up"))
            # a record without the outcome it bought, on a name not understood: told once, never settled on a guess
            self.assertTrue(old.get("pnl") is None and old.get("settle_alarmed"))
            self.assertIn("结果名称认不出：Up or Flat", old["settle_error"])
            self.assertAlmostEqual(taker.today_pnl(), 4.5 - 5.5)  # the loss stop sees the lost window
        run(go())

    def test_a_window_predict_is_slow_to_settle_is_read_less_and_less_often(self) -> None:
        async def go() -> None:
            taker, api, chain, stream, clock = make_taker(START)
            bought(taker, "slow", "up", 1, START - 5 * 60_000, "m2")
            api.markets["m2"] = market_of("m2", "slow", START, "CLOSED")  # (not settled yet)

            def reads() -> int:
                return sum(1 for c in api.calls if c == ("market", "m2"))

            async def at(ms: int) -> None:
                clock.ms += ms
                await taker.settle()
            await at(0)
            await at(T.SETTLE_EVERY_MS)
            self.assertEqual(reads(), 2)  # every minute while it is fresh
            await at(2 * 3_600_000)  # two hours past its end: every ten minutes
            await at(5 * 60_000)
            self.assertEqual(reads(), 3)
            await at(5 * 60_000)
            self.assertEqual(reads(), 4)
            await at(86_400_000)  # a day past it: every hour
            await at(30 * 60_000)
            self.assertEqual(reads(), 5)
            await at(30 * 60_000)
            self.assertEqual(reads(), 6)
        run(go())


class ReserveTest(unittest.TestCase):
    def test_an_order_past_its_expiry_holds_nothing_once_a_read_has_seen_it(self) -> None:
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

            def read_at(ms: int) -> float:
                clock.ms = taker.balance_asked_ms = ms
                return taker.usd()
            expiry = order["expires"] * 1000
            self.assertAlmostEqual(read_at(expiry + 60_000), 71.0)  # a read a minute past it may lack a match just before
            self.assertAlmostEqual(read_at(expiry + T.GIVE_UP_MS + T.BALANCE_LAG_MS + 1), 100.0)  # one well past it has all
            self.assertEqual(order["state"], "unknown")
        run(go())


class OneAtATimeTest(unittest.TestCase):
    def test_one_order_test_at_a_time(self) -> None:
        async def go() -> None:
            taker, api, *_ = make_taker(START - 3_600_000)
            await found(taker, api)
            api.books[MID] = ([(T.D("0.40"), T.D("10"))], [(T.D("0.60"), T.D("10"))])
            api.create_delay = 0.05  # (the first one's order on its way when the second is asked)
            first, second = await asyncio.gather(taker.order_test(), taker.order_test())
            self.assertIn("1/4", first)
            self.assertIn("挂单测试正在进行", second)
            self.assertEqual(len(api.created), 1)
            self.assertFalse(taker.testing)
            again = await taker.order_test()  # (done: the next one runs)
            self.assertIn("1/4", again)
            self.assertEqual(len(api.created), 2)
        run(go())

    def test_a_yes_tapped_twice_runs_its_command_once(self) -> None:
        from ruletaker.notify import Telegram

        async def go() -> None:
            calls: list[tuple] = []
            runs: list[str] = []

            async def fake_call(method: str, payload: dict, timeout: float = 15, token: str = ""):
                calls.append((method, payload))
                return True

            async def handler(command: str, args: str) -> str:
                runs.append(command)
                await asyncio.sleep(0.02)
                return "做完了"
            tg = Telegram("", (), admin=42, commands=True, handler=handler, command_token="CMD")
            tg.call = fake_call

            def tap(data: str) -> None:
                tg.on_update({"update_id": 1, "callback_query": {"id": "q", "data": data, "from": {"id": 42},
                                                                 "message": {"message_id": 77, "chat": {"id": 42}}}})
            now = int(time.time())
            tap(f"y:{now}:/test")
            tap(f"y:{now}:/test")  # (tapped again before the card changed)
            await asyncio.sleep(0.1)
            self.assertEqual(runs, ["/test"])
            self.assertTrue(any(m == "answerCallbackQuery" and p.get("text") == "已经确认过，正在执行或已执行完" for m, p in calls))
            self.assertGreaterEqual(sum(1 for m, _ in calls if m == "editMessageText"), 2)  # the card's buttons gone first
            tap(f"y:{now - 1}:/test")  # a new card on the same message (its own time): its yes runs
            await asyncio.sleep(0.1)
            self.assertEqual(runs, ["/test", "/test"])
        run(go())


class WithdrawTest(unittest.TestCase):
    def test_an_order_already_filled_when_the_pause_withdraws_it_is_not_withdrawn_twice(self) -> None:
        async def go() -> None:
            taker, api, chain, stream, clock = make_taker(START - 3_600_000)
            await found(taker, api)
            api.create_delay = 0.02
            api.remove_mode = "noop"  # (Predict: nothing to withdraw — it is filled)
            stream.take(MID, book(asks=[("0.55", "100")]))
            await asyncio.sleep(0)
            taker.stop.set()  # the service stops while the answer is on its way
            await asyncio.gather(*taker.sends)
            order = taker.record(SLUG, "on")["orders"][0]
            self.assertEqual(order["state"], "cancelling")
            self.assertTrue(order.get("removed"))
            self.assertEqual(api.removed, ["id1"])
            await taker.withdraw_all("服务停止")
            self.assertEqual(api.removed, ["id1"])  # (asked once, not again)
        run(go())


class SharesTest(unittest.TestCase):
    def test_the_record_keeps_the_shares_as_signed(self) -> None:
        async def go() -> None:
            taker, api, chain, stream, clock = make_taker(START - 3_600_000)
            await found(taker, api)
            taker.usdt = 1_000_000 * 10 ** 18
            taker.settings = taker.settings.__class__(**{**taker.settings.__dict__, "shares": (("gold", 123_456), ("oil", 50))})
            stream.take(MID, book(asks=[("0.55", "200000")]))
            await asyncio.sleep(0)
            await asyncio.gather(*taker.sends)
            self.assertEqual(int(api.created[0]["data"]["order"]["takerAmount"]), 123_450 * 10 ** 18)  # 5 significant digits
            self.assertEqual(taker.record(SLUG, "on")["orders"][0]["shares"], 123_450.0)
        run(go())


class CheckTest(unittest.TestCase):
    def test_the_check_asks_for_the_exchange_the_markets_settle_on(self) -> None:
        async def go() -> None:
            taker, api, chain, *_ = make_taker(START - 3_600_000)
            await found(taker, api)
            taker.track[SLUG]["market"]["isNegRisk"] = True
            chain.allowed_by["NEG_RISK_CTF_EXCHANGE"] = 0
            text = await taker.self_check()
            self.assertIn("❌ 授权：多结果交易所 没有 USDT 额度", text)
            self.assertNotIn("标准交易所", text)
            self.assertIn("结论：❌", text)
            chain.allowed_by.clear()
            text = await taker.self_check()
            self.assertIn("✅ 授权：多结果交易所 USDT 额度已授权", text)
            self.assertIn("结论：✅", text)
        run(go())

    def test_a_calendar_run_out_holds_every_real_order(self) -> None:
        async def go() -> None:
            taker, api, chain, stream, clock = make_taker(START - 3_600_000)
            await found(taker, api)
            today = dt.date(1970, 1, 1) + dt.timedelta(days=clock.ms // 86_400_000)
            taker.cal_last = today + dt.timedelta(days=10)  # ten days left: told, still trading
            self.assertIn("还剩 11 天", taker.status())
            taker.plan(clock.ms)
            self.assertEqual(taker.cal_told, day_of(clock.ms))
            self.assertEqual(taker.block(0.0), "")
            text = await taker.self_check()
            self.assertIn("⚠️ CME 休市日历只排到", text)
            self.assertIn("结论：✅", text)
            taker.cal_last = today - dt.timedelta(days=1)  # over: no real order
            self.assertIn("CME 休市日历只排到", taker.block(0.0))
            stream.take(MID, book(asks=[("0.55", "100")]))
            await asyncio.sleep(0)
            await asyncio.gather(*taker.sends)
            self.assertEqual(api.created, [])
            text = await taker.self_check()
            self.assertIn("❌ CME 休市日历只排到", text)
            self.assertIn("结论：❌", text)
        run(go())


class JournalTest(unittest.TestCase):
    def test_the_journal_is_made_durable_behind_the_order(self) -> None:
        async def go() -> None:
            taker, api, chain, stream, clock = make_taker(START - 3_600_000, journal=True)
            await found(taker, api)
            before = taker.journal_syncs
            stream.take(MID, book(asks=[("0.55", "100")]))
            await asyncio.sleep(0)
            await asyncio.gather(*taker.sends)
            for _ in range(200):
                if taker.journal_syncs > before:
                    break
                await asyncio.sleep(0.005)
            self.assertGreater(taker.journal_syncs, before)
        run(go())


class ConsoleServerTest(unittest.TestCase):
    def test_the_console_refuses_on_the_head_and_takes_no_flood(self) -> None:
        async def go() -> None:
            taker, *_ = make_taker(START - 3_600_000)
            server = StatusServer(taker, "127.0.0.1", 1, key="secret-key-123")
            listener = await asyncio.start_server(server.handle, "127.0.0.1", 0)
            port = listener.sockets[0].getsockname()[1]

            async def answer(reader, writer) -> tuple[bool, str]:
                try:
                    got = await asyncio.wait_for(reader.read(), 2)
                    return True, got.decode()
                except asyncio.TimeoutError:
                    return False, ""
                finally:
                    writer.close()
                    with contextlib.suppress(Exception):
                        await writer.wait_closed()
            # a POST without the key: refused on its head — the body it promises is never waited for
            reader, writer = await asyncio.open_connection("127.0.0.1", port)
            writer.write(b"POST /pause HTTP/1.1\r\nX-RT: 1\r\nContent-Length: 60000\r\n\r\n")
            in_time, text = await answer(reader, writer)
            self.assertTrue(in_time and text.startswith("HTTP/1.1 403"), text)
            self.assertEqual(taker.paused, "")
            # as many idle connections as it serves at once: the next one is closed at once
            idle = [await asyncio.open_connection("127.0.0.1", port) for _ in range(MAX_CONNECTIONS)]
            await asyncio.sleep(0.05)
            reader, writer = await asyncio.open_connection("127.0.0.1", port)
            in_time, text = await answer(reader, writer)
            self.assertTrue(in_time and text == "", text)
            gone = idle.pop()[1]  # one of them gone: served again
            gone.close()
            await gone.wait_closed()
            await asyncio.sleep(0.05)
            reader, writer = await asyncio.open_connection("127.0.0.1", port)
            writer.write(b"GET /healthz HTTP/1.1\r\n\r\n")
            in_time, text = await answer(reader, writer)
            self.assertTrue(in_time and text.startswith("HTTP/1.1 "), text)
            for _, w in idle:
                w.close()
                with contextlib.suppress(Exception):
                    await w.wait_closed()
            listener.close()
            await listener.wait_closed()
        run(go())


if __name__ == "__main__":
    unittest.main()
