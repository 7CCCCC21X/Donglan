//! What it says: the status (/status, /rules — the same layout as huice's), the lines of the control page.
use super::*;
use crate::clock::day_spans_text;
use crate::rules::{
    RACES_SHOWN, RULES, avg_price, cost, course_part, course_speed, course_stats, filled, lead_note, market_name, open_cost,
    race_summary, race_text, races, speed_stats, spent_on, watch_stats,
};
use crate::util::{DUST, bj, comma, g, median};

impl Taker {
    pub fn fast_line(&self, now: i64) -> String {
        let s = &self.settings;
        let lead = s.lead_minutes;
        let mut ahead: Vec<(i64, i64)> = self
            .track
            .values()
            .map(|w| {
                let b = w.borrow();
                (self.moment(&b), b.start)
            })
            .filter(|(at, _)| *at != 0 && now - at < SPRINT_MS)
            .collect();
        ahead.sort();
        let moment = if lead != 0 {
            format!(
                "｜对手提前 {lead} 分钟挂单的时刻前后也每秒检查（RULE_LEAD_MINUTES）{}",
                ahead
                    .first()
                    .map(|(at, start)| format!("，下一个 {}（{} 的场）", bj(*at, false), bj(*start, false)))
                    .unwrap_or_default()
            )
        } else {
            String::new()
        };
        let (mut text, wait) = if s.fast.is_empty() {
            ("每秒检查时段：关（RULE_FAST_HOURS=off）".to_string(), None)
        } else {
            (format!("每秒检查时段（北京时间）：{}", day_spans_text(&s.fast)), s.fast_in(now))
        };
        if self.fast {
            let listed = self.track.values().filter(|w| w.borrow().market.is_some()).count();
            let mut lags: Vec<f64> = self.stream.lags.borrow().iter().map(|x| *x as f64).collect();
            let mut trips: Vec<f64> = self.verify_ms.iter().map(|x| *x as f64).collect();
            let (lag, trip) = (median(&mut lags), median(&mut trips));
            let in_course = self.track.values().any(|w| self.phase(&w.borrow(), now) == "live");
            text += &format!(
                "｜进行中{}：每秒看 {listed} 场（已看 {} 次）；{}{}；REST 核对 {} 次{}{}{}{}",
                if in_course { "（有场次在盘中）" } else { "" },
                self.fast_looks,
                if self.stream.alive() { "推送正常" } else { "推送断开，靠 REST 轮流读" },
                lag.map(|l| format!("，推送延迟中位 {l:.0} ms")).unwrap_or_default(),
                self.verifies,
                if self.verify_diffs != 0 {
                    format!("，{} 次发现推送没送到的变化", self.verify_diffs)
                } else {
                    String::new()
                },
                trip.map(|t| format!("，一次往返中位 {t:.0} ms")).unwrap_or_default(),
                if self.status_reads != 0 {
                    format!("；等撮合开启时读状态 {} 次", self.status_reads)
                } else {
                    String::new()
                },
                if self.verify_error.is_empty() {
                    String::new()
                } else {
                    format!("；核对读取失败：{}", self.verify_error)
                }
            );
        } else if let Some(wait) = wait {
            text += &format!(
                "｜下一次 {} 开始{}",
                bj(now + wait, false),
                if wait <= WARM_MS {
                    "，正在准备（登录、授权额度、撮合状态、推送订阅）"
                } else {
                    "，提前 1 分钟准备"
                }
            );
        } else {
            text += "：推送一有变化就看，另外每 30 秒读一次、开场前 15 分钟每 2 秒";
        }
        text + &moment
    }

