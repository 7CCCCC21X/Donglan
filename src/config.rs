//! The service's settings, from the environment (an EnvironmentFile on the VM, or a .env file next to the code).
//!
//! The rule settings keep huice's names and limits (RULE_TAKER, RULE_*_CENTS, RULE_GOLD / RULE_OIL, RULE_*_SHARES,
//! RULE_MAX_DAY_USD, RULE_FAST_HOURS, RULE_LEAD_MINUTES, RULE_*_LIVE_CENTS), so the values set on Railway carry over as they are.
use crate::clock::{self, DEFAULT_CME_CLOSED, DEFAULT_CME_EARLY, Spans, day_spans, in_spans, parse_dates, spans_in};
use crate::util::g;
use std::collections::{BTreeSet, HashMap};

pub type Env = HashMap<String, String>;

pub const RULE_SETTING_KEYS: [(&str, &str); 8] = [
    ("gold_shut", "RULE_GOLD_SHUT_CENTS"),
    ("gold_open", "RULE_GOLD_OPEN_CENTS"),
    ("gold_0515", "RULE_GOLD_0515_CENTS"),
    ("gold_0500", "RULE_GOLD_0500_CENTS"),
    ("gold_1645", "RULE_GOLD_1645_CENTS"),
    ("oil_shut", "RULE_OIL_SHUT_CENTS"),
    ("oil_open", "RULE_OIL_OPEN_CENTS"),
    ("oil_0045", "RULE_OIL_0045_CENTS"),
];
pub const RULE_DEFAULT_CENTS: [(&str, i64); 8] = [
    ("gold_shut", 58),
    ("gold_open", 50),
    ("gold_0515", 58),
    ("gold_0500", 0),
    ("gold_1645", 0),
    ("oil_shut", 0),
    ("oil_open", 50),
    ("oil_0045", 0),
];
pub const RULE_MARKET_KEYS: [(&str, &str, &str); 2] =
    [("gold", "RULE_GOLD", "RULE_GOLD_SHARES"), ("oil", "RULE_OIL", "RULE_OIL_SHARES")];
pub const RULE_LIVE_KEYS: [(&str, &str); 2] =
    [("gold_shut", "RULE_GOLD_SHUT_LIVE_CENTS"), ("gold_open", "RULE_GOLD_OPEN_LIVE_CENTS")];
pub const RULE_FAST_DEFAULT: &str = "01:00-07:00";

pub fn default_cents(key: &str) -> i64 {
    RULE_DEFAULT_CENTS.iter().find(|(k, _)| *k == key).map(|(_, c)| *c).unwrap_or(0)
}

/// KEY=VALUE lines (comments, blank lines and an optional ``export`` allowed; quotes around a value taken off). Values already
/// in ``into`` win (the process environment over the file).
pub fn load_env_file(path: &str, into: &mut Env) {
    let Ok(text) = std::fs::read_to_string(path) else { return };
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') || !line.contains('=') {
            continue;
        }
        let line = line.strip_prefix("export ").unwrap_or(line);
        let (name, value) = line.split_once('=').unwrap_or((line, ""));
        let (name, mut value) = (name.trim().to_string(), value.trim().to_string());
        if value.contains('#') && !value.starts_with('\'') && !value.starts_with('"') {
            value = value.split(" #").next().unwrap_or("").trim().to_string();
        }
        let b = value.as_bytes();
        if b.len() >= 2 && b[0] == b[b.len() - 1] && (b[0] == b'\'' || b[0] == b'"') {
            value = value[1..value.len() - 1].to_string();
        }
        into.entry(name).or_insert(value);
    }
}

/// The name the private key is kept under when it is kept encrypted on the VM (deploy/gcp/vm-key.sh, bash deploy/gcp/rt.sh key):
/// systemd decrypts it for the service alone (LoadCredentialEncrypted= in deploy/gcp/ruletaker.service) into a directory of
/// the service's own in memory — $CREDENTIALS_DIRECTORY, read by its user only, gone when it stops.
pub const KEY_CREDENTIAL: &str = "predict_private_key";

