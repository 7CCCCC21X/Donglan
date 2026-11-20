"""The control page and the review page: saved settings (checked whole, in force at once, kept over a restart, dropped when the
settings file no longer agrees), the mode's ceiling, the API's routes and its header check, the review's sums, the CSV."""
from __future__ import annotations

import asyncio
import json
import os
import tempfile
import unittest

from fakes import make_taker, run
from ruletaker import console
from ruletaker.review import daily, records_csv, review, totals
from ruletaker.status import StatusServer
from ruletaker.store import Store
from ruletaker.taker import RuleTaker

START = 1_795_039_200_000  # 2026-11-19 06:00 Beijing (17:00 EST): gold's first shut window


def rec(slug: str, mode: str, start: int, filled: float, avg: float, pnl: float | None, rule: str = "gold_shut") -> dict:
    return {"slug": slug, "rule": rule, "side": "up", "mode": mode, "name": "黄金 15分涨跌 06:00–06:15", "start_ms": start, "end_ms": start + 900_000,
            "orders": [{"state": "final", "shares": 50, "filled": filled, "limit": 0.58, "avg": avg, "fee_bps": 200, "at": start - 3_600_000,
                        "hash": "0x" + slug.encode().hex()[:64].ljust(64, "0"), **({"paper": True} if mode == "paper" else {})}],
            "result": "up" if pnl is not None and pnl > 0 else "down" if pnl is not None else "", "pnl": pnl,
            **({"settled_day": "2026-11-19"} if pnl is not None else {})}


class SettingsTest(unittest.TestCase):
    def test_save_check_reset_and_restart(self) -> None:
        async def go() -> None:
            folder = tempfile.mkdtemp(prefix="ruletaker-console-")
            store = Store(os.path.join(folder, "c.sqlite3"))
            taker, *_ = make_taker(START - 3_600_000, store=store)
            self.assertEqual(taker.settings.price("gold_shut"), 58)
            msg = await taker.save_settings({"RULE_GOLD_SHUT_CENTS": "60", "RULE_MAX_DAILY_LOSS_USD": "150"})
            self.assertIn("已保存", msg)
            self.assertEqual(taker.settings.price("gold_shut"), 60)
            self.assertEqual(taker.loss_cap, 150)
            self.assertEqual(taker.planned_ms, 0)  # (the windows planned again at the next tick)
            with self.assertRaises(ValueError):
                await taker.save_settings({"RULE_GOLD_SHUT_CENTS": "99"})  # out of range: nothing changes
            self.assertEqual(taker.settings.price("gold_shut"), 60)
            with self.assertRaises(ValueError):
                await taker.save_settings({"PREDICT_PRIVATE_KEY": "0x00"})  # not a field of the page
            with self.assertRaises(ValueError):
                await taker.save_settings({"RULE_GOLD": "maybe"})
            fields = {f["key"]: f for g in taker.settings_view() for f in g["fields"]}
            self.assertEqual((fields["RULE_GOLD_SHUT_CENTS"]["value"], fields["RULE_GOLD_SHUT_CENTS"]["saved"]), ("60", True))
            self.assertFalse(fields["RULE_OIL_OPEN_CENTS"]["saved"])
            # a restart keeps what the page saved
            store.flush()
            again = RuleTaker(taker.config, Store(store.path), api=taker.api, chain=taker.chain, stream=taker.stream, clock=taker.clock)
            self.assertEqual(again.settings.price("gold_shut"), 60)
            self.assertEqual(again.loss_cap, 150)
            # back to the settings file's
            msg = await again.reset_settings(["RULE_GOLD_SHUT_CENTS"])
            self.assertIn("恢复", msg)
            self.assertEqual(again.settings.price("gold_shut"), 58)
            self.assertEqual(again.loss_cap, 150)
            await again.reset_settings(None)
            self.assertEqual(again.loss_cap, 0)
        run(go())

    def test_a_saved_value_the_settings_file_no_longer_takes_is_dropped(self) -> None:
        store = Store(":memory:")
        store.put(console.SAVED, {"RULE_GOLD_SHUT_CENTS": "61", "RULE_FAST_HOURS": "garbage"})
        taker, *_ = make_taker(START - 3_600_000, store=store)
        self.assertEqual(taker.settings_dropped, ["RULE_FAST_HOURS"])
        self.assertEqual(taker.settings.price("gold_shut"), 61)  # the other one kept
        self.assertEqual(store.get(console.SAVED), {"RULE_GOLD_SHUT_CENTS": "61"})

    def test_mode_never_above_the_settings_file_and_settings_kept(self) -> None:
        async def go() -> None:
            taker, *_ = make_taker(START - 3_600_000, RULE_TAKER="paper")
            await taker.save_settings({"RULE_GOLD_SHARES": "80"})
            reply = await taker.control("mode", "on")
            self.assertIn("最多调到", reply)
            self.assertEqual(taker.settings.mode, "paper")
            await taker.control("mode", "off")
            self.assertEqual((taker.settings.mode, taker.settings.shares_of("gold")), ("off", 80))
            await taker.control("mode", "paper")
            self.assertEqual((taker.settings.mode, taker.settings.shares_of("gold")), ("paper", 80))
        run(go())


