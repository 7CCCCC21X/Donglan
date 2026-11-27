//! The operator's HTTP server (STATUS_HOST:STATUS_PORT, 127.0.0.1:8080 by default — reached through an SSH tunnel: rt.sh console):
//!
//!   GET  /                   the control page and the review page (控制台 · 复盘), one page
//!   GET  /healthz            "ok" while the taker's loop runs (no key)
//!   GET  /status             the status in text, what /rules said in huice
//!   GET  /status.json        a summary in numbers
//!   GET  /api/state          the control page's data
//!   GET  /api/review?days=30 the review page's data (7 / 30 / 90 / all)
//!   GET  /api/records.csv    every order of every window
//!   POST /api/control        {"action": "pause" | "resume" | "mode" | "withdraw" | "check" | "test", "arg": ...}
//!   POST /api/settings       {"values": {"RULE_GOLD_SHUT_CENTS": "60", ...}} or {"reset": ["RULE_…"] (empty: all)}
//!   POST /pause  /resume  /mode?to=off|paper|on   (rt.sh)
//!
//! With STATUS_KEY set every path but / and /healthz needs it (?key=… or the X-Status-Key header). Every POST also needs the
//! header X-RT: 1 — a page of another site can send neither, so nobody can pause or change the settings through a logged-in
//! browser. A server on another address than the machine itself is refused at the start without a key (Config).
use crate::taker::{Me, control};
use serde_json::{Value, json};
use std::cell::Cell;
use std::collections::HashMap;
use std::rc::Rc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

pub const PAGE: &str = include_str!("../web/console.html");
pub const MAX_BODY: usize = 65_536;
/// Requests served at once (more are closed at once), and the pause after a failed accept.
pub const MAX_CONNECTIONS: usize = 32;
/// An answer's write given this long (s): a reader that stops reading lets go of its place then.
pub const WRITE_TIMEOUT_S: f64 = 10.0;
pub const ACCEPT_PAUSE_MS: u64 = 100;

pub struct Reply {
    pub status: u16,
    pub kind: &'static str,
    pub body: String,
}

const TEXT: &str = "text/plain; charset=utf-8";
const JS: &str = "application/json; charset=utf-8";

fn reply(status: u16, kind: &'static str, body: impl Into<String>) -> Reply {
    Reply { status, kind, body: body.into() }
}

fn json_reply(status: u16, value: Value) -> Reply {
    reply(status, JS, value.to_string())
}

/// Percent-decoding of a query part ('+' is a space).
pub fn unquote(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if b == b'+' {
            out.push(b' ');
            i += 1;
            continue;
        }
        if b == b'%' && i + 3 <= bytes.len() {
            if let Ok(v) = u8::from_str_radix(std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or("zz"), 16) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(b);
        i += 1;
    }
    String::from_utf8_lossy(&out).to_string()
}

pub fn parse_query(query: &str) -> HashMap<String, String> {
    let mut out = HashMap::new();
    for pair in query.split('&') {
        if pair.is_empty() {
            continue;
        }
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        if v.is_empty() {
            continue; // (parse_qs drops a key with no value)
        }
        out.entry(unquote(k)).or_insert_with(|| unquote(v));
    }
    out
}

fn same_key(given: &str, key: &str) -> bool {
    let (a, b) = (given.as_bytes(), key.as_bytes());
    let mut diff = (a.len() ^ b.len()) as u8;
    for i in 0..a.len().max(b.len()) {
        diff |= a.get(i).copied().unwrap_or(0) ^ b.get(i).copied().unwrap_or(0);
    }
    diff == 0 && a.len() == b.len()
}

/// Whether a request is refused on its head alone (every path but /healthz and the page): the key, the method, a POST's
/// X-RT header — judged before its body is read, so nobody without the key gets the server to take one in.
fn turned_away(key: &str, method: &str, query: &HashMap<String, String>, headers: &HashMap<String, String>) -> Option<Reply> {
    if !key.is_empty() {
        let given = query
            .get("key")
            .cloned()
            .filter(|k| !k.is_empty())
            .or_else(|| headers.get("x-status-key").cloned())
            .unwrap_or_default();
        if !same_key(&given, key) {
            return Some(reply(403, TEXT, "need ?key=STATUS_KEY"));
        }
    }
    if method != "GET" && method != "POST" {
        return Some(reply(405, TEXT, "method not allowed"));
    }
    if method == "POST" && headers.get("x-rt").map(String::as_str) != Some("1") {
        return Some(reply(403, TEXT, "POST needs the header X-RT: 1"));
    }
    None
}

