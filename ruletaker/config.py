"""The service's settings, from the environment (an EnvironmentFile on the VM, or a .env file next to the code).

The rule settings keep huice's names and limits (RULE_TAKER, RULE_*_CENTS, RULE_GOLD / RULE_OIL, RULE_*_SHARES,
RULE_MAX_DAY_USD, RULE_FAST_HOURS, RULE_LEAD_MINUTES, RULE_*_LIVE_CENTS), so the values set on Railway carry over as they are.
The Predict account's keep huice's too (PREDICT_API_KEY / RULE_API_KEY, PREDICT_PRIVATE_KEY, PREDICT_ACCOUNT, PREDICT_CHAIN_ID,
BSC_RPC_URL, LIVE_TRADING_ALLOWED)."""
from __future__ import annotations

import dataclasses
import os
from pathlib import Path
from typing import Mapping

from .clock import CALENDAR, DEFAULT_CME_CLOSED, DEFAULT_CME_EARLY, day_spans, in_spans, parse_dates, spans_in

RULE_SETTING_KEYS = {"gold_shut": "RULE_GOLD_SHUT_CENTS", "gold_open": "RULE_GOLD_OPEN_CENTS", "gold_0515": "RULE_GOLD_0515_CENTS",
                     "gold_0500": "RULE_GOLD_0500_CENTS", "gold_1645": "RULE_GOLD_1645_CENTS",
                     "oil_shut": "RULE_OIL_SHUT_CENTS", "oil_open": "RULE_OIL_OPEN_CENTS", "oil_0045": "RULE_OIL_0045_CENTS"}
RULE_DEFAULT_CENTS = {"gold_shut": 58, "gold_open": 50, "gold_0515": 58, "gold_0500": 0, "gold_1645": 0, "oil_shut": 0, "oil_open": 50, "oil_0045": 0}
RULE_MARKET_KEYS = {"gold": ("RULE_GOLD", "RULE_GOLD_SHARES"), "oil": ("RULE_OIL", "RULE_OIL_SHARES")}
RULE_LIVE_KEYS = {"gold_shut": "RULE_GOLD_SHUT_LIVE_CENTS", "gold_open": "RULE_GOLD_OPEN_LIVE_CENTS"}
RULE_FAST_DEFAULT = "01:00-07:00"


def load_env_file(path: str | os.PathLike, into: dict | None = None) -> dict:
    """KEY=VALUE lines (comments, blank lines and an optional ``export`` allowed; quotes around a value taken off). Values
    already in ``into`` win (the process environment over the file)."""
    out = dict(into) if into is not None else {}
    file = Path(path)
    if not file.is_file():
        return out
    for line in file.read_text(encoding="utf-8").splitlines():
        line = line.strip()
        if not line or line.startswith("#") or "=" not in line:
            continue
        name, _, value = line.removeprefix("export ").partition("=")
        name, value = name.strip(), value.strip()
        if "#" in value and not value.startswith(("'", '"')):
            value = value.split(" #", 1)[0].strip()  # a comment after the value ("RULE_GOLD_SHARES=50   # …")
        if len(value) >= 2 and value[0] == value[-1] and value[0] in "'\"":
            value = value[1:-1]
        out.setdefault(name, value)
    return out