/// What systemd handed the service: whether it runs with a credentials directory at all (the VM's unit gives it one), and the
/// private key kept there ("" none). No Debug: never printed.
#[derive(Clone, Default)]
pub struct Stored {
    pub store: bool,
    pub key: String,
}

impl Stored {
    /// From $CREDENTIALS_DIRECTORY (unset outside systemd).
    pub fn from_systemd() -> Result<Stored, String> {
        Stored::from_dir(std::env::var_os("CREDENTIALS_DIRECTORY"))
    }

    /// From a credentials directory (None: not run by systemd). A key that is there but cannot be read is an error, never
    /// "no key".
    pub fn from_dir(dir: Option<std::ffi::OsString>) -> Result<Stored, String> {
        let Some(dir) = dir.filter(|d| !d.is_empty()) else {
            return Ok(Stored::default());
        };
        let path = std::path::Path::new(&dir).join(KEY_CREDENTIAL);
        match std::fs::read_to_string(&path) {
            Ok(text) => Ok(Stored { store: true, key: text.trim().to_string() }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Stored { store: true, key: String::new() }),
            Err(e) => Err(format!("读不到加密保存的私钥（{}：{e}）", path.display())),
        }
    }
}

/// A private key as the wallet takes it: "0x" and 64 hex digits (the 0x may be left off); "" stays "" (none).
pub fn key_of(raw: &str) -> Option<String> {
    let k = raw.trim();
    if k.is_empty() {
        return Some(String::new());
    }
    let k = if k.starts_with("0x") { k.to_string() } else { format!("0x{k}") };
    (k.len() == 66 && k[2..].bytes().all(|c| c.is_ascii_hexdigit())).then_some(k)
}

fn get<'a>(e: &'a Env, name: &str, default: &'a str) -> &'a str {
    e.get(name).map(String::as_str).unwrap_or(default)
}

/// ``int(str(v).strip() or 0)``
fn int_of(name: &str, raw: &str) -> Result<i64, String> {
    let t = raw.trim();
    if t.is_empty() {
        return Ok(0);
    }
    t.replace('_', "").parse::<i64>().map_err(|_| format!("{name} 应为整数，现在是“{t}”"))
}

/// RULE_TAKER off | paper (只记录，不下单) | on (真实吃单); each market on or off with the most shares bought a window; each
/// rule's highest price in cents (0 = that rule off); the most spent a Beijing day; the per-second hours; the makers' lead;
/// the prices in the windows' course.
#[derive(Clone, Debug, PartialEq)]
pub struct RuleSettings {
    pub mode: String,
    pub cents: Vec<(String, i64)>,
    pub markets: BTreeSet<String>,
    pub shares: Vec<(String, i64)>,
    pub max_day_usd: f64,
    pub fast: Spans,
    pub live: Vec<(String, i64)>,
    pub lead_minutes: i64,
}

impl Default for RuleSettings {
    fn default() -> RuleSettings {
        RuleSettings {
            mode: "off".into(),
            cents: RULE_DEFAULT_CENTS.iter().map(|(k, c)| (k.to_string(), *c)).collect(),
            markets: ["gold", "oil"].iter().map(|s| s.to_string()).collect(),
            shares: vec![("gold".into(), 50), ("oil".into(), 50)],
            max_day_usd: 0.0,
            fast: vec![(60, 420)],
            live: vec![],
            lead_minutes: 300,
        }
    }
}

impl RuleSettings {
    pub fn enabled(&self) -> bool {
        self.mode != "off"
    }
    pub fn fast_at(&self, now_ms: i64) -> bool {
        in_spans(&self.fast, now_ms)
    }
    pub fn fast_in(&self, now_ms: i64) -> Option<i64> {
        spans_in(&self.fast, now_ms)
    }
    pub fn price(&self, key: &str) -> i64 {
        self.cents.iter().find(|(k, _)| k == key).map(|(_, c)| *c).unwrap_or(0)
    }
    pub fn on(&self, market: &str) -> bool {
        self.markets.contains(market)
    }
    pub fn live_price(&self, key: &str) -> i64 {
        self.live.iter().find(|(k, _)| k == key).map(|(_, c)| *c).unwrap_or(0)
    }
    pub fn shares_of(&self, market: &str) -> i64 {
        self.shares.iter().find(|(k, _)| k == market).map(|(_, c)| *c).unwrap_or(0)
    }
    pub fn with_mode(&self, mode: &str) -> RuleSettings {
        RuleSettings { mode: mode.to_string(), ..self.clone() }
    }