    pub fn first_plan(&self, now: i64) -> String {
        let s = &self.settings;
        let mut moments: std::collections::BTreeSet<i64> = std::collections::BTreeSet::new();
        let mut spans: Vec<[i64; 2]> = vec![];
        let mut windows: Vec<W> = self.track.values().cloned().collect();
        windows.sort_by_key(|w| w.borrow().start);
        for w in &windows {
            let b = w.borrow();
            let rule = b.rule();
            let (at, end) = (self.moment(&b), b.end());
            if at != 0 && now < at + QUIET_AFTER_MS && s.price(rule.key) > 0 {
                moments.insert(at);
            }
            if self.live_cents(rule) > 0 && now < end - LIVE_STOP_MS {
                match spans.last_mut() {
                    Some(last) if last[1] >= b.start => last[1] = last[1].max(end),
                    _ => spans.push([b.start, end]),
                }
            }
        }
        let told = |items: Vec<(i64, String)>| -> String {
            let mut out = vec![];
            let mut day = day_of(now);
            for (at, text) in items {
                let prefix = if day_of(at) != day { bj(at, true).chars().take(6).collect::<String>() } else { String::new() };
                out.push(format!("{prefix}{text}"));
                day = day_of(at);
            }
            out.join("、")
        };
        let mut parts = vec![];
        if !moments.is_empty() {
            let shown: Vec<i64> = moments.iter().take(8).copied().collect();
            parts.push(format!(
                "盘前抢单时刻 {}{}",
                told(shown.iter().map(|at| (*at, bj(*at, false))).collect()),
                if moments.len() > shown.len() { format!(" 等 {} 个", moments.len()) } else { String::new() }
            ));
        }
        if !spans.is_empty() {
            let whole = (if self.planned_ms != 0 { self.planned_ms } else { now }) + HORIZON_MS;
            let shown: Vec<(i64, String)> = spans
                .iter()
                .take(4)
                .map(|[a, b]| {
                    (
                        *a,
                        if *b < whole {
                            format!("{}–{}", bj(*a, false), bj(*b, false))
                        } else {
                            format!("{} 起", bj(*a, false))
                        },
                    )
                })
                .collect();
            parts.push(format!(
                "盘中 {}{}",
                told(shown),
                if spans.len() > 4 { format!(" 等 {} 段", spans.len()) } else { String::new() }
            ));
        }
        if parts.is_empty() { String::new() } else { format!("接下来一天：{}", parts.join("；")) }
    }

    /// How the process itself is doing: the loop's delay, the signing, the orders signed beforehand.
    pub fn speed_text(&self) -> String {
        let mut lags: Vec<f64> = self.loop_lags.iter().copied().collect();
        let mut parts = vec!["独立进程（Rust，盘口一到当场处理、当场发单）".to_string()];
        if !lags.is_empty() {
            let top = lags.iter().copied().fold(0.0, f64::max);
            parts.push(format!("主循环延迟近 1 分钟中位 {:.1} ms、最长 {top:.0} ms", median(&mut lags).unwrap_or(0.0)));
        }
        parts.push("签名：原生 secp256k1（Rust）".into());
        if self.presigned != 0 || self.presign_hits != 0 {
            parts.push(format!("预先签好 {} 张，下单时用上 {} 张", self.presigned, self.presign_hits));
        }
        if self.first_ticks != 0 {
            parts.push(format!("推送当场处理 {} 次", self.first_ticks));
        }
        if self.index_looks != 0 || self.index_late != 0 {
            parts.push(format!(
                "盘中由指数带来的检查 {} 次{}",
                self.index_looks,
                if self.index_late != 0 {
                    format!("（另有 {} 次指数到达时已超过 {} 秒）", self.index_late, g(INDEX_FRESH_MS as f64 / 1000.0))
                } else {
                    String::new()
                }
            ));
        }
        format!("速度：{}", parts.join("｜"))
    }

    pub fn net_line(&self) -> String {
        let host = self.api.host.clone();
        let net = crate::http::http().net_of(&host);
        let trips: Vec<(String, f64)> = self.api.trips.borrow().iter().cloned().collect();
        let mut reads: Vec<f64> = trips.iter().filter(|(m, _)| m == "GET").map(|(_, ms)| *ms).collect();
        let mut writes: Vec<f64> = trips.iter().filter(|(m, _)| m != "GET").map(|(_, ms)| *ms).collect();
        let spread = |xs: &mut Vec<f64>| {
            let low = xs.iter().copied().fold(f64::INFINITY, f64::min);
            format!("中位 {:.0} ms（最快 {low:.0}）", median(xs).unwrap_or(0.0))
        };
        let mut connect: Vec<f64> = net.connect.iter().copied().collect();
        let mut parts = vec![format!("服务在 {}", if self.region.is_empty() { "未知区域" } else { &self.region })];
        if !connect.is_empty() {
            parts.push(format!("到 {host} 新建连接 {}（TCP+TLS，2～3 个来回）", spread(&mut connect)));
        }
        if !net.edge.is_empty() {
            parts.push(format!("入口 {}", net.edge));
        }
        let read_mid = median(&mut reads.clone());
        if !reads.is_empty() {
            parts.push(format!("读一次 {}", spread(&mut reads)));
        }
        if !writes.is_empty() {
            parts.push(format!("下单 / 撤单一次 {}", spread(&mut writes)));
        }
        if reads.is_empty() && writes.is_empty() && connect.is_empty() {
            return "网络：还没有请求".into();
        }
        let hint = if read_mid.is_some_and(|m| m >= 60.0) {
            "｜一次读取超过 60 ms：这个区域离 Predict 的服务器不近，可以用 ruletaker latency 在别的区域量一下"
        } else {
            ""
        };
        format!("网络（不含排队）：{}{hint}", parts.join("｜"))
    }

