"""The deploy tooling against the service: rt.sh's helper on the VM (deploy/gcp/vm-call.sh) reads STATUS_KEY and STATUS_PORT the
way the service reads the settings file, whatever the line looks like; and the two copies of the control page are one page."""
from __future__ import annotations

import base64
import http.server
import os
import shutil
import stat
import subprocess
import sys
import tempfile
import threading
import unittest

from ruletaker.config import Config, Stored, load_env_file

ROOT = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
CALL = os.path.join(ROOT, "deploy", "gcp", "vm-call.sh")
KEYSH = os.path.join(ROOT, "deploy", "gcp", "vm-key.sh")
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


# Stand-ins for systemd's tools, in systemd's own file format as far as vm-key.sh looks at it (the first 16 bytes name the key it
# was sealed with): systemd-creds seals with a "host" or "host+tpm2" key (FAKE_NO_TPM: the TPM refuses), systemd-run opens a
# LoadCredentialEncrypted= the way the service manager does — into a directory of the command's own, $CREDENTIALS_DIRECTORY —
# (FAKE_OTHER: opens it to another key, a proof that fails), systemctl writes down what it was asked.
FAKE_CREDS = r"""#!PYTHON
import base64, sys, os
IDS = {"host": "5a1c6a86df9d4096b1d5a65e0862f19a", "host+tpm2": "93a894094874449090caf2fc93cab553"}
cmd, opts, args = sys.argv[1], {}, []
for a in sys.argv[2:]:
    if a.startswith("--") and "=" in a:
        k, _, v = a[2:].partition("=")
        opts[k] = v
    else:
        args.append(a)
if cmd == "encrypt":
    if opts["with-key"] == "host+tpm2" and os.environ.get("FAKE_NO_TPM"):
        sys.exit("Failed to create TPM2 context: Operation not supported")
    data = sys.stdin.buffer.read() if args[0] == "-" else open(args[0], "rb").read()
    blob = bytes.fromhex(IDS[opts["with-key"]]) + opts["name"].encode() + b"\0" + data
    with open(args[1], "w") as f:
        f.write(base64.b64encode(blob).decode())
elif cmd == "decrypt":
    blob = base64.b64decode(open(args[0]).read())
    name, _, data = blob[16:].partition(b"\0")
    if name.decode() != opts.get("name"):
        sys.exit("Embedded credential name does not match")
    sys.stdout.buffer.write(data)
"""
FAKE_RUN = r"""#!PYTHON
import os, subprocess, sys, tempfile
args, env, i = sys.argv[1:], dict(os.environ), 0
creds = tempfile.mkdtemp()
with open(os.environ["FAKE_LOG"], "a") as log:
    log.write("systemd-run " + " ".join(args) + "\n")
while i < len(args) and args[i].startswith("-"):
    if args[i] == "-p":
        prop = args[i + 1]
        i += 2
        if prop.startswith("LoadCredentialEncrypted="):
            ident, _, path = prop.split("=", 1)[1].partition(":")
            key = subprocess.run([os.environ["FAKE_CREDS"], "decrypt", "--name=" + ident, path, "-"], capture_output=True).stdout
            if os.environ.get("FAKE_OTHER"):
                key = b"0x" + b"7" * 64
            with open(os.path.join(creds, ident), "wb") as f:
                f.write(key)
            env["CREDENTIALS_DIRECTORY"] = creds
        elif prop.startswith("Environment="):
            k, _, v = prop.split("=", 1)[1].partition("=")
            env[k] = v
    else:
        i += 1
code = subprocess.run(args[i:], env=env, stdin=subprocess.DEVNULL).returncode
for f in os.listdir(creds):
    os.remove(os.path.join(creds, f))
os.rmdir(creds)
sys.exit(code)
"""
FAKE_SYSTEMCTL = """#!/usr/bin/env bash
echo "systemctl $*" >> "$FAKE_LOG"
[ "$1" = is-active ] && exit "${FAKE_DOWN:-0}"
exit 0
"""
A = "a4c123b1612dd272d1371c17149d439536b3216fdaeeb975729fae923d5a4fd1"
A_SIGNER = "0x511387682044334a3e855679c186455753356BF5"  # (tests/vectors/wallet.json)
B = "4c" * 32
OLD = "9d" * 32
MARK = "# PREDICT_PRIVATE_KEY：已加密保存"
try:
    import eth_account  # noqa: F401
    ETH = True
