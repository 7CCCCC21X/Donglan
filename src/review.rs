//! The review page's numbers (复盘): each rule's totals, the real and the paper apart, what the watches saw, what was bought in
//! the windows' course, the races, the orders against the clock, the last windows, the result day by day with its running
//! total, and the records as CSV.
use crate::clock::day_spans_text;
use crate::config::RuleSettings;
use crate::rules::{
    RULES, Recs, avg_price, cost, course_part, course_stats, filled, live_price_of, live_rule, market_name, orders, races,
    rule_of, speed_stats, watch_stats,
};
use crate::util::{DAY_MS, DUST, Obj, Py, bj, day_of, jf, py_str, round_to};
use serde_json::{Value, json};
use std::collections::BTreeMap;

pub fn since_of(now_ms: i64, days: i64) -> i64 {
    if days != 0 { now_ms - days * DAY_MS } else { 0 }
}

/// Everything the review page shows for the last ``days`` days (0 = all), the windows counted by their start.
pub fn review(recs: &Recs, settings: &RuleSettings, now_ms: i64, days: i64) -> Value {
    let since = since_of(now_ms, days);
    let mine: Recs = recs.iter().filter(|(_, r)| r.i("start_ms") >= since).map(|(k, r)| (k.clone(), r.clone())).collect();
    let mut rows = vec![];
    for rule in RULES.iter() {
        for mode in ["on", "paper"] {
            let bought: Vec<&Obj> =
                mine.values().filter(|r| r.s("rule") == rule.key && r.s("mode") == mode && filled(r) > DUST).collect();
            let stats = watch_stats(&mine, rule.key, mode, since, settings.price(rule.key));
            if bought.is_empty()
                && stats.i("watched") == 0
                && (mode == "paper" || !(live_rule(settings, rule) || live_price_of(settings, rule) != 0))
            {
                continue;
            }
            let done: Vec<&Obj> = bought.iter().copied().filter(|r| !r.none("pnl")).collect();
            rows.push(json!({
                "rule": rule.key, "name": rule.name, "side": rule.side, "market": rule.market,
                "cents": if live_rule(settings, rule) { settings.price(rule.key) } else { 0 }, "when": rule.when,
                "live_cents": live_price_of(settings, rule), "course": Value::Object(course_stats(&mine, rule.key, mode, since)),
                "mode": mode, "windows": bought.len(), "shares": jf(round_to(bought.iter().map(|r| filled(r)).sum(), 4)),
                "cost": jf(round_to(bought.iter().map(|r| cost(r)).sum(), 4)), "settled": done.len(),
                "wins": done.iter().filter(|r| r.f("pnl") > DUST).count(), "losses": done.iter().filter(|r| r.f("pnl") < -DUST).count(),
                "pnl": jf(round_to(done.iter().map(|r| r.f("pnl")).sum(), 4)), "open": bought.len() - done.len(),
                "watch": Value::Object(stats),
            }));
        }
    }
    let mut last: Vec<&Obj> = mine.values().filter(|r| filled(r) > DUST).collect();
    last.sort_by_key(|r| -r.i("start_ms"));
    last.truncate(100);
    let mut race_rows: Vec<Value> = races(&mine, "on", since).into_iter().take(80).map(Value::Object).collect();
    race_rows.extend(races(&mine, "paper", since).into_iter().take(40).map(Value::Object));
    let markets: serde_json::Map<String, Value> = ["gold", "oil"]
        .iter()
        .map(|m| (m.to_string(), json!({"on": settings.on(m), "shares": settings.shares_of(m), "name": market_name(m)})))
        .collect();
    json!({
        "mode": settings.mode, "days": days, "since_ms": since, "markets": markets,
        "max_day_usd": jf(settings.max_day_usd), "fast": day_spans_text(&settings.fast), "lead_minutes": settings.lead_minutes,
        "totals": {"on": totals(&mine, "on"), "paper": totals(&mine, "paper")},
        "rules": rows, "speed": speed_stats(&mine, since).map(Value::Object).unwrap_or(Value::Null),
        "races": race_rows,
        "daily": {"on": daily(&mine, "on"), "paper": daily(&mine, "paper")},
        "recent": last.iter().map(|r| json!({
            "slug": r.get("slug"), "rule": r.get("rule"), "name": rule_of(&r.s("rule")).unwrap_or(&RULES[0]).name,
            "window": r.get("name"), "start_ms": r.get("start_ms"), "side": r.get("side"), "mode": r.get("mode"),
            "shares": jf(round_to(filled(r), 4)), "avg": jf(round_to(avg_price(r), 4)), "cost": jf(round_to(cost(r), 4)),
            "live": jf(round_to(course_part(r).0, 4)), "result": r.s("result"), "pnl": r.get("pnl").cloned().unwrap_or(Value::Null),
        })).collect::<Vec<_>>(),
    })
}

/// One mode's windows bought, shares, cost, settled P&L, wins and losses, and the windows still to settle.
pub fn totals(recs: &Recs, mode: &str) -> Value {
    let bought: Vec<&Obj> = recs.values().filter(|r| r.s("mode") == mode && filled(r) > DUST).collect();
    let done: Vec<&Obj> = bought.iter().copied().filter(|r| !r.none("pnl")).collect();
    let spent: f64 = done.iter().map(|r| cost(r)).sum();
    let pnl: f64 = done.iter().map(|r| r.f("pnl")).sum();
    json!({
        "windows": bought.len(), "shares": jf(round_to(bought.iter().map(|r| filled(r)).sum(), 4)),
        "cost": jf(round_to(bought.iter().map(|r| cost(r)).sum(), 4)), "settled": done.len(),
        "wins": done.iter().filter(|r| r.f("pnl") > DUST).count(), "losses": done.iter().filter(|r| r.f("pnl") < -DUST).count(),
        "pnl": jf(round_to(pnl, 4)), "roi": if spent > DUST { jf(round_to(pnl / spent, 4)) } else { Value::Null },
        "open": bought.len() - done.len(),
    })
}

