//! The operator: the pause, the mode, the Telegram commands, the self-check and the order test, the control page's settings and
//! the review page's numbers, the control page's view of now.
use super::*;
use crate::clock::day_spans_text;
use crate::predict::Gate;
use crate::predict::{order_fill, outcome_for_side, parse_market};
use crate::rules::{RULES, live_rule, open_cost, speed_stats, spent_on};
use crate::util::{Res, bj, comma, fmt_amount, g, median, round_to};
use crate::wallet::{build_order, limit_amounts, order_body, to_wei};

pub const TEST_MAX_PRICE: &str = "0.02";
pub const TEST_USD: f64 = 1.0;
pub const TEST_SECONDS: i64 = 600;
/// an order test whose answer was lost: looked for by its hash this many times, this far apart
pub const TEST_LOOKUPS: usize = 4;
pub const TEST_LOOKUP_S: f64 = 1.5;

pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

pub async fn control(me: &Me, what: &str, arg: &str) -> String {
    match what {
        "check" => self_check(me).await,
        "test" => order_test(me).await,
        "pause" => {
            let paused = if arg.is_empty() { "手动暂停".to_string() } else { arg.to_string() };
            {
                let mut t = me.borrow_mut();
                t.paused = paused.clone();
                t.store.put("control:paused", &json!(paused));
            }
            let line = super::orders::withdraw_all(me, "暂停").await;
            let late = on_disk(me, "暂停", "恢复成没暂停（接着开新单）").await;
            joined(format!("⏸ 规则吃单已暂停：不再开新单（{paused}）"), &[line, late])
        }
        "resume" => {
            let reply = resume(me);
            let late = on_disk(me, "恢复", "仍是暂停").await;
            joined(reply, &[late])
        }
        "mode" => {
            let want = arg.trim().to_lowercase();
            if !["off", "paper", "on"].contains(&want.as_str()) {
                return "用法：/mode off | paper | on（不会高于部署变量 RULE_TAKER）".into();
            }
            let before = {
                let mut t = me.borrow_mut();
                if mode_rank(&want) > mode_rank(&t.env_mode) {
                    return format!(
                        "部署变量 RULE_TAKER={}，这里最多调到 {}（要真实吃单先改部署变量再重启）",
                        t.env_mode, t.env_mode
                    );
                }
                let before = t.settings.mode.clone();
                let settings = t.settings.with_mode(&want);
                t.configure(settings);
                let saved = if want != t.env_mode { want.clone() } else { String::new() };
                t.store.put("control:mode", &json!(saved));
                before
            };
            let line = if want != "on" {
                super::orders::withdraw_all(me, &format!("改成{}", mode_name(&want))).await
            } else {
                String::new()
            };
            let late = on_disk(me, "模式", &format!("回到“{}”", mode_name(&before))).await;
            joined(format!("规则吃单改成：{}", mode_name(&want)), &[line, late])
        }
        _ => "？".into(),
    }
}

/// A reply and the lines after it (the empty ones left out).
fn joined(head: String, extra: &[String]) -> String {
    let mut out = head;
    for line in extra.iter().filter(|l| !l.is_empty()) {
        out += &format!("\n{line}");
    }
    out
}

fn resume(me: &Me) -> String {
    {
        let mut t = me.borrow_mut();
        let mut parts = vec![];
        if !t.paused.is_empty() {
            t.paused.clear();
            t.store.put("control:paused", &json!(""));
            parts.push("暂停已解除".to_string());
        }
        let floor = t.loss_floor();
        let today = day_of(t.now());
        let pnl = day_pnl(&t.recs, &today);
        let held = t.loss_stop.s("day") == today;
        if held {
            t.loss_stop = Obj::new();
            t.store.put("control:loss_stop", &json!({}));
        }
        if let Some(floor) = floor {
            if pnl <= floor || held {
                let new_floor = pnl - t.loss_cap;
                // (the P&L now kept: the floor follows the cap should it change later today)
                t.loss_floor_at = crate::obj! {"day" => today, "base" => jf(pnl), "floor" => jf(new_floor)};
                let saved = Value::Object(t.loss_floor_at.clone());
                t.store.put("control:loss_floor", &saved);
                parts.push(format!(
                    "今日停机线移到 {}（从现在的 {} 起再亏 ${} 再停）",
                    money(new_floor),
                    money(pnl),
                    g(t.loss_cap)
                ));
            }
        }
        format!("▶️ {}｜现在：{}", if parts.is_empty() { "本来就没有暂停".to_string() } else { parts.join("；") }, t.mode_text())
    }
}

/// /set KEY VALUE: one setting changed, as the control page changes it (checked whole, in force at once, kept on disk).
async fn set_command(me: &Me, args: &str) -> String {
    let args = args.trim();
    let (key, value) = args.split_once(char::is_whitespace).map(|(k, v)| (k.trim(), v.trim())).unwrap_or((args, ""));
    if key.is_empty() || value.is_empty() {
        return "用法：/set 设置名 数值（例：/set RULE_MAX_DAILY_LOSS_USD 30）；设置名在 /settings 里点一项就能看到".into();
    }
    let mut values = serde_json::Map::new();
    values.insert(key.to_uppercase(), json!(value));
    match save_settings(me, &values).await {
        SaveResult::Done(m) => format!("✅ {m}"),
        SaveResult::NotOnDisk(m) => format!("⚠️ {m}"),
        SaveResult::Refused(m) => format!("❌ 没改：{m}"),
    }
}

/// /reset KEY: one setting back to the settings file's value.
async fn reset_command(me: &Me, args: &str) -> String {
    let key = args.trim().to_uppercase();
    if console::field(&key).is_none() {
        return "用法：/reset 设置名（回到设置文件里的值；设置名在 /settings 里点一项就能看到）".into();
    }
    match reset_settings(me, Some(&[key])).await {
        SaveResult::Done(m) => format!("✅ {m}"),
        SaveResult::NotOnDisk(m) => format!("⚠️ {m}"),
        SaveResult::Refused(m) => format!("❌ 没改：{m}"),
    }
}

/// /settings as text (where no buttons can be shown): each field's value and its name for /set.
fn settings_text(view: &Value) -> String {
    let mut lines = vec!["⚙️ 设置（改：/set 设置名 数值；回到设置文件的值：/reset 设置名）".to_string()];
    for group in view.as_array().map(Vec::as_slice).unwrap_or(&[]) {
        lines.push(group.s("group"));
        for f in group["fields"].as_array().map(Vec::as_slice).unwrap_or(&[]) {
            lines.push(format!("· {}：{}（{}）{}", f.s("label"), f.s("value"), f.s("key"), if f.t("saved") { " •" } else { "" }));
        }
    }
    lines.join("\n")
}

