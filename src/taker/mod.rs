//! The rule taker, in a process of its own (huice rules.RuleTaker 1.154.27, as the Python version has it).
//!
//! One loop works out the rules' windows of the next day, finds each one's market as Predict lists it, watches its matching
//! status and book before the start, buys what the rule allows, follows its orders to Predict's final word and settles each
//! window on Predict's result. What is kept from huice as it was: the windows and the prices, the sizes and the caps, the order
//! itself (a LIMIT at the rule's price, for all the window may still buy when enough is on offer; its rest withdrawn
//! FILL_READ_MS after its answer; signed to expire EXPIRY_BEFORE_S before the start), the hash on disk before the request
//! leaves, an answer gone missing (or a 409) looked for by the hash and never sent again, the late fills, the fee taken in
//! shares, the race's clock, the per-second hours, the makers' moment, the wait for matching to open, the in-play rules.
//!
//! For speed: a pushed book is looked at in the very turn that brought it (the WebSocket's reading task calls the look and
//! the order's request starts writing before that task reads the next frame); the order a window would place when the makers'
//! ladder shows is signed beforehand; the connection to Predict is kept warm through the busy stretches; no garbage collector.
//!
//! State lives in ``Taker`` behind a RefCell (``Me``); the async steps borrow it only between their waits, never across one.
pub mod control;
pub mod flow;
pub mod look;
pub mod orders;
pub mod text;

use crate::config::{Config, Env, RuleSettings};
use crate::console::{self, Saved};
use crate::index::IndexFeed;
use crate::predict::{Chain, PredictApi};
use crate::rules::{
    FINAL_STATES, NEVER_PLACED, PREFIX, Recs, Rule, day_pnl, is_final, mode_name, never_placed, orders as rec_orders, rid,
    rule_of,
};
use crate::store::Store;
use crate::stream::BookStream;
use crate::sync::Event;
use crate::telegram::Telegram;
use crate::util::{DAY_MS, Obj, Py, clean_error, day_of, jf, money, mono, now_ms};
use crate::wallet::{Wallet, from_wei};
use indexmap::IndexMap;
use rust_decimal::Decimal;
use serde_json::{Value, json};
use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet, VecDeque};
use std::rc::{Rc, Weak};
use tokio::task::JoinHandle;

