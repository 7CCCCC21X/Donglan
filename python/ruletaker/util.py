"""Small shared helpers: the clock zones, errors, the text forms, JSON (orjson when installed)."""
from __future__ import annotations

import datetime as dt
import json
import logging
import re
import time
from decimal import Decimal as D
from typing import Any

LOG = logging.getLogger("ruletaker")
BEIJING = dt.timezone(dt.timedelta(hours=8))
DAY_MS = 86_400_000
DUST = 1e-6

try:  # orjson parses a pushed book in a fraction of the stdlib's time (optional: requirements.txt lists it)
    import orjson as _orjson

    def loads(raw: Any) -> Any:
        return _orjson.loads(raw)

    def dumps_bytes(obj: Any) -> bytes:
        return _orjson.dumps(obj, default=str)
    FAST_JSON = True
except ImportError:  # pragma: no cover - depends on the environment
    def loads(raw: Any) -> Any:
        return json.loads(raw)

    def dumps_bytes(obj: Any) -> bytes:
        return json.dumps(obj, ensure_ascii=False, separators=(",", ":"), default=str).encode("utf-8")
    FAST_JSON = False


def dumps(obj: Any) -> str:
    return json.dumps(obj, ensure_ascii=False, separators=(",", ":"))


class RemoteError(Exception):
    """A request that failed: the HTTP status in its text ("HTTP 429: …"), the server's own retry-after when it gave one."""

    def __init__(self, message: str, retry_after: int = 0):
        super().__init__(message)
        self.retry_after = max(0, int(retry_after or 0))


class NotSent(RemoteError):
    """A local gate refused the request before its bytes were dispatched: nothing went out."""


class NotOnDisk(Exception):
    """An operator's change in force, but not yet on disk (the database took no write within the wait): never told as saved —
    the page shows the values in force (``applied``) and says what a restart before the write would bring back."""


def clean_error(error: BaseException | str) -> str:
    """An error's text for logs and messages: Telegram tokens and URLs taken out (a URL may carry a key), cut to 280."""
    text = str(error)
    text = re.sub(r"\b\d{5,}:[A-Za-z0-9_-]{15,}\b", "[TOKEN REDACTED]", text)
    text = re.sub(r"https?://\S+", "[URL]", text)
    return text[:280]


def brief_error(text: str, limit: int = 60) -> str:
    return text if len(text) <= limit else text[:limit - 1] + "…"


def http_error_text(body: Any) -> str:
    """The reason an API puts in an error body (description / msg / message / detail / error); "" when none is readable."""
    if not isinstance(body, dict):
        return ""
    for key in ("description", "msg", "message", "detail", "error"):
        value = body.get(key)
        if isinstance(value, dict):
            value = value.get("message") or value.get("msg") or value.get("detail") or ""
        if isinstance(value, list):
            value = "；".join(str(v) for v in value if v)
        if isinstance(value, str) and value.strip():
            return value.strip()[:200]
    return ""


def fmt(value: Any) -> str:
    value = D(str(value))
    return format(value, ",.8f").rstrip("0").rstrip(".")


def money(x: float) -> str:
    return ("+$" if x >= 0 else "−$") + f"{abs(x):,.2f}"


def bj(ms: int, day: bool = True) -> str:
    """Beijing time of an instant: 11-18 06:45 (or 06:45 with ``day`` False)."""
    return dt.datetime.fromtimestamp(ms / 1000, BEIJING).strftime("%m-%d %H:%M" if day else "%H:%M")


def bj_clock(ms: int) -> str:
    """Beijing time to the millisecond: 01:45:02.359 (the race's clock)."""
    return dt.datetime.fromtimestamp(ms / 1000, BEIJING).strftime("%H:%M:%S") + f".{int(ms) % 1000:03d}"


def day_of(ms: int) -> str:
    """The Beijing date of an instant, as the records keep it (2026-11-18)."""
    return dt.datetime.fromtimestamp(ms / 1000, BEIJING).date().isoformat()


def now_ms() -> int:
    return int(time.time() * 1000)


def env_bool(value: Any, default: bool = False) -> bool:
    text = str(value if value is not None else "").strip().lower()
    if not text:
        return default
    return text in {"1", "on", "true", "yes", "y", "开"}
