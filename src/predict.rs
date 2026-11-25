//! Predict.fun's REST API (api.predict.fun/v1) and the BNB-chain reads an order needs (huice live.PredictApi,
//! preopen.safe_api / category_market, main.PredictPacer and the market parsers).
//!
//! The API key goes in x-api-key on every request; the JWT from the sign-in flow (GET /auth/message → sign → POST /auth) in
//! Authorization, renewed before it expires or after a 401. Every key has a budget (PREDICT_RATE_PER_MIN, 220 a minute against
//! Predict's 240): a read waits for its token, an order or a cancel may borrow a burst ahead of the reads. A 429 pauses every
//! request on the client for its retry-after (20 s without one). Orders and cancels have slots of their own, never queued
//! behind a read.
use crate::rules::Level;
use crate::util::{Obj, Py, RemoteError, Res, as_f64, brief_error, clean_error, http_error_text, mono, py_str, truthy};
use crate::wallet::{Wallet, keccak};
use base64::Engine;
use futures_util::future::LocalBoxFuture;
use rust_decimal::Decimal;
use serde_json::{Value, json};
use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::rc::Rc;
use std::str::FromStr;
use tokio::sync::{Mutex, Semaphore};

pub const PREDICT_REST: &str = "https://api.predict.fun/v1";
pub const API_TIMEOUT: f64 = 12.0;
/// The sign-in renewed in the background this long before its token runs out; a request still uses the token until
/// AUTH_USABLE_S before (s).
pub const AUTH_RENEW_S: i64 = 300;
pub const AUTH_USABLE_S: i64 = 30;
pub const ORDER_TIMEOUT: f64 = 20.0;
pub const RPC_TIMEOUT: f64 = 15.0;
pub const PREDICT_DEPTH: usize = 20;
pub const UP_WORDS: [&str; 6] = ["yes", "up", "涨", "上涨", "higher", "above"];
pub const DOWN_WORDS: [&str; 6] = ["no", "down", "跌", "下跌", "lower", "below"];
pub const DUPLICATE_WORDS: [&str; 4] = ["already exist", "already placed", "duplicate", "exists"];
pub const LISTING_FULL_FIELDS: [&str; 9] =
    ["id", "conditionId", "status", "tradingStatus", "question", "outcomes", "feeRateBps", "isNegRisk", "isYieldBearing"];
pub const PRICE_FIELDS: [&str; 5] = ["startPrice", "priceToBeat", "targetPrice", "strikePrice", "referencePrice"];

pub fn api_base(chain_id: i64) -> &'static str {
    if chain_id == 97 { "https://api-testnet.predict.fun/v1" } else { PREDICT_REST }
}

pub fn rpc_url(chain_id: i64) -> &'static str {
    if chain_id == 97 { "https://bsc-testnet-dataseed.bnbchain.org/" } else { "https://bsc-dataseed.bnbchain.org/" }
}

// --- the transport (the real one is http(); the tests put a fake here) ------------------------------------------------------
/// Judged right before an order's bytes go out ("" sends it; a reason refuses it — nothing leaves).
pub type Gate = Rc<dyn Fn() -> String>;

pub struct Req {
    pub url: String,
    pub payload: Option<Rc<Vec<u8>>>,
    pub timeout: f64,
    pub headers: Vec<(String, String)>,
    /// an order's last check: judged again once a new connection is up (its TCP + TLS handshake is a wait the pause, the
    /// mode or the start's line may have come during), right before its bytes are written
    pub gate: Option<Gate>,
    /// an order's timing on its way, filled in by the network layer (None: not asked for)
    pub trace: Option<Rc<crate::http::Trace>>,
}

pub trait Transport {
    fn request(&self, req: Req) -> LocalBoxFuture<'_, Res<Vec<u8>>>;
}

pub struct Live;

impl Transport for Live {
    fn request(&self, req: Req) -> LocalBoxFuture<'_, Res<Vec<u8>>> {
        Box::pin(async move {
            let headers: Vec<(&str, &str)> = req.headers.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
            crate::http::http()
                .request_gated(
                    &req.url,
                    req.payload.as_deref().map(Vec::as_slice),
                    req.timeout,
                    &headers,
                    req.gate.as_deref(),
                    req.trace.as_deref(),
                )
                .await
        })
    }
}

pub fn http_status(error: &RemoteError) -> u16 {
    error.status()
}

pub fn jwt_expiry(token: &str) -> i64 {
    let Some(payload) = token.split('.').nth(1) else { return 0 };
    let Ok(raw) = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(payload.trim_end_matches('=')) else { return 0 };
    serde_json::from_slice::<Value>(&raw).ok().map(|d| d.i("exp")).unwrap_or(0)
}

/// An API answer's field names (one level into "data"), for an error that must not quote its values.
pub fn answer_shape(answer: &Value) -> String {
    let Some(obj) = answer.as_object() else {
        return match answer {
            Value::Null => "NoneType".into(),
            Value::Array(_) => "list".into(),
            Value::String(_) => "str".into(),
            Value::Bool(_) => "bool".into(),
            Value::Number(_) => "number".into(),
            Value::Object(_) => "dict".into(),
        };
    };
    let mut names: Vec<String> = obj.keys().cloned().collect();
    names.sort();
    if let Some(inner) = obj.get("data").and_then(Value::as_object) {
        let mut inner_names: Vec<String> = inner.keys().cloned().collect();
        inner_names.sort();
        names.push(format!("data{{{}}}", inner_names.join(",")));
    }
    names.join(",").chars().take(160).collect()
}

fn type_name(v: &Value) -> &'static str {
    match v {
        Value::Null => "NoneType",
        Value::Array(_) => "list",
        Value::String(_) => "str",
        Value::Bool(_) => "bool",
        Value::Number(n) => {
            if n.is_f64() {
                "float"
            } else {
                "int"
            }
        }
        Value::Object(_) => "dict",
    }
}

// --- the request budget of a key ------------------------------------------------------------------------------------------
/// A key's request budget: so many tokens a minute, a burst of BURST; a read waits for a whole token, an order or a cancel
/// (``write``) may borrow up to a burst ahead of the reads. Empty at first: a process just started does not know how much of
/// Predict's minute the one before it used.
pub struct Pacer {
    pub per_min: f64,
    pub tokens: f64,
    pub at: f64,
    pub waited: f64,
    pub waits: VecDeque<(f64, f64)>,
    pub sent: VecDeque<f64>,
    pub limits: VecDeque<(f64, usize)>,
}

impl Pacer {
    pub const BURST: f64 = 8.0;

