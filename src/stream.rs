//! Predict's WebSocket for the rule taker's windows: each listed window's book pushed the moment it changes
//! (predictOrderbook/<market id>), so an offer at a rule's price is bought at once — the taker is told of the change in the very
//! turn that brought it (on_change).
//!
//! Protocol (dev.predict.fun): JSON text frames; the client sends {"method": "subscribe" | "unsubscribe", "requestId": n,
//! "params": [topic]}, answered by {"type": "R", "requestId": n, "success": bool, "error"}; the server pushes {"type": "M",
//! "topic", "data"}, a book as {"version", "marketId", "updateTimestampMs", "bids", "asks": [[price, size], ...]} (an older
//! version than the one in hand is dropped); its heartbeat (topic "heartbeat", every 15 s) is echoed back.
//!
//! RULE_WS_CONNECTIONS (1–3): that many connections, each subscribed to every window; a book is taken from whichever brings it
//! first (the version decides between them).
use crate::http::{Stream, Url, connect, http};
use crate::predict::{predict_levels, push_cap};
use crate::rules::Level;
use crate::sync::Event;
use crate::util::{clean_error, mono, now_ms};
use futures_util::stream::{SplitSink, SplitStream};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use std::cell::{Cell, RefCell};
use std::collections::{BTreeSet, HashMap, HashSet, VecDeque};
use std::rc::Rc;
use std::time::Duration;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::protocol::{Message, WebSocketConfig};

pub const PREDICT_WS_URL: &str = "wss://ws.predict.fun/ws";
pub const TOPIC: &str = "predictOrderbook/";
pub const HEARTBEAT: &str = "heartbeat";

pub type Ws = WebSocketStream<Stream>;

/// A WebSocket connection: no compression (a frame is read the moment it lands), TCP_NODELAY, 15 s to open.
pub async fn ws_connect(url: &str, headers: &[(String, String)]) -> Result<Ws, String> {
    let parsed = Url::parse(url).map_err(|e| e.msg)?;
    let pool = http();
    let opening = async {
        let (stream, _) = connect(parsed.https, &parsed.host, parsed.port, Duration::from_secs(15), pool.tls())
            .await
            .map_err(|e| format!("{}", e))?;
        let mut request = url.into_client_request().map_err(|e| e.to_string())?;
        for (k, v) in headers {
            let name = tokio_tungstenite::tungstenite::http::HeaderName::from_bytes(k.as_bytes()).map_err(|e| e.to_string())?;
            let value = tokio_tungstenite::tungstenite::http::HeaderValue::from_str(v).map_err(|e| e.to_string())?;
            request.headers_mut().insert(name, value);
        }
        request.headers_mut().insert(
            "User-Agent",
            tokio_tungstenite::tungstenite::http::HeaderValue::from_str(&crate::http::user_agent()).map_err(|e| e.to_string())?,
        );
        let config = WebSocketConfig::default().max_message_size(Some(4 << 20)).max_frame_size(Some(4 << 20));
        let (ws, _) = tokio_tungstenite::client_async_with_config(request, stream, Some(config)).await.map_err(|e| e.to_string())?;
        Ok::<Ws, String>(ws)
    };
    tokio::time::timeout(Duration::from_secs(15), opening).await.map_err(|_| "timed out during opening handshake".to_string())?
}

pub struct Book {
    pub bids: Rc<Vec<Level>>,
    pub asks: Rc<Vec<Level>>,
    /// monotonic time of the push (or the REST read)
    pub at: f64,
}

pub type OnChange = Rc<dyn Fn(&str) -> bool>;

#[derive(Default)]
pub struct ConnState {
    pub subscribed: RefCell<BTreeSet<String>>,
    pending: RefCell<HashMap<i64, (String, String)>>,
    request_id: Cell<i64>,
    pub connected: Cell<bool>,
    pub connects: Cell<u64>,
    pub frames: Cell<u64>,
    pub last_frame: Cell<f64>,
    pub error: RefCell<String>,
    backoff: Cell<f64>,
    wake: Event,
}

impl ConnState {
    pub fn alive(&self) -> bool {
        self.connected.get() && mono() - self.last_frame.get() <= BookStream::ALIVE_S
    }
}

