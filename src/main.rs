//! 规则吃单 · 独立服务（Rust 版）。
//!
//!   ruletaker [run | latency | check | plan] [--env FILE] [--no-order-probe]
//!
//!   run       the service (the default): the rule taker, its Telegram notifications and its status page
//!   latency   read-only: how far Predict is from this machine (compare regions before moving; no order is sent)
//!   check     read-only: the settings, the sign-in, the Predict account, the balance, the allowances, and whether Predict's
//!             order endpoint answers this region (an empty order body: refused as invalid, never placed)
//!   plan      no network: the rules' windows of the next two days in Beijing and New York time
pub mod cards;
pub mod clock;
pub mod config;
pub mod console;
pub mod http;
pub mod index;
mod parity;
pub mod predict;
pub mod review;
pub mod rules;
pub mod server;
pub mod store;
pub mod stream;
pub mod sync;
pub mod taker;
pub mod telegram;
#[cfg(test)]
mod tests;
pub mod util;
pub mod wallet;

use config::{Config, Env, load_env_file};
use futures_util::{SinkExt, StreamExt};
use std::rc::Rc;
use util::{bj, clean_error, mono, now_ms};

const VERSION: &str = env!("CARGO_PKG_VERSION");

pub fn load_env(env_file: Option<&str>) -> Env {
    let mut env: Env = std::env::vars().collect();
    let candidates: Vec<String> = match env_file {
        Some(f) => vec![f.to_string()],
        None => vec!["/etc/ruletaker/ruletaker.env".into(), ".env".into()],
    };
    for path in candidates {
        if std::path::Path::new(&path).is_file() {
            load_env_file(&path, &mut env);
            break;
        }
    }
    env
}

/// The Compute Engine zone this runs in (the metadata server), "" elsewhere.
async fn gce_zone() -> String {
    let url = "http://metadata.google.internal/computeMetadata/v1/instance/zone";
    match tokio::time::timeout(
        std::time::Duration::from_secs(1),
        http::http().request(url, None, 1.0, &[("Metadata-Flavor", "Google")]),
    )
    .await
    {
        Ok(Ok(raw)) => String::from_utf8_lossy(&raw).rsplit('/').next().unwrap_or("").trim().to_string(),
        _ => String::new(),
    }
}

fn make_telegram(config: &Config) -> Rc<telegram::Telegram> {
    let notify_token = if config.telegram_token.is_empty() { &config.telegram_command_token } else { &config.telegram_token };
    Rc::new(telegram::Telegram::new(
        notify_token,
        &config.telegram_chats,
        config.admin_user_id,
        config.telegram_commands,
        !notify_token.is_empty(),
        &config.telegram_command_token,
        Rc::new(predict::Live),
    ))
}