    pub fn parse(e: &Env, mode: Option<&str>) -> Result<RuleSettings, String> {
        let mode = mode.unwrap_or_else(|| get(e, "RULE_TAKER", "off")).trim().to_lowercase();
        if !matches!(mode.as_str(), "off" | "paper" | "on") {
            return Err("RULE_TAKER 只能是 off、paper（只记录不下单）或 on（真实吃单）".into());
        }
        let mut cents = vec![];
        for (key, name) in RULE_SETTING_KEYS {
            let raw = e.get(name).cloned().unwrap_or_else(|| default_cents(key).to_string());
            cents.push((key.to_string(), int_of(name, &raw)?));
        }
        let switches: Vec<(String, String)> =
            RULE_MARKET_KEYS.iter().map(|(m, switch, _)| (m.to_string(), get(e, switch, "on").trim().to_lowercase())).collect();
        let default = {
            let d = get(e, "RULE_SHARES", "50").trim();
            if d.is_empty() { "50".to_string() } else { d.to_string() }
        };
        let mut shares = vec![];
        for (m, _, name) in RULE_MARKET_KEYS {
            let raw = e.get(name).cloned().unwrap_or_else(|| default.clone());
            shares.push((m.to_string(), int_of(name, &raw)?));
        }
        let max_raw = get(e, "RULE_MAX_DAY_USD", "0").trim();
        let max_day: f64 = if max_raw.is_empty() {
            0.0
        } else {
            max_raw.parse().map_err(|_| format!("RULE_MAX_DAY_USD 应为数字，现在是“{max_raw}”"))?
        };
        let fast = day_spans(get(e, "RULE_FAST_HOURS", RULE_FAST_DEFAULT), "RULE_FAST_HOURS")?;
        let lead = int_of("RULE_LEAD_MINUTES", get(e, "RULE_LEAD_MINUTES", "300"))?;
        if !(0..=1440).contains(&lead) {
            return Err(
                "RULE_LEAD_MINUTES 0～1440（对手提前多少分钟挂单，规则吃单在那一刻前后每秒检查；0 = 不按这个时刻）".into()
            );
        }
        let mut live = vec![];
        for (key, name) in RULE_LIVE_KEYS {
            live.push((key.to_string(), int_of(name, get(e, name, "0"))?));
        }
        if live.iter().any(|(_, c)| !(*c == 0 || (1..=97).contains(c))) {
            return Err("RULE_*_LIVE_CENTS 是 0（盘中不做）或 1～97（¢，开场后的最高买价）".into());
        }
        if switches.iter().any(|(_, v)| v != "on" && v != "off") {
            return Err("RULE_GOLD、RULE_OIL 只能是 on 或 off".into());
        }
        if cents.iter().any(|(_, c)| !(*c == 0 || (1..=95).contains(c)))
            || shares.iter().any(|(_, n)| !(1..=100_000).contains(n))
            || !(0.0..=1_000_000.0).contains(&max_day)
        {
            return Err("RULE_*_CENTS 是 0（不做这条规则）或 1～95（¢，最高买价）；RULE_GOLD_SHARES、RULE_OIL_SHARES 每场最多买 1～100000 份；\
                        RULE_MAX_DAY_USD 0～1000000（每天最多花多少，0 = 不限）"
                .into());
        }
        Ok(RuleSettings {
            mode,
            cents,
            markets: switches.into_iter().filter(|(_, v)| v == "on").map(|(m, _)| m).collect(),
            shares,
            max_day_usd: max_day,
            fast,
            live,
            lead_minutes: lead,
        })
    }
}