pub const TICK_S: f64 = 1.0;
pub const PLAN_EVERY_MS: i64 = 60_000;
pub const HORIZON_MS: i64 = 24 * 3_600_000 + 10 * 60_000;
pub const LISTED_AHEAD_MS: i64 = 24 * 3_600_000;
pub const PROBE_EARLY_MS: i64 = 120_000;
pub const PROBE_FAST_S: f64 = 2.0;
pub const PROBE_SLOW_S: f64 = 30.0;
pub const PROBE_LATE_MS: i64 = 5 * 60_000;
pub const NEAR_MS: i64 = 15 * 60_000;
pub const FAST_S: f64 = 2.0;
pub const SLOW_S: f64 = 30.0;
pub const CLOSED_SLOW_S: f64 = 60.0;
pub const OFFER_STATUS_S: f64 = 10.0;
pub const STATUS_RUSH_S: f64 = 0.25;
pub const STATUS_RUSH_MS: i64 = 20_000;
pub const RUSH_READS_MAX: i64 = 240;
pub const QUIET_BEFORE_MS: i64 = 1_000;
pub const QUIET_AFTER_MS: i64 = 15_000;
pub const COURSE_QUIET_MS: i64 = 12_000;
pub const RUSH_QUIET_MS: i64 = 3_000;
pub const INDEX_FRESH_MS: i64 = 1_500;
pub const STOP_MS: i64 = 15_000;
pub const EXPIRY_BEFORE_S: i64 = 5;
pub const SEND_MIN_LEFT_MS: i64 = 5_000;
pub const FILL_READ_MS: i64 = 1_500;
pub const LATE_FILL_MS: i64 = 60_000;
pub const REST_MIN_S: f64 = 0.1;
pub const REST_SOON_S: f64 = 0.005;
pub const FOLLOW_EVERY_MS: i64 = 1_000;
pub const UNKNOWN_EVERY_MS: i64 = 10_000;
pub const GIVE_UP_MS: i64 = 15 * 60_000;
pub const REJECT_PAUSE_MS: i64 = 30_000;
pub const MAX_REJECTS: usize = 5;
pub const SETTLE_AFTER_MS: i64 = 120_000;
pub const SETTLE_EVERY_MS: i64 = 60_000;
pub const KEEP_DAYS: i64 = 90;
pub const WARM_MS: i64 = 60_000;
pub const WARM_EVERY_MS: i64 = 30_000;
pub const FAST_LOOK_S: f64 = 1.0;
pub const FAST_STALE_S: f64 = 20.0;
pub const VERIFY_EVERY_S: f64 = 0.34;
pub const VERIFY_AGAIN_S: f64 = 1.0;
pub const LIVE_START_MS: i64 = 3_000;
pub const LIVE_FULL_MS: i64 = 60_000;
pub const LIVE_STOP_MS: i64 = 15_000;
pub const SWEEP_SHARES: i64 = 20;
pub const SWEEP_PART: i64 = 4;
pub const MAX_OPEN_ORDERS: usize = 3;
pub const JOURNAL_SUFFIX: &str = ".rule-orders";
pub const LAG_EVERY_S: f64 = 0.1;
pub const SPRINT_MS: i64 = 15 * 60_000;
pub const HOT_BEFORE_MS: i64 = 10_000;
pub const HOT_AFTER_MS: i64 = 120_000;
pub const ALLOWANCE_MS: i64 = 60_000;
/// (the Python version's 10**40 is past u128: an allowance read on-chain saturates at u128::MAX, so 10**37 tells the same apart —
/// the "approve max" allowance from any finite one an order could spend)
pub const ALLOWANCE_UNLIMITED: u128 = 10_000_000_000_000_000_000_000_000_000_000_000_000;
pub const ALLOWANCE_LONG_MS: i64 = 3_600_000;
pub const RECHECKED: [&str; 2] = ["规则吃单的设置刚改过", "盘中："];
pub const HELD_BACK_MAX: i64 = 3;
pub const CONFLICT_GRACE_MS: i64 = 90_000;
pub const CONFLICT_MISSES: i64 = 6;
pub const LAST_WITHDRAW_S: f64 = 4.0;
pub const BALANCE_BUSY_MS: i64 = 20_000;
pub const BALANCE_IDLE_MS: i64 = 120_000;
pub const BALANCE_LAG_MS: i64 = 10_000;
pub const RECENT_MS: i64 = 2 * DAY_MS;
pub const SEND_WAIT_S: f64 = 25.0;
pub const READY_RETRY_MS: i64 = 60_000;
pub const STRIKE_EVERY_MS: i64 = 5_000;
pub const STRIKE_FOR_MS: i64 = 180_000;
pub const WARM_CONN_S: f64 = 2.0;
pub const SIGN_CACHE: usize = 64;

pub fn mode_rank(mode: &str) -> i32 {
    match mode {
        "paper" => 1,
        "on" => 2,
        _ => 0,
    }
}

/// A window the taker follows (the Python version's ``w`` dict): one object a window, shared by the steps that look at it.
#[derive(Default)]
pub struct Win {
    pub id: u64,
    pub rule: &'static str,
    pub start: i64,
    pub market: Option<Obj>,
    pub listed_ms: i64,
    pub open_ms: i64,
    pub probe_ms: i64,
    pub look_ms: i64,
    pub status_ms: i64,
    pub offer: bool,
    pub word: String,
    /// the watch: the offers' coming and going (WATCH)
    pub seen: Option<Obj>,
    pub busy: bool,
    pub again: bool,
    pub kept: bool,
    pub look_wall: i64,
    pub fresh: bool,
    pub index_wall: i64,
    pub indexed: i64,
    pub book_at: (i64, i64),
    pub there: f64,
    /// monotonic: the window's last REST read beside the pushes
    pub rest_at: Option<f64>,
    pub open_wall: i64,
    pub rush_wall: i64,
    pub rush_reads: i64,
    pub rushed: bool,
    pub rush_error: String,
    pub rush_task: Option<JoinHandle<()>>,
    pub open_told: bool,
    pub held_back: i64,
    pub strike: Option<Decimal>,
    pub strike_ms: i64,
    pub strike_task: Option<JoinHandle<()>>,
    pub candle_task: Option<JoinHandle<()>>,
}

