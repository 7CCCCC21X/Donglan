//! Telegram: the fills, the results and the problems pushed to the chats (TELEGRAM_CHAT_ID), and — with TELEGRAM_COMMANDS=on —
//! the admin's commands (ADMIN_USER_ID): /menu（按钮菜单）, /status, /check（自检）, /test（挂单测试）, /pause, /resume, /mode,
//! /id. The menu's buttons run the same commands; the ones that start real orders (resume, live trading, the order test) ask
//! first, and a "yes" older than a minute is not taken. Why the bot may not answer is kept (getMe, a webhook, the polling's
//! failures, a command from someone not the admin) for the self-check, and logged when it changes.
//!
//! Sending needs no polling, so the token of the bot on Railway can carry the notifications; the commands poll for updates
//! (getUpdates), which only one process may do per token — they come through a bot of this service's own: TELEGRAM_COMMAND_TOKEN
//! (its replies go back through it), or TELEGRAM_BOT_TOKEN when that one is this service's alone. The command menu is
//! registered at the start (setMyCommands).
use crate::predict::{Req, Transport};
use crate::sync::{Event, sleep, spawn};
use crate::util::{Py, RemoteError, Res, clean_error, mono};
use futures_util::future::LocalBoxFuture;
use serde_json::{Value, json};
use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::rc::Rc;

pub const API: &str = "https://api.telegram.org";
pub const LIMIT: usize = 3900;
pub const CONFIRM_S: i64 = 60;
pub const COMMANDS: [(&str, &str); 9] = [
    ("menu", "菜单：按钮操作（状态、自检、暂停、恢复、切换模式、挂单测试）"),
    ("status", "状态（模式、下一场、抢单记录、今天的盈亏）"),
    ("rules", "同 /status（和 Railway 机器人的 /rules 一样的内容）"),
    ("check", "自检：登录、账户、余额、授权、下单接口、推送"),
    ("test", "挂单测试：挂一张约 $1 的低价单、查到、撤掉"),
    ("pause", "暂停开新单（挂着的单当场撤）"),
    ("resume", "恢复"),
    ("mode", "切换模式：/mode off | paper | on"),
    ("id", "查你的用户 ID 和这个聊天的 ID"),
];

pub type Handler = Rc<dyn Fn(String, String) -> LocalBoxFuture<'static, String>>;
pub type Chat = (i64, i64);
/// A message waiting to go: the chat, the text, the bot's token, its buttons.
pub type Outgoing = (Chat, String, String, Option<Value>);
/// The menu card's head (a line or two of what is in force) and the mode in force.
pub type MenuHead = Rc<dyn Fn() -> (String, String)>;

/// What a failure to reach the commands' bot usually means, and what to do.
pub fn hint(text: &str) -> &'static str {
    if ["401", "Unauthorized", "404", "Not Found"].iter().any(|w| text.contains(w)) {
        "（机器人 Token 不对：到 @BotFather 核对，再 bash deploy/gcp/rt.sh edit 改）"
    } else if text.contains("409") || text.contains("Conflict") {
        "（另一个程序也在用这个机器人收消息——同一个 Token 只能一个程序收：给本服务单独建一个机器人）"
    } else if text.contains("chat not found") {
        "（还没在机器人的对话里按 Start，或 TELEGRAM_CHAT_ID 写错了）"
    } else if text.contains("blocked") || text.contains("Forbidden") {
        "（机器人被屏蔽了，或不在那个群里）"
    } else {
        ""
    }
}

/// The commands a button may run: those that start real orders again only after a "yes".
fn needs_yes(cmd: &str) -> bool {
    matches!(cmd, "/resume" | "/mode on" | "/test")
}

fn button_command(cmd: &str) -> bool {
    matches!(cmd, "/status" | "/check" | "/pause" | "/mode off" | "/mode paper") || needs_yes(cmd)
}

/// The card asking for the "yes" (valid CONFIRM_S seconds).
pub fn confirm_card(cmd: &str, now_s: i64) -> (String, Value) {
    let (what, why) = match cmd {
        "/resume" => ("恢复开新单", "恢复后按现在的模式继续：真实吃单模式下会真实下单。"),
        "/mode on" => ("切到真实吃单", "盘口出现规则价以下的卖单就真实下单（不会高于设置文件的 RULE_TAKER）。"),
        "/test" => ("做一次挂单测试", "挂一张约 $1、远低于市价的真实买单，查到后马上撤掉。"),
        _ => ("执行这个操作", ""),
    };
    (
        format!("确定要{what}吗？\n{why}\n（{CONFIRM_S} 秒内点“确定”有效）"),
        json!({"inline_keyboard": [[{"text": "✅ 确定", "callback_data": format!("y:{now_s}:{cmd}")},
                                    {"text": "取消", "callback_data": "m:"}]]}),
    )
}

