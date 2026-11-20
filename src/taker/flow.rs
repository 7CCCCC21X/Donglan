//! The loop: readiness, the turns, the pace, the warm-up, the REST reads beside the pushes, the plan of the windows, the
//! in-play guard and the reads of the course's start lines.
use super::*;
use crate::predict::{market_start_price, parse_market};
use crate::rules::{RULES, filled, held, in_flight, live_price_of, live_rule, windows};
use crate::sync::{running, sleep, spawn, start_now};
use crate::util::{Res, bj, fmt_amount, g};
use crate::wallet::exchange_key;
use std::str::FromStr;

fn status_open(market: &Obj) -> bool {
    market.s("tradingStatus").to_uppercase() == "OPEN"
}

fn with_slug(market: Obj, slug: &str) -> Obj {
    let mut m = market;
    m.insert("categorySlug".into(), json!(slug));
    m
}

impl Taker {
    pub fn live_cents(&self, rule: &Rule) -> i64 {
        live_price_of(&self.settings, rule)
    }

    pub fn moment(&self, w: &Win) -> i64 {
        let lead = self.settings.lead_minutes * 60_000;
        if lead > 0 { w.start - lead } else { 0 }
    }

    pub fn sprinting(&self, w: &Win, now: i64) -> bool {
        let at = self.moment(w);
        at != 0 && -WARM_MS <= now - at && now - at < SPRINT_MS
    }

    pub fn hot(&self, w: &Win, now: i64) -> bool {
        let at = self.moment(w);
        at != 0 && -HOT_BEFORE_MS <= now - at && now - at < HOT_AFTER_MS
    }