impl Win {
    pub fn rule(&self) -> &'static Rule {
        rule_of(self.rule).expect("a window's rule is one of RULES")
    }
    pub fn mid(&self) -> Option<String> {
        self.market.as_ref().map(|m| m.get("id").map(crate::util::py_str).unwrap_or_default())
    }
    pub fn watch(&mut self) -> &mut Obj {
        self.seen.get_or_insert_with(crate::rules::watch_default)
    }
    pub fn end(&self) -> i64 {
        self.rule().spec().end_of(self.start)
    }
}

pub type W = Rc<RefCell<Win>>;
pub type Me = Rc<RefCell<Taker>>;

/// (hash, signature) of the orders signed beforehand, by what makes the order.
pub type Signed = IndexMap<String, (String, String)>;

pub struct Taker {
    pub config: Config,
    pub store: Store,
    pub clock: Option<Rc<Cell<i64>>>,
    pub env_mode: String,
    pub base_env: Env,
    pub settings: RuleSettings,
    pub loss_cap: f64,
    pub saved_settings: Saved,
    pub settings_dropped: Vec<String>,
    pub review_held: HashMap<i64, (i64, Value)>,
    pub wallet: Option<Rc<Wallet>>,
    pub api: Rc<PredictApi>,
    pub chain: Option<Rc<Chain>>,
    pub stream: Rc<BookStream>,
    pub index: Option<Rc<IndexFeed>>,
    pub telegram: Option<Rc<Telegram>>,
    pub paused: String,
    pub loss_floor_at: Obj,
    pub ready_error: String,
    pub account_checked: bool,
    pub account_error: String,
    pub ready_ms: i64,
    pub usdt: Option<u128>,
    pub bnb: Option<u128>,
    pub balance_ms: i64,
    pub balance_asked_ms: i64,
    pub balance_error: String,
    pub allowance: HashMap<String, (i64, u128)>,
    pub allowance_task: Option<JoinHandle<()>>,
    pub fast: bool,
    pub warm_ms: i64,
    pub warmed: bool,
    pub warming: Option<JoinHandle<()>>,
    pub verifying: Option<JoinHandle<()>>,
    pub verify_at: f64,
    pub verifies: u64,
    pub verify_diffs: u64,
    pub fast_looks: u64,
    pub status_reads: u64,
    pub first_ticks: u64,
    pub index_looks: u64,
    pub index_late: u64,
    pub verify_error: String,
    pub verify_ms: VecDeque<i64>,
    pub loop_lags: VecDeque<f64>,
    pub side_tasks: Vec<JoinHandle<()>>,
    pub sends: Vec<JoinHandle<()>>,
    pub running: bool,
    pub started_ms: i64,
    pub ticks: u64,
    pub looked: u64,
    pub track: IndexMap<String, W>,
    pub by_market: HashMap<String, String>,
    pub planned_ms: i64,
    /// hash -> slug while its request is out
    pub sending: HashMap<String, String>,
    /// ...and the settings its look decided it under
    pub decided: HashMap<String, RuleSettings>,
    pub signed: Signed,
    pub presigned: u64,
    pub presign_hits: u64,
    pub journal: Option<std::fs::File>,
    pub journal_cut: bool,
    pub errors: VecDeque<(i64, String)>,
    pub error: String,
    pub stop: Rc<Event>,
    pub wakeup: Rc<Event>,
    pub reads: u64,
    pub region: String,
    pub recs: Recs,
    pub recent_keys: HashSet<String>,
    pub pnl_epoch: u64,
    pub pnl_held: RefCell<Option<((String, u64, usize), f64)>>,
    pub next_win: u64,
    /// how long an order with no journal line waits for its record to reach the disk (s)
    pub flush_wait_s: f64,
}

pub struct Parts {
    pub config: Config,
    pub store: Store,
    pub api: Option<Rc<PredictApi>>,
    pub wallet: Option<Rc<Wallet>>,
    pub chain: Option<Option<Rc<Chain>>>,
    pub stream: Option<Rc<BookStream>>,
    pub index: Option<Rc<IndexFeed>>,
    pub telegram: Option<Rc<Telegram>>,
    pub clock: Option<Rc<Cell<i64>>>,
    pub base_env: Option<Env>,
}