/// A long text cut at line ends into messages of at most ``limit`` characters.
pub fn chunks(text: &str, limit: usize) -> Vec<String> {
    let mut out = vec![];
    let mut part = String::new();
    let mut part_len = 0usize;
    for line in text.split('\n') {
        let mut line: Vec<char> = line.chars().collect();
        while line.len() > limit {
            if !part.is_empty() {
                out.push(std::mem::take(&mut part));
                part_len = 0;
            }
            out.push(line[..limit].iter().collect());
            line = line[limit..].to_vec();
        }
        let line: String = line.iter().collect();
        let n = line.chars().count();
        if part_len + n + 1 > limit {
            out.push(std::mem::replace(&mut part, line));
            part_len = n;
        } else if part.is_empty() {
            part = line;
            part_len = n;
        } else {
            part.push('\n');
            part.push_str(&line);
            part_len += n + 1;
        }
    }
    if !part.is_empty() {
        out.push(part);
    }
    if out.is_empty() {
        out.push(String::new());
    }
    out
}

pub struct Telegram {
    pub token: String,
    pub admin: i64,
    pub commands: bool,
    pub cmd_token: String,
    pub chats: Vec<Chat>,
    pub handler: RefCell<Option<Handler>>,
    pub menu_head: RefCell<Option<MenuHead>>,
    pub enabled: bool,
    pub queue: RefCell<VecDeque<Outgoing>>,
    wake: Event,
    pub sent: Cell<u64>,
    pub failed: Cell<u64>,
    pub error: RefCell<String>,
    offset: Cell<i64>,
    stopping: Event,
    pub transport: Rc<dyn Transport>,
    pub base: String,
    /// The commands' bot as Telegram names it (getMe), once it answered.
    pub bot_name: RefCell<String>,
    /// The commands' last failure to be polled (getMe, getUpdates): "" while they come through.
    pub poll_error: RefCell<String>,
    /// A webhook set on the commands' bot (its host): another program takes its updates.
    pub webhook: RefCell<String>,
    /// The last command from someone not ADMIN_USER_ID (their user id, the command); whether the admin's own came since.
    pub stranger: RefCell<Option<(i64, String)>>,
    pub admin_seen: Cell<bool>,
    logged: RefCell<String>,
}

impl Telegram {
    pub fn new(
        token: &str,
        chats: &[Chat],
        admin: i64,
        commands: bool,
        enabled: bool,
        command_token: &str,
        transport: Rc<dyn Transport>,
    ) -> Telegram {
        let chats: Vec<Chat> = if chats.is_empty() && admin != 0 { vec![(admin, 0)] } else { chats.to_vec() };
        Telegram {
            token: token.to_string(),
            admin,
            commands,
            cmd_token: if command_token.is_empty() { token.to_string() } else { command_token.to_string() },
            enabled: enabled && !token.is_empty() && !chats.is_empty(),
            chats,
            handler: RefCell::new(None),
            menu_head: RefCell::new(None),
            queue: RefCell::new(VecDeque::new()),
            wake: Event::new(),
            sent: Cell::new(0),
            failed: Cell::new(0),
            error: RefCell::new(String::new()),
            offset: Cell::new(0),
            stopping: Event::new(),
            transport,
            base: API.to_string(),
            bot_name: RefCell::new(String::new()),
            poll_error: RefCell::new(String::new()),
            webhook: RefCell::new(String::new()),
            stranger: RefCell::new(None),
            admin_seen: Cell::new(false),
            logged: RefCell::new(String::new()),
        }
    }

