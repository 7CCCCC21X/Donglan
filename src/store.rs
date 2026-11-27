//! The records' store: SQLite (a key -> JSON value table, the Python version's own: either can run on the other's file), every
//! key read into memory at the start, written behind by a thread of its own.
//!
//! ``put`` serialises the value at once and queues the write; the writer commits in batches with synchronous=FULL. ``flush``
//! waits until everything queued so far is on disk. A real order's hash reaches the disk before its request leaves through the
//! taker's own journal file, not through this store.
use crate::util::now_ms;
use indexmap::IndexMap;
use serde_json::Value;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

enum Item {
    Put(String, String),
    Delete(Vec<String>),
}

#[derive(Default)]
struct Shared {
    written: Mutex<u64>,
    cond: Condvar,
    error: Mutex<String>,
}

pub struct Store {
    pub path: String,
    /// every key's value as last put (the rule records are taken out at the start: the taker holds them)
    data: IndexMap<String, Value>,
    tx: Option<Sender<Item>>,
    queued: AtomicU64,
    shared: Arc<Shared>,
}

fn connect(path: &str) -> rusqlite::Result<rusqlite::Connection> {
    let conn = rusqlite::Connection::open(path)?;
    conn.busy_timeout(Duration::from_secs(30))?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "synchronous", "FULL")?;
    conn.execute("CREATE TABLE IF NOT EXISTS kv (key TEXT PRIMARY KEY, value TEXT NOT NULL, updated REAL NOT NULL)", [])?;
    Ok(conn)
}

impl Store {
    pub fn open(path: &str) -> Result<Store, String> {
        let shared = Arc::new(Shared::default());
        let mut store = Store { path: path.to_string(), data: IndexMap::new(), tx: None, queued: AtomicU64::new(0), shared };
        if path == ":memory:" {
            return Ok(store);
        }
        if let Some(dir) = std::path::Path::new(path).parent() {
            if !dir.as_os_str().is_empty() {
                std::fs::create_dir_all(dir).map_err(|e| format!("数据库目录建不了：{e}"))?;
            }
        }
        let conn = connect(path).map_err(|e| format!("数据库打不开：{e}"))?;
        {
            let mut stmt = conn.prepare("SELECT key, value FROM kv").map_err(|e| format!("数据库读不了：{e}"))?;
            let rows = stmt
                .query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)))
                .map_err(|e| format!("数据库读不了：{e}"))?;
            for row in rows.flatten() {
                match serde_json::from_str::<Value>(&row.1) {
                    Ok(v) => {
                        store.data.insert(row.0, v);
                    }
                    Err(_) => crate::warn!("数据库里 {} 这一条读不出来，略过", row.0),
                }
            }
        }
        drop(conn);
        let (tx, rx) = channel();
        let shared = store.shared.clone();
        let file = path.to_string();
        std::thread::Builder::new()
            .name("store-writer".into())
            .spawn(move || write_loop(&file, rx, shared))
            .map_err(|e| format!("数据库写入线程起不来：{e}"))?;
        store.tx = Some(tx);
        Ok(store)
    }

    pub fn memory() -> Store {
        Store::open(":memory:").expect("an in-memory store always opens")
    }

    pub fn persistent(&self) -> bool {
        self.tx.is_some()
    }

    /// The tests' stand-in for a disk that refuses the writes.
    pub fn fail_for_test(&self, text: &str) {
        if let Ok(mut e) = self.shared.error.lock() {
            *e = text.to_string();
        }
    }

    /// The write that failed last ("" while the writes go through).
    pub fn error(&self) -> String {
        self.shared.error.lock().map(|e| e.clone()).unwrap_or_default()
    }

    pub fn get(&self, key: &str) -> Option<Value> {
        self.data.get(key).cloned()
    }

    pub fn get_str(&self, key: &str) -> String {
        match self.data.get(key) {
            Some(Value::String(s)) => s.clone(),
            Some(Value::Null) | None => String::new(),
            Some(other) => other.to_string(),
        }
    }

    /// The keys under ``prefix`` and their values, as kept in memory.
    pub fn items<'a>(&'a self, prefix: &'a str) -> impl Iterator<Item = (&'a String, &'a Value)> + 'a {
        self.data.iter().filter(move |(k, _)| k.starts_with(prefix))
    }

    /// The keys under ``prefix`` and their values, taken out of memory (the caller keeps them from now on).
    pub fn take_prefix(&mut self, prefix: &str) -> Vec<(String, Value)> {
        let keys: Vec<String> = self.data.keys().filter(|k| k.starts_with(prefix)).cloned().collect();
        keys.into_iter().filter_map(|k| self.data.shift_remove(&k).map(|v| (k, v))).collect()
    }

    /// The value written behind; kept in memory unless it is a rule record (the taker holds those).
    pub fn put(&mut self, key: &str, value: &Value) {
        let text = serde_json::to_string(value).unwrap_or_else(|_| "null".into());
        if !key.starts_with(crate::rules::PREFIX) {
            self.data.insert(key.to_string(), value.clone());
        }
        self.enqueue(Item::Put(key.to_string(), text));
    }

    /// A rule record written behind, serialised from the taker's own copy (nothing kept here: the taker holds it).
    pub fn put_record(&mut self, key: &str, rec: &serde_json::Map<String, Value>) {
        let text = serde_json::to_string(rec).unwrap_or_else(|_| "null".into());
        self.enqueue(Item::Put(key.to_string(), text));
    }

    pub fn delete_keys(&mut self, keys: &[String]) {
        for key in keys {
            self.data.shift_remove(key);
        }
        if !keys.is_empty() {
            self.enqueue(Item::Delete(keys.to_vec()));
        }
    }

    fn enqueue(&self, item: Item) {
        if let Some(tx) = &self.tx {
            self.queued.fetch_add(1, Ordering::SeqCst);
            if tx.send(item).is_err() {
                if let Ok(mut e) = self.shared.error.lock() {
                    *e = "数据库写入线程已停止".into();
                }
            }
        }
    }

    /// A waiter for everything queued so far: blocks (on a thread of its own) until written; false past ``timeout``.
    pub fn flush_waiter(&self, timeout: f64) -> impl FnOnce() -> bool + Send + 'static {
        let target = self.queued.load(Ordering::SeqCst);
        let shared = self.shared.clone();
        let persistent = self.tx.is_some();
        move || {
            if !persistent {
                return true;
            }
            let Ok(guard) = shared.written.lock() else { return false };
            let (guard, _) = shared
                .cond
                .wait_timeout_while(guard, Duration::from_secs_f64(timeout.max(0.0)), |w| *w < target)
                .unwrap_or_else(|e| e.into_inner());
            *guard >= target
        }
    }

    /// Wait (blocking) until everything queued so far is written.
    pub fn flush(&self, timeout: f64) -> bool {
        (self.flush_waiter(timeout))()
    }
}