    pub fn new(per_min: f64) -> Pacer {
        Pacer {
            per_min,
            tokens: 0.0,
            at: mono(),
            waited: 0.0,
            waits: VecDeque::new(),
            sent: VecDeque::new(),
            limits: VecDeque::new(),
        }
    }
    pub fn refill(&mut self) {
        let now = mono();
        self.tokens = Self::BURST.min(self.tokens + (now - self.at) * self.per_min / 60.0);
        self.at = now;
    }
    /// Whether a request would get its token this instant (no wait).
    pub fn ready(&mut self, write: bool) -> bool {
        if self.per_min <= 0.0 {
            return true;
        }
        self.refill();
        self.tokens >= (if write { 1.0 - Self::BURST } else { 1.0 }) - 1e-9
    }
    pub fn last_minute(&self) -> usize {
        let cut = mono() - 60.0;
        self.sent.iter().filter(|t| **t >= cut).count()
    }
    pub fn waited_last(&self, seconds: f64) -> (usize, f64) {
        let cut = mono() - seconds;
        let waits: Vec<f64> = self.waits.iter().filter(|(t, _)| *t >= cut).map(|(_, w)| *w).collect();
        (waits.len(), waits.iter().sum())
    }
    pub fn note_429(&mut self) {
        let n = self.last_minute();
        push_cap(&mut self.limits, (mono(), n), 200);
    }
}

pub fn push_cap<T>(q: &mut VecDeque<T>, v: T, cap: usize) {
    q.push_back(v);
    while q.len() > cap {
        q.pop_front();
    }
}

pub async fn pacer_take(pacer: &RefCell<Pacer>, write: bool) {
    let need = if write { 1.0 - Pacer::BURST } else { 1.0 };
    let mut began = 0.0;
    loop {
        let wait = {
            let mut p = pacer.borrow_mut();
            if p.per_min <= 0.0 {
                break;
            }
            p.refill();
            if p.tokens >= need - 1e-9 {
                p.tokens -= 1.0;
                None
            } else {
                let w = (need - p.tokens) * 60.0 / p.per_min;
                p.waited += w;
                Some(w)
            }
        };
        match wait {
            None => break,
            Some(w) => {
                if began == 0.0 {
                    began = mono();
                }
                tokio::time::sleep(std::time::Duration::from_secs_f64(w)).await;
            }
        }
    }
    let mut p = pacer.borrow_mut();
    if began != 0.0 {
        let now = mono();
        push_cap(&mut p.waits, (now, now - began), 4096);
    }
    let now = mono();
    push_cap(&mut p.sent, now, 4096);
}

// --- the API --------------------------------------------------------------------------------------------------------------
pub type BeforeSend = Rc<dyn Fn(&str) -> String>;

pub struct Created {
    pub order_id: String,
    pub hash: String,
    /// its way out: waiting for the key's budget and a slot (ms), a new connection's handshake (ms; None: a warm one), the
    /// moment its bytes were all written (wall ms; 0 when not known)
    pub queued_ms: f64,
    pub connect_ms: Option<f64>,
    pub wrote_wall: i64,
}

pub struct PredictApi {
    pub base: String,
    /// the main key (the first of ``keys``)
    pub api_key: String,
    /// the keys, the main one first, then the spares (RULE_API_KEY_2, _3, …): a 429 on the one in use sends the same request
    /// again at once through the next free one, and that one is used from then on; every request waits only when all are
    /// cooling
    pub keys: Vec<String>,
    /// the key in use (an index into ``keys``)
    pub active: Cell<usize>,
    /// per key: monotonic time its 429 cooling ends
    blocked: RefCell<Vec<f64>>,
    /// (monotonic, from, to): each time a 429 moved the requests to another key
    pub switches: RefCell<VecDeque<(f64, usize, usize)>>,
    pub wallet: Option<Rc<Wallet>>,
    pub host: String,
    pub pacer: RefCell<Pacer>,
    /// judged right before an order's bytes go out: "" sends it, a reason refuses it (NotSent)
    pub before_send: RefCell<Option<BeforeSend>>,
    pub jwt: RefCell<String>,
    pub jwt_exp: Cell<i64>,
    pub jwt_at: Cell<f64>,
    pub auth_error: RefCell<String>,
    auth_lock: Mutex<()>,
    gate: Semaphore,
    read_gate: Semaphore,
    pub writes_inflight: Cell<i64>,
    pub calls: Cell<u64>,
    pub limited: RefCell<VecDeque<f64>>,
    /// (method, ms): the request alone, after the pace and the gate
    pub trips: RefCell<VecDeque<(String, f64)>>,
    /// monotonic: the last answered request (a warm connection)
    pub last_ok: Cell<f64>,
    /// the last cancel's answer as Predict gave it (cut short): shown when the order asked for is not in it
    pub last_remove: RefCell<String>,
    pub transport: Rc<dyn Transport>,
    /// the tests' stand-in for whatever a request waits for before its bytes leave (a token, a slot, a sign-in)
    pub pre_send: RefCell<Option<Rc<dyn Fn()>>>,
}

impl PredictApi {
    pub const PARALLEL: usize = 6;
    pub const READ_PARALLEL: usize = 4;
    pub const BACKOFF_SECONDS: f64 = 20.0;

    pub fn new(base: &str, api_key: &str, wallet: Option<Rc<Wallet>>, per_min: f64, transport: Rc<dyn Transport>) -> PredictApi {
        let base = base.trim_end_matches('/').to_string();
        let host = crate::http::Url::parse(&base).map(|u| u.host).unwrap_or_default();
        PredictApi {
            base,
            api_key: api_key.to_string(),
            keys: vec![api_key.to_string()],
            active: Cell::new(0),
            blocked: RefCell::new(vec![0.0]),
            switches: RefCell::new(VecDeque::new()),
            wallet,
            host,
            pacer: RefCell::new(Pacer::new(per_min)),
            before_send: RefCell::new(None),
            jwt: RefCell::new(String::new()),
            jwt_exp: Cell::new(0),
            jwt_at: Cell::new(0.0),
            auth_error: RefCell::new(String::new()),
            auth_lock: Mutex::new(()),
            gate: Semaphore::new(Self::PARALLEL),
            read_gate: Semaphore::new(Self::READ_PARALLEL),
            writes_inflight: Cell::new(0),
            calls: Cell::new(0),
            limited: RefCell::new(VecDeque::new()),
            trips: RefCell::new(VecDeque::new()),
            last_ok: Cell::new(0.0),
            transport,
            pre_send: RefCell::new(None),
            last_remove: RefCell::new(String::new()),
        }
    }

    /// The spare keys after the main one (empty ones and repeats left out).
    pub fn with_spares(mut self, spares: &[String]) -> PredictApi {
        for k in spares {
            let k = k.trim();
            if !k.is_empty() && !self.keys.iter().any(|x| x == k) {
                self.keys.push(k.to_string());
            }
        }
        *self.blocked.borrow_mut() = vec![0.0; self.keys.len()];
        self
    }

    pub fn headers(&self) -> Vec<(String, String)> {
        self.headers_for(self.active.get())
    }

    /// The headers a request through key ``k`` carries (the sign-in's token with it).
    pub fn headers_for(&self, k: usize) -> Vec<(String, String)> {
        let mut out = vec![];
        let key = self.keys.get(k).map(String::as_str).unwrap_or(self.api_key.as_str());
        if !key.is_empty() {
            out.push(("x-api-key".to_string(), key.to_string()));
        }
        let jwt = self.jwt.borrow();
        if !jwt.is_empty() {
            out.push(("Authorization".to_string(), format!("Bearer {jwt}")));
        }
        out
    }