    pub async fn call(&self, method: &str, payload: Value, timeout: f64, token: &str) -> Res<Value> {
        let token = if token.is_empty() { &self.token } else { token };
        let req = Req {
            url: format!("{}/bot{token}/{method}", self.base),
            payload: Some(Rc::new(serde_json::to_vec(&payload).unwrap_or_default())),
            timeout,
            headers: vec![],
            gate: None,
        };
        let raw = self.transport.request(req).await?;
        let answer: Value = serde_json::from_slice(&raw).map_err(|_| RemoteError::new("接口未返回有效 JSON"))?;
        if !answer.is_object() || !answer.t("ok") {
            let said = if answer.is_object() {
                answer.get("description").map(crate::util::py_str).unwrap_or_else(|| "None".into())
            } else {
                answer.to_string()
            };
            return Err(RemoteError::new(format!("Telegram 拒绝：{said}")));
        }
        Ok(answer.get("result").cloned().unwrap_or(Value::Null))
    }

    /// Queued for the chats (the configured ones by default), through ``token``'s bot (the notifications' by default).
    pub fn send_to(&self, text: &str, chats: Option<&[Chat]>, token: &str) {
        let token_used = if token.is_empty() { self.token.as_str() } else { token };
        let chats = chats.unwrap_or(&self.chats);
        if text.is_empty() || token_used.is_empty() || chats.is_empty() || (token.is_empty() && !self.enabled) {
            return;
        }
        let mut queue = self.queue.borrow_mut();
        for chat in chats {
            for part in chunks(text, LIMIT) {
                queue.push_back((*chat, part, token_used.to_string(), None));
                while queue.len() > 200 {
                    queue.pop_front();
                }
            }
        }
        drop(queue);
        self.wake.set();
    }

    pub fn send(&self, text: &str) {
        self.send_to(text, None, "");
    }

    /// An answer to a command: back through the bot the command came to.
    pub fn reply(&self, text: &str, chat: Chat) {
        let token = self.cmd_token.clone();
        self.send_to(text, Some(&[chat]), &token);
    }

    /// A card (a message with buttons) to a chat, through the commands' bot.
    pub fn send_card(&self, chat: Chat, card: (String, Value)) {
        if self.cmd_token.is_empty() {
            return;
        }
        let mut queue = self.queue.borrow_mut();
        queue.push_back((chat, card.0, self.cmd_token.clone(), Some(card.1)));
        while queue.len() > 200 {
            queue.pop_front();
        }
        drop(queue);
        self.wake.set();
    }

    /// The menu: what is in force now, and a button for each thing the admin does most.
    pub fn menu_card(&self) -> (String, Value) {
        let head = self.menu_head.borrow().clone();
        let (head, mode) = head.map(|f| f()).unwrap_or_default();
        let tick = |m: &str, label: &str| if mode == m { format!("✅ {label}") } else { label.to_string() };
        let b = |text: String, data: &str| json!({"text": text, "callback_data": data});
        let keyboard = json!([
            [b("📊 状态".into(), "c:/status"), b("🔎 自检".into(), "c:/check")],
            [b("⏸ 暂停".into(), "c:/pause"), b("▶️ 恢复".into(), "c:/resume")],
            [
                b(tick("off", "关"), "c:/mode off"),
                b(tick("paper", "只记录"), "c:/mode paper"),
                b(tick("on", "真实吃单"), "c:/mode on")
            ],
            [b("🧪 挂单测试".into(), "c:/test"), b("🔄 刷新".into(), "m:")],
        ]);
        (format!("📋 规则吃单菜单\n{head}"), json!({"inline_keyboard": keyboard}))
    }

