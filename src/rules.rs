//! The rules, their windows, the book's arithmetic and the records' sums (the parts of huice rules.py 1.154.27 that are not the
//! taker itself).
//!
//! A few fixed windows of the gold and oil 15-minute cards have an outcome that is not the coin flip the book prices — gold's
//! index drifts while CME is shut (an end equal to the start settles Up), the first window after the daily reopen and gold's
//! 05:15 New York window lean Down — and are bought on the rule's side whenever, before the window starts, the book offers that
//! side at or under the rule's price. 盘中: gold's shut quarter hours and the reopen's first window are bought in the window's
//! course too, while the index stands ahead of the start on the rule's side by Rule.lead.
use crate::cards::{WindowCard, card};
use crate::clock::{et_parts, market_shuts};
use crate::config::RuleSettings;
use crate::util::{DAY_MS, DUST, Obj, Py, bj, bj_clock, day_of, g, jf, jopt_f, jopt_i, median, round_to};
use indexmap::IndexMap;
use rust_decimal::Decimal;
use serde_json::{Value, json};
use std::str::FromStr;

pub type Recs = IndexMap<String, Obj>;
pub type Level = (Decimal, Decimal);

pub const PREFIX: &str = "rule:";
pub const FINAL_STATES: [&str; 4] = ["final", "rejected", "not_sent", "not_found"];
pub const NEVER_PLACED: [&str; 3] = ["rejected", "not_sent", "not_found"];
pub const OPEN_STATES: [&str; 3] = ["placing", "open", "unknown"];
pub const RACES_SHOWN: usize = 8;

pub fn is_final(state: &str) -> bool {
    FINAL_STATES.contains(&state)
}
pub fn never_placed(state: &str) -> bool {
    NEVER_PLACED.contains(&state)
}
pub fn is_open_state(state: &str) -> bool {
    OPEN_STATES.contains(&state)
}

pub fn mode_name(mode: &str) -> &'static str {
    match mode {
        "off" => "关",
        "paper" => "只记录（不下单）",
        "on" => "真实吃单",
        _ => "关",
    }
}

pub fn market_name(market: &str) -> &'static str {
    if market == "oil" { "原油" } else { "黄金" }
}

// --- the windows --------------------------------------------------------------------------------------------------------
/// A quarter hour of CME's daily one-hour break: 17:00–17:45 New York, Monday to Thursday.
pub fn daily_break(start_ms: i64) -> bool {
    let (weekday, hour, minute) = et_parts(start_ms);
    weekday <= 3 && hour == 17 && minute % 15 == 0
}

/// The first quarter hour after CME's daily break: 18:00 New York, Monday to Thursday, the stretch shut before it the one-hour
/// break itself — not the weekend's nor a holiday's.
pub fn daily_reopen(start_ms: i64) -> bool {
    let (weekday, hour, minute) = et_parts(start_ms);
    if weekday > 3 || (hour, minute) != (18, 0) {
        return false;
    }
    let ending: Vec<(i64, i64)> =
        market_shuts("cme", start_ms - 60_000, start_ms).into_iter().filter(|s| s.1 == start_ms).collect();
    !ending.is_empty() && ending[0].1 - ending[0].0 <= 65 * 60_000
}

/// The quarter hour from hour:minute New York, Monday to Friday, CME open through it.
pub fn ny_slot(hour: u32, minute: u32, start_ms: i64) -> bool {
    let (weekday, h, mi) = et_parts(start_ms);
    weekday <= 4 && (h, mi) == (hour, minute) && market_shuts("cme", start_ms, start_ms + 15 * 60_000).is_empty()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Matcher {
    DailyBreak,
    DailyReopen,
    Slot(u32, u32),
}

impl Matcher {
    pub fn matches(self, start_ms: i64) -> bool {
        match self {
            Matcher::DailyBreak => daily_break(start_ms),
            Matcher::DailyReopen => daily_reopen(start_ms),
            Matcher::Slot(h, m) => ny_slot(h, m, start_ms),
        }
    }
}

#[derive(Clone, Debug)]
pub struct Rule {
    pub key: &'static str,
    pub market: &'static str,
    pub card: &'static str,
    pub side: &'static str,
    pub name: &'static str,
    pub when: &'static str,
    pub matcher: Matcher,
    /// 盘中: bought in the window's course while the index is ahead of the start on the rule's side by this much (relative);
    /// 0 — only before the window
    pub lead: &'static str,
}

impl Rule {
    pub fn word(&self) -> &'static str {
        if self.side == "up" { "涨" } else { "跌" }
    }
    pub fn spec(&self) -> &'static WindowCard {
        card(self.card)
    }
    pub fn matches(&self, start_ms: i64) -> bool {
        self.matcher.matches(start_ms)
    }
    pub fn lead(&self) -> Decimal {
        Decimal::from_str(self.lead).unwrap_or(Decimal::ZERO)
    }
    pub fn in_play(&self) -> bool {
        self.lead() > Decimal::ZERO
    }
}