pub fn bounded(e: &Env, name: &str, default: f64, low: f64, high: f64, integer: bool) -> Result<f64, String> {
    let raw = get(e, name, "").trim();
    let value = if raw.is_empty() {
        default
    } else if integer {
        raw.parse::<i64>().map(|v| v as f64).map_err(|_| format!("{name} 应为数字，现在是“{raw}”"))?
    } else {
        raw.parse::<f64>().map_err(|_| format!("{name} 应为数字，现在是“{raw}”"))?
    };
    if !(low <= value && value <= high) {
        return Err(format!("{name} 应在 {}～{} 之间，现在是 {}", g(low), g(high), g(value)));
    }
    Ok(value)
}

/// LIVE_TRADING_ALLOWED: only unset, or on / 1 / true / yes, allows real orders — anything else (empty, a typo, off) does not.
pub fn allowed(value: Option<&str>) -> bool {
    match value {
        None => true,
        Some(v) => matches!(v.trim().to_lowercase().as_str(), "on" | "1" | "true" | "yes"),
    }
}

#[derive(Clone)]
pub struct Config {
    pub rules: RuleSettings,
    pub trading_allowed: bool,
    pub api_key: String,
    pub own_key: bool,
    /// the spare keys (RULE_API_KEY_2 … RULE_API_KEY_9, in that order; empty ones and repeats of the main one left out): a 429
    /// on the key in use moves the requests to the next free one
    pub api_spares: Vec<String>,
    pub private_key: String,
    /// where the private key came from: "stored" (kept encrypted on this machine), "settings" (the settings file: plain text),
    /// "both" (the same key in both: the plain copy is to go), "" (none)
    pub key_kept: &'static str,
    /// run with systemd's credentials directory (the VM's unit): the key could be kept encrypted here
    pub key_store: bool,
    pub account: String,
    pub chain_id: i64,
    pub rpc_url: String,
    pub fee_bps: i64,
    pub rate_per_min: f64,
    /// how often the REST read beside the pushes may run at the makers' moment (ms; RULE_VERIFY_MS), never above 80% of the
    /// key's budget
    pub verify_ms: f64,
    pub max_daily_loss_usd: f64,
    pub state_db: String,
    pub telegram_token: String,
    pub telegram_command_token: String,
    pub telegram_chats: Vec<(i64, i64)>,
    pub admin_user_id: i64,
    pub telegram_commands: bool,
    pub notify: bool,
    pub status_host: String,
    pub status_port: u16,
    pub status_key: String,
    pub ws_connections: usize,
    pub binance_fapi: String,
    pub binance_ws: String,
    pub region: String,
    pub log_level: String,
}

impl Default for Config {
    fn default() -> Config {
        Config {
            rules: RuleSettings::default(),
            trading_allowed: true,
            api_key: String::new(),
            own_key: false,
            api_spares: vec![],
            private_key: String::new(),
            key_kept: "",
            key_store: false,
            account: String::new(),
            chain_id: 56,
            rpc_url: String::new(),
            fee_bps: 200,
            rate_per_min: 220.0,
            verify_ms: 340.0,
            max_daily_loss_usd: 0.0,
            state_db: "data/ruletaker.sqlite3".into(),
            telegram_token: String::new(),
            telegram_command_token: String::new(),
            telegram_chats: vec![],
            admin_user_id: 0,
            telegram_commands: false,
            notify: true,
            status_host: "127.0.0.1".into(),
            status_port: 8080,
            status_key: String::new(),
            ws_connections: 1,
            binance_fapi: "https://fapi.binance.com".into(),
            binance_ws: "wss://fstream.binance.com/market".into(),
            region: String::new(),
            log_level: "INFO".into(),
        }
    }
}

/// Printed without its secrets — the private key, the API key, the Telegram tokens, the page's key: a ``{:?}`` anywhere (a log
/// line, a panic, a test's message) shows only whether each is set.
impl std::fmt::Debug for Config {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Config {{ {} }}", self.secrets_line())
    }
}