fn open_path(method: &str, path: &str) -> bool {
    path == "/healthz" || (matches!(path, "/" | "/console" | "/index.html") && method == "GET")
}

pub async fn route(
    me: &Me,
    key: &str,
    method: &str,
    path: &str,
    query: &HashMap<String, String>,
    headers: &HashMap<String, String>,
    body: &[u8],
) -> Reply {
    if path == "/healthz" {
        let ok = me.borrow().healthy();
        return reply(if ok { 200 } else { 503 }, TEXT, if ok { "ok" } else { "not running" });
    }
    if open_path(method, path) {
        return reply(200, "text/html; charset=utf-8", PAGE);
    }
    if let Some(no) = turned_away(key, method, query, headers) {
        return no;
    }
    if method == "GET" {
        return match path {
            "/status" => {
                control::after_the_race(me).await; // (milliseconds of the one thread the orders use: never in the race)
                reply(200, TEXT, me.borrow().status())
            }
            "/status.json" => json_reply(200, me.borrow().summary()),
            "/api/state" => json_reply(200, me.borrow().state()),
            "/api/review" => {
                let raw = query.get("days").cloned().unwrap_or_else(|| "30".into());
                let days = if raw == "all" {
                    0
                } else if !raw.is_empty() && raw.bytes().all(|b| b.is_ascii_digit()) {
                    raw.parse::<i64>().unwrap_or(30).clamp(1, 3650)
                } else {
                    30
                };
                json_reply(200, me.borrow_mut().review_data(days))
            }
            "/api/records.csv" => {
                control::after_the_race(me).await;
                reply(200, "text/csv; charset=utf-8", format!("\u{feff}{}", crate::review::records_csv(&me.borrow().recs)))
            }
            _ => {
                let post_only = ["/pause", "/resume", "/mode", "/api/control", "/api/settings"].contains(&path);
                let api_like = ["/pause", "/resume", "/mode", "/api/"].iter().any(|p| path.starts_with(p));
                reply(if post_only { 405 } else { 404 }, TEXT, if api_like { "use POST" } else { "not found" })
            }
        };
    }
    match path {
        "/pause" => {
            let why = query.get("why").cloned().unwrap_or_else(|| "状态页".into());
            return reply(200, TEXT, control::control(me, "pause", &why).await);
        }
        "/resume" => return reply(200, TEXT, control::control(me, "resume", "").await),
        "/mode" => {
            let to = query.get("to").cloned().unwrap_or_default();
            return reply(200, TEXT, control::control(me, "mode", &to).await);
        }
        _ => {}
    }
    let data: Value = if body.is_empty() {
        json!({})
    } else {
        match serde_json::from_slice(body) {
            Ok(v) => v,
            Err(_) => return json_reply(400, json!({"ok": false, "message": "请求不是 JSON"})),
        }
    };
    let Some(data) = data.as_object() else {
        return json_reply(400, json!({"ok": false, "message": "请求不是 JSON 对象"}));
    };
    let result: control::SaveResult = match path {
        "/api/control" => {
            let action = data.get("action").map(crate::util::py_str).filter(|s| s != "None").unwrap_or_default();
            let arg = data.get("arg").filter(|v| crate::util::truthy(v)).map(crate::util::py_str).unwrap_or_default();
            match action.as_str() {
                "withdraw" => {
                    let line = crate::taker::orders::withdraw_all(me, "控制台撤单").await;
                    control::SaveResult::Done(if line.is_empty() { "没有挂着的规则吃单订单".into() } else { line })
                }
                "pause" | "resume" | "mode" | "check" | "test" => {
                    let arg = if arg.is_empty() && action == "pause" { "控制台".to_string() } else { arg };
                    control::SaveResult::Done(control::control(me, &action, &arg).await)
                }
                _ => return json_reply(400, json!({"ok": false, "message": format!("不认识的操作：{action}")})),
            }
        }
        "/api/settings" => {
            if let Some(reset) = data.get("reset") {
                match reset_keys(reset) {
                    Ok(None) => control::reset_settings(me, None).await,
                    Ok(Some(keys)) => control::reset_settings(me, Some(&keys)).await,
                    Err(why) => return json_reply(400, json!({"ok": false, "message": why})),
                }
            } else {
                match data.get("values").and_then(Value::as_object) {
                    Some(values) if !values.is_empty() => control::save_settings(me, values).await,
                    _ => return json_reply(400, json!({"ok": false, "message": "没有要保存的值"})),
                }
            }
        }
        _ => return json_reply(404, json!({"ok": false, "message": "not found"})),
    };
    match result {
        control::SaveResult::Done(message) => json_reply(200, json!({"ok": true, "message": message})),
        // (in force, not on disk: never told as saved — "applied" lets the page show the values in force)
        control::SaveResult::NotOnDisk(message) => json_reply(200, json!({"ok": false, "applied": true, "message": message})),
        control::SaveResult::Refused(message) => json_reply(200, json!({"ok": false, "message": message})),
    }
}