impl Taker {
    pub fn new(p: Parts) -> Result<Taker, String> {
        let Parts { config, mut store, api, wallet, chain, stream, index, telegram, clock, base_env } = p;
        let env_mode = config.rules.mode.clone();
        let saved_mode = store.get_str("control:mode");
        let mode = if mode_rank(&saved_mode) < mode_rank(&env_mode)
            && !saved_mode.is_empty()
            && ["off", "paper", "on"].contains(&saved_mode.as_str())
        {
            saved_mode
        } else {
            env_mode.clone()
        };
        let base_env: Env = base_env
            .unwrap_or_else(|| console::current(&config.rules, config.max_daily_loss_usd).into_iter().collect())
            .into_iter()
            .filter(|(k, _)| k.starts_with("RULE_") && k != "RULE_TAKER")
            .collect();
        let saved: Saved = store
            .get(console::SAVED)
            .and_then(|v| v.as_object().cloned())
            .map(|o| o.iter().map(|(k, v)| (k.clone(), crate::util::py_str(v))).collect())
            .unwrap_or_default();
        let (settings, loss_cap, saved_settings, settings_dropped) = console::settle_saved(&base_env, &saved, &mode)?;
        if !settings_dropped.is_empty() {
            let as_json: serde_json::Map<String, Value> = saved_settings.iter().map(|(k, v)| (k.clone(), json!(v))).collect();
            store.put(console::SAVED, &Value::Object(as_json));
        }
        let wallet = match wallet {
            Some(w) => Some(w),
            None if !config.private_key.is_empty() => {
                Some(Rc::new(Wallet::new(&config.private_key, config.chain_id, &config.account)?))
            }
            None => None,
        };
        let live: Rc<dyn crate::predict::Transport> = Rc::new(crate::predict::Live);
        let api = api.unwrap_or_else(|| {
            Rc::new(PredictApi::new(
                crate::predict::api_base(config.chain_id),
                &config.api_key,
                wallet.clone(),
                config.rate_per_min,
                live.clone(),
            ))
        });
        let chain = match chain {
            Some(c) => c,
            None => wallet.clone().map(|w| {
                let url = if config.rpc_url.is_empty() {
                    crate::predict::rpc_url(config.chain_id).to_string()
                } else {
                    config.rpc_url.clone()
                };
                Rc::new(Chain::new(&url, w, live.clone()))
            }),
        };
        let stream = stream.unwrap_or_else(|| Rc::new(BookStream::new(&config.api_key, config.ws_connections)));
        let paused = store.get_str("control:paused");
        let loss_floor_at = store.get("control:loss_floor").and_then(|v| v.as_object().cloned()).unwrap_or_default();
        let account_checked = !wallet.as_ref().map(|w| !w.predict_account.is_empty()).unwrap_or(false);
        let recs: Recs = store
            .take_prefix(PREFIX)
            .into_iter()
            .filter_map(|(k, v)| v.as_object().cloned().map(|o| (k[PREFIX.len()..].to_string(), o)))
            .collect();
        let region = config.region.clone();
        let mut t = Taker {
            config,
            store,
            clock,
            env_mode,
            base_env,
            settings,
            loss_cap,
            saved_settings,
            settings_dropped,
            review_held: HashMap::new(),
            wallet,
            api,
            chain,
            stream,
            index,
            telegram,
            paused,
            loss_floor_at,
            ready_error: "尚未登录 Predict".into(),
            account_checked,
            account_error: String::new(),
            ready_ms: 0,
            usdt: None,
            bnb: None,
            balance_ms: 0,
            balance_asked_ms: 0,
            balance_error: String::new(),
            allowance: HashMap::new(),
            allowance_task: None,
            fast: false,
            warm_ms: 0,
            warmed: false,
            warming: None,
            verifying: None,
            verify_at: 0.0,
            verifies: 0,
            verify_diffs: 0,
            fast_looks: 0,
            status_reads: 0,
            first_ticks: 0,
            index_looks: 0,
            index_late: 0,
            verify_error: String::new(),
            verify_ms: VecDeque::new(),
            loop_lags: VecDeque::new(),
            side_tasks: vec![],
            sends: vec![],
            running: false,
            started_ms: 0,
            ticks: 0,
            looked: 0,
            track: IndexMap::new(),
            by_market: HashMap::new(),
            planned_ms: 0,
            sending: HashMap::new(),
            decided: HashMap::new(),
            signed: IndexMap::new(),
            presigned: 0,
            presign_hits: 0,
            journal: None,
            journal_cut: false,
            errors: VecDeque::new(),
            error: String::new(),
            stop: Rc::new(Event::new()),
            wakeup: Rc::new(Event::new()),
            reads: 0,
            region,
            recs,
            recent_keys: HashSet::new(),
            pnl_epoch: 0,
            pnl_held: RefCell::new(None),
            next_win: 0,
            flush_wait_s: 10.0,
        };
        t.recover();
        let now = t.now();
        t.recent_keys = t.recs.iter().filter(|(_, r)| r.i("start_ms") >= now - RECENT_MS).map(|(k, _)| k.clone()).collect();
        Ok(t)
    }