    /// "before" — ahead of the start by more than STOP_MS; "live" — in its course, for a rule that buys there; "" — neither.
    pub fn phase(&self, w: &Win, now: i64) -> &'static str {
        if w.start - now > STOP_MS {
            return "before";
        }
        if self.live_cents(w.rule()) > 0 && now - w.start >= LIVE_START_MS && w.end() - now > LIVE_STOP_MS {
            return "live";
        }
        ""
    }

    pub fn over_at(&self, rule: &Rule, start: i64, now: i64) -> bool {
        if start - now > STOP_MS {
            return false;
        }
        !(self.live_cents(rule) > 0 && rule.spec().end_of(start) - now > LIVE_STOP_MS)
    }

    pub fn over(&self, w: &Win, now: i64) -> bool {
        self.over_at(w.rule(), w.start, now)
    }

    pub fn rushing(w: &Win) -> bool {
        running(&w.rush_task)
    }

    pub fn rush_pace(&self) -> f64 {
        let per_min = self.api.pacer.borrow().per_min;
        let pace = if per_min > 0.0 { STATUS_RUSH_S.max(66.0 / per_min) } else { STATUS_RUSH_S };
        pace * self.track.values().filter(|w| Self::rushing(&w.borrow())).count().max(1) as f64
    }

    /// Whether any rule buys in its windows' course by the settings in force: the index feed keeps its connection only then.
    pub fn index_wanted(&self) -> bool {
        self.settings.mode != "off" && RULES.iter().any(|r| self.live_cents(r) > 0)
    }

    /// Whether a window in (or about to begin) its course needs the index now: the REST read's pace.
    pub fn index_needed(&self) -> bool {
        if self.settings.mode == "off" {
            return false;
        }
        let now = self.now();
        self.track.values().any(|w| {
            let w = w.borrow();
            self.live_cents(w.rule()) > 0 && w.start - WARM_MS <= now && now < w.end()
        })
    }

    /// The ms left of the race's seconds: around the makers' moment of a window this taker can still buy before its start, the
    /// first seconds of a window's course for a rule that buys there, while an offer in hand waits for matching to open.
    pub fn quiet_ms(&self, now: i64) -> i64 {
        if self.settings.mode != "on" {
            return 0;
        }
        let mut left = 0;
        for (slug, w) in &self.track {
            let w = w.borrow();
            let rule = w.rule();
            let at = self.moment(&w);
            let begins = w.start + LIVE_START_MS;
            let (until, course) = if at != 0
                && at - QUIET_BEFORE_MS <= now
                && now < at + QUIET_AFTER_MS
                && now < w.start - STOP_MS
                && self.settings.price(rule.key) > 0
            {
                (at + QUIET_AFTER_MS, false)
            } else if begins - QUIET_BEFORE_MS <= now
                && now < begins + COURSE_QUIET_MS
                && self.live_cents(rule) > 0
                && self.settings.price(rule.key) > 0
            {
                (begins + COURSE_QUIET_MS, true)
            } else if Self::rushing(&w) {
                (now + RUSH_QUIET_MS, false)
            } else {
                continue;
            };
            if self.finished(slug, rule, course).is_empty() {
                left = left.max(until - now);
            }
        }
        left
    }

    /// How long until the taker needs speed, ms: 0 — it does now; None — not for any window it follows.
    pub fn busy_in(&self, now: i64) -> Option<i64> {
        if self.settings.mode == "off" {
            return None;
        }
        let mut soonest = self.settings.fast_in(now);
        if soonest == Some(0) || self.fast {
            return Some(0);
        }
        for w in self.track.values() {
            let w = w.borrow();
            let mut spans =
                vec![(w.start - NEAR_MS, if self.live_cents(w.rule()) > 0 { w.end() - LIVE_STOP_MS } else { w.start - STOP_MS })];
            let at = self.moment(&w);
            if at != 0 {
                spans.push((at - WARM_MS, at + SPRINT_MS));
            }
            for (begin, end) in spans {
                if now >= end {
                    continue;
                }
                if now >= begin {
                    return Some(0);
                }
                soonest = Some(soonest.map_or(begin - now, |s| s.min(begin - now)));
            }
        }
        soonest
    }

    /// The per-second hours, a window at the makers' moment, a window in its course: looked at each second and prepared
    /// (warm) from WARM_MS before. True when the warm-up is due now (the caller starts it).
    pub fn pace(&mut self, now: i64) -> bool {
        let on = self.settings.mode != "off";
        let wait = if on { self.settings.fast_in(now) } else { None };
        let (mut sprint, mut course, mut near, mut soon, mut listed) = (false, false, false, false, false);
        if on {
            for w in self.track.values() {
                let w = w.borrow();
                let rule = w.rule();
                sprint |= self.sprinting(&w, now);
                course |= self.phase(&w, now) == "live";
                near |= now >= w.start - WARM_MS && self.live_cents(rule) > 0;
                soon |= w.market.is_some() && 0 <= w.start - now && w.start - now <= NEAR_MS && self.settings.price(rule.key) > 0;
                listed |= w.market.is_some();
            }
        }
        let fast = wait == Some(0) || sprint || course;
        if fast != self.fast {
            self.fast = fast;
            crate::info!("规则吃单：每秒检查{}", if fast { "开始" } else { "结束" });
        }
        let first = on && !self.warmed && listed;
        if (wait.is_some_and(|w| w <= WARM_MS) || sprint || near || soon || first)
            && now - self.warm_ms >= WARM_EVERY_MS
            && !running(&self.warming)
        {
            self.warm_ms = now;
            self.warmed = true;
            return true;
        }
        false
    }

    /// Why nothing more is bought in this window, "" while something may be.
    pub fn finished(&self, slug: &str, rule: &Rule, course: bool) -> String {
        let Some(rec) = self.record(slug, None) else { return String::new() };
        let size = self.settings.shares_of(rule.market) as f64;
        if held(rec) >= size - crate::util::DUST {
            return if !in_flight(rec) {
                format!("这一场已买满 {} 份", g(size))
            } else {
                format!(
                    "这一场的 {} 份已全部下单，等结果（撤回的部分 {} 秒后确认没有晚到的成交才算空出来）",
                    g(size),
                    LATE_FILL_MS / 1000
                )
            };
        }
        if self.settings.mode == "paper" && rec_orders(rec).any(|o| o.t("paper") && o.t("live") == course) {
            return format!(
                "只记录：这一场{}已记过（只记一次：没真买，盘口上的单还在，再记就重复了）",
                if course { "盘中" } else { "" }
            );
        }
        let refused =
            rec_orders(rec).filter(|o| o.state() == "rejected" || (o.state() == "not_found" && o.t("conflict"))).count();
        if refused >= MAX_REJECTS {
            return format!("Predict 拒绝了 {MAX_REJECTS} 次，这一场不再下单");
        }
        String::new()
    }

    /// Why the rule's side is not bought in the window's course right now ("" while it may be).
    pub fn live_guard(&self, slug: &str, rule: &Rule, now: i64) -> String {
        let Some(feed) = &self.index else { return "没有币安指数行情（盘中吃单要用）".into() };
        let Some(w) = self.track.get(slug) else { return "这一场不在跟踪".into() };
        let w = w.borrow();
        let symbol = rule.spec().symbol;
        let candle = feed.close_of(symbol, w.start);
        let mut line_used = candle;
        if let (Some(c), Some(strike)) = (candle, w.strike) {
            if strike > Decimal::ZERO && (strike - c).abs() <= c * Decimal::from_str("0.001").unwrap_or_default() {
                line_used = Some(strike);
            }
        }
        let mut lines: Vec<Decimal> = vec![];
        for x in [candle, line_used].into_iter().flatten() {
            if x > Decimal::ZERO && !lines.contains(&x) {
                lines.push(x);
            }
        }
        let price = feed.price(symbol);
        if lines.is_empty() {
            return "还没读到这一场的起点价".into();
        }
        let Some(price) = price.filter(|p| *p > Decimal::ZERO) else { return "还没有实时指数".into() };
        let why = feed.price_problem(symbol, w.end(), now);
        if !why.is_empty() {
            return why;
        }
        let up = rule.side == "up";
        let line = if up { *lines.iter().max().unwrap_or(&lines[0]) } else { *lines.iter().min().unwrap_or(&lines[0]) };
        let ahead = if up { price - line } else { line - price };
        if ahead / line >= rule.lead() {
            return String::new();
        }
        let pct = crate::rules::dec_f64(rule.lead()) * 100.0;
        format!(
            "{}还没领先（指数 {}，起点 {}，要{}出 {}% 以上），不买",
            rule.word(),
            fmt_amount(crate::rules::dec_f64(price)),
            fmt_amount(crate::rules::dec_f64(line)),
            if up { "高" } else { "低" },
            g(pct)
        )
    }

    pub fn due(&self, w: &mut Win, now: i64) -> bool {
        let near = w.start - now <= NEAR_MS;
        let Some(mid) = w.mid() else {
            let expected = w.start - LISTED_AHEAD_MS;
            if now < expected - PROBE_EARLY_MS {
                return false;
            }
            let every = if now <= expected + PROBE_LATE_MS || near { PROBE_FAST_S } else { PROBE_SLOW_S };
            return (now - w.probe_ms) as f64 >= every * 1000.0;
        };
        if now < w.look_ms {
            self.stream.changed.borrow_mut().remove(&mid);
            w.indexed = 0;
            return false;
        }
        if self.stream.changed.borrow().contains(&mid) {
            return true;
        }
        let every = if self.fast {
            FAST_LOOK_S
        } else if near {
            FAST_S
        } else if w.offer && w.open_ms == 0 {
            OFFER_STATUS_S
        } else if w.open_ms != 0 {
            SLOW_S
        } else {
            CLOSED_SLOW_S
        };
        (now - w.look_ms) as f64 >= every * 1000.0
    }

    /// ``take`` within the USDT at hand and the day's cap left, whole shares.
    pub fn clamp(&self, take: Decimal, limit: Decimal, now: i64) -> Decimal {
        let mut take = take;
        if let Some(usd_left) = self.usd(None) {
            if limit > Decimal::ZERO {
                take = take.min((crate::rules::f64_dec(usd_left.max(0.0)) / limit).floor());
            }
        }
        if self.settings.max_day_usd != 0.0 && limit > Decimal::ZERO {
            let left = self.settings.max_day_usd - crate::rules::spent_on(self.recent_refs(), &day_of(now), &self.settings.mode);
            take = take.min(crate::rules::f64_dec(left.max(0.0)) / limit);
        }
        take.floor().max(Decimal::ZERO)
    }

    /// The windows of the enabled rules from now to a day on; a window of a rule switched off is dropped, and so is one this
    /// close to its start (or, for a rule that buys in its course too, to its end). Old finished records removed.
    pub fn plan(&mut self, now: i64) {
        // the holiday calendar running out: told once a day for its last CALENDAR_WARN_DAYS (then no real order goes out)
        if let (Some(left), Some(last)) = (self.calendar_left(now), self.cal_last) {
            let today = day_of(now);
            if left <= CALENDAR_WARN_DAYS && self.cal_told != today && self.settings.mode == "on" {
                self.cal_told = today;
                self.alarm(&if left == 0 {
                    format!("⛔ 规则吃单：{}", self.calendar_why(now))
                } else {
                    format!(
                        "⚠️ 规则吃单：CME 休市日历只排到 {last}（还剩 {left} 天）。到期后不再下单：在设置文件补上之后的 CME_CLOSED_DAYS、CME_EARLY_DAYS（bash deploy/gcp/rt.sh edit）"
                    )
                });
            }
        }
        let mut want: IndexMap<String, (&'static str, i64)> = IndexMap::new();
        if self.settings.mode != "off" {
            for rule in RULES.iter() {
                let course = self.live_cents(rule) > 0;
                if live_rule(&self.settings, rule) || course {
                    let back = if course { rule.spec().window_ms() - 1 } else { -STOP_MS };
                    for start in windows(rule, now - back, now + HORIZON_MS) {
                        if !self.over_at(rule, start, now) {
                            want.insert(rule.spec().slug_of(start), (rule.key, start));
                        }
                    }
                }
            }
        }
        let gone: Vec<String> = self.track.keys().filter(|s| !want.contains_key(*s)).cloned().collect();
        for slug in gone {
            let Some(w) = self.track.shift_remove(&slug) else { continue };
            let (mid, start, kept) = {
                let b = w.borrow();
                (b.mid(), b.start, b.kept)
            };
            if let Some(mid) = mid {
                self.by_market.remove(&mid);
            }
            if start - now <= STOP_MS && !kept {
                self.keep_watch(&slug, &w);
            }
        }
        for (slug, (key, start)) in want {
            if self.track.contains_key(&slug) {
                continue;
            }
            let w = self.new_win(key, start);
            let kept = self.record(&slug, None).and_then(|r| r.obj("seen")).cloned();
            if let Some(kept) = kept.filter(|k| k.t("open")) {
                let mut seen = crate::rules::watch_default();
                for (k, v) in seen.iter_mut() {
                    if let Some(x) = kept.get(k) {
                        *v = x.clone();
                    }
                }
                w.borrow_mut().seen = Some(seen);
            }
            self.track.insert(slug, w);
        }
        let recs = &self.recs;
        self.recent_keys.retain(|k| recs.get(k).map(|r| r.i("start_ms")).unwrap_or(0) >= now - RECENT_MS);
        let old: Vec<String> = self
            .recs
            .iter()
            .filter(|(_, r)| {
                r.i("start_ms") < now - KEEP_DAYS * DAY_MS && !in_flight(r) && (!r.none("pnl") || filled(r) <= crate::util::DUST)
            })
            .map(|(k, _)| k.clone())
            .collect();
        if !old.is_empty() {
            let keys: Vec<String> = old.iter().map(|k| format!("{PREFIX}{k}")).collect();
            self.store.delete_keys(&keys);
            for k in old {
                self.recs.shift_remove(&k);
            }
        }
    }
}

