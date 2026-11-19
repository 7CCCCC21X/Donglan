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
            for row in self.orders.borrow_mut().values_mut() {
                if ids.contains(&row.s("id")) && row.s("status") == "OPEN" {
                    row.insert("status".into(), json!("CANCELLED"));
                }
            }
            return Self::ok(json!({"removed": ids, "noop": []}));
        }
        if let Some(hash) = path.strip_prefix("/orders/") {
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
                    let a = self.chain_allowed.get();
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
                    self.created.borrow_mut().push(req.payload.as_deref().cloned().unwrap_or_default()); // (as the request lands)
                    if self.create_delay.get() > 0.0 {
                        crate::sync::sleep(self.create_delay.get()).await;
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

pub fn rig_full(clock_ms: i64, store: Store, api: Rc<FakePredict>, pairs: &[(&str, &str)], index: Option<Rc<crate::index::IndexFeed>>) -> Rig {
    let config = Config::from_env(&env_of(pairs)).unwrap();
    let wallet = Rc::new(Wallet::new(KEY, 56, "").unwrap());
    let transport: Rc<dyn Transport> = api.clone();
    let predict = Rc::new(PredictApi::new("https://api.predict.fun/v1", "k", Some(wallet.clone()), 0.0, transport.clone()));
    let chain = Rc::new(Chain::new("https://bsc-rpc.test/", wallet.clone(), transport));
    let stream = Rc::new(BookStream::new("", 1));
    stream.fake_alive.set(Some(true));
    *api.stream.borrow_mut() = Some(stream.clone());
    let clock = Rc::new(Cell::new(clock_ms));
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
        assert!(control::save_settings(&r.me, &values).await.unwrap().contains("已保存"));
        assert_eq!(r.me.borrow().settings.price("gold_shut"), 61);
        values.insert("RULE_GOLD_SHUT_CENTS".into(), json!("99"));
        assert!(control::save_settings(&r.me, &values).await.is_err());
        assert_eq!(r.me.borrow().settings.price("gold_shut"), 61);
        assert!(control::reset_settings(&r.me, &[]).await.unwrap().contains("已恢复部署设置"));
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
    let feed = Rc::new(crate::index::IndexFeed::new(&["XAUUSDT"], "https://fapi.binance.test", "wss://fstream.binance.test/market"));
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