    // -- plumbing --
    pub fn now(&self) -> i64 {
        self.clock.as_ref().map(|c| c.get()).unwrap_or_else(now_ms)
    }

    pub fn key_of(&self, slug: &str, mode: Option<&str>) -> String {
        rid(slug, mode.unwrap_or(&self.settings.mode))
    }

    pub fn record(&self, slug: &str, mode: Option<&str>) -> Option<&Obj> {
        self.recs.get(&self.key_of(slug, mode))
    }

    /// A record (by its id) written to the store as it now stands.
    pub fn save_key(&mut self, key: &str) {
        let Some(rec) = self.recs.get(key) else { return };
        if rec.i("start_ms") >= self.now() - RECENT_MS {
            self.recent_keys.insert(key.to_string());
        }
        let stored = format!("{PREFIX}{key}");
        self.store.put_record(&stored, rec);
    }

    pub fn save(&mut self, rec: Obj) -> String {
        let key = rid(&rec.s("slug"), &rec.s("mode"));
        self.recs.insert(key.clone(), rec);
        self.save_key(&key);
        key
    }

    /// The records of the last two days' windows (every order placed today or yesterday is on one of them).
    pub fn recent_refs(&self) -> Vec<&Obj> {
        self.recent_keys.iter().filter_map(|k| self.recs.get(k)).collect()
    }

    /// The real records' P&L settled today (Beijing): summed again after a settlement, a new record, a new day.
    pub fn today_pnl(&self) -> f64 {
        let mark = (day_of(self.now()), self.pnl_epoch, self.recs.len());
        let mut held = self.pnl_held.borrow_mut();
        if held.as_ref().map(|(m, _)| *m != mark).unwrap_or(true) {
            let pnl = day_pnl(&self.recs, &mark.0);
            *held = Some((mark, pnl));
        }
        held.as_ref().map(|(_, p)| *p).unwrap_or(0.0)
    }

    pub fn note(&mut self, text: &str) {
        self.error = text.to_string();
        if !text.is_empty() {
            let now = self.now();
            self.errors.push_back((now, text.to_string()));
            while self.errors.len() > 20 {
                self.errors.pop_front();
            }
            crate::warn!("规则吃单：{text}");
        }
    }

    pub fn notify(&self, text: &str) {
        crate::info!("{text}");
        if let Some(t) = &self.telegram {
            if self.config.notify {
                t.send(text);
            }
        }
    }

    /// A problem the operator must see: pushed whatever RULE_NOTIFY says.
    pub fn alarm(&self, text: &str) {
        crate::warn!("{text}");
        if let Some(t) = &self.telegram {
            t.send(text);
        }
    }

    pub fn cooling(&self) -> f64 {
        self.api.cooling()
    }

    /// The USDT this taker may still spend: the balance last read on-chain, less what the real orders placed since just before
    /// that read began may have spent (at their price for an order whose word is final, at their limit in full for one that is
    /// not). None before a read. ``excluding``: the hash of an order not to count.
    pub fn usd(&self, excluding: Option<&str>) -> Option<f64> {
        let usdt = self.usdt?;
        let since = self.balance_asked_ms - BALANCE_LAG_MS;
        let mut spent = 0.0;
        for rec in self.recent_refs() {
            if rec.sv("mode") != "on" || rec.i("start_ms") < since - DAY_MS {
                continue;
            }
            for o in rec_orders(rec) {
                if excluding.is_some_and(|h| !h.is_empty() && o.sv("hash") == h)
                    || o.t("paper")
                    || never_placed(o.state())
                    || o.i("at") < since
                {
                    continue;
                }
                spent +=
                    if is_final(o.state()) { o.f("filled") * crate::rules::order_avg(o) } else { o.f("shares") * o.f("limit") };
            }
        }
        Some(from_wei(usdt) - spent)
    }

    pub fn ready(&self) -> bool {
        self.wallet.is_some() && self.ready_error.is_empty() && self.account_error.is_empty() && self.account_checked
    }

