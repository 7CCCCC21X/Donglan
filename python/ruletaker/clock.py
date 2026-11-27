"""New York time and CME Globex's shut stretches (a port of huice main.py: us_eastern_offset, et_wall_ms, cme_shuts),
and the Beijing-day spans RULE_FAST_HOURS is written in (day_spans).

Gold and oil trade on CME Globex's hours — Sunday 18:00 to Friday 17:00 New York time, shut an hour from 17:00 every day —
and the rules' windows are set in New York time from that schedule, so they follow daylight saving: gold's shut quarter
hours are 06:00–06:45 Beijing in winter, 05:00–05:45 in summer. No tz database is needed (slim images often lack one)."""
from __future__ import annotations

import datetime as dt
import functools
import re
from typing import Iterable

from .util import DAY_MS

# CME Globex holiday schedules (huice 1.151.0; override with CME_EARLY_DAYS / CME_CLOSED_DAYS): an early halt of metals and
# energy (the US holidays CME trades through, the day after Thanksgiving, the eves of Christmas and New Year — 12:15 to 14:45
# New York time by product and year, fixed about two weeks ahead: taken as shut from noon), and the days with no session
# (Christmas, New Year's Day, Good Friday). Listed wide on purpose: a day too many costs an afternoon, a day missing trades a
# standing index.
DEFAULT_CME_EARLY = ("2026-11-26,2026-11-27,2026-12-24,2026-12-31,2027-01-18,2027-02-15,2027-05-31,2027-06-18,2027-07-05,"
                     "2027-09-06,2027-11-25,2027-11-26")
DEFAULT_CME_CLOSED = "2026-12-25,2027-01-01,2027-03-26,2027-12-24"
CME_SHUT_HOUR, CME_REOPEN_HOUR = 17, 18  # New York wall clock: shut at 17:00, reopened at 18:00 (Friday's shut to Sunday's 18:00)
CME_EARLY_SHUT_HOUR = 12
UNIX_ORDINAL = dt.date(1970, 1, 1).toordinal()


def us_eastern_offset(ms: int) -> int:
    """US Eastern UTC offset at an instant: −4 (EDT) from the second Sunday of March 02:00 EST to the first Sunday of
    November 02:00 EDT, else −5 (EST)."""
    utc = dt.datetime.fromtimestamp(ms / 1000, dt.timezone.utc)
    sunday = lambda day: day + dt.timedelta(days=(6 - day.weekday()) % 7)  # the first Sunday on or after day
    starts = dt.datetime.combine(sunday(dt.date(utc.year, 3, 8)), dt.time(7), dt.timezone.utc)    # 02:00 EST
    ends = dt.datetime.combine(sunday(dt.date(utc.year, 11, 1)), dt.time(6), dt.timezone.utc)     # 02:00 EDT
    return -4 if starts <= utc < ends else -5


def et_ms(year: int, month: int, day: int, hour: int, minute: int, utc_offset: int) -> int:
    """US Eastern wall time -> epoch ms; utc_offset is -4 (EDT) or -5 (EST)."""
    return int(dt.datetime(year, month, day, hour, minute, tzinfo=dt.timezone(dt.timedelta(hours=utc_offset))).timestamp() * 1000)


def et_wall_ms(day: dt.date, hour: int, minute: int) -> int:
    """US Eastern wall time on a date -> epoch ms, with that date's own offset (DST switches at 02:00; 17:00 UTC is the same
    civil day either side of the switch)."""
    probe = int(dt.datetime(day.year, day.month, day.day, 17, tzinfo=dt.timezone.utc).timestamp() * 1000)
    return et_ms(day.year, day.month, day.day, hour, minute, us_eastern_offset(probe))


def et_date(ms: int) -> dt.date:
    return dt.datetime.fromtimestamp(ms / 1000, dt.timezone(dt.timedelta(hours=us_eastern_offset(ms)))).date()