/// The review page's numbers as a message: the totals, each rule, the last days, the last windows.
pub fn review_text(data: &Value, days: i64) -> String {
    if data.t("busy") {
        return format!("📈 {}", data.s("busy"));
    }
    let span = if days == 0 { "全部".to_string() } else { format!("近 {days} 天") };
    let mut lines = vec![format!("📈 复盘（{span}，按开场时间算）")];
    let mut any = false;
    for (mode, name) in [("on", "真实"), ("paper", "只记录")] {
        let t = &data["totals"][mode];
        if t.i("windows") == 0 {
            continue;
        }
        any = true;
        let roi = t.get("roi").and_then(Value::as_f64).map(|r| format!("，收益率 {:+.1}%", r * 100.0)).unwrap_or_default();
        let open = if t.i("open") > 0 { format!("；还有 {} 场没结算", t.i("open")) } else { String::new() };
        lines.push(format!(
            "{name}：买了 {} 场 {} 份，花 ${:.2}；已结算 {} 场（赢 {} 输 {}），盈亏 {}{roi}{open}",
            t.i("windows"),
            g(t.f("shares")),
            t.f("cost"),
            t.i("settled"),
            t.i("wins"),
            t.i("losses"),
            money(t.f("pnl"))
        ));
    }
    if !any {
        lines.push("这段时间还没有买到过（真实和只记录都没有）".into());
    }
    let rules: Vec<&Value> =
        data["rules"].as_array().map(|r| r.iter().filter(|r| r.i("windows") > 0).collect()).unwrap_or_default();
    if !rules.is_empty() {
        lines.push("按规则：".into());
        for r in rules {
            lines.push(format!(
                "· {}（{}）：{} 场，赢 {} 输 {}，盈亏 {}",
                r.s("name"),
                if r.s("mode") == "on" { "真实" } else { "只记录" },
                r.i("windows"),
                r.i("wins"),
                r.i("losses"),
                money(r.f("pnl"))
            ));
        }
    }
    let daily: Vec<&Value> = data["daily"]["on"].as_array().map(|d| d.iter().rev().take(7).collect()).unwrap_or_default();
    if !daily.is_empty() {
        lines.push("最近几天（真实）：".into());
        for d in daily {
            lines.push(format!("  {}：{} 场 {}（累计 {}）", d.s("day"), d.i("windows"), money(d.f("pnl")), money(d.f("cum"))));
        }
    }
    let recent: Vec<&Value> = data["recent"].as_array().map(|r| r.iter().take(5).collect()).unwrap_or_default();
    if !recent.is_empty() {
        lines.push("最近几场：".into());
        for r in recent {
            let result = match r.s("result").as_str() {
                "up" => "涨",
                "down" => "跌",
                "split" => "平",
                _ => "待结算",
            };
            let pnl = if r.none("pnl") { String::new() } else { format!(" {}", money(r.f("pnl"))) };
            lines.push(format!(
                "  {} {}{} 买{} {} 份 均价 {:.1}¢ → {result}{pnl}",
                bj(r.i("start_ms"), true),
                r.s("window"),
                if r.s("mode") == "paper" { "（只记录）" } else { "" },
                if r.s("side") == "up" { "涨" } else { "跌" },
                g(r.f("shares")),
                r.f("avg") * 100.0
            ));
        }
    }
    lines.push("其他天数：/review 7、/review all；图表和 CSV 在网页控制台".into());
    lines.join("\n")
}

/// Waits out the race's seconds (RACE_WAIT_S at the most): what is worked out after it — the status text (milliseconds over a
/// long record), the CSV — holds the one thread the orders go out on, never while an offer may be there to take.
pub async fn after_the_race(me: &Me) {
    let began = mono();
    loop {
        let left = {
            let t = me.borrow();
            t.quiet_ms(t.now())
        };
        if left <= 0 || mono() - began >= RACE_WAIT_S {
            return;
        }
        crate::sync::sleep((left as f64 / 1000.0).min(0.25)).await;
    }
}

/// The admin's Telegram commands.
pub async fn command(me: &Me, command: &str, args: &str) -> String {
    match command {
        "/status" | "/rules" | "/start" => {
            after_the_race(me).await;
            me.borrow().status()
        }
        "/pause" => control(me, "pause", if args.is_empty() { "Telegram /pause" } else { args }).await,
        "/resume" => control(me, "resume", "").await,
        "/mode" => control(me, "mode", args).await,
        "/check" | "/selfcheck" => self_check(me).await,
        "/set" => set_command(me, args).await,
        "/reset" => reset_command(me, args).await,
        "/review" => {
            let days = match args.trim().to_lowercase().as_str() {
                "" => 30,
                "all" | "全部" => 0,
                d => d.parse::<i64>().unwrap_or(30).clamp(0, 3650),
            };
            let data = me.borrow_mut().review_data(days);
            review_text(&data, days)
        }
        "/settings" => settings_text(&me.borrow().settings_view()),
        "/test" => order_test(me).await,
        "/speed" => speed_test(me, args).await,
        _ => {
            "命令：/menu 按钮菜单｜/status 状态｜/check 自检｜/test 挂单测试｜/speed 测速｜/pause 暂停｜/resume 恢复｜/mode off|paper|on｜/settings 设置｜/set 设置名 数值｜/review 复盘｜/id"
                .into()
        }
    }
}

struct Report {
    lines: Vec<String>,
    bad: usize,
}

impl Report {
    fn ok(&mut self, text: impl Into<String>) {
        self.lines.push(format!("✅ {}", text.into()));
    }
    fn no(&mut self, text: impl Into<String>) {
        self.lines.push(format!("❌ {}", text.into()));
        self.bad += 1;
    }
    fn warn(&mut self, text: impl Into<String>) {
        self.lines.push(format!("⚠️ {}", text.into()));
    }
    fn either(&mut self, bad: bool, text: impl Into<String>) {
        if bad { self.no(text) } else { self.warn(text) }
    }
}

/// The key's use as Predict counts it (its rate-limit headers) against our own requests of the same minute: another program on
/// the same key shows as the difference (Railway's huice lending it out: RULE_KEY_LEND). (a warning?, the line); None without
/// Predict's figures.
pub fn key_budget_line(limits: &str, ours: usize) -> Option<(bool, String)> {
    let (limit, left) = crate::http::budget_of(limits)?;
    let used = (limit - left).max(0);
    let others = used - ours as i64;
    let text = format!("API Key 额度：Predict 记这一分钟用了 {used} 次（上限 {limit}），我们这一分钟发了 {ours} 次");
    if others > (limit / 10).max(30) {
        return Some((
            true,
            format!(
                "{text}：另外约 {others} 次是别的程序在用同一把 Key（多半是 Railway 上的 huice：它的主 Key 就是这把的话，\
                 这边换一把别的程序不用的——先 rt.sh set PREDICT_RATE_PER_MIN 220，再 rt.sh set RULE_API_KEY；\
                 是它的 RULE_API_KEY 借用这把的话，删掉那边的 RULE_API_KEY，或那边设 RULE_KEY_LEND=off）\
                 ——一撞上限流，这里所有请求停 20 秒以上，赶上抢单时刻就下不了单"
            ),
        ));
    }
    Some((false, text))
}

/// The last four characters of a key: enough to tell which one is meant, too few to use.
pub fn key_tail(key: &str) -> String {
    let chars: Vec<char> = key.chars().collect();
    chars[chars.len().saturating_sub(4)..].iter().collect()
}

fn short(addr: &str) -> String {
    if addr.len() < 10 { addr.to_string() } else { format!("{}…{}", &addr[..6], &addr[addr.len() - 4..]) }
}