// --- the steps that wait -------------------------------------------------------------------------------------------------
/// Real orders' readiness: the sign-in, the Predict account checked on-chain (its key must be the account's owner).
pub async fn prepare(me: &Me) {
    let (api, wallet, chain, has_key) = {
        let t = me.borrow();
        (t.api.clone(), t.wallet.clone(), t.chain.clone(), !t.config.api_key.is_empty())
    };
    let Some(wallet) = wallet else { return };
    match api.ensure_auth().await {
        Ok(()) => {
            let mut t = me.borrow_mut();
            t.ready_error.clear();
            t.ready_ms = t.now();
        }
        Err(e) => {
            let mut t = me.borrow_mut();
            t.ready_error = clean(&e);
            if matches!(e.status(), 401 | 403) && !has_key {
                t.ready_error += "（可能需要 PREDICT_API_KEY）";
            }
        }
    }
    let (Some(chain), false) = (chain, wallet.predict_account.is_empty()) else { return };
    match chain.account_owner(&wallet.predict_account).await {
        Err(e) => {
            let mut t = me.borrow_mut();
            if !t.account_checked {
                t.account_error = format!("无法在链上核对 PREDICT_ACCOUNT 的控制钥匙：{}", clean(&e));
            }
        }
        Ok(owner) => {
            let mut t = me.borrow_mut();
            let zero = owner.trim_start_matches("0x").chars().all(|c| c == '0');
            t.account_error = if zero {
                "链上没有这个 Predict 账户的签名记录：地址填错，或账户还没在链上激活（先在网站交易一次）".into()
            } else if owner.to_lowercase() != wallet.signer.to_lowercase() {
                "PREDICT_PRIVATE_KEY 不是 PREDICT_ACCOUNT 这个 Predict 账户的控制钥匙：请导出该账户的 Privy 钱包私钥".into()
            } else {
                String::new()
            };
            t.account_checked = t.account_error.is_empty();
        }
    }
}