    /// Seconds until a key is free again: none cooling (or a spare free) — zero or less.
    pub fn cooling(&self) -> f64 {
        let now = mono();
        self.blocked.borrow().iter().map(|until| until - now).fold(f64::INFINITY, f64::min)
    }

    /// Seconds key ``k`` still cools (zero or less: free).
    pub fn key_cooling(&self, k: usize) -> f64 {
        self.blocked.borrow().get(k).map(|until| until - mono()).unwrap_or(0.0)
    }

    /// One read through key ``k`` alone (a spare checked before it is needed): its round trip, ms — or why Predict refused it.
    /// Outside the budget and the cooling: the self-check's, a handful a check.
    pub async fn try_key(&self, k: usize, path: &str) -> Res<f64> {
        let began = mono();
        let req = Req {
            url: format!("{}{path}", self.base),
            payload: None,
            timeout: API_TIMEOUT,
            headers: self.headers_for(k),
            gate: None,
            trace: None,
        };
        let raw = self.transport.request(req).await?;
        let answer: Value = serde_json::from_slice(&raw).map_err(|_| RemoteError::new("Predict 接口未返回有效 JSON"))?;
        match answer.get("success") {
            None | Some(Value::Bool(true)) => Ok((mono() - began) * 1000.0),
            _ => Err(RemoteError::new(http_error_text(&answer))),
        }
    }

    /// The key a request goes through: the one in use while it is free, else the next free one (then in use from now on).
    fn pick_key(&self) -> usize {
        let n = self.keys.len().max(1);
        let now = mono();
        let blocked = self.blocked.borrow();
        let at = self.active.get().min(n - 1);
        let k = (0..n).map(|i| (at + i) % n).find(|k| blocked.get(*k).is_none_or(|until| *until <= now)).unwrap_or(at);
        if k != at {
            self.active.set(k);
            push_cap(&mut self.switches.borrow_mut(), (now, at, k), 100);
        }
        k
    }

    fn cooling_error(left: f64) -> RemoteError {
        let n = left as u64 + 1;
        RemoteError::retry(format!("Predict 接口限流冷却中（{n} 秒后重试）"), n)
    }

    /// One request: the key's token, a slot (a read takes one of the reads' slots first), then the request. ``gate``: an
    /// order's check, judged inside the slot right before the bytes go out (and again once a new connection is up). An
    /// explicit success=false is a refusal.
    pub async fn call(
        &self,
        method: &str,
        path: &str,
        payload: Option<Rc<Vec<u8>>>,
        timeout: f64,
        priority: bool,
        gate: Option<Gate>,
    ) -> Res<Value> {
        self.call_traced(method, path, payload, timeout, priority, gate, None).await
    }

    /// ``call``, its way out timed in ``trace`` (an order's).
    #[allow(clippy::too_many_arguments)]
    pub async fn call_traced(
        &self,
        method: &str,
        path: &str,
        payload: Option<Rc<Vec<u8>>>,
        timeout: f64,
        priority: bool,
        gate: Option<Gate>,
        trace: Option<Rc<crate::http::Trace>>,
    ) -> Res<Value> {
        if let Some(t) = &trace {
            t.entered.set(mono());
        }
        let left = self.cooling();
        if left > 0.0 {
            return Err(Self::cooling_error(left));
        }
        let payload = if method == "POST" && payload.is_none() { Some(Rc::new(b"{}".to_vec())) } else { payload };
        pacer_take(&self.pacer, priority).await;
        self.calls.set(self.calls.get() + 1);
        let (raw, began) = if priority {
            self.writes_inflight.set(self.writes_inflight.get() + 1);
            let result = async {
                let _slot = self.gate.acquire().await.map_err(|_| RemoteError::new("closed"))?;
                self.send(path, payload, timeout, gate, trace).await
            }
            .await;
            self.writes_inflight.set(self.writes_inflight.get() - 1);
            result?
        } else {
            let _read = self.read_gate.acquire().await.map_err(|_| RemoteError::new("closed"))?;
            let _slot = self.gate.acquire().await.map_err(|_| RemoteError::new("closed"))?;
            self.send(path, payload, timeout, gate, trace).await?
        };
        push_cap(&mut self.trips.borrow_mut(), (method.to_string(), (mono() - began) * 1000.0), 300);
        self.last_ok.set(mono());
        let answer: Value = if raw.is_empty() {
            json!({})
        } else {
            serde_json::from_slice(&raw).map_err(|_| RemoteError::new("Predict 接口未返回有效 JSON"))?
        };
        let ok = match &answer {
            Value::Object(o) => matches!(o.get("success"), None | Some(Value::Bool(true))),
            _ => false,
        };
        if !ok {
            let why = if answer.is_object() { http_error_text(&answer) } else { type_name(&answer).to_string() };
            return Err(RemoteError::new(format!(
                "Predict 应答不是已确认成功的数据（{method} {}）：{why}",
                path.split('?').next().unwrap_or(path)
            )));
        }
        Ok(answer)
    }

    async fn send(
        &self,
        path: &str,
        payload: Option<Rc<Vec<u8>>>,
        timeout: f64,
        gate: Option<Gate>,
        trace: Option<Rc<crate::http::Trace>>,
    ) -> Res<(Vec<u8>, f64)> {
        let left = self.cooling();
        if left > 0.0 {
            return Err(Self::cooling_error(left));
        }
        if let Some(g) = &gate {
            let hook = self.pre_send.borrow().clone();
            if let Some(hook) = hook {
                hook();
            }
            let why = g();
            if !why.is_empty() {
                return Err(RemoteError::not_sent(why));
            }
        }
        let began = mono();
        if let Some(t) = &trace {
            t.queued_ms.set((began - t.entered.get()) * 1000.0);
        }
        let mut tried = 0;
        loop {
            let k = self.pick_key();
            let req = Req {
                url: format!("{}{path}", self.base),
                payload: payload.clone(),
                timeout,
                headers: self.headers_for(k),
                gate: gate.clone(),
                trace: trace.clone(),
            };
            match self.transport.request(req).await {
                Ok(raw) => return Ok((raw, began)),
                Err(error) => {
                    if error.retry_after > 0 || error.status() == 429 {
                        let until = mono() + (error.retry_after as f64).max(Self::BACKOFF_SECONDS);
                        let mut blocked = self.blocked.borrow_mut();
                        for (i, b) in blocked.iter_mut().enumerate() {
                            // a 429 is this key's budget spent; any other wait asked for is the whole server's: every key
                            if i == k || error.status() != 429 {
                                *b = b.max(until);
                            }
                        }
                        drop(blocked);
                        if error.status() == 429 {
                            push_cap(&mut self.limited.borrow_mut(), mono(), 500);
                            self.pacer.borrow_mut().note_429();
                            // refused before anything was done (an order too: nothing placed): the same request again at once
                            // through a spare that is free — an order's own check before its bytes go judges it again
                            tried += 1;
                            if tried < self.keys.len() && self.cooling() <= 0.0 {
                                continue;
                            }
                        }
                    }
                    return Err(error);
                }
            }
        }
    }

    pub async fn authed(
        &self,
        method: &str,
        path: &str,
        payload: Option<Rc<Vec<u8>>>,
        timeout: f64,
        priority: bool,
        gate: Option<Gate>,
    ) -> Res<Value> {
        self.authed_traced(method, path, payload, timeout, priority, gate, None).await
    }

