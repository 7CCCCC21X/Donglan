"""Telegram: the fills, the results and the problems pushed to the chats (TELEGRAM_CHAT_ID), and — with TELEGRAM_COMMANDS=on — the
admin's commands (ADMIN_USER_ID): /menu（按钮菜单）, /status, /check（自检）, /test（挂单测试）, /pause, /resume, /mode, /id. The
menu's buttons run the same commands; the ones that start real orders (resume, live trading, the order test) ask first, and a
"yes" older than a minute is not taken. Why the bot may not answer is kept (getMe, a webhook, the polling's failures, a command
from someone not the admin) for the self-check, and logged when it changes.

Sending needs no polling, so the token of the bot on Railway can carry the notifications; the commands poll for updates
(getUpdates), which only one process may do per token — they come through a bot of this service's own: TELEGRAM_COMMAND_TOKEN
(its replies go back through it), or TELEGRAM_BOT_TOKEN when that one is this service's alone. The command menu is registered at
the start (setMyCommands)."""
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
CONFIRM_S = 60  # a button's "yes" is taken this long
COMMANDS = [("menu", "菜单：按钮操作（状态、自检、暂停、恢复、切换模式、挂单测试、设置、复盘）"),
            ("settings", "设置：点一项改价格、份数、每天上限、停机线（/set 设置名 数值 改成任意值）"),
            ("review", "复盘：近 30 天的场次、胜负、盈亏（/review 7、/review all）"),
            ("status", "状态（模式、下一场、抢单记录、今天的盈亏）"), ("rules", "同 /status（和 Railway 机器人的 /rules 一样的内容）"),
            ("check", "自检：登录、账户、余额、授权、下单接口、推送"),
            ("test", "挂单测试：挂一张约 $1 的低价单、查到、撤掉"), ("pause", "暂停开新单（挂着的单当场撤）"),
            ("resume", "恢复"), ("mode", "切换模式：/mode off | paper | on"), ("id", "查你的用户 ID 和这个聊天的 ID")]


def hint(text: str) -> str:
    """What a failure to reach the commands' bot usually means, and what to do."""
    if any(w in text for w in ("401", "Unauthorized", "404", "Not Found")):
        return "（机器人 Token 不对：到 @BotFather 核对，再 bash deploy/gcp/rt.sh edit 改）"
    if "409" in text or "Conflict" in text:
        return "（另一个程序也在用这个机器人收消息——同一个 Token 只能一个程序收：给本服务单独建一个机器人）"
    if "chat not found" in text:
        return "（还没在机器人的对话里按 Start，或 TELEGRAM_CHAT_ID 写错了）"
    if "blocked" in text or "Forbidden" in text:
        return "（机器人被屏蔽了，或不在那个群里）"
    return ""


def needs_yes(cmd: str) -> bool:
    """The commands a button runs only after a "yes": those that start real orders again."""
    return cmd in {"/resume", "/mode on", "/test"}


def button_command(cmd: str) -> bool:
    return cmd in {"/status", "/check", "/pause", "/mode off", "/mode paper"} or needs_yes(cmd)


def confirm_card(cmd: str, now_s: int) -> tuple[str, dict]:
    """The card asking for the "yes" (valid CONFIRM_S seconds)."""
    what, why = {"/resume": ("恢复开新单", "恢复后按现在的模式继续：真实吃单模式下会真实下单。"),
                 "/mode on": ("切到真实吃单", "盘口出现规则价以下的卖单就真实下单（不会高于设置文件的 RULE_TAKER）。"),
                 "/test": ("做一次挂单测试", "挂一张约 $1、远低于市价的真实买单，查到后马上撤掉。")}.get(cmd, ("执行这个操作", ""))
    return (f"确定要{what}吗？\n{why}\n（{CONFIRM_S} 秒内点“确定”有效）",
            {"inline_keyboard": [[{"text": "✅ 确定", "callback_data": f"y:{now_s}:{cmd}"}, {"text": "取消", "callback_data": "m:"}]]})


def fields(view: Any) -> list[dict]:
    """The settings' fields (the control page's console.view), every group's in turn."""
    return [f for g in (view or []) for f in (g.get("fields") or [])]


def preset_values(field: dict) -> list[str]:
    """The values a field's buttons offer (a choice's option, a number's or a text's preset)."""
    return [str(p[0]) if isinstance(p, (list, tuple)) else str(p) for p in field.get("presets") or []]