    /// The self-check's Telegram line: (fine, text) — not fine says what keeps the bot from answering, and what to do.
    pub fn health(&self) -> (bool, String) {
        if self.cmd_token.is_empty() && self.token.is_empty() {
            return (false, "Telegram：没设置机器人（不发通知、不收命令；要用的话见 README“Telegram：怎么绑定”）".into());
        }
        let failed = self.failed.get();
        let notes = if failed > 0 {
            let error = self.error.borrow().clone();
            format!("；通知发送失败 {failed} 次（{error}{}）", hint(&error))
        } else if self.sent.get() > 0 {
            format!("；已发 {} 条消息", self.sent.get())
        } else {
            String::new()
        };
        let bot = self.bot_name.borrow().clone();
        let bot = if bot.is_empty() { "（还没连上）".to_string() } else { bot };
        if !self.commands {
            return (
                false,
                format!(
                    "Telegram：TELEGRAM_COMMANDS 不是 on：机器人只发通知、不收命令（菜单、/status 都不会回）——bash deploy/gcp/rt.sh edit 改成 on{notes}"
                ),
            );
        }
        let webhook = self.webhook.borrow().clone();
        if !webhook.is_empty() {
            return (
                false,
                format!(
                    "Telegram：机器人 {bot} 设了 webhook（{webhook}）：别的程序在用这个 Token，本服务收不到命令——给本服务单独建一个机器人{notes}"
                ),
            );
        }
        let error = self.poll_error.borrow().clone();
        if !error.is_empty() {
            return (false, format!("Telegram：收不到命令：{error}{}{notes}", hint(&error)));
        }
        if self.bot_name.borrow().is_empty() {
            return (false, format!("Telegram：正在连接命令机器人（刚启动的话等几秒）{notes}"));
        }
        if self.admin == 0 {
            return (
                false,
                format!(
                    "Telegram：机器人 {bot} 在收命令，但 ADMIN_USER_ID 没填：除了 /id 都不回——给机器人发 /id，把回的用户 ID 填进 ADMIN_USER_ID（bash deploy/gcp/rt.sh edit）{notes}"
                ),
            );
        }
        if let Some((user, command)) = self.stranger.borrow().clone() {
            if !self.admin_seen.get() {
                return (
                    false,
                    format!(
                        "Telegram：用户 {user} 发过 {command}，但 ADMIN_USER_ID 是 {}：要是 {user} 是你，把 ADMIN_USER_ID 改成 {user}（bash deploy/gcp/rt.sh edit）{notes}",
                        self.admin
                    ),
                );
            }
        }
        (failed == 0, format!("Telegram：机器人 {bot} 在收命令（管理员 {}；发 /menu 出按钮菜单）{notes}", self.admin))
    }

    /// The commands' polling failed: kept for the self-check, logged when it changed.
    fn poll_failed(&self, error: &RemoteError) {
        let text = clean_error(error);
        *self.error.borrow_mut() = text.clone();
        *self.poll_error.borrow_mut() = text.clone();
        if *self.logged.borrow() != text {
            crate::warn!("Telegram 收不到命令：{text}{}", hint(&text));
            *self.logged.borrow_mut() = text;
        }
    }

    async fn answer_button(&self, id: &str, text: &str) {
        let token = self.cmd_token.clone();
        let _ = self.call("answerCallbackQuery", json!({"callback_query_id": id, "text": text}), 10.0, &token).await;
    }

    async fn edit_card(&self, chat: i64, message_id: i64, card: (String, Value)) {
        let token = self.cmd_token.clone();
        let payload = json!({"chat_id": chat, "message_id": message_id, "text": card.0, "reply_markup": card.1});
        let _ = self.call("editMessageText", payload, 15.0, &token).await; // ("message is not modified": nothing to do)
    }

    pub fn stop(&self) {
        self.stopping.set();
    }

    pub async fn run(self: Rc<Self>) {
        let sender = spawn(Self::send_loop(self.clone()));
        let poller = if self.commands && !self.cmd_token.is_empty() { Some(spawn(Self::poll_loop(self.clone()))) } else { None };
        self.stopping.wait().await;
        self.drain(5.0).await;
        sender.abort();
        if let Some(p) = poller {
            p.abort();
        }
    }

    pub async fn drain(&self, seconds: f64) {
        let deadline = mono() + seconds;
        while !self.queue.borrow().is_empty() && mono() < deadline {
            self.send_one().await;
        }
    }

    async fn send_one(&self) {
        let Some(((chat, thread), text, token, markup)) = self.queue.borrow_mut().pop_front() else { return };
        let mut payload = json!({"chat_id": chat, "text": text, "disable_web_page_preview": true});
        if thread != 0 {
            payload["message_thread_id"] = json!(thread);
        }
        if let Some(markup) = markup {
            payload["reply_markup"] = markup;
        }
        for attempt in 0..3u64 {
            match self.call("sendMessage", payload.clone(), 15.0, &token).await {
                Ok(_) => {
                    self.sent.set(self.sent.get() + 1);
                    return;
                }
                Err(error) => {
                    *self.error.borrow_mut() = clean_error(&error);
                    let wait = if error.retry_after > 0 { error.retry_after } else { 1 + attempt * 2 };
                    if error.msg.contains("HTTP 4") && !error.msg.contains("429") {
                        break; // (refused for good: a wrong chat, a bot not in the group)
                    }
                    sleep(wait.min(30) as f64).await;
                }
            }
        }
        self.failed.set(self.failed.get() + 1);
        crate::warn!("Telegram 消息没发出去：{}", self.error.borrow());
    }