class RoutesTest(unittest.TestCase):
    def test_page_api_and_the_header_check(self) -> None:
        async def go() -> None:
            taker, *_ = make_taker(START - 3_600_000)
            taker.recs["a"] = rec("xau-updown-15m-1", "on", START - 86_400_000, 50, 0.56, 21.2)
            server = StatusServer(taker, "127.0.0.1", 0, key="")
            status, kind, page = await server.route("GET", "/", {}, {})
            self.assertEqual(status, 200)
            self.assertIn("text/html", kind)
            self.assertIn("规则吃单", page)
            status, _, body = await server.route("GET", "/api/state", {}, {})
            state = json.loads(body)
            self.assertEqual((status, state["mode"]), (200, "on"))
            self.assertTrue(any(g["group"] == "黄金" for g in state["settings"]))
            status, _, body = await server.route("GET", "/api/review", {"days": ["all"]}, {})
            data = json.loads(body)
            self.assertEqual(data["totals"]["on"]["windows"], 1)
            # a POST without X-RT: refused (no other site's page can send it)
            status, _, _ = await server.route("POST", "/api/control", {}, {}, b'{"action":"pause"}')
            self.assertEqual(status, 403)
            self.assertEqual(taker.paused, "")
            status, _, body = await server.route("POST", "/api/control", {}, {"x-rt": "1"}, b'{"action":"pause"}')
            self.assertTrue(json.loads(body)["ok"])
            self.assertTrue(taker.paused)
            status, _, body = await server.route("POST", "/api/settings", {}, {"x-rt": "1"}, b'{"values":{"RULE_OIL_SHARES":"0"}}')
            answer = json.loads(body)
            self.assertFalse(answer["ok"])  # 0 shares: refused, and why
            self.assertIn("RULE_OIL_SHARES", answer["message"])
            status, _, body = await server.route("POST", "/api/settings", {}, {"x-rt": "1"}, b'{"values":{"RULE_OIL_SHARES":"30"}}')
            self.assertTrue(json.loads(body)["ok"])
            self.assertEqual(taker.settings.shares_of("oil"), 30)
            status, kind, csv_text = await server.route("GET", "/api/records.csv", {}, {})
            self.assertIn("text/csv", kind)
            self.assertIn("开场（北京）", csv_text)
            self.assertEqual(len(csv_text.strip().splitlines()), 2)
            # with a key: the page itself is open, the data is not
            locked = StatusServer(taker, "127.0.0.1", 0, key="k" * 16)
            self.assertEqual((await locked.route("GET", "/", {}, {}))[0], 200)
            self.assertEqual((await locked.route("GET", "/api/state", {}, {}))[0], 403)
            self.assertEqual((await locked.route("GET", "/api/state", {}, {"x-status-key": "k" * 16}))[0], 200)
        run(go())


