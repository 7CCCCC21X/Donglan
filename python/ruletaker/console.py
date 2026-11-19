"""The control page's settings (控制台“规则吃单”一组, as in huice): what can be changed while the service runs, how each is shown, and
the saved values laid over the deploy's settings file.

A value saved here (control:settings in the store) is in force at once and after a restart, over the settings file's; "恢复部署设置"
drops it. A save is checked whole, as a start is (RuleSettings.parse): a wrong value changes nothing and says why. The mode is not
here — it has its own switch, never above the settings file's RULE_TAKER; nor are the keys, the wallet or the chain."""
from __future__ import annotations

from typing import Any, Mapping

from .config import RuleSettings, bounded

# (key, label, kind, presets or options, unit, what 0 means, help) — grouped as the page shows them
GROUPS: tuple[tuple[str, tuple[tuple, ...]], ...] = (
    ("黄金", (
        ("RULE_GOLD", "黄金规则", "choice", [["on", "开"], ["off", "关"]], "", "",
         "on = 做黄金的规则（价格设为 0 的那条不做）；off = 黄金一条都不做"),
        ("RULE_GOLD_SHARES", "每场最多买多少份", "number", ["10", "20", "50", "100", "200", "500"], "份", "",
         "1～100000：一个 15 分钟场次最多买这么多份（盘前、盘中、几张单合计）。卖单够多（≥20 份且不少于还能买的 1/4）时一张单就按这一场还能买的全部份数下；"
         "USDT 不够时按买得起的份数下"),
        ("RULE_GOLD_SHUT_CENTS", "休市 4 档：买涨不高于", "number", ["0", "55", "58", "60", "65"], "¢", "不做",
         "纽约 17:00–17:45、周一至周四 CME 每天休市的 4 个 15 分钟（北京冬令时 06:00–06:45，夏令时 05:00–05:45）：2025-12 至 2026-11 涨占 90%"
         "（6 月起指数在休市时也在动，涨占 84%）"),
        ("RULE_GOLD_SHUT_LIVE_CENTS", "休市 4 档·盘中：买涨不高于", "number", ["0", "60", "70", "80", "90"], "¢", "盘中不做",
         "开场后也吃：币安指数比这一场的起点高 0.005% 以上（涨已经领先）时才买；开场后第 1 分钟最高按盘前的价格；和盘前共用每场份数。建议不超过 80¢"),
        ("RULE_GOLD_OPEN_CENTS", "开盘第一档：买跌不高于", "number", ["0", "45", "48", "50", "52"], "¢", "不做",
         "纽约 18:00、周一至周四每天重新开盘的第一个 15 分钟（北京冬令时 07:00，夏令时 06:00；周一早上那场不做）：2026 年跌占 58%，近半年 78%"),
        ("RULE_GOLD_OPEN_LIVE_CENTS", "开盘第一档·盘中：买跌不高于", "number", ["0", "60", "70", "80", "90"], "¢", "盘中不做",
         "开场后也吃：币安指数比这一场的起点低 0.02% 以上（跌已经领先）时才买；开场后第 1 分钟最高按盘前的价格"),
        ("RULE_GOLD_0515_CENTS", "纽约 05:15：买跌不高于", "number", ["0", "50", "55", "58"], "¢", "不做",
         "纽约 05:15–05:30、周一至周五（北京冬令时 18:15，夏令时 17:15）：伦敦上午定盘前，2026 年跌占 65%"),
        ("RULE_GOLD_0500_CENTS", "纽约 05:00：买涨不高于", "number", ["0", "50", "52", "55"], "¢", "不做",
         "纽约 05:00–05:15、周一至周五（北京冬令时 18:00）：涨占 62%；按全部时段扫出来的，建议先只记录"),
        ("RULE_GOLD_1645_CENTS", "休市前一档：买涨不高于", "number", ["0", "50", "52", "55"], "¢", "不做",
         "纽约 16:45–17:00、周一至周五（北京冬令时 05:45）：涨占 60%（周二只有 44%）；建议先只记录"),
    )),
    ("原油", (
        ("RULE_OIL", "原油规则", "choice", [["on", "开"], ["off", "关"]], "", "",
         "on = 做原油的规则（价格设为 0 的那条不做）；off = 原油一条都不做"),
        ("RULE_OIL_SHARES", "每场最多买多少份", "number", ["10", "20", "50", "100", "200", "500"], "份", "", "1～100000：原油一个 15 分钟场次最多买这么多份"),
        ("RULE_OIL_OPEN_CENTS", "开盘第一档：买跌不高于", "number", ["0", "45", "50", "55"], "¢", "不做",
         "纽约 18:00、周一至周四重新开盘的第一个 15 分钟（北京冬令时 07:00）：2026 年跌占 59%，50¢ 每份约赚 8¢"),
        ("RULE_OIL_SHUT_CENTS", "休市 4 档：买跌不高于", "number", ["0", "35", "40", "45"], "¢", "不做",
         "纽约 17:00–17:45、周一至周四：指数不动时按涨结算，跌只占 42%，默认不做"),
        ("RULE_OIL_0045_CENTS", "纽约 00:45：买跌不高于", "number", ["0", "45", "50", "52"], "¢", "不做",
         "纽约 00:45–01:00、周一至周五（北京冬令时 13:45）：跌占 62%；显著性刚到边，建议先只记录"),
    )),
    ("额度与时段", (
        ("RULE_MAX_DAY_USD", "每天最多花多少", "number", ["0", "50", "100", "200", "500", "1000"], "$", "不限",
         "黄金、原油合计，一天（北京时间）下单的金额（成交的按成交价、还没结果的按限价全额）到这个数就不再买"),
        ("RULE_MAX_DAILY_LOSS_USD", "今日亏损停机线", "number", ["0", "50", "100", "200", "500"], "$", "不限",
         "本服务今天已结算的亏损到这个数就停止开新单（明天自动恢复；在“现在”一栏点恢复会从当时的盈亏起再给一个额度）"),
        ("RULE_FAST_HOURS", "每秒检查时段（北京时间）", "text", ["off", "01:00-07:00", "00:00-07:00", "00:55-02:00", "00:00-24:00"], "", "",
         "写成 01:00-07:00（可写多段用逗号分开，结束早于开始 = 跨过半夜，off = 不做）：已发布的规则场次每秒看一次、每秒 REST 核对推送、预先签好单；"
         "开始前 1 分钟先准备好登录和授权"),
        ("RULE_LEAD_MINUTES", "对手提前多少分钟挂单", "number", ["0", "240", "300", "360"], "分钟", "不按这个时刻",
         "默认 300（5 小时）：有人在开场前这么久把单挂出来。每场在这个时刻的前 1 分钟到后 15 分钟每秒检查、预先签好单，前 10 秒到后 2 分钟每秒 3 次 REST 核对"),
    )),
)
FIELDS = {f[0]: f for _, fields in GROUPS for f in fields}
KEYS = tuple(FIELDS)
SAVED = "control:settings"


