//! HTTP/1.1 with connections kept alive (huice main.py _http_get / _http_keepalive, as the Python version has it).
//!
//! Connections are pooled per host and reused, the most recently used first. One the server closed meanwhile is retried once
//! on a fresh one when nothing of the request was taken (for a GET also when no answer came); a POST — an order — is never sent
//! twice: an answer lost on the way is the caller's "result unknown", found by the order's hash. A POST reuses only a connection
//! used within the last KEEPALIVE_POST_IDLE seconds (a stale one could swallow it). TCP_NODELAY on every connection.
//! Credentials (x-api-key, Authorization) go to the host the request was made to and to no other (a redirect elsewhere drops
//! them; a POST is never redirected).
use crate::util::{RemoteError, Res, clean_error, http_error_text, mono};
use std::cell::{Cell, RefCell};
use std::collections::{HashMap, VecDeque};
use std::io;
use std::pin::Pin;
use std::rc::Rc;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadBuf};
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;
use tokio_rustls::client::TlsStream;

pub const MAX_BODY: usize = 8_000_000;
pub const KEEPALIVE_IDLE: f64 = 25.0;
pub const KEEPALIVE_POST_IDLE: f64 = 3.0;
pub const KEEPALIVE_PER_HOST: usize = 24;
const SECRET_HEADERS: [&str; 4] = ["authorization", "x-api-key", "cookie", "proxy-authorization"];

pub fn user_agent() -> String {
    format!("RuleTaker/{}", env!("CARGO_PKG_VERSION"))
}

fn hint(code: u16) -> Option<&'static str> {
    match code {
        451 => Some("部署所在地或接口访问受限；请核对官方地区规则"),
        403 => Some("访问被拒绝；请核对权限与服务地区"),
        418 => Some("接口暂时封禁；停止高频请求并等待解除"),
        429 => Some("接口限流，等待后重试"),
        _ => None,
    }
}

// --- the connection -------------------------------------------------------------------------------------------------------
pub enum Stream {
    Plain(TcpStream),
    Tls(Box<TlsStream<TcpStream>>),
}

impl AsyncRead for Stream {
    fn poll_read(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Stream::Plain(s) => Pin::new(s).poll_read(cx, buf),
            Stream::Tls(s) => Pin::new(s.as_mut()).poll_read(cx, buf),
        }
    }
}

impl AsyncWrite for Stream {
    fn poll_write(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<io::Result<usize>> {
        match self.get_mut() {
            Stream::Plain(s) => Pin::new(s).poll_write(cx, buf),
            Stream::Tls(s) => Pin::new(s.as_mut()).poll_write(cx, buf),
        }
    }
    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Stream::Plain(s) => Pin::new(s).poll_flush(cx),
            Stream::Tls(s) => Pin::new(s.as_mut()).poll_flush(cx),
        }
    }
    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Stream::Plain(s) => Pin::new(s).poll_shutdown(cx),
            Stream::Tls(s) => Pin::new(s.as_mut()).poll_shutdown(cx),
        }
    }
}

pub struct Conn {
    stream: Stream,
    buf: Vec<u8>,
}

impl Conn {
    /// Whether the server has closed the connection while it sat in the pool: bytes past the last answer already read (by us,
    /// or by rustls with it: a close_notify that came on its heels); else a peek at the socket (no wait) finds its end, or
    /// bytes no request asked for. On TLS such bytes may be the protocol's own — a TLS 1.3 session ticket or key update sent
    /// after the answer — so rustls is let read them, without waiting: nothing for us behind them, the connection is good;
    /// the end of the stream, a close_notify, an alert or data (a 408 before the server hangs up) are a dead one.
    fn closed(&mut self) -> bool {
        use std::os::fd::AsRawFd;
        if !self.buf.is_empty() {
            return true;
        }
        if let Stream::Tls(tls) = &self.stream {
            if !tls.get_ref().1.wants_read() {
                return true; // (a close_notify taken in, or data it holds that nobody asked for)
            }
        }
        let fd = match &self.stream {
            Stream::Plain(s) => s.as_raw_fd(),
            Stream::Tls(s) => s.get_ref().0.as_raw_fd(),
        };
        let mut byte = [0u8; 1];
        // SAFETY: a valid fd of a socket this connection owns; a one-byte buffer of our own.
        let got = unsafe { libc::recv(fd, byte.as_mut_ptr().cast(), 1, libc::MSG_PEEK | libc::MSG_DONTWAIT) };
        if got < 0 {
            let err = io::Error::last_os_error();
            return !matches!(err.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted);
        }
        let Stream::Tls(tls) = &mut self.stream else { return true }; // 0: the end; > 0: bytes nobody asked for
        if got == 0 {
            return true;
        }
        let mut cx = Context::from_waker(std::task::Waker::noop());
        let mut one = [0u8; 1];
        let mut read = ReadBuf::new(&mut one);
        // Pending: TLS records of its own, read, and the socket empty after them
        !matches!(Pin::new(tls.as_mut()).poll_read(&mut cx, &mut read), Poll::Pending)
    }
}

pub fn tls_config() -> Arc<rustls::ClientConfig> {
    let mut roots = rustls::RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut config = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .expect("TLS 1.2 / 1.3")
        .with_root_certificates(roots)
        .with_no_client_auth();
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    Arc::new(config)
}

/// A TCP (+ TLS) connection to host:port, TCP_NODELAY set; and how long the handshakes took (ms).
pub async fn connect(https: bool, host: &str, port: u16, timeout: Duration, tls: &TlsConnector) -> io::Result<(Stream, f64)> {
    let began = mono();
    let tcp = tokio::time::timeout(timeout, TcpStream::connect((host, port)))
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "timed out"))??;
    tcp.set_nodelay(true)?;
    if !https {
        return Ok((Stream::Plain(tcp), (mono() - began) * 1000.0));
    }
    let name = rustls::pki_types::ServerName::try_from(host.to_string())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "invalid host name"))?;
    let tls_stream = tokio::time::timeout(timeout, tls.connect(name, tcp))
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "timed out"))??;
    Ok((Stream::Tls(Box::new(tls_stream)), (mono() - began) * 1000.0))
}

