"""The review's five holes, held shut: a save waiting for the disk never undoes a stop made meanwhile; a change the disk did not
take is never told as saved; the order test holds to the deploy's hard switch and the pause (at its start and right before its
bytes); an order is judged again after a new connection's handshake (on the loop, from the request's thread); a test order whose
answer was lost is looked for by its hash and withdrawn, never told as not placed."""
from __future__ import annotations

import asyncio
import json
import os
import sqlite3
import tempfile
import threading
import unittest
from decimal import Decimal as D

from fakes import make_taker, run
from ruletaker import http as nethttp
from ruletaker.predict import PredictApi
from ruletaker.status import StatusServer
from ruletaker.store import Store
from ruletaker.util import NotOnDisk, NotSent, RemoteError
from test_taker import MID, SLUG, START, found


def locked_store() -> tuple[Store, sqlite3.Connection]:
    """A store on a file, and another connection holding the database (BEGIN EXCLUSIVE: no write gets in until it lets go)."""
    store = Store(os.path.join(tempfile.mkdtemp(prefix="ruletaker-gates-"), "g.sqlite3"))
    lock = sqlite3.connect(store.path, timeout=0)
    lock.execute("BEGIN EXCLUSIVE")
    return store, lock


def a_book(api) -> None:
    """Predict's book for the order test: a 30¢ bid, a 55¢ offer (the test rests at 2¢)."""
    api.books[MID] = (((D("0.30"), D("10")),), ((D("0.55"), D("100")),))


class DiskTest(unittest.TestCase):
    def test_a_mode_switched_while_a_save_waits_for_the_disk_stays(self) -> None:
        async def go() -> None:
            store, lock = locked_store()
            taker, *_ = make_taker(START - 3_600_000, store=store)
            taker.flush_wait_s = 5.0
            try:
                save = asyncio.ensure_future(taker.save_settings({"RULE_GOLD_SHUT_CENTS": "61"}))
                await asyncio.sleep(0.2)  # the save waits for the disk...
                self.assertFalse(save.done())
                self.assertEqual(taker.settings.price("gold_shut"), 61)  # ...in force already
                stop = asyncio.ensure_future(taker.control("mode", "off"))  # ...and the operator stops real orders meanwhile
                await asyncio.sleep(0.1)
                self.assertEqual(taker.settings.mode, "off")
            finally:
                lock.rollback()
                lock.close()
            self.assertIn("已保存", await save)
            self.assertIn("规则吃单改成", await stop)
            self.assertEqual(taker.settings.mode, "off")  # the save, done after the stop, did not bring "on" back
            self.assertEqual(taker.settings.price("gold_shut"), 61)
            # and the same for a reset
            lock = sqlite3.connect(store.path, timeout=0)
            lock.execute("BEGIN EXCLUSIVE")
            await taker.control("mode", "on")
            try:
                reset = asyncio.ensure_future(taker.reset_settings(None))
                await asyncio.sleep(0.2)
                self.assertEqual(taker.settings.price("gold_shut"), 58)
                stop = asyncio.ensure_future(taker.control("mode", "paper"))
                await asyncio.sleep(0.1)
            finally:
                lock.rollback()
                lock.close()
            self.assertIn("恢复", await reset)
            await stop
            self.assertEqual(taker.settings.mode, "paper")
        run(go())

    def test_a_save_the_disk_did_not_take_is_never_told_as_saved(self) -> None:
        async def go() -> None:
            store, lock = locked_store()
            taker, *_ = make_taker(START - 3_600_000, store=store)
            taker.flush_wait_s = 0.3
            server = StatusServer(taker, "127.0.0.1", 0, key="")
            try:
                with self.assertRaises(NotOnDisk) as caught:
                    await taker.save_settings({"RULE_GOLD_SHUT_CENTS": "61"})
                self.assertIn("还没写进数据库", str(caught.exception))
                self.assertIn("回到改之前的值", str(caught.exception))
                self.assertEqual(taker.settings.price("gold_shut"), 61)  # in force all the same
                _, _, body = await server.route("POST", "/api/settings", {}, {"x-rt": "1"}, b'{"values":{"RULE_OIL_SHARES":"30"}}')
                answer = json.loads(body)
                self.assertEqual((answer["ok"], answer.get("applied")), (False, True))  # never "ok": the page shows what is in force
                self.assertIn("还没写进数据库", answer["message"])
                self.assertEqual(taker.settings.shares_of("oil"), 30)
                _, _, body = await server.route("POST", "/api/settings", {}, {"x-rt": "1"}, b'{"reset":"all"}')
                answer = json.loads(body)
                self.assertEqual((answer["ok"], answer.get("applied")), (False, True))
                self.assertIn("回到网页保存的值", answer["message"])
                # the pause, the mode, the resume: in force, and said not to be on disk yet
                self.assertIn("还没写进数据库", await taker.control("pause", "测试"))
                self.assertTrue(taker.paused)
                self.assertIn("还没写进数据库", await taker.control("mode", "paper"))
                self.assertIn("还没写进数据库", await taker.control("resume", ""))
                # a valid save, the disk free: told as saved
            finally:
                lock.rollback()
                lock.close()
            self.assertIn("已保存", await taker.save_settings({"RULE_GOLD_SHUT_CENTS": "60"}))
            self.assertTrue(await asyncio.to_thread(store.flush, 30))
            again = Store(store.path)
            self.assertEqual(again.get("control:mode"), "paper")
        run(go())


