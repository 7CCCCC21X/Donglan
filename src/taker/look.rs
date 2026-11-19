//! A window's look and its order: the market found, the book (pushed, or read), the offers at the rule's price, matching's
//! status, the take; the order signed (beforehand when it can be), recorded, journalled and sent.
use super::*;
use crate::predict::{DUPLICATE_WORDS, MarketInfo, Outcome, book_up_first, outcome_for_side, parse_market};
use crate::rules::{Level, claims, dec_f64, f64_dec, filled, held, offered, open_orders, spent_on, walk, without};
use crate::sync::{spawn, start_now};
use crate::util::{Kind, Res, comma, g, round_to};
use crate::wallet::{Amounts, MAX_SALT, Order, build_order, exchange_key, limit_amounts, order_body, to_wei};
use sha2::{Digest, Sha256};

fn levels_json(levels: &[Level]) -> Value {
    Value::Array(levels.iter().take(5).map(|(p, q)| json!([jf(dec_f64(*p)), jf(dec_f64(*q))])).collect())
}

fn total(levels: &[Level]) -> Decimal {
    levels.iter().map(|(_, q)| *q).sum()
}

fn same(t: &Taker, slug: &str, w: &W) -> bool {
    t.track.get(slug).is_some_and(|x| Rc::ptr_eq(x, w))
}

/// ``f"{d:g}"`` of a Decimal: its digits as they stand.
pub fn dg(d: Decimal) -> String {
    d.to_string()
}

impl Taker {
    pub fn new_record(&self, slug: &str, w: &Win, rule: &Rule, index_set: i64, mode: Option<&str>) -> Obj {
        let card = rule.spec();
        let mut r = Obj::new();
        r.insert("slug".into(), json!(slug));
        r.insert("rule".into(), json!(rule.key));
        r.insert("card".into(), json!(rule.card));
        r.insert("side".into(), json!(rule.side));
        r.insert("mode".into(), json!(mode.unwrap_or(&self.settings.mode)));
        r.insert("name".into(), json!(card.name_of(w.start)));
        r.insert("start_ms".into(), json!(w.start));
        r.insert("end_ms".into(), json!(card.end_of(w.start)));
        r.insert("market_id".into(), json!(w.mid().unwrap_or_default()));
        r.insert("index_set".into(), json!(index_set));
        r.insert("cents".into(), json!(self.settings.price(rule.key)));
        r.insert("orders".into(), json!([]));
        r.insert("created_ms".into(), json!(self.now()));
        r.insert("result".into(), json!(""));
        r.insert("pnl".into(), Value::Null);
        r
    }

    pub fn observe(&self, w: &mut Win, rule: &Rule, now: i64) -> bool {
        let Some(seen) = &w.seen else { return false };
        if !seen.t("offer_ms") || seen.t("gone_ms") {
            return false;
        }
        let Some(mid) = w.mid() else { return false };
        let (got, stamp) = (self.stream.held(&mid), self.stream.stamp(&mid));
        let limit = Decimal::from(self.settings.price(rule.key)) / Decimal::from(100);
        let (Some(got), Some(stamp)) = (got, stamp) else { return false };
        if limit <= Decimal::ZERO {
            return false;
        }
        let there = dec_f64(total(&offered(&got.0, &got.1, rule.side, limit)));
        Self::clock_offers(w, there, now, stamp.0)
    }

    pub fn note_offers(w: &mut Win, levels: &[Level], best: &[Level], now: i64) -> bool {
        if let Some((p, _)) = best.first() {
            let seen = w.watch();
            let held_best = seen.of("best").unwrap_or(1.0);
            seen.insert("best".into(), jf(dec_f64(*p).min(held_best)));
        }
        let stamp = w.book_at.0;
        let noted = Self::clock_offers(w, dec_f64(total(levels)), now, stamp);
        let open = w.open_ms;
        let seen = w.watch();
        if open != 0 && !seen.t("open_ms") {
            seen.insert("open_ms".into(), json!(open));
        }
        noted
    }

    pub fn clock_offers(w: &mut Win, total: f64, now: i64, stamp: i64) -> bool {
        let there = w.there;
        let seen = w.watch();
        let mut noted = false;
        if total > seen.f("offer_max") {
            seen.insert("offer_max".into(), jf(total));
            noted = true;
        }
        if total > 0.0 && !seen.t("offer_ms") {
            seen.insert("offer_ms".into(), json!(now));
            seen.insert("offer_ts".into(), json!(stamp));
            noted = true;
        } else if seen.t("offer_ms") && !seen.t("cut_ms") && total < there - crate::util::DUST {
            seen.insert("cut_ms".into(), json!(now));
            seen.insert("cut_ts".into(), json!(stamp));
            noted = true;
        }
        if total <= 0.0 && seen.t("offer_ms") && !seen.t("gone_ms") {
            seen.insert("gone_ms".into(), json!(now));
            seen.insert("gone_ts".into(), json!(stamp));
            noted = true;
        }
        w.there = total;
        noted
    }