/// The settled windows day by day (the Beijing date of their start), the oldest first, with the running total.
pub fn daily(recs: &Recs, mode: &str) -> Value {
    #[derive(Default)]
    struct Day {
        windows: i64,
        wins: i64,
        losses: i64,
        shares: f64,
        cost: f64,
        pnl: f64,
    }
    let mut days: BTreeMap<String, Day> = BTreeMap::new();
    for r in recs.values() {
        if r.s("mode") != mode || filled(r) <= DUST || r.none("pnl") {
            continue;
        }
        let d = days.entry(day_of(r.i("start_ms"))).or_default();
        let pnl = r.f("pnl");
        d.windows += 1;
        d.wins += (pnl > DUST) as i64;
        d.losses += (pnl < -DUST) as i64;
        d.shares += filled(r);
        d.cost += cost(r);
        d.pnl += pnl;
    }
    let mut run = 0.0;
    Value::Array(
        days.into_iter()
            .map(|(day, d)| {
                run += d.pnl;
                json!({"day": day, "windows": d.windows, "wins": d.wins, "losses": d.losses, "shares": jf(round_to(d.shares, 4)),
                       "cost": jf(round_to(d.cost, 4)), "pnl": jf(round_to(d.pnl, 4)), "cum": jf(round_to(run, 4))})
            })
            .collect(),
    )
}

fn csv_cell(text: &str) -> String {
    if text.contains([',', '"', '\n', '\r']) { format!("\"{}\"", text.replace('"', "\"\"")) } else { text.to_string() }
}

fn or_blank(v: Option<&Value>) -> String {
    match v {
        Some(v) if crate::util::truthy(v) => py_str(v),
        _ => String::new(),
    }
}

/// Every order of every window, one line each (the paper ones too), for a spreadsheet.
pub fn records_csv(recs: &Recs) -> String {
    let mut out = String::new();
    let mut row = |cells: Vec<String>| {
        out.push_str(&cells.iter().map(|c| csv_cell(c)).collect::<Vec<_>>().join(","));
        out.push_str("\r\n");
    };
    row([
        "开场（北京）",
        "场次",
        "规则",
        "模式",
        "买",
        "订单哈希",
        "状态",
        "限价",
        "份数",
        "成交",
        "均价",
        "手续费 bps",
        "盘中",
        "下单时刻（北京）",
        "推送延迟 ms",
        "到达→发出 ms",
        "往返 ms",
        "结果",
        "这一场盈亏",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect());
    let mut all: Vec<&Obj> = recs.values().collect();
    all.sort_by_key(|r| r.i("start_ms"));
    for r in all {
        let rule = rule_of(&r.s("rule"));
        for o in orders(r) {
            let pair = |a: &str, b: &str| if o.t(a) && o.t(b) { (o.i(a) - o.i(b)).to_string() } else { String::new() };
            row(vec![
                bj(r.i("start_ms"), true),
                if r.t("name") { r.s("name") } else { r.s("slug") },
                rule.map(|x| x.name.to_string()).unwrap_or_else(|| r.s("rule")),
                if r.s("mode") == "on" { "真实".into() } else { "只记录".into() },
                if r.s("side") == "up" { "涨".into() } else { "跌".into() },
                o.s("hash"),
                o.s("state"),
                or_blank(o.get("limit")),
                or_blank(o.get("shares")),
                o.get("filled").filter(|v| crate::util::truthy(v)).map(py_str).unwrap_or_else(|| "0".into()),
                or_blank(o.get("avg")),
                if o.none("fee_bps") { String::new() } else { py_str(&o["fee_bps"]) },
                if o.t("live") { "是".into() } else { String::new() },
                if o.t("at") { bj(o.i("at"), true) } else { String::new() },
                pair("seen_wall", "book_ts"),
                pair("sent_wall", "seen_wall"),
                pair("answered_wall", "sent_wall"),
                r.s("result"),
                if r.none("pnl") { String::new() } else { py_str(&r["pnl"]) },
            ]);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn days_and_csv() {
        let mut recs = Recs::new();
        let rec: Obj = serde_json::from_value(json!({"slug": "xau-updown-15m-1", "rule": "gold_shut", "mode": "on", "side": "up",
            "name": "黄金 15分涨跌 06:00–06:15", "start_ms": 1_763_500_000_000i64, "pnl": 4.5, "result": "up",
            "orders": [{"hash": "0xab", "state": "final", "shares": 10.0, "filled": 10.0, "limit": 0.55, "avg": 0.55, "fee_bps": 200,
                        "at": 1_763_499_000_000i64, "book_ts": 100, "seen_wall": 150, "sent_wall": 152, "answered_wall": 180}]})).unwrap();
        recs.insert("xau-updown-15m-1".into(), rec);
        let d = daily(&recs, "on");
        assert_eq!(d[0]["windows"], 1);
        assert_eq!(d[0]["cum"], 4.5);
        let t = totals(&recs, "on");
        assert_eq!(t["wins"], 1);
        let csv = records_csv(&recs);
        assert!(csv.contains(",0xab,final,0.55,10.0,10.0,0.55,200,,"), "{csv}");
        assert!(csv.contains(",50,2,28,up,4.5\r\n"), "{csv}");
    }
}
