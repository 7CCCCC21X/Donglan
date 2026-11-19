"""The 15-minute up/down markets the rules buy (a port of huice main.py WindowUpDownSpec for XAU-15M and CL-15M).

"黄金 (XAU) 涨或跌 15分" (xau-updown-15m-<window start, Unix seconds>) and "CL 涨或跌 15分" (cl-updown-15m-…): Up when the
price at the end of the window is above the one at its start, Down when below — Chainlink's XAU/USDT and CL/USDT streams,
the index price (field i) of Binance's USDⓈ-M futures mark price stream. Predict lists a window about a day before its start."""
from __future__ import annotations

import dataclasses
import datetime as dt
import re

from .clock import us_eastern_offset
from .util import BEIJING


def window_title_text(question: str, start_ms: int) -> str:
    """A 15-minute market's question names its time range ("… 10:00PM-10:15PM ET"): when it does, it must start where the
    slug's time was read as the start. "" when it agrees or says no time."""
    found = re.search(r"(\d{1,2}):(\d{2})\s*([AP]M)?\s*[-–—~至到]\s*(\d{1,2}):(\d{2})\s*([AP]M)?\s*(?:ET|EST|EDT)\b", question, re.I)
    if not found:
        return ""
    h1, m1, half1, h2, m2, half2 = (int(found.group(1)), int(found.group(2)), (found.group(3) or "").upper(),
                                    int(found.group(4)), int(found.group(5)), (found.group(6) or "").upper())
    wall = lambda h, m, half: ((h % 12 + (12 if half == "PM" else 0)) if half else h) * 60 + m
    inherited = not half1 and bool(half2)  # "10:00-10:15PM": the half of the day written once, for both
    if inherited:
        half1 = half2
    start, end = wall(h1, m1, half1), wall(h2, m2, half2)
    if inherited and start >= end:  # "11:45-12:00PM" (an hour before noon), "11:45-12:00AM" (before midnight)
        half1 = "AM" if half1 == "PM" else "PM"
        start = wall(h1, m1, half1)
    et = dt.datetime.fromtimestamp(start_ms / 1000, dt.timezone(dt.timedelta(hours=us_eastern_offset(start_ms))))
    if start == et.hour * 60 + et.minute:
        return ""
    return f"Predict 市场写的时间段（{found.group(0)}）和按链接推算的窗口（{et:%H:%M} ET 起）对不上"


@dataclasses.dataclass(frozen=True)
class WindowCard:
    key: str          # XAU-15M
    stem: str         # xau -> xau-updown-15m-1793942100
    symbol: str       # the Binance USDⓈ-M futures pair whose index settles it
    coin: str         # 黄金
    minutes: int = 15
    hours: str = "cme"

    @property
    def window_ms(self) -> int:
        return self.minutes * 60_000

    @property
    def prefix(self) -> str:
        return f"{self.stem}-updown-{self.minutes}m-"

    def slug_of(self, start_ms: int) -> str:
        return f"{self.prefix}{start_ms // 1000}"

    def start_of(self, slug: str) -> int | None:
        suffix = slug.removeprefix(self.prefix)
        return int(suffix) * 1000 if slug.startswith(self.prefix) and suffix.isdigit() else None

    def first_start(self, ms: int) -> int:
        """The first window start at or after ``ms`` on the card's grid."""
        return -(-ms // self.window_ms) * self.window_ms

    def next_start(self, start_ms: int) -> int:
        return start_ms + self.window_ms

    def end_of(self, start_ms: int) -> int:
        return start_ms + self.window_ms

    def name_of(self, start_ms: int) -> str:
        hm = lambda ms: dt.datetime.fromtimestamp(ms / 1000, BEIJING).strftime("%H:%M")
        return f"{self.coin} {self.minutes}分涨跌 {hm(start_ms)}–{hm(self.end_of(start_ms))}"

    def title_problem(self, question: str, start_ms: int) -> str:
        return window_title_text(question, start_ms)


CARDS: dict[str, WindowCard] = {
    "XAU-15M": WindowCard("XAU-15M", "xau", "XAUUSDT", "黄金"),
    "CL-15M": WindowCard("CL-15M", "cl", "CLUSDT", "原油"),
}