async fn run_service(config: Config, env: Env) -> i32 {
    info!("规则吃单 {VERSION}（Rust）启动｜{}", config.secrets_line());
    let store = match store::Store::open(&config.state_db) {
        Ok(s) => s,
        Err(e) => {
            error!("{e}");
            return 2;
        }
    };
    let region = if !config.region.is_empty() {
        config.region.clone()
    } else {
        let zone = gce_zone().await;
        if zone.is_empty() { std::env::var("RAILWAY_REPLICA_REGION").unwrap_or_default() } else { zone }
    };
    // the index feed of every rule that can buy in its windows' course (gold): made always — an in-play price may be set on the
    // control page while the service runs — and connected only while one is set (Taker::index_wanted)
    let mut symbols: Vec<&str> = rules::RULES.iter().filter(|r| r.in_play()).map(|r| r.spec().symbol).collect();
    symbols.sort();
    symbols.dedup();
    let index = Rc::new(index::IndexFeed::new(&symbols, &config.binance_fapi, &config.binance_ws));
    let telegram = make_telegram(&config);
    let base_env: Env = env.iter().filter(|(k, _)| k.starts_with("RULE_")).map(|(k, v)| (k.clone(), v.clone())).collect();
    let (host, port, key) = (config.status_host.clone(), config.status_port, config.status_key.clone());
    let made = taker::Taker::new(taker::Parts {
        config,
        store,
        api: None,
        wallet: None,
        chain: None,
        stream: None,
        index: Some(index),
        telegram: Some(telegram.clone()),
        clock: None,
        base_env: Some(base_env),
    });
    let me = match made {
        Ok(t) => taker::install(t),
        Err(e) => {
            error!("启动失败：{e}");
            return 2;
        }
    };
    me.borrow_mut().region = region.clone();
    {
        let me2 = me.clone();
        *telegram.handler.borrow_mut() = Some(Rc::new(move |command: String, args: String| {
            let me = me2.clone();
            Box::pin(async move { taker::control::command(&me, &command, &args).await })
        }));
        let me3 = me.clone();
        *telegram.menu_head.borrow_mut() = Some(Rc::new(move || me3.borrow().menu_head()));
        let me4 = me.clone();
        *telegram.settings_view.borrow_mut() = Some(Rc::new(move || me4.borrow().settings_view()));
    }
    let stop = me.borrow().stop.clone();
    {
        let stop = stop.clone();
        tokio::task::spawn_local(async move {
            use tokio::signal::unix::{SignalKind, signal};
            let (Ok(mut term), Ok(mut int)) = (signal(SignalKind::terminate()), signal(SignalKind::interrupt())) else { return };
            tokio::select! {
                _ = term.recv() => {}
                _ = int.recv() => {}
            }
            info!("收到停止信号：撤掉在途单、写完记录后退出");
            stop.set();
        });
    }
    if let Err(e) = server::serve(me.clone(), host, port, key).await {
        warn!("{e}");
    }
    let tg = tokio::task::spawn_local(telegram.clone().run());
    let running = tokio::task::spawn_local(taker::flow::run(me.clone()));
    {
        let (me, telegram, region) = (me.clone(), telegram.clone(), region.clone());
        tokio::task::spawn_local(async move {
            // once signed in (real orders) or after 20 s: what the operator sees first is the state it settled in
            for _ in 0..40 {
                let done = {
                    let t = me.borrow();
                    t.settings.mode != "on" || t.ready() || !t.running && t.started_ms != 0
                };
                if done {
                    break;
                }
                sync::sleep(0.5).await;
            }
            let text = me.borrow().mode_text();
            telegram.send(&format!(
                "🚀 规则吃单 {VERSION}（Rust）已启动（{}）：{text}",
                if region.is_empty() { "未知区域" } else { &region }
            ));
        });
    }
    let dropped = me.borrow().settings_dropped.clone();
    if !dropped.is_empty() {
        telegram.send(&format!("⚠️ 控制台保存的这些设置和设置文件冲突，已删掉（改用设置文件的值）：{}", dropped.join("、")));
        warn!("控制台保存的设置和设置文件冲突，已删掉：{}", dropped.join("、"));
    }
    let _ = running.await;
    telegram.send("🛑 规则吃单服务停止");
    telegram.stop();
    let _ = tokio::time::timeout(std::time::Duration::from_secs(8), tg).await;
    let waiter = me.borrow().store.flush_waiter(10.0);
    store::flush_async(waiter).await;
    0
}

fn spread(xs: &[f64]) -> String {
    if xs.is_empty() {
        return "没量到".into();
    }
    let mut v = xs.to_vec();
    let lo = v.iter().copied().fold(f64::INFINITY, f64::min);
    let hi = v.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    format!("中位 {:.1} ms（最快 {lo:.1}，最慢 {hi:.1}）", util::median(&mut v).unwrap_or(0.0))
}