pub static RULES: [Rule; 8] = [
    Rule {
        key: "gold_shut",
        market: "gold",
        card: "XAU-15M",
        side: "up",
        name: "黄金休市 4 档",
        when: "纽约 17:00–17:45，周一至周四（北京冬令时 06:00–06:45、夏令时 05:00–05:45）",
        matcher: Matcher::DailyBreak,
        lead: "0.00005",
    },
    Rule {
        key: "gold_open",
        market: "gold",
        card: "XAU-15M",
        side: "down",
        name: "黄金开盘第一档",
        when: "纽约 18:00，周一至周四（北京冬令时 07:00、夏令时 06:00；周一早上那场不做）",
        matcher: Matcher::DailyReopen,
        lead: "0.0002",
    },
    Rule {
        key: "gold_0515",
        market: "gold",
        card: "XAU-15M",
        side: "down",
        name: "黄金纽约 05:15",
        when: "纽约 05:15，周一至周五（北京冬令时 18:15、夏令时 17:15）",
        matcher: Matcher::Slot(5, 15),
        lead: "0",
    },
    Rule {
        key: "gold_0500",
        market: "gold",
        card: "XAU-15M",
        side: "up",
        name: "黄金纽约 05:00",
        when: "纽约 05:00，周一至周五（北京冬令时 18:00、夏令时 17:00）",
        matcher: Matcher::Slot(5, 0),
        lead: "0",
    },
    Rule {
        key: "gold_1645",
        market: "gold",
        card: "XAU-15M",
        side: "up",
        name: "黄金休市前一档",
        when: "纽约 16:45，周一至周五（北京冬令时 05:45、夏令时 04:45）",
        matcher: Matcher::Slot(16, 45),
        lead: "0",
    },
    Rule {
        key: "oil_shut",
        market: "oil",
        card: "CL-15M",
        side: "down",
        name: "原油休市 4 档",
        when: "纽约 17:00–17:45，周一至周四（北京冬令时 06:00–06:45、夏令时 05:00–05:45）",
        matcher: Matcher::DailyBreak,
        lead: "0",
    },
    Rule {
        key: "oil_0045",
        market: "oil",
        card: "CL-15M",
        side: "down",
        name: "原油纽约 00:45",
        when: "纽约 00:45，周一至周五（北京冬令时 13:45、夏令时 12:45）",
        matcher: Matcher::Slot(0, 45),
        lead: "0",
    },
    Rule {
        key: "oil_open",
        market: "oil",
        card: "CL-15M",
        side: "down",
        name: "原油开盘第一档",
        when: "纽约 18:00，周一至周四（北京冬令时 07:00、夏令时 06:00；周一早上那场不做）",
        matcher: Matcher::DailyReopen,
        lead: "0",
    },
];

pub fn rule_of(key: &str) -> Option<&'static Rule> {
    RULES.iter().find(|r| r.key == key)
}

/// The starts of the rule's windows in [from_ms, to_ms).
pub fn windows(rule: &Rule, from_ms: i64, to_ms: i64) -> Vec<i64> {
    let card = rule.spec();
    let mut out = vec![];
    let mut start = card.first_start(from_ms);
    while start < to_ms {
        if rule.matches(start) {
            out.push(start);
        }
        start = card.next_start(start);
    }
    out
}

/// Whether a rule buys before its windows by the settings: its market switched on and a price set.
pub fn live_rule(settings: &RuleSettings, rule: &Rule) -> bool {
    settings.on(rule.market) && settings.price(rule.key) > 0
}

/// The rule's price in its window's course (RULE_*_LIVE_CENTS), 0 = none.
pub fn live_price_of(settings: &RuleSettings, rule: &Rule) -> i64 {
    if !rule.in_play() || !settings.on(rule.market) {
        return 0;
    }
    settings.live_price(rule.key)
}

pub fn lead_note(rule: &Rule) -> String {
    let pct = rule.lead().to_string().parse::<f64>().unwrap_or(0.0) * 100.0;
    format!("指数比起点{} {}% 以上才买", if rule.side == "up" { "高" } else { "低" }, g(pct))
}

// --- the book -----------------------------------------------------------------------------------------------------------
/// What a buy of ``side`` at ``limit`` or under takes, the cheapest first: (price, shares). Predict's book is quoted for 涨 (Up):
/// its asks are the Up shares for sale; a bid for Up at p is Down for sale at 1 − p.
pub fn offered(bids: &[Level], asks: &[Level], side: &str, limit: Decimal) -> Vec<Level> {
    if side == "up" {
        let mut out: Vec<Level> = asks.iter().copied().filter(|(p, _)| *p <= limit).collect();
        out.sort_by(|a, b| a.cmp(b));
        out
    } else {
        let mut sorted: Vec<Level> = bids.to_vec();
        sorted.sort_by(|a, b| b.cmp(a));
        sorted.into_iter().map(|(p, q)| (Decimal::ONE - p, q)).filter(|(p, _)| *p <= limit).collect()
    }
}

/// ``levels`` (the cheapest first) less ``shares`` taken from the cheapest.
pub fn without(levels: &[Level], shares: Decimal) -> Vec<Level> {
    let mut out = vec![];
    let mut left = shares;
    for &(price, size) in levels {
        let took = size.min(left);
        left -= took;
        if size > took {
            out.push((price, size - took));
        }
    }
    out
}

