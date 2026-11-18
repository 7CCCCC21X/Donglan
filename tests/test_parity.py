"""The port against its source: huice's own functions and this package's, on the same inputs (skipped without a huice checkout:
HUICE_PATH, default ../huice)."""
from __future__ import annotations

import importlib
import os
import random
import sys
import unittest
from decimal import Decimal as D

import fakes

HUICE = os.environ.get("HUICE_PATH") or os.path.join(os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__)))), "huice")


def huice() -> tuple:
    if HUICE not in sys.path:
        sys.path.insert(0, HUICE)
    core = importlib.import_module("main")
    return core, importlib.import_module("rules"), importlib.import_module("live")


@unittest.skipUnless(os.path.isfile(os.path.join(HUICE, "rules.py")), "no huice checkout")
class ParityTest(unittest.TestCase):
    def test_windows_of_every_rule_for_a_year(self) -> None:
        core, old, _ = huice()
        from ruletaker import rules as new
        begin = 1_767_225_600_000  # 2026-01-01
        end = begin + 400 * 86_400_000
        for a, b in zip(old.RULES, new.RULES):
            self.assertEqual((a.key, a.side, a.market, a.card, a.lead), (b.key, b.side, b.market, b.card, b.lead))
            self.assertEqual(old.windows(a, begin, end), new.windows(b, begin, end), a.key)

    def test_cme_calendar(self) -> None:
        core, _, _ = huice()
        from ruletaker.clock import cme_shuts, us_eastern_offset
        begin = 1_767_225_600_000
        for day in range(0, 420, 3):
            at = begin + day * 86_400_000
            self.assertEqual(core.cme_shuts(at, at + 3 * 86_400_000), cme_shuts(at, at + 3 * 86_400_000))
            self.assertEqual(core.us_eastern_offset(at), us_eastern_offset(at))

    def test_book_and_money(self) -> None:
        _, old, live = huice()
        from ruletaker import rules as new
        from ruletaker import wallet
        rng = random.Random(7)
        for _ in range(300):
            asks = tuple(sorted((D(rng.randint(1, 99)) / 100, D(rng.randint(1, 500))) for _ in range(rng.randint(0, 6))))
            bids = tuple(sorted(((D(rng.randint(1, 99)) / 100, D(rng.randint(1, 500))) for _ in range(rng.randint(0, 6))), reverse=True))
            limit = D(rng.randint(1, 95)) / 100
            for side in ("up", "down"):
                self.assertEqual(old.offered(bids, asks, side, limit), new.offered(bids, asks, side, limit))
                lv = new.offered(bids, asks, side, limit)
                take = D(rng.randint(1, 300))
                self.assertEqual(old.walk(lv, take), new.walk(lv, take))
                self.assertEqual(old.without(lv, take), new.without(lv, take))
            price, qty = rng.randint(1, 99) * 10 ** 16 + rng.choice((0, 1, 10 ** 9 - 1)), rng.randint(1, 10 ** 6) * 10 ** 16
            a, b = live.limit_amounts(True, price, qty), wallet.limit_amounts(True, price, qty)
            self.assertEqual((a["price_per_share"], a["maker"], a["taker"]), (b["price_per_share"], b["maker"], b["taker"]))
            avg = rng.random()
            self.assertAlmostEqual(old.fee_share(avg, 200), new.fee_share(avg, 200))

    def test_orders_sign_the_same(self) -> None:
        _, _, live = huice()
        from ruletaker import wallet
        rng = random.Random(11)
        for account in ("", "0x1234567890abcdef1234567890abcdef12345678"):
            a, b = live.Wallet(fakes.KEY, 56, account), wallet.Wallet(fakes.KEY, 56, account)
            self.assertEqual(a.maker, b.maker)
            for _ in range(20):
                amounts = wallet.limit_amounts(True, rng.randint(1, 95) * 10 ** 16, rng.randint(1, 5000) * 10 ** 18)
                expires, salt, token = rng.randint(1_790_000_000, 1_800_000_000), rng.randint(0, wallet.MAX_SALT), str(rng.randint(1, 10 ** 30))
                o1 = live.build_order(a, token, amounts["maker"], amounts["taker"], 200, expires, salt=salt)
                o2 = wallet.build_order(b, token, amounts["maker"], amounts["taker"], 200, expires, salt=salt)
                self.assertEqual(o1, o2)
                neg, yb = rng.random() < 0.5, rng.random() < 0.5
                self.assertEqual(a.sign_order(o1, neg, yb), b.sign_order(o2, neg, yb))
            message = "Sign in to predict.fun: nonce 12345"
            self.assertEqual(a.sign_text(message), b.sign_text(message))

    def test_parsers(self) -> None:
        core, _, live = huice()
        from ruletaker import predict
        from ruletaker.cards import window_title_text
        rows = [["0.55", "10"], {"price": "0.5", "size": "3"}, ["1.2", "1"], ["0.4", "0"], "x"]
        for bids in (True, False):
            self.assertEqual(core.predict_levels(rows, bids), predict.predict_levels(rows, bids))
        for market in ({"outcomes": [{"name": "Up", "indexSet": 1, "status": "WON"}, {"name": "Down", "indexSet": 2}]},
                       {"outcomes": [{"name": "Up", "indexSet": 1, "payout": 0}, {"name": "Down", "indexSet": 2, "payout": 1}]},
                       {"status": "RESOLVED", "resolution": {"name": "Down"}, "outcomes": [{"name": "Up", "indexSet": 1}, {"name": "Down", "indexSet": 2}]},
                       {"status": "OPEN", "outcomes": [{"name": "Up", "indexSet": 1}, {"name": "Down", "indexSet": 2}]}):
            self.assertEqual(core.predict_resolution(market), predict.predict_resolution(market))
        row = {"status": "PARTIALLY_FILLED", "amount": str(50 * 10 ** 18), "amountFilled": str(20 * 10 ** 18), "averagePrice": "0.57"}
        self.assertEqual(live.order_fill(row, 50.0), predict.order_fill(row, 50.0))
        self.assertEqual(live.order_final(row, 50.0), predict.order_final(row, 50.0))
        self.assertEqual(live.order_price(row), predict.order_price(row))
        spec = next(c for c in core.WINDOW_UPDOWN_MARKETS if c.key == "XAU-15M")
        start = 1_795_039_200_000
        for q in ("Gold 5:00PM-5:15PM ET", "Gold 5:15PM-5:30PM ET", "11:45-12:00PM ET", "none"):
            self.assertEqual(bool(spec.title_problem(q, start)), bool(window_title_text(q, start)), q)
        from ruletaker.cards import CARDS
        self.assertEqual(spec.slug_of(start), CARDS["XAU-15M"].slug_of(start))

    def test_settings_parse_alike(self) -> None:
        core, _, _ = huice()
        from ruletaker.config import RuleSettings
        env = {"RULE_TAKER": "paper", "RULE_GOLD_SHUT_CENTS": "60", "RULE_OIL": "off", "RULE_GOLD_SHARES": "300", "RULE_MAX_DAY_USD": "500",
               "RULE_FAST_HOURS": "01:00-07:00,13:00-14:00", "RULE_LEAD_MINUTES": "300", "RULE_GOLD_SHUT_LIVE_CENTS": "80"}
        a, b = core.RuleSettings.from_env(env), RuleSettings.parse(env)
        self.assertEqual((a.mode, a.cents, a.markets, a.shares, a.max_day_usd, a.fast, a.live, a.lead_minutes),
                         (b.mode, b.cents, b.markets, b.shares, b.max_day_usd, b.fast, b.live, b.lead_minutes))


if __name__ == "__main__":
    unittest.main()
