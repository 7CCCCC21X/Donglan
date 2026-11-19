//! Telegram: the fills, the results and the problems pushed to the chats (TELEGRAM_CHAT_ID), and — with TELEGRAM_COMMANDS=on —
//! the admin's commands (ADMIN_USER_ID): /status, /check（自检）, /test（挂单测试）, /pause, /resume, /mode, /id.
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
pub const COMMANDS: [(&str, &str); 8] = [
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
    pub enabled: bool,
    pub queue: RefCell<VecDeque<(Chat, String, String)>>,
    wake: Event,
    pub sent: Cell<u64>,
    pub failed: Cell<u64>,
    pub error: RefCell<String>,
    offset: Cell<i64>,
    stopping: Event,
    pub transport: Rc<dyn Transport>,
    pub base: String,
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
            queue: RefCell::new(VecDeque::new()),
            wake: Event::new(),
            sent: Cell::new(0),
            failed: Cell::new(0),
            error: RefCell::new(String::new()),
            offset: Cell::new(0),
            stopping: Event::new(),
            transport,
            base: API.to_string(),
        }
    }

    pub async fn call(&self, method: &str, payload: Value, timeout: f64, token: &str) -> Res<Value> {
        let token = if token.is_empty() { &self.token } else { token };
        let req = Req {
            url: format!("{}/bot{token}/{method}", self.base),
            payload: Some(Rc::new(serde_json::to_vec(&payload).unwrap_or_default())),
            timeout,
            headers: vec![],
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
                queue.push_back((*chat, part, token_used.to_string()));
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
        let Some(((chat, thread), text, token)) = self.queue.borrow_mut().pop_front() else { return };
        let mut payload = json!({"chat_id": chat, "text": text, "disable_web_page_preview": true});
        if thread != 0 {
            payload["message_thread_id"] = json!(thread);
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
        let menu: Vec<Value> = COMMANDS.iter().map(|(c, d)| json!({"command": c, "description": d})).collect();
        let token = self.cmd_token.clone();
        let _ = self.call("setMyCommands", json!({"commands": menu}), 15.0, &token).await;
        while !self.stopping.is_set() {
            let updates = match self
                .call(
                    "getUpdates",
                    json!({"offset": self.offset.get(), "timeout": 25, "allowed_updates": ["message"]}),
                    35.0,
                    &token,
                )
                .await
            {
                Ok(u) => u,
                Err(error) => {
                    *self.error.borrow_mut() = clean_error(&error);
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

    /// One update: /id answered to anyone (how the admin finds the number), the rest to the admin alone.
    pub fn on_update(self: &Rc<Self>, update: &Value) {
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
        let handler = self.handler.borrow().clone();
        let (true, Some(handler)) = (self.admin != 0 && user == self.admin, handler) else { return };
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
