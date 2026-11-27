//! The index the in-play rules (盘中) are judged by.
//!
//! The windows settle on Chainlink's stream of the index price (field i) of Binance's USDⓈ-M futures mark price stream: the
//! index is pushed every second (<symbol>@markPrice@1s on wss://fstream.binance.com/market), read over REST (premiumIndex) while
//! the push is late, and each window's start line is the close of the index's 1-minute candle that ends at the start
//! (indexPriceKlines). An index is as fresh as its own event time (E), not its arrival.
use crate::predict::dec_of;
use crate::stream::ws_connect;
use crate::sync::Event;
use crate::util::{RemoteError, Res, as_i64, clean_error, mono, now_ms};
use futures_util::future::LocalBoxFuture;
use futures_util::{SinkExt, StreamExt};
use rust_decimal::Decimal;
use serde_json::Value;
use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, HashMap};
use std::rc::Rc;
use std::time::Duration;
use tokio_tungstenite::tungstenite::protocol::Message;

pub const CANDLE_GRACE_MS: i64 = 1_500;
pub const TRADE_STALE_SHARE: f64 = 0.02;
pub const TRADE_STALE_MIN_MS: f64 = 5_000.0;
pub const REST_EVERY_S: f64 = 1.0;
pub const PUSH_LATE_MS: i64 = 2_500;

pub type Getter = Rc<dyn Fn(String, Vec<(String, String)>) -> LocalBoxFuture<'static, Res<Value>>>;

pub struct IndexFeed {
    pub symbols: Vec<String>,
    pub fapi: String,
    pub ws_base: String,
    /// (path, params) -> JSON: the tests' stand-in for Binance's REST
    pub getter: RefCell<Option<Getter>>,
    pub prices: RefCell<HashMap<String, Decimal>>,
    pub index_ms: RefCell<HashMap<String, i64>>,
    pub priced_ms: RefCell<HashMap<String, i64>>,
    pub candles: RefCell<BTreeMap<(String, i64), Decimal>>,
    pub candle_error: RefCell<BTreeMap<(String, i64), String>>,
    /// (symbol, the index's own time) — the in-play looks
    pub on_change: RefCell<Option<Rc<dyn Fn(&str, i64)>>>,
    /// whether an in-play window needs the index now (the REST read's pace)
    pub needed: RefCell<Option<Rc<dyn Fn() -> bool>>>,
    /// whether any rule buys in its windows' course at all: connected only then
    pub wanted: RefCell<Option<Rc<dyn Fn() -> bool>>>,
    pub connected: Cell<bool>,
    pub error: RefCell<String>,
    pub pushes: Cell<u64>,
    pub reads: Cell<u64>,
    pub stopping: Event,
}

impl IndexFeed {
    pub fn new(symbols: &[&str], fapi: &str, ws_base: &str) -> IndexFeed {
        let mut symbols: Vec<String> = symbols.iter().map(|s| s.to_uppercase()).collect();
        symbols.sort();
        symbols.dedup();
        IndexFeed {
            symbols,
            fapi: fapi.trim_end_matches('/').to_string(),
            ws_base: ws_base.trim_end_matches('/').to_string(),
            getter: RefCell::new(None),
            prices: RefCell::new(HashMap::new()),
            index_ms: RefCell::new(HashMap::new()),
            priced_ms: RefCell::new(HashMap::new()),
            candles: RefCell::new(BTreeMap::new()),
            candle_error: RefCell::new(BTreeMap::new()),
            on_change: RefCell::new(None),
            needed: RefCell::new(None),
            wanted: RefCell::new(None),
            connected: Cell::new(false),
            error: RefCell::new(String::new()),
            pushes: Cell::new(0),
            reads: Cell::new(0),
            stopping: Event::new(),
        }
    }

    fn needed(&self) -> bool {
        let f = self.needed.borrow().clone();
        f.map(|f| f()).unwrap_or(true)
    }

    fn wanted(&self) -> bool {
        let f = self.wanted.borrow().clone();
        f.map(|f| f()).unwrap_or(true)
    }

