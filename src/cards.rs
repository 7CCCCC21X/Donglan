//! The 15-minute up/down markets the rules buy (XAU-15M and CL-15M).
//!
//! "黄金 (XAU) 涨或跌 15分" (xau-updown-15m-<window start, Unix seconds>) and "CL 涨或跌 15分" (cl-updown-15m-…): Up when the
//! price at the end of the window is above the one at its start, Down when below — the index price of Binance's USDⓈ-M futures
//! mark price stream. Predict lists a window about a day before its start.
use crate::clock::us_eastern_offset;
use crate::util::{BEIJING_MS, wall};

struct Found {
    text: String,
    h1: u32,
    m1: u32,
    half1: String,
    h2: u32,
    m2: u32,
    half2: String,
}

fn digits_at(c: &[char], i: usize, max: usize) -> Vec<usize> {
    // the lengths \d{1,max} can take at i, the longest first
    let mut n = 0;
    while i + n < c.len() && n < max && c[i + n].is_ascii_digit() {
        n += 1;
    }
    (1..=n).rev().collect()
}

fn num(c: &[char], i: usize, n: usize) -> u32 {
    c[i..i + n].iter().collect::<String>().parse().unwrap_or(0)
}

fn skip_ws(c: &[char], mut i: usize) -> usize {
    while i < c.len() && c[i].is_whitespace() {
        i += 1;
    }
    i
}

fn half_at(c: &[char], i: usize) -> Option<String> {
    if i + 1 < c.len() && matches!(c[i].to_ascii_uppercase(), 'A' | 'P') && c[i + 1].to_ascii_uppercase() == 'M' {
        return Some(format!("{}M", c[i].to_ascii_uppercase()));
    }
    None
}

fn is_word(ch: char) -> bool {
    ch.is_alphanumeric() || ch == '_'
}

/// After "hh:mm": optional spaces, an optional AM/PM, optional spaces — every way, the AM/PM taken first.
fn after_time(c: &[char], i: usize) -> Vec<(usize, String)> {
    let j = skip_ws(c, i);
    let mut out = vec![];
    if let Some(h) = half_at(c, j) {
        out.push((skip_ws(c, j + 2), h));
    }
    out.push((j, String::new()));
    out
}

/// `(\d{1,2}):(\d{2})\s*([AP]M)?\s*[-–—~至到]\s*(\d{1,2}):(\d{2})\s*([AP]M)?\s*(?:ET|EST|EDT)\b`, case-insensitive, the leftmost.
fn find_range(question: &str) -> Option<Found> {
    let c: Vec<char> = question.chars().collect();
    for start in 0..c.len() {
        for n1 in digits_at(&c, start, 2) {
            let mut i = start + n1;
            if i >= c.len() || c[i] != ':' {
                continue;
            }
            i += 1;
            if i + 2 > c.len() || !c[i].is_ascii_digit() || !c[i + 1].is_ascii_digit() {
                continue;
            }
            let (h1, m1) = (num(&c, start, n1), num(&c, i, 2));
            i += 2;
            for (j, half1) in after_time(&c, i) {
                if j >= c.len() || !matches!(c[j], '-' | '–' | '—' | '~' | '至' | '到') {
                    continue;
                }
                let k = skip_ws(&c, j + 1);
                for n2 in digits_at(&c, k, 2) {
                    let mut p = k + n2;
                    if p >= c.len() || c[p] != ':' {
                        continue;
                    }
                    p += 1;
                    if p + 2 > c.len() || !c[p].is_ascii_digit() || !c[p + 1].is_ascii_digit() {
                        continue;
                    }
                    let (h2, m2) = (num(&c, k, n2), num(&c, p, 2));
                    p += 2;
                    for (q, half2) in after_time(&c, p) {
                        for zone in ["ET", "EST", "EDT"] {
                            let z: Vec<char> = zone.chars().collect();
                            if q + z.len() <= c.len()
                                && c[q..q + z.len()].iter().zip(&z).all(|(a, b)| a.to_ascii_uppercase() == *b)
                            {
                                let end = q + z.len();
                                if end < c.len() && is_word(c[end]) {
                                    continue;
                                }
                                return Some(Found {
                                    text: c[start..end].iter().collect(),
                                    h1,
                                    m1,
                                    half1: half1.clone(),
                                    h2,
                                    m2,
                                    half2,
                                });
                            }
                        }
                    }
                }
            }
        }
    }
    None
}