pub async fn refresh_balance(me: &Me) {
    let Some(chain) = me.borrow().chain.clone() else { return };
    let asked = me.borrow().now();
    let (usdt, bnb) = tokio::join!(chain.usdt_balance(), chain.bnb_balance());
    let mut t = me.borrow_mut();
    match (usdt, bnb) {
        (Ok(u), Ok(b)) => {
            t.usdt = Some(u);
            t.bnb = Some(b);
            t.balance_ms = t.now();
            t.balance_asked_ms = asked;
            t.balance_error.clear();
        }
        (Err(e), _) | (_, Err(e)) => {
            t.balance_error = clean(&e);
            t.balance_ms = t.now() - BALANCE_IDLE_MS + 15_000; // (again in 15 s)
        }
    }
}

/// Signed in, checked, the balance read — and kept so.
pub async fn ready_loop(me: Me) {
    let stop = me.borrow().stop.clone();
    while !stop.is_set() {
        let (on, now, retry) = {
            let t = me.borrow();
            let now = t.now();
            let on = t.settings.mode == "on" && t.wallet.is_some();
            let retry = (!t.ready() || !t.api.fresh()) && now - t.ready_ms >= if t.ready_ms != 0 { READY_RETRY_MS } else { 0 };
            (on, now, retry)
        };
        if on {
            if retry {
                me.borrow_mut().ready_ms = now;
                prepare(&me).await;
            }
            let (busy, balance_ms) = {
                let t = me.borrow();
                (t.busy_in(now), t.balance_ms)
            };
            let every = if busy.is_some_and(|b| b <= WARM_MS) { BALANCE_BUSY_MS } else { BALANCE_IDLE_MS };
            if now - balance_ms >= every {
                refresh_balance(&me).await;
            }
        }
        let _ = stop.wait_for(2.0).await;
    }
}

pub async fn lag_loop(me: Weak<RefCell<Taker>>) {
    loop {
        let began = mono();
        sleep(LAG_EVERY_S).await;
        let lag = ((mono() - began - LAG_EVERY_S) * 1000.0).max(0.0);
        let Some(me) = me.upgrade() else { return };
        if let Ok(mut t) = me.try_borrow_mut() {
            t.loop_lags.push_back(lag);
            while t.loop_lags.len() > (60.0 / LAG_EVERY_S) as usize {
                t.loop_lags.pop_front();
            }
        }
    }
}

/// The service: the push connections, the readiness, the turns; at a stop the orders on their way given their answers, the
/// resting ones withdrawn, the store written.
pub async fn run(me: Me) {
    let (stream, index, stop) = {
        let t = me.borrow();
        (t.stream.clone(), t.index.clone(), t.stop.clone())
    };
    let mut side = vec![spawn(stream.clone().run()), spawn(lag_loop(Rc::downgrade(&me))), spawn(ready_loop(me.clone()))];
    if let Some(index) = index.clone() {
        side.push(spawn(index.run()));
    }
    {
        let mut t = me.borrow_mut();
        t.running = true;
        t.started_ms = t.now();
    }
    while !stop.is_set() {
        turn(&me).await;
        rest(&me).await;
    }
    me.borrow_mut().running = false;
    let pending: Vec<JoinHandle<()>> =
        std::mem::take(&mut me.borrow_mut().sends).into_iter().filter(|t| !t.is_finished()).collect();
    if !pending.is_empty() {
        // an order's answer still on its way: its record made whole (and its rest withdrawn: halted) before the process goes
        let _ =
            tokio::time::timeout(std::time::Duration::from_secs_f64(SEND_WAIT_S), futures_util::future::join_all(pending)).await;
    }
    let _ =
        tokio::time::timeout(std::time::Duration::from_secs_f64(LAST_WITHDRAW_S), super::orders::withdraw_all(&me, "服务停止"))
            .await;
    let waiter = me.borrow().store.flush_waiter(10.0);
    let _ = crate::store::flush_async(waiter).await;
    stream.stop();
    if let Some(index) = index {
        index.stop();
    }
    let mut held = me.borrow_mut();
    let t = &mut *held;
    for task in side.into_iter().chain(t.side_tasks.drain(..)).chain(t.warming.take()).chain(t.verifying.take()) {
        task.abort();
    }
}

pub fn rest_s(t: &Taker) -> f64 {
    let changed = !t.stream.changed.borrow().is_empty();
    if !changed {
        TICK_S
    } else if t.stream.arrivals.get() != t.looked {
        REST_SOON_S
    } else {
        REST_MIN_S
    }
}

