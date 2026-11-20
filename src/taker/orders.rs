//! The orders after their answer: followed to Predict's final word (their rest withdrawn, the late fills taken), withdrawn at a
//! pause or a stop; and the windows settled on Predict's result.
use super::*;
use crate::predict::{DOWN_WORDS, Resolution, UP_WORDS, order_fill, order_final, order_price, predict_resolution, won_index_set};
use crate::rules::{cost, filled, in_flight, net};
use crate::util::{DUST, g, round_to};

/// The orders due a read, read (one by one: the answers are small, the key's budget is not).
pub async fn follow(me: &Me) {
    let due: Vec<(String, String)> = {
        let t = me.borrow();
        let now = t.now();
        let mut out = vec![];
        for (key, rec) in &t.recs {
            if rec.s("mode") != "on" || !in_flight(rec) {
                continue;
            }
            for o in rec_orders(rec) {
                if o.t("paper") || is_final(o.state()) || now < o.i("next_ms") || t.sending.contains_key(&o.s("hash")) {
                    continue;
                }
                out.push((key.clone(), o.s("hash")));
            }
        }
        out
    };
    for (key, hash) in due {
        let now = {
            let t = me.borrow();
            let Some(o) = t.order_of(&key, &hash) else { continue };
            let now = t.now();
            if is_final(o.state()) || now < o.i("next_ms") || t.cooling() > 0.0 || t.sending.contains_key(&hash) {
                continue;
            }
            now
        };
        if let Err(error) = follow_order(me, &key, &hash, now).await {
            let mut t = me.borrow_mut();
            if let Some(o) = t.order_mut(&key, &hash) {
                o.insert("error".into(), json!(clean(&error)));
                o.insert("next_ms".into(), json!(now + UNKNOWN_EVERY_MS));
            }
        }
        me.borrow_mut().save_key(&key);
    }
}