except ImportError:
    ETH = False


@unittest.skipUnless(TOOLS and ETH and os.path.isfile(KEYSH), "no bash / awk, no eth-account, or no deploy folder")
class VmKeyTest(unittest.TestCase):
    """deploy/gcp/vm-key.sh with systemd's tools stood in for: the settings file's key kept encrypted and taken out of the file
    — every copy, the rest of the file as it was — only once systemd is proved to open it to the same address; a failed proof
    changes nothing; a new key is checked first and the service restarted on it; the key itself never printed."""

    def setUp(self) -> None:
        self.dir = tempfile.mkdtemp()
        bin_dir = os.path.join(self.dir, "bin")
        os.mkdir(bin_dir)
        self.creds = self.tool(bin_dir, "systemd-creds", FAKE_CREDS.replace("PYTHON", sys.executable))
        self.sdrun = self.tool(bin_dir, "systemd-run", FAKE_RUN.replace("PYTHON", sys.executable))
        self.systemctl = self.tool(bin_dir, "systemctl", FAKE_SYSTEMCTL)
        os.mkdir(os.path.join(self.dir, "etc"))
        self.env_file = os.path.join(self.dir, "etc", "ruletaker.env")
        self.store = os.path.join(self.dir, "credstore.encrypted", "predict_private_key")
        self.unit = os.path.join(self.dir, "ruletaker.service")
        with open(self.unit, "w") as f:
            f.write("[Service]\nLoadCredentialEncrypted=predict_private_key\n")
        self.log = os.path.join(self.dir, "log")
        open(self.log, "w").close()

    def tearDown(self) -> None:
        shutil.rmtree(self.dir, ignore_errors=True)

    @staticmethod
    def tool(folder: str, name: str, text: str) -> str:
        path = os.path.join(folder, name)
        with open(path, "w") as f:
            f.write(text)
        os.chmod(path, 0o755)
        return path

    def settings(self, text: str) -> None:
        with open(self.env_file, "w", encoding="utf-8") as f:
            f.write(text)
        os.chmod(self.env_file, 0o640)

    def read(self) -> str:
        with open(self.env_file, encoding="utf-8", newline="") as f:  # (as it is: line ends untranslated)
            return f.read()

    def kept(self) -> str:
        """The kept key, opened as the service's would be."""
        return subprocess.run([self.creds, "decrypt", "--name=predict_private_key", self.store, "-"], capture_output=True,
                              text=True, check=True).stdout

    def key_sh(self, *words: str, stdin: str = "", **extra: str) -> tuple[int, str]:
        python = os.path.join(ROOT, "python")
        env = dict(os.environ, RT_ENV=self.env_file, RT_STORE=self.store, RT_UNIT=self.unit, RT_CREDS=self.creds,
                   RT_SYSTEMD_RUN=self.sdrun, RT_SYSTEMCTL=self.systemctl, RT_AS="", RT_WAIT="0",
                   RT_RUN=f"env PYTHONPATH={python} {sys.executable} -m ruletaker", FAKE_LOG=self.log, FAKE_CREDS=self.creds)
        env.update(extra)
        env.pop("CREDENTIALS_DIRECTORY", None)
        out = subprocess.run(["bash", KEYSH, *words], capture_output=True, text=True, env=env, input=stdin, timeout=120)
        text = out.stdout + out.stderr
        for secret in (A, B, OLD):  # never printed, whatever happened
            self.assertNotIn(secret, text.lower())
        return out.returncode, text

    def test_move_takes_every_copy_out_and_keeps_the_key_encrypted(self) -> None:
        before = ("RULE_TAKER=on\n"
                  f"export PREDICT_PRIVATE_KEY=0x{A}  # 私钥\n"
                  "PREDICT_API_KEY=k\n"
                  f'#PREDICT_PRIVATE_KEY="0x{OLD}"\n'
                  f"PREDICT_PRIVATE_KEY={B}\n"
                  "TELEGRAM_CHAT_ID=1\r\n"
                  f"备忘 {A.upper()}\n"
                  "STATUS_KEY=x # 控制台\n")
        self.settings(before)
        code, out = self.key_sh("move")
        self.assertEqual(code, 0, out)
        self.assertIn("已从设置文件收走，加密保存在这台机器上", out)
        after = self.read()
        for secret in (A, B, OLD):
            self.assertNotIn(secret, after.lower())
        self.assertEqual(after.count(MARK), 1)
        self.assertEqual([line for line in after.split("\n") if not line.startswith(MARK)],
                         ["RULE_TAKER=on", "PREDICT_API_KEY=k", "TELEGRAM_CHAT_ID=1\r", "STATUS_KEY=x # 控制台", ""])
        self.assertTrue(after.split("\n")[1].startswith(MARK), "the note where the key was")
        self.assertEqual(stat.S_IMODE(os.stat(self.env_file).st_mode), 0o640)
        self.assertEqual(sorted(os.listdir(os.path.dirname(self.env_file))), ["ruletaker.env"])  # (no copy, no leftover)
        # the key the service read (its first line's), kept; the service reads no plain key any more
        self.assertEqual(self.kept(), "0x" + A)
        self.assertEqual(stat.S_IMODE(os.stat(self.store).st_mode), 0o600)
        settings = load_env_file(self.env_file)
        self.assertNotIn("PREDICT_PRIVATE_KEY", settings)
        config = Config.load({**settings, "PREDICT_API_KEY": "k"}, Stored(True, self.kept()))
        self.assertEqual((config.private_key, config.key_kept), ("0x" + A, "stored"))
        # proved by systemd opening it for the service's user, with the service's devices
        with open(self.log) as f:
            proof = f.read()
        self.assertIn("-p User=ruletaker", proof)
        self.assertIn("-p PrivateDevices=yes", proof)
        self.assertIn(f"签名地址 {A_SIGNER}", out)
        # once more: nothing to do, nothing touched
        stamp = os.stat(self.env_file).st_ino, os.stat(self.store).st_mtime_ns
        code, out = self.key_sh("move")
        self.assertEqual((code, out), (0, ""))
        self.assertEqual((os.stat(self.env_file).st_ino, os.stat(self.store).st_mtime_ns), stamp)
        # the commands the service's way: its kept key handed over
        code, out = self.key_sh("run", "key")
        self.assertEqual((code, out.strip()), (0, A_SIGNER))
        code, out = self.key_sh("status")
        self.assertIn("私钥：加密保存在", out)

    def test_a_failed_proof_changes_nothing(self) -> None:
        before = f"RULE_TAKER=on\nPREDICT_PRIVATE_KEY=0x{A}\n"
        self.settings(before)
        code, out = self.key_sh("move", FAKE_OTHER="1")
        self.assertNotEqual(code, 0)
        self.assertIn("什么都没改", out)
        self.assertEqual(self.read(), before)
        self.assertFalse(os.path.exists(self.store))
        self.assertFalse(os.path.exists(self.store + ".new"))
        # an old unit (it would not open a kept key): the key stays where it is
        with open(self.unit, "w") as f:
            f.write("[Service]\n")
        code, out = self.key_sh("move")
        self.assertNotEqual(code, 0)
        self.assertIn("rt.sh update", out)
        self.assertEqual(self.read(), before)
        self.assertFalse(os.path.exists(self.store))

    def test_the_tpm_first_and_the_host_key_when_there_is_none(self) -> None:
        self.settings(f"PREDICT_PRIVATE_KEY=0x{A}\n")
        code, out = self.key_sh("move", RT_TPM="1")
        self.assertEqual(code, 0, out)
        self.assertIn("虚拟 TPM：拷走磁盘也解不开", out)
        self.settings(f"PREDICT_PRIVATE_KEY=0x{A}\n")
        code, out = self.key_sh("move", RT_TPM="1", FAKE_NO_TPM="1")
        self.assertEqual(code, 0, out)
        self.assertIn("host+tpm2 没做成", out)
        self.assertIn("本机密钥：root 才读得到", out)
        self.assertEqual(self.kept(), "0x" + A)

    def test_a_settings_file_bringing_another_key_is_said(self) -> None:
        self.settings(f"PREDICT_PRIVATE_KEY=0x{A}\n")
        self.assertEqual(self.key_sh("move")[0], 0)
        self.settings(f"PREDICT_PRIVATE_KEY=0x{B}\n")  # (an old local file uploaded again with rt.sh env)
        code, out = self.key_sh("move")
        self.assertEqual(code, 0, out)
        self.assertIn(f"原来的签名地址 {A_SIGNER}", out)
        self.assertEqual(self.kept(), "0x" + B)

    def test_set_checks_a_new_key_then_keeps_it_and_restarts(self) -> None:
        self.settings(f"RULE_TAKER=on\nPREDICT_PRIVATE_KEY=0x{OLD}\nPREDICT_API_KEY=k\n")
        code, out = self.key_sh("set", stdin=A + "\n")
        self.assertEqual(code, 0, out)
        self.assertIn(A_SIGNER, out)  # (its address shown, never the key)
        self.assertIn("✅ 服务已重启", out)
        self.assertEqual(self.kept(), "0x" + A)
        self.assertNotIn(OLD, self.read())
        with open(self.log) as f:
            self.assertIn("systemctl restart ruletaker", f.read())
        # what is not a key, or nothing: refused before anything else is asked, nothing changed, nothing restarted
        before, kept = self.read(), self.kept()
        open(self.log, "w").close()
        for given, said in (("xyz\n", "不是 64 位十六进制的私钥"), ("\n", "没收到私钥"),
                            ("0x" + "1" * 63 + "\n", "不是 64 位十六进制的私钥")):
            code, out = self.key_sh("set", stdin=given)
            self.assertNotEqual(code, 0, given)
            self.assertIn(said, out)
            self.assertIn("什么都没改", out)
        self.assertEqual((self.read(), self.kept()), (before, kept))
        with open(self.log) as f:
            self.assertNotIn("systemd", f.read())  # (not even the check was run)
        # a key that does not control PREDICT_ACCOUNT, or a chain that cannot be read: said so, nothing changed
        python = os.path.join(ROOT, "python")
        cli = self.tool(self.dir, "cli", f"""#!/usr/bin/env bash
case " $* " in
  *" --env "*) cat > /dev/null; echo {A_SIGNER}; echo "$FAKE_SAYS"; exit "$FAKE_CODE" ;;
esac
exec env PYTHONPATH={python} {sys.executable} -m ruletaker "$@"
""")
        for fake_code, said in (("1", "这把私钥控制不了设置里的 PREDICT_ACCOUNT"), ("3", "链上核对没做成")):
            code, out = self.key_sh("set", stdin=B + "\n", RT_RUN=cli, FAKE_CODE=fake_code, FAKE_SAYS="❌ 不是 Predict 账户 0x…")
            self.assertNotEqual(code, 0)
            self.assertIn(said, out)
            self.assertIn("什么都没改", out)
            self.assertIn("❌ 不是 Predict 账户", out)  # (the check's own words passed on)
        self.assertEqual((self.read(), self.kept()), (before, kept))
        with open(self.log) as f:
            self.assertNotIn("restart", f.read())

    def test_it_reads_the_settings_as_vm_call_does(self) -> None:
        # (one reading of the settings file, tested against the service's in VmCallTest: the two copies kept the same)
        def reading(path: str) -> str:
            with open(path, encoding="utf-8") as f:
                text = f.read()
            start = text.index("setting() {")
            return text[text.index("\n", start):text.index("\n}\n", start)]
        self.assertEqual(reading(KEYSH), reading(CALL))

    def test_put_replaces_a_setting_everywhere_or_adds_it(self) -> None:
        # (rt.sh set: one setting from a phone, no editor)
        self.settings("RULE_TAKER=paper\nPREDICT_API_KEY=k\nRULE_WS_CONNECTIONS=1   # 推送连接\nexport RULE_WS_CONNECTIONS=2\n"
                      "# RULE_WS_CONNECTIONS=9 (a comment)\nPREDICT_RATE_PER_MIN=220\n")
        code, out = self.key_sh("put", "RULE_WS_CONNECTIONS", stdin="3\n")
        self.assertEqual(code, 0, out)
        self.assertIn("✅ RULE_WS_CONNECTIONS 已写进设置文件", out)
        # every line of it gone (the service reads only the first), the new one in the first one's place; comments stay
        self.assertEqual(self.read(), "RULE_TAKER=paper\nPREDICT_API_KEY=k\nRULE_WS_CONNECTIONS=3\n"
                                      "# RULE_WS_CONNECTIONS=9 (a comment)\nPREDICT_RATE_PER_MIN=220\n")
        self.assertEqual(load_env_file(self.env_file)["RULE_WS_CONNECTIONS"], "3")
        self.assertEqual(stat.S_IMODE(os.stat(self.env_file).st_mode), 0o640)
        with open(self.log) as f:
            self.assertIn("systemctl restart ruletaker", f.read())
        # a setting not there yet: added at the end — a key's value never printed
        code, out = self.key_sh("put", "RULE_API_KEY_2", stdin="spare-key-123\n")
        self.assertEqual(code, 0, out)
        self.assertTrue(self.read().endswith("PREDICT_RATE_PER_MIN=220\nRULE_API_KEY_2=spare-key-123\n"), self.read())
        self.assertNotIn("spare-key-123", out)
        self.assertEqual(sorted(os.listdir(os.path.dirname(self.env_file))), ["ruletaker.env"])  # (no copy left behind)

    def test_put_refuses_what_the_service_would_not_take_and_leaves_the_file(self) -> None:
        before = "RULE_TAKER=paper\nPREDICT_API_KEY=k\n"
        self.settings(before)
        for name, value, said in (("RULE_TAKER", "maybe", "设置文件已恢复原样"), ("RULE_WS_CONNECTIONS", "3 4", "不能有空格"),
                                  ("RULE_GOLD", "on#x", "不能有"), ("bad-name", "1", "设置名只能是"),
                                  ("PREDICT_PRIVATE_KEY", A, "rt.sh key")):
            code, out = self.key_sh("put", name, stdin=value + "\n")
            self.assertNotEqual(code, 0, name)
            self.assertIn(said, out, name)
            self.assertEqual(self.read(), before, name)
        self.assertEqual(sorted(os.listdir(os.path.dirname(self.env_file))), ["ruletaker.env"])
        with open(self.log) as f:
            self.assertNotIn("restart", f.read())

    def test_apply_after_an_edit(self) -> None:
        # a key pasted into the settings file the old way: taken in, the settings checked, the service restarted
        self.settings(f"RULE_TAKER=paper\nPREDICT_API_KEY=k\nPREDICT_PRIVATE_KEY={B}\n")
        code, out = self.key_sh("apply")
        self.assertEqual(code, 0, out)
        self.assertEqual(self.kept(), "0x" + B)
        self.assertNotIn(B, self.read())
        self.assertIn("已保存，服务按新设置运行", out)
        # a setting that does not parse: not restarted
        open(self.log, "w").close()
        self.settings("RULE_TAKER=maybe\n")
        code, out = self.key_sh("apply")
        self.assertNotEqual(code, 0)
        self.assertIn("服务没有重启", out)
        with open(self.log) as f:
            self.assertNotIn("restart", f.read())


