//! The control page's settings (控制台“规则吃单”一组, as in huice): what can be changed while the service runs, how each is shown,
//! and the saved values laid over the deploy's settings file.
//!
//! A value saved here (control:settings in the store) is in force at once and after a restart, over the settings file's;
//! "恢复部署设置" drops it. A save is checked whole, as a start is: a wrong value changes nothing and says why. The mode is not
//! here — it has its own switch, never above the settings file's RULE_TAKER; nor are the keys, the wallet or the chain.
use crate::clock::day_spans_text;
use crate::config::{Env, RULE_LIVE_KEYS, RULE_SETTING_KEYS, RuleSettings, bounded};
use crate::util::g;
use indexmap::IndexMap;
use serde_json::{Value, json};

pub const SAVED: &str = "control:settings";

pub struct Field {
    pub key: &'static str,
    pub label: &'static str,
    pub kind: &'static str,
    pub presets: Value,
    pub unit: &'static str,
    pub zero: &'static str,
    pub help: &'static str,
}

fn f(
    key: &'static str,
    label: &'static str,
    kind: &'static str,
    presets: Value,
    unit: &'static str,
    zero: &'static str,
    help: &'static str,
) -> Field {
    Field { key, label, kind, presets, unit, zero, help }
}

fn onoff() -> Value {
    json!([["on", "开"], ["off", "关"]])
}

/// (group, fields) as the page shows them.
pub fn groups() -> Vec<(&'static str, Vec<Field>)> {
    vec![
        (
            "黄金",
            vec![
                f(
                    "RULE_GOLD",
                    "黄金规则",
                    "choice",
                    onoff(),
                    "",
                    "",
                    "on = 做黄金的规则（价格设为 0 的那条不做）；off = 黄金一条都不做",
                ),
                f(
                    "RULE_GOLD_SHARES",
                    "每场最多买多少份",
                    "number",
                    json!(["10", "20", "50", "100", "200", "500"]),
                    "份",
                    "",
                    "1～100000：一个 15 分钟场次最多买这么多份（盘前、盘中、几张单合计）。卖单够多（≥20 份且不少于还能买的 1/4）时一张单就按这一场还能买的全部份数下；USDT 不够时按买得起的份数下",
                ),
                f(
                    "RULE_GOLD_SHUT_CENTS",
                    "休市 4 档：买涨不高于",
                    "number",
                    json!(["0", "55", "58", "60", "65"]),
                    "¢",
                    "不做",
                    "纽约 17:00–17:45、周一至周四 CME 每天休市的 4 个 15 分钟（北京冬令时 06:00–06:45，夏令时 05:00–05:45）：2025-12 至 2026-11 涨占 90%（6 月起指数在休市时也在动，涨占 84%）",
                ),
                f(
                    "RULE_GOLD_SHUT_LIVE_CENTS",
                    "休市 4 档·盘中：买涨不高于",
                    "number",
                    json!(["0", "60", "70", "80", "90"]),
                    "¢",
                    "盘中不做",
                    "开场后也吃：币安指数比这一场的起点高 0.005% 以上（涨已经领先）时才买；开场后第 1 分钟最高按盘前的价格；和盘前共用每场份数。建议不超过 80¢",
                ),
                f(
                    "RULE_GOLD_OPEN_CENTS",
                    "开盘第一档：买跌不高于",
                    "number",
                    json!(["0", "45", "48", "50", "52"]),
                    "¢",
                    "不做",
                    "纽约 18:00、周一至周四每天重新开盘的第一个 15 分钟（北京冬令时 07:00，夏令时 06:00；周一早上那场不做）：2026 年跌占 58%，近半年 78%",
                ),
                f(
                    "RULE_GOLD_OPEN_LIVE_CENTS",
                    "开盘第一档·盘中：买跌不高于",
                    "number",
                    json!(["0", "60", "70", "80", "90"]),
                    "¢",
                    "盘中不做",
                    "开场后也吃：币安指数比这一场的起点低 0.02% 以上（跌已经领先）时才买；开场后第 1 分钟最高按盘前的价格",
                ),
                f(
                    "RULE_GOLD_0515_CENTS",
                    "纽约 05:15：买跌不高于",
                    "number",
                    json!(["0", "50", "55", "58"]),
                    "¢",
                    "不做",
                    "纽约 05:15–05:30、周一至周五（北京冬令时 18:15，夏令时 17:15）：伦敦上午定盘前，2026 年跌占 65%",
                ),
                f(
                    "RULE_GOLD_0500_CENTS",
                    "纽约 05:00：买涨不高于",
                    "number",
                    json!(["0", "50", "52", "55"]),
                    "¢",
                    "不做",
                    "纽约 05:00–05:15、周一至周五（北京冬令时 18:00）：涨占 62%；按全部时段扫出来的，建议先只记录",
                ),
                f(
                    "RULE_GOLD_1645_CENTS",
                    "休市前一档：买涨不高于",
                    "number",
                    json!(["0", "50", "52", "55"]),
                    "¢",
                    "不做",
                    "纽约 16:45–17:00、周一至周五（北京冬令时 05:45）：涨占 60%（周二只有 44%）；建议先只记录",
                ),
            ],
        ),
        (
            "原油",
            vec![
                f(
                    "RULE_OIL",
                    "原油规则",
                    "choice",
                    onoff(),
                    "",
                    "",
                    "on = 做原油的规则（价格设为 0 的那条不做）；off = 原油一条都不做",
                ),
                f(
                    "RULE_OIL_SHARES",
                    "每场最多买多少份",
                    "number",
                    json!(["10", "20", "50", "100", "200", "500"]),
                    "份",
                    "",
                    "1～100000：原油一个 15 分钟场次最多买这么多份",
                ),
                f(
                    "RULE_OIL_OPEN_CENTS",
                    "开盘第一档：买跌不高于",
                    "number",
                    json!(["0", "45", "50", "55"]),
                    "¢",
                    "不做",
                    "纽约 18:00、周一至周四重新开盘的第一个 15 分钟（北京冬令时 07:00）：2026 年跌占 59%，50¢ 每份约赚 8¢",
                ),
                f(
                    "RULE_OIL_SHUT_CENTS",
                    "休市 4 档：买跌不高于",
                    "number",
                    json!(["0", "35", "40", "45"]),
                    "¢",
                    "不做",
                    "纽约 17:00–17:45、周一至周四：指数不动时按涨结算，跌只占 42%，默认不做",
                ),
                f(
                    "RULE_OIL_0045_CENTS",
                    "纽约 00:45：买跌不高于",
                    "number",
                    json!(["0", "45", "50", "52"]),
                    "¢",
                    "不做",
                    "纽约 00:45–01:00、周一至周五（北京冬令时 13:45）：跌占 62%；显著性刚到边，建议先只记录",
                ),
            ],
        ),
        (
            "额度与时段",
            vec![
                f(
                    "RULE_MAX_DAY_USD",
                    "每天最多花多少",
                    "number",
                    json!(["0", "50", "100", "200", "500", "1000"]),
                    "$",
                    "不限",
                    "黄金、原油合计，一天（北京时间）下单的金额（成交的按成交价、还没结果的按限价全额）到这个数就不再买",
                ),
                f(
                    "RULE_MAX_DAILY_LOSS_USD",
                    "今日亏损停机线",
                    "number",
                    json!(["0", "50", "100", "200", "500"]),
                    "$",
                    "不限",
                    "本服务今天已结算的亏损到这个数就停止开新单（明天自动恢复；在“现在”一栏点恢复会从当时的盈亏起再给一个额度）",
                ),
                f(
                    "RULE_FAST_HOURS",
                    "每秒检查时段（北京时间）",
                    "text",
                    json!(["off", "01:00-07:00", "00:00-07:00", "00:55-02:00", "00:00-24:00"]),
                    "",
                    "",
                    "写成 01:00-07:00（可写多段用逗号分开，结束早于开始 = 跨过半夜，off = 不做）：已发布的规则场次每秒看一次、每秒 REST 核对推送、预先签好单；开始前 1 分钟先准备好登录和授权",
                ),
                f(
                    "RULE_LEAD_MINUTES",
                    "对手提前多少分钟挂单",
                    "number",
                    json!(["0", "240", "300", "360"]),
                    "分钟",
                    "不按这个时刻",
                    "默认 300（5 小时）：有人在开场前这么久把单挂出来。每场在这个时刻的前 1 分钟到后 15 分钟每秒检查、预先签好单，前 10 秒到后 2 分钟每秒 3 次 REST 核对",
                ),
            ],
        ),
    ]
}