// --- the URL --------------------------------------------------------------------------------------------------------------
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Url {
    pub https: bool,
    pub host: String,
    pub port: u16,
    pub path: String,
}

impl Url {
    pub fn parse(url: &str) -> Res<Url> {
        let bad = || RemoteError::new("网络错误 (ValueError: 地址格式不对)");
        let (https, rest) = if let Some(r) = url.strip_prefix("https://") {
            (true, r)
        } else if let Some(r) = url.strip_prefix("http://") {
            (false, r)
        } else if let Some(r) = url.strip_prefix("wss://") {
            (true, r)
        } else if let Some(r) = url.strip_prefix("ws://") {
            (false, r)
        } else {
            return Err(bad());
        };
        let (authority, path) = match rest.find(['/', '?']) {
            Some(i) => (&rest[..i], &rest[i..]),
            None => (rest, "/"),
        };
        let authority = authority.rsplit('@').next().unwrap_or(authority);
        let (host, port) = match authority.rsplit_once(':') {
            Some((h, p)) if !h.is_empty() && p.bytes().all(|b| b.is_ascii_digit()) && !p.is_empty() => {
                (h, p.parse().map_err(|_| bad())?)
            }
            _ => (authority, if https { 443 } else { 80 }),
        };
        if host.is_empty() {
            return Err(bad());
        }
        let path = if path.starts_with('?') { format!("/{path}") } else { path.to_string() };
        Ok(Url { https, host: host.to_ascii_lowercase(), port, path })
    }

    /// A Location against this URL: absolute, absolute-path, or relative to the current directory.
    pub fn join(&self, location: &str) -> Res<Url> {
        if location.contains("://") {
            return Url::parse(location);
        }
        let mut next = self.clone();
        if let Some(rest) = location.strip_prefix("//") {
            return Url::parse(&format!("{}://{rest}", if self.https { "https" } else { "http" }));
        }
        if location.starts_with('/') {
            next.path = location.to_string();
        } else {
            let base = self.path.split('?').next().unwrap_or("/");
            let dir = &base[..base.rfind('/').map(|i| i + 1).unwrap_or(0)];
            next.path = format!("{}{location}", if dir.is_empty() { "/" } else { dir });
        }
        Ok(next)
    }
}

// --- the pool -------------------------------------------------------------------------------------------------------------
#[derive(Default, Clone, Debug)]
pub struct Stats {
    pub new: u64,
    pub reused: u64,
    pub retried: u64,
    /// pooled connections found closed by the server before a request went out on them
    pub dead: u64,
}

#[derive(Default, Clone, Debug)]
pub struct Net {
    /// the last new connections' TCP + TLS handshakes in ms
    pub connect: VecDeque<f64>,
    /// the last answer's cf-ray (its suffix names the Cloudflare site the request entered at), else its Server header
    pub edge: String,
    /// the last answer's rate-limit headers, as sent ("x-ratelimit-limit: 600; …"), when it had any
    pub limits: String,
}

/// What became of one request on its way (an order's): from the caller's call to the request handed to the network layer (the
/// key's budget, a free slot), its new connection's TCP + TLS handshake (none: a warm one reused), when its bytes were all
/// written (wall ms).
#[derive(Default, Debug)]
pub struct Trace {
    pub entered: Cell<f64>,
    pub queued_ms: Cell<f64>,
    pub connect_ms: Cell<Option<f64>>,
    pub wrote_wall: Cell<i64>,
}

pub struct Http {
    pool: RefCell<HashMap<(String, u16, bool), Vec<(Conn, f64)>>>,
    pub stats: RefCell<Stats>,
    pub net: RefCell<HashMap<String, Net>>,
    tls: TlsConnector,
    agent: String,
}

thread_local! {
    static HTTP: Rc<Http> = Rc::new(Http::new());
}

/// The process's pool (one a thread; the service runs on one).
pub fn http() -> Rc<Http> {
    HTTP.with(Rc::clone)
}

/// How a request that never got a connection is told: nothing of it went out (an order may be sent again).
pub const NOT_CONNECTED: &str = "连接没建立";

fn net_error(error: &io::Error) -> RemoteError {
    let kind = match error.kind() {
        io::ErrorKind::TimedOut => "TimeoutError",
        io::ErrorKind::ConnectionReset => "ConnectionResetError",
        io::ErrorKind::ConnectionRefused => "ConnectionRefusedError",
        io::ErrorKind::ConnectionAborted => "ConnectionAbortedError",
        io::ErrorKind::UnexpectedEof => "RemoteDisconnected",
        io::ErrorKind::BrokenPipe => "BrokenPipeError",
        io::ErrorKind::InvalidData => "BadStatusLine",
        _ => "OSError",
    };
    let detail: String = clean_error(error.to_string()).chars().take(80).collect();
    RemoteError::new(format!("网络错误 ({kind}{})", if detail.is_empty() { String::new() } else { format!(": {detail}") }))
}

pub fn remote_error(host: &str, code: u16, retry_after: &str, raw: &[u8]) -> RemoteError {
    let mut retry: u64 = retry_after.trim().parse().unwrap_or(0);
    let mut description = String::new();
    if let Ok(body) = serde_json::from_slice::<serde_json::Value>(raw) {
        description = http_error_text(&body);
        if let Some(p) = body.get("parameters").and_then(|p| p.get("retry_after")).and_then(|r| r.as_u64()) {
            retry = retry.max(p);
        }
    }
    let text = if host.ends_with("telegram.org") {
        if description.is_empty() { "接口请求失败".to_string() } else { description }
    } else if host.ends_with("predict.fun") {
        let mut t = if !description.is_empty() { description } else { hint(code).unwrap_or("接口请求失败").to_string() };
        if code == 403 {
            t += "（Predict 不向受限地区开放交易：受限地区读取正常、下单 403，把服务换到允许的地区）";
        }
        t
    } else {
        if host.contains("binance") && (code == 418 || code == 429) {
            retry = retry.max(if code == 418 { 120 } else { 30 });
        }
        hint(code).map(str::to_string).unwrap_or(if description.is_empty() {
            "接口请求失败".to_string()
        } else {
            description
        })
    };
    RemoteError::retry(clean_error(format!("HTTP {code}: {text}")), retry)
}