pub struct BookStream {
    pub key: String,
    pub url: String,
    pub wanted: RefCell<BTreeSet<String>>,
    pub books: RefCell<HashMap<String, Book>>,
    versions: RefCell<HashMap<String, i64>>,
    /// markets whose book changed since the taker last looked
    pub changed: RefCell<HashSet<String>>,
    pub arrivals: Cell<u64>,
    pub pushed: Event,
    /// told of a change on the spot; true: it took everything there was
    pub on_change: RefCell<Option<OnChange>>,
    /// market id -> (Predict's stamp on the book in hand — 0 for a REST read —, its arrival, wall ms)
    pub stamps: RefCell<HashMap<String, (i64, i64)>>,
    pub lags: RefCell<VecDeque<i64>>,
    pub conns: Vec<ConnState>,
    pub stopping: Event,
    /// the tests' stand-in for a live connection
    pub fake_alive: Cell<Option<bool>>,
}

impl BookStream {
    pub const FRESH_S: f64 = 20.0;
    pub const ALIVE_S: f64 = 25.0;
    pub const SILENT_S: f64 = 45.0;
    pub const BACKOFF: (f64, f64) = (2.0, 120.0);
    pub const SYNC_S: f64 = 30.0;
    pub const PING_S: f64 = 20.0;

    pub fn new(key: &str, connections: usize) -> BookStream {
        BookStream::with_url(key, connections, PREDICT_WS_URL)
    }

    pub fn with_url(key: &str, connections: usize, url: &str) -> BookStream {
        BookStream {
            key: key.to_string(),
            url: url.to_string(),
            wanted: RefCell::new(BTreeSet::new()),
            books: RefCell::new(HashMap::new()),
            versions: RefCell::new(HashMap::new()),
            changed: RefCell::new(HashSet::new()),
            arrivals: Cell::new(0),
            pushed: Event::new(),
            on_change: RefCell::new(None),
            stamps: RefCell::new(HashMap::new()),
            lags: RefCell::new(VecDeque::new()),
            conns: (0..connections.max(1)).map(|_| ConnState { backoff: Cell::new(Self::BACKOFF.0), ..Default::default() }).collect(),
            stopping: Event::new(),
            fake_alive: Cell::new(None),
        }
    }

    // -- what the taker asks --
    pub fn want(&self, ids: &BTreeSet<String>) {
        if *ids != *self.wanted.borrow() {
            *self.wanted.borrow_mut() = ids.clone();
            for c in &self.conns {
                c.wake.set();
            }
        }
    }

    pub fn alive(&self) -> bool {
        if let Some(v) = self.fake_alive.get() {
            return v;
        }
        self.conns.iter().any(ConnState::alive)
    }

    pub fn connected(&self) -> bool {
        if let Some(v) = self.fake_alive.get() {
            return v;
        }
        self.conns.iter().any(|c| c.connected.get())
    }

    pub fn error(&self) -> String {
        self.conns.iter().map(|c| c.error.borrow().clone()).find(|e| !e.is_empty()).unwrap_or_default()
    }

    pub fn connects(&self) -> u64 {
        self.conns.iter().map(|c| c.connects.get()).sum()
    }

    /// (bids, asks) as last pushed while a connection that has it lives, None when there is none fresh.
    pub fn book(&self, mid: &str) -> Option<(Rc<Vec<Level>>, Rc<Vec<Level>>)> {
        let books = self.books.borrow();
        let got = books.get(mid)?;
        if !self.connected() || mono() - got.at > Self::FRESH_S {
            return None;
        }
        Some((got.bids.clone(), got.asks.clone()))
    }

    /// The book in hand however old: (bids, asks, its monotonic time).
    pub fn held(&self, mid: &str) -> Option<(Rc<Vec<Level>>, Rc<Vec<Level>>, f64)> {
        self.books.borrow().get(mid).map(|b| (b.bids.clone(), b.asks.clone(), b.at))
    }

    pub fn stamp(&self, mid: &str) -> Option<(i64, i64)> {
        self.stamps.borrow().get(mid).copied()
    }

    /// The taker looks at these markets now though their books have not changed. (Never called while the taker is borrowed:
    /// it is told on the spot.)
    pub fn nudge(&self, mids: &[String]) {
        for mid in mids {
            self.changed.borrow_mut().insert(mid.clone());
            self.arrivals.set(self.arrivals.get() + 1);
            self.told(mid);
        }
    }

    pub fn told(&self, mid: &str) {
        let hook = self.on_change.borrow().clone();
        let took = hook.map(|f| f(mid)).unwrap_or(false);
        if !took {
            self.pushed.set();
        }
    }

