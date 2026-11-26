"""The private key kept encrypted on the VM (deploy/gcp/vm-key.sh): systemd hands it to the service in its credentials
directory; the settings file's PREDICT_PRIVATE_KEY is used only when none is kept; two different keys are refused; and
``python -m ruletaker key`` says a key's signing address — never the key — for the VM's script to check what it stored."""
from __future__ import annotations

import io
import os
import shutil
import tempfile
import unittest
from unittest import mock

from ruletaker.config import (
    KEY_CREDENTIAL,
    SECRETS_CREDENTIAL,
    Config,
    Stored,
    secrets_of,
)

A = "a4c123b1612dd272d1371c17149d439536b3216fdaeeb975729fae923d5a4fd1"
A_SIGNER = "0x511387682044334a3e855679c186455753356BF5"  # (tests/vectors/wallet.json)


def load(env: dict, store: bool, key: str) -> Config:
    return Config.load(env, Stored(store, key))


class KeptKeyTest(unittest.TestCase):
    def test_the_key_kept_encrypted_is_used_and_two_different_keys_are_refused(self) -> None:
        a = "0x" + A
        # kept encrypted alone: used (0x added; a line break left by whoever wrote it taken off)
        c = load({"RULE_TAKER": "on", "PREDICT_API_KEY": "k"}, True, A + "\n")
        self.assertEqual((c.private_key, c.key_kept, c.key_store), (a, "stored", True))
        # the settings file's alone: not run by systemd (a laptop) ...
        c = load({"PREDICT_PRIVATE_KEY": A}, False, "")
        self.assertEqual((c.private_key, c.key_kept, c.key_store), (a, "settings", False))
        # ... or on the VM with none kept yet
        c = load({"PREDICT_PRIVATE_KEY": A}, True, "")
        self.assertEqual((c.key_kept, c.key_store), ("settings", True))
        # the same key in both (letter case and the 0x aside): that key, the plain copy reported
        c = load({"PREDICT_PRIVATE_KEY": A.upper()}, True, a)
        self.assertEqual((c.private_key, c.key_kept), (a, "both"))
        # two different keys: refused, whichever way round
        other = "1" * 64
        for env_key, kept in ((other, A), (A, other)):
            with self.assertRaises(ValueError) as e:
                load({"PREDICT_PRIVATE_KEY": env_key}, True, kept)
            self.assertIn("不是同一把", str(e.exception))
        # a kept "key" that is none: refused, and said to be the kept one
        with self.assertRaisesRegex(ValueError, "加密保存的私钥"):
            load({}, True, "0x12")
        with self.assertRaisesRegex(ValueError, "PREDICT_PRIVATE_KEY 应为"):
            load({"PREDICT_PRIVATE_KEY": "0x12"}, True, A)
        # none anywhere: on refused as before, paper fine
        with self.assertRaisesRegex(ValueError, "PREDICT_PRIVATE_KEY"):
            load({"RULE_TAKER": "on", "PREDICT_API_KEY": "k"}, True, "")
        c = load({"RULE_TAKER": "paper", "PREDICT_API_KEY": "k"}, True, "")
        self.assertEqual((c.private_key, c.key_kept), ("", ""))
        self.assertTrue(c.secrets_line().endswith("private_key_kept=none"), c.secrets_line())
        c = load({}, True, A)
        self.assertTrue(c.secrets_line().endswith("private_key_kept=stored"))
        self.assertNotIn(A, repr(c) + repr(Stored(True, A)))

    def test_the_secrets_kept_encrypted_fill_in_what_the_settings_file_does_not_set(self) -> None:
        kept = "PREDICT_API_KEY=kept-api-key\nTELEGRAM_BOT_TOKEN=123:kept-token\nRULE_TAKER=on\nSTATUS_KEY=\nRULE_API_KEY_2=spare-two\n"
        stored = Stored(True, "", secrets_of(kept))
        # (RULE_TAKER is no secret: never taken from what is kept; an empty value neither)
        self.assertEqual([n for n, _ in stored.secrets], ["PREDICT_API_KEY", "TELEGRAM_BOT_TOKEN", "RULE_API_KEY_2"])
        c = Config.load({"RULE_TAKER": "paper"}, stored)
        self.assertEqual((c.api_key, c.telegram_token, c.api_spares, c.secrets_kept, c.rules.mode),
                         ("kept-api-key", "123:kept-token", ("spare-two",), 3, "paper"))
        self.assertIn("secrets_kept=3", c.secrets_line())
        # a value in the settings file wins (the next rt.sh edit / set / update keeps it instead); an empty one does not
        c = Config.load({"RULE_TAKER": "paper", "PREDICT_API_KEY": "file-api-key", "TELEGRAM_BOT_TOKEN": " "}, stored)
        self.assertEqual((c.api_key, c.telegram_token, c.secrets_kept), ("file-api-key", "123:kept-token", 2))
        shown = repr(c) + str(c) + repr(stored)
        for secret in ("kept-api-key", "kept-token", "spare-two", "file-api-key"):
            self.assertNotIn(secret, shown)

    def test_systemd_s_directory_is_read_and_an_unreadable_key_is_no_silent_none(self) -> None:
        self.assertFalse(Stored.from_dir(None).store)
        self.assertFalse(Stored.from_dir("").store)
        folder = tempfile.mkdtemp()
        try:
            s = Stored.from_dir(folder)
            self.assertTrue(s.store and not s.key, "a directory without the key: none kept")
            with open(os.path.join(folder, KEY_CREDENTIAL), "w") as f:
                f.write("0x" + A + "\n")
            self.assertEqual(Stored.from_dir(folder).key, "0x" + A)
            os.remove(os.path.join(folder, KEY_CREDENTIAL))
            with open(os.path.join(folder, SECRETS_CREDENTIAL), "w") as f:
                f.write("STATUS_KEY=kept-status-key-1\n")
            self.assertEqual(Stored.from_dir(folder).secrets, (("STATUS_KEY", "kept-status-key-1"),))
            os.mkdir(os.path.join(folder, KEY_CREDENTIAL))
            with self.assertRaisesRegex(ValueError, "读不到加密保存的私钥"):
                Stored.from_dir(folder)
            os.rmdir(os.path.join(folder, KEY_CREDENTIAL))
            os.remove(os.path.join(folder, SECRETS_CREDENTIAL))
            os.mkdir(os.path.join(folder, SECRETS_CREDENTIAL))
            with self.assertRaisesRegex(ValueError, "读不到加密保存的设置"):
                Stored.from_dir(folder)
        finally:
            shutil.rmtree(folder, ignore_errors=True)