    pub fn save_watch(&mut self, slug: &str, w: &Win, rule: &Rule) {
        let Some(seen) = &w.seen else { return };
        if w.market.is_none() || self.settings.mode == "off" || self.settings.price(rule.key) <= 0 {
            return;
        }
        let mut rec = self.record(slug, None).cloned().unwrap_or_else(|| self.new_record(slug, w, rule, 0, None));
        let mut seen = seen.clone();
        seen.insert("start_ms".into(), json!(w.start));
        seen.insert("cents".into(), json!(self.settings.price(rule.key)));
        seen.insert("open".into(), json!(true));
        rec.insert("seen".into(), Value::Object(seen));
        self.save(rec);
    }

    pub fn keep_watch(&mut self, slug: &str, w: &W) {
        let w = w.borrow();
        let Some(seen) = &w.seen else { return };
        let rule = w.rule();
        if !seen.t("listed_ms") || w.market.is_none() || self.settings.mode == "off" || self.settings.price(rule.key) <= 0 {
            return;
        }
        let mut rec = self.record(slug, None).cloned().unwrap_or_else(|| self.new_record(slug, &w, rule, 0, None));
        let mut seen = seen.clone();
        seen.insert("start_ms".into(), json!(w.start));
        seen.insert("cents".into(), json!(self.settings.price(rule.key)));
        seen.insert("bought".into(), jf(round_to(filled(&rec), 4)));
        rec.insert("seen".into(), Value::Object(seen));
        self.save(rec);
    }

    /// 只记录: what the order would have bought, at the book's prices — no order.
    pub fn paper(&mut self, slug: &str, w: &mut Win, rule: &Rule, levels: &[Level], take: Decimal, limit: Decimal, course: bool) {
        let Some(market) = &w.market else { return };
        let info = parse_market(market, self.config.fee_bps);
        let (outcome, _) = outcome_for_side(rule.side, &info.outcomes);
        let index_set = outcome.as_ref().map(|o| o.index_set).unwrap_or(0);
        let mut rec =
            self.record(slug, Some("paper")).cloned().unwrap_or_else(|| self.new_record(slug, w, rule, index_set, Some("paper")));
        if rec.i("index_set") == 0 {
            rec.insert("index_set".into(), json!(index_set));
        }
        let avg = walk(levels, take);
        let mut order = Obj::new();
        order.insert("paper".into(), json!(true));
        order.insert("state".into(), json!("final"));
        order.insert("shares".into(), jf(dec_f64(take)));
        order.insert("filled".into(), jf(dec_f64(take)));
        order.insert("limit".into(), jf(dec_f64(limit)));
        order.insert("avg".into(), jf(dec_f64(avg)));
        order.insert("fee_bps".into(), json!(info.fee_bps));
        order.insert("at".into(), json!(self.now()));
        if course {
            order.insert("live".into(), json!(true));
        }
        order.insert("book".into(), levels_json(levels));
        push_order(&mut rec, order);
        let name = rec.s("name");
        self.save(rec);
        let cents = dec_f64(avg) * 100.0;
        w.word = format!("{}只记录：{} {:.1}¢ × {} 份", if course { "盘中 " } else { "" }, rule.word(), cents, dg(take));
        self.notify(&format!(
            "🧪 规则吃单{}（只记录，没下单）：{name} 买{} 均价 {:.1}¢ × {} 份（{}{} ≤{}¢）",
            if course { "·盘中" } else { "" },
            rule.word(),
            cents,
            dg(take),
            rule.name,
            if course { "盘中" } else { "" },
            (limit * Decimal::from(100)).trunc()
        ));
    }

    /// The order a look would send now, unsigned: (the order, amounts, info, outcome, expires); None when the market's tokens
    /// cannot be told apart.
    pub fn order_for(
        &self,
        w: &Win,
        rule: &Rule,
        take: Decimal,
        limit: Decimal,
        course: bool,
        count: usize,
    ) -> Option<(Order, Amounts, MarketInfo, Outcome, i64)> {
        let wallet = self.wallet.as_ref()?;
        let info = parse_market(w.market.as_ref()?, self.config.fee_bps);
        let (outcome, assumed) = outcome_for_side(rule.side, &info.outcomes);
        let outcome = outcome?;
        if !assumed.is_empty() {
            return None;
        }
        let expires = (if course { w.end() } else { w.start }).div_euclid(1000) - EXPIRY_BEFORE_S;
        let amounts = limit_amounts(true, to_wei(dec_f64(limit)), to_wei(dec_f64(take))).ok()?;
        let identity = format!(
            "rule-v1|{}|{}|{}|{}|{}|{count}",
            self.config.chain_id,
            wallet.maker.to_lowercase(),
            info.market_id,
            outcome.token,
            w.start
        );
        let digest = Sha256::digest(identity.as_bytes());
        let mut head = [0u8; 8];
        head.copy_from_slice(&digest[..8]);
        let salt = u64::from_be_bytes(head) % MAX_SALT;
        let order = build_order(wallet, &outcome.token, amounts.maker, amounts.taker, info.fee_bps, expires, salt);
        Some((order, amounts, info, outcome, expires))
    }