pub async fn rest(me: &Me) {
    let (stream, wakeup, stop, wait) = {
        let t = me.borrow();
        (t.stream.clone(), t.wakeup.clone(), t.stop.clone(), rest_s(&t))
    };
    stream.pushed.clear();
    if wakeup.is_set() {
        // (set while the turn ran — an order's first read is due: the next turn at once)
        wakeup.clear();
        tokio::task::yield_now().await;
        return;
    }
    tokio::select! {
        _ = stop.wait() => {}
        _ = stream.pushed.wait() => {}
        _ = wakeup.wait() => {}
        _ = sleep(wait) => {}
    }
    wakeup.clear();
}

/// The loop woken in ``seconds`` (an order's first read, FILL_READ_MS after its answer: not up to a tick later).
pub fn wake_in(me: &Me, seconds: f64) {
    let wakeup = me.borrow().wakeup.clone();
    spawn(async move {
        sleep(seconds).await;
        wakeup.set();
    });
}

/// A followed window's book just changed: that window is looked at now, in this very turn — its order signed and its request
/// started before anything else gets a turn. True when the change was taken (the loop need not wake).
pub fn on_change(me: &Me, mid: &str) -> bool {
    let (slug, w) = {
        let Ok(t) = me.try_borrow() else { return false };
        if !t.running || t.stop.is_set() || t.settings.mode == "off" {
            return false;
        }
        let Some(slug) = t.by_market.get(mid).cloned() else { return false };
        let Some(w) = t.track.get(&slug).cloned() else { return false };
        (slug, w)
    };
    {
        let Ok(mut b) = w.try_borrow_mut() else { return false };
        if b.busy {
            b.again = true; // looked at again the moment the look under way ends
            return true;
        }
    }
    me.borrow_mut().first_ticks += 1;
    if let Some(task) = start_now(look_now(me.clone(), slug, w)) {
        me.borrow_mut().side_tasks.push(task);
        tidy(me);
    }
    !me.borrow().stream.changed.borrow().contains(mid)
}

/// The side tasks that ended dropped from the list.
pub fn tidy(me: &Me) {
    if let Ok(mut t) = me.try_borrow_mut() {
        if t.side_tasks.len() > 64 {
            t.side_tasks.retain(|h| !h.is_finished());
        }
        if t.sends.len() > 64 {
            t.sends.retain(|h| !h.is_finished());
        }
    }
}

struct Busy(W);

impl Drop for Busy {
    fn drop(&mut self) {
        if let Ok(mut w) = self.0.try_borrow_mut() {
            w.busy = false;
        }
    }
}

/// A window's look, one at a time a window (another change while it runs: looked at again right after).
pub async fn look_now(me: Me, slug: String, w: W) {
    {
        let mut b = w.borrow_mut();
        if b.busy {
            b.again = true;
            return;
        }
        b.busy = true;
    }
    let _busy = Busy(w.clone());
    for _ in 0..5 {
        w.borrow_mut().again = false;
        if let Err(error) = super::look::look(&me, &slug, &w).await {
            let word = format!("读取失败：{}", clean(&error));
            w.borrow_mut().word = word.clone();
            me.borrow_mut().note(&format!("{slug}：{word}"));
        }
        let again = w.borrow().again;
        let (stopped, same) = {
            let t = me.borrow();
            (t.stop.is_set(), t.track.get(&slug).is_some_and(|x| Rc::ptr_eq(x, &w)))
        };
        if !again || stopped || !same {
            break;
        }
    }
}

/// The index of a card just moved (or the start line of its window was read): the window in its course is looked at as the
/// index comes.
pub fn on_index(me: &Me, symbol: &str, at_ms: i64) {
    let mids = {
        let Ok(mut t) = me.try_borrow_mut() else { return };
        if !t.running || t.settings.mode == "off" {
            return;
        }
        let now = t.now();
        let mut hit: Vec<W> = vec![];
        let mut late = 0;
        for (slug, w) in &t.track {
            let b = w.borrow();
            let rule = b.rule();
            if b.market.is_none()
                || rule.spec().symbol != symbol
                || t.phase(&b, now) != "live"
                || now < b.look_ms
                || !t.finished(slug, rule, true).is_empty()
            {
                continue;
            }
            if at_ms != 0 && now - at_ms > INDEX_FRESH_MS {
                late += 1;
                continue;
            }
            hit.push(w.clone());
        }
        t.index_late += late;
        if hit.is_empty() || (t.settings.mode == "on" && !t.block(0.0, None).is_empty()) || t.cooling() > 0.0 {
            return;
        }
        let warm = if !t.fast { t.pace(now) } else { false };
        let wall = now_ms();
        let mids: Vec<String> = hit
            .iter()
            .filter_map(|w| {
                let mut b = w.borrow_mut();
                b.indexed = wall;
                b.mid()
            })
            .collect();
        t.index_looks += 1;
        drop(t);
        if warm {
            start_warm(me);
        }
        mids
    };
    let stream = me.borrow().stream.clone();
    stream.nudge(&mids);
}

pub fn start_warm(me: &Me) {
    let task = spawn(warm(me.clone()));
    me.borrow_mut().warming = Some(task);
}