    pub fn not_ready_why(&self) -> String {
        if self.wallet.is_none() {
            return "没有设置 PREDICT_PRIVATE_KEY".into();
        }
        if !self.ready_error.is_empty() {
            return self.ready_error.clone();
        }
        if !self.account_error.is_empty() {
            return self.account_error.clone();
        }
        if !self.account_checked { "Predict 账户的链上核对还没完成".into() } else { String::new() }
    }

    pub fn healthy(&self) -> bool {
        self.running && !self.stop.is_set()
    }

    pub fn configure(&mut self, settings: RuleSettings) {
        self.settings = settings;
        self.planned_ms = 0;
        self.signed.clear();
    }

    pub fn index_market(&mut self, slug: &str, w: &Win) {
        if let Some(mid) = w.mid() {
            self.by_market.insert(mid, slug.to_string());
        }
    }

    pub fn new_win(&mut self, rule: &'static str, start: i64) -> W {
        self.next_win += 1;
        Rc::new(RefCell::new(Win { id: self.next_win, rule, start, ..Default::default() }))
    }

    // -- the gates --
    /// The day's settled P&L at which no new order goes out; None with no cap.
    pub fn loss_floor(&self) -> Option<f64> {
        if self.loss_cap == 0.0 {
            return None;
        }
        let today = day_of(self.now());
        if self.loss_floor_at.s("day") == today {
            let floor = self.loss_floor_at.f("floor");
            return Some(if floor != 0.0 { floor } else { -self.loss_cap });
        }
        Some(-self.loss_cap)
    }

    /// Why no real order goes out now ("" to send): the mode, the deploy's switch, the pause, the day's loss stop, the
    /// readiness, the records' store, the API's cooldown, the balance (spendable beside ``excluding``).
    pub fn block(&self, usd: f64, excluding: Option<&str>) -> String {
        if self.settings.mode != "on" {
            return format!("规则吃单是“{}”，不下单", mode_name(&self.settings.mode));
        }
        if !self.config.trading_allowed {
            return "部署变量 LIVE_TRADING_ALLOWED 不是 on，不下单".into();
        }
        if !self.paused.is_empty() {
            return format!("已暂停（{}）", self.paused);
        }
        if let Some(floor) = self.loss_floor() {
            let pnl = self.today_pnl();
            if pnl <= floor {
                return format!(
                    "今日已结算 {} 已到 RULE_MAX_DAILY_LOSS_USD 的停机线（{}），今天不再开新单",
                    money(pnl),
                    money(floor)
                );
            }
        }
        if !self.ready() {
            return format!("真实交易未就绪：{}", self.not_ready_why());
        }
        let error = self.store.error();
        if !error.is_empty() {
            return format!("记录写不进数据库（{error}），不开新单");
        }
        let left = self.cooling();
        if left > 0.0 {
            return format!("Predict 接口限流冷却中（{} 秒后重试）", left as i64 + 1);
        }
        if let Some(usd_left) = self.usd(excluding) {
            if usd_left < usd - 1e-9 {
                return format!("USDT 余额 ${} 不足本单 ${}", crate::util::comma(usd_left, 2), crate::util::comma(usd, 2));
            }
        }
        String::new()
    }

    /// No real order may be opened now by the operator's word or the service's stop.
    pub fn halted(&self) -> bool {
        if self.settings.mode != "on" || !self.config.trading_allowed || !self.paused.is_empty() || self.stop.is_set() {
            return true;
        }
        self.loss_floor().is_some_and(|floor| self.today_pnl() <= floor)
    }