/// Read-only: a first request (a new connection: its handshake), reads of an order book on that connection, the push
/// connection's handshake, its ping there and back and a subscription's first book.
async fn latency(config: Config, rounds: usize) -> i32 {
    let base = predict::api_base(config.chain_id);
    let host = http::Url::parse(base).map(|u| u.host).unwrap_or_default();
    let headers: Vec<(&str, &str)> =
        if config.api_key.is_empty() { vec![] } else { vec![("x-api-key", config.api_key.as_str())] };
    let zone = if config.region.is_empty() { gce_zone().await } else { config.region.clone() };
    println!(
        "Predict 网络延迟探测（只读，不下单）｜区域 {}｜版本 {VERSION}（Rust）{}",
        if zone.is_empty() { "未知" } else { &zone },
        if config.api_key.is_empty() { "｜没有设置 API Key：可能被拒绝" } else { "" }
    );
    let pool = http::http();
    let now = now_ms();
    let slug = format!("btc-updown-15m-{}", (now - now % 900_000) / 1000);
    let mut market_id = String::new();
    let began = mono();
    match pool.request_json(&format!("{base}/categories/{slug}"), None, 10.0, &headers).await {
        Ok(answer) => {
            let first = (mono() - began) * 1000.0;
            market_id = predict::predict_markets(&answer).first().map(|m| util::Py::s(m, "id")).unwrap_or_default();
            println!(
                "第一次请求（含新建连接）{first:.0} ms｜市场 {slug} → {}",
                if market_id.is_empty() { "没找到" } else { &market_id }
            );
        }
        Err(e) => println!("读市场失败：{}", clean_error(&e)),
    }
    let path = if market_id.is_empty() { format!("/categories/{slug}") } else { format!("/markets/{market_id}/orderbook") };
    let (mut reads, mut refused, mut why) = (vec![], vec![], String::new());
    for _ in 0..rounds {
        let began = mono();
        match pool.request(&format!("{base}{path}"), None, 10.0, &headers).await {
            Ok(_) => reads.push((mono() - began) * 1000.0),
            Err(e) => {
                why = clean_error(&e);
                if why.starts_with("HTTP ") {
                    refused.push((mono() - began) * 1000.0);
                }
            }
        }
        sync::sleep(0.4).await;
    }
    let net = pool.net_of(&host);
    let connect: Vec<f64> = net.connect.iter().copied().collect();
    println!(
        "{}{}{}",
        if !reads.is_empty() || refused.is_empty() {
            format!("读盘口 {} 次（同一条连接）：{}", reads.len(), spread(&reads))
        } else {
            format!("读盘口被拒绝（{why}），{} 次被拒的应答来回 {}——也是一次往返，可以拿来比", refused.len(), spread(&refused))
        },
        if connect.is_empty() { String::new() } else { format!("｜新建连接（TCP+TLS）{}", spread(&connect)) },
        if net.edge.is_empty() { String::new() } else { format!("｜入口 {}", net.edge) }
    );
    let ws_headers: Vec<(String, String)> = headers.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
    let began = mono();
    match stream::ws_connect(stream::PREDICT_WS_URL, &ws_headers).await {
        Err(e) => println!("推送连接失败：{}", clean_error(&e)),
        Ok(ws) => {
            let open = (mono() - began) * 1000.0;
            let (mut sink, mut source) = ws.split();
            let mut pings = vec![];
            let mut book_ms = None;
            for _ in 0..5 {
                let began = mono();
                if sink.send(tokio_tungstenite::tungstenite::Message::Ping(Vec::new().into())).await.is_err() {
                    break;
                }
                let pong = tokio::time::timeout(std::time::Duration::from_secs(5), async {
                    while let Some(Ok(frame)) = source.next().await {
                        if matches!(frame, tokio_tungstenite::tungstenite::Message::Pong(_)) {
                            return true;
                        }
                    }
                    false
                })
                .await;
                if pong != Ok(true) {
                    break;
                }
                pings.push((mono() - began) * 1000.0);
                sync::sleep(0.2).await;
            }
            if !market_id.is_empty() {
                let began = mono();
                let ask = serde_json::json!({"method": "subscribe", "requestId": 1, "params": [format!("predictOrderbook/{market_id}")]}).to_string();
                if sink.send(tokio_tungstenite::tungstenite::Message::text(ask)).await.is_ok() {
                    let _ = tokio::time::timeout(std::time::Duration::from_secs(5), async {
                        while let Some(Ok(frame)) = source.next().await {
                            if let tokio_tungstenite::tungstenite::Message::Text(text) = frame {
                                let v: serde_json::Value = serde_json::from_str(text.as_str()).unwrap_or_default();
                                if v["type"] == "M" && v["topic"].as_str().unwrap_or("").starts_with("predictOrderbook/") {
                                    book_ms = Some((mono() - began) * 1000.0);
                                    return;
                                }
                            }
                        }
                    })
                    .await;
                }
            }
            let _ = sink.close().await;
            println!(
                "推送连接：握手 {open:.0} ms｜ping 来回 {}{}",
                spread(&pings),
                book_ms.map(|b| format!("｜订阅到第一份盘口 {b:.0} ms")).unwrap_or_default()
            );
        }
    }
    println!(
        "怎么看：“读盘口”的中位数就是这里到 Predict 的一次往返（下单也差不多）。在东京（asia-northeast1）量一次，\
         和原来 Railway 上 /rules 网络一行的“读一次”比——小得多就说明搬对了。"
    );
    0
}