    /// (hash, signature, signed beforehand) — the same fields always make the same order and the same signature, so an order
    /// signed ahead of the offer (presign) is the one the look would sign itself.
    pub fn sign(&mut self, order: &Order, neg_risk: bool, yield_bearing: bool) -> Result<(String, String, bool), String> {
        let key = format!(
            "{neg_risk}|{yield_bearing}|{}|{}|{}|{}|{}|{}|{}",
            order.salt, order.token_id, order.maker_amount, order.taker_amount, order.fee_rate_bps, order.expiration, order.maker
        );
        if let Some((h, s)) = self.signed.get(&key) {
            return Ok((h.clone(), s.clone(), true));
        }
        let wallet = self.wallet.clone().ok_or("没有签名钱包")?;
        let (digest, signature) = wallet.sign_order(order, neg_risk, yield_bearing)?;
        self.signed.insert(key, (digest.clone(), signature.clone()));
        while self.signed.len() > SIGN_CACHE {
            self.signed.shift_remove_index(0);
        }
        Ok((digest, signature, false))
    }
}

/// The order a window would send the moment the makers' ladder shows — all the window may still buy, at the rule's price —
/// signed ahead (real orders; the windows at the makers' moment, in the per-second hours or near their start).
pub fn presign(t: &mut Taker, now: i64) {
    if t.settings.mode != "on" || t.wallet.is_none() || !t.fast || !t.block(0.0, None).is_empty() {
        return;
    }
    let windows: Vec<(String, W)> = t.track.iter().map(|(s, w)| (s.clone(), w.clone())).collect();
    for (slug, w) in windows {
        let Ok(b) = w.try_borrow() else { continue };
        if b.market.is_none() || t.phase(&b, now) != "before" || b.busy {
            continue;
        }
        let rule = b.rule();
        let limit = Decimal::from(t.settings.price(rule.key)) / Decimal::from(100);
        if limit <= Decimal::ZERO || !t.finished(&slug, rule, false).is_empty() {
            continue;
        }
        let rec = t.record(&slug, Some("on"));
        if rec.map(|r| open_orders(r).len()).unwrap_or(0) >= MAX_OPEN_ORDERS {
            continue;
        }
        let room = Decimal::from(t.settings.shares_of(rule.market)) - f64_dec(rec.map(held).unwrap_or(0.0));
        let count = rec.filter(|r| r.s("mode") == "on").map(|r| rec_orders(r).count()).unwrap_or(0);
        let take = t.clamp(room, limit, now);
        if take < Decimal::ONE || dec_f64(take * limit) < 1.0 {
            continue;
        }
        if let Some((order, _, info, _, _)) = t.order_for(&b, rule, take, limit, false, count) {
            drop(b);
            let before = t.signed.len();
            if t.sign(&order, info.neg_risk, info.yield_bearing).is_ok() && t.signed.len() > before {
                t.presigned += 1;
            }
        }
    }
}

/// One look at a window: its market found as Predict lists it; its book (pushed, or read); the offers at the rule's price;
/// matching's status read when there is something to buy and it has not opened; then the take.
/// The window's watch saved when its look ends — after the order it led to is out, never on its way.
struct WatchSave<'a> {
    me: &'a Me,
    slug: &'a str,
    w: &'a W,
    due: std::cell::Cell<bool>,
}

impl Drop for WatchSave<'_> {
    fn drop(&mut self) {
        if !self.due.get() {
            return;
        }
        if let (Ok(mut t), Ok(b)) = (self.me.try_borrow_mut(), self.w.try_borrow()) {
            let rule = b.rule();
            t.save_watch(self.slug, &b, rule);
        }
    }
}

