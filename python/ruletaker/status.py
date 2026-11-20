"""The operator's HTTP server (STATUS_HOST:STATUS_PORT, 127.0.0.1:8080 by default — reached through an SSH tunnel: rt.sh console):

  GET  /                   the control page and the review page (控制台 · 复盘), one page
  GET  /healthz            "ok" while the taker's loop runs (no key)
  GET  /status             the status in text, what /rules said in huice
  GET  /status.json        a summary in numbers
  GET  /api/state          the control page's data
  GET  /api/review?days=30 the review page's data (7 / 30 / 90 / all)
  GET  /api/records.csv    every order of every window
  POST /api/control        {"action": "pause" | "resume" | "mode" | "withdraw" | "check"（自检） | "test"（挂单测试）, "arg": ...}
  POST /api/settings       {"values": {"RULE_GOLD_SHUT_CENTS": "60", ...}} or {"reset": ["RULE_…", ...]} or {"reset": "all"}
  POST /pause  /resume  /mode?to=off|paper|on   (rt.sh)

With STATUS_KEY set every path but / and /healthz needs it (?key=… or the X-Status-Key header). Every POST also needs the header
X-RT: 1 — a page of another site can send neither (no cross-site form can set a header, and no preflight is ever approved), so
nobody can pause or change the settings through a logged-in browser. A server on another address than the machine itself is
refused at the start without a key (Config)."""
from __future__ import annotations

import asyncio
import contextlib
import hmac
import json
import os
import urllib.parse
from typing import Any

from . import console
from .util import LOG, NotOnDisk, clean_error

PAGE = os.path.join(os.path.dirname(os.path.abspath(__file__)), "web", "console.html")
MAX_BODY = 65_536
MAX_CONNECTIONS = 32  # requests served at once (more are closed at once: the page asks again in two seconds)


def reset_keys(reset: Any) -> list[str] | None | str:
    """The fields a reset names: "all" -> None (every one the page saved), a list of the page's field names -> that list;
    anything else (a bare name, an empty list, a name the page has not) -> why it is refused (a str), never "all"."""
    how = 'reset 要写成字段名的数组（例如 ["RULE_GOLD_SHUT_CENTS"]），全部恢复写 "all"'
    if reset == "all":
        return None
    if not isinstance(reset, list) or not reset or not all(isinstance(k, str) for k in reset):
        return how
    for key in reset:
        if key not in console.FIELDS:
            return f"{key} 不是控制台的设置项"
    return list(reset)


