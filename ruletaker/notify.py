"""Telegram: the fills, the results and the problems pushed to the chats (TELEGRAM_CHAT_ID), and — with TELEGRAM_COMMANDS=on and a
bot token of this service's own — a few commands from the admin (ADMIN_USER_ID): /status, /pause, /resume, /mode, /id.

Sending needs no polling, so the token of the bot on Railway can be reused for the notifications alone; the commands poll for
updates (getUpdates), which only one process may do per token — give this service a bot of its own for them."""
from __future__ import annotations

import asyncio
import collections
import contextlib
import time
from typing import Any, Awaitable, Callable

from . import http
from .util import LOG, RemoteError, clean_error

API = "https://api.telegram.org/bot{token}/{method}"
LIMIT = 3900  # characters a message (Telegram allows 4096)


def chunks(text: str, limit: int = LIMIT) -> list[str]:
    """A long text cut at line ends into messages of at most ``limit`` characters."""
    out, part = [], ""
    for line in text.split("\n"):
        while len(line) > limit:
            if part:
                out.append(part)
                part = ""
            out.append(line[:limit])
            line = line[limit:]
        if len(part) + len(line) + 1 > limit:
            out.append(part)
            part = line
        else:
            part = f"{part}\n{line}" if part else line
    if part:
        out.append(part)
    return out or [""]


class Telegram:
    def __init__(self, token: str, chats: tuple, admin: int = 0, commands: bool = False,
                 handler: Callable[[str, str], Awaitable[str]] | None = None, enabled: bool = True):
        self.token, self.admin, self.commands = token, int(admin or 0), commands
        self.chats = tuple(chats) or (((self.admin, 0),) if self.admin else ())
        self.handler = handler
        self.enabled = enabled and bool(token) and bool(self.chats)
        self.queue: collections.deque = collections.deque(maxlen=200)
        self.wake = asyncio.Event()
        self.sent = self.failed = 0
        self.error = ""
        self.offset = 0
        self.stopping = asyncio.Event()

    async def call(self, method: str, payload: dict, timeout: float = 15) -> Any:
        loop = asyncio.get_running_loop()
        answer = await loop.run_in_executor(None, http.request_json, API.format(token=self.token, method=method), payload, timeout, {})
        if not isinstance(answer, dict) or not answer.get("ok"):
            raise RemoteError(f"Telegram 拒绝：{(answer or {}).get('description') if isinstance(answer, dict) else answer}")
        return answer.get("result")

    def send(self, text: str, chats: tuple | None = None) -> None:
        if not self.enabled or not text:
            return
        for chat in chats or self.chats:
            for part in chunks(text):
                self.queue.append((chat, part))
        self.wake.set()

    async def run(self) -> None:
        tasks = [asyncio.create_task(self.send_loop(), name="telegram-send")]
        if self.commands and self.token and self.admin:
            tasks.append(asyncio.create_task(self.poll_loop(), name="telegram-poll"))
        try:
            await self.stopping.wait()
        finally:
            await self.drain(5.0)
            for t in tasks:
                t.cancel()
            await asyncio.gather(*tasks, return_exceptions=True)

    async def drain(self, seconds: float) -> None:
        deadline = time.monotonic() + seconds
        while self.queue and time.monotonic() < deadline:
            await self.send_one()

    def stop(self) -> None:
        self.stopping.set()

    async def send_one(self) -> None:
        (chat, thread), text = self.queue.popleft()
        payload = {"chat_id": chat, "text": text, "disable_web_page_preview": True}
        if thread:
            payload["message_thread_id"] = thread
        for attempt in range(3):
            try:
                await self.call("sendMessage", payload)
                self.sent += 1
                return
            except asyncio.CancelledError:
                raise
            except Exception as error:
                self.error = clean_error(error) or type(error).__name__
                wait = getattr(error, "retry_after", 0) or (1 + attempt * 2)
                if "HTTP 4" in str(error) and "429" not in str(error):
                    break  # (refused for good: a wrong chat, a bot not in the group)
                await asyncio.sleep(min(wait, 30))
        self.failed += 1
        LOG.warning("Telegram 消息没发出去：%s", self.error)

    async def send_loop(self) -> None:
        while not self.stopping.is_set():
            if not self.queue:
                self.wake.clear()
                with contextlib.suppress(asyncio.TimeoutError):
                    await asyncio.wait_for(self.wake.wait(), 30)
                continue
            await self.send_one()
            await asyncio.sleep(0.35)  # (Telegram: about one message a second a chat, thirty a second in all)

    async def poll_loop(self) -> None:
        while not self.stopping.is_set():
            try:
                updates = await self.call("getUpdates", {"offset": self.offset, "timeout": 25, "allowed_updates": ["message"]}, timeout=35)
            except asyncio.CancelledError:
                raise
            except Exception as error:
                self.error = clean_error(error) or type(error).__name__
                await asyncio.sleep(10 if "409" in str(error) else 3)  # 409: another process polls this token
                continue
            for update in updates or []:
                self.offset = max(self.offset, int(update.get("update_id", 0)) + 1)
                msg = update.get("message") or {}
                text = str(msg.get("text") or "").strip()
                chat = (msg.get("chat") or {}).get("id")
                thread = int(msg.get("message_thread_id") or 0)
                user = int((msg.get("from") or {}).get("id") or 0)
                if not text.startswith("/") or chat is None:
                    continue
                command, _, args = text.partition(" ")
                command = command.split("@", 1)[0].lower()
                if command == "/id":
                    self.send(f"你的用户 ID：{user}\n这个聊天：{chat}" + (f":{thread}" if thread else ""), ((chat, thread),))
                    continue
                if user != self.admin or self.handler is None:
                    continue
                try:
                    reply = await self.handler(command, args.strip())
                except Exception as error:
                    reply = f"命令出错：{clean_error(error) or type(error).__name__}"
                if reply:
                    self.send(reply, ((chat, thread),))