async fn check(config: Config, probe_order: bool) -> i32 {
    println!("规则吃单 {VERSION}（Rust）自检（只读）");
    println!("{}", config.secrets_line());
    let s = &config.rules;
    println!(
        "模式 RULE_TAKER={}｜黄金 {} 每场 {} 份｜原油 {} 每场 {} 份｜每天上限 ${}｜日亏损停机 ${}｜签名 Rust 原生",
        s.mode,
        if s.on("gold") { "开" } else { "关" },
        s.shares_of("gold"),
        if s.on("oil") { "开" } else { "关" },
        s.shares_of("oil"),
        util::g(s.max_day_usd),
        util::g(config.max_daily_loss_usd)
    );
    if config.private_key.is_empty() {
        println!("没有 PREDICT_PRIVATE_KEY：只能 off / paper。");
        return 0;
    }
    let wallet = match wallet::Wallet::new(&config.private_key, config.chain_id, &config.account) {
        Ok(w) => Rc::new(w),
        Err(e) => {
            println!("❌ {e}");
            return 1;
        }
    };
    println!(
        "钱包：{}｜下单账户 {}{}",
        wallet.kind(),
        wallet.maker,
        if wallet.predict_account.is_empty() { String::new() } else { format!("｜签名钱包 {}", wallet.signer) }
    );
    let live: Rc<dyn predict::Transport> = Rc::new(predict::Live);
    let api = predict::PredictApi::new(
        predict::api_base(config.chain_id),
        &config.api_key,
        Some(wallet.clone()),
        config.rate_per_min,
        live.clone(),
    );
    let rpc = if config.rpc_url.is_empty() { predict::rpc_url(config.chain_id).to_string() } else { config.rpc_url.clone() };
    let chain = predict::Chain::new(&rpc, wallet.clone(), live);
    let mut ok = true;
    match api.ensure_auth().await {
        Ok(()) => println!("✅ Predict 登录成功"),
        Err(e) => {
            ok = false;
            println!("❌ Predict 登录失败：{}", clean_error(&e));
        }
    }
    if !wallet.predict_account.is_empty() {
        match chain.account_owner(&wallet.predict_account).await {
            Ok(owner) => {
                let same = owner.to_lowercase() == wallet.signer.to_lowercase();
                ok &= same;
                println!(
                    "{}",
                    if same {
                        "✅ 私钥是这个 Predict 账户的控制钥匙".to_string()
                    } else {
                        format!("❌ 私钥不是这个 Predict 账户的控制钥匙（链上记录 {owner}）")
                    }
                );
            }
            Err(e) => {
                ok = false;
                println!("❌ 链上核对 Predict 账户失败：{}", clean_error(&e));
            }
        }
    }
    match (chain.usdt_balance().await, chain.bnb_balance().await) {
        (Ok(u), Ok(b)) => println!("余额：USDT {}｜签名钱包 BNB {:.4}", util::comma(wallet::from_wei(u), 2), wallet::from_wei(b)),
        (Err(e), _) | (_, Err(e)) => println!("⚠️ 余额没读到：{}", clean_error(&e)),
    }
    for neg in [false, true] {
        let key = wallet::exchange_key(neg, false);
        match chain.allowance(key).await {
            Ok(a) => println!(
                "授权 {key}：{}",
                if a >= 100_000_000_000_000_000_000 {
                    "✅ 已授权"
                } else {
                    "❌ 没有授权（在 Predict 网站用这个账户交易一次）"
                }
            ),
            Err(e) => println!("⚠️ 授权 {key} 没读到：{}", clean_error(&e)),
        }
    }
    if probe_order && api.fresh() {
        // an empty order: Predict refuses it as invalid (400) — unless this region may not trade at all (403). Never placed
        match api.authed("POST", "/orders", Some(Rc::new(br#"{"data":{}}"#.to_vec())), predict::API_TIMEOUT, true, None).await {
            Ok(_) => println!("⚠️ 下单接口对空订单没有报错（意外），请人工核对"),
            Err(e) => {
                let status = e.status();
                if status == 403 {
                    ok = false;
                    println!("❌ 下单接口 403：这个区域可能不允许交易（{}）", clean_error(&e));
                } else {
                    println!(
                        "✅ 下单接口在这个区域可用（空订单被拒为无效：HTTP {}，这是预期的）",
                        if status != 0 { status.to_string() } else { "?".into() }
                    );
                }
            }
        }
    }
    println!("结论：{}", if ok { "可以开 RULE_TAKER=on" } else { "先把上面的 ❌ 解决" });
    if ok { 0 } else { 1 }
}

fn plan(config: &Config, days: i64) -> i32 {
    let now = now_ms();
    let s = &config.rules;
    println!("接下来 {days} 天的规则场次（北京时间｜纽约时间）；RULE_TAKER={}，RULE_LEAD_MINUTES={}", s.mode, s.lead_minutes);
    for rule in rules::RULES.iter() {
        let (price, live) = (s.price(rule.key), rules::live_price_of(s, rule));
        let mut state = if rules::live_rule(s, rule) { format!("≤{price}¢") } else { "盘前不买".into() };
        if live != 0 {
            state += &format!("，盘中 ≤{live}¢");
        }
        if !rules::live_rule(s, rule) && live == 0 {
            state = "不做".into();
        }
        let starts = rules::windows(rule, now, now + days * 86_400_000);
        let shown: Vec<String> = starts.iter().map(|t| format!("{}｜{}", bj(*t, true), clock::et_clock(*t))).collect();
        println!(
            "· {}（买{}，{state}）：{}",
            rule.name,
            rule.word(),
            if shown.is_empty() { "没有".into() } else { shown.join("、") }
        );
    }
    0
}

fn usage() -> &'static str {
    "用法：ruletaker [run | latency | check | plan] [--env 文件] [--no-order-probe]\n\
     \x20 run      运行服务（默认）\n\
     \x20 latency  只读：量这台机器到 Predict 的网络延迟\n\
     \x20 check    只读：设置、登录、账户、余额、授权、下单接口是否对这个区域开放\n\
     \x20 plan     不联网：接下来两天的规则场次\n\
     \x20 --env    环境变量文件（默认 /etc/ruletaker/ruletaker.env 或 ./.env）"
}

fn main() {
    let mut command = "run".to_string();
    let mut env_file: Option<String> = None;
    let mut probe = true;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "run" | "latency" | "check" | "plan" => command = arg,
            "--env" => env_file = args.next(),
            "--no-order-probe" => probe = false,
            "-h" | "--help" => {
                println!("{}", usage());
                return;
            }
            "-V" | "--version" => {
                println!("ruletaker {VERSION}");
                return;
            }
            other if other.starts_with("--env=") => env_file = Some(other["--env=".len()..].to_string()),
            other => {
                eprintln!("不认识的参数：{other}\n{}", usage());
                std::process::exit(2);
            }
        }
    }
    let env = load_env(env_file.as_deref());
    let config = match Config::from_env(&env) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("设置有误：{e}");
            std::process::exit(2);
        }
    };
    util::set_log_level(&config.log_level);
    if command == "plan" {
        std::process::exit(plan(&config, 2));
    }
    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("a runtime");
    let local = tokio::task::LocalSet::new();
    let code = local.block_on(&runtime, async move {
        match command.as_str() {
            "latency" => latency(config, 10).await,
            "check" => check(config, probe).await,
            _ => run_service(config, env).await,
        }
    });
    std::process::exit(code);
}
