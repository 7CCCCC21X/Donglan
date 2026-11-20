//! The rule taker end to end against stand-ins of Predict, the chain and the push connection (no network, no key): a push
//! brings an offer, the order goes out in the same turn, its fills are read, its rest withdrawn, the window settled — and the
//! guards (the cap, the pause, the loss stop, a refusal, a lost answer, a restart, the settings changed on the order's way).
use crate::config::{Config, Env};
use crate::predict::{Chain, PredictApi, Req, Transport};
use crate::rules::held;
use crate::store::Store;
use crate::stream::BookStream;
use crate::taker::{self, Me, Parts, Taker, control, flow, look, orders};
use crate::util::{Obj, Py, RemoteError, Res, day_of};
use crate::wallet::Wallet;
use base64::Engine;
use futures_util::future::LocalBoxFuture;
use serde_json::{Value, json};
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;

pub const KEY: &str = "0x4c4c4c4c4c4c4c4c4c4c4c4c4c4c4c4c4c4c4c4c4c4c4c4c4c4c4c4c4c4c4c4c";
pub const TOKEN_UP: &str = "1111111111111111111111";
pub const TOKEN_DOWN: &str = "2222222222222222222222";
pub const MID: &str = "77001";

/// 2026-11-18 22:00 UTC = 17:00 EST = 06:00 Beijing: gold's first shut window.
pub fn start() -> i64 {
    crate::util::Date::ymd(2026, 11, 18).0 * crate::util::DAY_MS + 22 * 3_600_000
}

pub fn slug_of(start: i64) -> String {
    format!("xau-updown-15m-{}", start / 1000)
}

pub fn market_of(mid: &str, slug: &str, status: &str) -> Obj {
    let v = json!({"id": mid, "conditionId": format!("0x{}", "ab".repeat(32)), "status": "REGISTERED", "tradingStatus": status,
        "question": "", "title": slug, "feeRateBps": 200, "isNegRisk": false, "isYieldBearing": false, "categorySlug": slug,
        "outcomes": [{"name": "Up", "indexSet": 1, "onChainId": TOKEN_UP}, {"name": "Down", "indexSet": 2, "onChainId": TOKEN_DOWN}]});
    v.as_object().cloned().unwrap_or_default()
}

/// What PredictApi asks of Predict, answered from maps; every request recorded.
#[derive(Default)]
pub struct FakePredict {
    pub markets: RefCell<HashMap<String, Obj>>,
    pub categories: RefCell<HashMap<String, String>>,
    pub books: RefCell<HashMap<String, Value>>,
    pub orders: RefCell<HashMap<String, Obj>>,
    pub created: RefCell<Vec<Vec<u8>>>,
    pub removed: RefCell<Vec<String>>,
    pub calls: RefCell<Vec<String>>,
    pub create_error: RefCell<Option<RemoteError>>,
    pub fill_on_create: Cell<f64>,
    pub create_delay: Cell<f64>,
    pub empty_order_answer: RefCell<String>,
    pub stream: RefCell<Option<Rc<BookStream>>>,
    pub chain_usdt: Cell<u128>,
    pub chain_allowed: Cell<u128>,
    /// a new connection's TCP + TLS handshake before an order's bytes can go out (s): the order's gate judged after it, as the
    /// real client does
    pub connect_delay: Cell<f64>,
    /// the order taken (it is on the book) and its answer lost on the way back
    pub lose_answer: Cell<bool>,
    /// no connection to be had (refused): an order's request never leaves, as the real client tells it
    pub refuse_connect: Cell<bool>,
    /// the taker's clock, moved on by ``book_ms`` while a REST book read is out
    pub clock: RefCell<Option<Rc<Cell<i64>>>>,
    pub book_ms: Cell<i64>,
    /// GET /orders/{hash}, one answer each in turn: "" as it stands, "empty" a 200 with no order in it, "timeout" lost
    pub order_reads: RefCell<std::collections::VecDeque<&'static str>>,
    /// a cancel answered without naming the ids ("silent": carried out all the same; "ignored": not carried out) or naming
    /// them as nothing to do ("noop": filled or gone already)
    pub remove_mode: Cell<&'static str>,
    /// the USDT allowance per exchange (wallet::EXCHANGES key); one not named has ``chain_allowed``
    pub allowed_by: RefCell<HashMap<&'static str, u128>>,
}

impl FakePredict {
    pub fn new() -> Rc<FakePredict> {
        let f = FakePredict::default();
        *f.empty_order_answer.borrow_mut() = "HTTP 400: order is invalid".into();
        f.chain_usdt.set(1000 * crate::wallet::WEI);
        f.chain_allowed.set(u128::MAX);
        Rc::new(f)
    }

    pub fn list(&self, mid: &str, slug: &str, status: &str) {
        self.categories.borrow_mut().insert(slug.to_string(), mid.to_string());
        self.markets.borrow_mut().insert(mid.to_string(), market_of(mid, slug, status));
    }

    fn ok(data: Value) -> Res<Vec<u8>> {
        Ok(serde_json::to_vec(&json!({"success": true, "data": data})).unwrap_or_default())
    }

    fn answer(&self, method: &str, path: &str, body: Option<&[u8]>) -> Res<Vec<u8>> {
        self.calls.borrow_mut().push(format!("{method} {path}"));
        let not_found = || Err(RemoteError::new("HTTP 404: not found"));
        if path == "/auth/message" {
            return Self::ok(json!({"message": "Sign in to predict.fun"}));
        }
        if path == "/auth" {
            let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(br#"{"exp":4102444800}"#);
            return Self::ok(json!({"token": format!("h.{payload}.s")}));
        }
        if let Some(slug) = path.strip_prefix("/categories/") {
            let Some(mid) = self.categories.borrow().get(slug).cloned() else { return not_found() };
            let market = self.markets.borrow().get(&mid).cloned().unwrap_or_default();
            return Self::ok(json!({"slug": slug, "markets": [market]}));
        }
        if let Some(rest) = path.strip_prefix("/markets/") {
            if let Some(mid) = rest.strip_suffix("/orderbook") {
                if let Some(clock) = self.clock.borrow().as_ref() {
                    clock.set(clock.get() + self.book_ms.get()); // (the read took that long)
                }
                if let Some(b) = self.books.borrow().get(mid) {
                    return Self::ok(b.clone());
                }
                // (Predict's book is the one it last pushed)
                let pushed = self.stream.borrow().as_ref().and_then(|s| s.held(mid));
                let (bids, asks) = pushed.map(|(b, a, _)| ((*b).clone(), (*a).clone())).unwrap_or_default();
                let rows = |levels: &Vec<crate::rules::Level>| -> Vec<Value> {
                    levels.iter().map(|(p, q)| json!([p.to_string(), q.to_string()])).collect()
                };
                return Self::ok(json!({"bids": rows(&bids), "asks": rows(&asks)}));
            }
            let Some(m) = self.markets.borrow().get(rest).cloned() else { return not_found() };
            return Self::ok(Value::Object(m));
        }
        if path == "/orders" && method == "POST" {
            let body: Value = serde_json::from_slice(body.unwrap_or(b"{}")).unwrap_or_default();
            if body["data"].as_object().map(|d| d.is_empty()).unwrap_or(true) {
                return Err(RemoteError::new(self.empty_order_answer.borrow().clone()));
            }
            if let Some(error) = self.create_error.borrow_mut().take() {
                return Err(error);
            }
            let order = &body["data"]["order"];
            let shares: u128 = order["takerAmount"].as_str().unwrap_or("0").parse().unwrap_or(0);
            let filled = ((self.fill_on_create.get() * 1e18) as u128).min(shares);
            let oid = format!("id{}", self.created.borrow().len());
            let hash = order["hash"].as_str().unwrap_or("").to_lowercase();
            let row = json!({"id": oid, "status": if filled >= shares { "FILLED" } else { "OPEN" }, "amount": shares.to_string(),
                             "amountFilled": filled.to_string(), "order": {"hash": hash}});
            self.orders.borrow_mut().insert(hash.clone(), row.as_object().cloned().unwrap_or_default());
            return Self::ok(json!({"orderId": oid, "orderHash": hash}));
        }
        if path == "/orders/remove" {
            let body: Value = serde_json::from_slice(body.unwrap_or(b"{}")).unwrap_or_default();
            let ids: Vec<String> =
                body["data"]["ids"].as_array().map(|a| a.iter().map(crate::util::py_str).collect()).unwrap_or_default();
            self.removed.borrow_mut().extend(ids.clone());
            let mode = self.remove_mode.get();
            if mode != "ignored" {
                for row in self.orders.borrow_mut().values_mut() {
                    if ids.contains(&row.s("id")) && row.s("status") == "OPEN" {
                        row.insert("status".into(), json!("CANCELLED"));
                    }
                }
            }
            if mode == "noop" {
                return Self::ok(json!({"removed": [], "noop": ids}));
            }
            if !mode.is_empty() {
                return Self::ok(json!({"removed": [], "noop": []}));
            }
            return Self::ok(json!({"removed": ids, "noop": []}));
        }
        if let Some(hash) = path.strip_prefix("/orders/") {
            match self.order_reads.borrow_mut().pop_front().unwrap_or("") {
                "empty" => return Self::ok(json!({})),
                "timeout" => return Err(RemoteError::new("网络错误 (TimeoutError: timed out)")),
                _ => {}
            }
            let Some(row) = self.orders.borrow().get(&hash.to_lowercase()).cloned() else { return not_found() };
            return Self::ok(Value::Object(row));
        }
        Err(RemoteError::new(format!("HTTP 404: {path}")))
    }

    fn rpc(&self, body: &[u8]) -> Res<Vec<u8>> {
        let req: Value = serde_json::from_slice(body).unwrap_or_default();
        let word = |v: u128| format!("0x{:064x}", v);
        let result = match req["method"].as_str().unwrap_or("") {
            "eth_getBalance" => json!(format!("0x{:x}", 100_000_000_000_000_000u128)),
            "eth_call" => {
                let data = req["params"][0]["data"].as_str().unwrap_or("");
                let sel = &data[..data.len().min(10)];
                let balance_of = format!("0x{}", hex::encode(crate::predict::selector("balanceOf(address)")));
                let allowance = format!("0x{}", hex::encode(crate::predict::selector("allowance(address,address)")));
                if sel == balance_of {
                    json!(word(self.chain_usdt.get()))
                } else if sel == allowance {
                    // (owner, spender): the spender's address is the last 20 bytes of the second word
                    let spender = data.get(10 + 64 + 24..10 + 128).unwrap_or("").to_lowercase();
                    let named = crate::wallet::addresses(56)
                        .unwrap()
                        .iter()
                        .find(|(_, a)| a.to_lowercase().ends_with(&spender) && !spender.is_empty())
                        .and_then(|(k, _)| self.allowed_by.borrow().get(k).copied());
                    let a = named.unwrap_or(self.chain_allowed.get());
                    if a == u128::MAX { json!(format!("0x{}", "f".repeat(64))) } else { json!(word(a)) }
                } else {
                    json!(word(0))
                }
            }
            _ => Value::Null,
        };
        Ok(serde_json::to_vec(&json!({"jsonrpc": "2.0", "id": 1, "result": result})).unwrap_or_default())
    }
}

impl Transport for FakePredict {
    fn request(&self, req: Req) -> LocalBoxFuture<'_, Res<Vec<u8>>> {
        Box::pin(async move {
            if req.url.contains("bsc") || req.url.contains("rpc") {
                return self.rpc(req.payload.as_deref().map(Vec::as_slice).unwrap_or(b"{}"));
            }
            let path = req.url.split("/v1").nth(1).unwrap_or("").to_string();
            let method = if req.payload.is_some() { "POST" } else { "GET" };
            if method == "POST" && path == "/orders" {
                let raw = req.payload.as_deref().map(Vec::as_slice).unwrap_or(b"{}");
                if raw != br#"{"data":{}}"# {
                    if self.refuse_connect.get() {
                        return Err(RemoteError::not_sent(format!(
                            "{}（网络错误 (ConnectionRefusedError: Connection refused)），请求没有发出",
                            crate::http::NOT_CONNECTED
                        )));
                    }
                    if self.connect_delay.get() > 0.0 {
                        crate::sync::sleep(self.connect_delay.get()).await;
                        if let Some(gate) = &req.gate {
                            let why = gate();
                            if !why.is_empty() {
                                return Err(RemoteError::not_sent(why)); // (nothing of it written)
                            }
                        }
                    }
                    self.created.borrow_mut().push(req.payload.as_deref().cloned().unwrap_or_default()); // (as the request lands)
                    if self.create_delay.get() > 0.0 {
                        crate::sync::sleep(self.create_delay.get()).await;
                    }
                    if self.lose_answer.get() {
                        let _ = self.answer(method, &path, Some(raw));
                        return Err(RemoteError::new("网络错误 (TimeoutError: timed out)"));
                    }
                }
            }
            self.answer(method, &path, req.payload.as_deref().map(Vec::as_slice))
        })
    }
}

pub struct Rig {
    pub me: Me,
    pub api: Rc<FakePredict>,
    pub stream: Rc<BookStream>,
    pub clock: Rc<Cell<i64>>,
}

pub fn env_of(pairs: &[(&str, &str)]) -> Env {
    let mut base: Env = [
        ("RULE_TAKER", "on"),
        ("PREDICT_API_KEY", "k"),
        ("PREDICT_PRIVATE_KEY", KEY),
        ("STATUS_PORT", "0"),
        ("RULE_FAST_HOURS", "off"),
        ("RULE_LEAD_MINUTES", "0"),
    ]
    .iter()
    .map(|(k, v)| (k.to_string(), v.to_string()))
    .collect();
    for (k, v) in pairs {
        base.insert(k.to_string(), v.to_string());
    }
    base
}