/// The average price of ``shares`` bought from ``levels`` (cheapest first).
pub fn walk(levels: &[Level], shares: Decimal) -> Decimal {
    let mut left = shares;
    let mut total = Decimal::ZERO;
    for &(price, size) in levels {
        let took = size.min(left);
        total += price * took;
        left -= took;
        if left <= Decimal::ZERO {
            break;
        }
    }
    if shares > left { total / (shares - left) } else { Decimal::ZERO }
}

const POW10: [f64; 23] = [
    1e0, 1e1, 1e2, 1e3, 1e4, 1e5, 1e6, 1e7, 1e8, 1e9, 1e10, 1e11, 1e12, 1e13, 1e14, 1e15, 1e16, 1e17, 1e18, 1e19, 1e20, 1e21,
    1e22,
];

/// ``float(d)``: correctly rounded (an exact quotient of two exact doubles when the digits allow, else through the text).
pub fn dec_f64(d: Decimal) -> f64 {
    let m = d.mantissa();
    let scale = d.scale() as usize;
    if m.unsigned_abs() < (1u128 << 53) && scale < POW10.len() {
        return m as f64 / POW10[scale];
    }
    d.to_string().parse().unwrap_or(0.0)
}

/// A float as the Decimal ``D(str(x))`` makes of it.
pub fn f64_dec(x: f64) -> Decimal {
    Decimal::from_str(&format!("{x}")).or_else(|_| Decimal::from_scientific(&format!("{x:e}"))).unwrap_or(Decimal::ZERO)
}

// --- the records --------------------------------------------------------------------------------------------------------
/// A record's id (its store key after PREFIX): the window's slug for a real one, ``paper:<slug>`` for a paper one.
pub fn rid(slug: &str, mode: &str) -> String {
    if mode == "on" { slug.to_string() } else { format!("{mode}:{slug}") }
}

/// The part of a taker's shares Predict keeps as its fee: rate × min(p, 1 − p) a share, taken in shares, not USDT.
pub fn fee_share(avg: f64, bps: i64) -> f64 {
    if avg > 0.0 { bps as f64 / 10_000.0 * avg.min(1.0 - avg) / avg } else { 0.0 }
}

pub fn order_avg(o: &Obj) -> f64 {
    let avg = o.f("avg");
    if avg != 0.0 { avg } else { o.f("limit") }
}

pub fn order_bps(o: &Obj) -> i64 {
    if o.none("fee_bps") { 200 } else { o.get("fee_bps").map(crate::util::as_i64).unwrap_or(200) }
}

pub fn orders(rec: &Obj) -> impl Iterator<Item = &Obj> {
    rec.arr("orders").iter().filter_map(Value::as_object)
}

pub fn filled(rec: &Obj) -> f64 {
    orders(rec).map(|o| o.f("filled")).sum()
}

/// What the fills cost in USDT: each order's shares at its average price (its limit while Predict states none).
pub fn cost(rec: &Obj) -> f64 {
    orders(rec).map(|o| o.f("filled") * order_avg(o)).sum()
}

/// The shares the account holds of the fills: those bought less the fee Predict takes in shares.
pub fn net(rec: &Obj) -> f64 {
    orders(rec).map(|o| o.f("filled") * (1.0 - fee_share(order_avg(o), order_bps(o)))).sum()
}

pub fn avg_price(rec: &Obj) -> f64 {
    let shares = filled(rec);
    if shares > DUST { orders(rec).map(|o| o.f("filled") * order_avg(o)).sum::<f64>() / shares } else { 0.0 }
}

pub fn in_flight(rec: &Obj) -> bool {
    orders(rec).any(|o| !is_final(o.state()))
}

/// The window's shares bought or being bought: the fills, and the whole of an order whose word is not final.
pub fn held(rec: &Obj) -> f64 {
    orders(rec).map(|o| if !is_final(o.state()) { o.f("shares") } else { o.f("filled") }).sum()
}

/// The window's orders that may still take from the book: on their way, or on it and not withdrawn.
pub fn open_orders(rec: &Obj) -> Vec<&Obj> {
    orders(rec).filter(|o| is_open_state(o.state()) && !o.t("removed")).collect()
}

/// The shares those orders may still take: asked for and not known filled.
pub fn claims(rec: &Obj) -> f64 {
    open_orders(rec).iter().map(|o| (o.f("shares") - o.f("filled")).max(0.0)).sum()
}

/// What the orders placed on a Beijing day cost in USDT, the mode's own records: the fills at their price, and the whole of an
/// order whose word is not final at its limit (RULE_MAX_DAY_USD).
pub fn spent_on<'a>(recs: impl IntoIterator<Item = &'a Obj>, day: &str, mode: &str) -> f64 {
    let mut total = 0.0;
    for rec in recs {
        if rec.sv("mode") != mode {
            continue;
        }
        for o in orders(rec) {
            if day_of(o.i("at")) != day {
                continue;
            }
            if is_final(o.state()) {
                total += o.f("filled") * order_avg(o);
            } else {
                total += o.f("shares") * o.f("limit");
            }
        }
    }
    total
}

