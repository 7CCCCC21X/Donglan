//! New York time and CME Globex's shut stretches (us_eastern_offset, et_wall_ms, cme_shuts), and the Beijing-day spans
//! RULE_FAST_HOURS is written in (day_spans).
//!
//! Gold and oil trade on CME Globex's hours — Sunday 18:00 to Friday 17:00 New York time, shut an hour from 17:00 every day —
//! and the rules' windows are set in New York time from that schedule, so they follow daylight saving. No tz database needed.
use crate::util::{DAY_MS, Date, wall};
use std::cell::RefCell;
use std::collections::{BTreeSet, HashMap};
use std::rc::Rc;
use std::sync::RwLock;

/// CME Globex holiday schedules (override with CME_EARLY_DAYS / CME_CLOSED_DAYS): an early halt of metals and energy (taken as
/// shut from noon), and the days with no session. Listed wide on purpose: a day too many costs an afternoon, a day missing
/// trades a standing index.
pub const DEFAULT_CME_EARLY: &str = "2026-11-26,2026-11-27,2026-12-24,2026-12-31,2027-01-18,2027-02-15,2027-05-31,2027-06-18,2027-07-05,\
2027-09-06,2027-11-25,2027-11-26";
pub const DEFAULT_CME_CLOSED: &str = "2026-12-25,2027-01-01,2027-03-26,2027-12-24";
pub const CME_SHUT_HOUR: u32 = 17;
pub const CME_REOPEN_HOUR: u32 = 18;
pub const CME_EARLY_SHUT_HOUR: u32 = 12;

/// US Eastern UTC offset at an instant: −4 (EDT) from the second Sunday of March 02:00 EST to the first Sunday of November
/// 02:00 EDT, else −5 (EST).
pub fn us_eastern_offset(ms: i64) -> i64 {
    let (date, _, _, _, _) = wall(ms, 0);
    let (year, _, _) = date.parts();
    let sunday = |d: Date| d.add(((6 - d.weekday() as i64) % 7 + 7) % 7);
    let starts = sunday(Date::ymd(year, 3, 8)).0 * DAY_MS + 7 * 3_600_000;
    let ends = sunday(Date::ymd(year, 11, 1)).0 * DAY_MS + 6 * 3_600_000;
    if starts <= ms && ms < ends { -4 } else { -5 }
}

/// US Eastern wall time -> epoch ms; ``utc_offset`` is -4 (EDT) or -5 (EST).
pub fn et_ms(day: Date, hour: u32, minute: u32, utc_offset: i64) -> i64 {
    day.0 * DAY_MS + hour as i64 * 3_600_000 + minute as i64 * 60_000 - utc_offset * 3_600_000
}

/// US Eastern wall time on a date -> epoch ms, with that date's own offset.
pub fn et_wall_ms(day: Date, hour: u32, minute: u32) -> i64 {
    let probe = day.0 * DAY_MS + 17 * 3_600_000;
    et_ms(day, hour, minute, us_eastern_offset(probe))
}

pub fn et_date(ms: i64) -> Date {
    wall(ms, us_eastern_offset(ms) * 3_600_000).0
}

/// (weekday — Monday 0, hour, minute) of an instant in New York, daylight saving followed.
pub fn et_parts(ms: i64) -> (u32, u32, u32) {
    let (d, h, m, _, _) = wall(ms, us_eastern_offset(ms) * 3_600_000);
    (d.weekday(), h, m)
}

/// "17:45 ET" of an instant.
pub fn et_clock(ms: i64) -> String {
    let (_, h, m, _, _) = wall(ms, us_eastern_offset(ms) * 3_600_000);
    format!("{h:02}:{m:02} ET")
}