pub fn rig_with(clock_ms: i64, store: Store, api: Rc<FakePredict>, pairs: &[(&str, &str)]) -> Rig {
    rig_full(clock_ms, store, api, pairs, None)
}

pub fn rig_full(
    clock_ms: i64,
    store: Store,
    api: Rc<FakePredict>,
    pairs: &[(&str, &str)],
    index: Option<Rc<crate::index::IndexFeed>>,
) -> Rig {
    let config = Config::from_env(&env_of(pairs)).unwrap();
    let wallet = Rc::new(Wallet::new(KEY, 56, "").unwrap());
    let transport: Rc<dyn Transport> = api.clone();
    let predict = Rc::new(PredictApi::new("https://api.predict.fun/v1", "k", Some(wallet.clone()), 0.0, transport.clone()));
    let chain = Rc::new(Chain::new("https://bsc-rpc.test/", wallet.clone(), transport));
    let stream = Rc::new(BookStream::new("", 1));
    stream.fake_alive.set(Some(true));
    *api.stream.borrow_mut() = Some(stream.clone());
    let clock = Rc::new(Cell::new(clock_ms));
    *api.clock.borrow_mut() = Some(clock.clone());
    let t = Taker::new(Parts {
        config,
        store,
        api: Some(predict),
        wallet: Some(wallet),
        chain: Some(Some(chain)),
        stream: Some(stream.clone()),
        index,
        telegram: None,
        clock: Some(clock.clone()),
        base_env: None,
    })
    .unwrap();
    let me = taker::install(t);
    {
        let mut t = me.borrow_mut();
        t.ready_error.clear();
        t.usdt = Some(api.chain_usdt.get());
        t.balance_asked_ms = clock_ms - 60_000; // (the balance read a minute before the test begins)
        t.running = true;
    }
    Rig { me, api, stream, clock }
}

pub fn rig(clock_ms: i64, pairs: &[(&str, &str)]) -> Rig {
    rig_with(clock_ms, Store::memory(), FakePredict::new(), pairs)
}

pub fn run<F: std::future::Future>(f: F) -> F::Output {
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    tokio::task::LocalSet::new().block_on(&rt, f)
}

/// A pushed frame's data: asks / bids as [[price, size], ...] (the book quoted for Up).
pub fn book(asks: &[(&str, &str)], bids: &[(&str, &str)]) -> Value {
    let now = crate::util::now_ms();
    json!({"version": now, "updateTimestampMs": now - 5, "asks": asks.iter().map(|(p, q)| json!([p, q])).collect::<Vec<_>>(),
           "bids": bids.iter().map(|(p, q)| json!([p, q])).collect::<Vec<_>>()})
}

impl Rig {
    /// The plan made and the window's market found (its first look).
    pub async fn found(&self) {
        {
            let mut t = self.me.borrow_mut();
            let now = t.now();
            t.plan(now);
            t.planned_ms = now;
        }
        self.api.list(MID, &slug_of(start()), "OPEN");
        flow::turn(&self.me).await;
        let t = self.me.borrow();
        let w = t.track.get(&slug_of(start())).expect("the window is followed").borrow();
        assert!(w.market.is_some(), "{}", w.word);
    }

    pub fn push(&self, mid: &str, data: Value) {
        self.stream.take(mid, &data);
    }

    pub async fn sends(&self) {
        let pending: Vec<_> = std::mem::take(&mut self.me.borrow_mut().sends);
        for task in pending {
            let _ = task.await;
        }
    }

    pub fn created(&self) -> Vec<Value> {
        self.api.created.borrow().iter().map(|raw| serde_json::from_slice(raw).unwrap_or_default()).collect()
    }

    pub fn taker_amount(&self, i: usize) -> u128 {
        self.created()[i]["data"]["order"]["takerAmount"].as_str().unwrap().parse().unwrap()
    }

    pub fn order(&self, i: usize) -> Obj {
        let t = self.me.borrow();
        let rec = t.record(&slug_of(start()), Some("on")).expect("a record");
        crate::rules::orders(rec).nth(i).cloned().expect("an order")
    }

    pub fn word(&self) -> String {
        self.me.borrow().track.get(&slug_of(start())).map(|w| w.borrow().word.clone()).unwrap_or_default()
    }

    pub fn advance(&self, ms: i64) {
        self.clock.set(self.clock.get() + ms);
    }
}

const WEI: u128 = crate::wallet::WEI;

#[test]
fn push_places_the_whole_window_at_once() {
    run(async {
        let r = rig(start() - 3_600_000, &[]);
        r.found().await;
        assert!(r.stream.wanted.borrow().contains(MID));
        assert!(r.created().is_empty());
        // an offer of 100 Up at 55¢ (the rule's 58¢): bought in the push's own turn — the window's 50 shares, at 58¢
        r.push(MID, book(&[("0.55", "100")], &[]));
        assert_eq!(r.created().len(), 1, "{}", r.word());
        let body = &r.created()[0]["data"];
        assert_eq!(body["strategy"], "LIMIT");
        assert_eq!(body["pricePerShare"], (58 * 10u128.pow(16)).to_string());
        assert_eq!(r.taker_amount(0), 50 * WEI);
        assert_eq!(body["order"]["makerAmount"], (29 * WEI).to_string());
        assert_eq!(body["order"]["tokenId"], TOKEN_UP);
        assert_eq!(body["order"]["expiration"], (start() / 1000 - taker::EXPIRY_BEFORE_S).to_string());
        r.sends().await;
        let o = r.order(0);
        assert_eq!(o.state(), "open");
        assert_eq!(o.s("hash"), body["order"]["hash"].as_str().unwrap().to_lowercase());
        assert!(o.is_true("fresh"));
        assert!(r.word().contains("已下单"), "{}", r.word());
        // the signature is the wallet's over the order's hash
        let hash = hex::decode(body["order"]["hash"].as_str().unwrap().trim_start_matches("0x")).unwrap();
        let sig = hex::decode(body["order"]["signature"].as_str().unwrap().trim_start_matches("0x")).unwrap();
        let rs = k256::ecdsa::Signature::from_slice(&sig[..64]).unwrap();
        let recid = k256::ecdsa::RecoveryId::from_byte(sig[64] - 27).unwrap();
        let key = k256::ecdsa::VerifyingKey::recover_from_prehash(&hash, &rs, recid).unwrap();
        let point = key.to_encoded_point(false);
        let addr = &crate::wallet::keccak(&point.as_bytes()[1..])[12..];
        let mut a = [0u8; 20];
        a.copy_from_slice(addr);
        assert_eq!(crate::wallet::checksum(&a), r.me.borrow().wallet.as_ref().unwrap().signer);
    });
}

#[test]
fn small_offer_is_bought_as_shown_and_a_second_order_beside_it() {
    run(async {
        let r = rig(start() - 3_600_000, &[]);
        r.found().await;
        r.api.create_delay.set(0.05); // the first order's answer still on its way...
        r.push(MID, book(&[("0.57", "8")], &[]));
        assert_eq!(r.taker_amount(0), 8 * WEI);
        r.push(MID, book(&[("0.57", "8"), ("0.58", "30")], &[])); // ...when more is posted: ordered beside it
        crate::sync::sleep(0.01).await;
        assert_eq!(r.created().len(), 2, "{}", r.word());
        // 38 offered, 8 of them the first order's: 30 fresh — at least 20 and a quarter of the 42 the window may still buy: all 42
        assert_eq!(r.taker_amount(1), 42 * WEI);
        assert_ne!(r.created()[0]["data"]["order"]["salt"], r.created()[1]["data"]["order"]["salt"]);
        r.sends().await;
        assert_eq!(held(r.me.borrow().record(&slug_of(start()), Some("on")).unwrap()), 50.0);
    });
}

#[test]
fn follow_reads_fills_withdraws_and_ends() {
    run(async {
        let r = rig(start() - 3_600_000, &[]);
        r.found().await;
        r.api.fill_on_create.set(30.0);
        r.push(MID, book(&[("0.55", "100")], &[]));
        r.sends().await;
        r.advance(taker::FILL_READ_MS);
        orders::follow(&r.me).await;
        assert_eq!(r.order(0).f("filled"), 30.0);
        assert_eq!(r.order(0).state(), "cancelling");
        assert_eq!(*r.api.removed.borrow(), vec!["id1".to_string()]);
        r.advance(1_000);
        orders::follow(&r.me).await;
        assert_eq!(r.order(0).state(), "closing"); // off the book: final only a minute on (a late fill)
        r.advance(taker::LATE_FILL_MS);
        orders::follow(&r.me).await;
        assert_eq!(r.order(0).state(), "final");
        // settled on Predict's result: Up won
        r.api.markets.borrow_mut().get_mut(MID).unwrap()["outcomes"][0]["status"] = json!("WON");
        r.clock.set(start() + 15 * 60_000 + taker::SETTLE_AFTER_MS);
        orders::settle(&r.me).await;
        let t = r.me.borrow();
        let rec = t.record(&slug_of(start()), Some("on")).unwrap();
        assert_eq!(rec.s("result"), "up");
        let fee = 0.02 * 0.42 / 0.58;
        assert!((rec.f("pnl") - (30.0 * (1.0 - fee) - 30.0 * 0.58)).abs() < 1e-6, "{}", rec.f("pnl"));
    });
}

#[test]
fn day_cap_counts_every_window() {
    run(async {
        let r = rig(start() - 3_600_000, &[("RULE_MAX_DAY_USD", "40")]);
        r.found().await;
        r.push(MID, book(&[("0.55", "100")], &[]));
        r.sends().await;
        // $40 a day at 58¢: the first window takes 50 ($29)
        assert_eq!(r.taker_amount(0), 50 * WEI);
        let second = start() + 15 * 60_000;
        r.api.list("77002", &slug_of(second), "OPEN");
        r.advance((taker::PROBE_SLOW_S * 1000.0) as i64 + 1); // (its listing looked for again)
        flow::turn(&r.me).await;
        r.push("77002", book(&[("0.55", "100")], &[]));
        r.sends().await;
        assert_eq!(r.taker_amount(1), 18 * WEI); // $11 left → 18 shares
    });
}

#[test]
fn balance_caps_the_order() {
    run(async {
        let r = rig(start() - 3_600_000, &[]);
        r.me.borrow_mut().usdt = Some(10 * WEI); // $10: 17 shares at 58¢
        r.found().await;
        r.push(MID, book(&[("0.55", "100")], &[]));
        assert_eq!(r.taker_amount(0), 17 * WEI);
        r.sends().await;
    });
}

#[test]
fn nothing_while_paused_or_stopped_for_the_day() {
    run(async {
        let r = rig(start() - 3_600_000, &[("RULE_MAX_DAILY_LOSS_USD", "20")]);
        r.found().await;
        control::control(&r.me, "pause", "test").await;
        r.push(MID, book(&[("0.55", "100")], &[]));
        assert!(r.created().is_empty());
        assert!(r.word().contains("暂停"), "{}", r.word());
        control::control(&r.me, "resume", "").await;
        // a settled loss of $25 today: the loss stop holds every new order
        {
            let mut t = r.me.borrow_mut();
            let day = t.now();
            let rec = json!({"slug": "old", "mode": "on", "rule": "gold_shut", "side": "up", "pnl": -25.0, "orders": [],
                             "settled_day": day_of(day), "start_ms": day});
            t.recs.insert("old".into(), rec.as_object().cloned().unwrap());
        }
        r.push(MID, book(&[("0.54", "100")], &[]));
        assert!(r.created().is_empty());
        assert!(r.me.borrow().block(0.0, None).contains("停机线"));
        // /resume moves the line: from the P&L now, another $20
        let reply = control::control(&r.me, "resume", "").await;
        assert!(reply.contains("停机线移到"), "{reply}");
        assert_eq!(r.me.borrow().block(0.0, None), "");
    });
}

#[test]
fn refusal_pauses_the_window_and_counts() {
    run(async {
        let r = rig(start() - 3_600_000, &[]);
        r.found().await;
        *r.api.create_error.borrow_mut() = Some(RemoteError::new("HTTP 400: order value too small"));
        r.push(MID, book(&[("0.55", "100")], &[]));
        r.sends().await;
        assert_eq!(r.order(0).state(), "rejected");
        let look_ms = r.me.borrow().track.get(&slug_of(start())).unwrap().borrow().look_ms;
        assert!(look_ms > r.clock.get()); // not again for half a minute
        r.push(MID, book(&[("0.54", "100")], &[]));
        assert_eq!(r.created().len(), 1);
    });
}

#[test]
fn lost_answer_is_found_by_hash_never_sent_again() {
    run(async {
        let r = rig(start() - 3_600_000, &[]);
        r.found().await;
        *r.api.create_error.borrow_mut() = Some(RemoteError::new("网络错误 (TimeoutError)"));
        r.push(MID, book(&[("0.55", "100")], &[]));
        r.sends().await;
        let o = r.order(0);
        assert_eq!(o.state(), "unknown");
        // the window holds: its 50 shares are counted whole while the order may exist
        r.push(MID, book(&[("0.54", "100")], &[]));
        assert_eq!(r.created().len(), 1);
        // Predict did take it (the answer was lost on the way): found by its hash
        let row =
            json!({"id": "x9", "status": "FILLED", "amount": (50 * WEI).to_string(), "amountFilled": (50 * WEI).to_string()});
        r.api.orders.borrow_mut().insert(o.s("hash"), row.as_object().cloned().unwrap());
        r.advance(taker::UNKNOWN_EVERY_MS);
        orders::follow(&r.me).await;
        let o = r.order(0);
        assert_eq!((o.s("id").as_str(), o.state(), o.f("filled")), ("x9", "final", 50.0));
    });
}