    /// ``authed``, its way out timed in ``trace`` (an order's; a retry after a refused sign-in times the retry).
    #[allow(clippy::too_many_arguments)]
    pub async fn authed_traced(
        &self,
        method: &str,
        path: &str,
        payload: Option<Rc<Vec<u8>>>,
        timeout: f64,
        priority: bool,
        gate: Option<Gate>,
        trace: Option<Rc<crate::http::Trace>>,
    ) -> Res<Value> {
        self.ensure_auth().await?;
        let used = self.jwt.borrow().clone();
        match self.call_traced(method, path, payload.clone(), timeout, priority, gate.clone(), trace.clone()).await {
            Err(error) if error.status() == 401 && self.wallet.is_some() => {
                if *self.jwt.borrow() == used {
                    self.jwt.borrow_mut().clear(); // only the token that was refused is dropped
                }
                self.ensure_auth().await?;
                self.call_traced(method, path, payload, timeout, priority, gate, trace).await
            }
            other => other,
        }
    }

    /// Signed in, the token good for AUTH_RENEW_S more: when not, the readiness loop renews it in the background (renew_auth).
    pub fn fresh(&self) -> bool {
        self.good_for(AUTH_RENEW_S)
    }

    /// Signed in, the token good for AUTH_USABLE_S more: a request (an order) uses it as it is.
    pub fn usable(&self) -> bool {
        self.good_for(AUTH_USABLE_S)
    }

    fn good_for(&self, seconds: i64) -> bool {
        let exp = self.jwt_exp.get();
        !self.jwt.borrow().is_empty() && (exp == 0 || (crate::util::now_ms() / 1000) < exp - seconds)
    }

    /// Signed in for a request now: a sign-in (two round trips) only when the token is out, or nearly — never on an order's
    /// way while it is good (the readiness loop renews it ahead: renew_auth).
    pub async fn ensure_auth(&self) -> Res<()> {
        if self.usable() {
            return Ok(());
        }
        let _held = self.auth_lock.lock().await;
        if self.usable() {
            return Ok(());
        }
        self.sign_in().await.map(|_| ())
    }

    /// Signed in ahead: renewed once the token has less than AUTH_RENEW_S left (the readiness loop and the warm-up before a
    /// busy stretch — off an order's way).
    pub async fn renew_auth(&self) -> Res<()> {
        if self.fresh() {
            return Ok(());
        }
        let _held = self.auth_lock.lock().await;
        if self.fresh() {
            return Ok(());
        }
        self.sign_in().await.map(|_| ())
    }

    async fn sign_in(&self) -> Res<String> {
        let Some(wallet) = self.wallet.clone() else { return Err(RemoteError::new("没有签名钱包，无法登录 Predict")) };
        let result: Res<String> = async {
            let data = self.call("GET", "/auth/message", None, API_TIMEOUT, false, None).await?;
            let message = data.val("data").map(|d| d.s("message")).unwrap_or_default();
            if message.is_empty() {
                return Err(RemoteError::new(format!("登录消息格式异常（应答字段：{}）", answer_shape(&data))));
            }
            let body = json!({"signer": wallet.maker, "signature": wallet.sign_text(&message), "message": message});
            let answer = self
                .call("POST", "/auth", Some(Rc::new(serde_json::to_vec(&body).unwrap_or_default())), API_TIMEOUT, false, None)
                .await?;
            let token = answer.val("data").map(|d| d.s("token")).unwrap_or_default();
            if token.is_empty() {
                return Err(RemoteError::new(format!("登录未返回 token（应答字段：{}）", answer_shape(&answer))));
            }
            Ok(token)
        }
        .await;
        match result {
            Ok(token) => {
                self.jwt_exp.set(jwt_expiry(&token));
                *self.jwt.borrow_mut() = token.clone();
                self.jwt_at.set(crate::util::now_ms() as f64 / 1000.0);
                self.auth_error.borrow_mut().clear();
                Ok(token)
            }
            Err(error) => {
                *self.auth_error.borrow_mut() = clean_error(&error);
                Err(error)
            }
        }
    }

    // -- the endpoints --
    pub async fn market(&self, market_id: &str, priority: bool) -> Res<Obj> {
        let data = self.call("GET", &format!("/markets/{}", quote(market_id)), None, API_TIMEOUT, priority, None).await?;
        let market = data.get("data").cloned().unwrap_or(data);
        match market {
            Value::Object(m) if !m.is_empty() => Ok(m),
            _ => Err(RemoteError::new("Predict 市场详情格式异常")),
        }
    }

    pub async fn orderbook(&self, market_id: &str) -> Res<(Vec<Level>, Vec<Level>)> {
        let answer =
            self.call("GET", &format!("/markets/{}/orderbook", quote(market_id)), None, API_TIMEOUT, false, None).await?;
        let data = answer.get("data").cloned().unwrap_or(answer);
        let Some(d) = data.as_object().filter(|d| d.contains_key("bids") || d.contains_key("asks")) else {
            return Err(RemoteError::new("订单簿格式异常"));
        };
        Ok((
            predict_levels(d.get("bids").unwrap_or(&Value::Null), true),
            predict_levels(d.get("asks").unwrap_or(&Value::Null), false),
        ))
    }

    /// A rule order's last check: before_send of its hash (None while no taker has put its before_send here).
    pub fn order_gate(&self, digest: &str) -> Option<Gate> {
        let hook = self.before_send.borrow().clone()?;
        let digest = digest.to_string();
        Some(Rc::new(move || hook(&digest)))
    }

    /// POST /orders -> the order's id and hash; ``body`` the request's bytes made beforehand, ``gate`` its last check (a rule
    /// order's before_send, the order test's own). A 2xx answer without an order id is no refusal: the order may be there.
    pub async fn create_order(&self, body: Rc<Vec<u8>>, gate: Option<Gate>) -> Res<Created> {
        let trace = Rc::new(crate::http::Trace::default());
        let answer = self.authed_traced("POST", "/orders", Some(body), ORDER_TIMEOUT, true, gate, Some(trace.clone())).await?;
        let shown = || brief_error(&serde_json::to_string(&answer).unwrap_or_default(), 120);
        let Some(data) = answer.get("data").and_then(Value::as_object) else {
            return Err(RemoteError::new(format!("下单应答没有可读的内容（订单可能已被接受，按哈希核对）：{}", shown())));
        };
        let order_id = [data.get("orderId"), data.get("id"), data.get("order_id")]
            .into_iter()
            .flatten()
            .find(|v| truthy(v))
            .map(py_str)
            .unwrap_or_default();
        if order_id.is_empty() {
            return Err(RemoteError::new(format!("下单应答没有订单号：{}", shown())));
        }
        let hash =
            [data.get("orderHash"), data.get("hash")].into_iter().flatten().find(|v| truthy(v)).map(py_str).unwrap_or_default();
        Ok(Created {
            order_id,
            hash,
            queued_ms: trace.queued_ms.get(),
            connect_ms: trace.connect_ms.get(),
            wrote_wall: trace.wrote_wall.get(),
        })
    }

