//! Small shared helpers: the clock zones, errors, the text forms, the records' JSON read the way the Python version reads its
//! dicts (`float(o.get(k) or 0)` and so on).
use serde_json::{Map, Value};
use std::fmt;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU8, Ordering};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

pub const DAY_MS: i64 = 86_400_000;
pub const BEIJING_MS: i64 = 8 * 3_600_000;
pub const DUST: f64 = 1e-6;

// --- time ---------------------------------------------------------------------------------------------------------------
pub fn now_ms() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
}

static START: OnceLock<Instant> = OnceLock::new();

/// Seconds on a monotonic clock (Python's time.monotonic()).
pub fn mono() -> f64 {
    START.get_or_init(Instant::now).elapsed().as_secs_f64()
}

/// Days since 1970-01-01 -> (year, month, day) (the proleptic Gregorian calendar).
pub fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// (year, month, day) -> days since 1970-01-01.
pub fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let m = m as i64;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Monday 0 … Sunday 6 of a day number.
pub fn weekday(days: i64) -> u32 {
    ((days + 3).rem_euclid(7)) as u32
}

/// A date as a day number since 1970-01-01, with its ISO text.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Date(pub i64);

impl Date {
    pub fn ymd(y: i64, m: u32, d: u32) -> Date {
        Date(days_from_civil(y, m, d))
    }
    pub fn parts(self) -> (i64, u32, u32) {
        civil_from_days(self.0)
    }
    pub fn weekday(self) -> u32 {
        weekday(self.0)
    }
    pub fn add(self, days: i64) -> Date {
        Date(self.0 + days)
    }
    /// "2026-12-25" (strictly YYYY-MM-DD, a real date).
    pub fn parse(text: &str) -> Option<Date> {
        let t = text.trim();
        let b = t.as_bytes();
        if b.len() != 10
            || b[4] != b'-'
            || b[7] != b'-'
            || !t.bytes().enumerate().all(|(i, c)| i == 4 || i == 7 || c.is_ascii_digit())
        {
            return None;
        }
        let (y, m, d) = (t[0..4].parse::<i64>().ok()?, t[5..7].parse::<u32>().ok()?, t[8..10].parse::<u32>().ok()?);
        if !(1..=12).contains(&m) || d == 0 || d > 31 {
            return None;
        }
        let date = Date::ymd(y, m, d);
        if date.parts() != (y, m, d) {
            return None;
        }
        Some(date)
    }
}

impl fmt::Display for Date {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (y, m, d) = self.parts();
        write!(f, "{y:04}-{m:02}-{d:02}")
    }
}

/// (date, hour, minute, second, ms) of an instant shifted by ``offset_ms``.
pub fn wall(ms: i64, offset_ms: i64) -> (Date, u32, u32, u32, u32) {
    let local = ms + offset_ms;
    let day = local.div_euclid(DAY_MS);
    let tod = local.rem_euclid(DAY_MS);
    (Date(day), (tod / 3_600_000) as u32, (tod / 60_000 % 60) as u32, (tod / 1000 % 60) as u32, (tod % 1000) as u32)
}

/// Beijing time of an instant: 11-18 06:45 (or 06:45 with ``day`` false).
pub fn bj(ms: i64, day: bool) -> String {
    let (d, h, mi, _, _) = wall(ms, BEIJING_MS);
    if day {
        let (_, mo, dd) = d.parts();
        format!("{mo:02}-{dd:02} {h:02}:{mi:02}")
    } else {
        format!("{h:02}:{mi:02}")
    }
}

/// Beijing time to the millisecond: 01:45:02.359.
pub fn bj_clock(ms: i64) -> String {
    let (_, h, mi, s, _) = wall(ms, BEIJING_MS);
    format!("{h:02}:{mi:02}:{s:02}.{:03}", ms.rem_euclid(1000))
}

/// The Beijing date of an instant, as the records keep it (2026-11-18).
pub fn day_of(ms: i64) -> String {
    wall(ms, BEIJING_MS).0.to_string()
}

// --- logging ------------------------------------------------------------------------------------------------------------
static LEVEL: AtomicU8 = AtomicU8::new(2); // 1 debug, 2 info, 3 warning, 4 error

pub fn set_log_level(name: &str) {
    let level = match name.trim().to_ascii_uppercase().as_str() {
        "DEBUG" => 1,
        "WARNING" | "WARN" => 3,
        "ERROR" | "CRITICAL" => 4,
        _ => 2,
    };
    LEVEL.store(level, Ordering::Relaxed);
}

