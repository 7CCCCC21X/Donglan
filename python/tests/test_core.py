"""The windows, the calendar, the book's arithmetic, the settings and the signing (offline)."""
from __future__ import annotations

import datetime as dt
import os
import tempfile
import unittest
from decimal import Decimal as D

import fakes  # noqa: F401  (puts the repository on the path)
from ruletaker.cards import CARDS, window_title_text
from ruletaker.clock import cme_shuts, day_spans, day_spans_text, et_parts, in_spans, spans_in, us_eastern_offset
from ruletaker.config import Config, RuleSettings, allowed, load_env_file
from ruletaker.rules import (RULE_OF, cost, fee_share, filled, held, net, offered, spent_on, walk, windows, without, claims,
                             daily_reopen)
from ruletaker.util import bj, day_of

UTC = dt.timezone.utc


def ms(y: int, mo: int, d: int, h: int, mi: int = 0) -> int:
    return int(dt.datetime(y, mo, d, h, mi, tzinfo=UTC).timestamp() * 1000)


class ClockTest(unittest.TestCase):
    def test_eastern_offset_follows_dst(self) -> None:
        self.assertEqual(us_eastern_offset(ms(2026, 7, 1, 12)), -4)
        self.assertEqual(us_eastern_offset(ms(2026, 11, 18, 12)), -5)
        self.assertEqual(us_eastern_offset(ms(2026, 3, 8, 6, 59)), -5)   # 01:59 EST
        self.assertEqual(us_eastern_offset(ms(2026, 3, 8, 7, 0)), -4)    # 03:00 EDT
        self.assertEqual(us_eastern_offset(ms(2026, 11, 1, 5, 59)), -4)
        self.assertEqual(us_eastern_offset(ms(2026, 11, 1, 6, 0)), -5)

    def test_shut_windows_winter_and_summer(self) -> None:
        rule = RULE_OF["gold_shut"]
        # winter: Wednesday 2026-11-18 17:00 ET = Thursday 06:00 Beijing
        starts = windows(rule, ms(2026, 11, 18, 0), ms(2026, 11, 19, 0))
        self.assertEqual([bj(s) for s in starts], ["11-19 06:00", "11-19 06:15", "11-19 06:30", "11-19 06:45"])
        # summer: 2026-07-15 (Wednesday) 17:00 EDT = 05:00 Beijing
        starts = windows(rule, ms(2026, 7, 15, 0), ms(2026, 7, 16, 0))
        self.assertEqual([bj(s) for s in starts], ["07-16 05:00", "07-16 05:15", "07-16 05:30", "07-16 05:45"])

    def test_no_shut_rule_on_friday_or_weekend(self) -> None:
        rule = RULE_OF["gold_shut"]
        # Friday 2026-11-20 17:00 ET starts the weekend: not a daily break
        self.assertEqual(windows(rule, ms(2026, 11, 20, 12), ms(2026, 11, 23, 12)), [])

    def test_reopen_first_window(self) -> None:
        rule = RULE_OF["gold_open"]
        self.assertEqual([bj(s) for s in windows(rule, ms(2026, 11, 18, 0), ms(2026, 11, 19, 0))], ["11-19 07:00"])
        # Sunday's reopen (Monday morning Beijing) is not one, nor a holiday's (Thanksgiving's early halt: shut from noon)
        sunday = ms(2026, 11, 22, 23)  # 18:00 EST Sunday
        self.assertFalse(daily_reopen(sunday))
        thanksgiving = ms(2026, 11, 26, 23)  # 18:00 EST, after an early halt from 12:00
        self.assertFalse(daily_reopen(thanksgiving))

    def test_0515_slot_and_holidays(self) -> None:
        rule = RULE_OF["gold_0515"]
        starts = windows(rule, ms(2026, 11, 18, 0), ms(2026, 11, 20, 0))
        self.assertEqual([bj(s) for s in starts], ["11-18 18:15", "11-19 18:15"])
        # Christmas: no session at all
        self.assertEqual(windows(rule, ms(2026, 12, 25, 0), ms(2026, 12, 26, 0)), [])

    def test_cme_shuts(self) -> None:
        shuts = cme_shuts(ms(2026, 11, 18, 21), ms(2026, 11, 19, 0))
        self.assertEqual(shuts, [(ms(2026, 11, 18, 22), ms(2026, 11, 18, 23))])
        weekday, hour, minute = et_parts(ms(2026, 11, 18, 22, 15))
        self.assertEqual((weekday, hour, minute), (2, 17, 15))

    def test_day_spans(self) -> None:
        # spaces around the dash: one span; spans set apart by spaces: as many (as src/clock.rs reads them)
        self.assertEqual(day_spans("01:00 - 07:00"), ((60, 420),))
        self.assertEqual(day_spans(" 01:00 至  07:00 ， 23:00 -02:00 09:00-10:00"), ((60, 420), (1380, 120), (540, 600)))
        spans = day_spans("01:00-07:00,23:00-02:00")
        self.assertEqual(spans, ((60, 420), (1380, 120)))
        self.assertEqual(day_spans_text(spans), "01:00-07:00,23:00-02:00")
        self.assertEqual(day_spans("off"), ())
        with self.assertRaises(ValueError):
            day_spans("25:00-26:00")
        one = day_spans("01:00-07:00")
        at = ms(2026, 11, 18, 17, 30)  # 01:30 Beijing
        self.assertTrue(in_spans(one, at))
        self.assertEqual(spans_in(one, at), 0)
        self.assertEqual(spans_in(one, ms(2026, 11, 18, 16, 59)), 60_000)  # 00:59 → a minute to go

    def test_window_title(self) -> None:
        start = ms(2026, 11, 18, 22)  # 17:00 EST
        self.assertEqual(window_title_text("Gold Up or Down - Nov 18, 5:00PM-5:15PM ET", start), "")
        self.assertIn("对不上", window_title_text("Gold Up or Down - Nov 18, 5:15PM-5:30PM ET", start))
        self.assertEqual(window_title_text("no time named", start), "")
        card = CARDS["XAU-15M"]
        self.assertEqual(card.slug_of(start), f"xau-updown-15m-{start // 1000}")
        self.assertEqual(card.start_of(card.slug_of(start)), start)
        self.assertEqual(card.name_of(start), "黄金 15分涨跌 06:00–06:15")