    pub async fn remove_orders(&self, ids: &[String]) -> Res<(Vec<String>, Vec<String>)> {
        let body = json!({"data": {"ids": ids}});
        let answer = self
            .authed(
                "POST",
                "/orders/remove",
                Some(Rc::new(serde_json::to_vec(&body).unwrap_or_default())),
                API_TIMEOUT,
                true,
                None,
            )
            .await?;
        *self.last_remove.borrow_mut() = answer.to_string().chars().take(200).collect();
        let data = answer.get("data").filter(|d| d.is_object()).cloned().unwrap_or(answer);
        let list = |k: &str| data.arr(k).iter().map(py_str).collect::<Vec<_>>();
        Ok((list("removed"), list("noop")))
    }

    /// One order, open or closed, by its hash (GET /orders/{hash}); None when Predict knows nothing of it (404). An answer
    /// with no order in it (an empty body, ``{}``) is None only for an order Predict never gave an id to: one it did is
    /// there, and such an answer says nothing of it — an error, read again later (its fills are never closed on it).
    pub async fn order(&self, order_id: &str, hash: &str) -> Res<Option<Obj>> {
        if hash.is_empty() {
            return Ok(None);
        }
        let data = match self.authed("GET", &format!("/orders/{}", quote(hash)), None, API_TIMEOUT, false, None).await {
            Ok(d) => d,
            Err(e) if e.status() == 404 => return Ok(None),
            Err(e) => return Err(e),
        };
        let mut row = data.get("data").cloned().unwrap_or(data);
        if let Value::Array(rows) = &row {
            let want = hash.to_lowercase();
            row = rows
                .iter()
                .find(|r| {
                    r.is_object()
                        && (order_hash_of(r) == want
                            || (!order_id.is_empty() && py_str(r.get("id").unwrap_or(&Value::Null)) == order_id))
                })
                .cloned()
                .unwrap_or(Value::Null);
        }
        Ok(match row {
            Value::Object(o) if !o.none("id") || o.t("order") || o.t("status") => Some(o),
            _ if !order_id.is_empty() => {
                return Err(RemoteError::new(format!("查单应答里没有订单 #{order_id} 的内容（不是 404）：过一会儿再查")));
            }
            _ => None,
        })
    }

    /// A window's market by its category (GET /categories/<slug>), then its details unless the category's own market object
    /// carries every field an order needs; the slug put on it; None when the category is not listed (404).
    pub async fn category_market(&self, slug: &str) -> Res<Option<Obj>> {
        let answer = match self.call("GET", &format!("/categories/{}", quote(slug)), None, API_TIMEOUT, false, None).await {
            Ok(a) => a,
            Err(e) if e.msg.contains("HTTP 404") => return Ok(None),
            Err(e) => return Err(e),
        };
        let found = predict_markets(&answer);
        let Some(first) = found.first() else { return Ok(None) };
        let first_id = first.s("id");
        if let Some(node) = category_node(&answer) {
            if py_str(node.get("id").unwrap_or(&Value::Null)) == first_id && listing_complete(node) {
                let mut out = node.clone();
                out.insert("categorySlug".into(), json!(slug));
                out.insert("id".into(), json!(first_id));
                out.insert("_from".into(), json!("category"));
                return Ok(Some(out));
            }
        }
        let mut market = self.market(&first_id, false).await?;
        let id = if market.t("id") { py_str(&market["id"]) } else { first_id };
        market.insert("categorySlug".into(), json!(slug));
        market.insert("id".into(), json!(id));
        market.insert("_from".into(), json!("details"));
        Ok(Some(market))
    }
}

