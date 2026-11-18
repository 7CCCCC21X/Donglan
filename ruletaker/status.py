"""A small HTTP server for the operator (STATUS_HOST:STATUS_PORT, 127.0.0.1:8080 by default — reached through `gcloud compute ssh`):

  GET  /healthz            "ok" while the taker's loop runs (no key)
  GET  /status             the status in text, what /rules said in huice
  GET  /status.json        the same in numbers
  POST /pause  /resume     no new orders / orders again (the pause is saved: it outlives a restart)
  POST /mode?to=off|paper|on   the mode, never above RULE_TAKER (the deploy's setting is the ceiling)

With STATUS_KEY set every path but /healthz needs it (?key=… or the X-Status-Key header); a server on another address than
the machine itself is refused at the start without one (Config)."""
from __future__ import annotations

import asyncio
import contextlib
import hmac
import json
import urllib.parse
from typing import Any

from .util import LOG


class StatusServer:
    def __init__(self, taker: Any, host: str, port: int, key: str = ""):
        self.taker, self.host, self.port, self.key = taker, host, port, key
        self.server: asyncio.base_events.Server | None = None

    async def start(self) -> None:
        if not self.port:
            return
        self.server = await asyncio.start_server(self.handle, self.host, self.port)
        LOG.info("状态页：http://%s:%d/status", self.host, self.port)

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

    async def handle(self, reader: asyncio.StreamReader, writer: asyncio.StreamWriter) -> None:
        try:
            head = await asyncio.wait_for(reader.readuntil(b"\r\n\r\n"), 5)
            lines = head.decode("latin-1").split("\r\n")
            method, target, _ = (lines[0].split(" ") + ["", "", ""])[:3]
            headers = {k.strip().lower(): v.strip() for k, _, v in (line.partition(":") for line in lines[1:] if ":" in line)}
            length = int(headers.get("content-length") or 0)
            if 0 < length <= 4096:
                await asyncio.wait_for(reader.readexactly(length), 5)
            url = urllib.parse.urlsplit(target)
            query = urllib.parse.parse_qs(url.query)
            status, kind, body = await self.route(method.upper(), url.path, query, headers)
        except Exception as error:  # (a malformed request: answered and closed, never the service's problem)
            status, kind, body = 400, "text/plain; charset=utf-8", f"bad request: {type(error).__name__}"
        data = body.encode("utf-8")
        writer.write(f"HTTP/1.1 {status} {'OK' if status == 200 else 'Error'}\r\nContent-Type: {kind}\r\n"
                     f"Content-Length: {len(data)}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n".encode() + data)
        with contextlib.suppress(Exception):
            await writer.drain()
            writer.close()

    async def route(self, method: str, path: str, query: dict, headers: dict) -> tuple[int, str, str]:
        text = "text/plain; charset=utf-8"
        if path == "/healthz":
            ok = self.taker.healthy()
            return (200 if ok else 503), text, "ok" if ok else "not running"
        if not self.allowed(query, headers):
            return 403, text, "need ?key=STATUS_KEY"
        if path in {"/", "/status"} and method == "GET":
            return 200, text, self.taker.status()
        if path == "/status.json" and method == "GET":
            return 200, "application/json; charset=utf-8", json.dumps(self.taker.summary(), ensure_ascii=False, default=str)
        if method != "POST":
            return 405, text, "use POST for /pause, /resume, /mode"
        if path == "/pause":
            return 200, text, await self.taker.control("pause", (query.get("why") or ["状态页"])[0])
        if path == "/resume":
            return 200, text, await self.taker.control("resume", "")
        if path == "/mode":
            return 200, text, await self.taker.control("mode", (query.get("to") or [""])[0])
        return 404, text, "not found"
