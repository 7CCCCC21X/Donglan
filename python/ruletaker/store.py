"""The records' store: SQLite (a key -> JSON value table), every key kept in memory, written behind by a thread of its own.

``put`` serialises the value at once (the caller may change its dict afterwards) and queues the write; the writer commits in
batches with synchronous=FULL. ``flush`` waits until everything queued so far is on disk. A real order's hash reaches the
disk before its request leaves through the taker's own journal file (RuleTaker.journal), not through this store."""
from __future__ import annotations

import contextlib
import json
import os
import queue
import sqlite3
import threading
import time
from typing import Any, Iterator

from .util import LOG

try:
    import orjson as _orjson

    def _encode(value: Any) -> str:
        return _orjson.dumps(value, default=str).decode("utf-8")
except ImportError:  # pragma: no cover
    def _encode(value: Any) -> str:
        return json.dumps(value, ensure_ascii=False, separators=(",", ":"), default=str)


class Store:
    def __init__(self, path: str):
        self.path = path
        self.data: dict[str, Any] = {}
        self.writer: threading.Thread | None = None
        self.queue: queue.Queue = queue.Queue()
        self.queued = self.written = 0
        self.cond = threading.Condition()
        self.error = ""
        if path == ":memory:":
            return
        os.makedirs(os.path.dirname(os.path.abspath(path)) or ".", exist_ok=True)
        conn = self.connect()
        try:
            for key, value in conn.execute("SELECT key, value FROM kv"):
                try:
                    self.data[key] = json.loads(value)
                except ValueError:
                    LOG.warning("数据库里 %s 这一条读不出来，略过", key)
        finally:
            conn.close()
        self.writer = threading.Thread(target=self.write_loop, name="store-writer", daemon=True)
        self.writer.start()

    def connect(self) -> sqlite3.Connection:
        conn = sqlite3.connect(self.path, timeout=30)
        conn.execute("PRAGMA journal_mode=WAL")
        conn.execute("PRAGMA synchronous=FULL")
        conn.execute("CREATE TABLE IF NOT EXISTS kv (key TEXT PRIMARY KEY, value TEXT NOT NULL, updated REAL NOT NULL)")
        conn.commit()
        return conn

    # -- the reads (memory) --
    def get(self, key: str, default: Any = None) -> Any:
        value = self.data.get(key, default)
        return json.loads(_encode(value)) if isinstance(value, (dict, list)) else value  # (a copy: the caller may change it)

    def items(self, prefix: str = "") -> Iterator[tuple[str, Any]]:
        for key in [k for k in self.data if k.startswith(prefix)]:
            yield key, self.data[key]

    # -- the writes --
    def put(self, key: str, value: Any) -> None:
        text = _encode(value)  # (the writer gets the value as it is now; the dict itself may change after this)
        self.data[key] = value
        self.enqueue(("put", key, text))

    def delete_keys(self, keys: list[str]) -> None:
        for key in keys:
            self.data.pop(key, None)
        if keys:
            self.enqueue(("delete", list(keys), None))

    def enqueue(self, item: tuple) -> None:
        if self.writer is None:
            return
        with self.cond:
            self.queued += 1
        self.queue.put(item)

    def flush(self, timeout: float = 10.0) -> bool:
        """Wait until everything queued so far is written; False when that did not happen within ``timeout``."""
        if self.writer is None:
            return True
        with self.cond:
            target = self.queued
            return self.cond.wait_for(lambda: self.written >= target, timeout)

    # (the writer's first open tried again, as src/store.rs does: a lock held a moment, a disk slow to come up — never a writer
    # thread gone with nobody told; the error said meanwhile, so no order goes out on a store that cannot write)
    CONNECT_RETRY_S = 2.0

    def write_loop(self) -> None:
        conn = None
        tries = 0
        while conn is None:
            try:
                conn = self.connect()
            except (sqlite3.Error, OSError) as error:
                tries += 1
                self.error = f"数据库写入失败：{error}"
                LOG.error("%s（打开数据库第 %d 次，稍后再试）", self.error, tries)
                time.sleep(self.CONNECT_RETRY_S)
        while True:
            item = self.queue.get()
            batch = [item]
            while len(batch) < 500:
                try:
                    batch.append(self.queue.get_nowait())
                except queue.Empty:
                    break
            # A batch is written or tried again — never counted as written while it is not: flush() says False until it is (a
            # full disk: the taker then sends no order, as nothing could find it after a restart), and error says why meanwhile
            attempt = 0
            while True:
                try:
                    with conn:
                        for kind, key, text in batch:
                            if kind == "put":
                                conn.execute("INSERT INTO kv (key, value, updated) VALUES (?, ?, ?) ON CONFLICT(key) DO UPDATE SET "
                                             "value = excluded.value, updated = excluded.updated", (key, text, time.time()))
                            else:
                                conn.executemany("DELETE FROM kv WHERE key = ?", [(k,) for k in key])
                    self.error = ""
                    break
                except sqlite3.Error as error:
                    attempt += 1
                    self.error = f"数据库写入失败：{error}"
                    LOG.error("%s（第 %d 次，稍后再试）", self.error, attempt)
                    time.sleep(min(30.0, 0.5 * attempt))
                    with contextlib.suppress(sqlite3.Error):
                        conn.close()
                    with contextlib.suppress(sqlite3.Error):
                        conn = self.connect()
            with self.cond:
                self.written += len(batch)
                self.cond.notify_all()