    async fn send_loop(self: Rc<Self>) {
        while !self.stopping.is_set() {
            if self.queue.borrow().is_empty() {
                self.wake.clear();
                let _ = self.wake.wait_for(30.0).await;
                continue;
            }
            self.send_one().await;
            sleep(0.35).await; // (Telegram: about one message a second a chat, thirty a second in all)
        }
    }

    async fn poll_loop(self: Rc<Self>) {
        let token = self.cmd_token.clone();
        // who the commands' bot is (a wrong token fails here), and whether another program took its updates (a webhook)
        while !self.stopping.is_set() {
            match self.call("getMe", json!({}), 15.0, &token).await {
                Ok(me) => {
                    *self.bot_name.borrow_mut() = format!("@{}", me.s("username"));
                    break;
                }
                Err(error) => {
                    self.poll_failed(&error);
                    sleep(30.0).await; // (a wrong token stays wrong: asked again now and then, not hammered)
                }
            }
        }
        if let Ok(info) = self.call("getWebhookInfo", json!({}), 15.0, &token).await {
            let url = info.s("url");
            if !url.is_empty() {
                let host = url.split("://").nth(1).unwrap_or(&url).split('/').next().unwrap_or("").to_string();
                crate::warn!(
                    "Telegram：命令机器人 {} 设了 webhook（{host}）：别的程序在用这个 Token，本服务收不到命令——给本服务单独建一个机器人",
                    self.bot_name.borrow()
                );
                *self.webhook.borrow_mut() = host;
            }
        }
        let menu: Vec<Value> = COMMANDS.iter().map(|(c, d)| json!({"command": c, "description": d})).collect();
        let _ = self.call("setMyCommands", json!({"commands": menu}), 15.0, &token).await;
        crate::info!(
            "Telegram 命令机器人 {} 开始收命令（{}）",
            self.bot_name.borrow(),
            if self.admin != 0 { format!("管理员 {}", self.admin) } else { "ADMIN_USER_ID 没填：只回 /id".into() }
        );
        while !self.stopping.is_set() {
            let updates = match self
                .call(
                    "getUpdates",
                    json!({"offset": self.offset.get(), "timeout": 25, "allowed_updates": ["message", "callback_query"]}),
                    35.0,
                    &token,
                )
                .await
            {
                Ok(u) => {
                    if !self.poll_error.borrow().is_empty() {
                        crate::info!("Telegram 收命令恢复正常");
                        self.poll_error.borrow_mut().clear();
                        self.logged.borrow_mut().clear();
                    }
                    u
                }
                Err(error) => {
                    self.poll_failed(&error);
                    sleep(if error.msg.contains("409") { 10.0 } else { 3.0 }).await; // 409: another process polls this token
                    continue;
                }
            };
            for update in updates.as_array().map(Vec::as_slice).unwrap_or(&[]) {
                self.offset.set(self.offset.get().max(update.i("update_id") + 1));
                self.on_update(update);
            }
        }
    }

    /// One update: /id answered to anyone (how the admin finds the number), the rest to the admin alone; a button's tap.
    pub fn on_update(self: &Rc<Self>, update: &Value) {
        if let Some(query) = update.get("callback_query").filter(|q| q.is_object()) {
            self.on_button(query);
            return;
        }
        let empty = json!({});
        let msg = update.get("message").filter(|m| m.is_object()).unwrap_or(&empty);
        let text = msg.s("text").trim().to_string();
        let chat = msg.get("chat").and_then(|c| c.get("id")).and_then(Value::as_i64);
        let thread = msg.i("message_thread_id");
        let user = msg.get("from").map(|f| f.i("id")).unwrap_or(0);
        let Some(chat) = chat else { return };
        if !text.starts_with('/') {
            return;
        }
        let (command, args) = text.split_once(' ').unwrap_or((&text, ""));
        let command = command.split('@').next().unwrap_or("").to_lowercase();
        if command == "/id" {
            let shown = if thread != 0 { format!("{chat}:{thread}") } else { chat.to_string() };
            self.reply(&format!("你的用户 ID：{user}\n这个聊天：{shown}"), (chat, thread));
            return;
        }
        if self.admin == 0 || user != self.admin {
            // someone not the admin: kept for the self-check; in a private chat told their own id (a wrong ADMIN_USER_ID found
            // at once, by whoever it is)
            crate::info!("Telegram：用户 {user} 发了 {command}，不是 ADMIN_USER_ID（{}），不理", self.admin);
            *self.stranger.borrow_mut() = Some((user, command.clone()));
            if chat == user {
                self.reply(
                    &format!("这个机器人只听管理员的命令。你的用户 ID 是 {user}：要是你就是管理员，把设置里的 ADMIN_USER_ID 改成它（bash deploy/gcp/rt.sh edit）"),
                    (chat, thread),
                );
            }
            return;
        }
        self.admin_seen.set(true);
        if command == "/menu" || command == "/start" {
            self.send_card((chat, thread), self.menu_card());
            return;
        }
        let Some(handler) = self.handler.borrow().clone() else { return };
        // each command in a task of its own: a self-check of a few seconds holds no other command (nor the polling) up
        let me = self.clone();
        let args = args.trim().to_string();
        spawn(async move {
            if command == "/check" {
                me.reply("🔎 自检中，结果几秒后发来…", (chat, thread));
            } else if command == "/test" {
                me.reply("🧪 挂单测试中（约 $1 的低价单，挂上就撤），结果几秒后发来…", (chat, thread));
            }
            let reply = handler(command, args).await;
            if !reply.is_empty() {
                me.reply(&reply, (chat, thread));
            }
        });
    }