pub async fn look(me: &Me, slug: &str, w: &W) -> Res<()> {
    let saver = WatchSave { me, slug, w, due: std::cell::Cell::new(false) };
    let watch_due = &saver.due;
    // -- the window's phase, whether it is due --
    let (rule, course, early, cents, settings, api) = {
        let mut held_t = me.borrow_mut();
        let t = &mut *held_t;
        let now = t.now();
        let rule = w.borrow().rule();
        let phase = t.phase(&w.borrow(), now);
        let course = phase == "live";
        if phase != "before" && !w.borrow().kept {
            w.borrow_mut().kept = true;
            t.keep_watch(slug, w);
        }
        if phase.is_empty() {
            if t.over(&w.borrow(), now) {
                t.track.shift_remove(slug);
                if let Some(mid) = w.borrow().mid() {
                    t.by_market.remove(&mid);
                }
            } else {
                w.borrow_mut().word = "开场前后这几秒不下单（等这一场的起点价）".into();
            }
            return Ok(());
        }
        let mut b = w.borrow_mut();
        if !t.due(&mut b, now) {
            return Ok(());
        }
        b.look_wall = now_ms();
        b.fresh = false;
        let indexed = std::mem::take(&mut b.indexed);
        b.index_wall = if course && (0..2_000).contains(&(b.look_wall - indexed)) { indexed } else { 0 };
        if let Some(mid) = b.mid() {
            b.fresh = t.stream.changed.borrow().contains(&mid) && indexed == 0;
            t.stream.changed.borrow_mut().remove(&mid);
            if !course && t.observe(&mut b, rule, now) {
                watch_due.set(true);
            }
        }
        let mut cents = if course { t.live_cents(rule) } else { t.settings.price(rule.key) };
        let early = course && now - b.start < LIVE_FULL_MS;
        if early {
            cents = cents.min(t.settings.price(rule.key));
        }
        let done = t.finished(slug, rule, course);
        if !done.is_empty() {
            b.word = done;
            return Ok(());
        }
        if t.settings.mode == "on" {
            let why = t.block(0.0, None);
            if !why.is_empty() {
                b.word = why;
                return Ok(());
            }
        }
        if b.market.is_none() {
            b.probe_ms = now;
            t.reads += 1;
        }
        (rule, course, early, cents, t.settings.clone(), t.api.clone())
    };
    // -- the market, as Predict lists it --
    if w.borrow().market.is_none() {
        let now = me.borrow().now();
        let Some(market) = api.category_market(slug).await? else {
            w.borrow_mut().word = "还没发布".into();
            return Ok(());
        };
        let mut t = me.borrow_mut();
        let mut b = w.borrow_mut();
        let question = if market.t("question") { market.s("question") } else { market.s("title") };
        let problem = rule.spec().title_problem(&question, b.start);
        if !problem.is_empty() {
            b.word = format!("市场标题和场次对不上：{problem}");
            b.probe_ms = now + 10 * 60_000;
            return Ok(());
        }
        if !book_up_first(&market) {
            b.word = "这个市场把“跌”列在第一个结果，盘口方向未确认，不做".into();
            b.probe_ms = now + 10 * 60_000;
            return Ok(());
        }
        drop(b);
        if !same(&t, slug, w) {
            return Ok(());
        }
        let mut b = w.borrow_mut();
        let open = market.s("tradingStatus").to_uppercase() == "OPEN";
        b.market = Some(market);
        b.listed_ms = now;
        b.status_ms = now;
        t.index_market(slug, &b);
        if open {
            b.open_ms = now;
        }
        let seen = b.watch();
        if seen.i("listed_ms") == 0 {
            seen.insert("listed_ms".into(), json!(now));
        }
        let ids: std::collections::BTreeSet<String> = t.by_market.keys().cloned().collect();
        t.stream.want(&ids); // subscribed now, not at the tick's end
    }
    // -- the book --
    let (mid, pushed, asked) = {
        let mut t = me.borrow_mut();
        let now = t.now();
        let mut b = w.borrow_mut();
        b.look_ms = now;
        if early && t.settings.mode == "paper" {
            b.word = "盘中：只记录模式从开场 1 分钟后按盘中的价格记一次（第 1 分钟的价格和盘前一样）".into();
            return Ok(());
        }
        if cents <= 0 {
            b.word = if early {
                "盘中：开场后第 1 分钟最高按盘前的价格买（盘前价格是 0，不买），1 分钟后按盘中的价格".into()
            } else {
                "盘前不买这条（盘前价格是 0），开场 1 分钟后按盘中的价格买".into()
            };
            return Ok(());
        }
        if course {
            drop(b);
            let why = t.live_guard(slug, rule, now);
            if !why.is_empty() {
                w.borrow_mut().word = format!("盘中：{why}");
                return Ok(());
            }
            b = w.borrow_mut();
        }
        let mid = b.mid().unwrap_or_default();
        let mut pushed = t.stream.book(&mid);
        let asked = mono();
        if t.fast {
            t.fast_looks += 1;
            if pushed.is_none() {
                let got = t.stream.held(&mid);
                if let Some(got) = got.filter(|g| t.stream.alive() || asked - g.2 <= FAST_STALE_S) {
                    pushed = Some((got.0, got.1));
                } else if asked - b.rest_at.unwrap_or(-1e9) < FAST_STALE_S {
                    b.word = "每秒检查：等盘口（推送还没送到，REST 轮流读取中）".into();
                    return Ok(());
                } else {
                    b.rest_at = Some(asked);
                }
            }
        }
        if pushed.is_none() {
            t.reads += 1;
        }
        (mid, pushed, asked)
    };
    let stream = me.borrow().stream.clone();
    let (bids, asks, from_push) = match pushed {
        Some((b, a)) => (b, a, true),
        None => {
            let (b, a) = api.orderbook(&mid).await?;
            match stream.held(&mid) {
                Some(got) if got.2 > asked => {
                    stream.changed.borrow_mut().remove(&mid);
                    (got.0, got.1, true) // a book was pushed while this read was out: the push is the newer
                }
                _ => {
                    let (b, a) = (Rc::new(b), Rc::new(a));
                    stream.merge(&mid, (*b).clone(), (*a).clone(), asked);
                    stream.changed.borrow_mut().remove(&mid);
                    (b, a, false)
                }
            }
        }
    };
    // -- the offers --
    let pending_read = {
        let mut held_t = me.borrow_mut();
        let t = &mut *held_t;
        let now = t.now();
        let mut b = w.borrow_mut();
        b.book_at = if from_push { t.stream.stamp(&mid).unwrap_or((0, 0)) } else { (0, now_ms()) };
        let limit = Decimal::from(cents) / Decimal::from(100);
        let mut levels = if limit > Decimal::ZERO { offered(&bids, &asks, rule.side, limit) } else { vec![] };
        if course && t.settings.mode == "paper" && !levels.is_empty() {
            if let Some(rec) = t.record(slug, None) {
                let bought: f64 = rec_orders(rec).filter(|o| o.t("paper") && !o.t("live")).map(|o| o.f("filled")).sum();
                levels = without(&levels, f64_dec(bought));
            }
        }
        b.offer = !levels.is_empty();
        if levels.is_empty() {
            b.rushed = false;
        }
        let best: Vec<Level> = offered(&bids, &asks, rule.side, Decimal::new(99, 2)).into_iter().take(1).collect();
        if !course && Taker::note_offers(&mut b, &levels, &best, now) {
            watch_due.set(true);
        }
        if levels.is_empty() {
            let shown = (limit * Decimal::from(100)).trunc();
            b.word = format!(
                "{}盘口没有 ≤{shown}¢ 的{}{}",
                if course { "盘中：" } else { "" },
                rule.word(),
                match best.first() {
                    Some((p, _)) => format!("（最低 {}¢）", g(dec_f64(*p) * 100.0)),
                    None => "（没人卖）".into(),
                }
            );
            return Ok(());
        }
        let mut read = false;
        if b.open_ms == 0 {
            let near = b.start - now <= NEAR_MS;
            if !b.rushed {
                b.rushed = true;
                drop(b);
                super::flow::rush(t, me, slug, w);
                b = w.borrow_mut();
            }
            let every = if near || t.fast { FAST_S } else { OFFER_STATUS_S };
            if (now - b.status_ms) as f64 >= every * 1000.0 {
                b.status_ms = now;
                t.reads += 1;
                read = true;
            }
        }
        (levels, limit, read)
    };
    let (levels, limit, read) = pending_read;
    if read {
        let market = api.market(&mid, false).await?;
        let now = me.borrow().now();
        let mut b = w.borrow_mut();
        let open = market.s("tradingStatus").to_uppercase() == "OPEN";
        let mut m = market;
        m.insert("categorySlug".into(), json!(slug));
        b.market = Some(m);
        if open && b.open_ms == 0 {
            b.open_ms = now;
            b.open_wall = now_ms();
            let seen = b.watch();
            if seen.i("open_ms") == 0 {
                seen.insert("open_ms".into(), json!(now));
            }
        }
    }
    // -- the take --
    let decided = {
        let t = me.borrow();
        let now = t.now();
        let pace = t.rush_pace();
        let mut b = w.borrow_mut();
        let there = total(&levels);
        if b.open_ms == 0 {
            let status = b.market.as_ref().map(|m| m.s("tradingStatus")).filter(|s| !s.is_empty()).unwrap_or_else(|| "?".into());
            b.word = format!(
                "盘口已有 ≤{}¢ 的{} {} 份，等撮合开启就买（现在 {status}{}）",
                (limit * Decimal::from(100)).trunc(),
                rule.word(),
                dg(there),
                if Taker::rushing(&b) { format!("，每 {} 秒读一次状态", g(pace)) } else { String::new() }
            );
            return Ok(());
        }
        let size = t.settings.shares_of(rule.market);
        let rec = t.record(slug, None);
        let room = Decimal::from(size) - f64_dec(rec.map(held).unwrap_or(0.0));
        let mut take = there.min(room);
        let cents_shown = (limit * Decimal::from(100)).trunc();
        if t.settings.mode == "on" {
            let out = rec.map(open_orders).unwrap_or_default();
            if out.len() >= MAX_OPEN_ORDERS {
                b.word = format!("这一场已有 {} 张单在途，等它们的结果", out.len());
                return Ok(());
            }
            let fresh = there - f64_dec(rec.map(claims).unwrap_or(0.0));
            if fresh < Decimal::ONE {
                b.word = format!("盘口 ≤{cents_shown}¢ 的{} {} 份，在途的单正在吃", rule.word(), dg(there));
                return Ok(());
            }
            let sweep =
                fresh >= Decimal::from(SWEEP_SHARES) && (fresh >= room / Decimal::from(SWEEP_PART) || t.sprinting(&b, now));
            take = if sweep { room } else { fresh.min(room) };
            if let Some(usd_left) = t.usd(None) {
                let afford = (f64_dec(usd_left.max(0.0)) / limit).floor();
                if afford < Decimal::ONE || dec_f64(afford * limit) < 1.0 {
                    b.word = format!("USDT 余额 ${} 不够一张最小订单", comma(usd_left, 2));
                    return Ok(());
                }
                take = take.min(afford);
            }
        }
        if t.settings.max_day_usd != 0.0 {
            let left = t.settings.max_day_usd - spent_on(t.recent_refs(), &day_of(now), &t.settings.mode);
            take = take.min(f64_dec(left.max(0.0)) / limit);
            if left < 1.0 {
                b.word = format!(
                    "今天已花 ${}，到了每天上限 ${}（RULE_MAX_DAY_USD）",
                    comma(t.settings.max_day_usd - left, 2),
                    g(t.settings.max_day_usd)
                );
                return Ok(());
            }
        }
        take = take.floor();
        if take < Decimal::ONE || dec_f64(take * limit) < 1.0 {
            b.word =
                format!("盘口 ≤{cents_shown}¢ 的{}只有 {} 份，不够一张最小订单（或额度、余额剩得不够）", rule.word(), dg(there));
            return Ok(());
        }
        if t.settings != settings {
            b.word = "设置刚改过：这一轮不下单，下一轮按新设置重新看".into();
            b.look_ms = 0;
            return Ok(());
        }
        (take, t.settings.mode == "paper")
    };
    let (take, is_paper) = decided;
    if is_paper {
        let mut t = me.borrow_mut();
        let mut b = w.borrow_mut();
        t.paper(slug, &mut b, rule, &levels, take, limit, course);
        return Ok(());
    }
    place(me, slug, w, rule, &levels, take, limit, course).await
}