def et_parts(ms: int) -> tuple[int, int, int]:
    """(weekday — Monday 0, hour, minute) of an instant in New York, daylight saving followed."""
    at = dt.datetime.fromtimestamp(ms / 1000 + us_eastern_offset(ms) * 3600, dt.timezone.utc)
    return at.weekday(), at.hour, at.minute


def et_clock(ms: int) -> str:
    """'17:45 ET' of an instant."""
    at = dt.datetime.fromtimestamp(ms / 1000, dt.timezone(dt.timedelta(hours=us_eastern_offset(ms))))
    return f"{at:%H:%M} ET"


def parse_dates(spec: str, label: str) -> frozenset:
    """'2026-12-25,2027-01-01' or spans '2026-10-01..2026-10-07' -> the dates; a span written backwards is read the way it means."""
    days: set[dt.date] = set()
    for item in str(spec or "").split(","):
        item = item.strip()
        if not item:
            continue
        start, _, end = item.partition("..")
        try:
            first, last = dt.date.fromisoformat(start.strip()), dt.date.fromisoformat((end or start).strip())
        except ValueError:
            raise ValueError(f"{label} 日期格式应为 YYYY-MM-DD 或 YYYY-MM-DD..YYYY-MM-DD：{item}") from None
        if first > last:
            first, last = last, first
        while first <= last:
            days.add(first)
            first += dt.timedelta(days=1)
    return frozenset(days)


class Calendar:
    """CME's holiday schedules in force (configure: CME_EARLY_DAYS / CME_CLOSED_DAYS)."""

    def __init__(self) -> None:
        self.cme_early: frozenset = parse_dates(DEFAULT_CME_EARLY, "CME_EARLY_DAYS")
        self.cme_closed: frozenset = parse_dates(DEFAULT_CME_CLOSED, "CME_CLOSED_DAYS")

    def configure(self, early: frozenset | None = None, closed: frozenset | None = None) -> None:
        if early is not None:
            self.cme_early = frozenset(early)
        if closed is not None:
            self.cme_closed = frozenset(closed)

    def last_day(self) -> dt.date | None:
        days = self.cme_early | self.cme_closed
        return max(days) if days else None


CALENDAR = Calendar()


@functools.lru_cache(maxsize=512)
def cme_day_shuts(ordinal: int, early: bool, closed: bool) -> tuple[tuple[int, int], ...]:
    """The shut stretches the New York date of this ordinal brings, (shut, reopened) in ms: its daily break 17:00–18:00 (a
    Friday's to Sunday 18:00); on a holiday schedule's early halt (``early``) from CME_EARLY_SHUT_HOUR; on a day without a
    session (``closed``) the session it would have had, from 18:00 the evening before. None on a Saturday or Sunday."""
    day = dt.date.fromordinal(ordinal)
    if day.weekday() > 4:
        return ()
    close = et_wall_ms(day, CME_SHUT_HOUR, 0)
    out = [(close, et_wall_ms(day + dt.timedelta(days=2 if day.weekday() == 4 else 0), CME_REOPEN_HOUR, 0))]
    if early:
        out.append((et_wall_ms(day, CME_EARLY_SHUT_HOUR, 0), close))
    if closed:
        out.append((et_wall_ms(day - dt.timedelta(days=1), CME_REOPEN_HOUR, 0), close))
    return tuple(out)


def merge_stretches(stretches: Iterable[tuple[int, int]]) -> tuple[tuple[int, int], ...]:
    out: list[tuple[int, int]] = []
    for a, b in sorted(stretches):
        if out and a <= out[-1][1]:
            out[-1] = (out[-1][0], max(out[-1][1], b))
        else:
            out.append((a, b))
    return tuple(out)