struct Head {
    status: u16,
    headers: Vec<(String, String)>,
    minor: u8,
}

impl Head {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }
    fn will_close(&self) -> bool {
        let conn = self.header("connection").unwrap_or("").to_ascii_lowercase();
        conn.contains("close") || (self.minor == 0 && !conn.contains("keep-alive"))
    }
}

async fn read_more(conn: &mut Conn) -> io::Result<usize> {
    let mut chunk = [0u8; 16384];
    let n = conn.stream.read(&mut chunk).await?;
    conn.buf.extend_from_slice(&chunk[..n]);
    Ok(n)
}

async fn read_head(conn: &mut Conn) -> io::Result<Head> {
    loop {
        if let Some(end) = find(&conn.buf, b"\r\n\r\n") {
            let mut headers = [httparse::EMPTY_HEADER; 96];
            let mut res = httparse::Response::new(&mut headers);
            res.parse(&conn.buf[..end + 4]).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
            let head = Head {
                status: res.code.unwrap_or(0),
                minor: res.version.unwrap_or(1),
                headers: res
                    .headers
                    .iter()
                    .map(|h| (h.name.to_string(), String::from_utf8_lossy(h.value).trim().to_string()))
                    .collect(),
            };
            conn.buf.drain(..end + 4);
            if (100..200).contains(&head.status) && head.status != 101 {
                continue; // 100 Continue and the like: the answer follows
            }
            return Ok(head);
        }
        if conn.buf.len() > 256 * 1024 {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "header too long"));
        }
        if read_more(conn).await? == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                if conn.buf.is_empty() { "Remote end closed connection without response" } else { "incomplete response" },
            ));
        }
    }
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

enum BodyError {
    Io(io::Error),
    TooBig,
}

/// The body framed by the head (Content-Length, chunked, or to the end); and whether the connection closes after it.
async fn read_body(conn: &mut Conn, head: &Head, limit: usize, head_request: bool) -> Result<(Vec<u8>, bool), BodyError> {
    let mut close = head.will_close();
    if head_request || head.status == 204 || head.status == 304 {
        return Ok((vec![], close));
    }
    let chunked = head.header("transfer-encoding").map(|t| t.to_ascii_lowercase().contains("chunked")).unwrap_or(false);
    if chunked {
        let mut out = Vec::new();
        loop {
            let line_end = loop {
                if let Some(i) = find(&conn.buf, b"\r\n") {
                    break i;
                }
                if read_more(conn).await.map_err(BodyError::Io)? == 0 {
                    return Err(BodyError::Io(io::Error::new(io::ErrorKind::UnexpectedEof, "incomplete chunked read")));
                }
            };
            let line = String::from_utf8_lossy(&conn.buf[..line_end]).to_string();
            let size = usize::from_str_radix(line.split(';').next().unwrap_or("").trim(), 16)
                .map_err(|_| BodyError::Io(io::Error::new(io::ErrorKind::InvalidData, "bad chunk size")))?;
            conn.buf.drain(..line_end + 2);
            if size == 0 {
                // trailers, up to the empty line
                loop {
                    if let Some(i) = find(&conn.buf, b"\r\n") {
                        let empty = i == 0;
                        conn.buf.drain(..i + 2);
                        if empty {
                            break;
                        }
                        continue;
                    }
                    if read_more(conn).await.map_err(BodyError::Io)? == 0 {
                        break;
                    }
                }
                return Ok((out, close));
            }
            if out.len() + size > limit {
                return Err(BodyError::TooBig);
            }
            while conn.buf.len() < size + 2 {
                if read_more(conn).await.map_err(BodyError::Io)? == 0 {
                    return Err(BodyError::Io(io::Error::new(io::ErrorKind::UnexpectedEof, "incomplete chunked read")));
                }
            }
            out.extend_from_slice(&conn.buf[..size]);
            conn.buf.drain(..size + 2);
        }
    }
    if let Some(len) = head.header("content-length").and_then(|v| v.trim().parse::<usize>().ok()) {
        if len > limit {
            return Err(BodyError::TooBig);
        }
        while conn.buf.len() < len {
            if read_more(conn).await.map_err(BodyError::Io)? == 0 {
                return Err(BodyError::Io(io::Error::new(io::ErrorKind::UnexpectedEof, "IncompleteRead")));
            }
        }
        let out: Vec<u8> = conn.buf.drain(..len).collect();
        return Ok((out, close));
    }
    // no framing: to the end of the connection
    close = true;
    loop {
        if conn.buf.len() > limit {
            return Err(BodyError::TooBig);
        }
        if read_more(conn).await.map_err(BodyError::Io)? == 0 {
            return Ok((std::mem::take(&mut conn.buf), close));
        }
    }
}

pub fn inflate(raw: Vec<u8>, encoding: &str) -> Res<Vec<u8>> {
    use std::io::Read;
    let encoding = encoding.trim().to_ascii_lowercase();
    let mut out = Vec::new();
    let result = match encoding.as_str() {
        "gzip" | "x-gzip" => flate2::read::GzDecoder::new(&raw[..]).take(MAX_BODY as u64 + 1).read_to_end(&mut out),
        "deflate" => flate2::read::ZlibDecoder::new(&raw[..]).take(MAX_BODY as u64 + 1).read_to_end(&mut out),
        _ => return Ok(raw),
    };
    if result.is_err() {
        return Ok(raw);
    }
    if out.len() > MAX_BODY {
        return Err(RemoteError::new("接口返回的数据过大"));
    }
    Ok(out)
}

impl Http {
    pub fn new() -> Http {
        Http {
            pool: RefCell::new(HashMap::new()),
            stats: RefCell::new(Stats::default()),
            net: RefCell::new(HashMap::new()),
            tls: TlsConnector::from(tls_config()),
            agent: user_agent(),
        }
    }

    pub fn tls(&self) -> &TlsConnector {
        &self.tls
    }