pub fn log_enabled(level: u8) -> bool {
    level >= LEVEL.load(Ordering::Relaxed)
}

pub fn log_line(level: u8, text: &str) {
    if !log_enabled(level) {
        return;
    }
    let name = match level {
        1 => "DEBUG",
        2 => "INFO",
        3 => "WARNING",
        _ => "ERROR",
    };
    let now = now_ms();
    let (d, h, mi, s, ms) = wall(now, BEIJING_MS);
    // (a log that cannot be written — journald restarting — is dropped, never a panic in the middle of an order)
    use std::io::Write;
    let _ = writeln!(std::io::stderr(), "{d} {h:02}:{mi:02}:{s:02}.{ms:03} {name} {}", clean_error_keep(text));
}

#[macro_export]
macro_rules! debug { ($($t:tt)*) => { if $crate::util::log_enabled(1) { $crate::util::log_line(1, &format!($($t)*)) } } }
#[macro_export]
macro_rules! info { ($($t:tt)*) => { $crate::util::log_line(2, &format!($($t)*)) } }
#[macro_export]
macro_rules! warn { ($($t:tt)*) => { $crate::util::log_line(3, &format!($($t)*)) } }
#[macro_export]
macro_rules! error { ($($t:tt)*) => { $crate::util::log_line(4, &format!($($t)*)) } }

// --- errors -------------------------------------------------------------------------------------------------------------
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// A request that failed (the HTTP status in its text: "HTTP 429: …").
    Remote,
    /// Predict refused the order outright: nothing was placed.
    Rejected,
    /// A local gate refused the request before its bytes were dispatched: nothing went out.
    NotSent,
}

/// A request that failed: the HTTP status in its text, the server's own retry-after when it gave one.
#[derive(Clone, Debug)]
pub struct RemoteError {
    pub msg: String,
    pub retry_after: u64,
    pub kind: Kind,
}

impl RemoteError {
    pub fn new(msg: impl Into<String>) -> RemoteError {
        RemoteError { msg: msg.into(), retry_after: 0, kind: Kind::Remote }
    }
    pub fn retry(msg: impl Into<String>, retry_after: u64) -> RemoteError {
        RemoteError { msg: msg.into(), retry_after, kind: Kind::Remote }
    }
    pub fn not_sent(msg: impl Into<String>) -> RemoteError {
        RemoteError { msg: msg.into(), retry_after: 0, kind: Kind::NotSent }
    }
    pub fn rejected(msg: impl Into<String>) -> RemoteError {
        RemoteError { msg: msg.into(), retry_after: 0, kind: Kind::Rejected }
    }
    /// The HTTP status a "HTTP 4xx: …" error names, 0 for none.
    pub fn status(&self) -> u16 {
        http_status(&self.msg)
    }
}

impl fmt::Display for RemoteError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.msg)
    }
}

impl std::error::Error for RemoteError {}

pub type Res<T> = Result<T, RemoteError>;

pub fn http_status(text: &str) -> u16 {
    let b = text.as_bytes();
    if b.len() >= 8 && text.starts_with("HTTP ") && b[5..8].iter().all(u8::is_ascii_digit) {
        return text[5..8].parse().unwrap_or(0);
    }
    0
}

