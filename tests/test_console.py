"""The control page and the review page: saved settings (checked whole, in force at once, kept over a restart, dropped when the
settings file no longer agrees), the mode's ceiling, the API's routes and its header check, the review's sums, the CSV."""
from __future__ import annotations

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
            await again.reset_settings([])
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


if __name__ == "__main__":
    unittest.main()