impl Config {
    pub fn live(&self) -> bool {
        self.rules.mode == "on"
    }

    /// The settings alone (no key kept encrypted: a test, a laptop).
    pub fn from_env(e: &Env) -> Result<Config, String> {
        Config::load(e, &Stored::default())
    }

    /// The settings, and the private key kept encrypted on this machine if there is one. The settings file's
    /// PREDICT_PRIVATE_KEY is used only when none is kept; the same key in both is that key (its plain copy reported, to go);
    /// two different keys are refused — never one of them picked behind the user's back.
    pub fn load(e: &Env, stored: &Stored) -> Result<Config, String> {
        let rules = RuleSettings::parse(e, None)?;
        clock::configure(
            Some(parse_dates(get(e, "CME_EARLY_DAYS", DEFAULT_CME_EARLY), "CME_EARLY_DAYS")?),
            Some(parse_dates(get(e, "CME_CLOSED_DAYS", DEFAULT_CME_CLOSED), "CME_CLOSED_DAYS")?),
        );
        let own = get(e, "RULE_API_KEY", "").trim().to_string();
        let shared = get(e, "PREDICT_API_KEY", "").trim().to_string();
        let key = if own.is_empty() { shared.clone() } else { own.clone() };
        let plain = key_of(get(e, "PREDICT_PRIVATE_KEY", "")).ok_or("PREDICT_PRIVATE_KEY 应为 64 位十六进制（0x 可省）")?;
        let kept = key_of(&stored.key).ok_or("加密保存的私钥不是 64 位十六进制：运行 bash deploy/gcp/rt.sh key 重新设定")?;
        let (private_key, key_kept) = match (kept.is_empty(), plain.is_empty()) {
            (true, true) => (String::new(), ""),
            (false, true) => (kept, "stored"),
            (true, false) => (plain, "settings"),
            (false, false) if kept.eq_ignore_ascii_case(&plain) => (kept, "both"),
            _ => {
                return Err("设置文件里的 PREDICT_PRIVATE_KEY 和加密保存的私钥不是同一把：要换成设置文件里这把，运行 \
                            bash deploy/gcp/rt.sh edit 存一次（会把它收进加密保存）；不换就删掉设置文件里那一行"
                    .into());
            }
        };
        let account = get(e, "PREDICT_ACCOUNT", "").trim().to_string();
        if !account.is_empty() && (!account.starts_with("0x") || account.len() != 42) {
            return Err("PREDICT_ACCOUNT 应为 0x 开头的 42 位地址（网站账户设置里的充值地址），用普通钱包交易就留空".into());
        }
        if rules.mode == "on" && private_key.is_empty() {
            return Err("RULE_TAKER=on 需要 PREDICT_PRIVATE_KEY（签名钱包私钥）".into());
        }
        let chain_raw = get(e, "PREDICT_CHAIN_ID", "56").trim();
        let chain_guess: i64 = if chain_raw.is_empty() { 56 } else { chain_raw.parse().unwrap_or(0) };
        if rules.mode != "off" && key.is_empty() && chain_guess == 56 {
            return Err("RULE_TAKER=paper / on 需要 PREDICT_API_KEY（或 RULE_API_KEY）：Predict 主网接口要 API Key".into());
        }
        let mut chats = vec![];
        for item in get(e, "TELEGRAM_CHAT_ID", "").replace('，', ",").split(',') {
            let item = item.trim();
            if item.is_empty() {
                continue;
            }
            let (chat, thread) = item.split_once(':').unwrap_or((item, ""));
            let bad = || format!("TELEGRAM_CHAT_ID 写成数字，群组话题写成 群ID:话题ID，多个用逗号分开；现在是“{item}”");
            let chat: i64 = chat.trim().parse().map_err(|_| bad())?;
            let thread: i64 = if thread.trim().is_empty() { 0 } else { thread.trim().parse().map_err(|_| bad())? };
            chats.push((chat, thread));
        }
        let ws = bounded(e, "RULE_WS_CONNECTIONS", 1.0, 1.0, 3.0, true)? as usize;
        let port = bounded(e, "STATUS_PORT", 8080.0, 0.0, 65535.0, true)? as u16;
        let mut status_host = get(e, "STATUS_HOST", "127.0.0.1").trim().to_string();
        if status_host.is_empty() {
            status_host = "127.0.0.1".into();
        }
        let status_key = get(e, "STATUS_KEY", "").trim().to_string();
        if !matches!(status_host.as_str(), "127.0.0.1" | "localhost" | "::1") && status_key.chars().count() < 12 {
            return Err("STATUS_HOST 不是本机时必须设置 STATUS_KEY（至少 12 位），否则任何人都能看状态、暂停".into());
        }
        let url = |name: &str, default: &str| {
            let v = get(e, name, default).trim().trim_end_matches('/').to_string();
            if v.is_empty() { default.to_string() } else { v }
        };
        let mut log_level = get(e, "LOG_LEVEL", "INFO").trim().to_uppercase();
        if log_level.is_empty() {
            log_level = "INFO".into();
        }
        Ok(Config {
            rules,
            trading_allowed: allowed(e.get("LIVE_TRADING_ALLOWED").map(String::as_str)),
            own_key: !own.is_empty() && own != shared,
            api_spares: {
                let mut spares: Vec<String> = vec![];
                for n in 2..=9 {
                    let k = get(e, &format!("RULE_API_KEY_{n}"), "").trim().to_string();
                    if !k.is_empty() && k != key && !spares.contains(&k) {
                        spares.push(k);
                    }
                }
                spares
            },
            api_key: key,
            private_key,
            key_kept,
            key_store: stored.store,
            account,
            chain_id: bounded(e, "PREDICT_CHAIN_ID", 56.0, 1.0, 1e9, true)? as i64,
            rpc_url: get(e, "BSC_RPC_URL", "").trim().trim_end_matches('/').to_string(),
            fee_bps: bounded(e, "PREDICT_FEE_BPS", 200.0, 0.0, 1000.0, true)? as i64,
            rate_per_min: bounded(e, "PREDICT_RATE_PER_MIN", 220.0, 0.0, 6000.0, false)?,
            verify_ms: bounded(e, "RULE_VERIFY_MS", 340.0, 50.0, 2000.0, false)?,
            max_daily_loss_usd: bounded(e, "RULE_MAX_DAILY_LOSS_USD", 0.0, 0.0, 1_000_000.0, false)?,
            state_db: {
                let v = get(e, "STATE_DB", "").trim();
                if v.is_empty() { "data/ruletaker.sqlite3".to_string() } else { v.to_string() }
            },
            telegram_token: get(e, "TELEGRAM_BOT_TOKEN", "").trim().to_string(),
            telegram_command_token: get(e, "TELEGRAM_COMMAND_TOKEN", "").trim().to_string(),
            telegram_chats: chats,
            admin_user_id: bounded(e, "ADMIN_USER_ID", 0.0, 0.0, 1e15, true)? as i64,
            telegram_commands: matches!(
                get(e, "TELEGRAM_COMMANDS", "off").trim().to_lowercase().as_str(),
                "on" | "1" | "true" | "yes"
            ),
            notify: !matches!(get(e, "RULE_NOTIFY", "on").trim().to_lowercase().as_str(), "off" | "0" | "false" | "no"),
            status_host,
            status_port: port,
            status_key,
            ws_connections: ws,
            binance_fapi: url("BINANCE_BASE_URL", "https://fapi.binance.com"),
            binance_ws: url("BINANCE_WS_URL", "wss://fstream.binance.com/market"),
            region: get(e, "RULE_REGION", "").trim().to_string(),
            log_level,
        })
    }