/// One tick's work: the plan, the pace, the looks that are due, a REST read beside the pushes, the orders signed
/// beforehand, the follow-ups and the results.
pub async fn turn(me: &Me) {
    let (order, warm) = {
        let mut t = me.borrow_mut();
        t.ticks += 1;
        t.hold_loss_stop();
        let now = t.now();
        if now - t.planned_ms >= PLAN_EVERY_MS {
            t.plan(now);
            t.planned_ms = now;
        }
        let warm = t.pace(now);
        let markets: HashSet<String> = t.by_market.keys().cloned().collect();
        t.stream.changed.borrow_mut().retain(|m| markets.contains(m));
        if t.settings.mode == "off" || t.cooling() > 0.0 {
            t.stream.changed.borrow_mut().clear();
            for w in t.track.values() {
                w.borrow_mut().indexed = 0;
            }
        }
        let mut order: Vec<(String, W)> = vec![];
        if t.settings.mode != "off" {
            let changed: HashSet<String> = t.stream.changed.borrow().clone();
            t.looked = t.stream.arrivals.get();
            order = t.track.iter().map(|(s, w)| (s.clone(), w.clone())).collect();
            order.sort_by_key(|(_, w)| {
                let b = w.borrow();
                (b.mid().map(|m| !changed.contains(&m)).unwrap_or(true), b.start)
            });
        }
        (order, warm)
    };
    if warm {
        start_warm(me);
    }
    let off = me.borrow().settings.mode == "off";
    if !off {
        for (slug, w) in order {
            let (stopped, cooling, same) = {
                let t = me.borrow();
                (t.stop.is_set(), t.cooling() > 0.0, t.track.get(&slug).is_some_and(|x| Rc::ptr_eq(x, &w)))
            };
            if stopped || cooling {
                break;
            }
            if w.borrow().busy || !same {
                continue;
            }
            look_now(me.clone(), slug, w).await;
        }
        if me.borrow().fast {
            verify_tick(me);
        }
        let now = me.borrow().now();
        keep_warm(me, now);
        course_reads(me, now);
        super::look::presign(&mut me.borrow_mut(), now);
    }
    {
        let t = me.borrow();
        let ids: std::collections::BTreeSet<String> = t.by_market.keys().cloned().collect();
        t.stream.want(&ids);
    }
    super::orders::follow(me).await;
    super::orders::settle(me).await;
    tidy(me);
}

/// An offer at the rule's price in hand on a market whose matching has not been seen open: its status asked every rush_pace()
/// seconds for STATUS_RUSH_MS; the moment it says OPEN the window is looked at.
pub fn rush(t: &mut Taker, me: &Me, slug: &str, w: &W) {
    if t.settings.mode != "on" {
        return;
    }
    let mut b = w.borrow_mut();
    if Taker::rushing(&b) || b.rush_reads >= RUSH_READS_MAX {
        return;
    }
    b.rush_wall = now_ms();
    let task = spawn(rush_run(me.clone(), slug.to_string(), w.clone()));
    b.rush_task = Some(task);
}

async fn rush_run(me: Me, slug: String, w: W) {
    // (a task of its own: it begins once the look that started it has let go)
    let began = mono();
    loop {
        let (mid, pace) = {
            let t = me.borrow();
            let b = w.borrow();
            let same = t.track.get(&slug).is_some_and(|x| Rc::ptr_eq(x, &w));
            if b.open_ms != 0
                || !b.offer
                || !same
                || b.market.is_none()
                || t.stop.is_set()
                || t.settings.mode != "on"
                || mono() - began >= STATUS_RUSH_MS as f64 / 1000.0
                || b.rush_reads >= RUSH_READS_MAX
            {
                return;
            }
            (b.mid().unwrap_or_default(), t.rush_pace())
        };
        let asked = mono();
        let cooling = me.borrow().cooling() > 0.0;
        if !cooling {
            let api = {
                let mut t = me.borrow_mut();
                t.reads += 1;
                t.status_reads += 1;
                w.borrow_mut().rush_reads += 1;
                t.api.clone()
            };
            match api.market(&mid, true).await {
                Err(e) => w.borrow_mut().rush_error = clean(&e),
                Ok(market) => {
                    let opened = {
                        let t = me.borrow();
                        let same = t.track.get(&slug).is_some_and(|x| Rc::ptr_eq(x, &w));
                        let mut b = w.borrow_mut();
                        if !same || b.market.is_none() {
                            return;
                        }
                        let open = status_open(&market);
                        b.market = Some(with_slug(market, &slug));
                        if open {
                            if b.open_ms == 0 {
                                b.open_ms = t.now();
                                b.open_wall = now_ms();
                                let at = b.open_ms;
                                let seen = b.watch();
                                if seen.i("open_ms") == 0 {
                                    seen.insert("open_ms".into(), json!(at));
                                }
                            }
                        }
                        open
                    };
                    if opened {
                        let stream = me.borrow().stream.clone();
                        stream.nudge(&[mid]); // looked at now: the offer in hand is bought
                        return;
                    }
                }
            }
        }
        sleep((pace - (mono() - asked)).max(0.0)).await;
    }
}

/// Ready before an offer shows: every window not yet found looked for at the next tick, a market not yet seen open for matching
/// read once more, and — real orders — the sign-in, the exchange allowances and the balance in hand.
pub async fn warm(me: Me) {
    if let Err(error) = warm_inner(&me).await {
        me.borrow_mut().note(&format!("每秒检查的准备没做完：{}", clean(&error)));
    }
}