class BookTest(unittest.TestCase):
    def test_offered_up_and_down(self) -> None:
        asks = ((D("0.55"), D("10")), (D("0.58"), D("20")), (D("0.60"), D("5")))
        bids = ((D("0.48"), D("7")), (D("0.45"), D("9")))
        self.assertEqual(offered((), asks, "up", D("0.58")), [(D("0.55"), D("10")), (D("0.58"), D("20"))])
        # Down at 1 − bid: 0.52 (7) and 0.55 (9)
        self.assertEqual(offered(bids, (), "down", D("0.53")), [(D("0.52"), D("7"))])
        self.assertEqual(offered(bids, (), "down", D("0.55")), [(D("0.52"), D("7")), (D("0.55"), D("9"))])

    def test_without_and_walk(self) -> None:
        levels = [(D("0.50"), D("10")), (D("0.55"), D("10"))]
        self.assertEqual(without(levels, D("12")), [(D("0.55"), D("8"))])
        self.assertEqual(walk(levels, D("15")), (D("0.50") * 10 + D("0.55") * 5) / 15)

    def test_fee_and_sums(self) -> None:
        self.assertAlmostEqual(fee_share(0.58, 200), 0.02 * 0.42 / 0.58)
        rec = {"orders": [{"state": "final", "shares": 50, "filled": 30, "limit": 0.58, "avg": 0.56, "fee_bps": 200, "at": ms(2026, 11, 18, 17)},
                          {"state": "open", "shares": 20, "filled": 5, "limit": 0.58, "fee_bps": 200, "at": ms(2026, 11, 18, 17)}]}
        self.assertEqual(filled(rec), 35)
        self.assertAlmostEqual(cost(rec), 30 * 0.56 + 5 * 0.58)
        self.assertAlmostEqual(net(rec), 30 * (1 - fee_share(0.56, 200)) + 5 * (1 - fee_share(0.58, 200)))
        self.assertEqual(held(rec), 30 + 20)        # the open order counted whole
        self.assertEqual(claims(rec), 15)           # it may still take 15
        recs = {"a": {**rec, "mode": "on"}}
        self.assertAlmostEqual(spent_on(recs, day_of(ms(2026, 11, 18, 17)), "on"), 30 * 0.56 + 20 * 0.58)


