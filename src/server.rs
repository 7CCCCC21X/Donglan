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
use std::collections::HashMap;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

pub const PAGE: &str = include_str!("../web/console.html");
pub const MAX_BODY: usize = 65_536;

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
    if matches!(path, "/" | "/console" | "/index.html") && method == "GET" {
        return reply(200, "text/html; charset=utf-8", PAGE);
    }
    if !key.is_empty() {
        let given = query
            .get("key")
            .cloned()
            .filter(|k| !k.is_empty())
            .or_else(|| headers.get("x-status-key").cloned())
            .unwrap_or_default();
        if !same_key(&given, key) {
            return reply(403, TEXT, "need ?key=STATUS_KEY");
        }
    }
    if method == "GET" {
        return match path {
            "/status" => reply(200, TEXT, me.borrow().status()),
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
                reply(200, "text/csv; charset=utf-8", format!("\u{feff}{}", crate::review::records_csv(&me.borrow().recs)))
            }
            _ => {
                let post_only = ["/pause", "/resume", "/mode", "/api/control", "/api/settings"].contains(&path);
                let api_like = ["/pause", "/resume", "/mode", "/api/"].iter().any(|p| path.starts_with(p));
                reply(if post_only { 405 } else { 404 }, TEXT, if api_like { "use POST" } else { "not found" })
            }
        };
    }
    if method != "POST" {
        return reply(405, TEXT, "method not allowed");
    }
    if headers.get("x-rt").map(String::as_str) != Some("1") {
        return reply(403, TEXT, "POST needs the header X-RT: 1");
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
    let result: Result<String, String> = match path {
        "/api/control" => {
            let action = data.get("action").map(crate::util::py_str).filter(|s| s != "None").unwrap_or_default();
            let arg = data.get("arg").filter(|v| crate::util::truthy(v)).map(crate::util::py_str).unwrap_or_default();
            match action.as_str() {
                "withdraw" => {
                    let line = crate::taker::orders::withdraw_all(me, "控制台撤单").await;
                    Ok(if line.is_empty() { "没有挂着的规则吃单订单".into() } else { line })
                }
                "pause" | "resume" | "mode" | "check" | "test" => {
                    let arg = if arg.is_empty() && action == "pause" { "控制台".to_string() } else { arg };
                    Ok(control::control(me, &action, &arg).await)
                }
                _ => return json_reply(400, json!({"ok": false, "message": format!("不认识的操作：{action}")})),
            }
        }
        "/api/settings" => {
            if data.contains_key("reset") {
                let keys: Vec<String> = data
                    .get("reset")
                    .and_then(Value::as_array)
                    .map(|a| a.iter().map(crate::util::py_str).collect())
                    .unwrap_or_default();
                control::reset_settings(me, &keys).await
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
        Ok(message) => json_reply(200, json!({"ok": true, "message": message})),
        Err(message) => json_reply(200, json!({"ok": false, "message": message})),
    }
}

struct Request {
    method: String,
    path: String,
    query: HashMap<String, String>,
    headers: HashMap<String, String>,
    body: Vec<u8>,
}

async fn read_request(sock: &mut tokio::net::TcpStream) -> Result<Request, String> {
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
    let length: usize =
        headers.get("content-length").map(|v| v.trim().parse().map_err(|_| "ValueError".to_string())).transpose()?.unwrap_or(0);
    if length > MAX_BODY {
        return Err("ValueError".into());
    }
    let mut body = buf[end + 4..].to_vec();
    while body.len() < length {
        let mut chunk = [0u8; 4096];
        let n = sock.read(&mut chunk).await.map_err(|_| "ConnectionError".to_string())?;
        if n == 0 {
            return Err("IncompleteReadError".into());
        }
        body.extend_from_slice(&chunk[..n]);
    }
    body.truncate(length);
    let (path, query) = target.split_once('?').unwrap_or((&target, ""));
    let path = path.split('#').next().unwrap_or("").to_string();
    Ok(Request { method, path: if path.is_empty() { "/".into() } else { path }, query: parse_query(query), headers, body })
}

async fn handle(me: Me, key: String, mut sock: tokio::net::TcpStream) {
    let request = tokio::time::timeout(Duration::from_secs(5), read_request(&mut sock))
        .await
        .unwrap_or_else(|_| Err("TimeoutError".into()));
    let (r, csv) = match request {
        Ok(req) => {
            let r = route(&me, &key, &req.method, &req.path, &req.query, &req.headers, &req.body).await;
            let csv = r.kind.starts_with("text/csv");
            (r, csv)
        }
        Err(kind) => (reply(400, TEXT, format!("bad request: {kind}")), false),
    };
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
    let _ = sock.write_all(&out).await;
    let _ = sock.shutdown().await;
}

/// The server, serving until the process ends (a port of 0: none).
pub async fn serve(me: Me, host: String, port: u16, key: String) -> Result<(), String> {
    if port == 0 {
        return Ok(());
    }
    let listener = TcpListener::bind((host.as_str(), port)).await.map_err(|e| format!("控制台端口 {host}:{port} 开不了：{e}"))?;
    crate::info!("控制台：http://{host}:{port}/  状态：http://{host}:{port}/status");
    tokio::task::spawn_local(async move {
        loop {
            let Ok((sock, _)) = listener.accept().await else { continue };
            let _ = sock.set_nodelay(true);
            tokio::task::spawn_local(handle(me.clone(), key.clone(), sock));
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