pub fn quote(text: &str) -> String {
    let mut out = String::new();
    for b in text.bytes() {
        if b.is_ascii_alphanumeric() || b"-._~/".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

// --- what the answers say -------------------------------------------------------------------------------------------------
/// ``D(str(v))`` of a JSON value: a number by its Python repr, a string as written.
pub fn dec_of(v: &Value) -> Option<Decimal> {
    let text = match v {
        Value::Number(n) => {
            if n.is_f64() {
                crate::util::py_float(n.as_f64()?)
            } else {
                n.to_string()
            }
        }
        Value::String(s) => s.trim().to_string(),
        Value::Bool(b) => (if *b { "1" } else { "0" }).to_string(),
        _ => return None,
    };
    Decimal::from_str(&text).ok().or_else(|| Decimal::from_scientific(&text).ok())
}

/// [[price, size], ...] or [{price, size}, ...] -> sorted (price, size), empty levels dropped, PREDICT_DEPTH a side.
pub fn predict_levels(rows: &Value, bids: bool) -> Vec<Level> {
    let mut out: Vec<Level> = vec![];
    for row in rows.as_array().map(Vec::as_slice).unwrap_or(&[]) {
        let (price, size) = match row {
            Value::Array(a) if a.len() >= 2 => (dec_of(&a[0]), dec_of(&a[1])),
            Value::Object(o) => (o.get("price").and_then(dec_of), o.get("size").or_else(|| o.get("quantity")).and_then(dec_of)),
            _ => continue,
        };
        let (Some(price), Some(size)) = (price, size) else { continue };
        if price > Decimal::ZERO && price < Decimal::ONE && size > Decimal::ZERO {
            out.push((price, size));
        }
    }
    if bids {
        out.sort_by(|a, b| b.0.cmp(&a.0));
    } else {
        out.sort_by(|a, b| a.0.cmp(&b.0));
    }
    out.truncate(PREDICT_DEPTH);
    out
}

pub fn predict_markets(data: &Value) -> Vec<Obj> {
    fn walk(node: &Value, depth: usize, found: &mut Vec<Obj>) {
        if depth > 8 {
            return;
        }
        match node {
            Value::Object(o) => {
                if !o.none("id") && (o.has("conditionId") || o.has("question")) && o.has("title") {
                    let title = if o.t("title") { o.s("title") } else { o.s("question") };
                    found.push(crate::obj! {
                        "id" => py_str(&o["id"]), "conditionId" => o.s("conditionId"), "title" => title, "question" => o.s("question")
                    });
                    return;
                }
                for v in o.values() {
                    walk(v, depth + 1, found);
                }
            }
            Value::Array(a) => {
                for v in a {
                    walk(v, depth + 1, found);
                }
            }
            _ => {}
        }
    }
    let mut found = vec![];
    walk(data, 0, &mut found);
    // the last of each id, in the order the first of each was found (a dict comprehension's way)
    let mut out: Vec<Obj> = vec![];
    for m in found {
        let id = m.s("id");
        if let Some(slot) = out.iter_mut().find(|x| x.s("id") == id) {
            *slot = m;
        } else {
            out.push(m);
        }
    }
    out
}

pub fn category_node(answer: &Value) -> Option<&Obj> {
    fn walk(node: &Value, depth: usize) -> Option<&Obj> {
        if depth > 8 {
            return None;
        }
        match node {
            Value::Object(o) => {
                if !o.none("id") && o.has("conditionId") && o.get("outcomes").map(Value::is_array).unwrap_or(false) {
                    return Some(o);
                }
                o.values().find_map(|v| walk(v, depth + 1))
            }
            Value::Array(a) => a.iter().find_map(|v| walk(v, depth + 1)),
            _ => None,
        }
    }
    walk(answer, 0)
}

pub fn listing_complete(node: &Obj) -> bool {
    let outcomes = node.arr("outcomes");
    LISTING_FULL_FIELDS.iter().all(|k| node.has(k))
        && node.get("outcomes").map(Value::is_array).unwrap_or(false)
        && outcomes.len() >= 2
        && outcomes.iter().all(|o| o.is_object() && o.t("onChainId") && !o.none("indexSet"))
}

/// The target price (price to beat) a market's details may carry, under whichever name; None when none.
pub fn market_start_price(market: &Obj) -> Option<Decimal> {
    let mut rows: Vec<&Obj> = vec![market];
    for k in ["marketData", "metadata"] {
        if let Some(inner) = market.obj(k) {
            rows.push(inner);
        }
    }
    for row in rows {
        for name in PRICE_FIELDS {
            let Some(value) = row.get(name) else { continue };
            if !truthy(value) {
                continue;
            }
            if let Some(found) = dec_of(value) {
                if found > Decimal::ZERO {
                    return Some(found);
                }
            }
        }
    }
    None
}

fn sorted_outcomes(market: &Obj) -> Vec<&Obj> {
    let mut rows: Vec<&Obj> = market.arr("outcomes").iter().filter_map(Value::as_object).collect();
    rows.sort_by_key(|o| o.i("indexSet"));
    rows
}

#[derive(Clone, Debug, PartialEq)]
pub struct Resolution {
    pub index: Option<usize>,
    pub name: String,
    pub split: bool,
    pub how: &'static str,
}

/// What a market object says it settled on; None while open or unreadable.
pub fn predict_resolution(market: &Obj) -> Option<Resolution> {
    let rows = sorted_outcomes(market);
    let names: Vec<String> = rows.iter().map(|o| o.s("name")).collect();
    let won: Vec<usize> = rows
        .iter()
        .enumerate()
        .filter(|(_, o)| matches!(o.s("status").trim().to_uppercase().as_str(), "WON" | "WIN" | "WINNER"))
        .map(|(i, _)| i)
        .collect();
    if won.len() == 1 {
        return Some(Resolution { index: Some(won[0]), name: names[won[0]].clone(), split: false, how: "outcomes.status" });
    }
    if won.len() == rows.len() && rows.len() == 2 {
        return Some(Resolution { index: None, name: "50/50".into(), split: true, how: "outcomes.status" });
    }
    let mut pays = vec![];
    for o in &rows {
        let value = ["payout", "payoutNumerator", "payoutPerShare"].iter().find_map(|k| match o.get(*k) {
            None | Some(Value::Null) => None,
            Some(Value::String(s)) if s.is_empty() => None,
            Some(v) => Some(v),
        });
        if let Some(v) = value {
            let parsed = match v {
                Value::Number(n) => n.as_f64(),
                Value::String(s) => s.trim().parse::<f64>().ok(),
                Value::Bool(b) => Some(*b as i64 as f64),
                _ => None,
            };
            if let Some(p) = parsed {
                pays.push(p);
            }
        }
    }
    if !rows.is_empty() && pays.len() == rows.len() && pays.iter().sum::<f64>() > 0.0 {
        let top = pays.iter().copied().fold(f64::NEG_INFINITY, f64::max);
        if pays.iter().all(|p| (p - top).abs() < 1e-9) {
            return Some(Resolution { index: None, name: "50/50".into(), split: true, how: "outcomes.payout" });
        }
        if pays.iter().filter(|p| **p > 0.0).count() == 1 {
            let i = pays.iter().position(|p| *p == top).unwrap_or(0);
            return Some(Resolution { index: Some(i), name: names[i].clone(), split: false, how: "outcomes.payout" });
        }
    }
    let status = market.s("status").to_uppercase();
    if !["RESOLV", "SETTL", "FINAL"].iter().any(|w| status.contains(w)) {
        return None;
    }
    let res =
        ["resolution", "resolvedOutcome", "winningOutcome", "winner"].iter().find_map(|k| market.get(*k).filter(|v| truthy(v)));
    let name = match res {
        Some(Value::Object(r)) => {
            if r.t("name") {
                r.s("name")
            } else {
                r.s("outcome")
            }
        }
        Some(Value::String(s)) => s.clone(),
        _ => String::new(),
    };
    let mut index = None;
    if let Some(Value::Object(r)) = res {
        let set = r.s("indexSet");
        if !set.is_empty() && set.bytes().all(|b| b.is_ascii_digit()) {
            index = rows.iter().position(|o| py_str(o.get("indexSet").unwrap_or(&Value::Null)) == set);
        }
    }
    if index.is_none() && !name.is_empty() {
        index = names.iter().position(|n| n.trim().to_lowercase() == name.trim().to_lowercase());
    }
    let index = index?;
    Some(Resolution { index: Some(index), name: names[index].clone(), split: false, how: "resolution" })
}

/// A market's settings that hold back the orders that take (a speed bump: whatever field names a delay or a speed bump,
/// at any depth), as "speedBump.delayMs=500" — the race is another one where Predict delays the takers.
pub fn delay_fields(market: &Obj) -> Vec<String> {
    fn walk(prefix: &str, value: &Value, depth: usize, named: bool, out: &mut Vec<String>) {
        match value {
            Value::Object(o) if depth < 4 => {
                for (k, v) in o {
                    let path = if prefix.is_empty() { k.clone() } else { format!("{prefix}.{k}") };
                    let low = k.to_ascii_lowercase();
                    walk(&path, v, depth + 1, named || low.contains("delay") || low.contains("speedbump"), out);
                }
            }
            Value::Null | Value::Object(_) | Value::Array(_) => {}
            v if named => out.push(format!("{prefix}={}", py_str(v).chars().take(40).collect::<String>())),
            _ => {}
        }
    }
    let mut out = vec![];
    walk("", &Value::Object(market.clone()), 0, false, &mut out);
    out
}

/// The indexSet of the outcome a market settled on, numbered as parse_market numbers its outcomes; None for a split.
pub fn won_index_set(market: &Obj, res: &Resolution) -> Option<i64> {
    let i = res.index?;
    let row = *sorted_outcomes(market).get(i)?;
    Some(if row.t("indexSet") { row.i("indexSet") } else { i as i64 + 1 })
}

#[derive(Clone, Debug, PartialEq)]
pub struct Outcome {
    pub name: String,
    pub index_set: i64,
    pub token: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct MarketInfo {
    pub market_id: String,
    pub outcomes: Vec<Outcome>,
    pub fee_bps: i64,
    pub neg_risk: bool,
    pub yield_bearing: bool,
    pub condition_id: String,
    pub status: String,
}

pub fn parse_market(market: &Obj, default_fee: i64) -> MarketInfo {
    let outcomes = sorted_outcomes(market)
        .iter()
        .enumerate()
        .map(|(i, o)| Outcome {
            name: o.s("name"),
            index_set: if o.t("indexSet") { o.i("indexSet") } else { i as i64 + 1 },
            token: ["onChainId", "tokenId", "token_id"].iter().find(|k| o.t(k)).map(|k| o.s(k)).unwrap_or_default(),
        })
        .collect();
    let fee = market.get("feeRateBps").and_then(|v| match v {
        Value::Number(n) => n.as_i64().or_else(|| n.as_f64().map(|x| x as i64)),
        Value::String(s) => s.trim().parse::<i64>().ok(),
        Value::Bool(b) => Some(*b as i64),
        _ => None,
    });
    let fee = fee.filter(|f| (0..=10_000).contains(f));
    MarketInfo {
        market_id: market.s("id"),
        outcomes,
        fee_bps: fee.unwrap_or(default_fee),
        neg_risk: market.t("isNegRisk"),
        yield_bearing: market.t("isYieldBearing"),
        condition_id: market.s("conditionId"),
        status: market.s("status"),
    }
}

/// The outcome an up/down window's side buys, and a note when it was assumed from the order of the outcomes rather than read
/// from their names.
pub fn outcome_for_side(side: &str, outcomes: &[Outcome]) -> (Option<Outcome>, String) {
    if outcomes.len() != 2 || !outcomes.iter().all(|o| !o.token.is_empty()) {
        return (None, "市场不是两个结果，或缺少结果代币编号".into());
    }
    let names: Vec<String> = outcomes.iter().map(|o| o.name.trim().to_lowercase()).collect();
    let ups: Vec<usize> = (0..2).filter(|i| UP_WORDS.contains(&names[*i].as_str())).collect();
    let downs: Vec<usize> = (0..2).filter(|i| DOWN_WORDS.contains(&names[*i].as_str())).collect();
    let hit = if side == "up" {
        if ups.len() == 1 {
            Some(ups[0])
        } else if downs.len() == 1 {
            Some(1 - downs[0])
        } else {
            None
        }
    } else if downs.len() == 1 {
        Some(downs[0])
    } else if ups.len() == 1 {
        Some(1 - ups[0])
    } else {
        None
    };
    if let Some(i) = hit {
        return (Some(outcomes[i].clone()), String::new());
    }
    (Some(outcomes[if side == "up" { 0 } else { 1 }].clone()), format!("按结果顺序推定（结果名称：{}）", names.join("、")))
}

/// Whether the market lists its Up outcome first (its book is then quoted for Up, as offered() reads it).
pub fn book_up_first(market: &Obj) -> bool {
    let rows = sorted_outcomes(market);
    let first = rows.first().map(|o| o.s("name").trim().to_lowercase()).unwrap_or_default();
    !DOWN_WORDS.contains(&first.as_str())
}

pub fn order_hash_of(row: &Value) -> String {
    let inner = row.get("order").filter(|o| o.is_object());
    let pick = inner
        .and_then(|o| o.get("hash"))
        .filter(|v| truthy(v))
        .or_else(|| row.get("hash").filter(|v| truthy(v)))
        .or_else(|| row.get("orderHash").filter(|v| truthy(v)));
    pick.map(py_str).unwrap_or_default().to_lowercase()
}

pub const FILLED_STATUSES: [&str; 4] = ["FILLED", "MATCHED", "COMPLETED", "DONE"];
pub const FINAL_STATUSES: [&str; 14] = [
    "FILLED",
    "CANCELLED",
    "CANCELED",
    "EXPIRED",
    "INVALIDATED",
    "REJECTED",
    "FAILED",
    "MATCHED",
    "CLOSED",
    "COMPLETED",
    "DONE",
    "SETTLED",
    "RESOLVED",
    "UNKNOWN",
];
pub const OPEN_STATUSES: [&str; 9] =
    ["OPEN", "PENDING", "ACTIVE", "LIVE", "NEW", "PARTIAL", "PARTIALLY_FILLED", "PARTIALLY FILLED", "RESTING"];

fn is_final_status(s: &str) -> bool {
    FINAL_STATUSES.contains(&s)
}

fn big_int(v: Option<&Value>) -> Option<i128> {
    match v {
        None | Some(Value::Null) => Some(0),
        Some(v) if !truthy(v) => Some(0),
        Some(Value::Number(n)) => {
            n.as_i64().map(i128::from).or_else(|| n.as_u64().map(i128::from)).or_else(|| n.as_f64().map(|x| x as i128))
        }
        Some(Value::String(s)) => s.trim().parse::<i128>().ok(),
        Some(other) => Some(as_f64(other) as i128),
    }
}

/// (shares filled, status) of an order record: amountFilled / amount of the shares ordered, the status upper-cased.
pub fn order_fill(row: &Obj, ordered_shares: f64) -> (f64, String) {
    let status = row.s("status").to_uppercase();
    let got_value = if row.t("amountFilled") { row.get("amountFilled") } else { row.get("filled") };
    let (amount, mut got) = match (big_int(row.get("amount")), big_int(got_value)) {
        (Some(a), Some(g)) => (a as f64, g as f64),
        _ => {
            // int(float(...)) of what does not read as an integer
            let f = |v: Option<&Value>| -> Option<f64> {
                match v {
                    None | Some(Value::Null) => Some(0.0),
                    Some(v) if !truthy(v) => Some(0.0),
                    Some(Value::Number(n)) => n.as_f64(),
                    Some(Value::String(s)) => s.trim().parse::<f64>().ok(),
                    Some(other) => Some(as_f64(other)),
                }
            };
            match (f(row.get("amount")), f(row.get("amountFilled"))) {
                (Some(a), Some(g)) => (a.trunc(), g.trunc()),
                _ => return (0.0, status),
            }
        }
    };
    if amount <= 0.0 {
        return (if FILLED_STATUSES.contains(&status.as_str()) { ordered_shares } else { 0.0 }, status);
    }
    if got <= 0.0 && FILLED_STATUSES.contains(&status.as_str()) {
        got = amount;
    }
    (ordered_shares.min(ordered_shares * got / amount), status)
}

pub fn order_final(row: &Obj, ordered_shares: f64) -> bool {
    let status = row.s("status").to_uppercase();
    if OPEN_STATUSES.contains(&status.as_str()) {
        return false;
    }
    if is_final_status(&status) {
        return true;
    }
    let (got, _) = order_fill(row, ordered_shares);
    ordered_shares > 0.0 && got >= ordered_shares - 1e-9
}

pub fn order_price(row: &Obj) -> Option<f64> {
    for key in ["averagePrice", "avgPrice", "priceFilled", "filledPrice", "executedPrice", "averageFillPrice"] {
        let Some(value) = row.get(key) else { continue };
        match value {
            Value::Null => continue,
            Value::String(s) if s.is_empty() || s == "0" => continue,
            Value::Number(n) if n.as_f64() == Some(0.0) => continue,
            Value::Bool(false) => continue,
            _ => {}
        }
        let Some(mut price) = dec_of(value) else { continue };
        if price > Decimal::ONE {
            price /= Decimal::from_str("1000000000000000000").unwrap_or(Decimal::ONE);
        }
        if price > Decimal::ZERO && price <= Decimal::ONE {
            return price.to_string().parse().ok();
        }
    }
    None
}

// --- BNB chain (JSON-RPC): the balance, the allowance, the account's owner ---------------------------------------------------
pub struct Chain {
    pub rpc_url: String,
    pub wallet: Rc<Wallet>,
    pub error: RefCell<String>,
    pub transport: Rc<dyn Transport>,
}

pub fn selector(signature: &str) -> [u8; 4] {
    let h = keccak(signature.as_bytes());
    [h[0], h[1], h[2], h[3]]
}

/// A 32-byte big-endian word as u128, saturating (an allowance of 2^256 − 1 is u128::MAX).
pub fn word_u128(bytes: &[u8]) -> u128 {
    if bytes.is_empty() {
        return 0;
    }
    let word = &bytes[..bytes.len().min(32)];
    let (high, low) = if word.len() > 16 { word.split_at(word.len() - 16) } else { (&[][..], word) };
    if high.iter().any(|b| *b != 0) {
        return u128::MAX;
    }
    let mut buf = [0u8; 16];
    buf[16 - low.len()..].copy_from_slice(low);
    u128::from_be_bytes(buf)
}

impl Chain {
    pub fn new(rpc_url: &str, wallet: Rc<Wallet>, transport: Rc<dyn Transport>) -> Chain {
        Chain { rpc_url: rpc_url.to_string(), wallet, error: RefCell::new(String::new()), transport }
    }

    pub async fn rpc(&self, method: &str, params: Value) -> Res<Value> {
        let body = json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params});
        let req = Req {
            url: self.rpc_url.clone(),
            payload: Some(Rc::new(serde_json::to_vec(&body).unwrap_or_default())),
            timeout: RPC_TIMEOUT,
            headers: vec![],
            gate: None,
            trace: None,
        };
        let raw = match self.transport.request(req).await {
            Ok(r) => r,
            Err(e) => {
                *self.error.borrow_mut() = clean_error(&e);
                return Err(e);
            }
        };
        let data: Value = match serde_json::from_slice(&raw) {
            Ok(d) => d,
            Err(_) => {
                *self.error.borrow_mut() = "RPC 未返回有效 JSON".into();
                return Err(RemoteError::new("RPC 未返回有效 JSON"));
            }
        };
        if let Some(err) = data.get("error").filter(|e| truthy(e)) {
            let said = if let Some(m) = err.get("message") { py_str(m) } else { py_str(err) };
            let text = format!("RPC 错误：{}", brief_error(&said, 120));
            *self.error.borrow_mut() = text.clone();
            return Err(RemoteError::new(text));
        }
        self.error.borrow_mut().clear();
        Ok(data.get("result").cloned().unwrap_or(Value::Null))
    }

    fn hex_bytes(result: &Value) -> Vec<u8> {
        let text = match result {
            Value::String(s) => s.clone(),
            _ => "0x".into(),
        };
        hex::decode(text.trim_start_matches("0x")).unwrap_or_default()
    }

    pub async fn call_uint(&self, to: &str, signature: &str, addresses: &[&str]) -> Res<u128> {
        let mut data = selector(signature).to_vec();
        for a in addresses {
            data.extend_from_slice(&crate::wallet::address_word(a).map_err(RemoteError::new)?);
        }
        let result = self.rpc("eth_call", json!([{"to": to, "data": format!("0x{}", hex::encode(&data))}, "latest"])).await?;
        Ok(word_u128(&Self::hex_bytes(&result)))
    }

    pub async fn usdt_balance(&self) -> Res<u128> {
        let usdt = self.wallet.address("USDT");
        self.call_uint(usdt, "balanceOf(address)", &[&self.wallet.maker]).await
    }

    pub async fn bnb_balance(&self) -> Res<u128> {
        let result = self.rpc("eth_getBalance", json!([self.wallet.signer, "latest"])).await?;
        let text = match &result {
            Value::String(s) => s.clone(),
            _ => "0x0".into(),
        };
        let text = text.trim_start_matches("0x");
        Ok(u128::from_str_radix(if text.is_empty() { "0" } else { text }, 16).unwrap_or(u128::MAX))
    }

    pub async fn allowance(&self, spender_key: &str) -> Res<u128> {
        let usdt = self.wallet.address("USDT");
        let spender = self.wallet.address(spender_key);
        self.call_uint(usdt, "allowance(address,address)", &[&self.wallet.maker, spender]).await
    }

    /// The signer the ECDSA validator holds for a Predict account (0x000… when the account is unknown to it).
    pub async fn account_owner(&self, account: &str) -> Res<String> {
        let mut data = selector("ecdsaValidatorStorage(address)").to_vec();
        data.extend_from_slice(&crate::wallet::address_word(account).map_err(RemoteError::new)?);
        let to = self.wallet.address("ECDSA_VALIDATOR");
        let result = self.rpc("eth_call", json!([{"to": to, "data": format!("0x{}", hex::encode(&data))}, "latest"])).await?;
        let out = Self::hex_bytes(&result);
        Ok(if out.len() >= 32 { format!("0x{}", hex::encode(&out[12..32])) } else { format!("0x{}", "0".repeat(40)) })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn levels_and_parsers() {
        let rows = json!([[0.42, 100], ["0.40", "50.5"], [1.2, 3], [0.3, 0], {"price": 0.45, "size": 7}]);
        let bids = predict_levels(&rows, true);
        assert_eq!(bids.iter().map(|l| l.0.to_string()).collect::<Vec<_>>(), vec!["0.45", "0.42", "0.40"]);
        let m: Obj = serde_json::from_value(json!({"id": 7, "status": "RESOLVED", "outcomes": [
            {"name": "Up", "indexSet": 1, "onChainId": "11", "status": "LOST"}, {"name": "Down", "indexSet": 2, "onChainId": "22", "status": "WON"}]}))
        .unwrap();
        assert_eq!(predict_resolution(&m).unwrap().index, Some(1));
        let info = parse_market(&m, 200);
        assert_eq!(info.market_id, "7");
        assert_eq!(info.fee_bps, 200);
        let (o, note) = outcome_for_side("down", &info.outcomes);
        assert_eq!((o.unwrap().token.as_str(), note.as_str()), ("22", ""));
        let row: Obj = serde_json::from_value(
            json!({"status": "open", "amount": "50000000000000000000", "amountFilled": "25000000000000000000"}),
        )
        .unwrap();
        assert_eq!(order_fill(&row, 50.0), (25.0, "OPEN".to_string()));
        assert!(!order_final(&row, 50.0));
        let row: Obj = serde_json::from_value(json!({"status": "FILLED", "amount": "0"})).unwrap();
        assert_eq!(order_fill(&row, 50.0).0, 50.0);
        let row: Obj = serde_json::from_value(json!({"averagePrice": "580000000000000000"})).unwrap();
        assert_eq!(order_price(&row), Some(0.58));
        assert_eq!(word_u128(&[0xff; 32]), u128::MAX);
        assert_eq!(answer_shape(&json!({"success": true, "data": {"b": 1, "a": 2}})), "data,success,data{a,b}");
        let jwt = format!("x.{}.y", base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(br#"{"exp":1790000000}"#));
        assert_eq!(jwt_expiry(&jwt), 1_790_000_000);
    }
}