fn is_word(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// Telegram tokens (digits:secret) and URLs taken out of a text.
fn redact(text: &str) -> String {
    redact_with(text, true)
}

/// ``urls``: every URL taken out (an error's text: a URL may carry a key); else only the tokens and a Telegram URL (a log line:
/// the console's own address, an API's host stay readable).
fn redact_with(text: &str, urls: bool) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < chars.len() {
        // https?://\S+
        let rest: String = chars[i..chars.len().min(i + 8)].iter().collect();
        let scheme = if rest.starts_with("https://") {
            8
        } else if rest.starts_with("http://") {
            7
        } else {
            0
        };
        if scheme > 0 && i + scheme < chars.len() && !chars[i + scheme].is_whitespace() {
            let mut j = i + scheme;
            while j < chars.len() && !chars[j].is_whitespace() {
                j += 1;
            }
            let url: String = chars[i..j].iter().collect();
            let (head, rest) = url[scheme..].split_once(['/', '?', '#']).map_or((url.as_str(), ""), |(host, _)| {
                let cut = scheme + host.len();
                (&url[..cut], &url[cut..])
            });
            let host = &head[scheme..];
            if urls || host.contains('@') || url.contains("telegram.org") || url.contains("key=") || url.contains("token=") {
                out.push_str("[URL]");
                i = j;
                continue;
            }
            // (a log line's URL kept to its host: its path and query dropped — a node's key may sit in its path, /v2/<key> —
            // unless it is the machine's own, the console's link)
            let name = host.to_ascii_lowercase();
            let name = if name.starts_with("[::1]") { "[::1]" } else { name.rsplit_once(':').map_or(name.as_str(), |(n, _)| n) };
            if rest.is_empty() || rest == "/" || ["127.0.0.1", "localhost", "[::1]", "0.0.0.0"].contains(&name) {
                out.push_str(&url);
            } else {
                out.push_str(head);
                out.push_str("/[…]");
            }
            i = j;
            continue;
        }
        // \b\d{5,}:[A-Za-z0-9_-]{15,}\b
        if chars[i].is_ascii_digit() && (i == 0 || !is_word(chars[i - 1])) {
            let mut j = i;
            while j < chars.len() && chars[j].is_ascii_digit() {
                j += 1;
            }
            if j - i >= 5 && j < chars.len() && chars[j] == ':' {
                let mut k = j + 1;
                while k < chars.len() && (chars[k].is_ascii_alphanumeric() || chars[k] == '_' || chars[k] == '-') {
                    k += 1;
                }
                // the greedy run backed off to the longest that ends on a word boundary (as the regex does)
                let boundary = |end: usize| {
                    if end >= chars.len() { is_word(chars[end - 1]) } else { is_word(chars[end - 1]) != is_word(chars[end]) }
                };
                if let Some(end) = (j + 16..=k).rev().find(|&end| boundary(end)) {
                    out.push_str("[TOKEN REDACTED]");
                    i = end;
                    continue;
                }
            }
            out.extend(&chars[i..j]);
            i = j;
            continue;
        }
        out.push(chars[i]);
        i += 1;
    }
    out
}

/// An error's text for logs and messages: Telegram tokens and URLs taken out (a URL may carry a key), cut to 280 characters.
pub fn clean_error(text: impl fmt::Display) -> String {
    let text = text.to_string();
    let text = if may_hold_secret(&text) { redact(&text) } else { text };
    if text.len() <= 280 { text } else { text.chars().take(280).collect() }
}

/// Whether a text may hold what redact() takes out: a URL, or five digits and a colon (a bot token's head).
fn may_hold_secret(text: &str) -> bool {
    if text.contains("http://") || text.contains("https://") {
        return true;
    }
    let b = text.as_bytes();
    let mut run = 0;
    for &c in b {
        if c.is_ascii_digit() {
            run += 1;
        } else {
            if c == b':' && run >= 5 {
                return true;
            }
            run = 0;
        }
    }
    false
}

/// A log line: the tokens and URLs taken out, not cut.
fn clean_error_keep(text: &str) -> String {
    if may_hold_secret(text) { redact_with(text, false) } else { text.to_string() }
}

pub fn brief_error(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        text.to_string()
    } else {
        let mut out: String = text.chars().take(limit.saturating_sub(1)).collect();
        out.push('…');
        out
    }
}

/// The reason an API puts in an error body (description / msg / message / detail / error); "" when none is readable.
pub fn http_error_text(body: &Value) -> String {
    let Some(obj) = body.as_object() else { return String::new() };
    for key in ["description", "msg", "message", "detail", "error"] {
        let mut value = obj.get(key).cloned().unwrap_or(Value::Null);
        if let Value::Object(inner) = &value {
            let found = [inner.get("message"), inner.get("msg"), inner.get("detail")]
                .into_iter()
                .flatten()
                .find(|v| truthy(v))
                .cloned()
                .unwrap_or(Value::String(String::new()));
            value = found;
        }
        if let Value::Array(items) = &value {
            value = Value::String(items.iter().filter(|v| truthy(v)).map(py_str).collect::<Vec<_>>().join("；"));
        }
        if let Value::String(s) = &value {
            let s = s.trim();
            if !s.is_empty() {
                return s.chars().take(200).collect();
            }
        }
    }
    String::new()
}

// --- text forms ---------------------------------------------------------------------------------------------------------
/// Python's ``format(x, "g")``: six significant digits, the zeros after the point dropped, exponent form outside 1e-4 … 1e6.
pub fn g(x: f64) -> String {
    gp(x, 6)
}