class StatusServer:
    def __init__(self, taker: Any, host: str, port: int, key: str = ""):
        self.taker, self.host, self.port, self.key = taker, host, port, key
        self.server: asyncio.base_events.Server | None = None
        self.open = 0

    async def start(self) -> None:
        if not self.port:
            return
        self.server = await asyncio.start_server(self.handle, self.host, self.port)
        if self.host in {"127.0.0.1", "localhost", "::1"}:  # (the machine's own address: a browser elsewhere comes through SSH)
            LOG.info("控制台和状态页在这台机器本机 %s:%d（别的电脑直接打不开；从浏览器看：Cloud Shell 里运行 bash deploy/gcp/rt.sh console）",
                     self.host, self.port)
        else:
            LOG.info("控制台：http://%s:%d/  状态：http://%s:%d/status", self.host, self.port, self.host, self.port)

    async def stop(self) -> None:
        if self.server is not None:
            self.server.close()
            with contextlib.suppress(Exception):
                await asyncio.wait_for(self.server.wait_closed(), 2)

    def allowed(self, query: dict, headers: dict) -> bool:
        if not self.key:
            return True
        given = (query.get("key") or [""])[0] or headers.get("x-status-key", "")
        return hmac.compare_digest(given.encode(), self.key.encode())

    def turned_away(self, method: str, path: str, query: dict, headers: dict) -> tuple[int, str, str] | None:
        """Whether a request is refused on its head alone (every path but /healthz and the page): the key, the method, a POST's
        X-RT header — judged before its body is read, so nobody without the key gets the server to take one in."""
        text = "text/plain; charset=utf-8"
        if path == "/healthz" or (path in {"/", "/console", "/index.html"} and method == "GET"):
            return None
        if not self.allowed(query, headers):
            return 403, text, "need ?key=STATUS_KEY"
        if method not in {"GET", "POST"}:
            return 405, text, "method not allowed"
        if method == "POST" and headers.get("x-rt") != "1":
            return 403, text, "POST needs the header X-RT: 1"
        return None

    async def handle(self, reader: asyncio.StreamReader, writer: asyncio.StreamWriter) -> None:
        if self.open >= MAX_CONNECTIONS:
            writer.close()
            return
        self.open += 1
        try:
            await self.serve_one(reader, writer)
        finally:
            self.open -= 1

    async def serve_one(self, reader: asyncio.StreamReader, writer: asyncio.StreamWriter) -> None:
        extra = ""
        try:
            head = await asyncio.wait_for(reader.readuntil(b"\r\n\r\n"), 5)
            lines = head.decode("latin-1").split("\r\n")
            method, target, _ = (lines[0].split(" ") + ["", "", ""])[:3]
            headers = {k.strip().lower(): v.strip() for k, _, v in (line.partition(":") for line in lines[1:] if ":" in line)}
            url = urllib.parse.urlsplit(target)
            query = urllib.parse.parse_qs(url.query)
            refused = self.turned_away(method.upper(), url.path, query, headers)
            if refused is not None:
                status, kind, text = refused
            else:
                length = int(headers.get("content-length") or 0)
                body = b""
                if length > MAX_BODY:
                    raise ValueError("body too large")
                if length > 0:
                    body = await asyncio.wait_for(reader.readexactly(length), 5)
                status, kind, text = await self.route(method.upper(), url.path, query, headers, body)
            if kind.startswith("text/csv"):
                extra = "Content-Disposition: attachment; filename=\"ruletaker-records.csv\"\r\n"
        except Exception as error:  # (a malformed request: answered and closed, never the service's problem)
            status, kind, text = 400, "text/plain; charset=utf-8", f"bad request: {type(error).__name__}"
        data = text.encode("utf-8")
        writer.write(f"HTTP/1.1 {status} {'OK' if status == 200 else 'Error'}\r\nContent-Type: {kind}\r\n{extra}"
                     f"Content-Length: {len(data)}\r\nCache-Control: no-store\r\nX-Content-Type-Options: nosniff\r\n"
                     f"Referrer-Policy: no-referrer\r\nConnection: close\r\n\r\n".encode() + data)
        with contextlib.suppress(Exception):
            await writer.drain()
            writer.close()

    async def route(self, method: str, path: str, query: dict, headers: dict, body: bytes = b"") -> tuple[int, str, str]:
        text, js = "text/plain; charset=utf-8", "application/json; charset=utf-8"
        if path == "/healthz":
            ok = self.taker.healthy()
            return (200 if ok else 503), text, "ok" if ok else "not running"
        if path in {"/", "/console", "/index.html"} and method == "GET":
            try:
                with open(PAGE, encoding="utf-8") as f:
                    return 200, "text/html; charset=utf-8", f.read()
            except OSError:
                return 500, text, "控制台页面文件不见了（ruletaker/web/console.html）"
        if not self.allowed(query, headers):
            return 403, text, "need ?key=STATUS_KEY"
        if method == "GET":
            if path == "/status":
                if hasattr(self.taker, "after_the_race"):
                    await self.taker.after_the_race()  # (milliseconds of the loop the orders use: never in the race)
                return 200, text, self.taker.status()
            if path == "/status.json":
                return 200, js, json.dumps(self.taker.summary(), ensure_ascii=False, default=str)
            if path == "/api/state":
                return 200, js, json.dumps(self.taker.state(), ensure_ascii=False, default=str)
            if path == "/api/review":
                raw = (query.get("days") or ["30"])[0]
                days = 0 if raw == "all" else max(1, min(3650, int(raw))) if raw.isdigit() else 30
                return 200, js, json.dumps(self.taker.review_data(days), ensure_ascii=False, default=str)
            if path == "/api/records.csv":
                from .review import records_csv
                if hasattr(self.taker, "after_the_race"):
                    await self.taker.after_the_race()
                return 200, "text/csv; charset=utf-8", "﻿" + records_csv(self.taker.recs)  # (the BOM: Excel reads it as UTF-8)
            return (405 if path in {"/pause", "/resume", "/mode", "/api/control", "/api/settings"} else 404), text, "use POST" if path.startswith(("/pause", "/resume", "/mode", "/api/")) else "not found"
        if method != "POST":
            return 405, text, "method not allowed"
        if headers.get("x-rt") != "1":
            return 403, text, "POST needs the header X-RT: 1"
        if path == "/pause":
            return 200, text, await self.taker.control("pause", (query.get("why") or ["状态页"])[0])
        if path == "/resume":
            return 200, text, await self.taker.control("resume", "")
        if path == "/mode":
            return 200, text, await self.taker.control("mode", (query.get("to") or [""])[0])
        try:
            data = json.loads(body or b"{}")
        except ValueError:
            return 400, js, json.dumps({"ok": False, "message": "请求不是 JSON"}, ensure_ascii=False)
        if not isinstance(data, dict):
            return 400, js, json.dumps({"ok": False, "message": "请求不是 JSON 对象"}, ensure_ascii=False)
        try:
            if path == "/api/control":
                action, arg = str(data.get("action") or ""), str(data.get("arg") or "")
                if action == "withdraw":
                    message = await self.taker.withdraw_all("控制台撤单") or "没有挂着的规则吃单订单"
                elif action in {"pause", "resume", "mode", "check", "test"}:
                    message = await self.taker.control(action, arg or ("控制台" if action == "pause" else ""))
                else:
                    return 400, js, json.dumps({"ok": False, "message": f"不认识的操作：{action}"}, ensure_ascii=False)
            elif path == "/api/settings":
                if "reset" in data:
                    keys = reset_keys(data["reset"])
                    if isinstance(keys, str):  # (refused: never read as "all")
                        return 400, js, json.dumps({"ok": False, "message": keys}, ensure_ascii=False)
                    message = await self.taker.reset_settings(keys)
                else:
                    values = data.get("values")
                    if not isinstance(values, dict) or not values:
                        return 400, js, json.dumps({"ok": False, "message": "没有要保存的值"}, ensure_ascii=False)
                    message = await self.taker.save_settings(values)
            else:
                return 404, js, json.dumps({"ok": False, "message": "not found"}, ensure_ascii=False)
        except NotOnDisk as late:  # (in force, not on disk: never told as saved — "applied" lets the page show the values in force)
            return 200, js, json.dumps({"ok": False, "applied": True, "message": str(late)}, ensure_ascii=False)
        except ValueError as error:  # (a setting that does not parse: nothing changed, and why)
            return 200, js, json.dumps({"ok": False, "message": str(error)}, ensure_ascii=False)
        except Exception as error:
            return 500, js, json.dumps({"ok": False, "message": clean_error(error) or type(error).__name__}, ensure_ascii=False)
        return 200, js, json.dumps({"ok": True, "message": message}, ensure_ascii=False)
