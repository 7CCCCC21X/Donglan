"""HTTP with connections kept alive (a port of huice main.py _http_get / _http_keepalive).

Every request used to open its own TCP + TLS connection; connections are pooled per host and reused. One the server closed
meanwhile is retried once on a fresh one when nothing of the request was taken (for a GET also when no answer came); a POST —
an order — is never sent twice: an answer lost on the way is the caller's "result unknown", found by the order's hash.
TCP_NODELAY is set by http.client on every connection. Credentials (x-api-key, Authorization) go to the host the request was
made to, over https, and to no other (a redirect elsewhere drops them)."""
from __future__ import annotations

import collections
import contextlib
import http.client
import json
import select
import ssl
import threading
import time
import urllib.error
import urllib.parse
import urllib.request
import zlib
from typing import Any, Callable

from . import VERSION
from .util import NotSent, RemoteError, clean_error, http_error_text

USER_AGENT = f"RuleTaker/{VERSION}"
MAX_BODY = 8_000_000
SSL_CONTEXT = ssl.create_default_context()
KEEPALIVE_IDLE_SECONDS = 25.0       # a pooled connection unused this long is dropped
KEEPALIVE_POST_IDLE_SECONDS = 3.0   # a request with a body reuses only a connection used this recently (a stale one could swallow it)
KEEPALIVE_PER_HOST = 24
PREWARM_TIMEOUT_S = 5.0             # a parked connection's handshake given this long at the most
HTTPS_CONNECTION = http.client.HTTPSConnection  # (the tests put a fake here)
SECRET_HEADERS = frozenset({"authorization", "x-api-key", "cookie", "proxy-authorization"})
HTTP_HINTS = {451: "部署所在地或接口访问受限；请核对官方地区规则", 403: "访问被拒绝；请核对权限与服务地区",
              418: "接口暂时封禁；停止高频请求并等待解除", 429: "接口限流，等待后重试"}

NOT_CONNECTED = "连接没建立"  # how a request that never got a connection is told: nothing of it went out (an order may go again)
_POOL: dict[tuple[str, int], list[tuple[Any, float]]] = {}
_POOL_LOCK = threading.Lock()
HTTP_STATS = {"new": 0, "reused": 0, "retried": 0, "dead": 0}  # (changed under _POOL_LOCK: the requests run on several threads)
# host -> "connect": the last new connections' TCP + TLS handshakes in ms (two or three round trips to whatever answers for the
# host — a CDN's site near the machine, or the service itself); "edge": the last answer's cf-ray (its suffix names the Cloudflare
# site the request entered at)
HTTP_NET: dict[str, dict] = {}


def http_net(host: str) -> dict:
    with _POOL_LOCK:
        return HTTP_NET.setdefault(host, {"connect": collections.deque(maxlen=40), "edge": "", "limits": ""})


def _note_limits(host: str, headers: Any) -> None:
    """The answer's rate-limit headers, as sent ("x-ratelimit-limit: 600; …"), kept for the host when it had any."""
    found = [f"{k.lower()}: {str(v)[:40]}" for k, v in headers if "ratelimit" in k.lower() or "rate-limit" in k.lower()]
    if found:
        http_net(host)["limits"] = "; ".join(found)


def budget_of(limits: str) -> tuple[int, int] | None:
    """The key's budget as Predict's last answer gave it (``limits``: "ratelimit-limit: 5000; ratelimit-remaining: 1236; …", x-
    prefixed or not): (the limit, what is left of it). None without both."""
    limit = left = None
    for part in str(limits or "").split(";"):
        name, sep, value = part.partition(":")
        if not sep:
            continue  # (a policy's own "q=…;w=…" pieces)
        digits = ""
        for c in value.strip():
            if not c.isdigit():
                break
            digits += c
        if not digits:
            continue
        name = name.strip().removeprefix("x-")
        if name == "ratelimit-limit":
            limit = int(digits)
        elif name == "ratelimit-remaining":
            left = int(digits)
    return (limit, left) if limit is not None and left is not None else None


def _count(name: str) -> None:
    with _POOL_LOCK:
        HTTP_STATS[name] += 1


def _closed(conn: Any) -> bool:
    """Whether the server has closed a pooled connection meanwhile (no wait): nothing come in — good; bytes come in are read
    without waiting: TLS records of its own (a TLS 1.3 session ticket sent after the answer), nothing for us behind them — good;
    its end, a close_notify, an alert, or data no request asked for (a 408 before it hangs up) — closed."""
    sock = getattr(conn, "sock", None)
    if sock is None:
        return False  # (no socket held: http.client opens one for the request)
    try:
        pending = sock.pending() if isinstance(sock, ssl.SSLSocket) else 0
        if not pending and not select.select([sock], [], [], 0)[0]:
            return False
    except (OSError, ValueError):
        return True
    timeout = sock.gettimeout()
    try:
        sock.setblocking(False)
        sock.recv(1)
        return True  # (b"": the end or a close_notify; a byte: data nobody asked for)
    except (ssl.SSLWantReadError, BlockingIOError, InterruptedError):
        return False
    except (OSError, ValueError):
        return True
    finally:
        with contextlib.suppress(OSError, ValueError):
            sock.settimeout(timeout)