@functools.lru_cache(maxsize=512)
def cme_stretches(first: int, last: int, early: frozenset, closed: frozenset) -> tuple[tuple[int, int], ...]:
    return merge_stretches(s for o in range(first, last + 1)
                           for s in cme_day_shuts(o, dt.date.fromordinal(o) in early, dt.date.fromordinal(o) in closed))


def cme_shuts(from_ms: int, to_ms: int) -> list[tuple[int, int]]:
    """CME Globex's shut stretches that overlap [from_ms, to_ms), as (shut, reopened) in ms: 17:00–18:00 New York time Monday
    to Thursday, Friday 17:00 to Sunday 18:00, and the holiday schedules' days, merged with them."""
    first = UNIX_ORDINAL + from_ms // DAY_MS - 5
    last = UNIX_ORDINAL + max(from_ms, to_ms - 1) // DAY_MS + 1
    return [s for s in cme_stretches(first, last, CALENDAR.cme_early, CALENDAR.cme_closed) if s[0] < to_ms and from_ms < s[1]]


def market_shuts(hours: str, from_ms: int, to_ms: int) -> list[tuple[int, int]]:
    """The shut stretches of a card's ``hours`` that overlap [from_ms, to_ms): "cme" (cme_shuts); none otherwise."""
    return cme_shuts(from_ms, to_ms) if hours == "cme" else []


# --- the Beijing-day spans of RULE_FAST_HOURS --------------------------------------------------------------------------------
def day_spans(text: str, name: str = "RULE_FAST_HOURS") -> tuple[tuple[int, int], ...]:
    """Spans of the Beijing day, as minutes after midnight: "01:00-07:00" → ((60, 420),); several, comma-separated; one whose
    end is before its beginning runs over midnight ("23:00-02:00"); "off" (or nothing): none."""
    text = str(text or "").strip().lower()
    if text in {"", "off", "0", "关"}:
        return ()
    out = []
    # (spaces around a span's dash taken out first — "01:00 - 07:00" is one span; spans may still be set apart by spaces)
    text = re.sub(r"\s*([-–—~～至到])\s*", r"\1", text)
    for part in re.split(r"[,，;；\s]+", text):
        if not part:
            continue
        got = re.fullmatch(r"(\d{1,2})(?:[:：](\d{2}))?\s*(?:-|–|—|~|～|至|到)\s*(\d{1,2})(?:[:：](\d{2}))?", part)
        if not got:
            raise ValueError(f"{name} 写成 01:00-07:00 这样（北京时间，可写多段用逗号分开；off = 不做），现在是“{part}”")
        a, b = int(got.group(1)) * 60 + int(got.group(2) or 0), int(got.group(3)) * 60 + int(got.group(4) or 0)
        b = b or 1440  # an end at 00:00 is the day's end
        if not (0 <= a < 1440 and 0 < b <= 1440 and int(got.group(2) or 0) < 60 and int(got.group(4) or 0) < 60) or a == b:
            raise ValueError(f"{name} 的时间要在 00:00～24:00 之间，开始和结束不能相同，现在是“{part}”")
        out.append((a, b))
    return tuple(out)


def day_spans_text(spans: tuple) -> str:
    clock = lambda minutes: f"{minutes // 60:02d}:{minutes % 60:02d}"
    return ",".join(f"{clock(a)}-{clock(b)}" for a, b in spans) or "off"


def in_spans(spans: tuple, now_ms: int) -> bool:
    tod = (now_ms + 8 * 3_600_000) % DAY_MS
    return any((a * 60_000 <= tod < b * 60_000) if a < b else (tod >= a * 60_000 or tod < b * 60_000) for a, b in spans)


def spans_in(spans: tuple, now_ms: int) -> int | None:
    """The ms until the spans next begin: 0 inside them, None when there are none."""
    if not spans:
        return None
    if in_spans(spans, now_ms):
        return 0
    tod = (now_ms + 8 * 3_600_000) % DAY_MS
    return min((a * 60_000 - tod) % DAY_MS for a, _ in spans)