/// ``format(x, ".{p}g")``.
pub fn gp(x: f64, p: usize) -> String {
    if x.is_nan() {
        return "nan".into();
    }
    if x.is_infinite() {
        return if x > 0.0 { "inf".into() } else { "-inf".into() };
    }
    if x == 0.0 {
        return if x.is_sign_negative() { "-0".into() } else { "0".into() };
    }
    let p = p.max(1);
    let sci = format!("{:.*e}", p - 1, x);
    let (mant, exp) = sci.split_once('e').unwrap_or((&sci, "0"));
    let exp: i32 = exp.parse().unwrap_or(0);
    if exp < -4 || exp >= p as i32 {
        let mant = strip_zeros(mant);
        format!("{mant}e{}{:02}", if exp < 0 { '-' } else { '+' }, exp.abs())
    } else {
        let decimals = (p as i32 - 1 - exp).max(0) as usize;
        strip_zeros(&format!("{:.*}", decimals, x))
    }
}

fn strip_zeros(text: &str) -> String {
    if text.contains('.') { text.trim_end_matches('0').trim_end_matches('.').to_string() } else { text.to_string() }
}

/// Python's ``str(x)`` / ``repr(x)`` of a float: the shortest digits, "5.0" for a whole number, the exponent form outside
/// 1e-4 … 1e16.
pub fn py_float(x: f64) -> String {
    if x.is_nan() {
        return "nan".into();
    }
    if x.is_infinite() {
        return if x > 0.0 { "inf".into() } else { "-inf".into() };
    }
    if x == 0.0 {
        return if x.is_sign_negative() { "-0.0".into() } else { "0.0".into() };
    }
    let exp = x.abs().log10().floor() as i32;
    let exp = {
        // log10 can be off by one at the boundaries: read it back from the shortest exponent form
        let sci = format!("{:e}", x);
        sci.split_once('e').and_then(|(_, e)| e.parse::<i32>().ok()).unwrap_or(exp)
    };
    if exp < -4 || exp >= 16 {
        let sci = format!("{:e}", x);
        let (mant, e) = sci.split_once('e').unwrap_or((&sci, "0"));
        let e: i32 = e.parse().unwrap_or(0);
        format!("{mant}e{}{:02}", if e < 0 { '-' } else { '+' }, e.abs())
    } else {
        let text = format!("{}", x);
        if text.contains('.') { text } else { format!("{text}.0") }
    }
}