#[allow(clippy::too_many_arguments)]
pub async fn place(
    me: &Me,
    slug: &str,
    w: &W,
    rule: &'static Rule,
    levels: &[Level],
    take: Decimal,
    limit: Decimal,
    course: bool,
) -> Res<()> {
    let usd = dec_f64(take * limit);
    let (settings, exchange, cached, api, read_now) = {
        let mut held_t = me.borrow_mut();
        let t = &mut *held_t;
        let why = t.block(usd, None);
        if !why.is_empty() {
            w.borrow_mut().word = why;
            return Ok(());
        }
        let b = w.borrow();
        let Some(market) = &b.market else { return Ok(()) };
        let info = parse_market(market, t.config.fee_bps);
        let (outcome, assumed) = outcome_for_side(rule.side, &info.outcomes);
        if outcome.is_none() || !assumed.is_empty() {
            drop(b);
            w.borrow_mut().word =
                format!("认不出{}的代币：{}", rule.word(), if assumed.is_empty() { "缺字段".to_string() } else { assumed });
            return Ok(());
        }
        let exchange = exchange_key(info.neg_risk, info.yield_bearing).to_string();
        let cached = t.allowance.get(&exchange).copied();
        let now = t.now();
        let mut read_now = false;
        if t.chain.is_some() {
            match cached {
                Some((at, amount)) if amount >= ALLOWANCE_UNLIMITED => {
                    if now - at > ALLOWANCE_LONG_MS && !crate::sync::running(&t.allowance_task) {
                        t.allowance_task = Some(spawn(super::flow::read_allowance_quietly(me.clone(), exchange.clone())));
                    }
                }
                Some((at, _)) if now - at <= ALLOWANCE_MS => {}
                _ => read_now = true,
            }
        }
        (t.settings.clone(), exchange, cached, t.api.clone(), read_now)
    };
    let cached = if read_now { Some(super::flow::read_allowance(me, &exchange).await?) } else { cached };
    if let Some((_, amount)) = cached {
        if amount < to_wei(usd) {
            w.borrow_mut().word =
                "交易所授权额度不足（在 Predict 网站用这个账户交易一次，或运行 huice 的 python main.py --approve）".into();
            return Ok(());
        }
    }
    api.ensure_auth().await?;
    // -- from here to the request started nothing is waited for: the day's cap, the balance and the window's shares are
    // counted again on the records as they stand now (another window's order may have gone out meanwhile)
    let send = {
        let mut held_t = me.borrow_mut();
        let t = &mut *held_t;
        if t.settings != settings {
            w.borrow_mut().word = "设置刚改过：这张单不下了，下一轮按新设置重新看".into();
            return Ok(());
        }
        let now = t.now();
        let mut b = w.borrow_mut();
        if t.phase(&b, now) != if course { "live" } else { "before" } || !same(t, slug, w) {
            b.word = "开场前 15 秒（盘中：结束前 15 秒）已到，这张单不下了".into();
            return Ok(());
        }
        let key = rid(slug, "on");
        let (bought, count) = t.recs.get(&key).map(|r| (held(r), rec_orders(r).count())).unwrap_or((0.0, 0));
        let room = Decimal::from(t.settings.shares_of(rule.market)) - f64_dec(bought);
        let take = t.clamp(take.min(room), limit, now);
        if take < Decimal::ONE || dec_f64(take * limit) < 1.0 {
            return Ok(());
        }
        let why = t.block(dec_f64(take * limit), None);
        if !why.is_empty() {
            b.word = why;
            return Ok(());
        }
        let Some((order, amounts, info, outcome, expires)) = t.order_for(&b, rule, take, limit, course, count) else {
            b.word = format!("认不出{}的代币", rule.word());
            return Ok(());
        };
        let (digest, signature, ahead) =
            t.sign(&order, info.neg_risk, info.yield_bearing).map_err(crate::util::RemoteError::new)?;
        if ahead {
            t.presign_hits += 1;
        }
        let (book_ts, seen_wall) = b.book_at;
        let hash = digest.to_lowercase();
        let mut o = Obj::new();
        o.insert("hash".into(), json!(hash));
        o.insert("id".into(), json!(""));
        o.insert("state".into(), json!("placing"));
        o.insert("shares".into(), jf(dec_f64(take)));
        o.insert("limit".into(), jf(dec_f64(limit)));
        o.insert("filled".into(), json!(0.0));
        o.insert("avg".into(), Value::Null);
        o.insert("fee_bps".into(), json!(info.fee_bps));
        o.insert("at".into(), json!(now));
        o.insert("expires".into(), json!(expires));
        o.insert("book".into(), levels_json(levels));
        o.insert("book_ts".into(), json!(book_ts));
        o.insert("seen_wall".into(), json!(seen_wall));
        o.insert("fresh".into(), json!(b.fresh));
        o.insert("look_wall".into(), json!(b.look_wall));
        o.insert("signed_wall".into(), json!(now_ms()));
        if ahead {
            o.insert("presigned".into(), json!(true));
        }
        if b.open_wall != 0 && b.rush_wall != 0 && !b.open_told {
            o.insert("open_wait".into(), json!(b.open_wall - b.rush_wall));
            o.insert("open_reads".into(), json!(b.rush_reads));
        }
        o.insert("seen".into(), jf(dec_f64(total(levels))));
        if course {
            o.insert("live".into(), json!(true));
            if b.index_wall != 0 {
                o.insert("index_wall".into(), json!(b.index_wall));
            }
        }
        b.open_told = true;
        if !t.recs.contains_key(&key) {
            let rec = t.new_record(slug, &b, rule, outcome.index_set, Some("on"));
            t.recs.insert(key.clone(), rec);
        }
        drop(b);
        let rec = t.recs.get_mut(&key).expect("the window's record");
        if rec.i("index_set") == 0 {
            rec.insert("index_set".into(), json!(outcome.index_set));
        }
        push_order(rec, o);
        t.sending.insert(hash.clone(), slug.to_string()); // (on its way from here: follow() leaves it to send(), which ends it)
        t.decided.insert(hash.clone(), settings);
        let body = Rc::new(order_body(&order, &signature, &digest, amounts.price_per_share));
        (key, hash, body, take)
    };
    // the request started within this very call: its bytes out before anything else gets a turn; the look does not wait for
    // the answer — another change of this window's book is looked at at once
    let (key, hash, body, take) = send;
    let task = start_now(send_order(me.clone(), slug.to_string(), w.clone(), rule, key.clone(), hash, body, take, limit));
    let mut t = me.borrow_mut();
    t.save_key(&key); // (the record written behind once the request is out: its journal line went before it)
    if let Some(task) = task {
        t.sends.push(task);
    }
    Ok(())
}