pub async fn follow_order(me: &Me, key: &str, hash: &str, now: i64) -> crate::util::Res<()> {
    let (api, id, state, late) = {
        let mut t = me.borrow_mut();
        t.reads += 1;
        let Some(o) = t.order_mut(key, hash) else { return Ok(()) };
        if o.state() == "placing" {
            // a request that left with the process gone before its answer: looked for by its hash
            o.insert("state".into(), json!("unknown"));
            o.insert("next_ms".into(), json!(now));
        }
        let state = o.state().to_string();
        let late = now >= o.i("expires") * 1000 + GIVE_UP_MS;
        let id = o.s("id");
        (t.api.clone(), id, state, late)
    };
    let row = api.order(&id, hash).await?;
    let to_remove: Option<String> = 'read: {
        let mut t = me.borrow_mut();
        let Some(row) = row else {
            let mut noted = None;
            if let Some(o) = t.order_mut(key, hash) {
                if late {
                    if state == "unknown" && !o.t("id") {
                        o.insert("state".into(), json!("not_found"));
                        o.insert("error".into(), json!("按哈希一直查不到：视为没下成"));
                    } else {
                        let text =
                            format!("到期后 {} 分钟仍查不到这张单：按已知成交 {} 份记", GIVE_UP_MS / 60_000, g(o.f("filled")));
                        o.insert("state".into(), json!("final"));
                        o.insert("error".into(), json!(text));
                    }
                } else if state == "unknown" && !o.t("id") && o.t("conflict") {
                    let misses = o.i("misses") + 1;
                    o.insert("misses".into(), json!(misses));
                    let at = if o.t("at") { o.i("at") } else { now };
                    if misses >= CONFLICT_MISSES && now - at >= CONFLICT_GRACE_MS {
                        o.insert("state".into(), json!("not_found"));
                        o.insert(
                            "error".into(),
                            json!(format!("Predict 答复 409，之后按哈希查了 {misses} 次都没有这张单：视为被拒、没下成")),
                        );
                        noted = Some(());
                    } else {
                        o.insert("next_ms".into(), json!(now + UNKNOWN_EVERY_MS));
                    }
                } else {
                    o.insert("next_ms".into(), json!(now + if state == "unknown" { UNKNOWN_EVERY_MS } else { FOLLOW_EVERY_MS }));
                }
            }
            if noted.is_some() {
                let name = t.recs.get(key).map(|r| if r.t("name") { r.s("name") } else { r.s("slug") }).unwrap_or_default();
                t.note(&format!("{name} 那张答复 409 的单按哈希查不到，视为没下成（这一场可以再买）"));
            }
            break 'read None;
        };
        {
            let Some(o) = t.order_mut(key, hash) else { break 'read None };
            let row_id = row.get("id").filter(|v| crate::util::truthy(v)).map(crate::util::py_str);
            let id = row_id.unwrap_or_else(|| o.s("id"));
            o.insert("id".into(), json!(id));
        }
        take_fills(&mut t, key, hash, &row);
        let Some(o) = t.order_mut(key, hash) else { break 'read None };
        let shares = o.f("shares");
        if o.f("filled") >= shares - DUST {
            o.insert("state".into(), json!("final"));
            break 'read None;
        }
        if order_final(&row, shares) || o.t("removed") {
            let left = if o.t("left_ms") { o.i("left_ms") } else { now };
            o.insert("left_ms".into(), json!(left));
            if now - left >= LATE_FILL_MS || late {
                o.insert("state".into(), json!("final"));
            } else {
                o.insert("state".into(), json!("closing"));
                o.insert("next_ms".into(), json!(left + LATE_FILL_MS));
            }
            break 'read None;
        }
        if late {
            o.insert("state".into(), json!("final"));
            o.insert("error".into(), json!("到期后仍显示挂着：按已知成交记"));
            break 'read None;
        }
        let id = o.s("id");
        if id.is_empty() {
            o.insert("next_ms".into(), json!(now + UNKNOWN_EVERY_MS));
            break 'read None;
        }
        Some(id)
    };
    let Some(id) = to_remove else { return Ok(()) };
    // still open: the rest withdrawn (a taker never waits on the book)
    let (removed, noop) = api.remove_orders(std::slice::from_ref(&id)).await?;
    let mut t = me.borrow_mut();
    if let Some(o) = t.order_mut(key, hash) {
        if removed.contains(&id) || noop.contains(&id) {
            o.insert("removed".into(), json!(now));
            o.insert("left_ms".into(), json!(now));
        }
        o.insert("state".into(), json!("cancelling"));
        o.insert("next_ms".into(), json!(now + FOLLOW_EVERY_MS));
    }
    Ok(())
}

pub async fn withdraw_one(me: &Me, key: &str, hash: &str) {
    let (api, id, now) = {
        let t = me.borrow();
        let Some(o) = t.order_of(key, hash) else { return };
        (t.api.clone(), o.s("id"), t.now())
    };
    let result = api.remove_orders(std::slice::from_ref(&id)).await;
    let mut t = me.borrow_mut();
    let Some(o) = t.order_mut(key, hash) else { return };
    match result {
        Err(error) => {
            o.insert("error".into(), json!(clean(&error)));
            o.insert("next_ms".into(), json!(0));
        }
        Ok((removed, noop)) => {
            // (noop: nothing left to withdraw — filled or expired already; either way off the book, as follow_order takes it)
            if removed.contains(&id) || noop.contains(&id) {
                o.insert("removed".into(), json!(now));
                o.insert("left_ms".into(), json!(now));
                o.insert("state".into(), json!("cancelling"));
                o.insert("next_ms".into(), json!(now + FOLLOW_EVERY_MS));
            } else {
                o.insert("next_ms".into(), json!(0));
            }
        }
    }
}

/// Every real order of this taker still on the book asked to be withdrawn now (the pause, a stop); one whose answer went
/// missing looked for by its hash first. The line for the reply; "" when there is no real order out.
pub async fn withdraw_all(me: &Me, why: &str) -> String {
    let (api, out): (Rc<PredictApi>, Vec<(String, String, String, String)>) = {
        let t = me.borrow();
        let mut out = vec![];
        for (key, rec) in &t.recs {
            if rec.s("mode") != "on" {
                continue;
            }
            for o in rec_orders(rec) {
                if !o.t("paper") && !is_final(o.state()) {
                    out.push((key.clone(), o.s("hash"), o.s("id"), o.state().to_string()));
                }
            }
        }
        (t.api.clone(), out)
    };
    if out.is_empty() {
        return String::new();
    }
    let lost: Vec<&(String, String, String, String)> =
        out.iter().filter(|(_, h, id, st)| id.is_empty() && st == "unknown" && !h.is_empty()).collect();
    if !lost.is_empty() {
        me.borrow_mut().reads += lost.len() as u64;
        let rows = futures_util::future::join_all(lost.iter().map(|(_, h, _, _)| api.order("", h))).await;
        let mut t = me.borrow_mut();
        for ((key, hash, _, _), row) in lost.iter().zip(rows) {
            if let Ok(Some(row)) = row {
                let rid_found = row.get("id").filter(|v| crate::util::truthy(v)).map(crate::util::py_str);
                if let (Some(found), Some(o)) = (rid_found, t.order_mut(key, hash)) {
                    if !o.t("id") {
                        o.insert("id".into(), json!(found));
                    }
                }
            }
        }
    }
    let resting: Vec<(String, String, String)> = {
        let t = me.borrow();
        out.iter()
            .filter_map(|(key, hash, _, _)| {
                let o = t.order_of(key, hash)?;
                (o.t("id") && !o.t("removed") && ["open", "cancelling", "unknown"].contains(&o.state()))
                    .then(|| (key.clone(), hash.clone(), o.s("id")))
            })
            .collect()
    };
    let mut done: HashSet<String> = HashSet::new();
    let mut error = String::new();
    if !resting.is_empty() {
        let ids: Vec<String> = resting.iter().map(|(_, _, id)| id.clone()).collect();
        match api.remove_orders(&ids).await {
            Ok((removed, noop)) => done = removed.into_iter().chain(noop).collect(),
            Err(e) => error = clean(&e),
        }
    }
    let mut t = me.borrow_mut();
    let now = t.now();
    let mut off = 0;
    for (key, hash, id) in &resting {
        let Some(o) = t.order_mut(key, hash) else { continue };
        if is_final(o.state()) || o.t("removed") || o.state() == "closing" {
            off += 1;
            continue;
        }
        if done.contains(id) {
            let left = if o.t("left_ms") { o.i("left_ms") } else { now };
            o.insert("removed".into(), json!(now));
            o.insert("left_ms".into(), json!(left));
            o.insert("state".into(), json!("cancelling"));
            o.insert("next_ms".into(), json!(now + FOLLOW_EVERY_MS));
            off += 1;
        } else {
            o.insert("next_ms".into(), json!(0));
        }
        t.save_key(key);
    }
    let unknown: Vec<(String, String)> = out
        .iter()
        .filter(|(key, hash, _, _)| t.order_of(key, hash).is_some_and(|o| !o.t("id") && o.state() == "unknown"))
        .map(|(k, h, _, _)| (k.clone(), h.clone()))
        .collect();
    let flying = out
        .iter()
        .filter(|(key, hash, _, _)| t.order_of(key, hash).is_some_and(|o| !o.t("id") && o.state() == "placing"))
        .count();
    for (key, hash) in &unknown {
        if let Some(o) = t.order_mut(key, hash) {
            o.insert("next_ms".into(), json!(0));
        }
        t.save_key(key);
    }
    let mut parts = vec![];
    if !resting.is_empty() {
        let missed = resting.len() - off;
        let mut line = format!("{} 张挂着的单已请求撤回，Predict 确认 {off} 张", resting.len());
        if missed > 0 {
            line +=
                &format!("，{missed} 张没撤成（{}），每秒再撤", if error.is_empty() { "Predict 的应答里没有它" } else { &error });
        }
        parts.push(line);
    }
    if !unknown.is_empty() {
        parts.push(format!("{} 张下单结果未知，按哈希现在查不到：接着核对，一查到就撤", unknown.len()));
    }
    if flying > 0 {
        parts.push(format!("{flying} 张还在发送途中，应答一到就撤"));
    }
    if parts.is_empty() {
        return String::new();
    }
    crate::warn!("规则吃单：{why}——{}", parts.join("；"));
    format!("规则吃单：{}。", parts.join("；"))
}

pub fn take_fills(t: &mut Taker, key: &str, hash: &str, row: &Obj) {
    let (rule_key, name, side) = {
        let Some(rec) = t.recs.get(key) else { return };
        (rec.s("rule"), rec.s("name"), rec.s("side"))
    };
    let now = t.now();
    let Some(o) = t.order_mut(key, hash) else { return };
    let (shares, _) = order_fill(row, o.f("shares"));
    if let Some(price) = order_price(row) {
        o.insert("avg".into(), jf(price));
        o.insert("avg_stated".into(), json!(true));
    }
    let before = o.f("filled");
    if shares > before + DUST {
        o.insert("filled".into(), jf(shares));
        o.insert("filled_ms".into(), json!(now)); // (the balance read before this cannot have seen it)
        let avg = if o.f("avg") != 0.0 { o.f("avg") } else { o.f("limit") };
        let live = o.t("live");
        let (total, limit) = (o.f("shares"), o.f("limit"));
        t.balance_ms = 0; // (the balance read again at once: the fill spent it)
        let rule = rule_of(&rule_key);
        t.notify(&format!(
            "🎯 规则吃单{}成交：{name} 买{} {:.1}¢ × {} 份（累计 {}/{}；{}{} ≤{}¢）",
            if live { "·盘中" } else { "" },
            rule.map(|r| r.word().to_string()).unwrap_or(side),
            avg * 100.0,
            g(shares - before),
            g(shares),
            g(total),
            rule.map(|r| r.name).unwrap_or(""),
            if live { "盘中" } else { "" },
            (limit * 100.0).round() as i64
        ));
    }
}

/// How often a window's settlement is read: every minute at first; still unsettled an hour past its end, every ten minutes;
/// a day past it, every hour (a market Predict is slow to settle, or settles in words not understood, costs a read an hour).
pub fn settle_every(since_end_ms: i64) -> i64 {
    if since_end_ms < 3_600_000 {
        SETTLE_EVERY_MS
    } else if since_end_ms < DAY_MS {
        10 * 60_000
    } else {
        3_600_000
    }
}

/// What a share the record holds paid at the settlement: by the outcome it bought (its indexSet) — the winning outcome's
/// name read only for a record without one (written before it was kept); a split pays half. None when it cannot be told.
pub fn paid(rec: &Obj, res: &Resolution, won: Option<i64>) -> Option<f64> {
    if res.split {
        return Some(0.5);
    }
    let held = rec.i("index_set");
    if let (true, Some(won)) = (held > 0, won) {
        return Some(if won == held { 1.0 } else { 0.0 });
    }
    let name = res.name.trim().to_lowercase();
    let up = if UP_WORDS.contains(&name.as_str()) {
        1.0
    } else if DOWN_WORDS.contains(&name.as_str()) {
        0.0
    } else {
        return None;
    };
    Some(if rec.s("side") == "up" { up } else { 1.0 - up })
}

pub async fn settle(me: &Me) {
    let due: Vec<String> = {
        let t = me.borrow();
        let now = t.now();
        t.recs
            .iter()
            .filter(|(_, r)| r.none("pnl") && !in_flight(r) && filled(r) > DUST)
            .filter(|(_, r)| {
                now >= r.i("end_ms") + SETTLE_AFTER_MS && now - r.i("settle_checked") >= settle_every(now - r.i("end_ms"))
            })
            .map(|(k, _)| k.clone())
            .collect()
    };
    for key in due {
        let (api, mid, now) = {
            let mut t = me.borrow_mut();
            if t.cooling() > 0.0 {
                return;
            }
            let now = t.now();
            t.reads += 1;
            let api = t.api.clone();
            let Some(rec) = t.recs.get_mut(&key) else { continue };
            rec.insert("settle_checked".into(), json!(now));
            (api, rec.s("market_id"), now)
        };
        let market = api.market(&mid, false).await;
        let mut t = me.borrow_mut();
        let (res, won) = match market {
            Err(e) => {
                if let Some(rec) = t.recs.get_mut(&key) {
                    rec.insert("settle_error".into(), json!(clean(&e)));
                }
                t.save_key(&key);
                continue;
            }
            Ok(m) => match predict_resolution(&m) {
                Some(res) => {
                    let won = won_index_set(&m, &res);
                    (res, won)
                }
                None => {
                    t.save_key(&key);
                    continue;
                }
            },
        };
        let Some(rec) = t.recs.get_mut(&key) else { continue };
        let Some(pay) = paid(rec, &res, won) else {
            let first = !rec.t("settle_alarmed");
            rec.insert("settle_error".into(), json!(format!("结果名称认不出：{}", res.name)));
            rec.insert("settle_alarmed".into(), json!(true));
            let name = if rec.t("name") { rec.s("name") } else { rec.s("slug") };
            t.save_key(&key);
            if first {
                t.alarm(&format!(
                    "⚠️ 规则吃单：{name} 已结算，但认不出结果（{}），这一场的盈亏算不出、不计入今天的亏损停机线：请到 Predict 网站核对",
                    res.name
                ));
            }
            continue;
        };
        let up = if res.split || rec.s("side") == "up" { pay } else { 1.0 - pay };
        let pnl = net(rec) * pay - cost(rec);
        rec.insert(
            "result".into(),
            json!(if up == 0.5 {
                "split"
            } else if up == 1.0 {
                "up"
            } else {
                "down"
            }),
        );
        rec.insert("pnl".into(), jf(round_to(pnl, 6)));
        rec.insert("settled_ms".into(), json!(now));
        rec.insert("settled_day".into(), json!(day_of(now)));
        let (mode, rule_key, side, bought, rec_name) = (rec.s("mode"), rec.s("rule"), rec.s("side"), filled(rec), rec.s("name"));
        t.pnl_epoch += 1;
        t.save_key(&key);
        let word = if pnl > DUST {
            "赢"
        } else if pnl < -DUST {
            "输"
        } else {
            "平"
        };
        let rule = rule_of(&rule_key);
        t.notify(&format!(
            "{} 规则吃单结算{}：{rec_name} 买{} {} 份，结果{}，{word} {}{}",
            if pnl > DUST {
                "✅"
            } else if pnl < -DUST {
                "❌"
            } else {
                "➖"
            },
            if mode == "paper" { "（只记录）" } else { "" },
            rule.map(|r| r.word().to_string()).unwrap_or(side),
            g(bought),
            if up == 1.0 {
                "涨"
            } else if up == 0.0 {
                "跌"
            } else {
                "平"
            },
            money(pnl),
            if mode == "on" && pay > 0.0 {
                "（赢的份额要在 Predict 领取：同一账户上 Railway 机器人开着自动领取会领，否则到网站上领）"
            } else {
                ""
            }
        ));
        if mode == "on" {
            if let Some(floor) = t.loss_floor() {
                let today = day_pnl(&t.recs, &day_of(now));
                if today <= floor {
                    t.hold_loss_stop();
                    t.alarm(&format!(
                        "⛔ 规则吃单今日已结算 {}，到了 RULE_MAX_DAILY_LOSS_USD 的停机线：今天不再开新单（明天自动恢复；/resume 从现在的盈亏起再给一个额度）",
                        money(today)
                    ));
                }
            }
        }
    }
}