/// Python's ``str()`` of a JSON value as the records hold it.
pub fn py_str(v: &Value) -> String {
    match v {
        Value::Null => "None".into(),
        Value::Bool(b) => {
            if *b {
                "True".into()
            } else {
                "False".into()
            }
        }
        Value::Number(n) => {
            if n.is_f64() {
                py_float(n.as_f64().unwrap_or(0.0))
            } else {
                n.to_string()
            }
        }
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

fn group_thousands(int_part: &str) -> String {
    let (sign, digits) = if let Some(rest) = int_part.strip_prefix('-') { ("-", rest) } else { ("", int_part) };
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    format!("{sign}{out}")
}

/// ``f"{x:,.{decimals}f}"``.
pub fn comma(x: f64, decimals: usize) -> String {
    let text = format!("{:.*}", decimals, x);
    match text.split_once('.') {
        Some((i, f)) => format!("{}.{f}", group_thousands(i)),
        None => group_thousands(&text),
    }
}

/// An amount: thousands grouped, up to eight decimals, the trailing zeros dropped (``fmt``).
pub fn fmt_amount(x: f64) -> String {
    let text = comma(x, 8);
    strip_zeros(&text)
}

/// +$1,234.56 / −$0.50
pub fn money(x: f64) -> String {
    format!("{}{}", if x >= 0.0 { "+$" } else { "−$" }, comma(x.abs(), 2))
}

/// Python's ``round(x, n)`` (correctly rounded, half to even).
pub fn round_to(x: f64, n: usize) -> f64 {
    format!("{:.*}", n, x).parse().unwrap_or(x)
}

pub fn env_bool(value: Option<&str>, default: bool) -> bool {
    let text = value.unwrap_or("").trim().to_lowercase();
    if text.is_empty() {
        return default;
    }
    matches!(text.as_str(), "1" | "on" | "true" | "yes" | "y" | "开")
}

pub fn median(xs: &mut [f64]) -> Option<f64> {
    if xs.is_empty() {
        return None;
    }
    xs.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let n = xs.len();
    Some(if n % 2 == 1 { xs[n / 2] } else { (xs[n / 2 - 1] + xs[n / 2]) / 2.0 })
}

/// statistics.median of ints: an int for an odd count or an even sum, else the half (as JSON keeps it).
pub fn median_json(xs: &mut [i64]) -> Value {
    if xs.is_empty() {
        return Value::Null;
    }
    xs.sort();
    let n = xs.len();
    if n % 2 == 1 { Value::from(xs[n / 2]) } else { Value::from((xs[n / 2 - 1] + xs[n / 2]) as f64 / 2.0) }
}

// --- the records' JSON, read as the Python dicts are -------------------------------------------------------------------
pub fn truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().map(|x| x != 0.0).unwrap_or(true),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

/// ``float(v)`` of a truthy value (0 for what does not read as a number).
pub fn as_f64(v: &Value) -> f64 {
    match v {
        Value::Number(n) => n.as_f64().unwrap_or(0.0),
        Value::Bool(b) => *b as i64 as f64,
        Value::String(s) => s.trim().parse().unwrap_or(0.0),
        _ => 0.0,
    }
}

/// ``int(v)`` of a truthy value (a float truncated).
pub fn as_i64(v: &Value) -> i64 {
    match v {
        Value::Number(n) => {
            n.as_i64().or_else(|| n.as_u64().map(|u| u as i64)).unwrap_or_else(|| n.as_f64().unwrap_or(0.0) as i64)
        }
        Value::Bool(b) => *b as i64,
        Value::String(s) => s.trim().parse::<i64>().unwrap_or_else(|_| s.trim().parse::<f64>().map(|x| x as i64).unwrap_or(0)),
        _ => 0,
    }
}

/// Python-style reads of a record (a JSON object).
pub trait Py {
    fn val(&self, k: &str) -> Option<&Value>;
    /// ``float(o.get(k) or 0)``
    fn f(&self, k: &str) -> f64 {
        self.val(k).filter(|v| truthy(v)).map(as_f64).unwrap_or(0.0)
    }
    /// ``int(o.get(k) or 0)``
    fn i(&self, k: &str) -> i64 {
        self.val(k).filter(|v| truthy(v)).map(as_i64).unwrap_or(0)
    }
    /// ``str(o.get(k) or "")``
    fn s(&self, k: &str) -> String {
        self.val(k).filter(|v| truthy(v)).map(py_str).unwrap_or_default()
    }
    /// ``bool(o.get(k))``
    fn t(&self, k: &str) -> bool {
        self.val(k).map(truthy).unwrap_or(false)
    }
    /// ``k in o``
    fn has(&self, k: &str) -> bool {
        self.val(k).is_some()
    }
    /// ``o.get(k) is None``
    fn none(&self, k: &str) -> bool {
        matches!(self.val(k), None | Some(Value::Null))
    }
    /// ``o.get(k) is False``
    fn is_false(&self, k: &str) -> bool {
        matches!(self.val(k), Some(Value::Bool(false)))
    }
    /// ``o.get(k) is True``
    fn is_true(&self, k: &str) -> bool {
        matches!(self.val(k), Some(Value::Bool(true)))
    }
    /// ``o.get(k)`` as a float when it is a number (None, null or not a number: None).
    fn of(&self, k: &str) -> Option<f64> {
        match self.val(k) {
            Some(Value::Number(n)) => n.as_f64(),
            _ => None,
        }
    }
    fn obj(&self, k: &str) -> Option<&Map<String, Value>> {
        self.val(k).and_then(Value::as_object)
    }
    fn arr(&self, k: &str) -> &[Value] {
        self.val(k).and_then(Value::as_array).map(Vec::as_slice).unwrap_or(&[])
    }
    fn state(&self) -> &str {
        self.val("state").and_then(Value::as_str).unwrap_or("")
    }
    /// A string field as it stands ("" for anything else) — no copy.
    fn sv(&self, k: &str) -> &str {
        self.val(k).and_then(Value::as_str).unwrap_or("")
    }
}

impl Py for Map<String, Value> {
    fn val(&self, k: &str) -> Option<&Value> {
        self.get(k)
    }
}

impl Py for Value {
    fn val(&self, k: &str) -> Option<&Value> {
        self.as_object().and_then(|o| o.get(k))
    }
}

pub type Obj = Map<String, Value>;

/// A JSON object from pairs.
#[macro_export]
macro_rules! obj {
    ($($k:expr => $v:expr),* $(,)?) => {{
        #[allow(unused_mut)]
        let mut m = serde_json::Map::new();
        $( m.insert(String::from($k), serde_json::Value::from($v)); )*
        m
    }};
}

/// A float as JSON (non-finite -> null).
pub fn jf(x: f64) -> Value {
    serde_json::Number::from_f64(x).map(Value::Number).unwrap_or(Value::Null)
}

pub fn jopt_f(x: Option<f64>) -> Value {
    x.map(jf).unwrap_or(Value::Null)
}

pub fn jopt_i(x: Option<i64>) -> Value {
    x.map(Value::from).unwrap_or(Value::Null)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dates() {
        assert_eq!(Date::parse("2026-12-25").unwrap().to_string(), "2026-12-25");
        assert!(Date::parse("2026-02-30").is_none());
        assert_eq!(Date::ymd(1970, 1, 1).0, 0);
        assert_eq!(Date::ymd(2026, 11, 19).weekday(), 3); // a Thursday
        assert_eq!(bj(1_763_500_000_000, true), "11-19 05:06");
        assert_eq!(bj_clock(1_763_500_000_359), "05:06:40.359");
        assert_eq!(day_of(1_763_500_000_000), "2025-11-19");
    }

    #[test]
    fn text_forms() {
        assert_eq!(g(900.0), "900");
        assert_eq!(g(0.5), "0.5");
        assert_eq!(g(1234567.0), "1.23457e+06");
        assert_eq!(g(0.0001), "0.0001");
        assert_eq!(g(0.00001), "1e-05");
        assert_eq!(g(12.3456789), "12.3457");
        assert_eq!(g(-3.0), "-3");
        assert_eq!(py_float(5.0), "5.0");
        assert_eq!(py_float(1e-5), "1e-05");
        assert_eq!(py_float(0.58), "0.58");
        assert_eq!(money(1234.5), "+$1,234.50");
        assert_eq!(money(-0.5), "−$0.50");
        assert_eq!(fmt_amount(1234.5), "1,234.5");
        assert_eq!(fmt_amount(3.0), "3");
        assert_eq!(round_to(0.125, 2), 0.12);
        assert_eq!(comma(1234567.891, 2), "1,234,567.89");
    }

    #[test]
    fn redaction() {
        assert_eq!(clean_error("bad 1234567:ABCDEFGHIJKLMNOPQRSTUV here"), "bad [TOKEN REDACTED] here");
        assert_eq!(clean_error("at https://api.telegram.org/bot1/x?y=1 failed"), "at [URL] failed");
        assert_eq!(clean_error("HTTP 429: too many"), "HTTP 429: too many");
        assert_eq!(http_status("HTTP 429: x"), 429);
        assert_eq!(http_status("网络错误"), 0);
        assert_eq!(clean_error_keep("控制台：http://127.0.0.1:8080/"), "控制台：http://127.0.0.1:8080/");
        assert_eq!(clean_error_keep("POST https://api.telegram.org/bot1234567:ABCDEFGHIJKLMNOPQRS/sendMessage"), "POST [URL]");
        // a log line's URL kept to its host: a node's key in its path never written (src: python/ruletaker/util.py redact_log)
        assert_eq!(clean_error_keep("节点 https://bsc.example:8545/v2/SUPER_SECRET_KEY 慢"), "节点 https://bsc.example:8545/[…] 慢");
        assert_eq!(clean_error_keep("https://user:pw@rpc.example/"), "[URL]");
        assert_eq!(clean_error_keep("https://rpc.example/?apikey=abc"), "[URL]");
        assert_eq!(clean_error_keep("状态：http://0.0.0.0:8080/status"), "状态：http://0.0.0.0:8080/status");
        assert_eq!(clean_error_keep("http://localhost:8080/status 和 https://api.predict.fun/"), "http://localhost:8080/status 和 https://api.predict.fun/");
    }

    #[test]
    fn py_reads() {
        let o: Value = serde_json::json!({"a": 1.5, "b": "2", "c": null, "d": false, "e": "", "f": 3});
        assert_eq!(o.f("a"), 1.5);
        assert_eq!(o.f("b"), 2.0);
        assert_eq!(o.f("c"), 0.0);
        assert_eq!(o.i("a"), 1);
        assert_eq!(o.s("f"), "3");
        assert_eq!(o.s("e"), "");
        assert!(o.is_false("d"));
        assert!(o.none("c") && o.none("zz"));
        assert!(!o.has("zz") && o.has("c"));
    }
}