/// Everything a real order needs, looked at now and said in one message, with a conclusion.
pub async fn self_check(me: &Me) -> String {
    let (now, s, region, env_mode, allowed, paused, api, wallet, chain, api_key) = {
        let t = me.borrow();
        (
            t.now(),
            t.settings.clone(),
            t.region.clone(),
            t.env_mode.clone(),
            t.config.trading_allowed,
            t.paused.clone(),
            t.api.clone(),
            t.wallet.clone(),
            t.chain.clone(),
            t.config.api_key.clone(),
        )
    };
    let mut r = Report {
        lines: vec![format!(
            "🔎 规则吃单自检 {}（{}，版本 {}）",
            bj(now, true),
            if region.is_empty() { "未知区域" } else { &region },
            version()
        )],
        bad: 0,
    };
    let (floor, today, stop) = {
        let t = me.borrow();
        (t.loss_floor(), t.today_pnl(), t.loss_stop_why())
    };
    r.lines.push(format!(
        "模式：{}（RULE_TAKER={env_mode}）｜LIVE_TRADING_ALLOWED {}｜{}｜今天已结算 {}{}",
        mode_name(&s.mode),
        if allowed { "允许" } else { "不是 on（不会真实下单）" },
        if paused.is_empty() { "没暂停".to_string() } else { format!("已暂停：{paused}") },
        money(today),
        floor.map(|f| format!("（停机线 {}）", money(f))).unwrap_or_else(|| "（不设日亏损停机）".into())
    ));
    let live = s.mode == "on";
    if live && !allowed {
        r.no("LIVE_TRADING_ALLOWED 不是 on：真实吃单模式也不会下单");
    }
    {
        let t = me.borrow();
        match (t.calendar_left(now), t.cal_last) {
            (Some(0), _) => r.either(live, t.calendar_why(now)),
            (Some(left), Some(last)) if left <= super::CALENDAR_WARN_DAYS => r.warn(format!(
                "CME 休市日历只排到 {last}（还剩 {left} 天）：到期前在设置文件补上之后的 CME_CLOSED_DAYS、CME_EARLY_DAYS，到期后不下单"
            )),
            (Some(_), Some(last)) => r.ok(format!("CME 休市日历排到 {last}")),
            _ => r.warn("CME 休市日历是空的（CME_CLOSED_DAYS、CME_EARLY_DAYS 都没写）：节假日也会照常下单"),
        }
    }
    if let Some(f) = floor {
        if live && !stop.is_empty() {
            r.no(format!("今天已到过日亏损停机线 {}：今天不再开新单（/resume 从现在的盈亏起再给一个额度）", money(f)));
        }
    }
    // where the key is kept: encrypted on this machine (deploy/gcp/vm-key.sh), or plain in the settings file
    let (kept, store) = {
        let t = me.borrow();
        (t.config.key_kept, t.config.key_store)
    };
    match &wallet {
        None if store => r.either(live, "没有设置 PREDICT_PRIVATE_KEY：只能 off / paper（设定私钥：bash deploy/gcp/rt.sh key）"),
        None => r.either(live, "没有设置 PREDICT_PRIVATE_KEY：只能 off / paper"),
        Some(wallet) => {
            match api.ensure_auth().await {
                Ok(()) => {
                    let exp = api.jwt_exp.get();
                    let mut t = me.borrow_mut();
                    t.ready_error.clear();
                    t.ready_ms = now;
                    r.ok(format!(
                        "Predict 登录：成功{}",
                        if exp != 0 { format!("（token 到 {}）", bj(exp * 1000, true)) } else { String::new() }
                    ));
                }
                Err(e) => {
                    let text = clean(&e);
                    me.borrow_mut().ready_error = text.clone();
                    r.no(format!("Predict 登录失败：{text}"));
                }
            }
            if !wallet.predict_account.is_empty() {
                super::flow::prepare(me).await;
                let t = me.borrow();
                if !t.account_error.is_empty() {
                    r.no(format!("账户：{}", t.account_error));
                } else if t.account_checked {
                    r.ok(format!("账户：Predict 账户 {}，私钥是它的控制钥匙（链上核对）", short(&wallet.maker)));
                } else {
                    r.no("账户：链上核对还没完成");
                }
            } else {
                r.ok(format!("账户：普通钱包 {}", short(&wallet.maker)));
            }
            match kept {
                "stored" => r.ok("私钥：加密保存在这台机器上，只在服务自己的内存里解开（设置文件里没有明文）"),
                "both" => r.warn(
                    "私钥：已加密保存，但设置文件里还留着同一把的明文：运行 bash deploy/gcp/rt.sh update（或 edit 存一次）就会收走",
                ),
                _ if store => {
                    r.warn("私钥：在设置文件里是明文：运行 bash deploy/gcp/rt.sh update 改成加密保存（只有这台机器能解开）")
                }
                _ => {} // (not run by systemd: a laptop, a test — nothing to keep it in)
            }
            super::flow::refresh_balance(me).await;
            {
                let t = me.borrow();
                match t.usdt {
                    None => r.no(format!(
                        "余额没读到：{}",
                        if t.balance_error.is_empty() { "链上节点没回答" } else { &t.balance_error }
                    )),
                    Some(usdt) => {
                        let need = RULES
                            .iter()
                            .filter(|rule| live_rule(&s, rule))
                            .map(|rule| s.shares_of(rule.market) as f64 * s.price(rule.key) as f64 / 100.0)
                            .fold(0.0, f64::max);
                        let usd = t.usd(None).unwrap_or(0.0);
                        let mut text = format!("余额：USDT {}（可用 {}）", comma(from_wei(usdt), 2), comma(usd, 2));
                        if let Some(bnb) = t.bnb {
                            text += &format!("｜签名钱包 BNB {:.4}", from_wei(bnb));
                        }
                        if need > 0.0 && usd < need {
                            r.warn(format!("{text}｜不够买满一场（最多要 ${}）", comma(need, 2)));
                        } else {
                            r.ok(text);
                        }
                    }
                }
            }
            if chain.is_some() {
                // the exchanges the markets followed now settle on (each market's isNegRisk × isYieldBearing, as an order
                // checks it): each must have the USDT allowance, or its orders are refused the moment they are due; with no
                // market followed yet, all four told (none of them failing the check)
                let used = exchanges_in_use(me);
                for key in crate::wallet::EXCHANGES {
                    if !used.is_empty() && !used.contains(key) {
                        continue;
                    }
                    let name = exchange_name(key);
                    match super::flow::read_allowance(me, key).await {
                        Ok((_, amount)) if amount >= 100_000_000_000_000_000_000 => r.ok(format!("授权：{name} USDT 额度已授权")),
                        Ok(_) if used.is_empty() => {
                            r.warn(format!("授权：{name} 没有 USDT 额度（现在没有跟踪的市场，用到它的市场才需要）"))
                        }
                        Ok(_) => {
                            r.no(format!("授权：{name} 没有 USDT 额度（跟踪的市场要用：在 Predict 网站用这个账户交易一次）"))
                        }
                        Err(e) => r.warn(format!("授权：{name} 没读到（{}）", clean(&e))),
                    }
                }
            }
            // a speed bump Predict puts on the takers of a market followed: the race is then decided at Predict, not here
            let delays: std::collections::BTreeSet<String> = me
                .borrow()
                .track
                .values()
                .filter_map(|w| {
                    let b = w.borrow();
                    let fields = crate::predict::delay_fields(b.market.as_ref()?);
                    (!fields.is_empty()).then(|| format!("{}：{}", b.rule().name, fields.join("、")))
                })
                .collect();
            if delays.is_empty() {
                r.ok("吃单延迟：跟踪的市场没有这类设置（speedBump / delay）");
            } else {
                r.warn(format!(
                    "Predict 对这些市场的吃单有延迟设置（吃单的单要在 Predict 那边等这么久才撮合）：{}",
                    delays.into_iter().collect::<Vec<_>>().join("；")
                ));
            }
            if api.usable() {
                let body = Rc::new(br#"{"data":{}}"#.to_vec());
                match api.authed("POST", "/orders", Some(body), crate::predict::API_TIMEOUT, true, None).await {
                    Ok(_) => r.warn("下单接口对空订单没有报错（意外），请用 /test 挂单测试确认"),
                    Err(e) => {
                        let status = e.status();
                        if status == 403 {
                            r.no(format!("下单接口 403：这个区域可能不允许交易（{}）", clean(&e)));
                        } else if status == 401 {
                            r.no(format!("下单接口 401：登录被拒（{}）", clean(&e)));
                        } else if (400..500).contains(&status) {
                            r.ok(format!(
                                "下单接口在这个区域可用（空订单被拒为无效：HTTP {status}，这是预期的；要连签名一起验证用 /test）"
                            ));
                        } else {
                            r.warn(format!("下单接口没回答清楚：{}", clean(&e)));
                        }
                    }
                }
            }
        }
    }
    if !api_key.is_empty() {
        let mid = {
            let t = me.borrow();
            let mut listed: Vec<(i64, String)> =
                t.track.values().filter_map(|w| w.borrow().mid().map(|m| (w.borrow().start, m))).collect();
            listed.sort();
            listed.first().map(|(_, m)| m.clone()).unwrap_or_default()
        };
        let began = mono();
        let got: Res<()> = if !mid.is_empty() {
            api.orderbook(&mid).await.map(|_| ())
        } else {
            api.category_market(&format!("btc-updown-15m-{}", (now - now % 900_000) / 1000)).await.map(|_| ())
        };
        match got {
            Ok(()) => {
                let took = (mono() - began) * 1000.0;
                let net = crate::http::http().net_of(&api.host);
                let text = format!(
                    "Predict 读取：一次往返 {took:.0} ms{}{}",
                    if net.edge.is_empty() { String::new() } else { format!("｜入口 {}", net.edge) },
                    if took < 150.0 { "" } else { "（偏慢：rt.sh probe 量一下别的区域）" }
                );
                if took < 150.0 {
                    r.ok(text)
                } else {
                    r.warn(text)
                }
                // (that read's answer brought the key's figures: is anyone else on it?)
                let ours = api.pacer.borrow().last_minute();
                if let Some((warn, line)) = key_budget_line(&net.limits, ours) {
                    if warn { r.warn(line) } else { r.ok(line) }
                }
            }
            Err(e) => r.no(format!("Predict 读取失败：{}", clean(&e))),
        }
    } else if s.mode != "off" {
        r.no("没有 API Key（RULE_API_KEY / PREDICT_API_KEY）");
    }
    // the spare keys: each asked once now (a read through it alone), so one Predict refuses is found here, not in a race
    if api.keys.len() > 1 {
        let path = format!("/categories/btc-updown-15m-{}", (now - now % 900_000) / 1000);
        let mut fine = vec![];
        let mut refused = vec![];
        for k in 1..api.keys.len() {
            let name = format!("第 {} 把（…{}）", k + 1, key_tail(&api.keys[k]));
            match api.try_key(k, &path).await {
                Ok(ms) => fine.push(format!("{name}可用 {ms:.0} ms")),
                Err(e) if e.status() == 404 => fine.push(format!("{name}可用")), // (that quarter not listed yet: the key passed)
                Err(e) => refused.push(format!("{name}：{}", clean(&e))),
            }
        }
        let switched = api.switches.borrow().iter().filter(|(at, _, _)| mono() - at < 86_400.0).count();
        let cooling: Vec<String> = (0..api.keys.len())
            .filter(|k| api.key_cooling(*k) > 0.0)
            .map(|k| format!("第 {} 把还要冷却 {} 秒", k + 1, api.key_cooling(k) as i64 + 1))
            .collect();
        let text = format!(
            "API Key：主用 + 备用 {} 把，现在用第 {} 把{}{}{}",
            api.keys.len() - 1,
            api.active.get() + 1,
            if fine.is_empty() { String::new() } else { format!("｜{}", fine.join("、")) },
            if cooling.is_empty() { String::new() } else { format!("｜{}", cooling.join("、")) },
            if switched > 0 { format!("｜最近一天被限流换了 {switched} 次 Key") } else { String::new() }
        );
        if refused.is_empty() {
            r.ok(text);
        } else {
            r.warn(text);
            r.no(format!("备用 API Key 被 Predict 拒绝：{}（rt.sh edit 改正或删掉这一行）", refused.join("；")));
        }
    }
    let limited = api.limited.borrow().iter().filter(|at| mono() - **at < 86_400.0).count();
    if limited > 0 {
        r.warn(format!(
            "最近一天被 Predict 限流（429）{limited} 次：每次这里所有请求停 {} 秒以上，期间不下新单",
            crate::predict::PredictApi::BACKOFF_SECONDS
        ));
    }
    let t = me.borrow();
    if !t.by_market.is_empty() {
        if t.stream.connected() && t.stream.alive() {
            let mut lags: Vec<f64> = t.stream.lags.borrow().iter().map(|x| *x as f64).collect();
            let lag = median(&mut lags);
            r.ok(format!(
                "推送：已连接（共连接过 {} 次）{}",
                t.stream.connects(),
                lag.map(|l| format!("，推送延迟中位 {l:.0} ms")).unwrap_or_default()
            ));
        } else {
            let error = t.stream.error();
            r.either(
                s.mode != "off",
                format!(
                    "推送没连上：{}（推送断开时靠 REST 轮流读，慢很多）",
                    if error.is_empty() { "连接中".to_string() } else { error }
                ),
            );
        }
    } else if s.mode != "off" {
        r.warn("推送：还没有已发布的规则场次要订阅（场次在开场前约 24 小时发布）");
    }
    if let Some(tg) = &t.telegram {
        let (fine, text) = tg.health();
        if fine {
            r.ok(text);
        } else {
            r.warn(text);
        }
    }
    if RULES.iter().any(|rule| t.live_cents(rule) > 0) {
        match &t.index {
            Some(feed) if feed.connected.get() => {
                let ages: Vec<String> = feed
                    .symbols
                    .iter()
                    .filter_map(|sym| {
                        let price = feed.price(sym)?;
                        let at = *feed.priced_ms.borrow().get(sym).unwrap_or(&0);
                        Some(format!(
                            "{sym} {}（{:.1} 秒前）",
                            fmt_amount(crate::rules::dec_f64(price)),
                            (now - at) as f64 / 1000.0
                        ))
                    })
                    .collect();
                r.ok(format!(
                    "币安指数：已连接{}",
                    if ages.is_empty() { String::new() } else { format!("，{}", ages.join("、")) }
                ));
            }
            feed => {
                let error = feed.as_ref().map(|f| f.error.borrow().clone()).unwrap_or_default();
                r.no(format!(
                    "币安指数没连上：盘中吃单不会买{}",
                    if error.is_empty() { String::new() } else { format!("（{error}）") }
                ));
            }
        }
    }
    let error = t.store.error();
    if !error.is_empty() {
        r.no(format!("记录：{error}（数据库写不进去时不开新单）"));
    } else {
        r.ok(format!(
            "记录：数据库正常{}",
            if t.journal.is_some() {
                "，订单日志已打开"
            } else {
                "（订单日志没打开：发单前改为等数据库写完，慢一点）"
            }
        ));
    }
    let listed = t.track.values().filter(|w| w.borrow().market.is_some()).count();
    let soonest = t.track.values().min_by_key(|w| w.borrow().start);
    if s.mode == "off" {
        r.warn("规则场次：模式是关，不跟踪场次");
    } else if let Some(w) = soonest {
        let b = w.borrow();
        r.ok(format!(
            "规则场次：接下来一天 {} 场，已找到市场 {listed} 场；下一场 {} {}{}",
            t.track.len(),
            bj(b.start, true),
            b.rule().name,
            if b.word.is_empty() { String::new() } else { format!("（{}）", b.word) }
        ));
    } else {
        r.warn("规则场次：接下来一天没有开着的规则场次（价格都是 0、市场关了，或者是周末 / 节假日）");
    }
    if !t.settings_dropped.is_empty() {
        r.warn(format!("启动时删掉了和设置文件冲突的网页设置：{}", t.settings_dropped.join("、")));
    }
    if !t.errors.is_empty() {
        r.lines.push("最近问题：".into());
        let n = t.errors.len();
        for (at, text) in t.errors.iter().skip(n.saturating_sub(5)) {
            r.lines.push(format!("  {} {text}", bj(*at, true)));
        }
    }
    if r.bad > 0 {
        r.lines.push("结论：❌ 先解决上面 ❌ 的几项".into());
    } else if s.mode == "on" {
        r.lines.push(format!("结论：✅ 可以真实吃单{}", if paused.is_empty() { "" } else { "（但现在暂停着）" }));
    } else {
        r.lines.push(format!("结论：✅ 没发现问题（现在是{}；真实吃单前可以发 /test 做一次挂单测试）", mode_name(&s.mode)));
    }
    r.lines.join("\n")
}

/// The exchanges (wallet::EXCHANGES keys) the markets followed now settle on.
pub fn exchanges_in_use(me: &Me) -> std::collections::BTreeSet<&'static str> {
    let t = me.borrow();
    let fee = t.config.fee_bps;
    t.track
        .values()
        .filter_map(|w| {
            let w = w.borrow();
            let info = parse_market(w.market.as_ref()?, fee);
            Some(crate::wallet::exchange_key(info.neg_risk, info.yield_bearing))
        })
        .collect()
}