/// The real records' P&L settled on a Beijing day (RULE_MAX_DAILY_LOSS_USD).
pub fn day_pnl(recs: &Recs, day: &str) -> f64 {
    recs.values().filter(|r| r.s("mode") == "on" && r.s("settled_day") == day).map(|r| r.f("pnl")).sum()
}

/// The real records' money not yet settled: fills unsettled at their cost, orders not final at their limit.
pub fn open_cost(recs: &Recs) -> f64 {
    let mut total = 0.0;
    for rec in recs.values() {
        if rec.s("mode") != "on" || !rec.none("pnl") {
            continue;
        }
        for o in orders(rec) {
            total += if is_final(o.state()) { o.f("filled") * order_avg(o) } else { o.f("shares") * o.f("limit") };
        }
    }
    total
}

/// A window's watch: the lowest offer of the rule's side at any price, the most on offer at the rule's price at once, when that
/// first showed, when the window was listed and matching first seen open; Predict's own stamps of the offer's coming and going,
/// and of when the amount on offer first went down.
pub fn watch_default() -> Obj {
    let mut m = Obj::new();
    m.insert("best".into(), Value::Null);
    m.insert("offer_max".into(), json!(0.0));
    for k in ["offer_ms", "listed_ms", "open_ms", "offer_ts", "gone_ms", "gone_ts", "cut_ms", "cut_ts"] {
        m.insert(k.into(), json!(0));
    }
    m
}

fn mid(xs: Vec<f64>) -> Value {
    let mut xs = xs;
    jopt_f(median(&mut xs))
}

/// What the watches of a rule's windows saw since ``since_ms``.
pub fn watch_stats(recs: &Recs, rule: &str, mode: &str, since_ms: i64) -> Obj {
    let seen: Vec<&Obj> = recs
        .values()
        .filter(|r| r.s("rule") == rule && r.s("mode") == mode)
        .filter_map(|r| r.obj("seen"))
        .filter(|x| x.i("start_ms") >= since_ms && !x.t("open"))
        .collect();
    let offers: Vec<&Obj> = seen.iter().copied().filter(|x| x.f("offer_max") > 0.0).collect();
    let lost: Vec<&Obj> = offers.iter().copied().filter(|x| x.t("gone_ms") && x.f("bought") <= DUST).collect();
    let won: Vec<&Obj> = offers.iter().copied().filter(|x| x.t("gone_ts") && x.t("offer_ts") && x.f("bought") > DUST).collect();
    let stood = |x: &Obj| -> i64 {
        if x.t("gone_ts") && x.t("offer_ts") { x.i("gone_ts") - x.i("offer_ts") } else { x.i("gone_ms") - x.i("offer_ms") }
    };
    let offer_avg = if offers.is_empty() {
        Value::Null
    } else {
        jf(round_to(offers.iter().map(|x| x.f("offer_max")).sum::<f64>() / offers.len() as f64, 2))
    };
    let mut out = Obj::new();
    out.insert("watched".into(), json!(seen.len()));
    out.insert("offered".into(), json!(offers.len()));
    out.insert("offer_avg".into(), offer_avg);
    out.insert("best_mid".into(), mid(seen.iter().filter(|x| !x.none("best")).map(|x| x.f("best")).collect()));
    out.insert(
        "open_lead".into(),
        mid(seen.iter().filter(|x| x.t("open_ms")).map(|x| (x.i("start_ms") - x.i("open_ms")) as f64 / 1000.0).collect()),
    );
    out.insert(
        "offer_lead".into(),
        mid(offers.iter().filter(|x| x.t("offer_ms")).map(|x| (x.i("start_ms") - x.i("offer_ms")) as f64 / 60_000.0).collect()),
    );
    out.insert("lost".into(), json!(lost.len()));
    out.insert("life".into(), mid_i(lost.iter().map(|x| stood(x).max(0)).collect()));
    out.insert("won".into(), json!(won.len()));
    out.insert("won_life".into(), mid_i(won.iter().map(|x| stood(x).max(0)).collect()));
    out
}

fn opt_i(v: Option<i64>) -> Value {
    jopt_i(v)
}

/// An order sent on a book Predict stamped within this of the offer's first showing answered that showing.
pub const FIRST_BOOK_MS: i64 = 1_000;