    /// The most recently used pooled connection to host:port when used within ``max_idle`` seconds and not closed by the server
    /// meanwhile (its FIN or close_notify already in), else none (and the pooled ones idle past KEEPALIVE_IDLE dropped).
    fn take(&self, key: &(String, u16, bool), max_idle: f64) -> Option<Conn> {
        let mut pool = self.pool.borrow_mut();
        let rows = pool.get_mut(key)?;
        let now = mono();
        while let Some((_, last)) = rows.last() {
            if now - *last > max_idle {
                break;
            }
            let (mut conn, _) = rows.pop()?;
            if conn.closed() {
                self.stats.borrow_mut().dead += 1;
                continue;
            }
            self.stats.borrow_mut().reused += 1;
            return Some(conn);
        }
        rows.retain(|(_, last)| now - *last <= KEEPALIVE_IDLE);
        None
    }

    fn put(&self, key: (String, u16, bool), conn: Conn) {
        if !conn.buf.is_empty() {
            return; // (bytes past the answer nobody asked for: the connection is not trusted with another request)
        }
        let mut pool = self.pool.borrow_mut();
        let rows = pool.entry(key).or_default();
        if rows.len() >= KEEPALIVE_PER_HOST {
            return;
        }
        rows.push((conn, mono()));
    }

    /// Seconds since the most recently used pooled connection to host:port was last used; None when there is none.
    pub fn pool_fresh(&self, host: &str, port: u16) -> Option<f64> {
        let pool = self.pool.borrow();
        let rows = pool.get(&(host.to_string(), port, true)).or_else(|| pool.get(&(host.to_string(), port, false)))?;
        rows.last().map(|(_, last)| mono() - last)
    }

    pub fn pooled(&self, host: &str) -> usize {
        self.pool.borrow().iter().filter(|(k, _)| k.0 == host).map(|(_, v)| v.len()).sum()
    }

    pub fn net_of(&self, host: &str) -> Net {
        self.net.borrow().get(host).cloned().unwrap_or_default()
    }

    async fn open(&self, url: &Url, timeout: Duration) -> io::Result<Conn> {
        self.stats.borrow_mut().new += 1;
        let (stream, ms) = connect(url.https, &url.host, url.port, timeout, &self.tls).await?;
        let mut net = self.net.borrow_mut();
        let entry = net.entry(url.host.clone()).or_default();
        entry.connect.push_back(ms);
        while entry.connect.len() > 40 {
            entry.connect.pop_front();
        }
        Ok(Conn { stream, buf: Vec::new() })
    }

    /// GET (or POST ``payload``: JSON bytes already encoded) and return the body; HTTP and network failures become RemoteError.
    pub async fn request(&self, url: &str, payload: Option<&[u8]>, timeout: f64, headers: &[(&str, &str)]) -> Res<Vec<u8>> {
        self.request_gated(url, payload, timeout, headers, None, None).await
    }