pub fn field(key: &str) -> Option<Field> {
    groups().into_iter().flat_map(|(_, fs)| fs).find(|f| f.key == key)
}

pub fn label(key: &str) -> String {
    field(key).map(|f| f.label.to_string()).unwrap_or_else(|| key.to_string())
}

pub type Saved = IndexMap<String, String>;

/// The submitted values as the settings file would write them (strings, trimmed); a key that is not here is refused.
pub fn clean(values: &serde_json::Map<String, Value>) -> Result<Saved, String> {
    let mut out = Saved::new();
    for (key, value) in values {
        let Some(fd) = field(key) else { return Err(format!("{key} 不能在控制台里改")) };
        let text = match value {
            Value::Null => String::new(),
            Value::String(s) => s.trim().to_string(),
            other => crate::util::py_str(other).trim().to_string(),
        };
        if fd.kind == "choice" {
            let options: Vec<String> = fd
                .presets
                .as_array()
                .map(|a| a.iter().filter_map(|o| o[0].as_str().map(String::from)).collect())
                .unwrap_or_default();
            if !options.contains(&text.to_lowercase()) {
                return Err(format!("{} 只能是 {}", fd.label, options.join(" / ")));
            }
            out.insert(key.clone(), text.to_lowercase());
        } else {
            out.insert(key.clone(), text);
        }
    }
    Ok(out)
}