#[test]
fn conflict_not_found_frees_the_window() {
    run(async {
        let r = rig(start() - 3_600_000, &[]);
        r.found().await;
        *r.api.create_error.borrow_mut() = Some(RemoteError::new("HTTP 409: Conflict"));
        r.push(MID, book(&[("0.55", "100")], &[]));
        r.sends().await;
        let o = r.order(0);
        assert_eq!((o.state(), o.t("conflict")), ("unknown", true));
        r.api.orders.borrow_mut().remove(&o.s("hash"));
        let key = crate::rules::rid(&slug_of(start()), "on");
        for _ in 0..=taker::CONFLICT_MISSES {
            r.advance(taker::CONFLICT_GRACE_MS / taker::CONFLICT_MISSES + 1);
            r.me.borrow_mut().order_mut(&key, &o.s("hash")).unwrap().insert("next_ms".into(), json!(0));
            orders::follow(&r.me).await;
        }
        assert_eq!(r.order(0).state(), "not_found");
        assert_eq!(held(r.me.borrow().record(&slug_of(start()), Some("on")).unwrap()), 0.0);
    });
}

#[test]
fn settings_changed_on_the_way_hold_the_order() {
    run(async {
        let r = rig(start() - 3_600_000, &[]);
        r.found().await;
        let me = Rc::downgrade(&r.me);
        let api = r.me.borrow().api.clone();
        *api.pre_send.borrow_mut() = Some(Rc::new(move || {
            // switched while the request waited for its slot
            if let Some(me) = me.upgrade() {
                let mut t = me.borrow_mut();
                let s = t.settings.with_mode("paper");
                t.configure(s);
            }
        }));
        r.push(MID, book(&[("0.55", "100")], &[]));
        r.sends().await;
        assert_eq!(r.order(0).state(), "not_sent");
        assert!(r.created().is_empty());
    });
}

#[test]
fn matching_not_open_waits_then_buys_the_moment_it_opens() {
    run(async {
        let r = rig(start() - 3_600_000, &[]);
        {
            let mut t = r.me.borrow_mut();
            let now = t.now();
            t.plan(now);
            t.planned_ms = now;
        }
        r.api.list(MID, &slug_of(start()), "MATCHING_NOT_ENABLED");
        flow::turn(&r.me).await;
        r.push(MID, book(&[("0.55", "100")], &[]));
        assert!(r.created().is_empty());
        assert!(r.word().contains("等撮合开启"), "{}", r.word());
        r.api.markets.borrow_mut().get_mut(MID).unwrap().insert("tradingStatus".into(), json!("OPEN"));
        crate::sync::sleep(0.4).await; // the rush's next read of the status sees it open, and the look buys
        assert_eq!(r.created().len(), 1, "{}", r.word());
        assert!(r.order(0).has("open_wait"));
        r.sends().await;
    });
}

#[test]
fn presigned_order_is_the_one_sent() {
    run(async {
        let r = rig(start() - 3_600_000, &[("RULE_FAST_HOURS", "00:00-24:00")]);
        r.found().await;
        {
            let mut t = r.me.borrow_mut();
            let now = t.now();
            let _ = t.pace(now);
            assert!(t.fast);
            look::presign(&mut t, now);
            assert_eq!(t.presigned, 1);
        }
        r.push(MID, book(&[("0.55", "100")], &[]));
        r.sends().await;
        assert_eq!(r.me.borrow().presign_hits, 1);
        assert!(r.order(0).is_true("presigned"));
    });
}

#[test]
fn journal_recovers_an_order_the_store_never_got() {
    run(async {
        let dir = std::env::temp_dir().join(format!("ruletaker-journal-{}-{}", std::process::id(), crate::util::now_ms()));
        let path = dir.join("t.sqlite3").to_string_lossy().to_string();
        let api = FakePredict::new();
        let r = rig_with(start() - 3_600_000, Store::open(&path).unwrap(), api.clone(), &[]);
        r.found().await;
        r.push(MID, book(&[("0.55", "100")], &[]));
        r.sends().await;
        let o = r.order(0);
        let journal = r.me.borrow().journal_path();
        let lines: Vec<Value> = std::fs::read_to_string(&journal)
            .unwrap()
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(lines[0]["order"]["hash"], o.s("hash"));
        // the store lost the record (a crash before its writer got to it): the journal brings the order back as unknown
        assert!(r.me.borrow().store.flush(10.0));
        let mut store = Store::open(&path).unwrap();
        let keys: Vec<String> = store.take_prefix("rule:").into_iter().map(|(k, _)| k).collect();
        store.delete_keys(&keys);
        assert!(store.flush(10.0));
        drop(store);
        let again = rig_with(start() - 3_600_000, Store::open(&path).unwrap(), api, &[]);
        let t = again.me.borrow();
        let rec = t.record(&slug_of(start()), Some("on")).expect("brought back");
        let back = crate::rules::orders(rec).next().unwrap();
        assert_eq!((back.s("hash"), back.state()), (o.s("hash"), "unknown"));
        drop(t);
        let _ = std::fs::remove_dir_all(dir);
    });
}

#[test]
fn paper_records_once_and_sends_nothing() {
    run(async {
        let r = rig(start() - 3_600_000, &[("RULE_TAKER", "paper")]);
        r.found().await;
        r.push(MID, book(&[("0.55", "20"), ("0.57", "40")], &[]));
        r.push(MID, book(&[("0.55", "25"), ("0.57", "40")], &[]));
        assert!(r.created().is_empty());
        let t = r.me.borrow();
        let rec = t.record(&slug_of(start()), Some("paper")).unwrap();
        let ords: Vec<&Obj> = crate::rules::orders(rec).collect();
        assert_eq!(ords.len(), 1);
        assert_eq!(ords[0].f("filled"), 50.0);
        assert!((ords[0].f("avg") - (0.55 * 20.0 + 0.57 * 30.0) / 50.0).abs() < 1e-9);
    });
}

#[test]
fn nothing_in_the_last_seconds_before_the_start() {
    run(async {
        let r = rig(start() - 3_600_000, &[]);
        r.found().await;
        r.clock.set(start() - taker::STOP_MS + 1);
        r.push(MID, book(&[("0.55", "100")], &[]));
        assert!(r.created().is_empty());
    });
}

#[test]
fn withdraw_all_on_pause() {
    run(async {
        let r = rig(start() - 3_600_000, &[]);
        r.found().await;
        r.push(MID, book(&[("0.55", "100")], &[]));
        r.sends().await;
        let reply = control::control(&r.me, "pause", "test").await;
        assert!(reply.contains("已请求撤回"), "{reply}");
        assert_eq!(*r.api.removed.borrow(), vec!["id1".to_string()]);
    });
}

#[test]
fn status_text_and_state() {
    run(async {
        let r = rig(start() - 3_600_000, &[]);
        r.found().await;
        r.push(MID, book(&[("0.55", "100")], &[]));
        r.sends().await;
        let text = r.me.borrow().status();
        assert!(text.contains("规则吃单"), "{text}");
        assert!(text.contains("真实吃单"), "{text}");
        assert!(text.contains("黄金休市 4 档 买涨 ≤58¢"), "{text}");
        assert!(text.contains("抢单记录"), "{text}");
        let state = r.me.borrow().state();
        assert_eq!(state["mode"], "on");
        assert_eq!(state["rules"].as_array().unwrap().len(), 8);
        assert!(state["settings"].as_array().unwrap().len() == 3);
        let summary = r.me.borrow().summary();
        assert_eq!(summary["windows"][0]["slug"], slug_of(start()));
    });
}

#[test]
fn the_15_second_line_holds_when_the_bytes_leave() {
    run(async {
        let r = rig(start() - 3_600_000, &[]);
        r.found().await;
        let clock = r.clock.clone();
        let api = r.me.borrow().api.clone();
        *api.pre_send.borrow_mut() = Some(Rc::new(move || clock.set(start() - taker::STOP_MS + 10)));
        r.push(MID, book(&[("0.55", "100")], &[]));
        r.sends().await;
        let o = r.order(0);
        assert_eq!(o.state(), "not_sent");
        assert!(o.s("error").contains("离开场不足 15 秒"), "{}", o.s("error"));
        assert!(r.created().is_empty());
    });
}

#[test]
fn an_order_answered_as_the_service_stops_is_withdrawn() {
    run(async {
        let r = rig(start() - 3_600_000, &[]);
        r.found().await;
        r.api.create_delay.set(0.02);
        r.push(MID, book(&[("0.55", "100")], &[]));
        r.me.borrow().stop.set(); // the service stops while the answer is on its way
        r.sends().await;
        assert_eq!(r.order(0).state(), "cancelling");
        assert_eq!(*r.api.removed.borrow(), vec!["id1".to_string()]);
    });
}

#[test]
fn follow_leaves_an_order_on_its_way_alone() {
    run(async {
        let r = rig(start() - 3_600_000, &[]);
        r.found().await;
        r.api.create_delay.set(0.05);
        r.push(MID, book(&[("0.55", "100")], &[]));
        r.advance(10 * taker::FILL_READ_MS);
        orders::follow(&r.me).await;
        assert!(!r.api.calls.borrow().iter().any(|c| c.starts_with("GET /orders/")), "{:?}", r.api.calls.borrow());
        r.sends().await;
        assert_eq!(r.order(0).state(), "open");
    });
}

#[test]
fn balance_counts_the_orders_placed_since_its_read() {
    run(async {
        let r = rig(start() - 3_600_000, &[]);
        r.found().await;
        r.me.borrow_mut().usdt = Some(40 * WEI);
        r.push(MID, book(&[("0.55", "100")], &[]));
        r.sends().await;
        // $40 read before the order: $29 of it is on the order's way — $11 left beside it
        let left = r.me.borrow().usd(None).unwrap();
        assert!((left - 11.0).abs() < 1e-9, "{left}");
    });
}

#[test]
fn a_store_that_cannot_write_holds_every_order() {
    run(async {
        let r = rig(start() - 3_600_000, &[]);
        r.found().await;
        r.me.borrow().store.fail_for_test("disk full");
        r.push(MID, book(&[("0.55", "100")], &[]));
        assert!(r.created().is_empty());
        assert!(r.word().contains("记录写不进数据库"), "{}", r.word());
    });
}

#[test]
fn self_check_all_good_then_a_region_that_may_not_trade() {
    run(async {
        let r = rig(start() - 3_600_000, &[]);
        r.found().await;
        let text = control::self_check(&r.me).await;
        assert!(text.contains("✅ Predict 登录：成功"), "{text}");
        assert!(text.contains("下单接口在这个区域可用"), "{text}");
        assert!(text.contains("结论：✅ 可以真实吃单"), "{text}");
        *r.api.empty_order_answer.borrow_mut() = "HTTP 403: 访问被拒绝".into();
        let text = control::self_check(&r.me).await;
        assert!(text.contains("❌ 下单接口 403"), "{text}");
        assert!(text.contains("结论：❌"), "{text}");
    });
}

#[test]
fn order_test_rests_far_under_the_market_and_is_withdrawn() {
    run(async {
        let r = rig(start() - 3_600_000, &[]);
        r.found().await;
        r.api.books.borrow_mut().insert(MID.into(), json!({"bids": [["0.40", "10"]], "asks": [["0.60", "10"]]}));
        let text = control::order_test(&r.me).await;
        assert!(text.contains("1/4 下单成功"), "{text}");
        assert!(text.contains("3/4 撤单成功"), "{text}");
        assert!(text.contains("结论：✅"), "{text}");
        let order = &r.created()[0]["data"]["order"];
        assert_eq!(r.created()[0]["data"]["pricePerShare"], (2 * 10u128.pow(16)).to_string()); // 2¢
        assert_eq!(order["takerAmount"], (50 * WEI).to_string()); // about a dollar
        assert!(r.me.borrow().record(&slug_of(start()), Some("on")).is_none()); // no rule order
    });
}

#[test]
fn settings_save_check_reset_and_mode() {
    run(async {
        let r = rig(start() - 3_600_000, &[("RULE_TAKER", "paper")]);
        let mut values = serde_json::Map::new();
        values.insert("RULE_GOLD_SHUT_CENTS".into(), json!("61"));
        assert!(matches!(control::save_settings(&r.me, &values).await, control::SaveResult::Done(m) if m.contains("已保存")));
        assert_eq!(r.me.borrow().settings.price("gold_shut"), 61);
        values.insert("RULE_GOLD_SHUT_CENTS".into(), json!("99"));
        assert!(matches!(control::save_settings(&r.me, &values).await, control::SaveResult::Refused(_)));
        assert_eq!(r.me.borrow().settings.price("gold_shut"), 61);
        assert!(
            matches!(control::reset_settings(&r.me, None).await, control::SaveResult::Done(m) if m.contains("已恢复部署设置"))
        );
        assert_eq!(r.me.borrow().settings.price("gold_shut"), 58);
        // the mode never above the settings file's RULE_TAKER
        let reply = control::control(&r.me, "mode", "on").await;
        assert!(reply.contains("最多调到 paper"), "{reply}");
        let reply = control::control(&r.me, "mode", "off").await;
        assert!(reply.contains("关"), "{reply}");
        assert_eq!(r.me.borrow().store.get_str("control:mode"), "off");
    });
}