    pub fn speed_line(&self, now: i64) -> String {
        let Some(sp) = speed_stats(&self.recs, now - 30 * DAY_MS) else { return String::new() };
        let best = sp.get("best").cloned().unwrap_or(Value::Null);
        let ms = |key: &str| -> String {
            match sp.get(key).and_then(Value::as_f64) {
                None => "—".into(),
                Some(v) => format!(
                    "{v:.0} ms{}",
                    best.get(key).and_then(Value::as_f64).map(|b| format!("（最快 {b:.0}）")).unwrap_or_default()
                ),
            }
        };
        let cs = course_speed(&self.recs, now - 30 * DAY_MS);
        format!(
            "下单速度（近 30 天真实订单 {} 张，{} 张有成交）：盘口变化→推送到达 中位 {}、到达→发出 {}{}、发出→应答 {}{}",
            sp.i("orders"),
            sp.i("filled"),
            ms("lag"),
            ms("own"),
            sp.get("wake").and_then(Value::as_f64).map(|w| format!("［其中到达→开始处理 {w:.0} ms］")).unwrap_or_default(),
            ms("trip"),
            cs.map(|c| format!(
                "｜盘中由指数带来的 {} 张：指数到达→发出 中位 {:.0} ms（最快 {:.0}）",
                c.i("orders"),
                c.get("own").and_then(Value::as_f64).unwrap_or(0.0),
                c.get("best").and_then(Value::as_f64).unwrap_or(0.0)
            ))
            .unwrap_or_default()
        )
    }

    pub fn account_line(&self) -> String {
        let Some(wallet) = &self.wallet else {
            return "账户：没有设置 PREDICT_PRIVATE_KEY（只能 off / paper）".into();
        };
        let bal = match self.usdt {
            Some(u) => format!("USDT {}", comma(from_wei(u), 2)),
            None => format!("USDT 未读到（{}）", if self.balance_error.is_empty() { "尚未读取" } else { &self.balance_error }),
        };
        let bnb = self.bnb.map(|b| format!("｜BNB {:.4}", from_wei(b))).unwrap_or_default();
        let maker = &wallet.maker;
        let open = open_cost(&self.recs);
        format!(
            "账户：{} {}…{}｜{bal}{bnb}｜{}{}{}",
            wallet.kind(),
            &maker[..6.min(maker.len())],
            &maker[maker.len().saturating_sub(4)..],
            if self.api.fresh() { "已登录" } else { "未登录" },
            if self.settings.mode == "on" && !self.ready() {
                format!("｜未就绪：{}", self.not_ready_why())
            } else {
                String::new()
            },
            if open > 0.0 { format!("｜在途和未结算 ${}", comma(open, 2)) } else { String::new() }
        )
    }

