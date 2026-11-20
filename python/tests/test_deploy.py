"""The deploy tooling against the service: rt.sh's helper on the VM (deploy/gcp/vm-call.sh) reads STATUS_KEY and STATUS_PORT the
way the service reads the settings file, whatever the line looks like; and the two copies of the control page are one page."""
from __future__ import annotations

import http.server
import os
import shutil
import subprocess
import tempfile
import threading
import unittest

from ruletaker.config import load_env_file

ROOT = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
CALL = os.path.join(ROOT, "deploy", "gcp", "vm-call.sh")
TOOLS = all(shutil.which(t) for t in ("bash", "awk", "curl"))


class _Echo(http.server.BaseHTTPRequestHandler):
    """Answers every GET with the X-Status-Key it was sent."""

    def log_message(self, *args) -> None:
        pass

    def do_GET(self) -> None:
        body = (self.headers.get("X-Status-Key") or "").encode()
        self.send_response(200)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)


@unittest.skipUnless(TOOLS and os.path.isfile(CALL), "no bash / awk / curl, or no deploy folder")
class VmCallTest(unittest.TestCase):
    KEYS = [
        "STATUS_KEY=plain",
        "export STATUS_KEY=abc123 # 控制台密码",
        'STATUS_KEY="quoted value"',
        "STATUS_KEY='single # inside'",
        "  STATUS_KEY = spaced   ",
        "STATUS_KEY=a#b",
        'STATUS_KEY="unterminated',
        'STATUS_KEY="quoted"  # then a comment',
        "STATUS_KEY=first\nSTATUS_KEY=second",
        "#STATUS_KEY=commented\nSTATUS_KEY=real",
        'STATUS_KEY=a\\b"c d # x',
        "STATUS_KEY=x=y=z",
        "STATUS_KEY=\r",
    ]
    PORTS = ["STATUS_PORT='{p}'", "STATUS_PORT={p} # 端口", 'export STATUS_PORT="{p}"', "STATUS_PORT={p}\nSTATUS_PORT=1"]

    def setUp(self) -> None:
        self.server = http.server.HTTPServer(("127.0.0.1", 0), _Echo)
        self.port = self.server.server_address[1]
        threading.Thread(target=self.server.serve_forever, daemon=True).start()
        self.dir = tempfile.mkdtemp()

    def tearDown(self) -> None:
        self.server.shutdown()
        self.server.server_close()
        shutil.rmtree(self.dir, ignore_errors=True)

    def call(self, text: str, *words: str) -> tuple[str, dict]:
        path = os.path.join(self.dir, "ruletaker.env")
        with open(path, "w", encoding="utf-8") as f:
            f.write(text + "\n")
        env = dict(os.environ, RT_ENV=path)
        out = subprocess.run(["bash", CALL, *words], capture_output=True, text=True, env=env, stdin=subprocess.DEVNULL, timeout=30)
        return out.stdout.rstrip("\n"), load_env_file(path)

    def test_the_key_is_read_as_the_service_reads_it(self) -> None:
        for line in self.KEYS:
            with self.subTest(line=line):
                got, service = self.call(f"STATUS_PORT={self.port}\n{line}", "get", "/status")
                self.assertEqual(got, service.get("STATUS_KEY", "").strip())

    def test_the_port_is_read_as_the_service_reads_it(self) -> None:
        for line in self.PORTS:
            with self.subTest(line=line):
                got, service = self.call(line.format(p=self.port), "port")
                self.assertEqual(got, service["STATUS_PORT"])
        got, _ = self.call("STATUS_KEY=x", "port")
        self.assertEqual(got, "8080")

    def test_only_its_own_words(self) -> None:
        got, _ = self.call(f"STATUS_PORT={self.port}", "post", "/mode?to=on;id")
        self.assertIn("不认识的调用", got)
        got, _ = self.call(f"STATUS_PORT={self.port}", "control", "$(id)")
        self.assertIn("不认识的操作", got)


class ConsolePageTest(unittest.TestCase):
    def test_the_two_copies_of_the_page_are_one_page(self) -> None:
        here = os.path.join(ROOT, "python", "ruletaker", "web", "console.html")
        main = os.path.join(ROOT, "web", "console.html")
        if not os.path.isfile(main):
            self.skipTest("no Rust copy of the page here")
        with open(here, encoding="utf-8") as a, open(main, encoding="utf-8") as b:
            ours, theirs = a.read(), b.read()
        # one page for both versions (it tells them apart by the state's "runtime"): a change made to one copy alone fails here
        self.assertTrue(ours == theirs, "python/ruletaker/web/console.html 和 web/console.html 不一样：改一份后复制到另一份")


if __name__ == "__main__":
    unittest.main()