pub fn exchange_name(key: &str) -> &'static str {
    match key {
        "CTF_EXCHANGE" => "标准交易所",
        "NEG_RISK_CTF_EXCHANGE" => "多结果交易所",
        "YIELD_BEARING_CTF_EXCHANGE" => "收益型交易所",
        "YIELD_BEARING_NEG_RISK_CTF_EXCHANGE" => "收益型多结果交易所",
        _ => "交易所",
    }
}

/// The real order path end to end with about a dollar for a few seconds: a LIMIT buy of Up far under the market (2¢ at the
/// most, a cent under the best bid, never at or above the best ask: it rests), on a rule window whose matching is open — else
/// the BTC quarter hour running now; read back by its hash, withdrawn, read again. Each step with Predict's answer and its
/// round trip. Signed to expire in ten minutes; no rule order (nothing follows it but this). One at a time: asked again
/// while one is under way (Telegram, the console, both), the second is told so and places nothing.
pub async fn order_test(me: &Me) -> String {
    if std::mem::replace(&mut me.borrow_mut().testing, true) {
        return "⏳ 挂单测试正在进行，等它的结果（同一时间只做一张，这次没有再挂）".into();
    }
    let _done = TestUnderWay(me.clone());
    run_order_test(me).await
}

/// How close to a race (a per-second stretch, the makers' moment, a window's last minutes) /speed still runs: it takes a quarter
/// of the key's budget for about a minute.
pub const SPEED_CLEAR_MS: i64 = 3 * 60_000;
/// Review spans kept worked out (review_data): the page's and /review's few.
pub const REVIEW_HELD: usize = 8;