def _inflate(raw: bytes, encoding: str) -> bytes:
    encoding = (encoding or "").strip().lower()
    if encoding not in {"gzip", "x-gzip", "deflate"}:
        return raw
    try:
        out = zlib.decompressobj(zlib.MAX_WBITS | (16 if encoding != "deflate" else 0)).decompress(raw, MAX_BODY + 1)
    except zlib.error:
        return raw
    if len(out) > MAX_BODY:
        raise RemoteError("接口返回的数据过大")
    return out


def _pool_take(host: str, port: int, timeout: float, max_idle: float) -> tuple[Any, bool]:
    """The most recently used pooled connection to host:port, when used within ``max_idle`` seconds and not closed by the
    server meanwhile, else a new one (only the pooled ones idle past KEEPALIVE_IDLE_SECONDS are dropped on a miss: the others
    stay for the GETs)."""
    while True:
        with _POOL_LOCK:
            rows = _POOL.get((host, port))
            now = time.monotonic()
            conn = None
            if rows and now - rows[-1][1] <= max_idle:
                conn, _ = rows.pop()
            elif rows:
                for old, last in rows:
                    if now - last > KEEPALIVE_IDLE_SECONDS:
                        old.close()
                rows[:] = [(old, last) for old, last in rows if now - last <= KEEPALIVE_IDLE_SECONDS]
        if conn is None:
            break
        if _closed(conn):  # (an order never goes out on a connection the server has let go: it could swallow it)
            conn.close()
            _count("dead")
            continue
        _count("reused")
        if getattr(conn, "sock", None) is not None:
            conn.sock.settimeout(timeout)
        return conn, True
    _count("new")
    return HTTPS_CONNECTION(host, port, timeout=timeout, context=SSL_CONTEXT), False


def _pool_put(host: str, port: int, conn: Any) -> None:
    with _POOL_LOCK:
        rows = _POOL.setdefault((host, port), [])
        if len(rows) >= KEEPALIVE_PER_HOST:
            conn.close()
            return
        rows.append((conn, time.monotonic()))


def prewarm(host: str, port: int = 443, within: float = 2.0) -> bool:
    """A connection to host:port opened now and parked in the pool unused, when none there was used (or opened) within
    ``within`` seconds: an order — whose POST takes only a connection that fresh (KEEPALIVE_POST_IDLE_SECONDS) — then waits
    for no TCP + TLS handshake of its own. No request goes over it: nothing of the key's budget is spent. The pooled ones past
    KEEPALIVE_POST_IDLE_SECONDS are let go (the fresh one serves the reads as well). False: one that fresh was there already,
    or the requests go by a proxy (its own connections)."""
    fresh = pool_fresh(host, port)
    if (fresh is not None and fresh < within) or _proxied(host):
        return False
    _count("new")
    conn = HTTPS_CONNECTION(host, port, timeout=PREWARM_TIMEOUT_S, context=SSL_CONTEXT)
    began = time.perf_counter()
    try:
        conn.connect()
    except (OSError, http.client.HTTPException) as error:
        conn.close()
        raise _net_error(error) from None
    http_net(host)["connect"].append((time.perf_counter() - began) * 1000)
    with _POOL_LOCK:
        rows = _POOL.setdefault((host, port), [])
        now = time.monotonic()
        for old, last in rows:
            if now - last > KEEPALIVE_POST_IDLE_SECONDS:
                old.close()
        rows[:] = [(old, last) for old, last in rows if now - last <= KEEPALIVE_POST_IDLE_SECONDS]
    _pool_put(host, port, conn)
    return True


def pool_fresh(host: str, port: int = 443) -> float | None:
    """Seconds since the most recently used pooled connection to host:port was last used; None when there is none."""
    with _POOL_LOCK:
        rows = _POOL.get((host, port))
        return time.monotonic() - rows[-1][1] if rows else None


def _proxied(host: str) -> bool:
    proxies = urllib.request.getproxies()
    return bool(proxies.get("https")) and not urllib.request.proxy_bypass(host)


def _net_error(error: BaseException) -> RemoteError:
    detail = clean_error(str(error))[:80]
    return RemoteError(f"网络错误 ({type(error).__name__}{': ' + detail if detail else ''})")