    /// ``request`` with an order's last check: the caller judged it just before this call; whenever this call then waits
    /// before the bytes are written (a new connection's TCP + TLS handshake, a stale pooled one found closed and replaced), it
    /// is judged again right before the write — a reason refuses the request (NotSent) and nothing of it leaves.
    pub async fn request_gated(
        &self,
        url: &str,
        payload: Option<&[u8]>,
        timeout: f64,
        headers: &[(&str, &str)],
        gate: Option<&dyn Fn() -> String>,
        trace: Option<&Trace>,
    ) -> Res<Vec<u8>> {
        let timeout_d = Duration::from_secs_f64(timeout.max(0.1));
        let mut url = Url::parse(url)?;
        let mut hdrs: Vec<(String, String)> = vec![
            ("User-Agent".into(), self.agent.clone()),
            ("Accept".into(), "application/json".into()),
            ("Accept-Encoding".into(), "gzip".into()),
        ];
        if payload.is_some() {
            hdrs.push(("Content-Type".into(), "application/json".into()));
        }
        for (k, v) in headers {
            hdrs.retain(|(name, _)| !name.eq_ignore_ascii_case(k));
            hdrs.push((k.to_string(), v.to_string()));
        }
        for _hop in 0..4 {
            let key = (url.host.clone(), url.port, url.https);
            let max_idle = if payload.is_some() { KEEPALIVE_POST_IDLE } else { KEEPALIVE_IDLE };
            let mut wire = Vec::with_capacity(512 + payload.map_or(0, <[u8]>::len));
            let method = if payload.is_some() { "POST" } else { "GET" };
            let host_header = if (url.https && url.port == 443) || (!url.https && url.port == 80) {
                url.host.clone()
            } else {
                format!("{}:{}", url.host, url.port)
            };
            wire.extend_from_slice(format!("{method} {} HTTP/1.1\r\nHost: {host_header}\r\n", url.path).as_bytes());
            for (k, v) in &hdrs {
                wire.extend_from_slice(format!("{k}: {v}\r\n").as_bytes());
            }
            if let Some(body) = payload {
                wire.extend_from_slice(format!("Content-Length: {}\r\n\r\n", body.len()).as_bytes());
                wire.extend_from_slice(body);
            } else {
                wire.extend_from_slice(b"\r\n");
            }
            // (an await since the caller's check: the order is judged again before its bytes go out)
            let mut waited = false;
            let (mut conn, head) = loop {
                let (mut conn, reused) = match self.take(&key, max_idle) {
                    Some(c) => {
                        if let Some(t) = trace {
                            t.connect_ms.set(None);
                        }
                        (c, true)
                    }
                    None => {
                        waited = true;
                        let opened = mono();
                        // no connection (refused, unreachable, the TLS handshake failed or timed out): not a byte of the
                        // request was written — a POST is told as not sent, so an order that never left holds nothing
                        let conn = self.open(&url, timeout_d).await.map_err(|e| {
                            let error = net_error(&e);
                            if payload.is_some() {
                                RemoteError::not_sent(format!("{NOT_CONNECTED}（{}），请求没有发出", error.msg))
                            } else {
                                error
                            }
                        })?;
                        if let Some(t) = trace {
                            t.connect_ms.set(Some((mono() - opened) * 1000.0));
                        }
                        (conn, false)
                    }
                };
                if let (true, Some(check)) = (waited, gate) {
                    let why = check();
                    if !why.is_empty() {
                        self.put(key.clone(), conn); // (nothing went out on it: it serves the next request)
                        return Err(RemoteError::not_sent(why));
                    }
                }
                let sent = tokio::time::timeout(timeout_d, async {
                    conn.stream.write_all(&wire).await?;
                    conn.stream.flush().await
                })
                .await
                .unwrap_or_else(|_| Err(io::Error::new(io::ErrorKind::TimedOut, "timed out")));
                if let (Ok(()), Some(t)) = (&sent, trace) {
                    t.wrote_wall.set(crate::util::now_ms());
                }
                if let Err(e) = sent {
                    if reused {
                        self.stats.borrow_mut().retried += 1; // the server had closed it; nothing of this request was taken
                        waited = true;
                        continue;
                    }
                    return Err(net_error(&e));
                }
                match tokio::time::timeout(timeout_d, read_head(&mut conn)).await {
                    Ok(Ok(head)) => break (conn, head),
                    Ok(Err(e)) => {
                        // a GET the closed connection swallowed: again; a POST may have been taken
                        if reused && payload.is_none() && e.kind() != io::ErrorKind::InvalidData {
                            self.stats.borrow_mut().retried += 1;
                            continue;
                        }
                        return Err(net_error(&e));
                    }
                    Err(_) => return Err(net_error(&io::Error::new(io::ErrorKind::TimedOut, "timed out"))),
                }
            };
            if let Some(edge) = head.header("cf-ray").or_else(|| head.header("server")) {
                let mut net = self.net.borrow_mut();
                net.entry(url.host.clone()).or_default().edge = edge.chars().take(40).collect();
            }
            let limits: Vec<String> = head
                .headers
                .iter()
                .filter(|(k, _)| {
                    let k = k.to_ascii_lowercase();
                    k.contains("ratelimit") || k.contains("rate-limit")
                })
                .map(|(k, v)| format!("{}: {}", k.to_ascii_lowercase(), v.chars().take(40).collect::<String>()))
                .collect();
            if !limits.is_empty() {
                self.net.borrow_mut().entry(url.host.clone()).or_default().limits = limits.join("; ");
            }
            let status = head.status;
            let encoding = head.header("content-encoding").unwrap_or("").to_string();
            if status >= 400 {
                let raw =
                    match tokio::time::timeout(timeout_d.min(Duration::from_secs(5)), read_body(&mut conn, &head, 65_536, false))
                        .await
                    {
                        Ok(Ok((raw, close))) => {
                            if !close {
                                self.put(key, conn); // (its answer read whole: the connection serves the next request)
                            }
                            raw
                        }
                        _ => vec![],
                    };
                let raw = inflate(raw, &encoding).unwrap_or_default();
                return Err(remote_error(&url.host, status, head.header("retry-after").unwrap_or(""), &raw));
            }
            if (300..400).contains(&status) {
                let location = head.header("location").map(str::to_string);
                if let (Some(location), None) = (location, payload) {
                    let drained = tokio::time::timeout(timeout_d, read_body(&mut conn, &head, MAX_BODY, false)).await;
                    if let Ok(Ok((_, false))) = drained {
                        self.put(key, conn);
                    }
                    let next = url.join(&location)?;
                    if next.https != url.https || next.host != url.host || next.port != url.port {
                        hdrs.retain(|(k, _)| !SECRET_HEADERS.contains(&k.to_ascii_lowercase().as_str()));
                    }
                    url = next;
                    continue;
                }
                return Err(RemoteError::new(format!(
                    "HTTP {status}: 重定向没有跟随（{}）",
                    if payload.is_some() { "POST 不会重发" } else { "没有给出去向" }
                )));
            }
            let deadline = Duration::from_secs_f64(2.0 * timeout + 1.0);
            let (raw, close) = match tokio::time::timeout(deadline, read_body(&mut conn, &head, MAX_BODY, false)).await {
                Ok(Ok(got)) => got,
                Ok(Err(BodyError::TooBig)) => return Err(RemoteError::new("接口返回的数据过大")),
                Ok(Err(BodyError::Io(e))) => return Err(net_error(&e)),
                Err(_) => {
                    return Err(RemoteError::new(format!("网络错误 (响应过慢：{} 秒内没有读完)", (2.0 * timeout + 1.0) as i64)));
                }
            };
            if !close {
                self.put(key, conn);
            }
            return inflate(raw, &encoding);
        }
        Err(RemoteError::new("网络错误 (重定向过多)"))
    }

    pub async fn request_json(
        &self,
        url: &str,
        payload: Option<&[u8]>,
        timeout: f64,
        headers: &[(&str, &str)],
    ) -> Res<serde_json::Value> {
        let raw = self.request(url, payload, timeout, headers).await?;
        serde_json::from_slice(&raw).map_err(|_| RemoteError::new("接口未返回有效 JSON"))
    }
}