/// The fields a reset names: "all" (every one the page saved), or a list of the page's field names — anything else (a
/// bare name, an empty list, a name the page has not) is refused, never read as "all".
fn reset_keys(reset: &Value) -> Result<Option<Vec<String>>, String> {
    const HOW: &str = "reset 要写成字段名的数组（例如 [\"RULE_GOLD_SHUT_CENTS\"]），全部恢复写 \"all\"";
    match reset {
        Value::String(s) if s == "all" => Ok(None),
        Value::Array(items) if !items.is_empty() => {
            let mut keys = vec![];
            for item in items {
                let Some(key) = item.as_str() else { return Err(HOW.into()) };
                if crate::console::field(key).is_none() {
                    return Err(format!("{key} 不是控制台的设置项"));
                }
                keys.push(key.to_string());
            }
            Ok(Some(keys))
        }
        _ => Err(HOW.into()),
    }
}

struct Request {
    method: String,
    path: String,
    query: HashMap<String, String>,
    headers: HashMap<String, String>,
    body: Vec<u8>,
}

/// A request's line and headers (its body not read yet: ``body`` holds the bytes that came with the head).
async fn read_head(sock: &mut tokio::net::TcpStream) -> Result<Request, String> {
    let mut buf = Vec::new();
    let end = loop {
        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break i;
        }
        if buf.len() > 32_768 {
            return Err("HeaderTooLong".into());
        }
        let mut chunk = [0u8; 4096];
        let n = sock.read(&mut chunk).await.map_err(|_| "ConnectionError".to_string())?;
        if n == 0 {
            return Err("IncompleteReadError".into());
        }
        buf.extend_from_slice(&chunk[..n]);
    };
    let head = String::from_utf8_lossy(&buf[..end]).to_string();
    let mut lines = head.split("\r\n");
    let first: Vec<&str> = lines.next().unwrap_or("").split(' ').collect();
    let (method, target) = (first.first().copied().unwrap_or("").to_uppercase(), first.get(1).copied().unwrap_or("").to_string());
    let mut headers = HashMap::new();
    for line in lines {
        if let Some((k, v)) = line.split_once(':') {
            headers.insert(k.trim().to_lowercase(), v.trim().to_string());
        }
    }
    let body = buf[end + 4..].to_vec();
    let (path, query) = target.split_once('?').unwrap_or((&target, ""));
    let path = path.split('#').next().unwrap_or("").to_string();
    Ok(Request { method, path: if path.is_empty() { "/".into() } else { path }, query: parse_query(query), headers, body })
}

/// The rest of a request's body (Content-Length, at most MAX_BODY).
async fn read_body(sock: &mut tokio::net::TcpStream, req: &mut Request) -> Result<(), String> {
    let length: usize = req
        .headers
        .get("content-length")
        .map(|v| v.trim().parse().map_err(|_| "ValueError".to_string()))
        .transpose()?
        .unwrap_or(0);
    if length > MAX_BODY {
        return Err("ValueError".into());
    }
    while req.body.len() < length {
        let mut chunk = [0u8; 4096];
        let n = sock.read(&mut chunk).await.map_err(|_| "ConnectionError".to_string())?;
        if n == 0 {
            return Err("IncompleteReadError".into());
        }
        req.body.extend_from_slice(&chunk[..n]);
    }
    req.body.truncate(length);
    Ok(())
}