/// The windows' races since ``since_ms``, the latest start first.
pub fn races(recs: &Recs, mode: &str, since_ms: i64) -> Vec<Obj> {
    let mut rows: Vec<Obj> = vec![];
    for r in recs.values() {
        if r.s("mode") != mode || r.i("start_ms") < since_ms {
            continue;
        }
        let empty = Obj::new();
        let seen = r.obj("seen").unwrap_or(&empty);
        let ords: Vec<&Obj> = orders(r).filter(|o| !o.t("live")).collect();
        if !seen.t("offer_ms") && ords.is_empty() {
            continue;
        }
        let shown = seen.i("offer_ts");
        let gap = |kind: &str| -> Option<i64> {
            let ms = format!("{kind}_ms");
            let ts = format!("{kind}_ts");
            if !seen.t(&ms) {
                return None;
            }
            if seen.t(&ts) && shown != 0 {
                return Some((seen.i(&ts) - shown).max(0));
            }
            Some((seen.i(&ms) - seen.i("offer_ms")).max(0))
        };
        let rule = rule_of(&r.s("rule"));
        let start_ms = r.i("start_ms");
        let cents = if seen.t("cents") { seen.i("cents") } else { r.i("cents") };
        let mut row = Obj::new();
        row.insert("slug".into(), r.get("slug").cloned().unwrap_or(Value::Null));
        row.insert("rule".into(), r.get("rule").cloned().unwrap_or(Value::Null));
        row.insert("name".into(), json!(rule.map(|x| x.name.to_string()).unwrap_or_else(|| r.s("rule"))));
        row.insert("window".into(), r.get("name").cloned().unwrap_or(Value::Null));
        row.insert("start_ms".into(), json!(start_ms));
        row.insert("cents".into(), json!(cents));
        row.insert("offer_at".into(), json!(if shown != 0 { shown } else { seen.i("offer_ms") }));
        row.insert("stamped".into(), json!(shown != 0));
        row.insert(
            "lead_min".into(),
            if seen.t("offer_ms") { jf(round_to((start_ms - seen.i("offer_ms")) as f64 / 60_000.0, 1)) } else { Value::Null },
        );
        row.insert("peak".into(), jf(seen.f("offer_max")));
        row.insert("cut_ms".into(), opt_i(gap("cut")));
        row.insert("gone_ms".into(), opt_i(gap("gone")));
        row.insert("open".into(), json!(seen.t("open")));
        row.insert(
            "asked".into(),
            jf(round_to(ords.iter().filter(|o| !never_placed(o.state())).map(|o| o.f("shares")).sum(), 4)),
        );
        row.insert("bought".into(), jf(round_to(ords.iter().map(|o| o.f("filled")).sum(), 4)));
        let mut out_orders = vec![];
        for o in &ords {
            let mut x = Obj::new();
            x.insert("shares".into(), jf(o.f("shares")));
            x.insert("filled".into(), jf(o.f("filled")));
            x.insert("paper".into(), json!(o.t("paper")));
            x.insert("seen".into(), o.get("seen").cloned().unwrap_or(Value::Null));
            x.insert("fresh".into(), o.get("fresh").cloned().unwrap_or(Value::Null));
            x.insert("state".into(), o.get("state").cloned().unwrap_or(Value::Null));
            x.insert("error".into(), json!(if never_placed(o.state()) { o.s("error") } else { String::new() }));
            let both = |a: &str, b: &str| o.t(a) && o.t(b);
            x.insert("after".into(), opt_i(if o.t("sent_wall") && shown != 0 { Some(o.i("sent_wall") - shown) } else { None }));
            // (sent on a book from the offer's first showing: only such an order measures how quick we were)
            x.insert("first".into(), json!(shown != 0 && (0..=FIRST_BOOK_MS).contains(&(o.i("book_ts") - shown))));
            x.insert(
                "lag".into(),
                opt_i(if both("book_ts", "seen_wall") { Some(o.i("seen_wall") - o.i("book_ts")) } else { None }),
            );
            x.insert(
                "own".into(),
                opt_i(if both("sent_wall", "seen_wall") { Some(o.i("sent_wall") - o.i("seen_wall")) } else { None }),
            );
            x.insert(
                "wake".into(),
                opt_i(if both("look_wall", "seen_wall") && o.i("look_wall") >= o.i("seen_wall") {
                    Some(o.i("look_wall") - o.i("seen_wall"))
                } else {
                    None
                }),
            );
            x.insert(
                "prep".into(),
                opt_i(if both("signed_wall", "look_wall") {
                    Some(o.i("signed_wall") - o.i("look_wall").max(o.i("seen_wall")))
                } else {
                    None
                }),
            );
            x.insert(
                "out".into(),
                opt_i(if both("sent_wall", "signed_wall") { Some(o.i("sent_wall") - o.i("signed_wall")) } else { None }),
            );
            x.insert("presigned".into(), json!(o.t("presigned")));
            x.insert("open_wait".into(), o.get("open_wait").cloned().unwrap_or(Value::Null));
            x.insert("open_reads".into(), o.get("open_reads").cloned().unwrap_or(Value::Null));
            x.insert(
                "trip".into(),
                opt_i(if both("answered_wall", "sent_wall") { Some(o.i("answered_wall") - o.i("sent_wall")) } else { None }),
            );
            x.insert("closed".into(), closed_by_predict(o));
            x.insert("queued".into(), o.get("queued_ms").cloned().unwrap_or(Value::Null));
            x.insert("connect".into(), o.get("connect_ms").cloned().unwrap_or(Value::Null));
            x.insert(
                "reply".into(),
                opt_i(if both("answered_wall", "wrote_wall") { Some(o.i("answered_wall") - o.i("wrote_wall")) } else { None }),
            );
            out_orders.push(Value::Object(x));
        }
        row.insert("orders".into(), Value::Array(out_orders));
        rows.push(row);
    }
    rows.sort_by_key(|x| -x.i("start_ms"));
    rows
}