impl Default for Http {
    fn default() -> Http {
        Http::new()
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use tokio::net::TcpListener;

    /// A local HTTP/1.1 server answering each request with ``answer(method, path, body)`` -> raw response bytes; ``close``
    /// after how many requests a connection is dropped (0: never).
    pub async fn serve(answer: fn(&str, &str, &[u8]) -> Vec<u8>, close_after: usize) -> (u16, Rc<RefCell<Vec<String>>>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let log = Rc::new(RefCell::new(vec![]));
        let seen = log.clone();
        tokio::task::spawn_local(async move {
            let mut conn_no = 0;
            loop {
                let Ok((mut sock, _)) = listener.accept().await else { return };
                conn_no += 1;
                let seen = seen.clone();
                let n = conn_no;
                tokio::task::spawn_local(async move {
                    let mut buf = Vec::new();
                    let mut served = 0;
                    loop {
                        let mut chunk = [0u8; 4096];
                        let got = match sock.read(&mut chunk).await {
                            Ok(0) | Err(_) => return,
                            Ok(k) => k,
                        };
                        buf.extend_from_slice(&chunk[..got]);
                        while let Some(end) = find(&buf, b"\r\n\r\n") {
                            let head = String::from_utf8_lossy(&buf[..end]).to_string();
                            let len = head
                                .lines()
                                .find_map(|l| {
                                    l.to_ascii_lowercase()
                                        .strip_prefix("content-length:")
                                        .map(|v| v.trim().parse::<usize>().unwrap())
                                })
                                .unwrap_or(0);
                            if buf.len() < end + 4 + len {
                                break;
                            }
                            let body = buf[end + 4..end + 4 + len].to_vec();
                            buf.drain(..end + 4 + len);
                            let first = head.lines().next().unwrap_or("").to_string();
                            let mut parts = first.split(' ');
                            let (method, path) = (parts.next().unwrap_or("").to_string(), parts.next().unwrap_or("").to_string());
                            seen.borrow_mut().push(format!("{n} {method} {path}"));
                            let reply = answer(&method, &path, &body);
                            if sock.write_all(&reply).await.is_err() {
                                return;
                            }
                            served += 1;
                            if close_after > 0 && served >= close_after {
                                return;
                            }
                        }
                    }
                });
            }
        });
        (port, log)
    }

    fn answers(method: &str, path: &str, body: &[u8]) -> Vec<u8> {
        match path {
            "/json" => b"HTTP/1.1 200 OK\r\nContent-Length: 11\r\nContent-Type: application/json\r\n\r\n{\"ok\":true}".to_vec(),
            "/chunked" => b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n4\r\n{\"a\"\r\n3\r\n:1}\r\n0\r\n\r\n".to_vec(),
            "/gzip" => {
                use std::io::Write;
                let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
                enc.write_all(b"{\"zipped\":1}").unwrap();
                let z = enc.finish().unwrap();
                let mut out =
                    format!("HTTP/1.1 200 OK\r\nContent-Encoding: gzip\r\nContent-Length: {}\r\n\r\n", z.len()).into_bytes();
                out.extend(z);
                out
            }
            "/limited" => b"HTTP/1.1 429 Too Many\r\nRetry-After: 7\r\nContent-Length: 2\r\n\r\n{}".to_vec(),
            "/predict" => {
                let b = br#"{"success":false,"message":"Order value too small"}"#;
                let mut out = format!("HTTP/1.1 400 Bad\r\nContent-Length: {}\r\n\r\n", b.len()).into_bytes();
                out.extend_from_slice(b);
                out
            }
            "/moved" => b"HTTP/1.1 302 Found\r\nLocation: /json\r\nContent-Length: 0\r\n\r\n".to_vec(),
            "/echo" => {
                let mut out = format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n", body.len() + method.len()).into_bytes();
                out.extend_from_slice(method.as_bytes());
                out.extend_from_slice(body);
                out
            }
            "/close" => b"HTTP/1.1 200 OK\r\nConnection: close\r\nContent-Length: 2\r\n\r\n{}".to_vec(),
            _ => b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n".to_vec(),
        }
    }

    fn run<F: std::future::Future>(f: F) -> F::Output {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        tokio::task::LocalSet::new().block_on(&rt, f)
    }

    // A throwaway CA and its "localhost" certificate (P-256, valid to 2126), for a TLS server on this machine alone.
    const TEST_CA: &str = "-----BEGIN CERTIFICATE-----\
MIIBnjCCAUWgAwIBAgIUOqfX6aXo6NlIiI/DvptlFIhyX8AwCgYIKoZIzj0EAwIw\
HDEaMBgGA1UEAwwRcnVsZXRha2VyIHRlc3QgQ0EwIBcNMjYxMTIwMDYwMzE5WhgP\
MjEyNjEwMjcwNjAzMTlaMBwxGjAYBgNVBAMMEXJ1bGV0YWtlciB0ZXN0IENBMFkw\
EwYHKoZIzj0CAQYIKoZIzj0DAQcDQgAE/TgO2tFcIpiObsvu2LeLK3aDmIxodKHl\
dxbZ1Xt361EpsH7nozeyK3UkZ583ZnFBjBR6+cMrLoHl7oN5K8PyDqNjMGEwHQYD\
VR0OBBYEFPrSzeGNxhuXPgc3eXAVfcJa3gKfMB8GA1UdIwQYMBaAFPrSzeGNxhuX\
Pgc3eXAVfcJa3gKfMA8GA1UdEwEB/wQFMAMBAf8wDgYDVR0PAQH/BAQDAgIEMAoG\
CCqGSM49BAMCA0cAMEQCIB/IFr5n3t9cDDMD9NAFADRRORHd7fJh9xXdIYhKt4uo\
AiAXQoqBxdZ2fkVarobJM3UVSJBVCOmjnE55H+iX0IMtxA==\
-----END CERTIFICATE-----";
    const TEST_LEAF: &str = "-----BEGIN CERTIFICATE-----\
MIIBvjCCAWSgAwIBAgIUQm5Da2Rjc+E1VJICNND50UBi67UwCgYIKoZIzj0EAwIw\
HDEaMBgGA1UEAwwRcnVsZXRha2VyIHRlc3QgQ0EwIBcNMjYxMTIwMDYwMzIwWhgP\
MjEyNjEwMjcwNjAzMjBaMBQxEjAQBgNVBAMMCWxvY2FsaG9zdDBZMBMGByqGSM49\
AgEGCCqGSM49AwEHA0IABALYsYucdVHYBFFJXS9LHP2O1Mm4KFeQfiq5/0l273H2\
5fZEsXC8Nlv5piHCFNh1pTelRddDNyazSuol9UqmjlKjgYkwgYYwFAYDVR0RBA0w\
C4IJbG9jYWxob3N0MAkGA1UdEwQCMAAwDgYDVR0PAQH/BAQDAgeAMBMGA1UdJQQM\
MAoGCCsGAQUFBwMBMB0GA1UdDgQWBBTbb1YBSPPUQ29NVnOswtnhTRfzvzAfBgNV\
HSMEGDAWgBT60s3hjcYblz4HN3lwFX3CWt4CnzAKBggqhkjOPQQDAgNIADBFAiEA\
mAh6FOKPfIgRbiy6DyVU2OGedMKZCjcpOrhvgEmFgf8CIDouLX/N6Gejt0V/kN3N\
WjuUKhV0CK7XVYafsCk/NzIt\
-----END CERTIFICATE-----";
    const TEST_LEAF_KEY: &str = "-----BEGIN PRIVATE KEY-----\
MIGHAgEAMBMGByqGSM49AgEGCCqGSM49AwEHBG0wawIBAQQg1l5p9ecwhLZ8+qiE\
ugon6QeKhziJ6kufs07JYpM//KihRANCAAQC2LGLnHVR2ARRSV0vSxz9jtTJuChX\
kH4quf9Jdu9x9uX2RLFwvDZb+aYhwhTYdaU3pUXXQzcms0rqJfVKpo5S\
-----END PRIVATE KEY-----";

    fn pem_der(pem: &str) -> Vec<u8> {
        use base64::Engine;
        let text: String = pem.split("-----").filter(|p| !p.is_empty() && !p.contains("BEGIN") && !p.contains("END")).collect();
        base64::engine::general_purpose::STANDARD.decode(text.trim()).unwrap()
    }

    /// A local TLS 1.3 server on one connection (its session tickets sent once the handshake is done, as a TLS 1.3 server
    /// does): each request answered "ok"; ``after_ms`` after the first answer, ``then`` — a close_notify (the TCP connection
    /// left open), bytes nobody asked for, or nothing.
    async fn tls_serve(then: &'static str, after_ms: u64) -> u16 {
        use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
        let certs = vec![CertificateDer::from(pem_der(TEST_LEAF))];
        let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(pem_der(TEST_LEAF_KEY)));
        let config = rustls::ServerConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
            .with_protocol_versions(&[&rustls::version::TLS13])
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(certs, key)
            .unwrap();
        let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(config));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::task::spawn_local(async move {
            let (tcp, _) = listener.accept().await.unwrap();
            let mut tls = acceptor.accept(tcp).await.unwrap();
            let (mut buf, mut served) = (Vec::new(), 0);
            loop {
                let mut chunk = [0u8; 4096];
                let n = match tls.read(&mut chunk).await {
                    Ok(0) | Err(_) => return,
                    Ok(n) => n,
                };
                buf.extend_from_slice(&chunk[..n]);
                while let Some(end) = find(&buf, b"\r\n\r\n") {
                    buf.drain(..end + 4);
                    tls.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok").await.unwrap();
                    tls.flush().await.unwrap();
                    served += 1;
                    if served == 1 {
                        tokio::time::sleep(Duration::from_millis(after_ms)).await; // (0: on the answer's heels)
                        match then {
                            "close_notify" => tls.get_mut().1.send_close_notify(),
                            "408" => tls.write_all(b"HTTP/1.1 408 Request Timeout\r\nContent-Length: 0\r\n\r\n").await.unwrap(),
                            _ => {}
                        }
                        tls.flush().await.unwrap();
                    }
                }
            }
        });
        port
    }