@dataclasses.dataclass(frozen=True)
class RuleSettings:
    """RULE_TAKER off | paper (只记录，不下单) | on (真实吃单); each market on or off by itself with the most shares bought a window;
    each rule's highest price in cents (0 = that rule off); the most spent a Beijing day; the per-second hours; the makers'
    lead; the prices in the windows' course."""
    mode: str = "off"
    cents: tuple = tuple(RULE_DEFAULT_CENTS.items())
    markets: frozenset = frozenset(RULE_MARKET_KEYS)
    shares: tuple = (("gold", 50), ("oil", 50))
    max_day_usd: float = 0.0
    fast: tuple = ((60, 420),)
    live: tuple = ()
    lead_minutes: int = 300

    @property
    def enabled(self) -> bool:
        return self.mode != "off"

    def fast_at(self, now_ms: int) -> bool:
        return in_spans(self.fast, now_ms)

    def fast_in(self, now_ms: int) -> int | None:
        return spans_in(self.fast, now_ms)

    def price(self, key: str) -> int:
        return int(dict(self.cents).get(key, 0))

    def on(self, market: str) -> bool:
        return market in self.markets

    def live_price(self, key: str) -> int:
        return int(dict(self.live).get(key, 0))

    def shares_of(self, market: str) -> int:
        return int(dict(self.shares).get(market, 0))

    def with_mode(self, mode: str) -> "RuleSettings":
        return dataclasses.replace(self, mode=mode)

    @classmethod
    def parse(cls, e: Mapping, mode: str | None = None) -> "RuleSettings":
        mode = str(e.get("RULE_TAKER", "off") if mode is None else mode).strip().lower()
        if mode not in {"off", "paper", "on"}:
            raise ValueError("RULE_TAKER 只能是 off、paper（只记录不下单）或 on（真实吃单）")
        cents = {key: int(str(e.get(name, RULE_DEFAULT_CENTS[key])).strip() or 0) for key, name in RULE_SETTING_KEYS.items()}
        switches = {m: str(e.get(switch, "on")).strip().lower() for m, (switch, _) in RULE_MARKET_KEYS.items()}
        default = str(e.get("RULE_SHARES", "50")).strip() or "50"
        shares = {m: int(str(e.get(name, default)).strip() or 0) for m, (_, name) in RULE_MARKET_KEYS.items()}
        max_day = float(str(e.get("RULE_MAX_DAY_USD", "0")).strip() or 0)
        fast = day_spans(e.get("RULE_FAST_HOURS", RULE_FAST_DEFAULT))
        lead = int(str(e.get("RULE_LEAD_MINUTES", "300")).strip() or 0)
        if not 0 <= lead <= 1440:
            raise ValueError("RULE_LEAD_MINUTES 0～1440（对手提前多少分钟挂单，规则吃单在那一刻前后每秒检查；0 = 不按这个时刻）")
        live = {key: int(str(e.get(name, "0")).strip() or 0) for key, name in RULE_LIVE_KEYS.items()}
        if any(not (c == 0 or 1 <= c <= 97) for c in live.values()):
            raise ValueError("RULE_*_LIVE_CENTS 是 0（盘中不做）或 1～97（¢，开场后的最高买价）")
        if any(v not in {"on", "off"} for v in switches.values()):
            raise ValueError("RULE_GOLD、RULE_OIL 只能是 on 或 off")
        if (any(not (c == 0 or 1 <= c <= 95) for c in cents.values()) or any(not 1 <= n <= 100_000 for n in shares.values())
                or not 0 <= max_day <= 1_000_000):
            raise ValueError("RULE_*_CENTS 是 0（不做这条规则）或 1～95（¢，最高买价）；RULE_GOLD_SHARES、RULE_OIL_SHARES 每场最多买 1～100000 份；"
                             "RULE_MAX_DAY_USD 0～1000000（每天最多花多少，0 = 不限）")
        return cls(mode, tuple(cents.items()), frozenset(m for m, v in switches.items() if v == "on"), tuple(shares.items()), max_day, fast,
                   tuple(live.items()), lead)


def bounded(e: Mapping, name: str, default: float, low: float, high: float, kind: type = int) -> float:
    raw = str(e.get(name, "")).strip()
    try:
        value = kind(raw) if raw else kind(default)
    except ValueError:
        raise ValueError(f"{name} 应为数字，现在是“{raw}”") from None
    if not low <= value <= high:
        raise ValueError(f"{name} 应在 {low:g}～{high:g} 之间，现在是 {value:g}")
    return value


def allowed(value: str | None) -> bool:
    """LIVE_TRADING_ALLOWED: only unset, or on / 1 / true / yes, allows real orders — anything else (empty, a typo, off) does not."""
    if value is None:
        return True
    return str(value).strip().lower() in {"on", "1", "true", "yes"}