fn sec(ms: i64) -> String {
    format!("{:.2} 秒", ms as f64 / 1000.0)
}

/// A stretch of time in the unit that reads best: 秒, 分钟, 小时.
pub fn ago(ms: i64) -> String {
    if ms < 60_000 {
        sec(ms)
    } else if ms < 3_600_000 {
        format!("{} 分钟", g(round_to(ms as f64 / 60_000.0, 1)))
    } else {
        format!("{} 小时", g(round_to(ms as f64 / 3_600_000.0, 1)))
    }
}

/// Predict closed this order itself, short of its shares, and never withdrawn by us (an unfilled rest is withdrawn only at a
/// follow, from FILL_READ_MS after the answer): the status it gave then — "?" for a record from before it was kept; Null
/// otherwise. The usual cause: the other side was this account's own resting order, and self-trade prevention cancels the
/// taker.
pub fn closed_by_predict(o: &Obj) -> Value {
    let done = matches!(o.state(), "final" | "closing");
    if o.t("paper") || !done || o.t("removed") || !o.t("id") || o.t("error") || o.f("filled") >= o.f("shares") - DUST {
        return Value::Null;
    }
    json!(if o.t("closed_as") { o.s("closed_as") } else { "?".to_string() })
}

pub fn race_text(row: &Obj) -> String {
    let mut text = format!("{} {}：", bj(row.i("start_ms"), true), row.s("name"));
    let open = row.t("open");
    if row.t("offer_at") {
        text += &format!("≤{}¢ 的卖单 {} 出现", row.i("cents"), bj_clock(row.i("offer_at")));
        if let Some(lead) = row.of("lead_min") {
            text += &format!("（开场前 {} 分钟）", g(lead));
        }
        text += &format!("，最多 {} 份；", g(row.f("peak")));
        let cut = row.get("cut_ms").and_then(Value::as_i64);
        let gone = row.get("gone_ms").and_then(Value::as_i64);
        let mut parts: Vec<String> = vec![];
        if let Some(c) = cut {
            parts.push(format!("{}后开始被吃", sec(c)));
        }
        if let Some(gm) = gone {
            parts.push(format!("{}后被吃光", sec(gm)));
        } else if cut.is_some() {
            parts.push(if open { "还没吃光".into() } else { "到开场前没吃光".into() });
        }
        let joined = parts.join("、");
        text += &if joined.is_empty() {
            (if open { "还没有被吃" } else { "到开场前没人吃" }).to_string()
        } else {
            joined
        };
    } else {
        text += "卖单出现的时刻没有记到（重启前的，或没看到规则价以内的卖单）";
    }
    let ords: Vec<&Obj> = row.arr("orders").iter().filter_map(Value::as_object).collect();
    if ords.is_empty() {
        return text + "；没下单" + if open { "（这一场还在看）" } else { "" };
    }
    let (bought, asked) = (row.f("bought"), row.f("asked"));
    text += "；";
    if ords.iter().all(|o| o.t("paper")) {
        text += "只记录 ";
    }
    text += &if bought > DUST {
        format!("买到 {}/{} 份", g(bought), g(asked))
    } else if asked > DUST {
        format!("下了 {} 份，没买到", g(asked))
    } else {
        "单没下成".to_string()
    };
    for (i, o) in ords.iter().enumerate() {
        let i = i + 1;
        if o.t("paper") {
            continue;
        }
        if never_placed(o.state()) {
            text += &format!("｜第 {i} 张 要 {}：没下成", g(o.f("shares")));
            if o.t("error") {
                text += &format!("（{}）", o.s("error"));
            }
            continue;
        }
        let num = |k: &str| o.get(k).and_then(Value::as_i64);
        let mut parts: Vec<String> = vec![];
        if let Some(a) = num("after") {
            parts.push(if o.t("first") {
                format!("出现后 {a} ms 发出")
            } else {
                format!("首次出现 {}后才发出（跟的是后来的盘口）", ago(a))
            });
        }
        if let Some(l) = num("lag") {
            parts.push(format!("推送 {l} ms"));
        }
        if let Some(own) = num("own") {
            let mut split: Vec<String> = [("到达→开始处理", "wake"), ("处理→签好", "prep"), ("签好→发出", "out")]
                .iter()
                .filter_map(|(name, key)| num(key).map(|v| format!("{name} {v} ms")))
                .collect();
            if o.t("presigned") {
                split.push("预先签好".into());
            }
            if !o.none("open_wait") {
                split.push(format!(
                    "卖单先到、等撮合开启 {} ms（读了 {} 次状态）",
                    o.get("open_wait").map(crate::util::py_str).unwrap_or_default(),
                    o.i("open_reads")
                ));
            }
            let mut piece = format!("到达→发出 {own} ms");
            if !split.is_empty() {
                piece += &format!("［{}］", split.join("、"));
            }
            if o.is_false("fresh") {
                piece += "（不是盘口一到就下的）";
            }
            parts.push(piece);
        }
        if let Some(t) = num("trip") {
            let mut piece = format!("往返 {t} ms");
            if let Some(r) = num("reply") {
                // (where it went: waiting for the key's budget and a slot, a connection, Predict's own answer)
                let link = if o.t("connect") {
                    format!("新建连接 {} ms", g(o.f("connect").round()))
                } else {
                    "复用连接".to_string()
                };
                piece += &format!("［排队 {} ms、{link}、写出→应答 {r} ms］", g(o.f("queued").round()));
            }
            parts.push(piece);
        }
        if let Some(status) = o.get("closed").and_then(Value::as_str) {
            parts.push(format!(
                "Predict 当场就撤了这张单{}，不是我们撤的——多半是对面正是这个账户自己的挂单（比如 huice 的预挂），\
                 自成交保护撤掉吃单的一方",
                if status == "?" { String::new() } else { format!("（它记的状态 {status}）") }
            ));
        }
        text += &format!("｜第 {i} 张 要 {} 成交 {}", g(o.f("shares")), g(o.f("filled")));
        if o.t("seen") {
            text += &format!("（当时盘口上 {} 份）", g(o.f("seen")));
        }
        if !parts.is_empty() {
            text += &format!("：{}", parts.join("、"));
        }
    }
    text
}