    async fn get_ok(conn: &mut Conn) -> Vec<u8> {
        conn.stream.write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\n\r\n").await.unwrap();
        conn.stream.flush().await.unwrap();
        let head = read_head(conn).await.unwrap();
        match read_body(conn, &head, 1000, false).await {
            Ok((body, _)) => body,
            Err(_) => panic!("no body"),
        }
    }

    #[test]
    fn a_tls_record_of_its_own_after_the_answer_leaves_the_connection_good() {
        run(async {
            let mut roots = rustls::RootCertStore::empty();
            roots.add(rustls::pki_types::CertificateDer::from(pem_der(TEST_CA))).unwrap();
            let config = rustls::ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
                .with_safe_default_protocol_versions()
                .unwrap()
                .with_root_certificates(roots)
                .with_no_client_auth();
            let connector = TlsConnector::from(Arc::new(config));
            // the session tickets come after the client's side of the handshake is done: on the socket, nothing for us in them
            let port = tls_serve("", 0).await;
            let (stream, _) = connect(true, "localhost", port, Duration::from_secs(5), &connector).await.unwrap();
            let mut conn = Conn { stream, buf: Vec::new() };
            tokio::time::sleep(Duration::from_millis(100)).await;
            assert!(!conn.closed(), "session tickets");
            assert_eq!(get_ok(&mut conn).await, b"ok");
            let cases = [("close_notify", true), ("408", true), ("", false)];
            for ((then, dead), after_ms) in cases.iter().flat_map(|c| [(*c, 0), (*c, 40)]) {
                let port = tls_serve(then, after_ms).await;
                let (stream, _) = connect(true, "localhost", port, Duration::from_secs(5), &connector).await.unwrap();
                let mut conn = Conn { stream, buf: Vec::new() };
                assert_eq!(get_ok(&mut conn).await, b"ok");
                tokio::time::sleep(Duration::from_millis(100)).await; // (what the server sent after the answer is in)
                assert_eq!(conn.closed(), dead, "{then} {after_ms} ms after");
                if !dead {
                    assert_eq!(get_ok(&mut conn).await, b"ok", "{then}"); // and it carries the next request
                    assert!(!conn.closed());
                }
            }
        });
    }

    #[test]
    fn an_order_s_way_out_is_timed() {
        run(async {
            let (port, _) = serve(answers, 0).await;
            let h = Http::new();
            let url = format!("http://127.0.0.1:{port}/echo");
            let trace = Trace::default();
            let before = crate::util::now_ms();
            h.request_gated(&url, Some(b"1"), 5.0, &[], None, Some(&trace)).await.unwrap();
            assert!(trace.connect_ms.get().is_some_and(|ms| ms >= 0.0)); // (no warm one: a handshake)
            assert!(trace.wrote_wall.get() >= before);
            let again = Trace::default();
            h.request_gated(&url, Some(b"2"), 5.0, &[], None, Some(&again)).await.unwrap();
            assert_eq!(again.connect_ms.get(), None); // (the warm one reused)
            assert!(again.wrote_wall.get() >= before);
        });
    }

    #[test]
    fn an_error_answer_read_whole_keeps_its_connection() {
        run(async {
            let (port, log) = serve(answers, 0).await;
            let h = Http::new();
            let base = format!("http://127.0.0.1:{port}");
            let e = h.request(&format!("{base}/predict"), Some(b"{}"), 5.0, &[]).await.unwrap_err();
            assert_eq!(e.msg, "HTTP 400: Order value too small");
            assert_eq!(h.request_json(&format!("{base}/json"), None, 5.0, &[]).await.unwrap()["ok"], true);
            assert_eq!(*log.borrow(), vec![format!("1 POST /predict"), format!("1 GET /json")]); // one connection
            assert_eq!(h.stats.borrow().new, 1);
        });
    }