/// /speed: the measurement ``ruletaker latency`` makes, run here (read-only, about a minute) and its report sent back — the push
/// delay, the REST reads against the pushes, Binance's index, the key's figures — and the service's own push connections beside
/// it. With a rate (/speed 4000) the REST reads take a quarter of that, this once. Never in a race or within SPEED_CLEAR_MS of
/// one (the key's budget is the race's); one at a time.
pub async fn speed_test(me: &Me, args: &str) -> String {
    let (mut config, busy) = {
        let t = me.borrow();
        (t.config.clone(), t.busy_in(t.now()))
    };
    if let Some(ms) = busy.filter(|ms| *ms < SPEED_CLEAR_MS) {
        return if ms <= 0 {
            "⏳ 正在抢单时段：不测速（它要占这把 Key 四分之一的额度），过了再发 /speed".into()
        } else {
            format!("⏳ {} 分钟后就是抢单时段：不测速（它要占这把 Key 四分之一的额度），过了再发 /speed", ms / 60_000 + 1)
        };
    }
    let rate = args.trim();
    if !rate.is_empty() {
        match rate.parse::<f64>() {
            Ok(r) if (1.0..=6000.0).contains(&r) => config.rate_per_min = r,
            _ => return "用法：/speed，或 /speed 4000（4000 = 这次测量用的每分钟请求数，1～6000；设置不动）".into(),
        }
    }
    {
        let t = me.borrow();
        if t.stop.is_set() {
            return "⏳ 服务正在停止：不测速".into();
        }
        // (the measurement goes through the main key: never while it cools — it would be refused, and its refusals add on)
        let left = t.api.key_cooling(0).max(t.cooling());
        if left > 0.0 {
            return format!("⏳ 这把 Key 正在限流冷却（还要 {} 秒）：不测速，过了再发 /speed", left as u64 + 1);
        }
    }
    if std::mem::replace(&mut me.borrow_mut().speeding, true) {
        return "⏳ 测速正在进行，等它的结果（同一时间只测一次）".into();
    }
    let _done = SpeedUnderWay(me.clone());
    let say = crate::Say::keeping();
    let base = crate::predict::api_base(config.chain_id).to_string();
    let found = crate::probe(config, 10, &base, crate::stream::PREDICT_WS_URL, crate::PUSH_SECONDS, &say).await;
    let mut lines = say.lines.take();
    if found.limited > 0 {
        // its requests went through the main key outside the service's pace: a 429 on them is that key's budget spent — the
        // service's own requests kept off it for the cooldown, as a 429 of theirs would
        me.borrow().api.cool_key(0);
        lines.push(format!(
            "⚠️ 测速的请求被 Predict 限流 {} 次（429）：这把 Key 先冷却 {} 秒，服务的请求这期间走备用 Key（没有备用 Key 就等）——测速别用超过这把 Key 额度的速率",
            found.limited,
            crate::predict::PredictApi::BACKOFF_SECONDS
        ));
    }
    // the service's own pushes beside it: what its extra connections save
    let hedge = me.borrow().stream.hedge();
    lines.push(match hedge {
        None => "服务自己的推送：1 条连接（RULE_WS_CONNECTIONS=3 开 3 条，谁先送到用谁的；开了以后这里写省了多少）".into(),
        Some((firsts, None, _)) => format!("服务自己的推送：{} 条连接，还没有可比的盘口", firsts.len()),
        Some((firsts, Some(behind), copies)) => {
            let each: Vec<String> = firsts.iter().map(u64::to_string).collect();
            format!(
                "服务自己的推送：{} 条连接，各自先到 {} 次，同一份盘口后到的比先到的晚 中位 {behind:.1} ms（{copies} 次；这就是多开连接省下的）",
                firsts.len(),
                each.join("/")
            )
        }
    });
    format!("📶 测速结果\n{}", lines.join("\n"))
}

/// The speed test's mark, taken off however it ends.
struct SpeedUnderWay(Me);

impl Drop for SpeedUnderWay {
    fn drop(&mut self) {
        if let Ok(mut t) = self.0.try_borrow_mut() {
            t.speeding = false;
        }
    }
}

/// The order test's mark, taken off however the test ends (its answer, an early return, the task dropped).
struct TestUnderWay(Me);

impl Drop for TestUnderWay {
    fn drop(&mut self) {
        if let Ok(mut t) = self.0.try_borrow_mut() {
            t.testing = false;
        }
    }
}