AUDIT = os.path.join(ROOT, "deploy", "gcp", "vm-audit.sh")


@unittest.skipUnless(TOOLS and os.path.isfile(AUDIT), "no bash / awk, or no deploy folder")
class VmAuditTest(unittest.TestCase):
    """rt.sh audit on the VM (read-only): the key kept encrypted is opened only to look for plain copies of it — the settings
    file, any other file — and none of it is ever printed."""

    def setUp(self) -> None:
        self.dir = tempfile.mkdtemp()
        self.creds = VmKeyTest.tool(self.dir, "systemd-creds", FAKE_CREDS.replace("PYTHON", sys.executable))
        os.mkdir(os.path.join(self.dir, "etc"))
        self.env_file = os.path.join(self.dir, "etc", "ruletaker.env")
        self.store = os.path.join(self.dir, "credstore.encrypted", "predict_private_key")

    def tearDown(self) -> None:
        shutil.rmtree(self.dir, ignore_errors=True)

    def keep(self, key: str) -> None:
        os.makedirs(os.path.dirname(self.store), exist_ok=True)
        subprocess.run([self.creds, "encrypt", "--name=predict_private_key", "--with-key=host", "-", self.store],
                       input=key.encode(), check=True)
        os.chmod(self.store, 0o600)

    def audit(self, settings: str) -> str:
        with open(self.env_file, "w") as f:
            f.write(settings)
        env = dict(os.environ, RT_ENV=self.env_file, RT_STORE=self.store, RT_CREDS=self.creds, RT_SEARCH=self.dir,
                   RT_NO="systemctl journalctl sshd swapon ss")
        out = subprocess.run(["bash", AUDIT], capture_output=True, text=True, env=env, stdin=subprocess.DEVNULL, timeout=60)
        text = out.stdout + out.stderr
        for secret in (A, B):
            self.assertNotIn(secret, text.lower())
        return text

    def test_kept_encrypted_and_nowhere_in_plain_text(self) -> None:
        self.keep("0x" + A)
        out = self.audit(f"RULE_TAKER=on\n{MARK}（…）\n")
        self.assertIn("加密保存在", out)
        self.assertIn("✅ 除了加密保存的那份，哪个文件里都没有私钥的明文", out)
        self.assertNotIn("设置文件里有明文私钥", out)
        # a plain copy somewhere (any letter case): found and named
        with open(os.path.join(self.dir, "notes.txt"), "w") as f:
            f.write("备忘 " + A.upper() + "\n")
        out = self.audit("RULE_TAKER=on\n")
        self.assertIn("⚠️ 私钥的明文还出现在这些文件里", out)
        self.assertIn("notes.txt", out)
        # the kept one and a plain one left in the settings file (an old commented-out line counts)
        os.remove(os.path.join(self.dir, "notes.txt"))
        out = self.audit(f"#PREDICT_PRIVATE_KEY=0x{A}\n")
        self.assertIn("⚠️ 设置文件里有明文私钥", out)
        self.assertNotIn("✅ 除了加密保存的那份", out)

    def test_in_the_settings_file_only(self) -> None:
        out = self.audit(f"PREDICT_PRIVATE_KEY={B}\n")
        self.assertIn("⚠️ 设置文件里有明文私钥：运行 bash deploy/gcp/rt.sh update", out)
        self.assertIn("✅ 私钥只在", out)

    def test_a_kept_key_this_machine_cannot_open(self) -> None:
        os.makedirs(os.path.dirname(self.store))
        with open(self.store, "w") as f:
            f.write("bm90IGEga2V5")
        out = self.audit("RULE_TAKER=on\n")
        self.assertIn("这台机器解不开它", out)
        self.assertIn("rt.sh key", out)


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