/// The rule settings and the day's loss cap with the saved values over the settings file's; an error when they do not parse.
pub fn build(base: &Env, saved: &Saved, mode: &str) -> Result<(RuleSettings, f64), String> {
    let mut merged = base.clone();
    for (k, v) in saved {
        merged.insert(k.clone(), v.clone());
    }
    let settings = RuleSettings::parse(&merged, Some(mode))?;
    let cap = bounded(&merged, "RULE_MAX_DAILY_LOSS_USD", 0.0, 0.0, 1_000_000.0, false)?;
    Ok((settings, cap))
}

/// At a start: the saved values that still parse over this settings file (it may have changed since); the others dropped one by
/// one, named in the list returned.
pub fn settle_saved(base: &Env, saved: &Saved, mode: &str) -> Result<(RuleSettings, f64, Saved, Vec<String>), String> {
    let mut keep = saved.clone();
    let mut dropped = vec![];
    loop {
        match build(base, &keep, mode) {
            Ok((settings, cap)) => return Ok((settings, cap, keep, dropped)),
            Err(error) => {
                if keep.is_empty() {
                    return Err(error); // (the settings file itself does not parse)
                }
                let bad = keep.iter().find(|(k, v)| {
                    let mut one = Saved::new();
                    one.insert((*k).clone(), (*v).clone());
                    build(base, &one, mode).is_err()
                });
                match bad.map(|(k, _)| k.clone()) {
                    Some(k) => {
                        dropped.push(k.clone());
                        keep.shift_remove(&k);
                    }
                    None => {
                        dropped.extend(keep.keys().cloned()); // (together they fail, each alone parses: all dropped)
                        keep.clear();
                    }
                }
            }
        }
    }
}

/// Every field's value in force, written as the settings file would.
pub fn current(settings: &RuleSettings, cap: f64) -> IndexMap<String, String> {
    let mut out = IndexMap::new();
    for (key, name) in RULE_SETTING_KEYS {
        out.insert(name.to_string(), settings.price(key).to_string());
    }
    for (key, name) in RULE_LIVE_KEYS {
        out.insert(name.to_string(), settings.live_price(key).to_string());
    }
    out.insert("RULE_GOLD".into(), if settings.on("gold") { "on" } else { "off" }.into());
    out.insert("RULE_OIL".into(), if settings.on("oil") { "on" } else { "off" }.into());
    out.insert("RULE_GOLD_SHARES".into(), settings.shares_of("gold").to_string());
    out.insert("RULE_OIL_SHARES".into(), settings.shares_of("oil").to_string());
    out.insert("RULE_MAX_DAY_USD".into(), g(settings.max_day_usd));
    out.insert("RULE_MAX_DAILY_LOSS_USD".into(), g(cap));
    out.insert("RULE_FAST_HOURS".into(), day_spans_text(&settings.fast));
    out.insert("RULE_LEAD_MINUTES".into(), settings.lead_minutes.to_string());
    out
}

/// The page's settings: each group's fields, with the value in force and whether it is the page's or the settings file's.
pub fn view(settings: &RuleSettings, cap: f64, saved: &Saved) -> Value {
    let now = current(settings, cap);
    Value::Array(
        groups()
            .into_iter()
            .map(|(name, fields)| {
                json!({"group": name, "fields": fields.into_iter().map(|fd| json!({
                    "key": fd.key, "label": fd.label, "kind": fd.kind, "presets": fd.presets, "unit": fd.unit, "zero": fd.zero,
                    "help": fd.help, "value": now.get(fd.key).cloned().unwrap_or_default(), "saved": saved.contains_key(fd.key),
                })).collect::<Vec<_>>()})
            })
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(pairs: &[(&str, &str)]) -> Env {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    #[test]
    fn saved_values() {
        let base = env(&[("RULE_GOLD_SHARES", "50")]);
        let mut saved = Saved::new();
        saved.insert("RULE_GOLD_SHUT_CENTS".into(), "60".into());
        saved.insert("RULE_OIL_SHARES".into(), "0".into()); // (not 1～100000: dropped)
        let (s, cap, keep, dropped) = settle_saved(&base, &saved, "paper").unwrap();
        assert_eq!(s.price("gold_shut"), 60);
        assert_eq!(cap, 0.0);
        assert_eq!(dropped, vec!["RULE_OIL_SHARES".to_string()]);
        assert!(keep.contains_key("RULE_GOLD_SHUT_CENTS"));
        let mut v = serde_json::Map::new();
        v.insert("RULE_GOLD".into(), json!("ON"));
        assert_eq!(clean(&v).unwrap()["RULE_GOLD"], "on");
        v.insert("PREDICT_PRIVATE_KEY".into(), json!("x"));
        assert!(clean(&v).is_err());
        let cur = current(&s, 0.0);
        assert_eq!(cur["RULE_FAST_HOURS"], "01:00-07:00");
        assert_eq!(cur["RULE_MAX_DAY_USD"], "0");
        assert_eq!(view(&s, 0.0, &keep).as_array().unwrap().len(), 3);
    }
}