/// "2026-12-25,2027-01-01" or spans "2026-10-01..2026-10-07" -> the dates; a span written backwards is read the way it means.
pub fn parse_dates(spec: &str, label: &str) -> Result<BTreeSet<Date>, String> {
    let mut days = BTreeSet::new();
    for item in spec.split(',') {
        let item = item.trim();
        if item.is_empty() {
            continue;
        }
        let (start, end) = match item.split_once("..") {
            Some((a, b)) => (a, if b.is_empty() { a } else { b }),
            None => (item, item),
        };
        let bad = || format!("{label} 日期格式应为 YYYY-MM-DD 或 YYYY-MM-DD..YYYY-MM-DD：{item}");
        let (mut first, mut last) = (Date::parse(start).ok_or_else(bad)?, Date::parse(end).ok_or_else(bad)?);
        if first > last {
            std::mem::swap(&mut first, &mut last);
        }
        while first <= last {
            days.insert(first);
            first = first.add(1);
        }
    }
    Ok(days)
}

/// CME's holiday schedules in force.
#[derive(Clone, Debug)]
pub struct Calendar {
    pub cme_early: BTreeSet<Date>,
    pub cme_closed: BTreeSet<Date>,
}

impl Default for Calendar {
    fn default() -> Calendar {
        Calendar {
            cme_early: parse_dates(DEFAULT_CME_EARLY, "CME_EARLY_DAYS").unwrap_or_default(),
            cme_closed: parse_dates(DEFAULT_CME_CLOSED, "CME_CLOSED_DAYS").unwrap_or_default(),
        }
    }
}

impl Calendar {
    pub fn last_day(&self) -> Option<Date> {
        self.cme_early.iter().chain(self.cme_closed.iter()).max().copied()
    }
}

static CALENDAR: RwLock<Option<Calendar>> = RwLock::new(None);
static GENERATION: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

pub fn calendar() -> Calendar {
    CALENDAR.read().ok().and_then(|c| c.clone()).unwrap_or_default()
}

pub fn configure(early: Option<BTreeSet<Date>>, closed: Option<BTreeSet<Date>>) {
    let mut cal = calendar();
    if let Some(e) = early {
        cal.cme_early = e;
    }
    if let Some(c) = closed {
        cal.cme_closed = c;
    }
    if let Ok(mut slot) = CALENDAR.write() {
        *slot = Some(cal);
    }
    GENERATION.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
}

/// The shut stretches the New York date brings, (shut, reopened) in ms: its daily break 17:00–18:00 (a Friday's to Sunday
/// 18:00); on an early halt from CME_EARLY_SHUT_HOUR; on a day without a session the session it would have had, from 18:00
/// the evening before. None on a Saturday or Sunday.
pub fn cme_day_shuts(day: Date, early: bool, closed: bool) -> Vec<(i64, i64)> {
    if day.weekday() > 4 {
        return vec![];
    }
    let close = et_wall_ms(day, CME_SHUT_HOUR, 0);
    let mut out = vec![(close, et_wall_ms(day.add(if day.weekday() == 4 { 2 } else { 0 }), CME_REOPEN_HOUR, 0))];
    if early {
        out.push((et_wall_ms(day, CME_EARLY_SHUT_HOUR, 0), close));
    }
    if closed {
        out.push((et_wall_ms(day.add(-1), CME_REOPEN_HOUR, 0), close));
    }
    out
}

pub fn merge_stretches(mut stretches: Vec<(i64, i64)>) -> Vec<(i64, i64)> {
    stretches.sort();
    let mut out: Vec<(i64, i64)> = Vec::with_capacity(stretches.len());
    for (a, b) in stretches {
        match out.last_mut() {
            Some(last) if a <= last.1 => last.1 = last.1.max(b),
            _ => out.push((a, b)),
        }
    }
    out
}

thread_local! {
    static STRETCHES: RefCell<(u64, HashMap<(i64, i64), Rc<Vec<(i64, i64)>>>)> = RefCell::new((u64::MAX, HashMap::new()));
}

fn cme_stretches(first: i64, last: i64) -> Rc<Vec<(i64, i64)>> {
    let generation = GENERATION.load(std::sync::atomic::Ordering::SeqCst);
    STRETCHES.with(|cell| {
        let mut cache = cell.borrow_mut();
        if cache.0 != generation || cache.1.len() > 512 {
            *cache = (generation, HashMap::new());
        }
        if let Some(hit) = cache.1.get(&(first, last)) {
            return hit.clone();
        }
        let cal = calendar();
        let mut all = Vec::new();
        for o in first..=last {
            let d = Date(o);
            all.extend(cme_day_shuts(d, cal.cme_early.contains(&d), cal.cme_closed.contains(&d)));
        }
        let merged = Rc::new(merge_stretches(all));
        cache.1.insert((first, last), merged.clone());
        merged
    })
}