struct Sending(Me, String);

impl Drop for Sending {
    fn drop(&mut self) {
        if let Ok(mut t) = self.0.try_borrow_mut() {
            t.sending.remove(&self.1);
            t.decided.remove(&self.1);
        }
    }
}

/// One prepared order out: its hash on the disk first (a crash in between is found by it), then the request; Predict's answer
/// onto the record. The order is "on its way" (``sending`` — follow() leaves it alone) from the moment place() recorded it
/// until this is wholly done, its rest withdrawn when real trading stopped meanwhile.
#[allow(clippy::too_many_arguments)]
pub async fn send_order(
    me: Me,
    slug: String,
    w: W,
    rule: &'static Rule,
    key: String,
    hash: String,
    body: Rc<Vec<u8>>,
    take: Decimal,
    limit: Decimal,
) {
    let _sending = Sending(me.clone(), hash.clone());
    // -- the journal: the order's line on disk before its request leaves --
    let journalled = {
        let mut t = me.borrow_mut();
        let order = t.order_of(&key, &hash).cloned();
        order.map(|o| t.journal_write(&key, &o)).unwrap_or(false)
    };
    if !journalled {
        // no journal line (none open, or the disk refused it): the record itself must be on disk before the request leaves —
        // queued now, and the flush waits for it (a waiter counts what was queued when it was made)
        let (persistent, waiter) = {
            let mut t = me.borrow_mut();
            t.save_key(&key);
            let wait = t.flush_wait_s;
            (t.store.persistent(), t.store.flush_waiter(wait))
        };
        if persistent && !crate::store::flush_async(waiter).await {
            let mut t = me.borrow_mut();
            if let Some(o) = t.order_mut(&key, &hash) {
                o.insert("state".into(), json!("not_sent"));
                o.insert("error".into(), json!("数据库写入未完成，没有发单"));
            }
            t.save_key(&key);
            let name = t.recs.get(&key).map(|r| r.s("name")).unwrap_or_default();
            w.borrow_mut().word = "没下成：数据库写入未完成，没有发单".into();
            t.note(&format!("{name} 没下成：数据库写入未完成，没有发单"));
            return;
        }
    }
    let api = {
        let mut t = me.borrow_mut();
        t.sending.insert(hash.clone(), slug.clone());
        if let Some(o) = t.order_mut(&key, &hash) {
            o.insert("sent_wall".into(), json!(now_ms()));
        }
        t.api.clone()
    };
    let result = api.create_order(body, &hash, true).await;
    let (withdraw, state) = {
        let mut t = me.borrow_mut();
        let now = t.now();
        let halted = t.halted();
        t.decided.remove(&hash);
        let Some(o) = t.order_mut(&key, &hash) else { return };
        match result {
            Ok(created) => {
                o.insert("id".into(), json!(created.order_id));
                o.insert("state".into(), json!("open"));
                o.insert("next_ms".into(), json!(now + FILL_READ_MS));
            }
            Err(error) if error.kind == Kind::NotSent => {
                o.insert("state".into(), json!("not_sent"));
                o.insert("error".into(), json!(clean(&error)));
                o.shift_remove("sent_wall");
            }
            Err(error) => {
                let text = clean(&error);
                let status = error.status();
                let definite = error.kind == Kind::Rejected || (400..500).contains(&status);
                let lower = text.to_lowercase();
                if status == 429 || text.contains("限流冷却中") {
                    o.insert("state".into(), json!("not_sent"));
                    o.insert("error".into(), json!(text));
                } else if definite && status != 409 && !DUPLICATE_WORDS.iter().any(|w| lower.contains(w)) {
                    o.insert("state".into(), json!("rejected"));
                    o.insert("error".into(), json!(text));
                } else {
                    // a timeout, a lost answer, a bare 409: it may exist — looked for by its hash, never sent again
                    o.insert("state".into(), json!("unknown"));
                    o.insert("error".into(), json!(text));
                    o.insert("next_ms".into(), json!(now + if status == 409 { FILL_READ_MS } else { UNKNOWN_EVERY_MS }));
                    if status == 409 {
                        o.insert("conflict".into(), json!(true));
                    }
                }
            }
        }
        o.insert("answered_wall".into(), json!(now_ms()));
        let state = o.state().to_string();
        (state == "open" && o.t("id") && halted, state)
    };
    if withdraw {
        // real trading stopped while the request was out: its rest withdrawn now, by the id this answer brought
        super::orders::withdraw_one(&me, &key, &hash).await;
    }
    let mut t = me.borrow_mut();
    t.save_key(&key);
    let state_now = t.order_of(&key, &hash).map(|o| o.state().to_string()).unwrap_or(state);
    if state_now == "open" || state_now == "unknown" {
        drop(t);
        super::flow::wake_in(&me, FILL_READ_MS as f64 / 1000.0 + 0.01);
        t = me.borrow_mut();
    }
    let order = t.order_of(&key, &hash).cloned().unwrap_or_default();
    let live = if order.t("live") { "盘中 " } else { "" };
    let cents_shown = (limit * Decimal::from(100)).trunc();
    let word = match state_now.as_str() {
        "open" => format!("{live}已下单：{} ≤{cents_shown}¢ × {} 份", rule.word(), dg(take)),
        "unknown" => format!("{live}下单结果未知（{}），按哈希核对", order.s("error")),
        other => format!("{live}没下成：{}", if order.t("error") { order.s("error") } else { other.to_string() }),
    };
    let name = t.recs.get(&key).map(|r| r.s("name")).unwrap_or_default();
    let mut b = w.borrow_mut();
    b.word = word;
    if never_placed(&state_now) {
        let error = if order.t("error") { order.s("error") } else { state_now.clone() };
        drop(b);
        t.note(&format!("{name} 没下成：{error}"));
        let mut b = w.borrow_mut();
        let held_back = state_now == "not_sent" && RECHECKED.iter().any(|p| order.s("error").starts_with(p));
        b.held_back = if held_back { b.held_back + 1 } else { 0 };
        if !held_back || b.held_back >= HELD_BACK_MAX {
            b.held_back = 0;
            b.look_ms = t.now() + REJECT_PAUSE_MS;
        } else {
            b.look_ms = 0;
        }
    } else {
        b.held_back = 0;
        let trip = order.i("answered_wall") - if order.t("sent_wall") { order.i("sent_wall") } else { order.i("answered_wall") };
        crate::info!("规则吃单下单：{name} {} ≤{cents_shown}¢ × {} 份 → {state_now}（往返 {trip} ms）", rule.word(), dg(take));
    }
}