    /// Which secrets are set (never their values): the deploy's first log line.
    /// (Also what a ``{:?}`` of the whole configuration shows: see the Debug below.)
    pub fn secrets_line(&self) -> String {
        format!(
            "secrets configured: predict_api_key={} own_rule_key={} spare_api_keys={} predict_private_key={} predict_account={} \
             telegram_token={} telegram_command_token={} telegram_commands={} status_key={} mode={} trading_allowed={} \
             private_key_kept={}",
            py_bool(!self.api_key.is_empty()),
            py_bool(self.own_key),
            self.api_spares.len(),
            py_bool(!self.private_key.is_empty()),
            py_bool(!self.account.is_empty()),
            py_bool(!self.telegram_token.is_empty()),
            py_bool(!self.telegram_command_token.is_empty()),
            py_bool(self.telegram_commands),
            py_bool(!self.status_key.is_empty()),
            self.rules.mode,
            py_bool(self.trading_allowed),
            if self.key_kept.is_empty() { "none" } else { self.key_kept }
        )
    }
}

fn py_bool(b: bool) -> &'static str {
    if b { "True" } else { "False" }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(pairs: &[(&str, &str)]) -> Env {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    #[test]
    fn settings() {
        let s = RuleSettings::parse(&env(&[("RULE_TAKER", "paper"), ("RULE_GOLD_SHARES", "900"), ("RULE_OIL", "off")]), None)
            .unwrap();
        assert_eq!(s.mode, "paper");
        assert_eq!(s.shares_of("gold"), 900);
        assert_eq!(s.shares_of("oil"), 50);
        assert!(s.on("gold") && !s.on("oil"));
        assert_eq!(s.price("gold_shut"), 58);
        assert_eq!(s.price("oil_open"), 50);
        assert_eq!(s.fast, vec![(60, 420)]);
        assert!(RuleSettings::parse(&env(&[("RULE_TAKER", "maybe")]), None).is_err());
        assert!(RuleSettings::parse(&env(&[("RULE_GOLD_SHUT_CENTS", "96")]), None).is_err());
        assert!(RuleSettings::parse(&env(&[("RULE_GOLD_SHUT_LIVE_CENTS", "98")]), None).is_err());
        assert!(RuleSettings::parse(&env(&[("RULE_LEAD_MINUTES", "1441")]), None).is_err());
    }

    #[test]
    fn config() {
        let c =
            Config::from_env(&env(&[("RULE_TAKER", "paper"), ("PREDICT_API_KEY", "k"), ("TELEGRAM_CHAT_ID", "-100123:7, 55")]))
                .unwrap();
        assert_eq!(c.telegram_chats, vec![(-100123, 7), (55, 0)]);
        assert!(!c.own_key);
        assert!(Config::from_env(&env(&[("RULE_TAKER", "paper")])).is_err());
        assert!(Config::from_env(&env(&[("STATUS_HOST", "0.0.0.0"), ("STATUS_KEY", "short")])).is_err());
        assert!(Config::from_env(&env(&[("STATUS_PORT", "70000")])).unwrap_err().contains("STATUS_PORT 应在 0～65535 之间"));
        assert!(!allowed(Some("")) && allowed(None) && allowed(Some("ON")));
    }
}