/// CME Globex's shut stretches that overlap [from_ms, to_ms), as (shut, reopened) in ms.
pub fn cme_shuts(from_ms: i64, to_ms: i64) -> Vec<(i64, i64)> {
    let first = from_ms.div_euclid(DAY_MS) - 5;
    let last = from_ms.max(to_ms - 1).div_euclid(DAY_MS) + 1;
    cme_stretches(first, last).iter().copied().filter(|s| s.0 < to_ms && from_ms < s.1).collect()
}

/// The shut stretches of a card's ``hours`` that overlap [from_ms, to_ms): "cme" (cme_shuts); none otherwise.
pub fn market_shuts(hours: &str, from_ms: i64, to_ms: i64) -> Vec<(i64, i64)> {
    if hours == "cme" { cme_shuts(from_ms, to_ms) } else { vec![] }
}

// --- the Beijing-day spans of RULE_FAST_HOURS ---------------------------------------------------------------------------
pub type Spans = Vec<(u32, u32)>;

fn take_digits(chars: &[char], i: &mut usize, max: usize) -> Option<u32> {
    let start = *i;
    while *i < chars.len() && *i - start < max && chars[*i].is_ascii_digit() {
        *i += 1;
    }
    if *i == start {
        return None;
    }
    chars[start..*i].iter().collect::<String>().parse().ok()
}

/// "01:00-07:00" -> [(60, 420)] (minutes after Beijing midnight); one written as "01-07"; with ":" or "：", any of - – — ~ ～ 至 到
/// between; None when the part is not of that form.
fn span_part(part: &str) -> Option<(u32, u32, u32, u32)> {
    let chars: Vec<char> = part.chars().collect();
    let mut i = 0;
    let h1 = take_digits(&chars, &mut i, 2)?;
    let mut m1 = 0;
    if i < chars.len() && (chars[i] == ':' || chars[i] == '：') {
        i += 1;
        let at = i;
        m1 = take_digits(&chars, &mut i, 2)?;
        if i - at != 2 {
            return None;
        }
    }
    while i < chars.len() && chars[i].is_whitespace() {
        i += 1;
    }
    if i >= chars.len() || !matches!(chars[i], '-' | '–' | '—' | '~' | '～' | '至' | '到') {
        return None;
    }
    i += 1;
    while i < chars.len() && chars[i].is_whitespace() {
        i += 1;
    }
    let h2 = take_digits(&chars, &mut i, 2)?;
    let mut m2 = 0;
    if i < chars.len() && (chars[i] == ':' || chars[i] == '：') {
        i += 1;
        let at = i;
        m2 = take_digits(&chars, &mut i, 2)?;
        if i - at != 2 {
            return None;
        }
    }
    if i != chars.len() {
        return None;
    }
    Some((h1, m1, h2, m2))
}

/// Spans of the Beijing day, as minutes after midnight: "01:00-07:00" -> [(60, 420)]; several, comma-separated; one whose end is
/// before its beginning runs over midnight ("23:00-02:00"); "off" (or nothing): none.
pub fn day_spans(text: &str, name: &str) -> Result<Spans, String> {
    let text = text.trim().to_lowercase();
    if matches!(text.as_str(), "" | "off" | "0" | "关") {
        return Ok(vec![]);
    }
    // (spaces around a span's dash taken out first — "01:00 - 07:00" is one span; spans may still be set apart by spaces)
    let chars: Vec<char> = text.chars().collect();
    let dash = |c: char| matches!(c, '-' | '–' | '—' | '~' | '～' | '至' | '到');
    let mut tight = String::with_capacity(text.len());
    let mut i = 0;
    while i < chars.len() {
        if chars[i].is_whitespace() {
            let mut j = i;
            while j < chars.len() && chars[j].is_whitespace() {
                j += 1;
            }
            if !(tight.chars().last().is_some_and(dash) || (j < chars.len() && dash(chars[j]))) {
                tight.push(' ');
            }
            i = j;
            continue;
        }
        tight.push(chars[i]);
        i += 1;
    }
    let mut out = vec![];
    for part in tight.split(|c: char| matches!(c, ',' | '，' | ';' | '；') || c.is_whitespace()) {
        if part.is_empty() {
            continue;
        }
        let Some((h1, m1, h2, m2)) = span_part(part) else {
            return Err(format!("{name} 写成 01:00-07:00 这样（北京时间，可写多段用逗号分开；off = 不做），现在是“{part}”"));
        };
        let a = h1 * 60 + m1;
        let mut b = h2 * 60 + m2;
        if b == 0 {
            b = 1440; // an end at 00:00 is the day's end
        }
        if !(a < 1440 && b > 0 && b <= 1440 && m1 < 60 && m2 < 60) || a == b {
            return Err(format!("{name} 的时间要在 00:00～24:00 之间，开始和结束不能相同，现在是“{part}”"));
        }
        out.push((a, b));
    }
    Ok(out)
}