async fn run_order_test(me: &Me) -> String {
    let (now, wallet, api, stop, fee) = {
        let t = me.borrow();
        (t.now(), t.wallet.clone(), t.api.clone(), t.stop.clone(), t.config.fee_bps)
    };
    let Some(wallet) = wallet else { return "❌ 没有设置 PREDICT_PRIVATE_KEY，做不了挂单测试".into() };
    if stop.is_set() {
        return "服务正在停止".into();
    }
    // the test is a real order: the deploy's hard switch and the pause hold it as they hold every order
    let why = me.borrow().test_gate();
    if !why.is_empty() {
        return format!("❌ 挂单测试没做：{why}");
    }
    if let Err(e) = api.ensure_auth().await {
        return format!("❌ Predict 登录失败：{}", clean(&e));
    }
    let mut markets: Vec<(String, Obj)> = {
        let t = me.borrow();
        let mut list: Vec<(i64, String, Obj)> =
            t.track.iter().filter_map(|(s, w)| w.borrow().market.clone().map(|m| (w.borrow().start, s.clone(), m))).collect();
        list.sort_by_key(|x| x.0);
        list.into_iter().map(|(_, s, m)| (s, m)).collect()
    };
    let btc = format!("btc-updown-15m-{}", (now - now % 900_000) / 1000);
    if let Ok(Some(found)) = api.category_market(&btc).await {
        markets.push((btc, found));
    }
    let mut tried: Vec<String> = vec![];
    for (slug, market) in markets.into_iter().take(4) {
        let mid = market.s("id");
        let mut market = market;
        match api.market(&mid, false).await {
            Ok(details) => {
                for (k, v) in details {
                    market.insert(k, v);
                }
            }
            Err(e) => {
                tried.push(format!("{slug}：读不到详情（{}）", clean(&e)));
                continue;
            }
        }
        if market.s("tradingStatus").to_uppercase() != "OPEN" {
            let status = if market.t("tradingStatus") { market.s("tradingStatus") } else { "?".into() };
            tried.push(format!("{slug}：撮合没开启（{status}）"));
            continue;
        }
        let info = parse_market(&market, fee);
        let (outcome, assumed) = outcome_for_side("up", &info.outcomes);
        let (Some(outcome), true) = (outcome, assumed.is_empty()) else {
            tried.push(format!("{slug}：认不出涨的代币"));
            continue;
        };
        let (bids, asks) = match api.orderbook(&info.market_id).await {
            Ok(b) => b,
            Err(e) => {
                tried.push(format!("{slug}：读不到盘口（{}）", clean(&e)));
                continue;
            }
        };
        let cent = Decimal::new(1, 2);
        let best_bid = bids.first().map(|l| l.0).unwrap_or(Decimal::ZERO);
        let best_ask = asks.first().map(|l| l.0).unwrap_or(Decimal::ONE);
        let max_price = Decimal::from_str_exact(TEST_MAX_PRICE).unwrap_or(cent);
        let price = cent.max(max_price.min(best_bid - cent));
        if price >= best_ask {
            tried.push(format!("{slug}：最低卖价 {}¢，放不下一张不成交的测试单", g(crate::rules::dec_f64(best_ask) * 100.0)));
            continue;
        }
        let pf = crate::rules::dec_f64(price);
        let shares = (TEST_USD / pf).ceil();
        let Ok(amounts) = limit_amounts(true, to_wei(pf), to_wei(shares)) else {
            tried.push(format!("{slug}：测试单的数量算不出来"));
            continue;
        };
        let salt = (crate::wallet::keccak(format!("test|{now}|{}", info.market_id).as_bytes())[0] as u64) << 24
            | (now as u64 & 0xff_ffff);
        let order = build_order(
            &wallet,
            &outcome.token,
            amounts.maker,
            amounts.taker,
            info.fee_bps,
            now / 1000 + TEST_SECONDS,
            salt % crate::wallet::MAX_SALT,
        );
        let (digest, signature) = match wallet.sign_order(&order, info.neg_risk, info.yield_bearing) {
            Ok(x) => x,
            Err(e) => return format!("❌ 签名失败：{e}"),
        };
        let body = Rc::new(order_body(&order, &signature, &digest, amounts.price_per_share));
        let name = if market.t("question") {
            market.s("question")
        } else if market.t("title") {
            market.s("title")
        } else {
            slug.clone()
        };
        let mut lines = vec![format!(
            "🧪 挂单测试 {}：{name}（市场 {}）买涨 {}¢ × {} 份，最多花 ${:.2}，{} 分钟后自动过期；盘口最高买价 {}¢、最低卖价 {}¢",
            bj(now, true),
            info.market_id,
            g(pf * 100.0),
            g(shares),
            from_wei(amounts.maker),
            TEST_SECONDS / 60,
            g(crate::rules::dec_f64(best_bid) * 100.0),
            g(crate::rules::dec_f64(best_ask) * 100.0)
        )];
        let hash = digest.to_lowercase();
        let test_key = format!("test:{hash}");
        let mut record = crate::obj! {
            "at" => now, "slug" => slug.clone(), "market_id" => info.market_id.clone(), "price" => jf(pf), "shares" => jf(shares),
            "hash" => hash.clone(), "steps" => json!(lines.clone()),
        };
        me.borrow_mut().store.put(&test_key, &Value::Object(record.clone()));
        let began = mono();
        // (judged again right before its bytes leave, as a rule order is: the switch, the pause, the stop)
        let gate: Gate = {
            let weak = Rc::downgrade(me);
            Rc::new(move || match weak.upgrade() {
                Some(me) => me.try_borrow().map(|t| t.test_gate()).unwrap_or_else(|_| "内部状态正忙，测试单不发了".into()),
                None => "服务正在停止，不再发单".into(),
            })
        };
        let oid = match api.create_order(body, Some(gate)).await {
            Ok(created) => {
                lines.push(format!("1/4 下单成功：订单 #{}（往返 {:.0} ms）", created.order_id, (mono() - began) * 1000.0));
                created.order_id
            }
            Err(e) if super::look::sent_or_not(&e) != super::look::Placed::Maybe => {
                lines.push(format!("1/4 下单失败（{:.0} ms）：{}", (mono() - began) * 1000.0, clean(&e)));
                record.insert("steps".into(), json!(lines.clone()));
                me.borrow_mut().store.put(&test_key, &Value::Object(record));
                lines.push(format!(
                    "结论：❌ 这张测试单没下成：看上面 Predict 的说法{}",
                    if e.status() == 403 { "（403 = 这个区域不能交易）" } else { "" }
                ));
                return lines.join("\n");
            }
            Err(e) => {
                // the answer lost on the way (a timeout, a 5xx, a 409): Predict may hold the order — looked for by its hash,
                // and withdrawn by the id it shows, as a rule order is
                lines.push(format!(
                    "1/4 下单的应答没收到（{:.0} ms）：{}——Predict 可能已经收下了，按哈希核对",
                    (mono() - began) * 1000.0,
                    clean(&e)
                ));
                let mut found = String::new();
                for attempt in 0..TEST_LOOKUPS {
                    if attempt > 0 {
                        let pause = me.borrow().lookup_s;
                        crate::sync::sleep(pause).await;
                    }
                    if let Ok(Some(row)) = api.order("", &digest).await {
                        found = row.get("id").filter(|v| crate::util::truthy(v)).map(crate::util::py_str).unwrap_or_default();
                        if !found.is_empty() {
                            break;
                        }
                    }
                }
                if found.is_empty() {
                    lines.push(format!(
                        "结论：⚠️ 下单结果未知：按哈希查了 {TEST_LOOKUPS} 次都没有这张单（多半没下成）；它签的是 {} 分钟后过期，万一挂上了也会自己失效——到 Predict 网站的订单里核对一下",
                        TEST_SECONDS / 60
                    ));
                    record.insert("steps".into(), json!(lines.clone()));
                    record.insert("unknown".into(), json!(true));
                    me.borrow_mut().store.put(&test_key, &Value::Object(record));
                    return lines.join("\n");
                }
                lines.push(format!("1/4 按哈希找到了：订单 #{found}（Predict 收下了，只是应答没送到）"));
                found
            }
        };
        let began = mono();
        let mut filled_seen = 0.0_f64; // (fills only grow: the most any step read is the order's)
        match api.order(&oid, &digest).await {
            Ok(Some(row)) => {
                let (got, status) = order_fill(&row, shares);
                filled_seen = got;
                lines.push(format!(
                    "2/4 按哈希查到：{}，成交 {}/{}（往返 {:.0} ms）",
                    if status.is_empty() { "?" } else { &status },
                    g(got),
                    g(shares),
                    (mono() - began) * 1000.0
                ));
            }
            Ok(None) => lines
                .push(format!("2/4 按哈希暂时查不到（往返 {:.0} ms）：Predict 的记录可能晚一点出现", (mono() - began) * 1000.0)),
            Err(e) => lines.push(format!("2/4 查询失败：{}", clean(&e))),
        }
        let began = mono();
        let mut removed = false;
        match api.remove_orders(std::slice::from_ref(&oid)).await {
            Ok((gone, noop)) => {
                removed = gone.contains(&oid) || noop.contains(&oid);
                lines.push(if removed {
                    format!("3/4 撤单成功（往返 {:.0} ms）", (mono() - began) * 1000.0)
                } else {
                    format!(
                        "3/4 撤单请求发出，Predict 的应答里没有它（往返 {:.0} ms；应答：{}）",
                        (mono() - began) * 1000.0,
                        api.last_remove.borrow()
                    )
                });
            }
            Err(e) => lines.push(format!("3/4 撤单失败：{}（它 {} 分钟后自己过期）", clean(&e), TEST_SECONDS / 60)),
        }
        if !removed {
            // the answer did not name it: the order itself says whether it is gone (a cancel Predict carries out a moment
            // later); still open after a few reads, asked once more
            let pause = me.borrow().lookup_s;
            for attempt in 0..=TEST_LOOKUPS {
                crate::sync::sleep(pause / 3.0).await;
                if let Ok(Some(row)) = api.order(&oid, &digest).await {
                    let (got, status) = order_fill(&row, shares);
                    filled_seen = filled_seen.max(got);
                    if !status.is_empty() && status != "OPEN" {
                        removed = true;
                        lines.push(format!("3/4 再查：已经是 {status}，撤单生效了"));
                        break;
                    }
                }
                if attempt == TEST_LOOKUPS / 2 {
                    if let Ok((gone, noop)) = api.remove_orders(std::slice::from_ref(&oid)).await {
                        if gone.contains(&oid) || noop.contains(&oid) {
                            removed = true;
                            lines.push("3/4 再撤一次：撤单成功".into());
                            break;
                        }
                    }
                }
            }
            if !removed {
                lines.push(format!("3/4 再查了几次仍是挂着（最后一次撤单应答：{}）", api.last_remove.borrow()));
            }
        }
        let spent = |n: f64| {
            if n > crate::util::DUST { format!("（花了约 ${:.2}，这几份照常结算）", n * pf) } else { String::new() }
        };
        let final_read = match api.order(&oid, &digest).await {
            Ok(Some(row)) => {
                let (got, status) = order_fill(&row, shares);
                filled_seen = filled_seen.max(got);
                lines.push(format!(
                    "4/4 最终状态：{}，成交 {} 份{}",
                    if status.is_empty() { "?" } else { &status },
                    g(filled_seen),
                    spent(filled_seen)
                ));
                true
            }
            Ok(None) => {
                lines.push(format!("4/4 最终状态暂时查不到：成交按前面查到的 {} 份记{}", g(filled_seen), spent(filled_seen)));
                false
            }
            Err(e) => {
                lines.push(format!(
                    "4/4 最终状态没读到：{}——成交按前面查到的 {} 份记{}",
                    clean(&e),
                    g(filled_seen),
                    spent(filled_seen)
                ));
                false
            }
        };
        lines.push(format!(
            "结论：{}",
            if !removed {
                "⚠️ 下单成功但撤单没确认：到 Predict 网站看一下这张单（10 分钟后自己过期）".to_string()
            } else if !final_read {
                "⚠️ 下单、撤单都正常，但最后一次查询没读到这张单的最终状态：成交数以 Predict 网站为准，核对一下".to_string()
            } else if filled_seen > crate::util::DUST {
                format!(
                    "✅ 下单、查询、撤单都正常：签名、授权、这个区域都没问题，可以真实吃单（这张测试单撤掉前成交了 {} 份）",
                    g(filled_seen)
                )
            } else {
                "✅ 下单、查询、撤单都正常：签名、授权、这个区域都没问题，可以真实吃单".to_string()
            }
        ));
        record.insert("steps".into(), json!(lines.clone()));
        record.insert("order_id".into(), json!(oid));
        record.insert("removed".into(), json!(removed));
        record.insert("filled".into(), jf(filled_seen));
        record.insert("final_read".into(), json!(final_read));
        me.borrow_mut().store.put(&test_key, &Value::Object(record));
        return lines.join("\n");
    }
    let shown: Vec<String> = tried.into_iter().take(6).collect();
    format!("❌ 没找到能做挂单测试的市场（需要撮合已开启、最低卖价高于 1¢）：\n{}", shown.join("\n"))
}