class ReviewTest(unittest.TestCase):
    def test_sums_daily_and_csv(self) -> None:
        day = 86_400_000
        recs = {"a": rec("s1", "on", START - 3 * day, 50, 0.56, 21.2), "b": rec("s2", "on", START - 2 * day, 40, 0.57, -22.8),
                "c": rec("s3", "on", START - day, 50, 0.55, None), "d": rec("s4", "paper", START - day, 50, 0.55, 22.0)}
        t = totals(recs, "on")
        self.assertEqual((t["windows"], t["settled"], t["wins"], t["losses"], t["open"]), (3, 2, 1, 1, 1))
        self.assertAlmostEqual(t["pnl"], -1.6)
        d = daily(recs, "on")
        self.assertEqual([x["cum"] for x in d], [21.2, -1.6])
        taker, *_ = make_taker(START)
        data = review(recs, taker.settings, START, 30)
        self.assertEqual(data["totals"]["paper"]["windows"], 1)
        names = {(r["rule"], r["mode"]) for r in data["rules"]}
        self.assertIn(("gold_shut", "on"), names)
        self.assertEqual(review(recs, taker.settings, START, 2)["totals"]["on"]["windows"], 2)  # the last two days' windows alone
        lines = records_csv(recs).strip().splitlines()
        self.assertEqual(len(lines), 5)

    def test_cached_and_never_in_the_race_seconds(self) -> None:
        taker, *_ = make_taker(START - 3_600_000)
        first = taker.review_data(30)
        self.assertNotIn("busy", first)
        taker.clock.ms += 1_000
        self.assertEqual(taker.review_data(30)["computed_ms"], first["computed_ms"])  # kept 30 s
        taker.quiet_ms = lambda now: 5_000  # the race's seconds: the kept one, never worked out now
        taker.clock.ms += 60_000
        self.assertEqual(taker.review_data(30)["computed_ms"], first["computed_ms"])
        self.assertIn("busy", taker.review_data(7))

    def test_the_status_text_waits_out_the_race_seconds(self) -> None:
        async def go() -> None:
            taker, *_ = make_taker(START - 3_600_000)
            taker.quiet_ms = lambda now: 5_000  # the race's seconds: /status is worked out once they are over
            asked = asyncio.ensure_future(taker.command("/status", ""))
            await asyncio.sleep(0.3)
            self.assertFalse(asked.done())
            taker.quiet_ms = lambda now: 0
            self.assertIn("规则吃单", await asyncio.wait_for(asked, 2))
            self.assertIn("规则吃单", await asyncio.wait_for(taker.command("/status", ""), 0.1))  # out of them: at once
        run(go())


if __name__ == "__main__":
    unittest.main()