pub fn day_spans_text(spans: &[(u32, u32)]) -> String {
    if spans.is_empty() {
        return "off".into();
    }
    spans.iter().map(|(a, b)| format!("{:02}:{:02}-{:02}:{:02}", a / 60, a % 60, b / 60, b % 60)).collect::<Vec<_>>().join(",")
}

pub fn in_spans(spans: &[(u32, u32)], now_ms: i64) -> bool {
    let tod = (now_ms + 8 * 3_600_000).rem_euclid(DAY_MS);
    spans.iter().any(|&(a, b)| {
        let (a, b) = (a as i64 * 60_000, b as i64 * 60_000);
        if a < b { a <= tod && tod < b } else { tod >= a || tod < b }
    })
}

/// The ms until the spans next begin: 0 inside them, None when there are none.
pub fn spans_in(spans: &[(u32, u32)], now_ms: i64) -> Option<i64> {
    if spans.is_empty() {
        return None;
    }
    if in_spans(spans, now_ms) {
        return Some(0);
    }
    let tod = (now_ms + 8 * 3_600_000).rem_euclid(DAY_MS);
    spans.iter().map(|&(a, _)| (a as i64 * 60_000 - tod).rem_euclid(DAY_MS)).min()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn offsets() {
        // 2026-03-08 07:00 UTC is the switch to EDT; 2026-11-01 06:00 UTC back to EST
        let switch = Date::ymd(2026, 3, 8).0 * DAY_MS + 7 * 3_600_000;
        assert_eq!(us_eastern_offset(switch - 1), -5);
        assert_eq!(us_eastern_offset(switch), -4);
        let back = Date::ymd(2026, 11, 1).0 * DAY_MS + 6 * 3_600_000;
        assert_eq!(us_eastern_offset(back - 1), -4);
        assert_eq!(us_eastern_offset(back), -5);
        assert_eq!(et_clock(et_wall_ms(Date::ymd(2026, 11, 18), 17, 45)), "17:45 ET");
    }

    #[test]
    fn spans() {
        assert_eq!(day_spans("01:00-07:00", "X").unwrap(), vec![(60, 420)]);
        // spaces around the dash: one span; spans set apart by spaces: as many
        assert_eq!(day_spans("01:00 - 07:00", "X").unwrap(), vec![(60, 420)]);
        assert_eq!(day_spans(" 01:00 至  07:00 ， 23:00 -02:00 09:00-10:00", "X").unwrap(), vec![(60, 420), (1380, 120), (540, 600)]);
        assert_eq!(day_spans("23:00-02:00，08：30至09:00", "X").unwrap(), vec![(1380, 120), (510, 540)]);
        assert_eq!(day_spans("00:55-02:00", "X").unwrap(), vec![(55, 120)]);
        assert_eq!(day_spans("22-00", "X").unwrap(), vec![(1320, 1440)]);
        assert!(day_spans("off", "X").unwrap().is_empty());
        assert!(day_spans("7点", "X").is_err());
        assert!(day_spans("01:00-01:00", "X").is_err());
        assert_eq!(day_spans_text(&[(55, 120)]), "00:55-02:00");
    }
}