#[test]
fn routes_page_api_and_the_header_check() {
    run(async {
        let r = rig(start() - 3_600_000, &[]);
        let q = HashMap::new();
        let mut h = HashMap::new();
        let page = crate::server::route(&r.me, "", "GET", "/", &q, &h, b"").await;
        assert!(page.body.contains("<html") || page.body.contains("<!doctype") || page.body.contains("<!DOCTYPE"));
        let state = crate::server::route(&r.me, "", "GET", "/api/state", &q, &h, b"").await;
        assert_eq!(state.status, 200);
        assert_eq!(serde_json::from_str::<Value>(&state.body).unwrap()["mode"], "on");
        let refused = crate::server::route(&r.me, "", "POST", "/api/control", &q, &h, br#"{"action":"pause"}"#).await;
        assert_eq!(refused.status, 403);
        h.insert("x-rt".to_string(), "1".to_string());
        let paused = crate::server::route(&r.me, "", "POST", "/api/control", &q, &h, br#"{"action":"pause"}"#).await;
        assert!(paused.body.contains("已暂停"), "{}", paused.body);
        let keyed = crate::server::route(&r.me, "secret-key-123", "GET", "/status", &q, &h, b"").await;
        assert_eq!(keyed.status, 403);
        let mut q2 = HashMap::new();
        q2.insert("key".to_string(), "secret-key-123".to_string());
        assert_eq!(crate::server::route(&r.me, "secret-key-123", "GET", "/status", &q2, &h, b"").await.status, 200);
        let csv = crate::server::route(&r.me, "", "GET", "/api/records.csv", &q, &h, b"").await;
        assert!(csv.body.starts_with('\u{feff}'));
        let bad = crate::server::route(&r.me, "", "POST", "/api/settings", &q, &h, br#"{"values":{"RULE_GOLD":"maybe"}}"#).await;
        assert!(bad.body.contains("\"ok\":false"), "{}", bad.body);
    });
}

#[test]
fn telegram_commands_and_id() {
    run(async {
        #[derive(Default)]
        struct Sent(RefCell<Vec<(String, Value)>>);
        impl Transport for Sent {
            fn request(&self, req: Req) -> LocalBoxFuture<'_, Res<Vec<u8>>> {
                Box::pin(async move {
                    let body: Value =
                        serde_json::from_slice(req.payload.as_deref().map(Vec::as_slice).unwrap_or(b"{}")).unwrap_or_default();
                    self.0.borrow_mut().push((req.url.clone(), body));
                    Ok(br#"{"ok":true,"result":true}"#.to_vec())
                })
            }
        }
        let sent = Rc::new(Sent::default());
        let tg =
            Rc::new(crate::telegram::Telegram::new("111:notify", &[(-100, 0)], 42, true, true, "222:commands", sent.clone()));
        let r = rig(start() - 3_600_000, &[]);
        let me = r.me.clone();
        *tg.handler.borrow_mut() = Some(Rc::new(move |c: String, a: String| {
            let me = me.clone();
            Box::pin(async move { control::command(&me, &c, &a).await })
        }));
        // /id from anyone; /status from the admin alone; the replies through the commands' own bot
        tg.on_update(&json!({"update_id": 1, "message": {"text": "/id", "chat": {"id": 5}, "from": {"id": 9}}}));
        tg.on_update(&json!({"update_id": 2, "message": {"text": "/status", "chat": {"id": 5}, "from": {"id": 9}}}));
        tg.on_update(&json!({"update_id": 3, "message": {"text": "/status@bot", "chat": {"id": 5}, "from": {"id": 42}}}));
        tokio::task::yield_now().await;
        tokio::task::yield_now().await;
        tg.drain(2.0).await;
        let sent = sent.0.borrow();
        assert_eq!(sent.len(), 2, "{sent:?}");
        assert!(sent[0].0.contains("/bot222:commands/sendMessage"));
        assert!(sent[0].1["text"].as_str().unwrap().contains("你的用户 ID：9"));
        assert!(sent[1].1["text"].as_str().unwrap().contains("规则吃单"));
    });
}

/// Telegram's side for the tests: every call recorded (its method and body), answered by ``answer`` from the method's name.
struct FakeTelegram {
    calls: RefCell<Vec<(String, Value)>>,
    answer: Box<dyn Fn(&str) -> Res<Vec<u8>>>,
}

impl FakeTelegram {
    fn new(answer: impl Fn(&str) -> Res<Vec<u8>> + 'static) -> Rc<FakeTelegram> {
        Rc::new(FakeTelegram { calls: RefCell::new(vec![]), answer: Box::new(answer) })
    }

    fn methods(&self) -> Vec<String> {
        self.calls.borrow().iter().map(|(m, _)| m.clone()).collect()
    }

    fn last(&self, method: &str) -> Value {
        self.calls.borrow().iter().rev().find(|(m, _)| m == method).map(|(_, b)| b.clone()).unwrap_or_default()
    }
}

impl Transport for FakeTelegram {
    fn request(&self, req: Req) -> LocalBoxFuture<'_, Res<Vec<u8>>> {
        Box::pin(async move {
            let method = req.url.rsplit('/').next().unwrap_or("").to_string();
            let body: Value =
                serde_json::from_slice(req.payload.as_deref().map(Vec::as_slice).unwrap_or(b"{}")).unwrap_or_default();
            self.calls.borrow_mut().push((method.clone(), body));
            (self.answer)(&method)
        })
    }
}

async fn settle_tasks() {
    crate::sync::sleep(0.05).await; // (a command's own awaits: the withdrawal, the wait for the disk)
}

#[test]
fn telegram_menu_buttons_and_their_yes() {
    run(async {
        let fake = FakeTelegram::new(|_| Ok(br#"{"ok":true,"result":true}"#.to_vec()));
        let tg = Rc::new(crate::telegram::Telegram::new("", &[], 42, true, true, "222:commands", fake.clone()));
        let r = rig(start() - 3_600_000, &[]);
        let me = r.me.clone();
        *tg.handler.borrow_mut() = Some(Rc::new(move |c: String, a: String| {
            let me = me.clone();
            Box::pin(async move { control::command(&me, &c, &a).await })
        }));
        let me = r.me.clone();
        *tg.menu_head.borrow_mut() = Some(Rc::new(move || me.borrow().menu_head()));
        let tap = |data: &str, user: i64| {
            json!({"update_id": 1, "callback_query": {"id": "q1", "data": data, "from": {"id": user},
                   "message": {"message_id": 77, "chat": {"id": 42}}}})
        };
        // /menu: a card with the buttons, the mode in force ticked
        tg.on_update(&json!({"update_id": 1, "message": {"text": "/menu", "chat": {"id": 42}, "from": {"id": 42}}}));
        tg.drain(1.0).await;
        let card = fake.last("sendMessage");
        assert!(card["text"].as_str().unwrap().contains("规则吃单菜单"), "{card}");
        let keys = card["reply_markup"]["inline_keyboard"].to_string();
        assert!(keys.contains("✅ 真实吃单") && keys.contains("c:/pause") && keys.contains("c:/test"), "{keys}");
        // pause: done at once, its answer sent, the card brought up to date
        tg.on_update(&tap("c:/pause", 42));
        settle_tasks().await;
        tg.drain(1.0).await;
        assert!(!r.me.borrow().paused.is_empty());
        assert!(fake.methods().contains(&"answerCallbackQuery".to_string()));
        assert!(fake.last("sendMessage")["text"].as_str().unwrap().contains("已暂停"));
        assert_eq!(fake.last("editMessageText")["message_id"], 77);
        // resume starts real orders again: asked first, nothing done yet
        tg.on_update(&tap("c:/resume", 42));
        settle_tasks().await;
        let ask = fake.last("editMessageText");
        assert!(ask["text"].as_str().unwrap().contains("确定要恢复开新单吗"), "{ask}");
        assert!(!r.me.borrow().paused.is_empty());
        // a "yes" older than a minute: not taken
        let old = crate::util::now_ms() / 1000 - crate::telegram::CONFIRM_S - 5;
        tg.on_update(&tap(&format!("y:{old}:/resume"), 42));
        settle_tasks().await;
        assert!(!r.me.borrow().paused.is_empty());
        assert_eq!(fake.last("answerCallbackQuery")["text"], "确认已过期，请重新点一次");
        // the "yes" in time: resumed
        let yes = ask["reply_markup"]["inline_keyboard"][0][0]["callback_data"].as_str().unwrap().to_string();
        tg.on_update(&tap(&yes, 42));
        settle_tasks().await;
        assert!(r.me.borrow().paused.is_empty());
        // live trading and the order test ask too; someone not the admin, or a made-up button, does nothing
        tg.on_update(&tap("c:/mode on", 42));
        settle_tasks().await;
        assert!(fake.last("editMessageText")["text"].as_str().unwrap().contains("确定要切到真实吃单吗"));
        tg.on_update(&tap("c:/pause", 9));
        settle_tasks().await;
        assert!(r.me.borrow().paused.is_empty());
        assert_eq!(fake.last("answerCallbackQuery")["text"], "这个机器人只听管理员的");
        // made-up buttons: a disguised "live trading" or "resume" (an extra space) never gets past the "yes"
        tg.on_update(&tap("c:/mode paper", 42));
        settle_tasks().await;
        assert_eq!(r.me.borrow().settings.mode, "paper");
        tg.on_update(&tap("c:/mode  on", 42));
        tg.on_update(&tap("y:9999999999:/mode on", 42));
        settle_tasks().await;
        assert_eq!(r.me.borrow().settings.mode, "paper");
        tg.on_update(&tap("c:/pause", 42));
        settle_tasks().await;
        tg.on_update(&tap("c:/resume ", 42));
        settle_tasks().await;
        assert!(!r.me.borrow().paused.is_empty());
    });
}

#[test]
fn telegram_settings_and_review() {
    run(async {
        let fake = FakeTelegram::new(|_| Ok(br#"{"ok":true,"result":true}"#.to_vec()));
        let tg = Rc::new(crate::telegram::Telegram::new("", &[], 42, true, true, "222:commands", fake.clone()));
        let r = rig(start() - 3_600_000, &[]);
        let me = r.me.clone();
        *tg.handler.borrow_mut() = Some(Rc::new(move |c: String, a: String| {
            let me = me.clone();
            Box::pin(async move { control::command(&me, &c, &a).await })
        }));
        let me = r.me.clone();
        *tg.settings_view.borrow_mut() = Some(Rc::new(move || me.borrow().settings_view()));
        let tap = |data: &str| {
            json!({"update_id": 1, "callback_query": {"id": "q1", "data": data, "from": {"id": 42},
                   "message": {"message_id": 78, "chat": {"id": 42}}}})
        };
        // /settings: a card, a button a field
        tg.on_update(&json!({"update_id": 1, "message": {"text": "/settings", "chat": {"id": 42}, "from": {"id": 42}}}));
        tg.drain(1.0).await;
        let card = fake.last("sendMessage");
        assert!(card["text"].as_str().unwrap().contains("⚙️ 设置"), "{card}");
        assert!(card["reply_markup"].to_string().contains("f:RULE_MAX_DAILY_LOSS_USD"));
        // a field: what it does, a button a preset
        tg.on_update(&tap("f:RULE_MAX_DAILY_LOSS_USD"));
        settle_tasks().await;
        let field = fake.last("editMessageText");
        assert!(
            field["text"].as_str().unwrap().contains("今日亏损停机线") && field["text"].as_str().unwrap().contains("现在：不限")
        );
        assert!(field["reply_markup"].to_string().contains("s:RULE_MAX_DAILY_LOSS_USD:50"));
        // a preset: in force, said, the card shows it ticked
        tg.on_update(&tap("s:RULE_MAX_DAILY_LOSS_USD:50"));
        settle_tasks().await;
        tg.drain(1.0).await;
        assert_eq!(r.me.borrow().loss_cap, 50.0);
        assert!(fake.last("sendMessage")["text"].as_str().unwrap().contains("✅ 已保存"));
        assert!(fake.last("editMessageText")["reply_markup"].to_string().contains("✅ $50"));
        // made-up buttons: a value no button offers, a field the page has not — nothing changes
        tg.on_update(&tap("s:RULE_MAX_DAILY_LOSS_USD:999999"));
        tg.on_update(&tap("s:PREDICT_PRIVATE_KEY:0x00"));
        settle_tasks().await;
        assert_eq!(r.me.borrow().loss_cap, 50.0);
        assert_eq!(fake.last("answerCallbackQuery")["text"], "这个按钮已经过时，重新打开设置");
        // back to the settings file's value
        tg.on_update(&tap("r:RULE_MAX_DAILY_LOSS_USD"));
        settle_tasks().await;
        assert_eq!(r.me.borrow().loss_cap, 0.0);
        // typed: any value the settings file would take
        assert!(control::command(&r.me, "/set", "RULE_GOLD_SHARES 120").await.contains("✅"));
        assert_eq!(r.me.borrow().settings.shares_of("gold"), 120);
        assert!(control::command(&r.me, "/set", "NOPE 1").await.contains("❌ 没改"));
        assert!(control::command(&r.me, "/set", "RULE_GOLD_SHARES").await.contains("用法"));
        assert!(control::command(&r.me, "/reset", "RULE_GOLD_SHARES").await.contains("✅"));
        assert!(control::command(&r.me, "/settings", "").await.contains("RULE_GOLD_SHARES"));
        // the review
        tg.on_update(&tap("v:30"));
        settle_tasks().await;
        tg.drain(1.0).await;
        assert!(fake.last("sendMessage")["text"].as_str().unwrap().contains("📈 复盘（近 30 天"));
        history(&r, 30);
        let text = control::command(&r.me, "/review", "all").await;
        assert!(text.contains("📈 复盘（全部") && text.contains("按规则："), "{text}");
    });
}

#[test]
fn telegram_says_why_the_bot_is_silent() {
    run(async {
        let ok = || Ok(br#"{"ok":true,"result":true}"#.to_vec());
        // commands off: notifications only
        let fake = FakeTelegram::new(move |_| ok());
        let tg = Rc::new(crate::telegram::Telegram::new("", &[], 42, false, true, "222:x", fake.clone()));
        assert!(tg.health().1.contains("TELEGRAM_COMMANDS 不是 on"), "{}", tg.health().1);
        // a wrong token: getMe refused — told, and logged
        let fake = FakeTelegram::new(|m| {
            if m == "getMe" {
                Err(RemoteError::new("HTTP 401: Unauthorized"))
            } else {
                Ok(br#"{"ok":true,"result":[]}"#.to_vec())
            }
        });
        let tg = Rc::new(crate::telegram::Telegram::new("", &[], 42, true, true, "222:x", fake.clone()));
        let task = tokio::task::spawn_local(tg.clone().run());
        crate::sync::sleep(0.05).await;
        let (fine, text) = tg.health();
        assert!(!fine && text.contains("Token 不对"), "{text}");
        tg.stop();
        let _ = task.await;
        // the token in another program's hands: a webhook set on it
        let fake = FakeTelegram::new(|m| match m {
            "getMe" => Ok(br#"{"ok":true,"result":{"username":"rt_tokyo_bot"}}"#.to_vec()),
            "getWebhookInfo" => Ok(br#"{"ok":true,"result":{"url":"https://other.example.com/hook/secret"}}"#.to_vec()),
            "getUpdates" => Err(RemoteError::new("HTTP 409: Conflict: can't use getUpdates method while webhook is active")),
            _ => Ok(br#"{"ok":true,"result":true}"#.to_vec()),
        });
        let tg = Rc::new(crate::telegram::Telegram::new("", &[], 42, true, true, "222:x", fake.clone()));
        let task = tokio::task::spawn_local(tg.clone().run());
        crate::sync::sleep(0.05).await;
        let text = tg.health().1;
        assert!(text.contains("@rt_tokyo_bot") && text.contains("webhook（other.example.com）"), "{text}");
        assert!(!text.contains("secret"));
        tg.stop();
        let _ = task.await;
        // a command from someone not the admin (the admin's id mistyped): they are told their id; the self-check says it
        let fake = FakeTelegram::new(move |m| match m {
            "getMe" => Ok(br#"{"ok":true,"result":{"username":"rt_tokyo_bot"}}"#.to_vec()),
            _ => Ok(br#"{"ok":true,"result":true}"#.to_vec()),
        });
        let tg = Rc::new(crate::telegram::Telegram::new("", &[], 42, true, true, "222:x", fake.clone()));
        *tg.bot_name.borrow_mut() = "@rt_tokyo_bot".into();
        tg.on_update(&json!({"update_id": 1, "message": {"text": "/status", "chat": {"id": 9}, "from": {"id": 9}}}));
        tg.drain(1.0).await;
        assert!(fake.last("sendMessage")["text"].as_str().unwrap().contains("你的用户 ID 是 9"));
        let text = tg.health().1;
        assert!(text.contains("用户 9 发过 /status") && text.contains("ADMIN_USER_ID 是 42"), "{text}");
        // ... and the self-check shows the line
        let r = rig(start() - 3_600_000, &[]);
        r.me.borrow_mut().telegram = Some(tg.clone());
        let check = control::self_check(&r.me).await;
        assert!(check.contains("Telegram：用户 9 发过"), "{check}");
        // no admin id yet: only /id answered, and said so
        let tg = Rc::new(crate::telegram::Telegram::new("", &[], 0, true, true, "222:x", fake.clone()));
        *tg.bot_name.borrow_mut() = "@rt_tokyo_bot".into();
        assert!(tg.health().1.contains("ADMIN_USER_ID 没填"));
    });
}

#[test]
fn the_loop_runs_reads_fills_and_stops_cleanly() {
    run(async {
        let r = rig(start() - 3_600_000, &[]);
        r.me.borrow_mut().running = false;
        r.api.list(MID, &slug_of(start()), "OPEN");
        let task = tokio::task::spawn_local(flow::run(r.me.clone()));
        crate::sync::sleep(0.05).await;
        assert!(r.me.borrow().track.get(&slug_of(start())).is_some_and(|w| w.borrow().market.is_some()));
        r.api.fill_on_create.set(50.0);
        r.push(MID, book(&[("0.55", "100")], &[]));
        r.advance(taker::FILL_READ_MS + 10);
        crate::sync::sleep(1.2).await;
        assert_eq!(r.order(0).f("filled"), 50.0);
        assert_eq!(r.order(0).state(), "final");
        r.me.borrow().stop.set();
        let _ = tokio::time::timeout(std::time::Duration::from_secs(10), task).await.expect("stops");
        assert!(!r.me.borrow().running);
    });
}

fn dec(s: &str) -> rust_decimal::Decimal {
    s.parse().unwrap()
}

fn live_rig(clock_ms: i64) -> (Rig, Rc<crate::index::IndexFeed>) {
    let feed =
        Rc::new(crate::index::IndexFeed::new(&["XAUUSDT"], "https://fapi.binance.test", "wss://fstream.binance.test/market"));
    let r = rig_full(clock_ms, Store::memory(), FakePredict::new(), &[("RULE_GOLD_SHUT_LIVE_CENTS", "80")], Some(feed.clone()));
    (r, feed)
}

#[test]
fn course_buys_only_with_the_index_ahead() {
    run(async {
        let (r, feed) = live_rig(start() - 60_000);
        r.found().await;
        r.clock.set(start() + 90_000); // a minute and a half into the window
        r.me.borrow().track.get(&slug_of(start())).unwrap().borrow_mut().look_ms = 0;
        feed.candles.borrow_mut().insert(("XAUUSDT".into(), start() - 60_000), dec("4000"));
        let now = r.clock.get();
        feed.push_index("XAUUSDT", dec("4000.1"), now - 100, now); // +0.0025%: not the 0.005% the rule asks
        r.push(MID, book(&[("0.70", "100")], &[]));
        assert!(r.created().is_empty());
        assert!(r.word().contains("还没领先"), "{}", r.word());
        feed.push_index("XAUUSDT", dec("4000.4"), now - 50, now); // +0.01%: ahead — the index's arrival is the look
        assert_eq!(r.created().len(), 1, "{}", r.word());
        let body = &r.created()[0]["data"];
        assert_eq!(body["pricePerShare"], (80 * 10u128.pow(16)).to_string());
        assert_eq!(body["order"]["expiration"], ((start() + 15 * 60_000) / 1000 - taker::EXPIRY_BEFORE_S).to_string());
        r.sends().await;
        let o = r.order(0);
        assert!(o.is_true("live"));
        assert!(o.t("index_wall"));
    });
}

#[test]
fn first_minute_no_dearer_than_before() {
    run(async {
        let (r, feed) = live_rig(start() - 60_000);
        r.found().await;
        r.clock.set(start() + 20_000);
        r.me.borrow().track.get(&slug_of(start())).unwrap().borrow_mut().look_ms = 0;
        feed.candles.borrow_mut().insert(("XAUUSDT".into(), start() - 60_000), dec("4000"));
        let now = r.clock.get();
        feed.push_index("XAUUSDT", dec("4001"), now - 100, now);
        r.push(MID, book(&[("0.70", "100")], &[]));
        assert!(r.created().is_empty()); // 70¢ is above the 58¢ before the window: not in the first minute
        r.push(MID, book(&[("0.57", "10")], &[]));
        assert_eq!(r.created().len(), 1, "{}", r.word());
        assert_eq!(r.created()[0]["data"]["pricePerShare"], (58 * 10u128.pow(16)).to_string());
        r.sends().await;
    });
}

/// 600 windows of the last 90 days already on the records (settled, an order each), as a service that has run a while.
fn history(r: &Rig, n: i64) {
    let mut t = r.me.borrow_mut();
    let now = t.now();
    for i in 0..n {
        let at = now - (i + 1) * 3 * 3_600_000;
        let slug = format!("xau-updown-15m-{}", at / 1000);
        let rec = json!({"slug": slug, "rule": "gold_shut", "card": "XAU-15M", "side": "up", "mode": "on", "name": "黄金 15分涨跌",
            "start_ms": at, "end_ms": at + 900_000, "market_id": format!("{i}"), "index_set": 1, "cents": 58, "result": "up", "pnl": 1.5,
            "settled_day": day_of(at), "orders": [{"hash": format!("0x{i:064x}"), "id": format!("o{i}"), "state": "final", "shares": 50.0,
            "limit": 0.58, "filled": 50.0, "avg": 0.55, "fee_bps": 200, "at": at - 60_000, "expires": at / 1000 - 5,
            "sent_wall": at - 60_000, "seen_wall": at - 60_001, "answered_wall": at - 59_950}]});
        t.recs.insert(slug, rec.as_object().cloned().unwrap());
    }
}

/// The hot path's own time: a pushed frame taken (parsed, merged) to the order's request handed to the transport — the
/// window's look, the sizing, the signing (or the presigned order), the record, the journal. `cargo test --release bench -- --ignored --nocapture`
#[test]
#[ignore]
fn bench_hot_path() {
    run(async {
        for presign in [false, true] {
            let mut took = vec![];
            for round in 0..200 {
                let r = rig(start() - 3_600_000, if presign { &[("RULE_FAST_HOURS", "00:00-24:00")] } else { &[] });
                history(&r, 600);
                r.found().await;
                r.api.create_delay.set(0.001); // (the answer later, as from the network: the time is the way out alone)
                // (signed in and the allowance read beforehand, as the warm-up does before a busy stretch)
                let api = r.me.borrow().api.clone();
                api.ensure_auth().await.unwrap();
                flow::read_allowance(&r.me, "CTF_EXCHANGE").await.unwrap();
                if presign {
                    let mut t = r.me.borrow_mut();
                    let now = t.now();
                    let _ = t.pace(now);
                    look::presign(&mut t, now);
                }
                let frame = serde_json::to_string(&json!({"type": "M", "topic": format!("predictOrderbook/{MID}"),
                    "data": book(&[("0.55", "100"), ("0.56", "40"), ("0.60", "300")], &[("0.40", "50"), ("0.39", "80")])}))
                .unwrap();
                let began = std::time::Instant::now();
                let msg: Value = serde_json::from_str(&frame).unwrap();
                r.stream.take(MID, &msg["data"]);
                let elapsed = began.elapsed().as_secs_f64() * 1e6;
                if r.created().len() != 1 {
                    let t = r.me.borrow();
                    let w = t.track.get(&slug_of(start())).unwrap().borrow();
                    panic!(
                        "round {round} presign {presign}: {} | busy {} look_ms {} now {} first_ticks {} changed {:?} running {} mode {}",
                        w.word,
                        w.busy,
                        w.look_ms,
                        t.now(),
                        t.first_ticks,
                        t.stream.changed.borrow(),
                        t.running,
                        t.settings.mode
                    );
                }
                if round >= 20 {
                    took.push(elapsed);
                }
                r.sends().await;
            }
            took.sort_by(|a, b| a.partial_cmp(b).unwrap());
            println!(
                "{}（记录里已有 600 场）：推送到达→请求交给网络 中位 {:.0} µs（最快 {:.0} µs，p90 {:.0} µs）",
                if presign { "预先签好" } else { "当场签名" },
                took[took.len() / 2],
                took[0],
                took[took.len() * 9 / 10]
            );
        }
    });
}

#[test]
#[ignore]
fn bench_hot_path_parts() {
    run(async {
        let (mut parse, mut look_ms, mut sign) = (vec![], vec![], vec![]);
        for round in 0..200 {
            let r = rig(start() - 3_600_000, &[]);
            r.found().await;
            r.api.create_delay.set(0.001);
            let api = r.me.borrow().api.clone();
            api.ensure_auth().await.unwrap();
            flow::read_allowance(&r.me, "CTF_EXCHANGE").await.unwrap();
            let hook = r.stream.on_change.borrow_mut().take();
            let data = book(&[("0.55", "100"), ("0.56", "40"), ("0.60", "300")], &[("0.40", "50"), ("0.39", "80")]);
            let began = std::time::Instant::now();
            r.stream.take(MID, &data);
            let a = began.elapsed().as_secs_f64() * 1e6;
            *r.stream.on_change.borrow_mut() = hook;
            let began = std::time::Instant::now();
            flow::on_change(&r.me, MID);
            let b = began.elapsed().as_secs_f64() * 1e6;
            assert_eq!(r.created().len(), 1);
            let wallet = r.me.borrow().wallet.clone().unwrap();
            let order = crate::wallet::build_order(&wallet, TOKEN_UP, 29 * WEI, 50 * WEI, 200, 1_800_000_000, round as u64);
            let began = std::time::Instant::now();
            let _ = wallet.sign_order(&order, false, false).unwrap();
            let c = began.elapsed().as_secs_f64() * 1e6;
            if round >= 20 {
                parse.push(a);
                look_ms.push(b);
                sign.push(c);
            }
            r.sends().await;
        }
        for (name, xs) in [("收盘口（解析+存）", &mut parse), ("看盘+下单（到交给网络）", &mut look_ms), ("签名一张", &mut sign)]
        {
            xs.sort_by(|a, b| a.partial_cmp(b).unwrap());
            println!("{name}：中位 {:.1} µs（最快 {:.1}）", xs[xs.len() / 2], xs[0]);
        }
    });
}

#[test]
fn with_no_journal_an_order_waits_for_its_own_record() {
    run(async {
        // no journal (it did not open, or the disk refused its line) and a database that takes no write: the order's own record
        // must reach the disk before its request leaves — it never does here, so nothing is sent
        let dir = std::env::temp_dir().join(format!("ruletaker-nojournal-{}-{}", std::process::id(), crate::util::now_ms()));
        let path = dir.join("t.sqlite3").to_string_lossy().to_string();
        let r = rig_with(start() - 3_600_000, Store::open(&path).unwrap(), FakePredict::new(), &[]);
        r.found().await;
        assert!(r.me.borrow().store.flush(10.0));
        {
            let mut t = r.me.borrow_mut();
            t.journal = None;
            t.flush_wait_s = 0.3;
        }
        let lock = rusqlite::Connection::open(&path).unwrap();
        lock.execute_batch("BEGIN EXCLUSIVE").unwrap();
        r.push(MID, book(&[("0.55", "100")], &[]));
        r.sends().await;
        assert!(r.created().is_empty(), "sent with its record not on disk");
        let o = r.order(0);
        assert_eq!(o.state(), "not_sent");
        assert!(o.s("error").contains("数据库写入未完成"), "{}", o.s("error"));
        drop(lock);
    });
}

fn store_at(name: &str) -> (std::path::PathBuf, String) {
    let dir = std::env::temp_dir().join(format!("ruletaker-{name}-{}-{}", std::process::id(), crate::util::now_ms()));
    let path = dir.join("t.sqlite3").to_string_lossy().to_string();
    (dir, path)
}

#[test]
fn a_mode_switched_while_a_save_waits_for_the_disk_stays() {
    run(async {
        // a save of the control page's settings waits for the database; the mode switched off meanwhile must stay off
        let (_dir, path) = store_at("save-race");
        let r = rig_with(start() - 3_600_000, Store::open(&path).unwrap(), FakePredict::new(), &[]);
        assert!(r.me.borrow().store.flush(10.0));
        r.me.borrow_mut().flush_wait_s = 5.0;
        let lock = rusqlite::Connection::open(&path).unwrap();
        lock.execute_batch("BEGIN EXCLUSIVE").unwrap();
        let me = r.me.clone();
        let save = tokio::task::spawn_local(async move {
            let mut values = serde_json::Map::new();
            values.insert("RULE_GOLD_SHUT_CENTS".into(), json!("61"));
            control::save_settings(&me, &values).await
        });
        crate::sync::sleep(0.05).await;
        let me = r.me.clone();
        let stop = tokio::task::spawn_local(async move { control::control(&me, "mode", "off").await });
        crate::sync::sleep(0.05).await;
        assert_eq!(r.me.borrow().settings.mode, "off");
        drop(lock); // (the database takes the writes again)
        let saved = save.await.unwrap();
        assert!(matches!(&saved, control::SaveResult::Done(m) if m.contains("已保存")), "{saved:?}");
        assert!(stop.await.unwrap().contains("规则吃单改成：关"));
        let t = r.me.borrow();
        assert_eq!(t.settings.mode, "off", "the save put the mode back");
        assert_eq!(t.settings.price("gold_shut"), 61);
        assert!(!t.block(0.0, None).is_empty());
    });
}

#[test]
fn a_save_the_disk_did_not_take_is_never_told_as_saved() {
    run(async {
        let (_dir, path) = store_at("save-late");
        let r = rig_with(start() - 3_600_000, Store::open(&path).unwrap(), FakePredict::new(), &[]);
        assert!(r.me.borrow().store.flush(10.0));
        r.me.borrow_mut().flush_wait_s = 0.3;
        let lock = rusqlite::Connection::open(&path).unwrap();
        lock.execute_batch("BEGIN EXCLUSIVE").unwrap();
        let q = HashMap::new();
        let mut h = HashMap::new();
        h.insert("x-rt".to_string(), "1".to_string());
        let reply =
            crate::server::route(&r.me, "", "POST", "/api/settings", &q, &h, br#"{"values":{"RULE_GOLD_SHUT_CENTS":"62"}}"#)
                .await;
        let v: Value = serde_json::from_str(&reply.body).unwrap();
        assert_eq!(v["ok"], false, "{v}");
        assert_eq!(v["applied"], true, "{v}");
        assert!(v["message"].as_str().unwrap().contains("还没写进数据库"), "{v}");
        assert_eq!(r.me.borrow().settings.price("gold_shut"), 62); // (in force now, said so)
        // the pause the same: in force, and said when it is not on disk yet
        let reply = control::control(&r.me, "pause", "test").await;
        assert!(reply.contains("已暂停") && reply.contains("还没写进数据库"), "{reply}");
        drop(lock);
    });
}

#[test]
fn the_order_test_holds_to_the_hard_switch_and_the_pause() {
    run(async {
        let r = rig(start() - 3_600_000, &[("LIVE_TRADING_ALLOWED", "off")]);
        // (a market ready for the test, as it would be: the switch alone holds it)
        r.api.list(MID, &format!("btc-updown-15m-{}", (r.clock.get() - r.clock.get() % 900_000) / 1000), "OPEN");
        r.api.books.borrow_mut().insert(MID.into(), json!({"bids": [["0.40", "10"]], "asks": [["0.60", "10"]]}));
        let text = control::order_test(&r.me).await;
        assert!(text.contains("LIVE_TRADING_ALLOWED"), "{text}");
        assert!(r.created().is_empty());
        let r = rig(start() - 3_600_000, &[]);
        r.found().await;
        r.api.books.borrow_mut().insert(MID.into(), json!({"bids": [["0.40", "10"]], "asks": [["0.60", "10"]]}));
        control::control(&r.me, "pause", "test").await;
        let text = control::order_test(&r.me).await;
        assert!(text.contains("已暂停"), "{text}");
        assert!(r.created().is_empty());
    });
}

#[test]
fn a_pause_during_a_new_connection_holds_the_order() {
    run(async {
        // the order passed its checks, then waits for a new connection's handshake: the pause that comes meanwhile holds it
        let r = rig(start() - 3_600_000, &[]);
        r.found().await;
        r.api.connect_delay.set(0.05);
        r.push(MID, book(&[("0.55", "100")], &[]));
        let me = r.me.clone();
        tokio::task::spawn_local(async move {
            control::control(&me, "pause", "test").await;
        });
        r.sends().await;
        assert!(r.created().is_empty(), "the order went out after the pause");
        let o = r.order(0);
        assert_eq!(o.state(), "not_sent");
        assert!(o.s("error").contains("已暂停"), "{}", o.s("error"));
    });
}

#[test]
fn an_order_test_whose_answer_is_lost_is_found_by_its_hash_and_withdrawn() {
    run(async {
        let r = rig(start() - 3_600_000, &[]);
        r.found().await;
        r.api.books.borrow_mut().insert(MID.into(), json!({"bids": [["0.40", "10"]], "asks": [["0.60", "10"]]}));
        r.api.lose_answer.set(true);
        let text = control::order_test(&r.me).await;
        assert!(text.contains("应答没收到"), "{text}");
        assert!(text.contains("按哈希找到了：订单 #id1"), "{text}");
        assert!(text.contains("3/4 撤单成功"), "{text}");
        assert_eq!(*r.api.removed.borrow(), vec!["id1".to_string()]);
        // and when Predict has no such order: said as unknown, never as not placed
        let r = rig(start() - 3_600_000, &[]);
        r.found().await;
        r.api.books.borrow_mut().insert(MID.into(), json!({"bids": [["0.40", "10"]], "asks": [["0.60", "10"]]}));
        r.me.borrow_mut().lookup_s = 0.01;
        *r.api.create_error.borrow_mut() = Some(RemoteError::new("网络错误 (TimeoutError: timed out)"));
        let text = control::order_test(&r.me).await;
        assert!(text.contains("下单结果未知"), "{text}");
        assert!(!text.contains("结论：❌") && !text.contains("这张测试单没下成"), "{text}");
    });
}

// -- the second review --

#[test]
fn the_day_s_loss_stop_holds_after_a_later_profit_until_resume() {
    run(async {
        let (_dir, path) = store_at("loss-stop");
        let pairs = [("RULE_MAX_DAILY_LOSS_USD", "10")];
        let r = rig_with(start() - 3_600_000, Store::open(&path).unwrap(), FakePredict::new(), &pairs);
        r.found().await;
        let settle = |t: &mut Taker, key: &str, pnl: f64| {
            let day = t.now();
            let rec = json!({"slug": key, "mode": "on", "rule": "gold_shut", "side": "up", "pnl": pnl, "orders": [],
                             "settled_day": day_of(day), "start_ms": day});
            t.recs.insert(key.into(), rec.as_object().cloned().unwrap());
            t.pnl_epoch += 1;
            t.hold_loss_stop(); // (as a settlement does)
        };
        settle(&mut r.me.borrow_mut(), "a", -12.0); // -$12 settled: the $10 line passed
        assert!(r.me.borrow().block(0.0, None).contains("停机线"));
        // a profit settled later the same day (-$7 now, over the line again): the stop holds all the same
        settle(&mut r.me.borrow_mut(), "b", 5.0);
        let why = r.me.borrow().block(0.0, None);
        assert!(why.contains("之后的盈利不解除"), "{why}");
        assert!(r.me.borrow().halted());
        r.push(MID, book(&[("0.55", "100")], &[]));
        assert!(r.created().is_empty(), "{}", r.word());
        assert!(r.me.borrow().status().contains("今天已停机"));
        // ... over a restart too
        assert!(r.me.borrow().store.flush(10.0));
        let again = rig_with(start() - 3_600_000, Store::open(&path).unwrap(), FakePredict::new(), &pairs);
        assert!(again.me.borrow().block(0.0, None).contains("之后的盈利不解除"));
        // /resume lifts it: another $10 from the P&L now (-$7 -> the line at -$17)
        let reply = control::control(&r.me, "resume", "").await;
        assert!(reply.contains("停机线移到 −$17.00"), "{reply}");
        assert_eq!(r.me.borrow().block(0.0, None), "");
        flow::turn(&r.me).await; // (a turn latches nothing over the new line)
        assert_eq!(r.me.borrow().block(0.0, None), "");
        // and the next day starts free
        let (_dir2, path2) = store_at("loss-stop-2");
        let r = rig_with(start() - 3_600_000, Store::open(&path2).unwrap(), FakePredict::new(), &pairs);
        settle(&mut r.me.borrow_mut(), "a", -12.0);
        r.advance(crate::util::DAY_MS);
        assert_eq!(r.me.borrow().loss_stop_why(), "");
    });
}

#[test]
fn the_balance_keeps_what_an_unconfirmed_order_may_still_take() {
    run(async {
        let r = rig(start() - 3_600_000, &[]);
        r.found().await;
        r.me.borrow_mut().usdt = Some(100 * WEI);
        *r.api.create_error.borrow_mut() = Some(RemoteError::new("网络错误 (TimeoutError: timed out)"));
        r.push(MID, book(&[("0.55", "100")], &[]));
        r.sends().await;
        assert_eq!(r.order(0).state(), "unknown");
        // read again 20 s on, the chain still at $100 (nothing taken yet): the $29 the order may take stays aside
        r.advance(20_000);
        let read_again = |r: &Rig| {
            let mut t = r.me.borrow_mut();
            t.balance_asked_ms = t.now();
        };
        read_again(&r);
        let left = r.me.borrow().usd(None).unwrap();
        assert!((left - 71.0).abs() < 1e-9, "{left}");
        // resting on the book with 20 of its 50 matched (the chain has those): the unfilled 30 at 58¢ aside
        let key = crate::rules::rid(&slug_of(start()), "on");
        let hash = r.order(0).s("hash");
        let set = |r: &Rig, state: &str, filled: f64, filled_ms: i64| {
            let mut t = r.me.borrow_mut();
            let o = t.order_mut(&key, &hash).unwrap();
            o.insert("state".into(), json!(state));
            o.insert("filled".into(), json!(filled));
            o.insert("filled_ms".into(), json!(filled_ms));
        };
        let now = r.me.borrow().now();
        set(&r, "open", 20.0, now - 60_000);
        let left = r.me.borrow().usd(None).unwrap();
        assert!((left - (100.0 - 30.0 * 0.58)).abs() < 1e-9, "{left}");
        // filled since the read began (the chain may not have it yet): the whole order aside
        set(&r, "open", 20.0, now - 1_000);
        let left = r.me.borrow().usd(None).unwrap();
        assert!((left - 71.0).abs() < 1e-9, "{left}");
        // done, its fills older than the read: nothing aside (the chain has them)
        set(&r, "final", 50.0, now - 60_000);
        assert!((r.me.borrow().usd(None).unwrap() - 100.0).abs() < 1e-9);
    });
}

#[test]
fn an_order_that_got_no_connection_holds_nothing_and_is_tried_again_soon() {
    run(async {
        let r = rig(start() - 3_600_000, &[]);
        r.found().await;
        r.api.refuse_connect.set(true);
        r.push(MID, book(&[("0.55", "100")], &[]));
        r.sends().await;
        let o = r.order(0);
        assert_eq!(o.state(), "not_sent", "{}", o.s("error"));
        assert!(o.s("error").starts_with(crate::http::NOT_CONNECTED));
        {
            let t = r.me.borrow();
            assert_eq!(held(t.record(&slug_of(start()), Some("on")).unwrap()), 0.0); // the window's shares all still free
            let w = t.track.get(&slug_of(start())).unwrap().borrow();
            assert_eq!(w.look_ms, t.now() + taker::CONNECT_RETRY_MS); // looked at again in a moment, not half a minute
        }
        r.api.refuse_connect.set(false);
        r.advance(taker::CONNECT_RETRY_MS + 1);
        r.push(MID, book(&[("0.54", "100")], &[]));
        r.sends().await;
        assert_eq!(r.created().len(), 1);
        assert_eq!(r.taker_amount(0), 50 * WEI); // the whole window, as if nothing had happened
    });
}

#[test]
fn an_empty_answer_never_closes_an_order_with_an_id() {
    run(async {
        let r = rig(start() - 3_600_000, &[]);
        r.found().await;
        r.push(MID, book(&[("0.55", "100")], &[]));
        r.sends().await;
        let o = r.order(0);
        assert_eq!((o.state(), o.s("id").as_str()), ("open", "id1"));
        // long past its end, Predict's read answers 200 with nothing in it: no word on the order — read again, not closed
        r.clock.set(o.i("expires") * 1000 + taker::GIVE_UP_MS + 1_000);
        r.api.order_reads.borrow_mut().push_back("empty");
        orders::follow(&r.me).await;
        let o = r.order(0);
        assert_eq!(o.state(), "open", "{}", o.s("error"));
        assert!(o.s("error").contains("查单应答里没有"), "{}", o.s("error"));
        // the read back: its fills taken
        {
            let hash = o.s("hash");
            let mut orders = r.api.orders.borrow_mut();
            let row = orders.get_mut(&hash).unwrap();
            row.insert("status".into(), json!("FILLED"));
            row.insert("amountFilled".into(), json!((50 * WEI).to_string()));
        }
        r.advance(taker::UNKNOWN_EVERY_MS + 1);
        orders::follow(&r.me).await;
        let o = r.order(0);
        assert_eq!((o.state(), o.f("filled")), ("final", 50.0));
        // an order Predict never gave an id to: an empty answer is as good as its 404 (the API's own word for none)
        let api = r.me.borrow().api.clone();
        r.api.order_reads.borrow_mut().push_back("empty");
        assert!(api.order("", &o.s("hash")).await.unwrap().is_none());
        r.api.order_reads.borrow_mut().push_back("empty");
        assert!(api.order("id1", &o.s("hash")).await.is_err());
    });
}

#[test]
fn an_order_test_keeps_the_fills_an_earlier_step_read() {
    run(async {
        let r = rig(start() - 3_600_000, &[]);
        r.found().await;
        r.api.books.borrow_mut().insert(MID.into(), json!({"bids": [["0.40", "10"]], "asks": [["0.60", "10"]]}));
        r.api.fill_on_create.set(10.0); // 10 of its 50 shares matched before the cancel
        r.api.order_reads.borrow_mut().extend(["", "timeout"]); // the last read lost
        let text = control::order_test(&r.me).await;
        assert!(text.contains("2/4 按哈希查到：OPEN，成交 10/50"), "{text}");
        assert!(text.contains("3/4 撤单成功"), "{text}");
        assert!(text.contains("4/4 最终状态没读到") && text.contains("成交按前面查到的 10 份记"), "{text}");
        assert!(!text.contains("结论：✅") && text.contains("最后一次查询没读到"), "{text}");
        let hash = r.created()[0]["data"]["order"]["hash"].as_str().unwrap().to_lowercase();
        let saved = r.me.borrow().store.get(&format!("test:{hash}")).unwrap();
        assert_eq!((saved["filled"].as_f64(), saved["final_read"].as_bool()), (Some(10.0), Some(false)));
    });
}

#[test]
fn a_reset_of_the_wrong_shape_resets_nothing() {
    run(async {
        let r = rig(start() - 3_600_000, &[]);
        let mut values = serde_json::Map::new();
        values.insert("RULE_GOLD_SHUT_CENTS".into(), json!("61"));
        values.insert("RULE_MAX_DAY_USD".into(), json!("100"));
        assert!(matches!(control::save_settings(&r.me, &values).await, control::SaveResult::Done(_)));
        let q = HashMap::new();
        let mut h = HashMap::new();
        h.insert("x-rt".to_string(), "1".to_string());
        let bodies: [&[u8]; 6] = [
            br#"{"reset":"RULE_GOLD_SHUT_CENTS"}"#,
            br#"{"reset":[]}"#,
            br#"{"reset":["NOPE"]}"#,
            br#"{"reset":[1]}"#,
            br#"{"reset":null}"#,
            br#"{"reset":{"RULE_GOLD_SHUT_CENTS":true}}"#,
        ];
        for body in bodies {
            let reply = crate::server::route(&r.me, "", "POST", "/api/settings", &q, &h, body).await;
            assert_eq!(reply.status, 400, "{}", reply.body);
            let t = r.me.borrow();
            assert_eq!((t.saved_settings.len(), t.settings.max_day_usd), (2, 100.0), "{}", String::from_utf8_lossy(body));
        }
        let reply =
            crate::server::route(&r.me, "", "POST", "/api/settings", &q, &h, br#"{"reset":["RULE_GOLD_SHUT_CENTS"]}"#).await;
        assert_eq!(reply.status, 200, "{}", reply.body);
        assert_eq!((r.me.borrow().saved_settings.len(), r.me.borrow().settings.max_day_usd), (1, 100.0));
        let reply = crate::server::route(&r.me, "", "POST", "/api/settings", &q, &h, br#"{"reset":"all"}"#).await;
        assert_eq!(reply.status, 200, "{}", reply.body);
        assert!(r.me.borrow().saved_settings.is_empty());
    });
}

#[test]
fn paper_records_nothing_past_the_15_second_line() {
    run(async {
        // the look begins 16 s before the start; Predict's book takes 2 s to read: no real order could go now, so no
        // paper one is recorded either
        let r = rig(start() - 3_600_000, &[("RULE_TAKER", "paper")]);
        r.found().await;
        r.api.books.borrow_mut().insert(MID.into(), json!({"asks": [["0.55", "100"]], "bids": []}));
        r.stream.fake_alive.set(Some(false)); // (no push to go by: the book read from REST)
        r.clock.set(start() - 16_000);
        r.api.book_ms.set(2_000);
        flow::turn(&r.me).await;
        let bought = r.me.borrow().record(&slug_of(start()), Some("paper")).map(crate::rules::filled).unwrap_or(0.0);
        assert_eq!(bought, 0.0, "{}", r.word()); // (the window itself is over by then: nothing more to look at)
        // the same read done quickly: recorded
        let r = rig(start() - 3_600_000, &[("RULE_TAKER", "paper")]);
        r.found().await;
        r.api.books.borrow_mut().insert(MID.into(), json!({"asks": [["0.55", "100"]], "bids": []}));
        r.stream.fake_alive.set(Some(false));
        r.clock.set(start() - 16_000);
        flow::turn(&r.me).await;
        let bought = r.me.borrow().record(&slug_of(start()), Some("paper")).map(crate::rules::filled).unwrap_or(0.0);
        assert_eq!(bought, 50.0, "{}", r.word());
    });
}

#[test]
fn an_order_test_whose_cancel_answer_names_nothing_reads_the_order_itself() {
    run(async {
        for (mode, done) in [("silent", true), ("ignored", false)] {
            let r = rig(start() - 3_600_000, &[]);
            r.found().await;
            r.api.books.borrow_mut().insert(MID.into(), json!({"bids": [["0.40", "10"]], "asks": [["0.60", "10"]]}));
            r.api.remove_mode.set(mode);
            r.me.borrow_mut().lookup_s = 0.03;
            let text = control::order_test(&r.me).await;
            assert!(
                text.contains("应答里没有它") && text.contains(r#"{"success":true,"data":{"removed":[],"noop":[]}}"#),
                "{text}"
            );
            if done {
                assert!(text.contains("已经是 CANCELLED，撤单生效了") && text.contains("结论：✅"), "{text}");
            } else {
                assert!(text.contains("仍是挂着") && text.contains("撤单没确认"), "{text}");
                assert_eq!(r.api.removed.borrow().len(), 2, "{text}"); // (asked once more)
            }
        }
    });
}

#[test]
fn the_status_text_waits_out_the_race_seconds() {
    run(async {
        // the makers' moment (5 minutes before the start): the race's seconds — /status is worked out once they are over
        let r = rig(start() - 300_000, &[("RULE_LEAD_MINUTES", "5")]);
        r.found().await;
        assert!(r.me.borrow().quiet_ms(start() - 300_000) > 0);
        let me = r.me.clone();
        let asked = tokio::task::spawn_local(async move { control::command(&me, "/status", "").await });
        crate::sync::sleep(0.3).await;
        assert!(!asked.is_finished(), "worked out in the race's seconds");
        r.advance(taker::QUIET_AFTER_MS + 1_000); // the race's seconds over
        let text = tokio::time::timeout(std::time::Duration::from_secs(2), asked).await.unwrap().unwrap();
        assert!(text.contains("规则吃单"), "{text}");
        // out of them: at once
        let text = tokio::time::timeout(std::time::Duration::from_millis(100), control::command(&r.me, "/status", "")).await;
        assert!(text.is_ok());
    });
}

/// What the console, the review page and the Telegram bot cost the one thread that also sends the orders: each of them runs
/// there between two frames, so a frame that arrives meanwhile waits this long at the most (600 windows on record).
#[test]
#[ignore]
fn bench_side_tasks() {
    run(async {
        let r = rig(start() - 3_600_000, &[]);
        history(&r, 600);
        r.found().await;
        let time = |name: &str, n: usize, f: &mut dyn FnMut()| {
            let mut took: Vec<f64> = (0..n)
                .map(|_| {
                    let began = std::time::Instant::now();
                    f();
                    began.elapsed().as_secs_f64() * 1e6
                })
                .collect();
            took.sort_by(|a, b| a.partial_cmp(b).unwrap());
            println!(
                "{name}：中位 {:.0} µs，p99 {:.0} µs，最慢 {:.0} µs",
                took[took.len() / 2],
                took[took.len() * 99 / 100],
                took[took.len() - 1]
            );
        };
        time("控制台刷新一次（/api/state，页面开着时每 3 秒）", 200, &mut || {
            let _ = serde_json::to_string(&r.me.borrow().state()).unwrap();
        });
        time("状态文字（Telegram /status、rt.sh status）", 200, &mut || {
            let _ = r.me.borrow().status();
        });
        time("复盘计算一次（全部记录；30 秒内用缓存，抢单那几秒不算）", 50, &mut || {
            let t = r.me.borrow();
            let _ = serde_json::to_string(&crate::review::review(&t.recs, &t.settings, t.now(), 0)).unwrap();
        });
        time("导出 CSV（全部记录）", 50, &mut || {
            let _ = crate::review::records_csv(&r.me.borrow().recs);
        });
        let text = r.me.borrow().status();
        time("Telegram 消息排队（切段、入队）", 200, &mut || {
            let _ = crate::telegram::chunks(&text, crate::telegram::LIMIT);
        });
    });
}

/// A real record of a finished window, its shares bought and its order final: ``index_set`` the outcome it bought.
fn bought(t: &mut Taker, key: &str, side: &str, index_set: i64, end_ms: i64, mid: &str) {
    let rec = json!({"slug": key, "name": key, "mode": "on", "rule": "gold_shut", "side": side, "index_set": index_set,
        "market_id": mid, "start_ms": end_ms - 900_000, "end_ms": end_ms,
        "orders": [{"hash": format!("0x{key}"), "id": "", "state": "final", "shares": 10.0, "filled": 10.0, "limit": 0.55,
                    "avg": 0.55, "fee_bps": 0, "at": end_ms - 1_000_000}]});
    t.recs.insert(key.into(), rec.as_object().cloned().unwrap());
}

/// A market settled: the outcome with ``won`` its indexSet marked WON, under the names given.
fn settled(mid: &str, names: [&str; 2], won: i64) -> Obj {
    let row = |i: i64, name: &str| {
        json!({"name": name, "indexSet": i, "onChainId": format!("{i}{i}{i}"),
                                         "status": if i == won { "WON" } else { "LOST" }})
    };
    json!({"id": mid, "status": "RESOLVED", "outcomes": [row(1, names[0]), row(2, names[1])]}).as_object().cloned().unwrap()
}

#[test]
fn a_window_settles_on_the_outcome_it_bought_whatever_its_name() {
    run(async {
        let r = rig(start(), &[]);
        let end = start() - 3_600_000;
        {
            let mut t = r.me.borrow_mut();
            // "Up or Flat" / "Down": the order bought outcome 1 for Up (named by its partner), outcome 2 for Down
            bought(&mut t, "up-won", "up", 1, end, "m1");
            bought(&mut t, "down-lost", "down", 2, end, "m1");
            bought(&mut t, "old-unnamed", "up", 0, end, "m1"); // (written before records kept the outcome)
        }
        r.api.markets.borrow_mut().insert("m1".into(), settled("m1", ["Up or Flat", "Down"], 1));
        orders::settle(&r.me).await;
        let t = r.me.borrow();
        let (won, lost, old) = (&t.recs["up-won"], &t.recs["down-lost"], &t.recs["old-unnamed"]);
        assert_eq!((won.f("pnl"), won.s("result").as_str()), (4.5, "up"), "{won:?}");
        assert_eq!((lost.f("pnl"), lost.s("result").as_str()), (-5.5, "up"), "{lost:?}");
        // a record without the outcome it bought, on a name not understood: told once, never settled on a guess
        assert!(old.none("pnl") && old.t("settle_alarmed"), "{old:?}");
        assert!(old.s("settle_error").contains("结果名称认不出：Up or Flat"));
        // the day's loss counts the lost window (the loss stop sees it)
        assert!((t.today_pnl() - (4.5 - 5.5)).abs() < 1e-9);
    });
}

#[test]
fn a_window_predict_is_slow_to_settle_is_read_less_and_less_often() {
    run(async {
        let r = rig(start(), &[]);
        let reads = |r: &Rig| r.api.calls.borrow().iter().filter(|c| *c == "GET /markets/m2").count();
        bought(&mut r.me.borrow_mut(), "slow", "up", 1, start() - 5 * 60_000, "m2");
        r.api.markets.borrow_mut().insert("m2".into(), market_of("m2", "slow", "CLOSED")); // (not settled yet)
        orders::settle(&r.me).await;
        r.advance(taker::SETTLE_EVERY_MS);
        orders::settle(&r.me).await;
        assert_eq!(reads(&r), 2); // every minute while it is fresh
        r.advance(2 * 3_600_000); // two hours past its end: every ten minutes
        orders::settle(&r.me).await;
        r.advance(5 * 60_000);
        orders::settle(&r.me).await;
        assert_eq!(reads(&r), 3);
        r.advance(5 * 60_000);
        orders::settle(&r.me).await;
        assert_eq!(reads(&r), 4);
        r.advance(crate::util::DAY_MS); // a day past it: every hour
        orders::settle(&r.me).await;
        r.advance(30 * 60_000);
        orders::settle(&r.me).await;
        assert_eq!(reads(&r), 5);
        r.advance(30 * 60_000);
        orders::settle(&r.me).await;
        assert_eq!(reads(&r), 6);
    });
}

#[test]
fn an_order_past_its_expiry_holds_nothing_once_a_read_has_seen_it() {
    run(async {
        let r = rig(start() - 3_600_000, &[]);
        r.found().await;
        r.me.borrow_mut().usdt = Some(100 * WEI);
        *r.api.create_error.borrow_mut() = Some(RemoteError::new("网络错误 (TimeoutError: timed out)"));
        r.push(MID, book(&[("0.55", "100")], &[]));
        r.sends().await;
        let o = r.order(0);
        assert_eq!(o.state(), "unknown"); // (its answer lost: it may be on the book)
        let read_at = |r: &Rig, at: i64| {
            let mut t = r.me.borrow_mut();
            r.clock.set(at);
            t.balance_asked_ms = at;
            t.usd(None).unwrap()
        };
        let expiry = o.i("expires") * 1000;
        // a read a minute past its expiry may not have what it matched just before: still aside
        assert!((read_at(&r, expiry + 60_000) - 71.0).abs() < 1e-9);
        // a read begun well past it has everything it ever took: nothing aside, however long its answer stays lost
        assert!((read_at(&r, expiry + taker::GIVE_UP_MS + taker::BALANCE_LAG_MS + 1) - 100.0).abs() < 1e-9);
        assert_eq!(r.order(0).state(), "unknown");
    });
}

#[test]
fn one_order_test_at_a_time() {
    run(async {
        let r = rig(start() - 3_600_000, &[]);
        r.found().await;
        r.api.books.borrow_mut().insert(MID.into(), json!({"bids": [["0.40", "10"]], "asks": [["0.60", "10"]]}));
        r.api.create_delay.set(0.05); // (the first one's order on its way when the second is asked)
        let (a, b) = tokio::join!(control::order_test(&r.me), control::order_test(&r.me));
        assert!(a.contains("1/4"), "{a}");
        assert!(b.contains("挂单测试正在进行"), "{b}");
        assert_eq!(r.created().len(), 1);
        assert!(!r.me.borrow().testing);
        let again = control::order_test(&r.me).await; // (done: the next one runs)
        assert!(again.contains("1/4"), "{again}");
        assert_eq!(r.created().len(), 2);
    });
}

#[test]
fn a_yes_tapped_twice_runs_its_command_once() {
    run(async {
        let fake = FakeTelegram::new(|_| Ok(br#"{"ok":true,"result":true}"#.to_vec()));
        let tg = Rc::new(crate::telegram::Telegram::new("", &[], 42, true, true, "222:commands", fake.clone()));
        let runs = Rc::new(Cell::new(0));
        let counted = runs.clone();
        *tg.handler.borrow_mut() = Some(Rc::new(move |_c: String, _a: String| {
            counted.set(counted.get() + 1);
            Box::pin(async {
                crate::sync::sleep(0.02).await;
                "做完了".to_string()
            })
        }));
        let tap = |data: String| {
            json!({"update_id": 1, "callback_query": {"id": "q", "data": data, "from": {"id": 42},
                   "message": {"message_id": 77, "chat": {"id": 42}}}})
        };
        let now = crate::util::now_ms() / 1000;
        tg.on_update(&tap(format!("y:{now}:/test")));
        tg.on_update(&tap(format!("y:{now}:/test"))); // (tapped again before the card changed)
        settle_tasks().await;
        assert_eq!(runs.get(), 1);
        assert!(
            fake.calls.borrow().iter().any(|(m, p)| m == "answerCallbackQuery" && p["text"] == "已经确认过，正在执行或已执行完")
        );
        // the card's buttons replaced before the command ran (then the menu brought up to date again)
        assert!(fake.methods().iter().filter(|m| *m == "editMessageText").count() >= 2);
        // a new card on the same message (its own time): its yes runs
        tg.on_update(&tap(format!("y:{}:/test", now - 1)));
        settle_tasks().await;
        assert_eq!(runs.get(), 2);
    });
}

#[test]
fn an_order_already_filled_when_the_pause_withdraws_it_is_not_withdrawn_twice() {
    run(async {
        let r = rig(start() - 3_600_000, &[]);
        r.found().await;
        r.api.create_delay.set(0.02);
        r.api.remove_mode.set("noop"); // (Predict: nothing to withdraw — it is filled)
        r.push(MID, book(&[("0.55", "100")], &[]));
        r.me.borrow().stop.set(); // the service stops while the answer is on its way
        r.sends().await;
        let o = r.order(0);
        assert_eq!(o.state(), "cancelling", "{o:?}");
        assert!(o.t("removed"));
        assert_eq!(*r.api.removed.borrow(), vec!["id1".to_string()]);
        let told = orders::withdraw_all(&r.me, "服务停止").await;
        assert_eq!(*r.api.removed.borrow(), vec!["id1".to_string()], "{told}"); // (asked once, not again)
    });
}

#[test]
fn the_record_keeps_the_shares_as_signed() {
    run(async {
        let r = rig(start() - 3_600_000, &[]);
        r.found().await;
        {
            let mut t = r.me.borrow_mut();
            t.usdt = Some(1_000_000 * WEI);
            t.settings.shares = vec![("gold".into(), 123_456), ("oil".into(), 50)]; // (past the settings' 100 000)
        }
        r.push(MID, book(&[("0.55", "200000")], &[]));
        r.sends().await;
        assert_eq!(r.taker_amount(0), 123_450 * WEI); // Predict's 5 significant digits
        assert_eq!(r.order(0).f("shares"), 123_450.0); // ... and the record's, so a fill is read against it
    });
}

#[test]
fn the_check_asks_for_the_exchange_the_markets_settle_on() {
    run(async {
        let r = rig(start() - 3_600_000, &[]);
        r.found().await;
        {
            let t = r.me.borrow();
            let mut w = t.track.get(&slug_of(start())).unwrap().borrow_mut();
            w.market.as_mut().unwrap().insert("isNegRisk".into(), json!(true));
        }
        r.api.allowed_by.borrow_mut().insert("NEG_RISK_CTF_EXCHANGE", 0);
        let text = control::self_check(&r.me).await;
        assert!(text.contains("❌ 授权：多结果交易所 没有 USDT 额度"), "{text}");
        assert!(!text.contains("标准交易所") && text.contains("结论：❌"), "{text}");
        r.api.allowed_by.borrow_mut().clear();
        let text = control::self_check(&r.me).await;
        assert!(text.contains("✅ 授权：多结果交易所 USDT 额度已授权") && text.contains("结论：✅"), "{text}");
    });
}

#[test]
fn a_calendar_run_out_holds_every_real_order() {
    run(async {
        let r = rig(start() - 3_600_000, &[]);
        r.found().await;
        let today = crate::util::Date(r.clock.get().div_euclid(crate::util::DAY_MS));
        r.me.borrow_mut().cal_last = Some(today.add(10)); // ten days left: told, still trading
        assert!(r.me.borrow().status().contains("还剩 11 天"), "{}", r.me.borrow().status());
        let now = r.me.borrow().now();
        r.me.borrow_mut().plan(now);
        assert_eq!(r.me.borrow().cal_told, day_of(now));
        assert_eq!(r.me.borrow().block(0.0, None), "");
        let text = control::self_check(&r.me).await;
        assert!(text.contains("⚠️ CME 休市日历只排到") && text.contains("结论：✅"), "{text}");
        r.me.borrow_mut().cal_last = Some(today.add(-1)); // over: no real order
        assert!(r.me.borrow().block(0.0, None).contains("CME 休市日历只排到"));
        r.push(MID, book(&[("0.55", "100")], &[]));
        r.sends().await;
        assert!(r.created().is_empty(), "{}", r.word());
        let text = control::self_check(&r.me).await;
        assert!(text.contains("❌ CME 休市日历只排到") && text.contains("结论：❌"), "{text}");
    });
}

#[test]
fn the_journal_is_made_durable_behind_the_order() {
    run(async {
        let (_dir, path) = store_at("journal-sync");
        let r = rig_with(start() - 3_600_000, Store::open(&path).unwrap(), FakePredict::new(), &[]);
        r.found().await;
        let before = r.me.borrow().journal_syncs.load(std::sync::atomic::Ordering::SeqCst);
        r.push(MID, book(&[("0.55", "100")], &[]));
        r.sends().await;
        let mut synced = false;
        for _ in 0..200 {
            if r.me.borrow().journal_syncs.load(std::sync::atomic::Ordering::SeqCst) > before {
                synced = true;
                break;
            }
            crate::sync::sleep(0.005).await;
        }
        assert!(synced);
    });
}

#[test]
fn the_console_refuses_on_the_head_and_takes_no_flood() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpStream;
    run(async {
        let r = rig(start() - 3_600_000, &[]);
        let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        crate::server::serve(r.me.clone(), "127.0.0.1".into(), port, "secret-key-123".into()).await.unwrap();
        let answer = |mut sock: TcpStream| async move {
            let mut got = Vec::new();
            let read = tokio::time::timeout(std::time::Duration::from_secs(2), sock.read_to_end(&mut got)).await;
            (read.is_ok(), String::from_utf8_lossy(&got).to_string())
        };
        // a POST without the key: refused on its head — the body it promises is never waited for
        let mut sock = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        sock.write_all(b"POST /pause HTTP/1.1\r\nX-RT: 1\r\nContent-Length: 60000\r\n\r\n").await.unwrap();
        let (in_time, text) = answer(sock).await;
        assert!(in_time && text.starts_with("HTTP/1.1 403"), "{text}");
        assert!(r.me.borrow().paused.is_empty());
        // as many idle connections as it serves at once: the next one is closed at once
        let mut idle = vec![];
        for _ in 0..crate::server::MAX_CONNECTIONS {
            idle.push(TcpStream::connect(("127.0.0.1", port)).await.unwrap());
        }
        crate::sync::sleep(0.05).await;
        let (in_time, text) = answer(TcpStream::connect(("127.0.0.1", port)).await.unwrap()).await;
        assert!(in_time && text.is_empty(), "{text}");
        // one of them gone: served again
        drop(idle.pop());
        crate::sync::sleep(0.05).await;
        let mut sock = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        sock.write_all(b"GET /healthz HTTP/1.1\r\n\r\n").await.unwrap();
        let (in_time, text) = answer(sock).await;
        assert!(in_time && text.starts_with("HTTP/1.1 "), "{text}");
    });
}

/// The probe's stand-in API: a quarter hour's BTC market (and the next one's) and an empty book.
fn probe_api(_method: &str, path: &str, _body: &[u8]) -> Vec<u8> {
    let data = match path.rsplit_once("btc-updown-15m-") {
        Some((_, quarter)) => {
            let q: i64 = quarter.parse().unwrap_or(0);
            json!({"markets": [{"id": format!("m{}", q % 1800 / 900), "conditionId": "0x01", "title": "BTC Up or Down"}]})
        }
        None => json!({"bids": [], "asks": []}),
    };
    let body = json!({"success": true, "data": data}).to_string();
    format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{body}", body.len()).into_bytes()
}

#[test]
fn the_probe_times_each_pushed_change_and_answers_the_heartbeat() {
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::Message;
    run(async {
        let (api_port, _) = crate::http::tests::serve(probe_api, 0).await;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let ws_port = listener.local_addr().unwrap().port();
        let echoed = Rc::new(RefCell::new(vec![]));
        let heard = echoed.clone();
        tokio::task::spawn_local(async move {
            let (tcp, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(tcp).await.unwrap();
            while let Some(Ok(frame)) = ws.next().await {
                let Message::Text(text) = frame else { continue };
                let msg: Value = serde_json::from_str(text.as_str()).unwrap();
                if msg["method"] == "heartbeat" {
                    heard.borrow_mut().push(msg["data"].clone());
                    continue;
                }
                let topic = msg["params"][0].as_str().unwrap_or("").to_string();
                let send = |v: Value| Message::text(v.to_string());
                let now = || crate::util::now_ms() as f64;
                ws.send(send(json!({"type": "R", "requestId": msg["requestId"], "success": true}))).await.unwrap();
                ws.send(send(json!({"type": "M", "topic": topic, "data": {"updateTimestampMs": now() - 5000.0}}))).await.unwrap();
                ws.send(send(json!({"type": "M", "topic": "heartbeat", "data": 7}))).await.unwrap();
                for _ in 0..4 {
                    // four changes, each pushed 40 ms after Predict's own stamp
                    crate::sync::sleep(0.02).await;
                    ws.send(send(json!({"type": "M", "topic": topic, "data": {"updateTimestampMs": now() - 40.0}})))
                        .await
                        .unwrap();
                }
            }
        });
        // Binance's index: a markPrice push every 50 ms, each 3 ms after Binance's own event time
        let binance = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let binance_port = binance.local_addr().unwrap().port();
        tokio::task::spawn_local(async move {
            let (tcp, _) = binance.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(tcp).await.unwrap();
            loop {
                let e = crate::util::now_ms() - 3;
                let frame = json!({"stream": "xauusdt@markPrice@1s",
                                   "data": {"e": "markPriceUpdate", "E": e, "s": "XAUUSDT", "i": "4000.5"}});
                if ws.send(Message::text(frame.to_string())).await.is_err() {
                    break;
                }
                crate::sync::sleep(0.05).await;
            }
        });
        let binance_url = format!("ws://127.0.0.1:{binance_port}");
        let pairs = [("RULE_TAKER", "off"), ("RULE_REGION", "test"), ("BINANCE_WS_URL", binance_url.as_str())];
        let config = Config::from_env(&env_of(&pairs)).unwrap();
        let base = format!("http://127.0.0.1:{api_port}/v1");
        let (reads, lags, index) = crate::probe(config, 2, &base, &format!("ws://127.0.0.1:{ws_port}/ws"), 0.8).await;
        assert_eq!(reads.len(), 2);
        assert_eq!(lags.len(), 8, "{lags:?}"); // two markets × four changes; the books as they stood (5 s old) not counted
        assert!(lags.iter().all(|l| (35.0..400.0).contains(l)), "{lags:?}");
        assert_eq!(*echoed.borrow(), vec![json!(7), json!(7)]);
        assert!(index.len() >= 5, "{index:?}"); // (the index watched over the same seconds)
        assert!(index.iter().all(|l| (0.0..200.0).contains(l)), "{index:?}");
    });
}