@dataclasses.dataclass(frozen=True)
class Config:
    rules: RuleSettings
    trading_allowed: bool = True
    api_key: str = ""                 # RULE_API_KEY, else PREDICT_API_KEY
    own_key: bool = False             # the key is RULE_API_KEY (one of the taker's own)
    private_key: str = ""
    account: str = ""
    chain_id: int = 56
    rpc_url: str = ""
    fee_bps: int = 200
    rate_per_min: float = 220.0
    max_daily_loss_usd: float = 0.0
    state_db: str = "data/ruletaker.sqlite3"
    telegram_token: str = ""
    telegram_chats: tuple = ()        # ((chat id, thread id or 0), ...)
    admin_user_id: int = 0
    telegram_commands: bool = False
    notify: bool = True
    status_host: str = "127.0.0.1"
    status_port: int = 8080
    status_key: str = ""
    ws_connections: int = 1
    binance_fapi: str = "https://fapi.binance.com"
    binance_ws: str = "wss://fstream.binance.com/market"
    region: str = ""
    log_level: str = "INFO"

    @property
    def live(self) -> bool:
        return self.rules.mode == "on"

    @classmethod
    def from_env(cls, e: Mapping | None = None) -> "Config":
        e = dict(os.environ if e is None else e)
        rules = RuleSettings.parse(e)
        CALENDAR.configure(parse_dates(e.get("CME_EARLY_DAYS", DEFAULT_CME_EARLY), "CME_EARLY_DAYS"),
                           parse_dates(e.get("CME_CLOSED_DAYS", DEFAULT_CME_CLOSED), "CME_CLOSED_DAYS"))
        own = str(e.get("RULE_API_KEY", "")).strip()
        shared = str(e.get("PREDICT_API_KEY", "")).strip()
        key = own or shared
        private_key = str(e.get("PREDICT_PRIVATE_KEY", "")).strip()
        if private_key and not private_key.startswith("0x"):
            private_key = "0x" + private_key
        if private_key and (len(private_key) != 66 or any(c not in "0123456789abcdefABCDEF" for c in private_key[2:])):
            raise ValueError("PREDICT_PRIVATE_KEY 应为 64 位十六进制（0x 可省）")
        account = str(e.get("PREDICT_ACCOUNT", "")).strip()
        if account and (not account.startswith("0x") or len(account) != 42):
            raise ValueError("PREDICT_ACCOUNT 应为 0x 开头的 42 位地址（网站账户设置里的充值地址），用普通钱包交易就留空")
        if rules.mode == "on" and not private_key:
            raise ValueError("RULE_TAKER=on 需要 PREDICT_PRIVATE_KEY（签名钱包私钥）")
        if rules.mode != "off" and not key and int(e.get("PREDICT_CHAIN_ID", "56") or 56) == 56:
            raise ValueError("RULE_TAKER=paper / on 需要 PREDICT_API_KEY（或 RULE_API_KEY）：Predict 主网接口要 API Key")
        chats = []
        for item in str(e.get("TELEGRAM_CHAT_ID", "")).replace("，", ",").split(","):
            item = item.strip()
            if not item:
                continue
            chat, _, thread = item.partition(":")
            try:
                chats.append((int(chat), int(thread or 0)))
            except ValueError:
                raise ValueError(f"TELEGRAM_CHAT_ID 写成数字，群组话题写成 群ID:话题ID，多个用逗号分开；现在是“{item}”") from None
        ws = int(bounded(e, "RULE_WS_CONNECTIONS", 1, 1, 3))
        port = int(bounded(e, "STATUS_PORT", 8080, 0, 65535))
        status_host = str(e.get("STATUS_HOST", "127.0.0.1")).strip() or "127.0.0.1"
        status_key = str(e.get("STATUS_KEY", "")).strip()
        if status_host not in {"127.0.0.1", "localhost", "::1"} and len(status_key) < 12:
            raise ValueError("STATUS_HOST 不是本机时必须设置 STATUS_KEY（至少 12 位），否则任何人都能看状态、暂停")
        return cls(
            rules=rules,
            trading_allowed=allowed(e.get("LIVE_TRADING_ALLOWED")),
            api_key=key, own_key=bool(own) and own != shared,
            private_key=private_key, account=account,
            chain_id=int(bounded(e, "PREDICT_CHAIN_ID", 56, 1, 10 ** 9)),
            rpc_url=str(e.get("BSC_RPC_URL", "")).strip().rstrip("/"),
            fee_bps=int(bounded(e, "PREDICT_FEE_BPS", 200, 0, 1000)),
            rate_per_min=float(bounded(e, "PREDICT_RATE_PER_MIN", 220, 0, 6000, float)),
            max_daily_loss_usd=float(bounded(e, "RULE_MAX_DAILY_LOSS_USD", 0, 0, 1_000_000, float)),
            state_db=str(e.get("STATE_DB", "")).strip() or "data/ruletaker.sqlite3",
            telegram_token=str(e.get("TELEGRAM_BOT_TOKEN", "")).strip(),
            telegram_chats=tuple(chats),
            admin_user_id=int(bounded(e, "ADMIN_USER_ID", 0, 0, 10 ** 15)),
            telegram_commands=str(e.get("TELEGRAM_COMMANDS", "off")).strip().lower() in {"on", "1", "true", "yes"},
            notify=str(e.get("RULE_NOTIFY", "on")).strip().lower() not in {"off", "0", "false", "no"},
            status_host=status_host, status_port=port, status_key=status_key,
            ws_connections=ws,
            binance_fapi=str(e.get("BINANCE_BASE_URL", "https://fapi.binance.com")).strip().rstrip("/") or "https://fapi.binance.com",
            binance_ws=str(e.get("BINANCE_WS_URL", "wss://fstream.binance.com/market")).strip().rstrip("/") or "wss://fstream.binance.com/market",
            region=str(e.get("RULE_REGION", "")).strip(),
            log_level=str(e.get("LOG_LEVEL", "INFO")).strip().upper() or "INFO",
        )

    def secrets_line(self) -> str:
        """Which secrets are set (never their values): the deploy's first log line."""
        return (f"secrets configured: predict_api_key={bool(self.api_key)} own_rule_key={self.own_key} "
                f"predict_private_key={bool(self.private_key)} predict_account={bool(self.account)} "
                f"telegram_token={bool(self.telegram_token)} status_key={bool(self.status_key)} mode={self.rules.mode} "
                f"trading_allowed={self.trading_allowed}")