/// ``flush`` without holding up the loop (the wait on a blocking thread).
pub async fn flush_async(waiter: impl FnOnce() -> bool + Send + 'static) -> bool {
    tokio::task::spawn_blocking(waiter).await.unwrap_or(false)
}

fn write_loop(path: &str, rx: Receiver<Item>, shared: Arc<Shared>) {
    let mut conn = loop {
        match connect(path) {
            Ok(c) => break c,
            Err(e) => {
                if let Ok(mut err) = shared.error.lock() {
                    *err = format!("数据库写入失败：{e}");
                }
                std::thread::sleep(Duration::from_secs(2));
            }
        }
    };
    while let Ok(first) = rx.recv() {
        let mut batch = vec![first];
        while batch.len() < 500 {
            match rx.try_recv() {
                Ok(item) => batch.push(item),
                Err(_) => break,
            }
        }
        // A batch is written or tried again — never counted as written while it is not: flush() says false until it is (a full
        // disk: the taker then sends no order, as nothing could find it after a restart), and error says why meanwhile
        let mut attempt = 0u32;
        loop {
            let result = (|| -> rusqlite::Result<()> {
                let tx = conn.transaction()?;
                {
                    let mut put = tx.prepare_cached(
                        "INSERT INTO kv (key, value, updated) VALUES (?1, ?2, ?3) ON CONFLICT(key) DO UPDATE SET \
                         value = excluded.value, updated = excluded.updated",
                    )?;
                    let mut del = tx.prepare_cached("DELETE FROM kv WHERE key = ?1")?;
                    let stamp = now_ms() as f64 / 1000.0;
                    for item in &batch {
                        match item {
                            Item::Put(k, v) => {
                                put.execute(rusqlite::params![k, v, stamp])?;
                            }
                            Item::Delete(keys) => {
                                for k in keys {
                                    del.execute(rusqlite::params![k])?;
                                }
                            }
                        }
                    }
                }
                tx.commit()
            })();
            match result {
                Ok(()) => {
                    if let Ok(mut e) = shared.error.lock() {
                        e.clear();
                    }
                    break;
                }
                Err(e) => {
                    attempt += 1;
                    let text = format!("数据库写入失败：{e}");
                    crate::error!("{text}（第 {attempt} 次，稍后再试）");
                    if let Ok(mut err) = shared.error.lock() {
                        *err = text;
                    }
                    std::thread::sleep(Duration::from_secs_f64((0.5 * attempt as f64).min(30.0)));
                    if let Ok(c) = connect(path) {
                        conn = c;
                    }
                }
            }
        }
        if let Ok(mut written) = shared.written.lock() {
            *written += batch.len() as u64;
            shared.cond.notify_all();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn round_trip() {
        let dir = std::env::temp_dir().join(format!("ruletaker-store-{}", std::process::id()));
        let path = dir.join("t.sqlite3").to_string_lossy().to_string();
        {
            let mut s = Store::open(&path).unwrap();
            s.put("control:mode", &json!("paper"));
            s.put("rule:x", &json!({"slug": "x", "orders": []}));
            s.put("gone", &json!(1));
            s.delete_keys(&["gone".to_string()]);
            assert!(s.flush(10.0));
            assert_eq!(s.get_str("control:mode"), "paper");
            assert!(s.get("rule:x").is_none()); // (held by the taker, not here)
        }
        let mut again = Store::open(&path).unwrap();
        assert_eq!(again.get_str("control:mode"), "paper");
        assert!(again.get("gone").is_none());
        let recs = again.take_prefix("rule:");
        assert_eq!(recs.len(), 1);
        assert_eq!(recs[0].1["slug"], "x");
        let _ = std::fs::remove_dir_all(dir);
    }
}