async fn warm_inner(me: &Me) -> Res<()> {
    let windows: Vec<(String, W)> = {
        let t = me.borrow();
        let now = t.now();
        for w in t.track.values() {
            let mut b = w.borrow_mut();
            if b.market.is_none() && b.probe_ms <= now {
                b.probe_ms = 0;
            }
        }
        if t.cooling() > 0.0 {
            return Ok(());
        }
        let mut list: Vec<(String, W)> = t.track.iter().map(|(s, w)| (s.clone(), w.clone())).collect();
        list.sort_by_key(|(_, w)| w.borrow().start);
        list
    };
    for (slug, w) in windows {
        let (mid, api) = {
            let t = me.borrow();
            let b = w.borrow();
            if b.market.is_none() || b.open_ms != 0 || t.stop.is_set() || !t.track.contains_key(&slug) {
                continue;
            }
            (b.mid().unwrap_or_default(), t.api.clone())
        };
        let now = {
            let mut t = me.borrow_mut();
            t.reads += 1;
            t.now()
        };
        let market = api.market(&mid, false).await?;
        let mut b = w.borrow_mut();
        let open = status_open(&market);
        b.market = Some(with_slug(market, &slug));
        b.status_ms = now;
        if open {
            b.open_ms = now;
            b.open_wall = now_ms();
            let seen = b.watch();
            if seen.i("open_ms") == 0 {
                seen.insert("open_ms".into(), json!(now));
            }
        }
    }
    let (api, chain, exchanges) = {
        let t = me.borrow();
        if t.settings.mode != "on" || !t.block(0.0, None).is_empty() {
            return Ok(());
        }
        let mut exchanges: Vec<String> = t
            .track
            .values()
            .filter_map(|w| w.borrow().market.as_ref().map(|m| parse_market(m, t.config.fee_bps)))
            .map(|info| exchange_key(info.neg_risk, info.yield_bearing).to_string())
            .collect();
        exchanges.sort();
        exchanges.dedup();
        (t.api.clone(), t.chain.clone(), exchanges)
    };
    api.ensure_auth().await?;
    if let Some(chain) = chain {
        for exchange in exchanges {
            let got = chain.allowance(&exchange).await?;
            let mut t = me.borrow_mut();
            let now = t.now();
            t.allowance.insert(exchange, (now, got));
        }
    }
    let due = {
        let t = me.borrow();
        t.now() - t.balance_ms >= BALANCE_BUSY_MS
    };
    if due {
        refresh_balance(me).await;
    }
    Ok(())
}

/// The REST read beside the pushes (in the per-second stretches): one window's book at a time, the longest unread first,
/// three a second at most and a window not again within a second. At the makers' moment the reads are that window's; a market
/// not yet seen open for matching is asked its status instead.
pub fn verify_tick(me: &Me) {
    let pick = {
        let t = me.borrow();
        if running(&t.verifying) {
            return;
        }
        let m = mono();
        if m - t.verify_at < VERIFY_EVERY_S || t.cooling() > 0.0 || (t.settings.mode == "on" && !t.block(0.0, None).is_empty()) {
            return;
        }
        let now = t.now();
        let mut open_to: Vec<(String, W)> = vec![];
        for (slug, w) in &t.track {
            let b = w.borrow();
            if b.market.is_none() {
                continue;
            }
            let phase = t.phase(&b, now);
            if phase.is_empty() || now < b.look_ms {
                continue;
            }
            let rule = b.rule();
            let cents = if phase == "live" { t.live_cents(rule) } else { t.settings.price(rule.key) };
            if cents <= 0 || !t.finished(slug, rule, phase == "live").is_empty() {
                continue;
            }
            drop(b);
            if phase == "live" && !t.live_guard(slug, rule, now).is_empty() {
                continue;
            }
            open_to.push((slug.clone(), w.clone()));
        }
        let hot: Vec<(String, W)> = open_to.iter().filter(|(_, w)| t.hot(&w.borrow(), now)).cloned().collect();
        let often = if hot.is_empty() { VERIFY_AGAIN_S } else { VERIFY_EVERY_S };
        let pool = if hot.is_empty() { &open_to } else { &hot };
        let due = pool
            .iter()
            .map(|(s, w)| (w.borrow().rest_at.unwrap_or(0.0), s.clone(), w.clone()))
            .filter(|(at, _, _)| m - at >= often)
            .min_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal).then_with(|| a.1.cmp(&b.1)));
        let Some((_, slug, w)) = due else { return };
        let status = {
            let b = w.borrow();
            let mid = b.mid().unwrap_or_default();
            !hot.is_empty() && b.open_ms == 0 && t.stream.alive() && t.stream.held(&mid).is_some()
        };
        (slug, w, status, m)
    };
    let (slug, w, status, m) = pick;
    w.borrow_mut().rest_at = Some(m);
    me.borrow_mut().verify_at = m;
    let task = if status {
        spawn(verify_status(me.clone(), slug, w))
    } else {
        let mid = w.borrow().mid().unwrap_or_default();
        spawn(verify(me.clone(), mid))
    };
    me.borrow_mut().verifying = Some(task);
}

/// Through every busy stretch a pooled connection to Predict is kept used within WARM_CONN_S (an order reuses only a connection
/// that fresh; on a cold one it waits for a TCP + TLS handshake): a book of the soonest window read and merged.
pub fn keep_warm(me: &Me, now: i64) {
    let mid = {
        let t = me.borrow();
        if t.settings.mode != "on" || running(&t.verifying) || t.cooling() > 0.0 {
            return;
        }
        let fresh = crate::http::http().pool_fresh(&t.api.host, 443);
        if fresh.is_some_and(|f| f < WARM_CONN_S) {
            return;
        }
        if t.busy_in(now) != Some(0) || !t.block(0.0, None).is_empty() {
            return;
        }
        let soon = t
            .track
            .iter()
            .filter(|(_, w)| {
                let b = w.borrow();
                b.market.is_some() && !t.phase(&b, now).is_empty()
            })
            .min_by_key(|(s, w)| (w.borrow().start, (*s).clone()));
        let Some((_, w)) = soon else { return };
        let m = mono();
        w.borrow_mut().rest_at = Some(m);
        let mid = w.borrow().mid().unwrap_or_default();
        drop(t);
        me.borrow_mut().verify_at = m;
        mid
    };
    let task = spawn(verify(me.clone(), mid));
    me.borrow_mut().verifying = Some(task);
}