    /// Judged right before an order's bytes go out: the gates again, the settings the look decided under, the window's start
    /// not passed, in a window's course the index still on the rule's side.
    pub fn before_send(&self, digest: &str) -> String {
        let digest = digest.to_lowercase();
        let Some(slug) = self.sending.get(&digest) else { return "不是规则吃单这一刻要发的订单".into() };
        if self.stop.is_set() {
            return "服务正在停止，不再发单".into();
        }
        if let Some(decided) = self.decided.get(&digest) {
            if *decided != self.settings {
                return "规则吃单的设置刚改过，这张单不发了（下一轮按新设置重新判断）".into();
            }
        }
        let empty = Obj::new();
        let rec = self.record(slug, Some("on")).unwrap_or(&empty);
        let Some(order) = rec_orders(rec).find(|o| o.sv("hash") == digest) else { return "订单记录不见了".into() };
        let now = self.now();
        if order.i("expires") * 1000 - now < SEND_MIN_LEFT_MS {
            return "距订单到期不足 5 秒（开场前 5 秒到期；盘中的单在这一场结束前 5 秒到期），不再发送".into();
        }
        if order.t("live") {
            if now > rec.i("end_ms") - LIVE_STOP_MS {
                return "离这一场结束不足 15 秒，盘中的单不再发送".into();
            }
        } else if now > rec.i("start_ms") - STOP_MS {
            return "离开场不足 15 秒，这张单不再发送（它在路上等了太久）".into();
        }
        let why = self.block(order.f("shares") * order.f("limit"), Some(&digest));
        if !why.is_empty() {
            return why;
        }
        if order.t("live") {
            if let Some(rule) = rule_of(&rec.s("rule")) {
                let why = self.live_guard(slug, rule, now);
                if !why.is_empty() {
                    return format!("盘中：{why}");
                }
            }
        }
        String::new()
    }

    pub fn mode_text(&self) -> String {
        let why = if self.settings.mode == "on" { self.block(0.0, None) } else { String::new() };
        format!(
            "{}{}",
            mode_name(&self.settings.mode),
            if why.is_empty() { String::new() } else { format!("（现在不下单：{why}）") }
        )
    }

    // -- the journal --
    pub fn journal_path(&self) -> String {
        if self.store.path.is_empty() || self.store.path == ":memory:" {
            String::new()
        } else {
            format!("{}{JOURNAL_SUFFIX}", self.store.path)
        }
    }

    /// A real order's record in a line of the journal file, before its request leaves: what a restart needs to look for it by
    /// its hash should the process end before the store has it. False without such a file (the caller flushes the store).
    pub fn journal_write(&mut self, key: &str, order: &Obj) -> bool {
        use std::io::Write;
        let Some(file) = self.journal.as_mut() else { return false };
        let Some(rec) = self.recs.get(key) else { return false };
        let head: Obj = rec.iter().filter(|(k, _)| *k != "orders").map(|(k, v)| (k.clone(), v.clone())).collect();
        let mut line = if self.journal_cut { b"\n".to_vec() } else { vec![] };
        line.extend(serde_json::to_vec(&json!({"key": key, "rec": head, "order": order})).unwrap_or_default());
        line.push(b'\n');
        match file.write(&line) {
            Err(e) => {
                let text = format!("订单日志写入失败（{}），改为等数据库写完再发单", e.kind());
                self.note(&text);
                false
            }
            Ok(wrote) if wrote != line.len() => {
                self.journal_cut = true;
                if let Some(f) = self.journal.as_mut() {
                    if let Ok(1) = f.write(b"\n") {
                        self.journal_cut = false;
                    }
                }
                let text = format!("订单日志只写进 {wrote}/{} 字节（磁盘满了？），改为等数据库写完再发单", line.len());
                self.note(&text);
                false
            }
            Ok(_) => {
                self.journal_cut = false;
                true
            }
        }
    }