class SelfCheckTest(unittest.TestCase):
    def test_self_check_says_where_the_key_is_kept(self) -> None:
        import dataclasses
        from test_taker import found

        async def go() -> None:
            taker, api, chain, stream, clock = make_taker(START - 3_600_000)
            await found(taker, api)
            # not run by systemd (a laptop, a test): nothing to keep it in, nothing said
            self.assertNotIn("私钥：", await taker.self_check())
            for kept, want in (("stored", "✅ 私钥：加密保存在这台机器上"),
                               ("both", "⚠️ 私钥：已加密保存，但设置文件里还留着同一把的明文"),
                               ("settings", "⚠️ 私钥：在设置文件里是明文")):
                taker.config = dataclasses.replace(taker.config, key_kept=kept, key_store=True)
                text = await taker.self_check()
                self.assertIn(want, text, kept)
                self.assertIn("结论：✅ 可以真实吃单", text, kept)  # (a warning, never a reason not to trade)
        run(go())

    def test_self_check_all_good_then_a_region_that_may_not_trade(self) -> None:
        from fakes import book, market_of
        from test_taker import MID, SLUG, found

        async def go() -> None:
            taker, api, chain, stream, clock = make_taker(START - 3_600_000)
            await found(taker, api)
            stream.take(MID, book(asks=[("0.65", "10")]))  # (nothing at the rule's price: no order)
            text = await taker.self_check()
            self.assertIn("✅ Predict 登录", text)
            self.assertIn("✅ 下单接口在这个区域可用", text)
            self.assertIn("✅ 推送：已连接", text)
            self.assertIn("✅ 授权：标准交易所", text)
            self.assertIn("结论：✅ 可以真实吃单", text)
            self.assertEqual(api.created, [])  # a self-check places nothing
            api.empty_order_answer = "HTTP 403: 访问被拒绝"
            text = await taker.self_check()
            self.assertIn("❌ 下单接口 403", text)
            self.assertIn("结论：❌", text)
            chain.allowed = 0
            api.empty_order_answer = "HTTP 400: order is invalid"
            self.assertIn("❌ 授权：标准交易所 没有 USDT 额度", await taker.self_check())
            self.assertTrue(market_of)  # (imported for the next test's market)
            self.assertTrue(SLUG)
        run(go())

    def test_order_test_rests_far_under_the_market_and_is_withdrawn(self) -> None:
        from test_taker import MID, found

        async def go() -> None:
            taker, api, chain, stream, clock = make_taker(START - 3_600_000)
            await found(taker, api)
            api.books[MID] = (((__import__("decimal").Decimal("0.30"), __import__("decimal").Decimal("10")),),
                              ((__import__("decimal").Decimal("0.55"), __import__("decimal").Decimal("100")),))
            records_before = dict(taker.recs)
            text = await taker.order_test()
            self.assertIn("1/4 下单成功", text)
            self.assertIn("3/4 撤单成功", text)
            self.assertIn("结论：✅", text)
            sent = api.created[0]["data"]
            self.assertEqual(sent["pricePerShare"], str(2 * 10 ** 16))            # 2¢: far under the 55¢ offer, a cent under the 30¢ bid at most
            self.assertEqual(int(sent["order"]["takerAmount"]), 50 * 10 ** 18)    # $1 of it
            self.assertGreater(int(sent["order"]["expiration"]), taker.now() // 1000)  # (not the rule's expiry: ten minutes on)
            self.assertEqual(api.removed, ["id1"])
            self.assertEqual(taker.recs, records_before)                         # no rule record: nothing else follows it
            self.assertTrue(any(k.startswith("test:") for k, _ in taker.store.items("test:")))
            # (the book as Predict answers the test's read — not pushed: an offer under the rule's price would be bought by the taker)
            api.books[MID] = ((), ((__import__("decimal").Decimal("0.01"), __import__("decimal").Decimal("100")),))  # an offer at 1¢: no room
            text = await taker.order_test()
            self.assertIn("放不下一张不成交的测试单", text)
            self.assertEqual(len(api.created), 1)
        run(go())

    def test_commands_reach_the_checks(self) -> None:
        async def go() -> None:
            taker, *_ = make_taker(START - 3_600_000)
            self.assertIn("规则吃单自检", await taker.command("/check", ""))
            self.assertIn("/check 自检", await taker.command("/help", ""))
        run(go())


class TelegramCommandsTest(unittest.TestCase):
    def test_commands_through_a_bot_of_their_own(self) -> None:
        from ruletaker.notify import Telegram

        async def go() -> None:
            taker, *_ = make_taker(START - 3_600_000)
            calls: list[tuple] = []
            updates = [[{"update_id": 1, "message": {"text": "/check", "chat": {"id": 42}, "from": {"id": 42}}},
                        {"update_id": 2, "message": {"text": "/pause", "chat": {"id": 7}, "from": {"id": 7}}},          # not the admin
                        {"update_id": 3, "message": {"text": "/id@rt_bot", "chat": {"id": -100, "type": "supergroup"}, "message_thread_id": 9, "from": {"id": 7}}}]]

            async def fake_call(method: str, payload: dict, timeout: float = 15, token: str = ""):
                calls.append((method, token, payload))
                if method == "getUpdates":
                    if updates:
                        return updates.pop(0)
                    await asyncio.sleep(3600)
                return True
            tg = Telegram("NOTIFY", ((1, 0),), admin=42, commands=True, handler=taker.command, command_token="CMD")
            tg.call = fake_call
            task = asyncio.create_task(tg.run())
            for _ in range(200):
                await asyncio.sleep(0.01)
                if sum(1 for c in calls if c[0] == "sendMessage") >= 3:
                    break
            tg.send("🎯 规则吃单成交：…")  # a notification: through the notifying bot, to the configured chat
            await asyncio.sleep(1.0)
            tg.stop()
            await asyncio.wait_for(task, 10)
            self.assertEqual([c[0] for c in calls[:3]], ["getMe", "getWebhookInfo", "setMyCommands"])  # (who the bot is, first)
            self.assertTrue(all(c[1] == "CMD" for c in calls[:3]))
            sent = [(c[1], c[2]["chat_id"], c[2].get("message_thread_id"), c[2]["text"]) for c in calls if c[0] == "sendMessage"]
            self.assertTrue(any(tok == "CMD" and chat == 42 and "自检中" in text for tok, chat, _, text in sent))
            self.assertTrue(any(tok == "CMD" and chat == 42 and "规则吃单自检" in text for tok, chat, _, text in sent))
            self.assertTrue(any(tok == "CMD" and chat == -100 and thread == 9 and "你的用户 ID：7" in text for tok, chat, thread, text in sent))
            self.assertTrue(any(tok == "NOTIFY" and chat == 1 and "成交" in text for tok, chat, _, text in sent))
            self.assertEqual(taker.paused, "")  # the stranger's /pause did nothing
        run(go())


class TelegramIdFirstTest(unittest.TestCase):
    def test_id_answered_before_an_admin_is_set_nothing_else(self) -> None:
        from ruletaker.notify import Telegram

        async def go() -> None:
            taker, *_ = make_taker(START - 3_600_000)
            calls: list[tuple] = []
            updates = [[{"update_id": 1, "message": {"text": "/id", "chat": {"id": 5}, "from": {"id": 5}}},
                        {"update_id": 2, "message": {"text": "/pause", "chat": {"id": 5}, "from": {"id": 5}}}]]

            async def fake_call(method: str, payload: dict, timeout: float = 15, token: str = ""):
                calls.append((method, token, payload))
                if method == "getUpdates":
                    if updates:
                        return updates.pop(0)
                    await asyncio.sleep(3600)
                return True
            tg = Telegram("", (), admin=0, commands=True, handler=taker.command, command_token="CMD")
            tg.call = fake_call
            task = asyncio.create_task(tg.run())
            for _ in range(200):
                await asyncio.sleep(0.01)
                if any(c[0] == "sendMessage" for c in calls):
                    break
            await asyncio.sleep(0.5)
            tg.stop()
            await asyncio.wait_for(task, 10)
            sent = [c[2]["text"] for c in calls if c[0] == "sendMessage"]
            self.assertEqual(len(sent), 2)
            self.assertIn("你的用户 ID：5", sent[0])
            self.assertIn("只听管理员的命令。你的用户 ID 是 5", sent[1])  # (the /pause: not run, and why)
            self.assertEqual(taker.paused, "")
        run(go())


class TelegramMenuTest(unittest.TestCase):
    def test_menu_buttons_and_their_yes(self) -> None:
        import time as clock
        from ruletaker.notify import CONFIRM_S, Telegram

        async def go() -> None:
            taker, *_ = make_taker(START - 3_600_000)
            calls: list[tuple] = []

            async def fake_call(method: str, payload: dict, timeout: float = 15, token: str = ""):
                calls.append((method, payload))
                return True
            tg = Telegram("", (), admin=42, commands=True, handler=taker.command, command_token="CMD")
            tg.call, tg.menu_head = fake_call, taker.menu_head

            def last(method: str) -> dict:
                return next((p for m, p in reversed(calls) if m == method), {})

            async def tap(data: str, user: int = 42) -> None:
                tg.on_update({"update_id": 1, "callback_query": {"id": "q1", "data": data, "from": {"id": user},
                                                                 "message": {"message_id": 77, "chat": {"id": 42}}}})
                await asyncio.sleep(0.05)
                await tg.drain(1.0)
            tg.on_update({"update_id": 1, "message": {"text": "/menu", "chat": {"id": 42}, "from": {"id": 42}}})
            await tg.drain(1.0)
            card = last("sendMessage")
            self.assertIn("规则吃单菜单", card["text"])
            keys = json.dumps(card["reply_markup"], ensure_ascii=False)
            self.assertIn("✅ 真实吃单", keys)
            self.assertIn("c:/pause", keys)
            await tap("c:/pause")  # at once; its answer sent; the card brought up to date
            self.assertTrue(taker.paused)
            self.assertIn("已暂停", last("sendMessage")["text"])
            self.assertEqual(last("editMessageText")["message_id"], 77)
            await tap("c:/resume")  # starts real orders again: asked first
            ask = last("editMessageText")
            self.assertIn("确定要恢复开新单吗", ask["text"])
            self.assertTrue(taker.paused)
            await tap(f"y:{int(clock.time()) - CONFIRM_S - 5}:/resume")  # a "yes" older than a minute
            self.assertTrue(taker.paused)
            self.assertEqual(last("answerCallbackQuery")["text"], "确认已过期，请重新点一次")
            await tap(ask["reply_markup"]["inline_keyboard"][0][0]["callback_data"])  # the "yes" in time
            self.assertEqual(taker.paused, "")
            await tap("c:/mode on")
            self.assertIn("确定要切到真实吃单吗", last("editMessageText")["text"])
            await tap("c:/pause", user=9)  # not the admin
            self.assertEqual(taker.paused, "")
            self.assertEqual(last("answerCallbackQuery")["text"], "这个机器人只听管理员的")
            await tap("c:/mode paper")
            self.assertEqual(taker.settings.mode, "paper")
            await tap("c:/mode  on")  # made-up buttons: a disguised "live trading" or "resume" never gets past the "yes"
            await tap("y:9999999999:/mode on")
            self.assertEqual(taker.settings.mode, "paper")
            await tap("c:/pause")
            await tap("c:/resume ")
            self.assertTrue(taker.paused)
        run(go())

    def test_settings_and_review(self) -> None:
        from ruletaker.notify import Telegram

        async def go() -> None:
            taker, *_ = make_taker(START - 3_600_000)
            calls: list[tuple] = []

            async def fake_call(method: str, payload: dict, timeout: float = 15, token: str = ""):
                calls.append((method, payload))
                return True
            tg = Telegram("", (), admin=42, commands=True, handler=taker.command, command_token="CMD")
            tg.call, tg.settings_view = fake_call, taker.settings_view

            def last(method: str) -> dict:
                return next((p for m, p in reversed(calls) if m == method), {})

            async def tap(data: str) -> None:
                tg.on_update({"update_id": 1, "callback_query": {"id": "q1", "data": data, "from": {"id": 42},
                                                                 "message": {"message_id": 78, "chat": {"id": 42}}}})
                await asyncio.sleep(0.05)
                await tg.drain(1.0)
            tg.on_update({"update_id": 1, "message": {"text": "/settings", "chat": {"id": 42}, "from": {"id": 42}}})
            await tg.drain(1.0)
            self.assertIn("⚙️ 设置", last("sendMessage")["text"])
            self.assertIn("f:RULE_MAX_DAILY_LOSS_USD", json.dumps(last("sendMessage")["reply_markup"]))
            await tap("f:RULE_MAX_DAILY_LOSS_USD")  # a field: what it does, a button a preset
            self.assertIn("今日亏损停机线", last("editMessageText")["text"])
            self.assertIn("现在：不限", last("editMessageText")["text"])
            self.assertIn("s:RULE_MAX_DAILY_LOSS_USD:50", json.dumps(last("editMessageText")["reply_markup"]))
            await tap("s:RULE_MAX_DAILY_LOSS_USD:50")  # in force, said, ticked
            self.assertEqual(taker.loss_cap, 50)
            self.assertIn("✅ 已保存", last("sendMessage")["text"])
            self.assertIn("✅ $50", json.dumps(last("editMessageText")["reply_markup"], ensure_ascii=False))
            await tap("s:RULE_MAX_DAILY_LOSS_USD:999999")  # made-up buttons: nothing changes
            await tap("s:PREDICT_PRIVATE_KEY:0x00")
            self.assertEqual(taker.loss_cap, 50)
            self.assertEqual(last("answerCallbackQuery")["text"], "这个按钮已经过时，重新打开设置")
            await tap("r:RULE_MAX_DAILY_LOSS_USD")  # back to the settings file's value
            self.assertEqual(taker.loss_cap, 0)
            self.assertIn("✅", await taker.command("/set", "RULE_GOLD_SHARES 120"))  # typed: any value the file would take
            self.assertEqual(taker.settings.shares_of("gold"), 120)
            self.assertIn("❌ 没改", await taker.command("/set", "NOPE 1"))
            self.assertIn("用法", await taker.command("/set", "RULE_GOLD_SHARES"))
            self.assertIn("✅", await taker.command("/reset", "RULE_GOLD_SHARES"))
            self.assertIn("RULE_GOLD_SHARES", await taker.command("/settings", ""))
            await tap("v:30")  # the review
            self.assertIn("📈 复盘（近 30 天", last("sendMessage")["text"])
            taker.recs["a"] = rec("xau-updown-15m-1", "on", START - 86_400_000, 50, 0.56, 21.2)
            text = await taker.command("/review", "all")
            self.assertIn("📈 复盘（全部", text)
            self.assertIn("按规则：", text)
            self.assertIn("+$21.20", text)
        run(go())

    def test_says_why_the_bot_is_silent(self) -> None:
        from ruletaker.notify import Telegram
        from ruletaker.util import RemoteError

        async def go() -> None:
            self.assertIn("TELEGRAM_COMMANDS 不是 on", Telegram("", (), admin=42, commands=False, command_token="CMD").health()[1])

            def bot(answers: dict) -> Telegram:
                tg = Telegram("", (), admin=42, commands=True, command_token="CMD")

                async def fake_call(method: str, payload: dict, timeout: float = 15, token: str = ""):
                    got = answers.get(method, True)
                    if isinstance(got, Exception):
                        raise got
                    if method == "getUpdates":
                        await asyncio.sleep(0.01)
                        return []
                    return got
                tg.call = fake_call
                return tg

            async def run_a_moment(tg: Telegram) -> tuple:
                task = asyncio.create_task(tg.run())
                await asyncio.sleep(0.1)
                tg.stop()
                await asyncio.wait_for(task, 10)
                return tg.health()
            fine, text = await run_a_moment(bot({"getMe": RemoteError("HTTP 401: Unauthorized")}))
            self.assertFalse(fine)
            self.assertIn("Token 不对", text)
            fine, text = await run_a_moment(bot({"getMe": {"username": "rt_tokyo_bot"},
                                                 "getWebhookInfo": {"url": "https://other.example.com/hook/secret"},
                                                 "getUpdates": RemoteError("HTTP 409: Conflict: can't use getUpdates method while webhook is active")}))
            self.assertIn("@rt_tokyo_bot", text)
            self.assertIn("webhook（other.example.com）", text)
            self.assertNotIn("secret", text)
            # a command from someone not the admin (the admin's id mistyped): they are told their id; the self-check says it
            tg = bot({})
            tg.bot_name = "@rt_tokyo_bot"
            tg.on_update({"update_id": 1, "message": {"text": "/status", "chat": {"id": 9}, "from": {"id": 9}}})
            sent: list = []

            async def record(method: str, payload: dict, timeout: float = 15, token: str = ""):
                sent.append(payload)
                return True
            tg.call = record
            await tg.drain(1.0)
            self.assertIn("你的用户 ID 是 9", sent[-1]["text"])
            self.assertIn("用户 9 发过 /status", tg.health()[1])
            taker, *_ = make_taker(START - 3_600_000)
            taker.telegram = tg
            self.assertIn("Telegram：用户 9 发过", await taker.self_check())
            nobody = Telegram("", (), admin=0, commands=True, command_token="CMD")
            nobody.bot_name = "@rt_tokyo_bot"
            self.assertIn("ADMIN_USER_ID 没填", nobody.health()[1])
        run(go())


class TelegramBindingTest(unittest.TestCase):
    def test_one_bot_for_both_or_one_each(self) -> None:
        from fakes import make_config
        from ruletaker.__main__ import make_telegram

        async def handler(command: str, args: str) -> str:
            return ""
        # one new bot, its token under TELEGRAM_COMMAND_TOKEN alone: the notifications go through it too, to the admin's chat
        one = make_telegram(make_config(TELEGRAM_COMMAND_TOKEN="CMD", TELEGRAM_COMMANDS="on", ADMIN_USER_ID="42"), handler)
        self.assertEqual((one.token, one.cmd_token, one.enabled, one.chats), ("CMD", "CMD", True, ((42, 0),)))
        # Railway's bot keeps the notifications (to a group topic), the new bot takes the commands
        two = make_telegram(make_config(TELEGRAM_BOT_TOKEN="NOTIFY", TELEGRAM_CHAT_ID="-1001:7", TELEGRAM_COMMAND_TOKEN="CMD",
                                        TELEGRAM_COMMANDS="on", ADMIN_USER_ID="42"), handler)
        self.assertEqual((two.token, two.cmd_token, two.enabled, two.chats), ("NOTIFY", "CMD", True, ((-1001, 7),)))
        # nothing set: no Telegram at all
        none = make_telegram(make_config(), handler)
        self.assertFalse(none.enabled)


class InPlayFromThePageTest(unittest.TestCase):
    def test_an_in_play_price_set_on_the_page_works_without_a_restart(self) -> None:
        from decimal import Decimal as D
        from ruletaker.index import IndexFeed
        from ruletaker.rules import RULE_OF
        from test_taker import SLUG, found

        async def go() -> None:
            taker, api, chain, stream, clock = make_taker(START - 60_000)
            taker.index = feed = IndexFeed(["XAUUSDT"])   # (the service makes the feed at its start, whatever the prices are then)
            feed.wanted, feed.needed = taker.index_wanted, taker.index_needed
            self.assertFalse(taker.index_wanted())        # no in-play price yet: the feed keeps no connection
            self.assertIsNone(taker.state()["index"])
            await taker.save_settings({"RULE_GOLD_SHUT_LIVE_CENTS": "80"})
            self.assertTrue(taker.index_wanted())
            self.assertIsNotNone(taker.state()["index"])
            await found(taker, api)
            clock.ms = START + 90_000
            feed.candles[("XAUUSDT", START - 60_000)] = D("4000")
            feed.push_index("XAUUSDT", D("4000.4"), clock.ms - 50, clock.ms)
            self.assertEqual(taker.live_guard(SLUG, RULE_OF["gold_shut"], clock.ms), "")  # judged on the feed: ahead by 0.01%
        run(go())