    fn told(&self, symbol: &str, at: i64) {
        let f = self.on_change.borrow().clone();
        if let Some(f) = f {
            f(symbol, at);
        }
    }

    // -- what the taker asks --
    pub fn price(&self, symbol: &str) -> Option<Decimal> {
        self.prices.borrow().get(&symbol.to_uppercase()).copied()
    }

    /// Why the index in hand is too old to trade on ("" while it is not).
    pub fn price_problem(&self, symbol: &str, end_ms: i64, now_ms: i64) -> String {
        let left = (end_ms - now_ms) as f64;
        let limit = TRADE_STALE_MIN_MS.max(TRADE_STALE_SHARE * left);
        let priced = *self.priced_ms.borrow().get(&symbol.to_uppercase()).unwrap_or(&0);
        let age = (now_ms - priced) as f64;
        if age <= limit {
            return String::new();
        }
        format!(
            "指数 {} 秒未更新，超过离结束 {:.1} 分钟时的上限 {} 秒",
            (age / 1000.0).ceil() as i64,
            left.max(0.0) / 60_000.0,
            crate::util::g((limit / 100.0 + 1e-6).floor() / 10.0)
        )
    }

    /// The start line of the window starting at ``start_ms``: the close of the index's 1-minute candle ending there.
    pub fn close_of(&self, symbol: &str, start_ms: i64) -> Option<Decimal> {
        self.candles.borrow().get(&(symbol.to_uppercase(), start_ms - 60_000)).copied()
    }

    /// An index (pushed, or read): taken when newer than the one held; true when it moved.
    pub fn push_index(&self, symbol: &str, price: Decimal, at_ms: i64, now_ms: i64) -> bool {
        let symbol = symbol.to_uppercase();
        if at_ms <= *self.index_ms.borrow().get(&symbol).unwrap_or(&0) || price <= Decimal::ZERO {
            return false;
        }
        let moved = self.prices.borrow().get(&symbol) != Some(&price);
        self.index_ms.borrow_mut().insert(symbol.clone(), at_ms);
        self.prices.borrow_mut().insert(symbol.clone(), price);
        self.priced_ms.borrow_mut().insert(symbol.clone(), now_ms.min(at_ms));
        if moved {
            self.told(&symbol, at_ms);
        }
        moved
    }

    pub async fn get(&self, path: &str, params: Vec<(String, String)>) -> Res<Value> {
        let getter = self.getter.borrow().clone();
        if let Some(g) = getter {
            return g(path.to_string(), params).await;
        }
        let query: Vec<String> = params.iter().map(|(k, v)| format!("{k}={}", crate::predict::quote(v))).collect();
        let url = format!("{}/fapi/v1/{path}?{}", self.fapi, query.join("&"));
        crate::http::http().request_json(&url, None, 8.0, &[]).await
    }

    /// The close of exactly the index's 1-minute candle that opens at open_ms (a neighbouring minute is never taken).
    pub async fn read_candle(&self, symbol: &str, open_ms: i64) -> Res<Decimal> {
        let params = vec![
            ("pair".to_string(), symbol.to_string()),
            ("interval".to_string(), "1m".to_string()),
            ("startTime".to_string(), open_ms.to_string()),
            ("endTime".to_string(), (open_ms + 59_999).to_string()),
            ("limit".to_string(), "1".to_string()),
        ];
        let rows = self.get("indexPriceKlines", params).await?;
        let row = rows
            .as_array()
            .and_then(|rs| rs.iter().find(|r| r.as_array().map(|a| a.len() > 6 && as_i64(&a[0]) == open_ms).unwrap_or(false)));
        let Some(row) = row.and_then(Value::as_array).filter(|r| as_i64(&r[6]) == open_ms + 59_999) else {
            return Err(RemoteError::new(format!("币安没有返回 {symbol} 这根 1 分钟指数 K 线")));
        };
        match dec_of(&row[4]) {
            Some(close) if close > Decimal::ZERO => Ok(close),
            _ => Err(RemoteError::new(format!("{symbol} 指数 K 线收盘价无效"))),
        }
    }