    /// A REST read of a window's book beside the pushes: dropped when a push came after the read was asked for; else the book
    /// in hand. True when it differed from a book held — a change no push brought.
    pub fn merge(&self, mid: &str, bids: Vec<Level>, asks: Vec<Level>, asked: f64) -> bool {
        let differed = {
            let mut books = self.books.borrow_mut();
            let old = books.get(mid);
            if let Some(o) = old {
                if o.at > asked {
                    return false;
                }
            }
            let same = old.map(|o| *o.bids == bids && *o.asks == asks).unwrap_or(false);
            let had = old.is_some();
            books.insert(mid.to_string(), Book { bids: Rc::new(bids), asks: Rc::new(asks), at: mono() });
            if same {
                return false;
            }
            self.stamps.borrow_mut().insert(mid.to_string(), (0, now_ms()));
            self.changed.borrow_mut().insert(mid.to_string());
            self.arrivals.set(self.arrivals.get() + 1);
            had
        };
        self.told(mid);
        differed
    }

    /// A pushed book: kept (an older version dropped, a side left out keeps its own), the taker told when it changed.
    pub fn take(&self, mid: &str, data: &Value) {
        if !self.wanted.borrow().contains(mid) || !data.is_object() {
            return;
        }
        if let Some(version) = data.get("version").filter(|v| v.is_number()) {
            let v = version.as_f64().unwrap_or(0.0);
            let mut versions = self.versions.borrow_mut();
            if v < *versions.get(mid).unwrap_or(&-1) as f64 {
                return;
            }
            versions.insert(mid.to_string(), v as i64);
        }
        let changed = {
            let mut books = self.books.borrow_mut();
            let old = books.get(mid);
            let bids = match data.get("bids") {
                Some(rows @ Value::Array(_)) => Rc::new(predict_levels(rows, true)),
                _ => old.map(|o| o.bids.clone()).unwrap_or_default(),
            };
            let asks = match data.get("asks") {
                Some(rows @ Value::Array(_)) => Rc::new(predict_levels(rows, false)),
                _ => old.map(|o| o.asks.clone()).unwrap_or_default(),
            };
            let same = old.map(|o| *o.bids == *bids && *o.asks == *asks).unwrap_or(false);
            let had = old.is_some();
            books.insert(mid.to_string(), Book { bids, asks, at: mono() });
            if same {
                false
            } else {
                let wall = now_ms();
                let stamp = data.get("updateTimestampMs").and_then(Value::as_f64).unwrap_or(0.0);
                let stamp = if stamp > 0.0 && stamp <= (wall + 60_000) as f64 { stamp as i64 } else { 0 };
                self.stamps.borrow_mut().insert(mid.to_string(), (stamp, wall));
                if stamp != 0 && had {
                    push_cap(&mut self.lags.borrow_mut(), wall - stamp, 300);
                }
                self.changed.borrow_mut().insert(mid.to_string());
                self.arrivals.set(self.arrivals.get() + 1);
                true
            }
        };
        if changed {
            self.told(mid);
        }
    }

    pub fn forget(&self, mid: &str) {
        if self.conns.iter().all(|c| !c.subscribed.borrow().contains(mid)) {
            self.books.borrow_mut().remove(mid);
            self.versions.borrow_mut().remove(mid);
            self.stamps.borrow_mut().remove(mid);
            self.changed.borrow_mut().remove(mid);
        }
    }

    pub fn headers(&self) -> Vec<(String, String)> {
        if self.key.is_empty() { vec![] } else { vec![("x-api-key".to_string(), self.key.clone())] }
    }

    pub fn stop(&self) {
        self.stopping.set();
    }

    pub async fn run(self: Rc<Self>) {
        let tasks: Vec<_> = (0..self.conns.len()).map(|n| tokio::task::spawn_local(Self::conn_loop(self.clone(), n))).collect();
        self.stopping.wait().await;
        for t in tasks {
            t.abort();
        }
    }