def _remote_error(host: str, code: int, retry_after: Any, raw: bytes, encoding: str) -> RemoteError:
    retry = 0
    with contextlib.suppress(ValueError, TypeError):
        retry = max(0, int(retry_after or "0"))
    description = ""
    try:
        body = json.loads(_inflate(raw, encoding or ""))
        description = http_error_text(body)
        if isinstance(body, dict) and isinstance(body.get("parameters"), dict):
            retry = max(retry, int(body["parameters"].get("retry_after", 0)))
    except (ValueError, TypeError, AttributeError, RemoteError):
        pass
    if host.endswith("telegram.org"):
        text = description or "接口请求失败"
    elif host.endswith("predict.fun"):  # Predict says what is wrong (minimum order value, closed market…): its words first
        text = description or HTTP_HINTS.get(code) or "接口请求失败"
        if code == 403:
            text += "（Predict 不向受限地区开放交易：受限地区读取正常、下单 403，把服务换到允许的地区）"
    else:
        if "binance" in host and code in {418, 429}:
            retry = max(retry, 120 if code == 418 else 30)
        text = HTTP_HINTS.get(code) or description or "接口请求失败"
    return RemoteError(clean_error(f"HTTP {code}: {text}"), retry)


def _read_body(response: Any, timeout: float) -> bytes:
    deadline, chunks, size = time.monotonic() + 2 * timeout + 1, [], 0
    while True:
        chunk = response.read(65536)
        if not chunk:
            return b"".join(chunks)
        chunks.append(chunk)
        size += len(chunk)
        if size > MAX_BODY:
            raise RemoteError("接口返回的数据过大")
        if time.monotonic() > deadline:
            raise RemoteError(f"网络错误 (响应过慢：{int(2 * timeout + 1)} 秒内没有读完)")


def request(url: str, payload: Any = None, timeout: float = 15, headers: dict[str, str] | None = None,
            gate: Callable[[], str] | None = None, trace: dict | None = None) -> bytes:
    """GET (or POST ``payload`` — a dict sent as JSON, or bytes already encoded) and return the body; HTTP and network failures
    become RemoteError. Over https without a proxy the connection is kept alive and reused. ``gate`` (an order's): judged again
    when the request waited before its bytes could go — a new connection's handshake, a dead one replaced: a reason refuses it
    (NotSent), nothing sent. (Through a proxy urllib opens and writes in one step: the caller judged it right before.)"""
    data = None if payload is None else payload if isinstance(payload, (bytes, bytearray)) else json.dumps(payload).encode("utf-8")
    hdrs = {"User-Agent": USER_AGENT, "Accept": "application/json", "Accept-Encoding": "gzip",
            **({"Content-Type": "application/json"} if data is not None else {}), **(headers or {})}
    split = urllib.parse.urlsplit(url)
    host = split.hostname or ""
    if trace is not None:  # (an order's way out: the moment its thread took it up)
        trace["started"] = time.monotonic()
    if split.scheme != "https" or not host or _proxied(host):
        return _request_urllib(url, data, hdrs, timeout, host)
    return _request_keepalive(url, split, data, hdrs, timeout, gate, trace)


def _request_urllib(url: str, data: bytes | None, hdrs: dict[str, str], timeout: float, host: str) -> bytes:
    req = urllib.request.Request(url, data=data, headers={k: v for k, v in hdrs.items() if k.lower() not in SECRET_HEADERS})
    for name, value in hdrs.items():
        if name.lower() in SECRET_HEADERS:
            req.add_unredirected_header(name, value)
    try:
        with urllib.request.urlopen(req, timeout=timeout, context=SSL_CONTEXT) as response:
            _note_limits(host, response.headers.items())
            raw = _read_body(response, timeout)
            return _inflate(raw, response.headers.get("Content-Encoding", ""))
    except urllib.error.HTTPError as error:
        raw = b""
        with contextlib.suppress(Exception):
            raw = error.read(4096)
        raise _remote_error(host, error.code, error.headers.get("Retry-After"), raw, error.headers.get("Content-Encoding", "")) from None
    except (urllib.error.URLError, TimeoutError, OSError) as error:
        reason = getattr(error, "reason", None) if isinstance(error, urllib.error.URLError) else None
        detail = f": {clean_error(str(reason))[:80]}" if reason else ""
        raise RemoteError(f"网络错误 ({type(error).__name__}{detail})") from None
    except http.client.HTTPException as error:
        raise _net_error(error) from None