    #[test]
    fn requests() {
        run(async {
            let (port, log) = serve(answers, 0).await;
            let h = Http::new();
            let base = format!("http://127.0.0.1:{port}");
            assert_eq!(h.request_json(&format!("{base}/json"), None, 5.0, &[]).await.unwrap()["ok"], true);
            assert_eq!(h.request_json(&format!("{base}/chunked"), None, 5.0, &[]).await.unwrap()["a"], 1);
            assert_eq!(h.request_json(&format!("{base}/gzip"), None, 5.0, &[]).await.unwrap()["zipped"], 1);
            assert_eq!(h.request(&format!("{base}/echo"), Some(b"{\"x\":1}"), 5.0, &[]).await.unwrap(), b"POST{\"x\":1}");
            let e = h.request(&format!("{base}/limited"), None, 5.0, &[]).await.unwrap_err();
            assert_eq!((e.status(), e.retry_after), (429, 7));
            assert_eq!(e.msg, "HTTP 429: 接口限流，等待后重试");
            assert_eq!(h.request_json(&format!("{base}/moved"), None, 5.0, &[]).await.unwrap()["ok"], true);
            let e = h.request(&format!("{base}/moved"), Some(b"{}"), 5.0, &[]).await.unwrap_err();
            assert!(e.msg.contains("POST 不会重发"), "{e}");
            // one connection carried them all (but for the POST's redirect, which is not followed: its rest left unread)
            let conns: std::collections::HashSet<String> =
                log.borrow().iter().map(|l| l.split(' ').next().unwrap().to_string()).collect();
            assert!(conns.len() <= 2, "{:?}", log.borrow());
            assert!(h.stats.borrow().reused >= 3);
        });
    }

    #[test]
    fn stale_connections() {
        run(async {
            // the server drops each connection after one answer without saying so
            let (port, log) = serve(answers, 1).await;
            let h = Http::new();
            let base = format!("http://127.0.0.1:{port}");
            for _ in 0..3 {
                assert_eq!(h.request_json(&format!("{base}/json"), None, 5.0, &[]).await.unwrap()["ok"], true);
            }
            // the connections the server closed are found closed before a request goes out on them
            assert_eq!(log.borrow().len(), 3);
            assert_eq!(h.stats.borrow().retried, 0);
            assert!(h.stats.borrow().dead >= 2, "{:?}", h.stats.borrow());
            // so the POST goes out once, on a fresh connection
            assert_eq!(h.request(&format!("{base}/echo"), Some(b"1"), 5.0, &[]).await.unwrap(), b"POST1");
            assert_eq!(log.borrow().iter().filter(|l| l.ends_with("POST /echo")).count(), 1);
        });
    }

    #[test]
    fn an_order_is_judged_again_after_a_new_connection() {
        run(async {
            let (port, log) = serve(answers, 0).await;
            let h = Http::new();
            let url = format!("http://127.0.0.1:{port}/echo");
            let refuse = || "已暂停".to_string();
            // no pooled connection: one is made (a wait), then the order is judged again — and held, nothing written
            let e = h.request_gated(&url, Some(b"1"), 5.0, &[], Some(&refuse), None).await.unwrap_err();
            assert_eq!((e.kind, e.msg.as_str()), (crate::util::Kind::NotSent, "已暂停"));
            assert!(log.borrow().iter().all(|l| !l.ends_with("POST /echo")), "{:?}", log.borrow());
            // the new connection was kept for the next request; one taken from the pool at once needs no second look (no
            // wait since the caller's own check)
            let pass = || String::new();
            assert_eq!(h.request_gated(&url, Some(b"2"), 5.0, &[], Some(&pass), None).await.unwrap(), b"POST2");
            assert_eq!(h.stats.borrow().new, 1);
        });
    }

    #[test]
    fn a_post_that_got_no_connection_is_not_sent() {
        run(async {
            let port = {
                let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
                l.local_addr().unwrap().port()
            }; // (closed again: nothing listens there)
            let h = Http::new();
            let url = format!("http://127.0.0.1:{port}/orders");
            let e = h.request(&url, Some(b"{}"), 2.0, &[]).await.unwrap_err();
            assert_eq!(e.kind, crate::util::Kind::NotSent, "{}", e.msg);
            assert!(e.msg.starts_with(NOT_CONNECTED), "{}", e.msg);
            let e = h.request(&url, None, 2.0, &[]).await.unwrap_err(); // a read: as before
            assert_eq!(e.kind, crate::util::Kind::Remote, "{}", e.msg);
        });
    }

    #[test]
    fn errors_and_urls() {
        let e = remote_error("api.predict.fun", 400, "", br#"{"success":false,"message":"Order value too small"}"#);
        assert_eq!(e.msg, "HTTP 400: Order value too small");
        let e = remote_error("api.predict.fun", 403, "", b"");
        assert!(e.msg.starts_with("HTTP 403: 访问被拒绝；请核对权限与服务地区（Predict 不向受限地区开放交易"));
        let e = remote_error("fapi.binance.com", 429, "", b"");
        assert_eq!(e.retry_after, 30);
        let e = remote_error(
            "api.telegram.org",
            429,
            "",
            br#"{"ok":false,"description":"Too Many Requests: retry after 5","parameters":{"retry_after":5}}"#,
        );
        assert_eq!((e.msg.as_str(), e.retry_after), ("HTTP 429: Too Many Requests: retry after 5", 5));
        let u = Url::parse("https://api.predict.fun/v1/markets?x=1").unwrap();
        assert_eq!((u.host.as_str(), u.port, u.path.as_str()), ("api.predict.fun", 443, "/v1/markets?x=1"));
        assert_eq!(u.join("/v2/a").unwrap().path, "/v2/a");
        assert_eq!(u.join("b").unwrap().path, "/v1/b");
        assert_eq!(Url::parse("http://127.0.0.1:8080").unwrap().port, 8080);
    }
}