class ConfigTest(unittest.TestCase):
    def test_defaults_and_validation(self) -> None:
        c = Config.from_env({})
        self.assertEqual(c.rules.mode, "off")
        self.assertEqual(c.rules.price("gold_shut"), 58)
        self.assertEqual(c.rules.price("oil_shut"), 0)
        self.assertEqual(c.rules.shares_of("gold"), 50)
        self.assertTrue(c.trading_allowed)
        with self.assertRaises(ValueError):
            Config.from_env({"RULE_TAKER": "on"})  # no key
        with self.assertRaises(ValueError):
            Config.from_env({"RULE_GOLD_SHUT_CENTS": "99"})
        with self.assertRaises(ValueError):
            Config.from_env({"STATUS_HOST": "0.0.0.0"})  # public without a key
        c = Config.from_env({"RULE_TAKER": "on", "PREDICT_API_KEY": "k", "PREDICT_PRIVATE_KEY": "4c" * 32, "RULE_API_KEY": "own",
                             "TELEGRAM_CHAT_ID": "-100123:45,678"})
        self.assertEqual(c.api_key, "own")
        self.assertTrue(c.own_key)
        self.assertEqual(c.telegram_chats, ((-100123, 45), (678, 0)))
        self.assertTrue(c.private_key.startswith("0x"))

    def test_trading_switch(self) -> None:
        self.assertTrue(allowed(None))
        self.assertTrue(allowed("on"))
        self.assertFalse(allowed(""))
        self.assertFalse(allowed("off"))
        self.assertFalse(allowed("关"))

    def test_env_file(self) -> None:
        with tempfile.NamedTemporaryFile("w", suffix=".env", delete=False, encoding="utf-8") as f:
            f.write("# comment\nRULE_TAKER=paper\nRULE_GOLD_SHARES=80   # a comment after the value\nexport PREDICT_API_KEY='abc'\n")
        try:
            env = load_env_file(f.name, {"RULE_TAKER": "off"})
            self.assertEqual(env["RULE_TAKER"], "off")  # the process environment wins
            self.assertEqual(env["RULE_GOLD_SHARES"], "80")
            self.assertEqual(env["PREDICT_API_KEY"], "abc")
        finally:
            os.unlink(f.name)

    def test_fast_hours(self) -> None:
        s = RuleSettings.parse({"RULE_FAST_HOURS": "01:00-07:00"}, "paper")
        self.assertEqual(s.fast_in(ms(2026, 11, 18, 16, 0)), 3_600_000)  # 00:00 Beijing: an hour to go