    /// The window's start line, read once its minute is over (and CANDLE_GRACE_MS more); None until then or while it fails.
    pub async fn ensure_start(&self, symbol: &str, start_ms: i64, now_ms: i64) -> Option<Decimal> {
        let key = (symbol.to_uppercase(), start_ms - 60_000);
        if let Some(c) = self.candles.borrow().get(&key) {
            return Some(*c);
        }
        if now_ms < start_ms + CANDLE_GRACE_MS {
            return None;
        }
        match self.read_candle(&key.0, key.1).await {
            Ok(close) => {
                {
                    let mut candles = self.candles.borrow_mut();
                    candles.insert(key.clone(), close);
                    if candles.len() > 400 {
                        let old: Vec<(String, i64)> = candles.keys().take(100).cloned().collect();
                        for k in old {
                            candles.remove(&k);
                        }
                    }
                }
                self.candle_error.borrow_mut().remove(&key);
                self.told(&key.0, 0); // (the first look of a course waits for it: looked at now, whatever the index's age)
                Some(close)
            }
            Err(error) => {
                let mut errors = self.candle_error.borrow_mut();
                errors.insert(key, clean_error(&error));
                // (as the candles themselves: the oldest let go — a window whose minute never reads adds one only)
                while errors.len() > 400 {
                    errors.pop_first();
                }
                None
            }
        }
    }

    pub async fn read_price(&self, symbol: &str) -> Res<()> {
        let data = self.get("premiumIndex", vec![("symbol".to_string(), symbol.to_string())]).await?;
        let now = now_ms();
        let at = data.get("time").map(as_i64).filter(|t| *t != 0).unwrap_or(now);
        self.reads.set(self.reads.get() + 1);
        let price = data.get("indexPrice").and_then(dec_of).ok_or_else(|| RemoteError::new("币安指数应答没有 indexPrice"))?;
        self.push_index(symbol, price, at, now);
        Ok(())
    }

    // -- the loops --
    pub fn stop(&self) {
        self.stopping.set();
    }

    pub async fn run(self: Rc<Self>) {
        let a = tokio::task::spawn_local(Self::stream_loop(self.clone()));
        let b = tokio::task::spawn_local(Self::rest_loop(self.clone()));
        self.stopping.wait().await;
        a.abort();
        b.abort();
    }

    async fn rest_loop(self: Rc<Self>) {
        while !self.stopping.is_set() {
            let now = now_ms();
            if self.needed() {
                for symbol in self.symbols.clone() {
                    let priced = *self.priced_ms.borrow().get(&symbol).unwrap_or(&0);
                    if now - priced > PUSH_LATE_MS {
                        if let Err(e) = self.read_price(&symbol).await {
                            *self.error.borrow_mut() = format!("REST 指数：{}", clean_error(&e));
                        }
                    }
                }
            }
            let _ = self.stopping.wait_for(if self.needed() { REST_EVERY_S } else { 15.0 }).await;
        }
    }

    pub fn stream_url(&self) -> String {
        let streams: Vec<String> = self.symbols.iter().map(|s| format!("{}@markPrice@1s", s.to_lowercase())).collect();
        format!("{}/stream?streams={}", self.ws_base, streams.join("/"))
    }