pub fn race_summary(rows: &[Obj]) -> String {
    let mut marks: Vec<f64> =
        rows.iter().filter(|r| r.t("stamped")).map(|r| (r.i("offer_at") % 900_000) as f64 / 1000.0).collect();
    let mut cuts: Vec<f64> = rows
        .iter()
        .filter(|r| r.t("stamped"))
        .filter_map(|r| r.get("cut_ms").and_then(Value::as_i64))
        .map(|c| c as f64 / 1000.0)
        .collect();
    let mut ours: Vec<f64> = rows
        .iter()
        .flat_map(|r| r.arr("orders").iter().filter_map(Value::as_object))
        // (only an order sent on a book from the offer's first showing is set against the fastest hand: one that went out on a
        // later book — the offer back again, matching opened hours on — says nothing of how quick we were)
        .filter(|o| o.t("first") && !o.is_false("fresh"))
        .filter_map(|o| o.get("after").and_then(Value::as_i64))
        .map(|a| a as f64)
        .collect();
    let mut parts = vec![];
    let lo = |xs: &[f64]| xs.iter().copied().fold(f64::INFINITY, f64::min);
    let hi = |xs: &[f64]| xs.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    if !marks.is_empty() {
        let (a, b) = (lo(&marks), hi(&marks));
        let n = marks.len();
        parts.push(format!("卖单出现在整刻钟后 中位 {:.2} 秒（{a:.2}～{b:.2}，{n} 场）", median(&mut marks).unwrap_or(0.0)));
    }
    if !cuts.is_empty() {
        let a = lo(&cuts);
        parts.push(format!("最快的人在出现后 中位 {:.2} 秒下手（最快 {a:.2}）", median(&mut cuts).unwrap_or(0.0)));
    }
    if !ours.is_empty() {
        let a = lo(&ours);
        parts.push(format!(
            "我们的单在出现后 中位 {:.0} ms 发出（最快 {a:.0}），再加半个往返才到",
            median(&mut ours).unwrap_or(0.0)
        ));
    }
    parts.join("；")
}

/// A record's fills bought in the window's course (盘中): (shares, what they cost, their P&L — None until settled).
pub fn course_part(rec: &Obj) -> (f64, f64, Option<f64>) {
    let ords: Vec<&Obj> = orders(rec).filter(|o| o.t("live")).collect();
    let shares: f64 = ords.iter().map(|o| o.f("filled")).sum();
    let spent: f64 = ords.iter().map(|o| o.f("filled") * order_avg(o)).sum();
    let up = match rec.s("result").as_str() {
        "up" => Some(1.0),
        "down" => Some(0.0),
        "split" => Some(0.5),
        _ => None,
    };
    let (Some(up), false) = (up, rec.none("pnl")) else { return (shares, spent, None) };
    let kept: f64 = ords.iter().map(|o| o.f("filled") * (1.0 - fee_share(order_avg(o), order_bps(o)))).sum();
    (shares, spent, Some(kept * if rec.s("side") == "up" { up } else { 1.0 - up } - spent))
}

pub fn course_stats(recs: &Recs, rule: &str, mode: &str, since_ms: i64) -> Obj {
    let rows: Vec<(f64, f64, Option<f64>)> = recs
        .values()
        .filter(|r| r.s("rule") == rule && r.s("mode") == mode && r.i("start_ms") >= since_ms)
        .map(course_part)
        .filter(|x| x.0 > DUST)
        .collect();
    let done: Vec<f64> = rows.iter().filter_map(|x| x.2).collect();
    let mut out = Obj::new();
    out.insert("windows".into(), json!(rows.len()));
    out.insert("shares".into(), jf(round_to(rows.iter().map(|x| x.0).sum(), 4)));
    out.insert("cost".into(), jf(round_to(rows.iter().map(|x| x.1).sum(), 4)));
    out.insert("settled".into(), json!(done.len()));
    out.insert("wins".into(), json!(done.iter().filter(|x| **x > DUST).count()));
    out.insert("losses".into(), json!(done.iter().filter(|x| **x < -DUST).count()));
    out.insert("pnl".into(), jf(round_to(done.iter().sum(), 4)));
    out
}