def shown(field: dict, value: str) -> str:
    """A field's value as the control page shows it: a choice by its option's name, 0 as "不做" / "不限", with its unit."""
    if field.get("kind") == "choice":
        return next((str(p[1]) for p in field.get("presets") or [] if isinstance(p, (list, tuple)) and str(p[0]) == value), value)
    if value == "":
        return "（空）"
    if value == "0" and field.get("zero"):
        return str(field["zero"])
    unit = str(field.get("unit") or "")
    return f"${value}" if unit == "$" else value if not unit else f"{value}¢" if unit == "¢" else f"{value} {unit}"


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
                 handler: Callable[[str, str], Awaitable[str]] | None = None, enabled: bool = True, command_token: str = ""):
        self.token, self.admin, self.commands = token, int(admin or 0), commands
        self.cmd_token = command_token or token
        self.chats = tuple(chats) or (((self.admin, 0),) if self.admin else ())
        self.handler = handler
        self.menu_head: Callable[[], tuple[str, str]] | None = None  # the menu card's head and the mode in force (the taker's)
        self.settings_view: Callable[[], list] | None = None  # the control page's settings (console.view)
        self.enabled = enabled and bool(token) and bool(self.chats)
        self.bot_name = ""        # the commands' bot as Telegram names it (getMe), once it answered
        self.poll_error = ""      # the commands' last failure to be polled: "" while they come through
        self.webhook = ""         # a webhook set on the commands' bot (its host): another program takes its updates
        self.stranger: tuple[int, str] | None = None  # the last command from someone not ADMIN_USER_ID: (their id, the command)
        self.admin_seen = False
        self.logged = ""
        self.queue: collections.deque = collections.deque(maxlen=200)
        self.wake = asyncio.Event()
        self.sent = self.failed = 0
        self.error = ""
        self.offset = 0
        self.running: set = set()
        self.stopping = asyncio.Event()

    async def call(self, method: str, payload: dict, timeout: float = 15, token: str = "") -> Any:
        loop = asyncio.get_running_loop()
        url = API.format(token=token or self.token, method=method)
        answer = await loop.run_in_executor(None, http.request_json, url, payload, timeout, {})
        if not isinstance(answer, dict) or not answer.get("ok"):
            raise RemoteError(f"Telegram 拒绝：{(answer or {}).get('description') if isinstance(answer, dict) else answer}")
        return answer.get("result")

    def send(self, text: str, chats: tuple | None = None, token: str = "") -> None:
        """Queued for the chats (the configured ones by default), through ``token``'s bot (the notifications' by default)."""
        if not text or not (token or self.token) or not (chats or self.chats) or (token == "" and not self.enabled):
            return
        for chat in chats or self.chats:
            for part in chunks(text):
                self.queue.append((chat, part, token or self.token, None))
        self.wake.set()

    def reply(self, text: str, chat: tuple) -> None:
        """An answer to a command: back through the bot the command came to."""
        self.send(text, (chat,), self.cmd_token)

    def send_card(self, chat: tuple, card: tuple[str, dict]) -> None:
        """A card (a message with buttons) to a chat, through the commands' bot."""
        if self.cmd_token:
            self.queue.append((chat, card[0], self.cmd_token, card[1]))
            self.wake.set()

    def menu_card(self) -> tuple[str, dict]:
        """The menu: what is in force now, and a button for each thing the admin does most."""
        head, mode = self.menu_head() if self.menu_head is not None else ("", "")

        def tick(m: str, label: str) -> str:
            return f"✅ {label}" if mode == m else label

        def b(text: str, data: str) -> dict:
            return {"text": text, "callback_data": data}
        keyboard = [[b("📊 状态", "c:/status"), b("🔎 自检", "c:/check")],
                    [b("⏸ 暂停", "c:/pause"), b("▶️ 恢复", "c:/resume")],
                    [b(tick("off", "关"), "c:/mode off"), b(tick("paper", "只记录"), "c:/mode paper"), b(tick("on", "真实吃单"), "c:/mode on")],
                    [b("⚙️ 设置", "g:"), b("📈 复盘", "v:30")],
                    [b("🧪 挂单测试", "c:/test"), b("🔄 刷新", "m:")]]
        return f"📋 规则吃单菜单\n{head}", {"inline_keyboard": keyboard}

    def view(self) -> list:
        return self.settings_view() if self.settings_view is not None else []

    def settings_card(self) -> tuple[str, dict]:
        """The settings: one button a field (its group, its name, its value); "•" marks a value saved here or on the page."""
        rows = [[{"text": f"{'• ' if f.get('saved') else ''}{g['group']}·{f['label']}：{shown(f, str(f.get('value') or ''))}",
                  "callback_data": f"f:{f['key']}"}] for g in self.view() for f in g.get("fields") or []]
        rows.append([{"text": "← 菜单", "callback_data": "m:"}])
        return "⚙️ 设置：点一项看说明、换一个值（当场生效）。• = 在这里或网页上改过的（不是设置文件里的值）", {"inline_keyboard": rows}

    def field_card(self, key: str) -> tuple[str, dict] | None:
        """One field: its value, what it does, a button for each preset (the one in force ticked), back to the settings file's."""
        field = next((f for f in fields(self.view()) if f.get("key") == key), None)
        if field is None:
            return None
        value = str(field.get("value") or "")
        buttons = [{"text": ("✅ " if v == value else "") + shown(field, v), "callback_data": f"s:{key}:{v}"} for v in preset_values(field)]
        rows = [buttons[i:i + 3] for i in range(0, len(buttons), 3)]
        rows.append(([{"text": "恢复设置文件的值", "callback_data": f"r:{key}"}] if field.get("saved") else [])
                    + [{"text": "← 设置", "callback_data": "g:"}])
        text = (f"⚙️ {field['label']}\n现在：{shown(field, value)}（{'在这里或网页上改过的' if field.get('saved') else '设置文件里的'}）\n"
                f"{field.get('help') or ''}\n别的数值：发 /set {key} 数值")
        return text, {"inline_keyboard": rows}

    def health(self) -> tuple[bool, str]:
        """The self-check's Telegram line: (fine, text) — not fine says what keeps the bot from answering, and what to do."""
        if not self.cmd_token and not self.token:
            return False, "Telegram：没设置机器人（不发通知、不收命令；要用的话见 README“Telegram：怎么绑定”）"
        notes = (f"；通知发送失败 {self.failed} 次（{self.error}{hint(self.error)}）" if self.failed
                 else f"；已发 {self.sent} 条消息" if self.sent else "")
        bot = self.bot_name or "（还没连上）"
        if not self.commands:
            return False, f"Telegram：TELEGRAM_COMMANDS 不是 on：机器人只发通知、不收命令（菜单、/status 都不会回）——bash deploy/gcp/rt.sh edit 改成 on{notes}"
        if self.webhook:
            return False, f"Telegram：机器人 {bot} 设了 webhook（{self.webhook}）：别的程序在用这个 Token，本服务收不到命令——给本服务单独建一个机器人{notes}"
        if self.poll_error:
            return False, f"Telegram：收不到命令：{self.poll_error}{hint(self.poll_error)}{notes}"
        if not self.bot_name:
            return False, f"Telegram：正在连接命令机器人（刚启动的话等几秒）{notes}"
        if not self.admin:
            return False, (f"Telegram：机器人 {bot} 在收命令，但 ADMIN_USER_ID 没填：除了 /id 都不回——给机器人发 /id，把回的用户 ID 填进 "
                           f"ADMIN_USER_ID（bash deploy/gcp/rt.sh edit）{notes}")
        if self.stranger is not None and not self.admin_seen:
            user, command = self.stranger
            return False, (f"Telegram：用户 {user} 发过 {command}，但 ADMIN_USER_ID 是 {self.admin}：要是 {user} 是你，把 ADMIN_USER_ID 改成 "
                           f"{user}（bash deploy/gcp/rt.sh edit）{notes}")
        return not self.failed, f"Telegram：机器人 {bot} 在收命令（管理员 {self.admin}；发 /menu 出按钮菜单）{notes}"

    def poll_failed(self, error: BaseException) -> None:
        """The commands' polling failed: kept for the self-check, logged when it changed."""
        text = clean_error(error) or type(error).__name__
        self.error = self.poll_error = text
        if self.logged != text:
            LOG.warning("Telegram 收不到命令：%s%s", text, hint(text))
            self.logged = text

    async def answer_button(self, query_id: str, text: str) -> None:
        with contextlib.suppress(Exception):
            await self.call("answerCallbackQuery", {"callback_query_id": query_id, "text": text}, 10, token=self.cmd_token)

    async def edit_card(self, chat: int, message_id: int, card: tuple[str, dict]) -> None:
        with contextlib.suppress(Exception):  # ("message is not modified": nothing to do)
            await self.call("editMessageText", {"chat_id": chat, "message_id": message_id, "text": card[0], "reply_markup": card[1]},
                            token=self.cmd_token)

    async def run(self) -> None:
        tasks = [asyncio.create_task(self.send_loop(), name="telegram-send")]
        if self.commands and self.cmd_token:  # (with no ADMIN_USER_ID yet only /id is answered: how the admin finds the number)
            tasks.append(asyncio.create_task(self.poll_loop(), name="telegram-poll"))
        try:
            await self.stopping.wait()
        finally:
            await self.drain(5.0)
            for t in [*tasks, *self.running]:
                t.cancel()
            await asyncio.gather(*tasks, *self.running, return_exceptions=True)

    async def drain(self, seconds: float) -> None:
        deadline = time.monotonic() + seconds
        while self.queue and time.monotonic() < deadline:
            await self.send_one()

    def stop(self) -> None:
        self.stopping.set()

    async def send_one(self) -> None:
        (chat, thread), text, token, markup = self.queue.popleft()
        payload = {"chat_id": chat, "text": text, "disable_web_page_preview": True}
        if thread:
            payload["message_thread_id"] = thread
        if markup:
            payload["reply_markup"] = markup
        for attempt in range(3):
            try:
                await self.call("sendMessage", payload, token=token)
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
        # who the commands' bot is (a wrong token fails here), and whether another program took its updates (a webhook)
        while not self.stopping.is_set():
            try:
                me = await self.call("getMe", {}, token=self.cmd_token)
                self.bot_name = "@" + str((me or {}).get("username") or "?") if isinstance(me, dict) else "@?"
                break
            except asyncio.CancelledError:
                raise
            except Exception as error:
                self.poll_failed(error)
                await asyncio.sleep(30)  # (a wrong token stays wrong: asked again now and then, not hammered)
        with contextlib.suppress(Exception):
            info = await self.call("getWebhookInfo", {}, token=self.cmd_token)
            url = str(info.get("url") or "") if isinstance(info, dict) else ""
            if url:
                self.webhook = url.split("://", 1)[-1].split("/", 1)[0]
                LOG.warning("Telegram：命令机器人 %s 设了 webhook（%s）：别的程序在用这个 Token，本服务收不到命令——给本服务单独建一个机器人",
                            self.bot_name, self.webhook)
        with contextlib.suppress(Exception):  # the menu beside the input box (a failure here changes nothing: typed commands work)
            await self.call("setMyCommands", {"commands": [{"command": c, "description": d} for c, d in COMMANDS]}, token=self.cmd_token)
        LOG.info("Telegram 命令机器人 %s 开始收命令（%s）", self.bot_name, f"管理员 {self.admin}" if self.admin else "ADMIN_USER_ID 没填：只回 /id")
        while not self.stopping.is_set():
            try:
                updates = await self.call("getUpdates", {"offset": self.offset, "timeout": 25, "allowed_updates": ["message", "callback_query"]},
                                          timeout=35, token=self.cmd_token)
            except asyncio.CancelledError:
                raise
            except Exception as error:
                self.poll_failed(error)
                await asyncio.sleep(10 if "409" in str(error) else 3)  # 409: another process polls this token
                continue
            if self.poll_error:
                LOG.info("Telegram 收命令恢复正常")
                self.poll_error = self.logged = ""
            for update in updates or []:
                self.offset = max(self.offset, int(update.get("update_id", 0)) + 1)
                self.on_update(update)

    def on_update(self, update: dict) -> None:
        """One update: /id answered to anyone (how the admin finds the number), the rest to the admin alone; a button's tap."""
        if isinstance(update.get("callback_query"), dict):
            self.spawn(self.on_button(update["callback_query"]))
            return
        msg = update.get("message") or {}
        text = str(msg.get("text") or "").strip()
        chat = (msg.get("chat") or {}).get("id")
        thread = int(msg.get("message_thread_id") or 0)
        user = int((msg.get("from") or {}).get("id") or 0)
        if not text.startswith("/") or chat is None:
            return
        command, _, args = text.partition(" ")
        command = command.split("@", 1)[0].lower()
        if command == "/id":
            self.reply(f"你的用户 ID：{user}\n这个聊天：{chat}" + (f":{thread}" if thread else ""), (chat, thread))
            return
        if not self.admin or user != self.admin:
            # someone not the admin: kept for the self-check; in a private chat told their own id (a wrong ADMIN_USER_ID found at
            # once, by whoever it is)
            LOG.info("Telegram：用户 %s 发了 %s，不是 ADMIN_USER_ID（%s），不理", user, command, self.admin)
            self.stranger = (user, command)
            if chat == user:
                self.reply(f"这个机器人只听管理员的命令。你的用户 ID 是 {user}：要是你就是管理员，把设置里的 ADMIN_USER_ID 改成它"
                           "（bash deploy/gcp/rt.sh edit）", (chat, thread))
            return
        self.admin_seen = True
        if command in {"/menu", "/start"}:
            self.send_card((chat, thread), self.menu_card())
            return
        if command == "/settings" and self.settings_view is not None:
            self.send_card((chat, thread), self.settings_card())
            return
        if self.handler is not None:
            # each command in a task of its own: a self-check of a few seconds holds no other command (nor the polling) up
            self.spawn(self.answer(command, args.strip(), (chat, thread)))

    def spawn(self, coro: Any) -> None:
        task = asyncio.create_task(coro, name="telegram-command")
        self.running.add(task)
        task.add_done_callback(self.running.discard)

    async def on_button(self, query: dict) -> None:
        """A tap on a menu button (the admin's alone): "c:" a command (one that starts real orders asks first), "y:<at>:" its "yes"
        (CONFIRM_S seconds), "m:" the menu again. The command's answer comes as a message; the card then shows what is in force."""
        query_id, data = str(query.get("id") or ""), str(query.get("data") or "")
        user = int((query.get("from") or {}).get("id") or 0)
        msg = query.get("message") or {}
        chat = int((msg.get("chat") or {}).get("id") or 0)
        thread, message_id = int(msg.get("message_thread_id") or 0), int(msg.get("message_id") or 0)
        if not self.admin or user != self.admin:
            await self.answer_button(query_id, "这个机器人只听管理员的")
            return
        self.admin_seen = True
        kind, _, rest = data.partition(":")
        # the settings and the review: a field's card, a preset saved (one of the field's own), back to the file's value
        if kind == "g":
            await self.answer_button(query_id, "")
            await self.edit_card(chat, message_id, self.settings_card())
            return
        if kind == "f":
            await self.answer_button(query_id, "")
            await self.edit_card(chat, message_id, self.field_card(rest) or self.settings_card())
            return
        if kind in {"s", "r"}:
            key, _, value = rest.partition(":")
            field = next((f for f in fields(self.view()) if f.get("key") == key), None)
            fine = field is not None and (value in preset_values(field) if kind == "s" else bool(field.get("saved")))
            if not fine:
                await self.answer_button(query_id, "这个按钮已经过时，重新打开设置")
                await self.edit_card(chat, message_id, self.settings_card())
                return
            await self.answer_button(query_id, "收到")
            await self.answer("/set" if kind == "s" else "/reset", f"{key} {value}" if kind == "s" else key, (chat, thread))
            await self.edit_card(chat, message_id, self.field_card(key) or self.settings_card())
            return
        if kind == "v":
            await self.answer_button(query_id, "收到")
            await self.answer("/review", rest if rest in {"7", "30", "90", "all"} else "30", (chat, thread))
            return
        if kind == "c" and button_command(rest) and needs_yes(rest):
            await self.answer_button(query_id, "")
            await self.edit_card(chat, message_id, confirm_card(rest, int(time.time())))
            return
        if kind == "c" and button_command(rest):
            cmd = rest
        elif kind == "y":
            at, _, cmd = rest.partition(":")
            age = int(time.time()) - (int(at) if at.isdigit() else 0)
            if not button_command(cmd) or not 0 <= age <= CONFIRM_S:
                await self.answer_button(query_id, "确认已过期，请重新点一次")
                await self.edit_card(chat, message_id, self.menu_card())
                return
        else:
            await self.answer_button(query_id, "")
            await self.edit_card(chat, message_id, self.menu_card())
            return
        command, _, args = cmd.partition(" ")
        await self.answer_button(query_id, "收到")
        await self.answer(command, args, (chat, thread))
        await self.edit_card(chat, message_id, self.menu_card())

    async def answer(self, command: str, args: str, chat: tuple) -> None:
        if command in {"/check", "/test"}:
            self.reply("🔎 自检中，结果几秒后发来…" if command == "/check" else "🧪 挂单测试中（约 $1 的低价单，挂上就撤），结果几秒后发来…", chat)
        try:
            reply = await self.handler(command, args)
        except Exception as error:
            reply = f"命令出错：{clean_error(error) or type(error).__name__}"
        if reply:
            self.reply(reply, chat)