class OrderTestGatesTest(unittest.TestCase):
    def test_the_order_test_holds_to_the_hard_switch_and_the_pause(self) -> None:
        async def go() -> None:
            # the deploy's hard switch off: the test is a real order — not sent
            taker, api, *_ = make_taker(START - 3_600_000, LIVE_TRADING_ALLOWED="off")
            text = await taker.order_test()
            self.assertIn("❌ 挂单测试没做", text)
            self.assertIn("LIVE_TRADING_ALLOWED", text)
            self.assertFalse(any(c[0] == "create" for c in api.calls))
            # paused: not sent either
            taker, api, chain, stream, clock = make_taker(START - 3_600_000)
            await found(taker, api)
            a_book(api)
            await taker.control("pause", "测试")
            text = await taker.order_test()
            self.assertIn("❌ 挂单测试没做：已暂停", text)
            self.assertFalse(any(c[0] == "create" for c in api.calls))
            await taker.control("resume", "")
            # paused while the test's request waited on its way (a token, a slot): held right before its bytes

            async def paused_meanwhile() -> None:
                taker.paused = "路上按的暂停"
            api.pre_send = paused_meanwhile
            text = await taker.order_test()
            self.assertIn("1/4 下单失败", text)
            self.assertIn("已暂停（路上按的暂停）", text)
            self.assertEqual(api.created, [])
            # stopping during a new connection's handshake: held as well
            api.pre_send, taker.paused = None, ""
            api.connect_delay = 0.05

            async def stop_soon() -> None:
                await asyncio.sleep(0.01)
                taker.stop.set()
            stopper = asyncio.ensure_future(stop_soon())
            text = await taker.order_test()
            await stopper
            self.assertIn("服务正在停止", text)
            self.assertEqual(api.created, [])
            # all clear: the test goes out
            taker.stop.clear()
            api.connect_delay = 0.0
            self.assertIn("1/4 下单成功", await taker.order_test())
            self.assertEqual(len(api.created), 1)
        run(go())

    def test_an_order_test_whose_answer_is_lost_is_found_by_its_hash_and_withdrawn(self) -> None:
        async def go() -> None:
            taker, api, chain, stream, clock = make_taker(START - 3_600_000)
            await found(taker, api)
            a_book(api)
            taker.lookup_s = 0.01
            api.lose_answer = True  # Predict took it; its answer was lost on the way
            text = await taker.order_test()
            self.assertIn("1/4 下单的应答没收到", text)
            self.assertIn("1/4 按哈希找到了：订单 #id1", text)
            self.assertIn("3/4 撤单成功", text)
            self.assertIn("结论：✅", text)
            self.assertEqual(api.removed, ["id1"])
            # lost, and nowhere to be found: unknown — never "not placed"
            api.lose_answer = False
            api.create_error = RemoteError("网络错误 (TimeoutError: timed out)")
            text = await taker.order_test()
            self.assertIn("下单结果未知", text)
            self.assertNotIn("结论：❌", text)
            self.assertNotIn("这张测试单没下成", text)
            self.assertEqual(sum(1 for c in api.calls if c[0] == "order"), 3 + 4)  # (the first test's two reads and its lookup; four here)
            self.assertEqual(len([v for _, v in taker.store.items("test:") if v.get("unknown")]), 1)
            # a refusal is still told as one, at once
            api.create_error = RemoteError("HTTP 400: order value too small")
            text = await taker.order_test()
            self.assertIn("结论：❌ 这张测试单没下成", text)
            self.assertEqual(api.removed, ["id1"])
        run(go())


class LastMomentGateTest(unittest.TestCase):
    def test_a_pause_during_a_new_connection_holds_the_order(self) -> None:
        from fakes import book

        async def go() -> None:
            taker, api, chain, stream, clock = make_taker(START - 3_600_000)
            await found(taker, api)
            api.connect_delay = 0.05  # no warm connection: the handshake before the bytes
            stream.take(MID, book(asks=[("0.55", "100")]))
            await asyncio.sleep(0.01)
            await taker.control("pause", "握手时按的")
            await asyncio.gather(*taker.sends)
            order = taker.record(SLUG, "on")["orders"][0]
            self.assertEqual(order["state"], "not_sent")
            self.assertIn("已暂停（握手时按的）", order["error"])
            self.assertEqual(api.created, [])
        run(go())

    def test_the_request_thread_asks_the_loop(self) -> None:
        async def go() -> None:
            state = {"paused": False}
            asked: list = []

            def judge(check: str) -> str:
                asked.append((check, threading.current_thread() is threading.main_thread()))
                return "已暂停（握手时按的）" if state["paused"] else ""
            gates: list = []

            def fake_request(url, payload=None, timeout=15, headers=None, gate=None, trace=None):
                gates.append(gate is not None)
                if gate is not None:
                    state["paused"] = True  # (the pause lands while the handshake runs on this thread)
                    if why := gate():
                        raise NotSent(why)
                return b'{"success": true, "data": {"orderId": "7", "orderHash": "0xab"}}'
            api = PredictApi("https://api.predict.fun/v1", "k", None, before_send=judge)
            saved = nethttp.request
            nethttp.request = fake_request
            try:
                with self.assertRaises(NotSent) as caught:
                    await api.call("POST", "/orders", {}, priority=True, check="0xab")
                self.assertIn("已暂停", str(caught.exception))
                self.assertEqual(asked, [("0xab", True), ("0xab", True)])  # before the thread, and from it — on the loop both times
                await api.call("GET", "/markets/1")  # a read: no gate
                self.assertEqual(gates, [True, False])
            finally:
                nethttp.request = saved
        run(go())


if __name__ == "__main__":
    unittest.main()