    pub fn status(&self) -> String {
        let now = self.now();
        let s = &self.settings;
        let recs = &self.recs;
        let in_course = RULES.iter().any(|r| self.live_cents(r) > 0);
        let today = day_of(now);
        let mut head = format!(
            "规则吃单 {}（独立服务）：{}（RULE_TAKER={}{}）｜{}，盘口一有就买，到开场前 {} 秒",
            super::control::version(),
            mode_name(&s.mode),
            self.env_mode,
            if s.mode != self.env_mode { format!("，运行中调成 {}", s.mode) } else { String::new() },
            if in_course { "开场前吃" } else { "只在开场前吃" },
            STOP_MS / 1000
        );
        if in_course {
            head += &format!(
                "；设了盘中价格的规则开场后接着买（开场 {} 秒后到结束前 {} 秒，第 1 分钟最高按盘前的价格）",
                LIVE_START_MS / 1000,
                LIVE_STOP_MS / 1000
            );
        }
        if s.max_day_usd != 0.0 {
            head += &format!(
                "｜每天最多花 ${}（今天已花 ${}）",
                g(s.max_day_usd),
                comma(spent_on(recs.values(), &today, &s.mode), 2)
            );
        }
        if let Some(floor) = self.loss_floor() {
            head += &format!(
                "｜日亏损停机线 {}（今天已结算 {}{}）",
                money(floor),
                money(day_pnl(recs, &today)),
                if self.loss_stop_why().is_empty() { "" } else { "，今天已停机：/resume 解除" }
            );
        }
        let error = self.stream.error();
        head += &format!(
            "｜推送 {}",
            if self.stream.connected() {
                "已连接".to_string()
            } else {
                format!("未连接{}", if error.is_empty() { String::new() } else { format!("（{error}）") })
            }
        );
        if let (Some(index), true) = (&self.index, in_course) {
            let e = index.error.borrow().clone();
            head += &format!(
                "｜币安指数 {}",
                if index.connected.get() {
                    "已连接".to_string()
                } else {
                    format!("未连接{}", if e.is_empty() { String::new() } else { format!("（{e}）") })
                }
            );
        }
        let mut lines = vec![head];
        if s.mode == "on" {
            let why = self.block(0.0, None);
            if !why.is_empty() {
                lines.push(format!("⏸ 现在不下单：{why}"));
            }
        }
        lines.push(self.account_line());
        if s.mode != "off" {
            lines.push(self.fast_line(now));
            let plan = self.first_plan(now);
            if !plan.is_empty() {
                lines.push(plan);
            }
            lines.push(self.speed_text());
            let speed = self.speed_line(now);
            if !speed.is_empty() {
                lines.push(speed);
            }
            lines.push(self.net_line());
        }
        for market in ["gold", "oil"] {
            let name = market_name(market);
            lines.push(format!(
                "{name}：{}{}",
                if s.on(market) { "开" } else { "关" },
                if s.on(market) {
                    format!("｜每场最多 {} 份", g(s.shares_of(market) as f64))
                } else {
                    "（这个市场的规则都不做）".to_string()
                }
            ));
            if !s.on(market) {
                continue;
            }
            for rule in RULES.iter().filter(|r| r.market == market) {
                let (cents, during) = (s.price(rule.key), self.live_cents(rule));
                if cents <= 0 && during <= 0 {
                    lines.push(format!("· {}：不做（价格设为 0）", rule.name));
                    continue;
                }
                let nxt = self.track.values().filter(|w| w.borrow().rule == rule.key).min_by_key(|w| w.borrow().start);
                let st = if s.mode != "off" && cents > 0 {
                    Some(watch_stats(recs, rule.key, &s.mode, now - 30 * DAY_MS))
                } else {
                    None
                };
                let cs = if s.mode != "off" { Some(course_stats(recs, rule.key, &s.mode, now - 30 * DAY_MS)) } else { None };
                let mut line = format!(
                    "· {} 买{} {}",
                    rule.name,
                    rule.word(),
                    if cents > 0 { format!("≤{cents}¢") } else { "盘前不买".into() }
                );
                if during > 0 {
                    line += &format!("｜盘中 ≤{during}¢（{}）", lead_note(rule));
                }
                line += &format!("：{}", rule.when);
                if let Some(w) = nxt {
                    let b = w.borrow();
                    line += &format!(
                        "｜{} {}{}",
                        if b.start <= now { "这一场" } else { "下一场" },
                        bj(b.start, true),
                        if b.word.is_empty() { String::new() } else { format!("：{}", b.word) }
                    );
                }
                if let Some(st) = st.filter(|st| st.i("watched") != 0) {
                    line += &format!("｜近 30 天看了 {} 场，{} 场有 ≤{cents}¢ 的卖单", st.i("watched"), st.i("offered"));
                    if let Some(avg) = st.get("offer_avg").and_then(Value::as_f64).filter(|a| *a != 0.0) {
                        line += &format!("（平均 {} 份）", g(avg));
                    }
                    if let Some(best) = st.get("best_mid").and_then(Value::as_f64) {
                        line += &format!("，最低卖价中位 {:.0}¢", best * 100.0);
                    }
                    if let Some(lead) = st.get("open_lead").and_then(Value::as_f64) {
                        line += &format!("，撮合开启在开场前中位 {lead:.0} 秒");
                    }
                    if let Some(lead) = st.get("offer_lead").and_then(Value::as_f64) {
                        line += &format!("，卖单首次出现在开场前中位 {lead:.0} 分钟");
                    }
                    if st.i("lost") != 0 {
                        line += &format!(
                            "，{} 场的卖单没买到就没了（存活中位 {:.1} 秒）",
                            st.i("lost"),
                            st.get("life").and_then(Value::as_f64).unwrap_or(0.0) / 1000.0
                        );
                    }
                    if st.i("won") != 0 {
                        line += &format!(
                            "，买到的 {} 场从卖单出现到被吃掉中位 {:.1} 秒",
                            st.i("won"),
                            st.get("won_life").and_then(Value::as_f64).unwrap_or(0.0) / 1000.0
                        );
                    }
                }
                if let Some(cs) = cs.filter(|cs| cs.i("windows") != 0) {
                    let shares = cs.f("shares");
                    line += &format!(
                        "｜盘中近 30 天买到 {} 场 {} 份（均价 {:.1}¢）",
                        cs.i("windows"),
                        g(shares),
                        if shares != 0.0 { cs.f("cost") / shares * 100.0 } else { 0.0 }
                    );
                    if cs.i("settled") != 0 {
                        line += &format!(
                            "，已结算 {} 场：赢 {} 输 {}，{}",
                            cs.i("settled"),
                            cs.i("wins"),
                            cs.i("losses"),
                            money(cs.f("pnl"))
                        );
                    }
                }
                lines.push(line);
            }
        }
        if s.mode != "off" {
            let all = races(recs, &s.mode, now - 30 * DAY_MS);
            let rows: Vec<&Obj> = all.iter().take(RACES_SHOWN).collect();
            if !rows.is_empty() {
                lines.push(format!(
                    "抢单记录（最近 {} 场，北京时间；“开始被吃 / 被吃光”是卖单出现之后多久，按 Predict 盘口自己的时间戳）：",
                    rows.len()
                ));
                let summary = race_summary(&all);
                if !summary.is_empty() {
                    lines.push(format!("  近 30 天：{summary}"));
                }
                for row in rows {
                    lines.push(format!("  {}", race_text(row)));
                }
            }
        }
        for (mode, word) in [("on", "今天"), ("paper", "今天（只记录，没真买）")] {
            let mine: Vec<&Obj> =
                recs.values().filter(|r| r.s("mode") == mode && day_of(r.i("start_ms")) == today && filled(r) > DUST).collect();
            if mine.is_empty() {
                continue;
            }
            let done: Vec<&&Obj> = mine.iter().filter(|r| !r.none("pnl")).collect();
            lines.push(format!(
                "{word}：{} 场买到 {} 份，成本 ${}{}",
                mine.len(),
                g(mine.iter().map(|r| filled(r)).sum()),
                comma(mine.iter().map(|r| cost(r)).sum(), 2),
                if done.is_empty() {
                    String::new()
                } else {
                    format!("；已结算 {} 场 {}", done.len(), money(done.iter().map(|r| r.f("pnl")).sum()))
                }
            ));
        }
        let mut last: Vec<&Obj> = recs.values().filter(|r| filled(r) > DUST).collect();
        last.sort_by_key(|r| -r.i("start_ms"));
        for r in last.into_iter().take(5) {
            let rule = rule_of(&r.s("rule")).unwrap_or(&RULES[0]);
            let live = course_part(r).0;
            lines.push(format!(
                "  {} {} 买{} {:.1}¢×{}{}{}{}",
                bj(r.i("start_ms"), true),
                rule.name,
                if r.s("side") == "up" { "涨" } else { "跌" },
                avg_price(r) * 100.0,
                g(filled(r)),
                if live > DUST { format!("（其中盘中 {} 份）", g(live)) } else { String::new() },
                if r.s("mode") == "paper" { "（只记录）" } else { "" },
                if r.none("pnl") { "（待结算）".to_string() } else { format!(" → {}", money(r.f("pnl"))) }
            ));
        }
        if !self.error.is_empty() {
            lines.push(format!("最近问题：{}", self.error));
        }
        lines.join("\n")
    }