/// A 15-minute market's question names its time range ("… 10:00PM-10:15PM ET"): when it does, it must start where the slug's
/// time was read as the start. "" when it agrees or says no time.
pub fn window_title_text(question: &str, start_ms: i64) -> String {
    let Some(f) = find_range(question) else { return String::new() };
    let wall_min = |h: u32, m: u32, half: &str| -> u32 {
        (if half.is_empty() { h } else { h % 12 + if half == "PM" { 12 } else { 0 } }) * 60 + m
    };
    let mut half1 = f.half1.clone();
    let inherited = half1.is_empty() && !f.half2.is_empty();
    if inherited {
        half1 = f.half2.clone();
    }
    let mut start = wall_min(f.h1, f.m1, &half1);
    let end = wall_min(f.h2, f.m2, &f.half2);
    if inherited && start >= end {
        half1 = if half1 == "PM" { "AM".into() } else { "PM".into() };
        start = wall_min(f.h1, f.m1, &half1);
    }
    let (_, h, m, _, _) = wall(start_ms, us_eastern_offset(start_ms) * 3_600_000);
    if start == h * 60 + m {
        return String::new();
    }
    format!("Predict 市场写的时间段（{}）和按链接推算的窗口（{h:02}:{m:02} ET 起）对不上", f.text)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WindowCard {
    pub key: &'static str,
    pub stem: &'static str,
    pub symbol: &'static str,
    pub coin: &'static str,
    pub minutes: i64,
    pub hours: &'static str,
}

impl WindowCard {
    pub fn window_ms(&self) -> i64 {
        self.minutes * 60_000
    }
    pub fn prefix(&self) -> String {
        format!("{}-updown-{}m-", self.stem, self.minutes)
    }
    pub fn slug_of(&self, start_ms: i64) -> String {
        format!("{}{}", self.prefix(), start_ms.div_euclid(1000))
    }
    pub fn start_of(&self, slug: &str) -> Option<i64> {
        let suffix = slug.strip_prefix(&self.prefix())?;
        if suffix.is_empty() || !suffix.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        suffix.parse::<i64>().ok().map(|s| s * 1000)
    }
    /// The first window start at or after ``ms`` on the card's grid.
    pub fn first_start(&self, ms: i64) -> i64 {
        -((-ms).div_euclid(self.window_ms())) * self.window_ms()
    }
    pub fn next_start(&self, start_ms: i64) -> i64 {
        start_ms + self.window_ms()
    }
    pub fn end_of(&self, start_ms: i64) -> i64 {
        start_ms + self.window_ms()
    }
    pub fn name_of(&self, start_ms: i64) -> String {
        let hm = |ms: i64| {
            let (_, h, m, _, _) = wall(ms, BEIJING_MS);
            format!("{h:02}:{m:02}")
        };
        format!("{} {}分涨跌 {}–{}", self.coin, self.minutes, hm(start_ms), hm(self.end_of(start_ms)))
    }
    pub fn title_problem(&self, question: &str, start_ms: i64) -> String {
        window_title_text(question, start_ms)
    }
}

pub static XAU: WindowCard =
    WindowCard { key: "XAU-15M", stem: "xau", symbol: "XAUUSDT", coin: "黄金", minutes: 15, hours: "cme" };
pub static CL: WindowCard = WindowCard { key: "CL-15M", stem: "cl", symbol: "CLUSDT", coin: "原油", minutes: 15, hours: "cme" };

pub fn card(key: &str) -> &'static WindowCard {
    if key == "CL-15M" { &CL } else { &XAU }
}

pub fn cards() -> [&'static WindowCard; 2] {
    [&XAU, &CL]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::et_wall_ms;
    use crate::util::Date;

    #[test]
    fn slugs() {
        assert_eq!(XAU.slug_of(1_793_942_100_000), "xau-updown-15m-1793942100");
        assert_eq!(XAU.start_of("xau-updown-15m-1793942100"), Some(1_793_942_100_000));
        assert_eq!(XAU.start_of("cl-updown-15m-1793942100"), None);
        assert_eq!(XAU.first_start(1_793_942_100_001), 1_793_943_000_000);
        assert_eq!(XAU.first_start(1_793_942_100_000), 1_793_942_100_000);
    }

    #[test]
    fn titles() {
        let start = et_wall_ms(Date::ymd(2026, 11, 18), 22, 0);
        assert_eq!(window_title_text("Gold Up or Down - Nov 18, 10:00PM-10:15PM ET", start), "");
        assert_eq!(window_title_text("Gold Up or Down - Nov 18, 10:00-10:15PM ET", start), "");
        assert!(!window_title_text("Gold Up or Down - Nov 18, 9:45PM-10:00PM ET", start).is_empty());
        assert_eq!(window_title_text("Gold Up or Down", start), "");
        let noon = et_wall_ms(Date::ymd(2026, 11, 18), 11, 45);
        assert_eq!(window_title_text("XAU 11:45-12:00PM ET", noon), "");
    }
}