    /// At the start: the journal's orders the records do not hold onto their records as orders whose answer is unknown (looked
    /// for by their hash, never sent again); then, the store written, the journal begun anew.
    pub fn recover(&mut self) {
        let path = self.journal_path();
        if path.is_empty() {
            return;
        }
        if let Ok(text) = std::fs::read(&path) {
            let text = String::from_utf8_lossy(&text).to_string();
            for line in text.lines() {
                let Ok(entry) = serde_json::from_str::<Value>(line) else { continue };
                let key = entry.s("key");
                let Some(order) = entry.get("order").and_then(Value::as_object).cloned() else { continue };
                let kept = self.recs.get(&key).cloned();
                let mut rec = kept.clone().unwrap_or_else(|| {
                    let mut r = entry.get("rec").and_then(Value::as_object).cloned().unwrap_or_default();
                    r.insert("orders".into(), json!([]));
                    r
                });
                let old = kept.is_none() && rec.i("start_ms") < self.now() - KEEP_DAYS * DAY_MS;
                let hash = order.s("hash");
                if key.is_empty() || hash.is_empty() || !rec.t("slug") || rec_orders(&rec).any(|o| o.s("hash") == hash) || old {
                    continue;
                }
                let mut order = order;
                order.insert("state".into(), json!("unknown"));
                order.insert("error".into(), json!("重启前发出的单，记录里还没有：按哈希核对"));
                order.insert("next_ms".into(), json!(0));
                if let Some(Value::Array(list)) = rec.get_mut("orders") {
                    list.push(Value::Object(order));
                } else {
                    rec.insert("orders".into(), json!([Value::Object(order)]));
                }
                self.store.put(&format!("{PREFIX}{key}"), &Value::Object(rec.clone()));
                self.recs.insert(key, rec);
                crate::warn!("规则吃单：订单日志里有一张记录里没有的单（{}），按哈希核对", &hash[..hash.len().min(12)]);
            }
        }
        let written = self.store.flush(10.0);
        use std::os::unix::fs::OpenOptionsExt;
        let mut options = std::fs::OpenOptions::new();
        options.append(true).create(true).mode(0o600);
        if written {
            // (append and truncate cannot be asked together: the file is emptied first)
            let _ = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&path);
        }
        match options.open(&path) {
            Ok(f) => self.journal = Some(f),
            Err(e) => crate::warn!("规则吃单：订单日志打不开（{}），发单前改为等数据库写完", e.kind()),
        }
    }

    /// An order of a record (by its hash), to change.
    pub fn order_mut(&mut self, key: &str, hash: &str) -> Option<&mut Obj> {
        self.recs
            .get_mut(key)?
            .get_mut("orders")?
            .as_array_mut()?
            .iter_mut()
            .filter_map(Value::as_object_mut)
            .find(|o| o.sv("hash") == hash)
    }

    pub fn order_of(&self, key: &str, hash: &str) -> Option<&Obj> {
        rec_orders(self.recs.get(key)?).find(|o| o.sv("hash") == hash)
    }
}

/// The taker made, its hooks put on the stream, the index and the API (a weak hold: no cycle).
pub fn install(taker: Taker) -> Me {
    let me: Me = Rc::new(RefCell::new(taker));
    let weak: Weak<RefCell<Taker>> = Rc::downgrade(&me);
    let (stream, index, api) = {
        let t = me.borrow();
        (t.stream.clone(), t.index.clone(), t.api.clone())
    };
    let w = weak.clone();
    *stream.on_change.borrow_mut() =
        Some(Rc::new(move |mid: &str| w.upgrade().map(|me| flow::on_change(&me, mid)).unwrap_or(false)));
    if let Some(index) = index {
        let w = weak.clone();
        *index.on_change.borrow_mut() = Some(Rc::new(move |symbol: &str, at: i64| {
            if let Some(me) = w.upgrade() {
                flow::on_index(&me, symbol, at);
            }
        }));
        let w = weak.clone();
        *index.needed.borrow_mut() =
            Some(Rc::new(move || w.upgrade().and_then(|me| me.try_borrow().ok().map(|t| t.index_needed())).unwrap_or(false)));
        let w = weak.clone();
        *index.wanted.borrow_mut() =
            Some(Rc::new(move || w.upgrade().and_then(|me| me.try_borrow().ok().map(|t| t.index_wanted())).unwrap_or(false)));
    }
    let w = weak;
    *api.before_send.borrow_mut() = Some(Rc::new(move |digest: &str| match w.upgrade() {
        Some(me) => match me.try_borrow() {
            Ok(t) => t.before_send(digest),
            Err(_) => "内部状态正忙，这张单不发了（下一轮重新判断）".to_string(),
        },
        None => "服务正在停止，不再发单".to_string(),
    }));
    me
}

/// A record's fresh order list entry as JSON.
pub fn push_order(rec: &mut Obj, order: Obj) {
    match rec.get_mut("orders") {
        Some(Value::Array(list)) => list.push(Value::Object(order)),
        _ => {
            rec.insert("orders".into(), json!([Value::Object(order)]));
        }
    }
}

pub fn clean(error: &crate::util::RemoteError) -> String {
    let text = clean_error(error);
    if text.is_empty() { "RemoteError".into() } else { text }
}

pub fn mono_now() -> f64 {
    mono()
}

pub fn opt_f(x: Option<f64>) -> Value {
    x.map(jf).unwrap_or(Value::Null)
}

pub fn final_states() -> &'static [&'static str] {
    &FINAL_STATES
}

pub fn never_placed_states() -> &'static [&'static str] {
    &NEVER_PLACED
}