    async fn conn_loop(self: Rc<Self>, n: usize) {
        let c = &self.conns[n];
        while !self.stopping.is_set() {
            if self.wanted.borrow().is_empty() {
                c.wake.clear();
                tokio::select! {
                    _ = c.wake.wait_for(5.0) => {}
                    _ = self.stopping.wait() => {}
                }
                continue;
            }
            let headers = self.headers();
            let ws = tokio::select! {
                got = ws_connect(&self.url, &headers) => got,
                _ = self.stopping.wait() => return,
            };
            let ws = match ws {
                Ok(ws) => ws,
                Err(error) => {
                    *c.error.borrow_mut() = clean_error(&error);
                    self.pause(c.backoff.get()).await;
                    c.backoff.set((c.backoff.get() * 2.0).min(Self::BACKOFF.1));
                    continue;
                }
            };
            let since = mono();
            c.last_frame.set(since);
            c.connected.set(true);
            c.connects.set(c.connects.get() + 1);
            c.subscribed.borrow_mut().clear();
            c.pending.borrow_mut().clear();
            self.versions.borrow_mut().clear(); // (a fresh connection's snapshot starts the versions over)
            let why = self.session(n, ws).await;
            c.connected.set(false);
            let gone: Vec<String> = std::mem::take(&mut *c.subscribed.borrow_mut()).into_iter().collect();
            for mid in gone {
                self.forget(&mid);
            }
            if self.stopping.is_set() {
                return;
            }
            *c.error.borrow_mut() = if why.is_empty() { "连接结束".into() } else { why };
            crate::warn!("Predict 推送连接 {n} 断开：{}", c.error.borrow());
            c.backoff.set(if mono() - since >= 60.0 { Self::BACKOFF.0 } else { (c.backoff.get() * 2.0).min(Self::BACKOFF.1) });
            self.pause(c.backoff.get()).await;
        }
    }

    async fn pause(&self, seconds: f64) {
        let _ = self.stopping.wait_for(seconds).await;
    }

    async fn sync(&self, n: usize, sink: &mut SplitSink<Ws, Message>) -> Result<(), String> {
        let c = &self.conns[n];
        c.wake.clear();
        let wanted = self.wanted.borrow().clone();
        let add: Vec<String> = wanted.difference(&c.subscribed.borrow()).cloned().collect();
        for mid in add {
            self.request(n, sink, "subscribe", &mid).await?;
            c.subscribed.borrow_mut().insert(mid);
        }
        let drop: Vec<String> = c.subscribed.borrow().difference(&wanted).cloned().collect();
        for mid in drop {
            self.request(n, sink, "unsubscribe", &mid).await?;
            c.subscribed.borrow_mut().remove(&mid);
            self.forget(&mid);
        }
        Ok(())
    }

    async fn request(&self, n: usize, sink: &mut SplitSink<Ws, Message>, method: &str, mid: &str) -> Result<(), String> {
        let c = &self.conns[n];
        let id = c.request_id.get() + 1;
        c.request_id.set(id);
        c.pending.borrow_mut().insert(id, (method.to_string(), mid.to_string()));
        let text = json!({"method": method, "requestId": id, "params": [format!("{TOPIC}{mid}")]}).to_string();
        sink.send(Message::text(text)).await.map_err(|e| clean_error(e.to_string()))
    }

    async fn session(&self, n: usize, ws: Ws) -> String {
        let c = &self.conns[n];
        let (mut sink, mut source): (SplitSink<Ws, Message>, SplitStream<Ws>) = ws.split();
        let mut last_sync = mono();
        let mut ping_at = mono() + Self::PING_S;
        let mut pong_due: Option<f64> = None;
        let mut last_message = mono();
        if let Err(e) = self.sync(n, &mut sink).await {
            return e;
        }
        let why = loop {
            if self.stopping.is_set() {
                break String::new();
            }
            let now = mono();
            let silent = last_message + Self::SILENT_S - now;
            let next_sync = last_sync + Self::SYNC_S - now;
            let next_ping = pong_due.unwrap_or(ping_at) - now;
            let wait = silent.min(next_sync).min(next_ping).max(0.0);
            tokio::select! {
                frame = source.next() => {
                    let Some(frame) = frame else { break "连接结束".to_string() };
                    let frame = match frame { Ok(f) => f, Err(e) => break clean_error(e.to_string()) };
                    c.frames.set(c.frames.get() + 1);
                    c.last_frame.set(mono());
                    match frame {
                        Message::Text(text) => {
                            last_message = mono();
                            if let Err(e) = self.handle(n, &mut sink, text.as_str()).await { break e; }
                        }
                        Message::Binary(bytes) => {
                            last_message = mono();
                            if let Ok(text) = std::str::from_utf8(&bytes) {
                                if let Err(e) = self.handle(n, &mut sink, text).await { break e; }
                            }
                        }
                        Message::Pong(_) => { pong_due = None; }
                        Message::Ping(_) => {}
                        Message::Close(_) => break "连接结束".to_string(),
                        Message::Frame(_) => {}
                    }
                }
                _ = c.wake.wait() => {
                    if let Err(e) = self.sync(n, &mut sink).await { break e; }
                    last_sync = mono();
                }
                _ = self.stopping.wait() => break String::new(),
                _ = tokio::time::sleep(Duration::from_secs_f64(wait)) => {
                    let now = mono();
                    if now - last_message >= Self::SILENT_S {
                        break format!("{:.0} 秒没有收到任何帧", Self::SILENT_S);
                    }
                    if let Some(due) = pong_due {
                        if now >= due { break "keepalive ping timeout".to_string(); }
                    } else if now >= ping_at {
                        if let Err(e) = sink.send(Message::Ping(Vec::new().into())).await { break clean_error(e.to_string()); }
                        pong_due = Some(now + Self::PING_S);
                        ping_at = now + Self::PING_S;
                    }
                    if now - last_sync >= Self::SYNC_S {
                        if let Err(e) = self.sync(n, &mut sink).await { break e; }
                        last_sync = mono();
                    }
                }
            }
        };
        let _ = tokio::time::timeout(Duration::from_secs(1), sink.close()).await;
        why
    }