    async fn stream_loop(self: Rc<Self>) {
        let mut backoff = 2.0f64;
        let url = self.stream_url();
        while !self.stopping.is_set() {
            if !self.wanted() {
                let _ = self.stopping.wait_for(3.0).await;
                continue;
            }
            let ws = match ws_connect(&url, &[]).await {
                Ok(ws) => ws,
                Err(error) => {
                    *self.error.borrow_mut() = format!("币安推送连不上：{}", clean_error(&error));
                    let _ = self.stopping.wait_for(backoff).await;
                    backoff = (backoff * 2.0).min(120.0);
                    continue;
                }
            };
            let began = mono();
            self.connected.set(true);
            self.error.borrow_mut().clear();
            let (mut sink, mut source) = ws.split();
            let why: String = loop {
                if self.stopping.is_set() || !self.wanted() {
                    break String::new();
                }
                let frame = tokio::select! {
                    f = tokio::time::timeout(Duration::from_secs(30), source.next()) => f,
                    _ = self.stopping.wait() => break String::new(),
                };
                match frame {
                    Err(_) => break "TimeoutError".into(),
                    Ok(None) => break "连接结束".into(),
                    Ok(Some(Err(e))) => break clean_error(e.to_string()),
                    Ok(Some(Ok(Message::Text(text)))) => self.take(text.as_str()),
                    Ok(Some(Ok(Message::Close(_)))) => break "连接结束".into(),
                    Ok(Some(Ok(_))) => {}
                }
            };
            self.connected.set(false);
            let _ = tokio::time::timeout(Duration::from_secs(1), sink.close()).await;
            if !why.is_empty() {
                *self.error.borrow_mut() = format!("币安推送断开：{why}");
                crate::warn!("{}", self.error.borrow());
            }
            backoff = if mono() - began > 60.0 { 2.0 } else { (backoff * 2.0).min(120.0) };
            let _ = self.stopping.wait_for(backoff).await;
        }
    }

    pub fn take(&self, raw: &str) {
        let Ok(msg) = serde_json::from_str::<Value>(raw) else { return };
        let data = match msg.get("data") {
            Some(d @ Value::Object(_)) => d,
            _ => &msg,
        };
        if data.get("e").and_then(Value::as_str) != Some("markPriceUpdate") {
            return;
        }
        let (Some(symbol), Some(price), Some(at)) = (
            data.get("s").map(crate::util::py_str),
            data.get("i").and_then(dec_of),
            data.get("E").and_then(|e| e.as_i64().or_else(|| e.as_f64().map(|x| x as i64))),
        ) else {
            return;
        };
        self.pushes.set(self.pushes.get() + 1);
        self.push_index(&symbol, price, at, now_ms());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    #[test]
    fn index() {
        let f = IndexFeed::new(&["xauusdt"], "https://fapi.binance.com", "wss://fstream.binance.com/market");
        assert_eq!(f.stream_url(), "wss://fstream.binance.com/market/stream?streams=xauusdt@markPrice@1s");
        let now = now_ms();
        f.take(&format!(r#"{{"stream":"x","data":{{"e":"markPriceUpdate","s":"XAUUSDT","i":"4012.5","E":{now}}}}}"#));
        assert_eq!(f.price("XAUUSDT"), Some(Decimal::from_str("4012.5").unwrap()));
        assert_eq!(f.pushes.get(), 1);
        assert!(!f.push_index("XAUUSDT", Decimal::from_str("4013").unwrap(), now - 1, now)); // older
        assert_eq!(f.price_problem("XAUUSDT", now + 600_000, now), "");
        assert!(f.price_problem("XAUUSDT", now + 60_000, now + 30_000).starts_with("指数 30 秒未更新"));
    }

    #[test]
    fn a_start_line_that_never_reads_is_kept_as_a_few_errors_only() {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(async {
            let f = IndexFeed::new(&["xauusdt"], "https://fapi.binance.test", "wss://fstream.binance.test/market");
            let getter: Getter = Rc::new(|_, _| Box::pin(async { Err(crate::util::RemoteError::new("HTTP 503: down")) }));
            *f.getter.borrow_mut() = Some(getter);
            let start = 1_795_039_200_000i64;
            for i in 0..450 {
                let at = start + i * 900_000;
                assert!(f.ensure_start("XAUUSDT", at, at + 2_000).await.is_none());
            }
            let errors = f.candle_error.borrow();
            assert_eq!(errors.len(), 400);
            assert!(errors.keys().all(|(_, minute)| *minute >= start + 50 * 900_000 - 60_000), "the oldest let go");
        });
    }
}