/// The request read (its head, then — the key and the header judged first — its body; five seconds for both) and routed.
async fn answer(me: &Me, key: &str, sock: &mut tokio::net::TcpStream) -> Reply {
    let began = std::time::Instant::now();
    let limit = Duration::from_secs(5);
    let mut req = match tokio::time::timeout(limit, read_head(sock)).await {
        Ok(Ok(req)) => req,
        Ok(Err(kind)) => return reply(400, TEXT, format!("bad request: {kind}")),
        Err(_) => return reply(400, TEXT, "bad request: TimeoutError"),
    };
    if !open_path(&req.method, &req.path) {
        if let Some(no) = turned_away(key, &req.method, &req.query, &req.headers) {
            return no;
        }
    }
    match tokio::time::timeout(limit.saturating_sub(began.elapsed()), read_body(sock, &mut req)).await {
        Ok(Ok(())) => route(me, key, &req.method, &req.path, &req.query, &req.headers, &req.body).await,
        Ok(Err(kind)) => reply(400, TEXT, format!("bad request: {kind}")),
        Err(_) => reply(400, TEXT, "bad request: TimeoutError"),
    }
}

async fn handle(me: Me, key: String, mut sock: tokio::net::TcpStream) {
    let r = answer(&me, &key, &mut sock).await;
    let csv = r.kind.starts_with("text/csv");
    let data = r.body.into_bytes();
    let extra = if csv { "Content-Disposition: attachment; filename=\"ruletaker-records.csv\"\r\n" } else { "" };
    let head = format!(
        "HTTP/1.1 {} {}\r\nContent-Type: {}\r\n{extra}Content-Length: {}\r\nCache-Control: no-store\r\nX-Content-Type-Options: nosniff\r\nReferrer-Policy: no-referrer\r\nConnection: close\r\n\r\n",
        r.status,
        if r.status == 200 { "OK" } else { "Error" },
        r.kind,
        data.len()
    );
    let mut out = head.into_bytes();
    out.extend(data);
    // (a reader that stops taking the answer holds its place WRITE_TIMEOUT_S at the most: never one of the MAX_CONNECTIONS for
    // good)
    let limit = std::time::Duration::from_secs_f64(WRITE_TIMEOUT_S);
    let _ = tokio::time::timeout(limit, async {
        let _ = sock.write_all(&out).await;
        let _ = sock.shutdown().await;
    })
    .await;
}

/// A request's place among the MAX_CONNECTIONS, given back however its task ends.
struct Slot(Rc<Cell<usize>>);

impl Drop for Slot {
    fn drop(&mut self) {
        self.0.set(self.0.get().saturating_sub(1));
    }
}

/// The server, serving until the process ends (a port of 0: none).
pub async fn serve(me: Me, host: String, port: u16, key: String) -> Result<(), String> {
    if port == 0 {
        return Ok(());
    }
    let listener = TcpListener::bind((host.as_str(), port)).await.map_err(|e| format!("控制台端口 {host}:{port} 开不了：{e}"))?;
    if ["127.0.0.1", "localhost", "::1"].contains(&host.as_str()) {
        // (the machine's own address: a browser elsewhere reaches it through the SSH tunnel alone)
        crate::info!(
            "控制台和状态页在这台机器本机 {host}:{port}（别的电脑直接打不开；从浏览器看：Cloud Shell 里运行 bash deploy/gcp/rt.sh console）"
        );
    } else {
        crate::info!("控制台：http://{host}:{port}/  状态：http://{host}:{port}/status");
    }
    tokio::task::spawn_local(async move {
        let open = Rc::new(Cell::new(0usize));
        loop {
            let sock = match listener.accept().await {
                Ok((sock, _)) => sock,
                Err(_) => {
                    // (out of file handles, or the like: a breath before the next try — never a spin on the orders' thread)
                    tokio::time::sleep(Duration::from_millis(ACCEPT_PAUSE_MS)).await;
                    continue;
                }
            };
            if open.get() >= MAX_CONNECTIONS {
                continue; // (closed at once: the page asks again in two seconds)
            }
            let _ = sock.set_nodelay(true);
            open.set(open.get() + 1);
            let (me, key, slot) = (me.clone(), key.clone(), Slot(open.clone()));
            tokio::task::spawn_local(async move {
                let _slot = slot;
                handle(me, key, sock).await;
            });
        }
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn queries() {
        let q = parse_query("days=all&key=a%2Bb+c&empty=");
        assert_eq!(q["days"], "all");
        assert_eq!(q["key"], "a+b c");
        assert!(!q.contains_key("empty"));
        assert!(same_key("abc", "abc") && !same_key("abd", "abc") && !same_key("ab", "abc"));
        assert_eq!(unquote("%E4%B8%AD"), "中");
        assert_eq!(unquote("100%"), "100%");
    }
}