class WalletTest(unittest.TestCase):
    def test_fast_signing_matches_generic(self) -> None:
        from ruletaker.wallet import Wallet, build_order, limit_amounts, to_wei
        for account in ("", "0x1234567890abcdef1234567890abcdef12345678"):
            w = Wallet(fakes.KEY, 56, account)
            a = limit_amounts(True, to_wei("0.58"), to_wei(300))
            order = build_order(w, fakes.TOKEN_UP, a["maker"], a["taker"], 200, 1795000000, salt=987654)
            for neg in (False, True):
                for yb in (False, True):
                    self.assertEqual(w.sign_order(order, neg, yb), w.sign_order_generic(order, neg, yb))

    def test_signature_recovers_signer(self) -> None:
        from eth_account import Account
        from ruletaker.wallet import Wallet, build_order, limit_amounts, to_wei
        w = Wallet(fakes.KEY, 56)
        a = limit_amounts(True, to_wei("0.5"), to_wei(10))
        order = build_order(w, fakes.TOKEN_DOWN, a["maker"], a["taker"], 200, 1795000000, salt=1)
        digest, signature = w.sign_order(order, False, False)
        signer = Account._recover_hash(bytes.fromhex(digest[2:]), signature=bytes.fromhex(signature[2:]))
        self.assertEqual(signer, w.signer)

    def test_amounts(self) -> None:
        from ruletaker.wallet import limit_amounts, to_wei
        a = limit_amounts(True, to_wei("0.58"), to_wei(50))
        self.assertEqual(a["price_per_share"], 58 * 10 ** 16)
        self.assertEqual(a["taker"], 50 * 10 ** 18)
        self.assertEqual(a["maker"], 29 * 10 ** 18)
        self.assertEqual(limit_amounts(True, to_wei("0.869999999999"), to_wei(1))["price_per_share"], 87 * 10 ** 16)


if __name__ == "__main__":
    unittest.main()


class LockTest(unittest.TestCase):
    """What the VM installs (requirements.lock, every file by its hash) is what requirements.txt asks for."""

    def test_lock_pins_every_direct_dependency_with_hashes(self) -> None:
        import re
        root = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
        with open(os.path.join(root, "requirements.txt"), encoding="utf-8") as f:
            wanted = dict(re.findall(r"^([A-Za-z0-9_.-]+)==([^\s#]+)", f.read(), re.M))
        with open(os.path.join(root, "requirements.lock"), encoding="utf-8") as f:
            lock = f.read()
        locked = dict(re.findall(r"^([A-Za-z0-9_.-]+)==([^\s\\]+)", lock, re.M))
        self.assertTrue(wanted)
        for name, version in wanted.items():
            self.assertEqual(locked.get(name), version, f"{name}: requirements.lock 没跟上 requirements.txt（按 requirements.txt 顶上的命令重新生成）")
        # every pinned package carries at least one sha256, and nothing in the lock is unpinned
        blocks = re.split(r"\n(?=[A-Za-z0-9_.-]+==)", lock.strip())
        self.assertEqual(len(blocks), len(locked))
        for block in blocks:
            self.assertRegex(block, r"--hash=sha256:[0-9a-f]{64}", block.splitlines()[0])

class LogRedactTest(unittest.TestCase):
    def test_a_log_line_s_url_is_kept_to_its_host(self) -> None:
        from ruletaker.util import LOG, redact_log
        # a node's key in its path never written (as src/util.rs clean_error_keep writes a line)
        self.assertEqual(redact_log("节点 https://bsc.example:8545/v2/SUPER_SECRET_KEY 慢"), "节点 https://bsc.example:8545/[…] 慢")
        self.assertEqual(redact_log("https://user:pw@rpc.example/"), "[URL]")
        self.assertEqual(redact_log("https://rpc.example/?apikey=abc"), "[URL]")
        self.assertEqual(redact_log("POST https://api.telegram.org/bot1234567:ABCDEFGHIJKLMNOPQRS/sendMessage"), "POST [URL]")
        self.assertEqual(redact_log("控制台：http://127.0.0.1:8080/"), "控制台：http://127.0.0.1:8080/")
        self.assertEqual(redact_log("状态：http://0.0.0.0:8080/status"), "状态：http://0.0.0.0:8080/status")
        self.assertEqual(redact_log("http://localhost:8080/status 和 https://api.predict.fun/"),
                         "http://localhost:8080/status 和 https://api.predict.fun/")
        with self.assertLogs(LOG, "WARNING") as seen:
            LOG.warning("读不到 %s", "https://bsc.example/v2/SUPER_SECRET_KEY")
        self.assertNotIn("SUPER_SECRET_KEY", "\n".join(seen.output))