#[cfg(test)]
mod secret_tests {
    use super::*;

    #[test]
    fn a_printed_configuration_never_shows_a_secret() {
        let key = format!("0x{}", "ab12".repeat(16));
        let env: Env = [
            ("RULE_TAKER", "paper"),
            ("PREDICT_PRIVATE_KEY", key.as_str()),
            ("PREDICT_API_KEY", "api-key-secret-123"),
            ("TELEGRAM_BOT_TOKEN", "123456:telegram-secret-token"),
            ("STATUS_KEY", "status-secret-key"),
        ]
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
        let config = Config::from_env(&env).unwrap();
        let shown = format!("{config:?} {config:#?}");
        for secret in [&key[2..], "api-key-secret-123", "telegram-secret-token", "status-secret-key"] {
            assert!(!shown.contains(secret), "{shown}");
        }
        assert!(shown.contains("predict_private_key=True"), "{shown}");
    }
}

#[cfg(test)]
mod key_tests {
    use super::*;

    const A: &str = "a4c123b1612dd272d1371c17149d439536b3216fdaeeb975729fae923d5a4fd1";

    fn load(pairs: &[(&str, &str)], store: bool, key: &str) -> Result<Config, String> {
        let e: Env = pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        Config::load(&e, &Stored { store, key: key.to_string() })
    }