pub async fn verify_status(me: Me, slug: String, w: W) {
    let (api, mid, now) = {
        let mut t = me.borrow_mut();
        t.reads += 1;
        t.status_reads += 1;
        (t.api.clone(), w.borrow().mid().unwrap_or_default(), t.now())
    };
    match api.market(&mid, false).await {
        Err(e) => me.borrow_mut().note(&format!("撮合状态没读到：{}", clean(&e))),
        Ok(market) => {
            let nudge = {
                let t = me.borrow();
                let same = t.track.get(&slug).is_some_and(|x| Rc::ptr_eq(x, &w));
                let mut b = w.borrow_mut();
                if !same || b.market.is_none() {
                    return;
                }
                let open = status_open(&market);
                b.market = Some(with_slug(market, &slug));
                b.status_ms = now.max(b.status_ms);
                if open && b.open_ms == 0 {
                    b.open_ms = t.now();
                    b.open_wall = now_ms();
                    let at = b.open_ms;
                    let seen = b.watch();
                    if seen.i("open_ms") == 0 {
                        seen.insert("open_ms".into(), json!(at));
                    }
                    b.offer
                } else {
                    false
                }
            };
            if nudge {
                let stream = me.borrow().stream.clone();
                stream.nudge(&[mid]);
            }
        }
    }
}

pub async fn verify(me: Me, mid: String) {
    let asked = mono();
    let api = {
        let mut t = me.borrow_mut();
        t.reads += 1;
        t.verifies += 1;
        t.api.clone()
    };
    match api.orderbook(&mid).await {
        Err(e) => {
            let mut t = me.borrow_mut();
            t.verify_error = format!("{} {}", bj(t.now(), false), clean(&e));
        }
        Ok((bids, asks)) => {
            let stream = {
                let mut t = me.borrow_mut();
                t.verify_error.clear();
                push_cap_i(&mut t.verify_ms, ((mono() - asked) * 1000.0) as i64, 120);
                t.stream.clone()
            };
            if stream.merge(&mid, bids, asks, asked) {
                me.borrow_mut().verify_diffs += 1;
            }
        }
    }
}

fn push_cap_i(q: &mut VecDeque<i64>, v: i64, cap: usize) {
    q.push_back(v);
    while q.len() > cap {
        q.pop_front();
    }
}

/// 盘中: the start line of a window in (or entering) its course read once its minute is over, and Predict's own start price
/// asked for every STRIKE_EVERY_MS through the course's first minutes — beside the looks, never in their way.
pub fn course_reads(me: &Me, now: i64) {
    let t = me.borrow();
    let Some(index) = t.index.clone() else { return };
    let mut jobs: Vec<(String, W, bool, bool)> = vec![];
    for (slug, w) in &t.track {
        let b = w.borrow();
        let rule = b.rule();
        if t.live_cents(rule) <= 0 || b.market.is_none() || !(b.start <= now && now < b.end()) {
            continue;
        }
        let candle = index.close_of(rule.spec().symbol, b.start).is_none() && !running(&b.candle_task);
        let strike = b.strike.is_none()
            && now - b.start < STRIKE_FOR_MS
            && now - b.strike_ms >= STRIKE_EVERY_MS
            && !running(&b.strike_task)
            && t.cooling() <= 0.0;
        if candle || strike {
            jobs.push((slug.clone(), w.clone(), candle, strike));
        }
    }
    drop(t);
    for (slug, w, candle, strike) in jobs {
        let (symbol, start) = {
            let b = w.borrow();
            (b.rule().spec().symbol, b.start)
        };
        if candle {
            let feed = index.clone();
            let task = spawn(async move {
                feed.ensure_start(symbol, start, now).await;
            });
            w.borrow_mut().candle_task = Some(task);
        }
        if strike {
            w.borrow_mut().strike_ms = now;
            let task = spawn(read_strike(me.clone(), slug, w.clone()));
            w.borrow_mut().strike_task = Some(task);
        }
    }
}

/// The account's USDT allowance for an exchange, read on-chain.
pub async fn read_allowance(me: &Me, exchange: &str) -> Res<(i64, u128)> {
    let Some(chain) = me.borrow().chain.clone() else { return Err(crate::util::RemoteError::new("没有链上节点")) };
    let got = chain.allowance(exchange).await?;
    let mut t = me.borrow_mut();
    let pair = (t.now(), got);
    t.allowance.insert(exchange.to_string(), pair);
    Ok(pair)
}

pub async fn read_allowance_quietly(me: Me, exchange: String) {
    if let Err(e) = read_allowance(&me, &exchange).await {
        me.borrow_mut().note(&format!("交易所授权额度没读到（沿用上一次的）：{}", clean(&e)));
    }
}

pub async fn read_strike(me: Me, slug: String, w: W) {
    let (api, mid) = {
        let mut t = me.borrow_mut();
        t.reads += 1;
        (t.api.clone(), w.borrow().mid().unwrap_or_default())
    };
    let Ok(market) = api.market(&mid, false).await else { return };
    let symbol = {
        let t = me.borrow();
        if !t.track.get(&slug).is_some_and(|x| Rc::ptr_eq(x, &w)) {
            return;
        }
        let mut b = w.borrow_mut();
        b.strike = market_start_price(&market);
        if b.strike.is_none() || t.index.is_none() {
            return;
        }
        b.rule().spec().symbol
    };
    on_index(&me, symbol, 0);
}