def clean(values: Mapping[str, Any]) -> dict[str, str]:
    """The submitted values as the settings file would write them (strings, trimmed); a key that is not here is refused."""
    out = {}
    for key, value in values.items():
        if key not in FIELDS:
            raise ValueError(f"{key} 不能在控制台里改")
        text = str(value if value is not None else "").strip()
        if FIELDS[key][2] == "choice" and text.lower() not in {o[0] for o in FIELDS[key][3]}:
            raise ValueError(f"{FIELDS[key][1]} 只能是 " + " / ".join(o[0] for o in FIELDS[key][3]))
        out[key] = text.lower() if FIELDS[key][2] == "choice" else text
    return out


def build(base: Mapping[str, str], saved: Mapping[str, str], mode: str) -> tuple[RuleSettings, float]:
    """The rule settings and the day's loss cap with the saved values over the settings file's; ValueError when they do not
    parse (the same check as at a start)."""
    merged = {**base, **saved}
    settings = RuleSettings.parse(merged, mode)
    cap = float(bounded(merged, "RULE_MAX_DAILY_LOSS_USD", 0, 0, 1_000_000, float))
    return settings, cap


def settle_saved(base: Mapping[str, str], saved: Mapping[str, str], mode: str) -> tuple[RuleSettings, float, dict, list[str]]:
    """At a start: the saved values that still parse over this settings file (it may have changed since); the others dropped
    one by one, named in the list returned."""
    keep, dropped = dict(saved), []
    while True:
        try:
            settings, cap = build(base, keep, mode)
            return settings, cap, keep, dropped
        except ValueError:
            bad = next((k for k in list(keep) if _alone_fails(base, k, keep[k], mode)), None)
            if bad is None:  # (together they fail, each alone parses: all dropped)
                dropped.extend(keep)
                keep = {}
            else:
                dropped.append(bad)
                keep.pop(bad)


def _alone_fails(base: Mapping[str, str], key: str, value: str, mode: str) -> bool:
    try:
        build(base, {key: value}, mode)
        return False
    except ValueError:
        return True


def current(settings: RuleSettings, cap: float) -> dict[str, str]:
    """Every field's value in force, written as the settings file would."""
    from .clock import day_spans_text
    from .config import RULE_LIVE_KEYS, RULE_SETTING_KEYS
    out = {name: str(settings.price(key)) for key, name in RULE_SETTING_KEYS.items()}
    out.update({name: str(settings.live_price(key)) for key, name in RULE_LIVE_KEYS.items()})
    out.update({"RULE_GOLD": "on" if settings.on("gold") else "off", "RULE_OIL": "on" if settings.on("oil") else "off",
                "RULE_GOLD_SHARES": str(settings.shares_of("gold")), "RULE_OIL_SHARES": str(settings.shares_of("oil")),
                "RULE_MAX_DAY_USD": f"{settings.max_day_usd:g}", "RULE_MAX_DAILY_LOSS_USD": f"{cap:g}",
                "RULE_FAST_HOURS": day_spans_text(settings.fast), "RULE_LEAD_MINUTES": str(settings.lead_minutes)})
    return out


def view(settings: RuleSettings, cap: float, saved: Mapping[str, str]) -> list[dict]:
    """The page's settings: each group's fields, with the value in force and whether it is the page's or the settings file's."""
    now = current(settings, cap)
    return [{"group": name, "fields": [{"key": key, "label": label, "kind": kind, "presets": presets, "unit": unit, "zero": zero,
                                        "help": text, "value": now.get(key, ""), "saved": key in saved}
                                       for key, label, kind, presets, unit, zero, text in fields]}
            for name, fields in GROUPS]