    #[test]
    fn the_key_kept_encrypted_is_used_and_two_different_keys_are_refused() {
        let a = format!("0x{A}");
        // kept encrypted alone: used (0x added; a line break left by whoever wrote it taken off)
        let c = load(&[("RULE_TAKER", "on"), ("PREDICT_API_KEY", "k")], true, &format!("{A}\n")).unwrap();
        assert_eq!((c.private_key.as_str(), c.key_kept, c.key_store), (a.as_str(), "stored", true));
        // the settings file's alone: not run by systemd (a laptop) ...
        let c = load(&[("PREDICT_PRIVATE_KEY", A)], false, "").unwrap();
        assert_eq!((c.private_key.as_str(), c.key_kept, c.key_store), (a.as_str(), "settings", false));
        // ... or on the VM with none kept yet
        let c = load(&[("PREDICT_PRIVATE_KEY", A)], true, "").unwrap();
        assert_eq!((c.key_kept, c.key_store), ("settings", true));
        // the same key in both (letter case and the 0x aside): that key, the plain copy reported
        let c = load(&[("PREDICT_PRIVATE_KEY", &A.to_uppercase())], true, &a).unwrap();
        assert_eq!((c.private_key.as_str(), c.key_kept), (a.as_str(), "both"));
        // two different keys: refused, whichever way round
        let other = "1".repeat(64);
        assert!(load(&[("PREDICT_PRIVATE_KEY", &other)], true, A).unwrap_err().contains("不是同一把"));
        assert!(load(&[("PREDICT_PRIVATE_KEY", A)], true, &other).unwrap_err().contains("不是同一把"));
        // a kept "key" that is none: refused, and said to be the kept one
        assert!(load(&[], true, "0x12").unwrap_err().contains("加密保存的私钥"));
        assert!(load(&[("PREDICT_PRIVATE_KEY", "0x12")], true, A).unwrap_err().contains("PREDICT_PRIVATE_KEY 应为"));
        // none anywhere: on refused as before, paper fine
        assert!(load(&[("RULE_TAKER", "on"), ("PREDICT_API_KEY", "k")], true, "").unwrap_err().contains("PREDICT_PRIVATE_KEY"));
        let c = load(&[("RULE_TAKER", "paper"), ("PREDICT_API_KEY", "k")], true, "").unwrap();
        assert_eq!((c.private_key.as_str(), c.key_kept), ("", ""));
        assert!(c.secrets_line().ends_with("private_key_kept=none"), "{}", c.secrets_line());
        let c = load(&[], true, A).unwrap();
        assert!(c.secrets_line().ends_with("private_key_kept=stored"));
        assert!(!format!("{c:?}").contains(A));
    }

    #[test]
    fn systemd_s_directory_is_read_and_an_unreadable_key_is_no_silent_none() {
        assert!(!Stored::from_dir(None).unwrap().store);
        assert!(!Stored::from_dir(Some("".into())).unwrap().store);
        let dir = std::env::temp_dir().join(format!("rt-creds-{}-{}", std::process::id(), crate::util::now_ms()));
        std::fs::create_dir_all(&dir).unwrap();
        let s = Stored::from_dir(Some(dir.clone().into_os_string())).unwrap();
        assert!(s.store && s.key.is_empty(), "a directory without the key: none kept");
        std::fs::write(dir.join(KEY_CREDENTIAL), format!("0x{A}\n")).unwrap();
        let s = Stored::from_dir(Some(dir.clone().into_os_string())).unwrap();
        assert_eq!(s.key, format!("0x{A}"));
        std::fs::remove_file(dir.join(KEY_CREDENTIAL)).unwrap();
        std::fs::create_dir(dir.join(KEY_CREDENTIAL)).unwrap();
        let e = Stored::from_dir(Some(dir.clone().into_os_string())).err().expect("an unreadable key is an error");
        assert!(e.contains("读不到加密保存的私钥"), "{e}");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