class KeyCommandTest(unittest.TestCase):
    """``python -m ruletaker key``: the address alone on the first line; exit 2 for no key or not a key."""

    def key(self, *args: str, stdin: str = "", creds: str | None = None) -> tuple[int, str, str]:
        from ruletaker.__main__ import main
        out, err = io.StringIO(), io.StringIO()
        env = {k: v for k, v in os.environ.items() if k != "CREDENTIALS_DIRECTORY"}
        if creds is not None:
            env["CREDENTIALS_DIRECTORY"] = creds
        with mock.patch.dict(os.environ, env, clear=True), mock.patch("sys.stdin", io.StringIO(stdin)), \
                mock.patch("sys.stdout", out), mock.patch("sys.stderr", err):
            code = main(["key", *args])
        return code, out.getvalue(), err.getvalue()

    def test_a_key_s_address_never_the_key(self) -> None:
        try:
            import eth_account  # noqa: F401
        except ImportError:
            self.skipTest("no eth-account")
        code, out, err = self.key("--stdin", stdin=A + "\n")
        self.assertEqual((code, out), (0, A_SIGNER + "\n"))
        self.assertNotIn(A, out + err)
        folder = tempfile.mkdtemp()
        try:
            with open(os.path.join(folder, KEY_CREDENTIAL), "w") as f:
                f.write("0x" + A)
            code, out, _ = self.key(creds=folder)  # (the one systemd hands over)
            self.assertEqual((code, out), (0, A_SIGNER + "\n"))
            os.remove(os.path.join(folder, KEY_CREDENTIAL))
            code, out, err = self.key(creds=folder)
            self.assertEqual((code, out), (2, ""))
            self.assertIn("没有加密保存的私钥", err)
        finally:
            shutil.rmtree(folder, ignore_errors=True)

    def test_not_a_key(self) -> None:
        for given in ("zz\n", "0x12\n", A[:-1] + "g\n"):
            code, out, err = self.key("--stdin", stdin=given)
            self.assertEqual((code, out), (2, ""), given)
            self.assertIn("这不是私钥", err)
        code, out, err = self.key("--stdin", stdin="")
        self.assertEqual((code, out), (2, ""))
        self.assertIn("标准输入是空的", err)


if __name__ == "__main__":
    unittest.main()