    /// A tap on a menu button (the admin's alone): "c:" a command (one that starts real orders asks first), "y:<at>:" its
    /// "yes" (CONFIRM_S seconds), "m:" the menu again. The command's answer comes as a message; the card then shows what is
    /// in force.
    fn on_button(self: &Rc<Self>, query: &Value) {
        let id = query.s("id");
        let user = query.get("from").map(|f| f.i("id")).unwrap_or(0);
        let data = query.s("data");
        let empty = json!({});
        let msg = query.get("message").filter(|m| m.is_object()).unwrap_or(&empty);
        let chat = msg.get("chat").and_then(|c| c.get("id")).and_then(Value::as_i64).unwrap_or(0);
        let thread = msg.i("message_thread_id");
        let message_id = msg.i("message_id");
        let me = self.clone();
        spawn(async move {
            if me.admin == 0 || user != me.admin {
                me.answer_button(&id, "这个机器人只听管理员的").await;
                return;
            }
            me.admin_seen.set(true);
            let (kind, rest) = data.split_once(':').unwrap_or((data.as_str(), ""));
            let cmd = match kind {
                "c" if button_command(rest) && needs_yes(rest) => {
                    me.answer_button(&id, "").await;
                    me.edit_card(chat, message_id, confirm_card(rest, crate::util::now_ms() / 1000)).await;
                    return;
                }
                "c" if button_command(rest) => rest.to_string(),
                "y" => {
                    let (at, cmd) = rest.split_once(':').unwrap_or(("0", ""));
                    let age = crate::util::now_ms() / 1000 - at.parse::<i64>().unwrap_or(0);
                    if !button_command(cmd) || !(0..=CONFIRM_S).contains(&age) {
                        me.answer_button(&id, "确认已过期，请重新点一次").await;
                        me.edit_card(chat, message_id, me.menu_card()).await;
                        return;
                    }
                    cmd.to_string()
                }
                _ => {
                    me.answer_button(&id, "").await;
                    me.edit_card(chat, message_id, me.menu_card()).await;
                    return;
                }
            };
            let (command, args) = cmd.split_once(' ').unwrap_or((cmd.as_str(), ""));
            me.answer_button(&id, "收到").await;
            if command == "/check" {
                me.reply("🔎 自检中，结果几秒后发来…", (chat, thread));
            } else if command == "/test" {
                me.reply("🧪 挂单测试中（约 $1 的低价单，挂上就撤），结果几秒后发来…", (chat, thread));
            }
            let handler = me.handler.borrow().clone();
            if let Some(handler) = handler {
                let reply = handler(command.to_string(), args.to_string()).await;
                if !reply.is_empty() {
                    me.reply(&reply, (chat, thread));
                }
            }
            me.edit_card(chat, message_id, me.menu_card()).await;
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunking() {
        let text = format!("{}\n{}\n{}", "a".repeat(10), "b".repeat(10), "c".repeat(25));
        assert_eq!(chunks(&text, 22), vec!["a".repeat(10) + "\n" + &"b".repeat(10), "c".repeat(22), "c".repeat(3)]);
        assert_eq!(chunks("", 10), vec![String::new()]);
        assert_eq!(chunks("x\ny", 10), vec!["x\ny".to_string()]);
    }
}