/// What a save of the control page's settings came to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SaveResult {
    /// in force and on disk
    Done(String),
    /// in force now, but not yet on disk (the database took no write within its wait): a restart before the write lands
    /// brings the values before it back — said so, never reported as saved
    NotOnDisk(String),
    /// nothing changed (a value that does not parse, a field the page may not change)
    Refused(String),
}

/// The wait for a change of the operator's to reach the disk; the line to add when it did not.
async fn on_disk(me: &Me, what: &str, back: &str) -> String {
    let waiter = {
        let t = me.borrow();
        t.store.flush_waiter(t.flush_wait_s)
    };
    if crate::store::flush_async(waiter).await {
        return String::new();
    }
    let error = me.borrow().store.error();
    format!(
        "⚠️ {what}已经生效，但还没写进数据库（{}）：写完之前服务重启的话会{back}；数据库恢复后会自动补写",
        if error.is_empty() { "数据库忙，等了也没写完".to_string() } else { error }
    )
}

/// The control page's save: checked whole against the settings file with the other saved values (a wrong one changes
/// nothing), in force at once — in the same step as the check, so nothing (a mode switched meanwhile, another save) can come
/// between them — and saved; the windows planned again with it at the next tick. The reply waits for the disk and says so
/// when the write did not get there.
pub async fn save_settings(me: &Me, values: &serde_json::Map<String, Value>) -> SaveResult {
    let values = match console::clean(values) {
        Ok(v) => v,
        Err(e) => return SaveResult::Refused(e),
    };
    let names = {
        let mut held = me.borrow_mut();
        let t = &mut *held;
        let mut saved = t.saved_settings.clone();
        for (k, v) in &values {
            saved.insert(k.clone(), v.clone());
        }
        let (settings, cap) = match console::build(&t.base_env, &saved, &t.settings.mode) {
            Ok(x) => x,
            Err(e) => return SaveResult::Refused(e),
        };
        let as_json: serde_json::Map<String, Value> = saved.iter().map(|(k, v)| (k.clone(), json!(v))).collect();
        t.saved_settings = saved;
        t.loss_cap = cap;
        t.store.put(console::SAVED, &Value::Object(as_json));
        t.configure(settings);
        t.hold_loss_stop();
        values
            .iter()
            .map(|(k, v)| format!("{}＝{}", console::label(k), if v.is_empty() { "（空）" } else { v }))
            .collect::<Vec<_>>()
            .join("、")
    };
    let late = on_disk(me, "设置", "回到改之前的值").await;
    if !late.is_empty() {
        me.borrow().alarm(&format!("⚙️ 规则吃单设置已改（控制台）：{names}\n{late}"));
        return SaveResult::NotOnDisk(format!("{names}：{late}"));
    }
    me.borrow().notify(&format!("⚙️ 规则吃单设置已改（控制台）：{names}"));
    SaveResult::Done(format!("已保存：{names}"))
}

/// Back to the settings file's value for these fields (``None``: all of them) — in force at once, as a save is.
pub async fn reset_settings(me: &Me, keys: Option<&[String]>) -> SaveResult {
    let names = {
        let mut held = me.borrow_mut();
        let t = &mut *held;
        let keys: Vec<String> = match keys {
            None => t.saved_settings.keys().cloned().collect(),
            Some(keys) => keys.to_vec(),
        };
        let keys: Vec<String> = keys.into_iter().filter(|k| t.saved_settings.contains_key(k)).collect();
        let saved: Saved =
            t.saved_settings.iter().filter(|(k, _)| !keys.contains(k)).map(|(k, v)| (k.clone(), v.clone())).collect();
        let (settings, cap) = match console::build(&t.base_env, &saved, &t.settings.mode) {
            Ok(x) => x,
            Err(e) => return SaveResult::Refused(e),
        };
        let as_json: serde_json::Map<String, Value> = saved.iter().map(|(k, v)| (k.clone(), json!(v))).collect();
        t.saved_settings = saved;
        t.loss_cap = cap;
        t.store.put(console::SAVED, &Value::Object(as_json));
        t.configure(settings);
        t.hold_loss_stop();
        keys.iter().filter(|k| console::field(k).is_some()).map(|k| console::label(k)).collect::<Vec<_>>().join("、")
    };
    let names = if names.is_empty() { "（没有网页保存的值）".to_string() } else { names };
    let late = on_disk(me, "恢复部署设置", "回到网页保存的值").await;
    if !late.is_empty() {
        return SaveResult::NotOnDisk(format!("已恢复部署设置：{names}\n{late}"));
    }
    SaveResult::Done(format!("已恢复部署设置：{names}"))
}