def _request_keepalive(url: str, split: Any, data: bytes | None, hdrs: dict[str, str], timeout: float,
                       gate: Callable[[], str] | None = None, trace: dict | None = None) -> bytes:
    host, port = split.hostname, split.port or 443
    path = (split.path or "/") + (f"?{split.query}" if split.query else "")
    method = "POST" if data is not None else "GET"
    waited = False  # (a dead connection replaced: time passed since the caller judged the request)
    for _hop in range(4):
        max_idle = KEEPALIVE_POST_IDLE_SECONDS if data is not None else KEEPALIVE_IDLE_SECONDS
        while True:
            conn, reused = _pool_take(host, port, timeout, max_idle)
            if trace is not None:
                trace.pop("connect_ms", None)
            if not reused and callable(getattr(conn, "connect", None)):
                began = time.perf_counter()
                try:
                    conn.connect()
                except (OSError, http.client.HTTPException) as error:
                    # no connection (refused, unreachable, the TLS handshake failed or timed out): not a byte of the request
                    # was written — a POST is told as not sent, so an order that never left holds nothing
                    conn.close()
                    if data is not None:
                        raise NotSent(f"{NOT_CONNECTED}（{_net_error(error)}），请求没有发出") from None
                    raise _net_error(error) from None
                http_net(host)["connect"].append((time.perf_counter() - began) * 1000)
                if trace is not None:
                    trace["connect_ms"] = (time.perf_counter() - began) * 1000
            try:
                if gate is not None and (waited or not reused) and (why := gate()):
                    _pool_put(host, port, conn)  # (untouched by this request: kept for the next one)
                    raise NotSent(why)
                conn.request(method, path, body=data, headers=hdrs)
                if trace is not None:
                    trace["wrote_wall"] = int(time.time() * 1000)
            except (OSError, http.client.HTTPException) as error:
                conn.close()
                # the server had closed it: once more, on another (judged again first). Safe for a POST too: a send that fails
                # never handed the whole request to the kernel (sendall returns only once every byte is in), so the server has
                # at most part of it, which it drops with the connection — no order can come of it. An answer that does not come
                # after a whole request is another thing: below, never sent again.
                if reused:
                    _count("retried")
                    waited = True
                    continue
                raise _net_error(error) from None
            try:
                response = conn.getresponse()
            except (OSError, http.client.HTTPException) as error:
                conn.close()
                if reused and data is None:  # a GET the closed connection swallowed: again; a POST may have been taken
                    _count("retried")
                    continue
                raise _net_error(error) from None
            break
        status = response.status
        if edge := (response.getheader("cf-ray") or response.getheader("server")):
            http_net(host)["edge"] = str(edge)[:40]
        _note_limits(host, response.getheaders())
        try:
            if status >= 400:
                raw = b""
                with contextlib.suppress(Exception):
                    raw = response.read(65536)
                if response.isclosed() and not response.will_close:  # (its answer read whole: the connection serves the next one)
                    _pool_put(host, port, conn)
                    conn = None
                raise _remote_error(host, status, response.getheader("Retry-After"), raw, response.getheader("Content-Encoding") or "")
            location = response.getheader("Location")
            if 300 <= status < 400 and location and data is None:
                with contextlib.suppress(Exception):
                    _read_body(response, timeout)
                if response.will_close:
                    conn.close()
                else:
                    _pool_put(host, port, conn)
                url = urllib.parse.urljoin(url, location)
                split = urllib.parse.urlsplit(url)
                if split.scheme != "https" or (split.hostname, split.port or 443) != (host, port):
                    hdrs = {k: v for k, v in hdrs.items() if k.lower() not in SECRET_HEADERS}
                if split.scheme != "https" or not split.hostname:
                    conn = None
                    return _request_urllib(url, data, hdrs, timeout, split.hostname or "")
                host, port = split.hostname, split.port or 443
                path = (split.path or "/") + (f"?{split.query}" if split.query else "")
                continue
            if 300 <= status < 400:
                raise RemoteError(f"HTTP {status}: 重定向没有跟随（{'POST 不会重发' if data is not None else '没有给出去向'}）")
            raw = _read_body(response, timeout)
        except RemoteError:
            if conn is not None:
                conn.close()
            raise
        except (OSError, http.client.HTTPException) as error:
            if conn is not None:
                conn.close()
            raise _net_error(error) from None
        if response.will_close:
            conn.close()
        else:
            _pool_put(host, port, conn)
        return _inflate(raw, response.getheader("Content-Encoding") or "")
    raise RemoteError("网络错误 (重定向过多)")


def request_json(url: str, payload: Any = None, timeout: float = 15, headers: dict[str, str] | None = None) -> Any:
    try:
        return json.loads(request(url, payload, timeout, headers))
    except (json.JSONDecodeError, UnicodeDecodeError):
        raise RemoteError("接口未返回有效 JSON") from None