fn mid_i(xs: Vec<i64>) -> Value {
    let mut xs = xs;
    crate::util::median_json(&mut xs)
}

/// The real orders against the clock since ``since_ms``, None for none.
pub fn speed_stats(recs: &Recs, since_ms: i64) -> Option<Obj> {
    let rows: Vec<&Obj> = recs
        .values()
        .filter(|r| r.s("mode") == "on" && r.i("start_ms") >= since_ms)
        .flat_map(orders)
        .filter(|o| o.t("sent_wall"))
        .collect();
    if rows.is_empty() {
        return None;
    }
    let marked = rows.iter().any(|o| o.has("fresh"));
    let quick: Vec<&Obj> = if marked { rows.iter().copied().filter(|o| o.t("fresh")).collect() } else { rows.clone() };
    let lag: Vec<i64> =
        rows.iter().filter(|o| o.t("book_ts") && o.t("seen_wall")).map(|o| o.i("seen_wall") - o.i("book_ts")).collect();
    let own: Vec<i64> = quick.iter().filter(|o| o.t("seen_wall")).map(|o| o.i("sent_wall") - o.i("seen_wall")).collect();
    let wake: Vec<i64> =
        quick.iter().filter(|o| o.t("seen_wall") && o.t("look_wall")).map(|o| o.i("look_wall") - o.i("seen_wall")).collect();
    let trip: Vec<i64> = rows.iter().filter(|o| o.t("answered_wall")).map(|o| o.i("answered_wall") - o.i("sent_wall")).collect();
    let best = json!({
        "lag": lag.iter().min().copied(), "own": own.iter().min().copied(), "trip": trip.iter().min().copied(),
    });
    let mut out = Obj::new();
    out.insert("orders".into(), json!(rows.len()));
    out.insert("filled".into(), json!(rows.iter().filter(|o| o.f("filled") > DUST).count()));
    out.insert("lag".into(), mid_i(lag));
    let timed = own.len();
    out.insert("own".into(), mid_i(own));
    out.insert("trip".into(), mid_i(trip));
    out.insert("wake".into(), mid_i(wake));
    out.insert("timed".into(), json!(timed));
    out.insert("best".into(), best);
    Some(out)
}

pub fn course_speed(recs: &Recs, since_ms: i64) -> Option<Obj> {
    let took: Vec<i64> = recs
        .values()
        .filter(|r| r.s("mode") == "on" && r.i("start_ms") >= since_ms)
        .flat_map(orders)
        .filter(|o| o.t("live") && o.t("index_wall") && o.t("sent_wall"))
        .map(|o| o.i("sent_wall") - o.i("index_wall"))
        .collect();
    if took.is_empty() {
        return None;
    }
    let best = *took.iter().min().unwrap_or(&0);
    let n = took.len();
    Some(crate::obj! {"orders" => n, "own" => mid_i(took), "best" => best})
}

pub fn since_days(now_ms: i64, days: i64) -> i64 {
    now_ms - days * DAY_MS
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::et_wall_ms;
    use crate::util::Date;

    fn d(s: &str) -> Decimal {
        Decimal::from_str(s).unwrap()
    }

    #[test]
    fn book() {
        let bids = vec![(d("0.42"), d("100")), (d("0.40"), d("50"))];
        let asks = vec![(d("0.60"), d("10")), (d("0.58"), d("20"))];
        let down = offered(&bids, &asks, "down", d("0.58"));
        assert_eq!(down, vec![(d("0.58"), d("100"))]); // 1 − 0.42 exactly at the limit
        let up = offered(&bids, &asks, "up", d("0.60"));
        assert_eq!(up, vec![(d("0.58"), d("20")), (d("0.60"), d("10"))]);
        assert_eq!(without(&up, d("25")), vec![(d("0.60"), d("5"))]);
        assert_eq!(walk(&up, d("30")).round_dp(6), d("0.586667"));
    }

    #[test]
    fn rule_windows() {
        let gold_shut = rule_of("gold_shut").unwrap();
        // Wednesday 2026-11-18: 17:00, 17:15, 17:30, 17:45 New York
        let day = Date::ymd(2026, 11, 18);
        let got = windows(gold_shut, et_wall_ms(day, 0, 0), et_wall_ms(day.add(1), 0, 0));
        assert_eq!(got, vec![et_wall_ms(day, 17, 0), et_wall_ms(day, 17, 15), et_wall_ms(day, 17, 30), et_wall_ms(day, 17, 45)]);
        let open = rule_of("gold_open").unwrap();
        assert_eq!(windows(open, et_wall_ms(day, 0, 0), et_wall_ms(day.add(1), 0, 0)), vec![et_wall_ms(day, 18, 0)]);
        // Thanksgiving 2026-11-26 is an early halt: no reopen window that evening (the stretch shut is not the hour's break)
        let thanks = Date::ymd(2026, 11, 26);
        assert!(windows(open, et_wall_ms(thanks, 0, 0), et_wall_ms(thanks.add(1), 0, 0)).is_empty());
        assert!(fee_share(0.58, 200) > 0.0);
    }
}