impl Taker {
    /// The Telegram menu card's head: what is in force now (microseconds: no status text worked out), and the mode.
    pub fn menu_head(&self) -> (String, String) {
        let paused = if self.paused.is_empty() { "没暂停".to_string() } else { format!("已暂停（{}）", self.paused) };
        let stopped = if self.loss_stop_why().is_empty() { "" } else { "｜今日已到停机线" };
        let usd = self.usd(None).map(|u| format!("｜可用 ${}", comma(u, 2))).unwrap_or_default();
        (
            format!("模式：{}｜{paused}｜今日已结算 {}{usd}{stopped}", mode_name(&self.settings.mode), money(self.today_pnl())),
            self.settings.mode.clone(),
        )
    }

    /// Why the operator's order test may not go out now ("" when it may): the deploy's hard switch, the pause, the stop.
    pub fn test_gate(&self) -> String {
        if self.stop.is_set() {
            return "服务正在停止，不再发单".into();
        }
        if !self.config.trading_allowed {
            return "部署变量 LIVE_TRADING_ALLOWED 不是 on——这个硬开关关着时一张真实的单都不发，挂单测试也是真实的单".into();
        }
        if !self.paused.is_empty() {
            return format!(
                "已暂停（{}）：暂停时一张新单都不开，挂单测试也不做。要测：先 /mode paper（只记录，不会自动下单），再 /resume，再 /test",
                self.paused
            );
        }
        String::new()
    }

    pub fn settings_view(&self) -> Value {
        console::view(&self.settings, self.loss_cap, &self.saved_settings)
    }

    /// The review page's numbers, kept 30 s; not worked out in the race's seconds — the one kept is given then.
    pub fn review_data(&mut self, days: i64) -> Value {
        let now = self.now();
        let quiet = self.quiet_ms(now) > 0;
        if let Some((at, data)) = self.review_held.get(&days) {
            if now - at < 30_000 || quiet {
                let mut out = data.clone();
                out["computed_ms"] = json!(at);
                return out;
            }
        }
        if quiet {
            return json!({"busy": "抢单时刻前后这几秒不算复盘数据，稍后再刷新", "computed_ms": now});
        }
        let data = crate::review::review(&self.recs, &self.settings, now, days);
        // (a few spans kept, the one asked longest ago let go: /review 1 … /review 3650 never pile up)
        while self.review_held.len() >= REVIEW_HELD && !self.review_held.contains_key(&days) {
            let Some(oldest) = self.review_held.iter().min_by_key(|(_, (at, _))| *at).map(|(d, _)| *d) else { break };
            self.review_held.remove(&oldest);
        }
        self.review_held.insert(days, (now, data.clone()));
        let mut out = data;
        out["computed_ms"] = json!(now);
        out
    }

    /// The control page's view of now.
    pub fn state(&self) -> Value {
        let now = self.now();
        let s = &self.settings;
        let mut lags: Vec<f64> = self.loop_lags.iter().copied().collect();
        let host = self.api.host.clone();
        let net = crate::http::http().net_of(&host);
        let trips: Vec<(String, f64)> = self.api.trips.borrow().iter().cloned().collect();
        let mid = |xs: &mut Vec<f64>| median(xs).map(|m| jf(round_to(m, 1))).unwrap_or(Value::Null);
        let mut reads: Vec<f64> = trips.iter().filter(|(m, _)| m == "GET").map(|(_, ms)| *ms).collect();
        let mut writes: Vec<f64> = trips.iter().filter(|(m, _)| m != "GET").map(|(_, ms)| *ms).collect();
        let best = |xs: &Vec<f64>| xs.iter().copied().reduce(f64::min).map(|m| jf(round_to(m, 1))).unwrap_or(Value::Null);
        let (read_best, write_best) = (best(&reads), best(&writes));
        let rules: Vec<Value> = RULES
            .iter()
            .map(|rule| {
                let nxt = self.track.values().filter(|w| w.borrow().rule == rule.key).min_by_key(|w| w.borrow().start);
                let nb = nxt.map(|w| w.borrow());
                json!({
                    "key": rule.key, "name": rule.name, "market": rule.market, "side": rule.side, "word": rule.word(), "when": rule.when,
                    "cents": if s.on(rule.market) { s.price(rule.key) } else { 0 }, "live_cents": self.live_cents(rule),
                    "next_ms": nb.as_ref().map(|w| w.start), "moment_ms": nb.as_ref().map(|w| self.moment(w)),
                    "listed": nb.as_ref().is_some_and(|w| w.market.is_some()), "state": nb.as_ref().map(|w| w.word.clone()).unwrap_or_default(),
                    "phase": nb.as_ref().map(|w| self.phase(w, now)).unwrap_or(""),
                })
            })
            .collect();
        let wait = s.fast_in(now);
        let floor = self.loss_floor();
        let mut stream_lags: Vec<f64> = self.stream.lags.borrow().iter().map(|x| *x as f64).collect();
        let mut connect: Vec<f64> = net.connect.iter().copied().collect();
        let lag_max = lags.iter().copied().reduce(f64::max).map(|m| jf(round_to(m, 1))).unwrap_or(Value::Null);
        json!({
            "version": version(), "now_ms": now, "region": self.region, "mode": s.mode, "env_mode": self.env_mode,
            "modes": {"off": mode_name("off"), "paper": mode_name("paper"), "on": mode_name("on")},
            "paused": self.paused, "loss_stop": self.loss_stop_why(),
            "blocked": if s.mode == "on" { self.block(0.0, None) } else { String::new() }, "ready": self.ready(),
            "not_ready": if s.mode == "on" && !self.ready() { self.not_ready_why() } else { String::new() },
            "trading_allowed": self.config.trading_allowed,
            "account": {
                "kind": self.wallet.as_ref().map(|w| w.kind()).unwrap_or(""), "maker": self.wallet.as_ref().map(|w| w.maker.clone()).unwrap_or_default(),
                "usdt": self.usdt.map(|u| jf(from_wei(u))), "spendable": self.usd(None).map(jf),
                "bnb": self.bnb.map(|b| jf(from_wei(b))), "balance_ms": self.balance_ms, "balance_error": self.balance_error,
                "signed_in": self.api.usable(),
            },
            "stream": {"connected": self.stream.connected(), "alive": self.stream.alive(), "error": self.stream.error(),
                       "connects": self.stream.connects(), "lag_ms": mid(&mut stream_lags)},
            "index": match &self.index {
                Some(i) if self.index_wanted() => json!({"connected": i.connected.get(), "error": *i.error.borrow(), "pushes": i.pushes.get()}),
                _ => Value::Null,
            },
            "fast": {"hours": day_spans_text(&s.fast), "active": self.fast, "next_ms": wait.filter(|w| *w != 0).map(|w| now + w),
                     "looks": self.fast_looks, "verifies": self.verifies, "diffs": self.verify_diffs},
            "lead_minutes": s.lead_minutes, "plan": self.first_plan(now),
            "today": {"spent": jf(round_to(spent_on(self.recent_refs(), &day_of(now), &s.mode), 2)), "pnl": jf(round_to(self.today_pnl(), 4)),
                      "max_day_usd": jf(s.max_day_usd), "loss_cap": jf(self.loss_cap), "loss_floor": floor.map(jf),
                      "open_cost": jf(round_to(open_cost(&self.recs), 2))},
            "rules": rules, "speed": speed_stats(&self.recs, now - 30 * DAY_MS).map(Value::Object).unwrap_or(Value::Null),
            "net": {"region": self.region, "host": host, "edge": net.edge, "connect_ms": mid(&mut connect),
                    "read_ms": mid(&mut reads), "read_best_ms": read_best, "write_ms": mid(&mut writes), "write_best_ms": write_best},
            "process": {"loop_lag_ms": mid(&mut lags), "loop_lag_max_ms": lag_max,
                        "native_signing": true, "presigned": self.presigned, "presign_hits": self.presign_hits,
                        "first_ticks": self.first_ticks, "eager": true, "python": "", "runtime": "rust"},
            "errors": self.errors.iter().rev().take(12).map(|(at, text)| json!({"at": at, "text": text})).collect::<Vec<_>>(),
            "settings": self.settings_view(), "settings_dropped": self.settings_dropped,
            "fast_line": self.fast_line(now),
        })
    }
}