    pub fn summary(&self) -> Value {
        let now = self.now();
        let mut lags: Vec<f64> = self.loop_lags.iter().copied().collect();
        let mut windows: Vec<(i64, String, W)> =
            self.track.iter().map(|(s, w)| (w.borrow().start, s.clone(), w.clone())).collect();
        windows.sort_by_key(|x| x.0);
        json!({
            "version": super::control::version(), "mode": self.settings.mode, "env_mode": self.env_mode, "paused": self.paused,
            "blocked": if self.settings.mode == "on" { self.block(0.0, None) } else { String::new() }, "ready": self.ready(),
            "region": self.region, "usdt": self.usd(None).map(jf), "stream_connected": self.stream.connected(), "fast": self.fast,
            "records": self.recs.len(), "today_spent": jf(spent_on(self.recs.values(), &day_of(now), &self.settings.mode)),
            "today_pnl": jf(day_pnl(&self.recs, &day_of(now))),
            "speed_30d": speed_stats(&self.recs, now - 30 * DAY_MS).map(Value::Object).unwrap_or(Value::Null),
            "loop_lag_ms": median(&mut lags).map(jf),
            "windows": windows.iter().take(20).map(|(start, slug, w)| {
                let b = w.borrow();
                json!({"slug": slug, "rule": b.rule, "start_ms": start, "market": b.market.as_ref().and_then(|m| m.get("id").cloned()), "word": b.word})
            }).collect::<Vec<_>>(),
            "error": self.error,
        })
    }
}