    async fn handle(&self, n: usize, sink: &mut SplitSink<Ws, Message>, raw: &str) -> Result<(), String> {
        let Ok(msg) = serde_json::from_str::<Value>(raw) else { return Ok(()) };
        let Some(obj) = msg.as_object() else { return Ok(()) };
        let c = &self.conns[n];
        match obj.get("type").and_then(Value::as_str) {
            Some("M") => {
                let topic = obj.get("topic").and_then(Value::as_str).unwrap_or("");
                if let Some(mid) = topic.strip_prefix(TOPIC) {
                    let subscribed = c.subscribed.borrow().contains(mid);
                    if subscribed {
                        self.take(mid, obj.get("data").unwrap_or(&Value::Null));
                    }
                } else if topic == HEARTBEAT {
                    let echo = json!({"method": "heartbeat", "data": obj.get("data").cloned().unwrap_or(Value::Null)}).to_string();
                    sink.send(Message::text(echo)).await.map_err(|e| clean_error(e.to_string()))?;
                }
            }
            Some("R") => {
                let id = obj.get("requestId").and_then(|v| v.as_i64().or_else(|| v.as_str().and_then(|s| s.parse().ok())));
                if let Some(id) = id {
                    let found = c.pending.borrow_mut().remove(&id);
                    if let Some((method, mid)) = found {
                        let success = obj.get("success");
                        let failed = matches!(success, Some(Value::Bool(false)))
                            || (matches!(success, None | Some(Value::Null)) && obj.get("error").map(crate::util::truthy).unwrap_or(false));
                        if !mid.is_empty() && failed {
                            if method == "subscribe" {
                                c.subscribed.borrow_mut().remove(&mid); // asked for again at the next sync
                            }
                            let said = obj.get("error").filter(|e| crate::util::truthy(e)).map(crate::util::py_str).unwrap_or_else(|| "拒绝".into());
                            *c.error.borrow_mut() = format!("{method} {mid}：{said}");
                        }
                    }
                }
            }
            _ => {}
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pushes() {
        let s = BookStream::new("", 1);
        s.fake_alive.set(Some(true));
        let told = Rc::new(Cell::new(0));
        let seen = told.clone();
        *s.on_change.borrow_mut() = Some(Rc::new(move |_mid: &str| {
            seen.set(seen.get() + 1);
            true
        }));
        s.want(&["7".to_string()].into_iter().collect());
        s.take("7", &json!({"version": 2, "asks": [[0.58, 10]], "bids": [[0.40, 5]], "updateTimestampMs": now_ms() - 5}));
        assert_eq!(told.get(), 1);
        s.take("7", &json!({"version": 1, "asks": [[0.50, 10]]})); // older: dropped
        assert_eq!(s.book("7").unwrap().1[0].0.to_string(), "0.58");
        s.take("7", &json!({"version": 3, "bids": [[0.41, 5]]})); // the asks kept
        let (bids, asks) = s.book("7").unwrap();
        assert_eq!((bids[0].0.to_string(), asks[0].0.to_string()), ("0.41".to_string(), "0.58".to_string()));
        assert_eq!(told.get(), 2);
        s.take("8", &json!({"asks": [[0.5, 1]]})); // not wanted
        assert!(s.held("8").is_none());
        assert!(!s.merge("7", (*bids).clone(), (*asks).clone(), mono()));
        assert_eq!(told.get(), 2);
    }
}
