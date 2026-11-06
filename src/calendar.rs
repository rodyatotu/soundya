//! Google Calendar (or any calendar with an iCal link) without OAuth: the
//! user pastes the calendar's «Секретный адрес в формате iCal» (Google
//! Calendar → Настройки календаря → Интеграция календаря). Read-only. The
//! address is a secret: it stays in config.json, is masked on the page and cut
//! out of bug reports. Events go into «какие планы на сегодня» and are said
//! N minutes before they start.
//!
//! Times: UTC («…Z») exactly; with a TZID or floating — as the speaker's own
//! offset (settings.tz_offset), since there is no time-zone database here.
//! For a calendar in the speaker's own zone (the usual case) this is exact.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::thread;
use std::time::Duration;

use serde_json::{json, Value};

use crate::planner::{civil, days_from_civil, local, now, offset, weekday};

#[derive(Debug, Clone, PartialEq)]
pub struct Event {
    pub uid: String,
    pub title: String,
    /// Unix time UTC of the start (all-day: local midnight).
    pub start: i64,
    pub all_day: bool,
}

/// A VEVENT as read, before repeats are unfolded.
#[derive(Debug, Clone, Default)]
struct Raw {
    uid: String,
    title: String,
    start: i64,
    all_day: bool,
    rrule: String,
    exdates: Vec<i64>,
    /// RECURRENCE-ID: this one replaces that occurrence of `uid`.
    recurrence: Option<i64>,
    cancelled: bool,
}

#[derive(Default)]
struct State {
    url: String,
    on: bool,
    remind_min: u16,
    raws: Vec<Raw>,
    error: String,
    fetched: i64,
    said: HashSet<(String, i64)>,
}

static STATE: Mutex<Option<State>> = Mutex::new(None);
static DIR: Mutex<Option<PathBuf>> = Mutex::new(None);
static WAKE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

fn with<T>(f: impl FnOnce(&mut State) -> T) -> T {
    let mut g = STATE.lock().unwrap();
    f(g.get_or_insert_with(State::default))
}

pub fn configure(on: bool, url: &str, remind_min: u16) {
    let changed = with(|s| {
        let changed = s.url != url || (on && !s.on);
        s.on = on && !url.is_empty();
        s.url = url.to_owned();
        s.remind_min = remind_min.min(180);
        if url.is_empty() {
            s.raws.clear();
            s.error.clear();
        }
        changed
    });
    if changed {
        WAKE.store(true, std::sync::atomic::Ordering::Release);
    }
}

/// Fetch again now.
pub fn wake() {
    WAKE.store(true, std::sync::atomic::Ordering::Release);
}

/// On and with something loaded: «какие планы» may answer from the calendar.
pub fn active() -> bool {
    with(|s| s.on)
}

/// `https://`, `http://` or `webcal://` (Apple and Google both give these).
pub fn valid_url(url: &str) -> bool {
    let u = url.trim();
    (u.starts_with("https://") || u.starts_with("http://") || u.starts_with("webcal://")) && u.len() < 2000 && !u.contains(char::is_whitespace)
}

// ---------------------------------------------------------------- parsing

/// RFC 5545 §3.1: lines starting with a space or a tab continue the previous one.
fn unfold(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for line in text.split('\n') {
        let line = line.strip_suffix('\r').unwrap_or(line);
        if let Some(rest) = line.strip_prefix(' ').or_else(|| line.strip_prefix('\t')) {
            if let Some(last) = out.last_mut() {
                last.push_str(rest);
                continue;
            }
        }
        out.push(line.to_owned());
    }
    out
}

fn unescape(v: &str) -> String {
    let mut out = String::with_capacity(v.len());
    let mut it = v.chars();
    while let Some(c) = it.next() {
        if c == '\\' {
            match it.next() {
                Some('n' | 'N') => out.push(' '),
                Some(x) => out.push(x),
                None => {}
            }
        } else {
            out.push(c);
        }
    }
    out.trim().to_owned()
}

/// «20261106T150000Z», «20261106T150000» (+TZID or floating), «20261106» (VALUE=DATE).
/// Returns (unix time, all-day).
fn parse_time(params: &str, v: &str, off: i64) -> Option<(i64, bool)> {
    let v = v.trim();
    let num = |a: usize, b: usize| v.get(a..b).and_then(|s| s.parse::<i64>().ok());
    let (y, m, d) = (num(0, 4)?, num(4, 6)?, num(6, 8)?);
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return None;
    }
    let day = days_from_civil(y, m as u32, d as u32);
    if v.len() == 8 || params.contains("VALUE=DATE") && !params.contains("VALUE=DATE-TIME") {
        return Some((day * 86_400 - off, true));
    }
    if v.as_bytes().get(8) != Some(&b'T') {
        return None;
    }
    let (h, mi, s) = (num(9, 11)?, num(11, 13)?, num(13, 15).unwrap_or(0));
    let t = day * 86_400 + h * 3600 + mi * 60 + s;
    Some(if v.ends_with('Z') { (t, false) } else { (t - off, false) })
}

fn parse(text: &str, off: i64) -> Vec<Raw> {
    let mut out = Vec::new();
    let mut cur: Option<Raw> = None;
    let mut depth = 0; // nested blocks inside a VEVENT (VALARM)
    for line in unfold(text) {
        let Some((head, value)) = line.split_once(':') else { continue };
        let (name, params) = head.split_once(';').unwrap_or((head, ""));
        let name = name.to_ascii_uppercase();
        match (name.as_str(), value.trim()) {
            ("BEGIN", "VEVENT") => {
                cur = Some(Raw::default());
                depth = 0;
                continue;
            }
            ("END", "VEVENT") => {
                if let Some(r) = cur.take() {
                    if r.start != 0 && !r.cancelled {
                        out.push(r);
                    } else if r.cancelled && r.recurrence.is_some() {
                        // A cancelled occurrence of a series: keep it to hide that one.
                        out.push(r);
                    }
                }
                continue;
            }
            ("BEGIN", _) if cur.is_some() => depth += 1,
            ("END", _) if cur.is_some() => depth -= 1,
            _ => {}
        }
        let Some(r) = cur.as_mut() else { continue };
        if depth > 0 {
            continue;
        }
        match name.as_str() {
            "UID" => r.uid = value.trim().to_owned(),
            "SUMMARY" => r.title = unescape(value),
            "DTSTART" => {
                if let Some((t, all)) = parse_time(params, value, off) {
                    r.start = t;
                    r.all_day = all;
                }
            }
            "RRULE" => r.rrule = value.trim().to_ascii_uppercase(),
            "EXDATE" => {
                for v in value.split(',') {
                    if let Some((t, _)) = parse_time(params, v, off) {
                        r.exdates.push(t);
                    }
                }
            }
            "RECURRENCE-ID" => r.recurrence = parse_time(params, value, off).map(|x| x.0),
            "STATUS" => r.cancelled = value.trim().eq_ignore_ascii_case("CANCELLED"),
            _ => {}
        }
    }
    for r in &mut out {
        if r.title.is_empty() {
            r.title = "Событие".into();
        }
    }
    out
}

fn rule<'a>(rrule: &'a str, key: &str) -> Option<&'a str> {
    rrule.split(';').find_map(|p| p.split_once('=').filter(|(k, _)| *k == key).map(|(_, v)| v))
}

/// Occurrences of `r` starting in [from, to).
fn occurrences(r: &Raw, from: i64, to: i64, off: i64) -> Vec<i64> {
    if r.rrule.is_empty() {
        return if r.start >= from && r.start < to { vec![r.start] } else { vec![] };
    }
    let freq = rule(&r.rrule, "FREQ").unwrap_or("");
    let interval = rule(&r.rrule, "INTERVAL").and_then(|v| v.parse::<i64>().ok()).unwrap_or(1).max(1);
    let count = rule(&r.rrule, "COUNT").and_then(|v| v.parse::<usize>().ok());
    let until = rule(&r.rrule, "UNTIL").and_then(|v| parse_time("", v, off)).map(|x| x.0);
    const DAYS: [&str; 7] = ["MO", "TU", "WE", "TH", "FR", "SA", "SU"];
    let byday: Vec<i64> = rule(&r.rrule, "BYDAY")
        .map(|v| v.split(',').filter_map(|d| DAYS.iter().position(|x| d.ends_with(x)).map(|i| i as i64)).collect())
        .unwrap_or_default();
    let (day0, secs) = local(r.start, off);
    let (y0, m0, d0) = civil(day0);
    let mut out = Vec::new();
    let mut n = 0usize;
    // Walk the series from its start; bounded so a broken rule cannot spin.
    for step in 0..5000i64 {
        let days: Vec<i64> = match freq {
            "DAILY" => vec![day0 + step * interval],
            "WEEKLY" => {
                let week = day0 - weekday(day0) + step * 7 * interval;
                if byday.is_empty() {
                    vec![day0 + step * 7 * interval]
                } else {
                    let mut v: Vec<i64> = byday.iter().map(|w| week + w).filter(|d| *d >= day0).collect();
                    v.sort_unstable();
                    v
                }
            }
            "MONTHLY" => {
                let months = i64::from(m0) - 1 + step * interval;
                let (y, m) = (y0 + months.div_euclid(12), (months.rem_euclid(12) + 1) as u32);
                // The 31st in a 30-day month is skipped, as RFC 5545 says.
                let d = days_from_civil(y, m, d0);
                if civil(d).2 == d0 { vec![d] } else { vec![] }
            }
            "YEARLY" => {
                let d = days_from_civil(y0 + step * interval, m0, d0);
                if civil(d).2 == d0 { vec![d] } else { vec![] }
            }
            _ => return if r.start >= from && r.start < to { vec![r.start] } else { vec![] },
        };
        for d in days {
            let t = d * 86_400 + secs - off;
            if until.is_some_and(|u| t > u) || count.is_some_and(|c| n >= c) || t >= to {
                return out;
            }
            n += 1;
            if t >= from && !r.exdates.contains(&t) {
                out.push(t);
            }
        }
    }
    out
}

/// Events starting in [from, to), sorted, with moved/cancelled occurrences applied.
fn expand(raws: &[Raw], from: i64, to: i64, off: i64) -> Vec<Event> {
    let overrides: HashSet<(String, i64)> = raws.iter().filter_map(|r| r.recurrence.map(|t| (r.uid.clone(), t))).collect();
    let mut out = Vec::new();
    for r in raws {
        if r.recurrence.is_some() {
            if !r.cancelled && r.start >= from && r.start < to {
                out.push(Event { uid: r.uid.clone(), title: r.title.clone(), start: r.start, all_day: r.all_day });
            }
            continue;
        }
        for t in occurrences(r, from, to, off) {
            if !overrides.contains(&(r.uid.clone(), t)) {
                out.push(Event { uid: r.uid.clone(), title: r.title.clone(), start: t, all_day: r.all_day });
            }
        }
    }
    out.sort_by(|a, b| (a.start, !a.all_day, &a.title).cmp(&(b.start, !b.all_day, &b.title)));
    out
}

/// Events of the local day `today + day` (for the planner's agenda).
pub fn day_events(day: i64) -> Vec<Event> {
    let (n, off) = (now(), offset());
    let (today, _) = local(n, off);
    let from = (today + day) * 86_400 - off;
    with(|s| if s.on { expand(&s.raws, from, from + 86_400, off) } else { vec![] })
}

pub fn json() -> Value {
    let (n, off) = (now(), offset());
    with(|s| {
        let next = if s.on { expand(&s.raws, n - 3600, n + 7 * 86_400, off) } else { vec![] };
        json!({
            "on": s.on,
            "events": s.raws.len(),
            "error": s.error,
            "fetched": s.fetched,
            "remind_min": s.remind_min,
            "next": next.iter().take(8).map(|e| json!({
                "title": e.title,
                "start": e.start,
                "all_day": e.all_day,
                "when": if e.all_day { crate::planner::say_time(e.start, n, off).replace(" в 0:00", "").replace("в 0:00", "сегодня") + ", весь день" } else { crate::planner::say_time(e.start, n, off) },
            })).collect::<Vec<_>>(),
        })
    })
}

// ---------------------------------------------------------------- fetching

fn cache(dir: &Path) -> PathBuf {
    dir.join("calendar.ics")
}

fn fetch(shared: &crate::Shared, url: &str) -> Result<String, String> {
    let url = url.trim().replacen("webcal://", "https://", 1);
    let proxy = shared.proxy.lock().unwrap().clone();
    let agent = crate::sc::build_agent(&proxy).map_err(|e| e.to_string())?;
    let resp = agent.get(&url).timeout(Duration::from_secs(30)).call().map_err(|e| match e {
        ureq::Error::Status(404, _) => "адрес не найден (404) — скопируйте секретный адрес заново".to_owned(),
        ureq::Error::Status(c, _) => format!("сервер календаря ответил {c}"),
        // The URL itself is a secret: never in the message.
        ureq::Error::Transport(t) => format!("нет связи с сервером календаря ({})", t.kind()),
    })?;
    let mut body = String::new();
    std::io::Read::read_to_string(&mut std::io::Read::take(resp.into_reader(), 4 << 20), &mut body).map_err(|e| e.to_string())?;
    if !body.contains("BEGIN:VCALENDAR") {
        return Err("по адресу не календарь iCal (нужен «Секретный адрес в формате iCal»)".into());
    }
    Ok(body)
}

fn refresh(shared: &crate::Shared) {
    let (on, url) = with(|s| (s.on, s.url.clone()));
    if !on || url.is_empty() {
        return;
    }
    match fetch(shared, &url) {
        Ok(body) => {
            let raws = parse(&body, offset());
            eprintln!("calendar: {} events", raws.len());
            if let Some(dir) = DIR.lock().unwrap().clone() {
                let _ = crate::settings::write_synced(&cache(&dir), body.as_bytes());
            }
            with(|s| {
                s.raws = raws;
                s.error.clear();
                s.fetched = now();
            });
        }
        Err(e) => {
            eprintln!("calendar: {e}");
            with(|s| s.error = e);
        }
    }
}

/// Loads the last copy, refreshes every 15 minutes, says reminders.
pub fn run(dir: &Path, shared: std::sync::Arc<crate::Shared>) {
    *DIR.lock().unwrap() = Some(dir.to_owned());
    if let Some(body) = crate::fsx::read_string(&cache(dir)) {
        let raws = parse(&body, offset());
        with(|s| s.raws = raws);
    }
    thread::spawn(move || {
        let mut last: Option<std::time::Instant> = None;
        loop {
            if WAKE.swap(false, std::sync::atomic::Ordering::AcqRel) || last.is_none_or(|t| t.elapsed() > Duration::from_secs(900)) {
                last = Some(std::time::Instant::now());
                refresh(&shared);
            }
            remind();
            thread::sleep(Duration::from_secs(15));
        }
    });
}

fn remind() {
    let (n, off) = (now(), offset());
    let due: Vec<(Event, i64)> = with(|s| {
        if !s.on || s.remind_min == 0 {
            return vec![];
        }
        let lead = i64::from(s.remind_min) * 60;
        let soon: Vec<Event> =
            expand(&s.raws, n, n + lead + 1, off).into_iter().filter(|e| !e.all_day && !s.said.contains(&(e.uid.clone(), e.start))).collect();
        for e in &soon {
            s.said.insert((e.uid.clone(), e.start));
        }
        if s.said.len() > 500 {
            s.said.clear();
        }
        soon.into_iter().map(|e| (e.clone(), e.start - n)).collect()
    });
    for (e, left) in due {
        let mins = (left + 30) / 60;
        let text = if mins <= 1 { format!("Сейчас начинается: {}", e.title) } else { format!("Через {} минут: {}", mins, e.title) };
        eprintln!("calendar: {text}");
        crate::display::notify("rem");
        crate::alice::say(text);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MSK: i64 = 3 * 3600;
    // 2026-11-06 00:00 MSK (a Friday).
    const DAY: i64 = 1_793_912_400;

    const ICS: &str = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//Google Inc//Google Calendar 70.9054//EN\r\n\
BEGIN:VEVENT\r\nDTSTART:20261106T120000Z\r\nDTEND:20261106T130000Z\r\nUID:one@google.com\r\nSUMMARY:Встреча с \r\n командой\\, важно\r\n\
BEGIN:VALARM\r\nACTION:DISPLAY\r\nSUMMARY:не это\r\nEND:VALARM\r\nEND:VEVENT\r\n\
BEGIN:VEVENT\r\nDTSTART;TZID=Europe/Moscow:20261102T090000\r\nRRULE:FREQ=WEEKLY;BYDAY=MO,WE,FR;UNTIL=20261231T000000Z\r\n\
EXDATE;TZID=Europe/Moscow:20261111T090000\r\nUID:standup@google.com\r\nSUMMARY:Планёрка\r\nEND:VEVENT\r\n\
BEGIN:VEVENT\r\nDTSTART;TZID=Europe/Moscow:20261113T100000\r\nRECURRENCE-ID;TZID=Europe/Moscow:20261113T090000\r\n\
UID:standup@google.com\r\nSUMMARY:Планёрка (перенесена)\r\nEND:VEVENT\r\n\
BEGIN:VEVENT\r\nDTSTART;VALUE=DATE:20261107\r\nDTEND;VALUE=DATE:20261108\r\nUID:bd@google.com\r\nSUMMARY:День рождения Маши\r\n\
RRULE:FREQ=YEARLY\r\nEND:VEVENT\r\n\
BEGIN:VEVENT\r\nDTSTART:20261106T150000Z\r\nUID:gone@google.com\r\nSUMMARY:Отменили\r\nSTATUS:CANCELLED\r\nEND:VEVENT\r\n\
BEGIN:VEVENT\r\nDTSTART;TZID=Europe/Moscow:20261101T200000\r\nRRULE:FREQ=DAILY;COUNT=3\r\nUID:pill@google.com\r\nSUMMARY:Таблетка\r\nEND:VEVENT\r\n\
END:VCALENDAR\r\n";

    fn titles(from: i64, days: i64) -> Vec<String> {
        expand(&parse(ICS, MSK), from, from + days * 86_400, MSK).into_iter().map(|e| e.title).collect()
    }

    #[test]
    fn reads_google_ics() {
        let raws = parse(ICS, MSK);
        assert_eq!(raws.len(), 5, "cancelled single event dropped");
        let one = raws.iter().find(|r| r.uid == "one@google.com").unwrap();
        assert_eq!(one.title, "Встреча с командой, важно", "folded, unescaped, VALARM ignored");
        assert_eq!(one.start, DAY + 15 * 3600, "12:00Z = 15:00 MSK");
        assert_eq!(parse_time("TZID=Europe/Moscow", "20261106T150000", MSK), Some((DAY + 15 * 3600, false)));
        assert_eq!(parse_time("VALUE=DATE", "20261107", MSK), Some((DAY + 86_400, true)));
        assert!(valid_url("https://calendar.google.com/calendar/ical/x%40gmail.com/private-abc/basic.ics"));
        assert!(valid_url("webcal://p01-caldav.icloud.com/published/2/abc"));
        assert!(!valid_url("calendar.google.com"));
    }

    #[test]
    fn repeats_exceptions_and_moves() {
        // Friday 6th: standup 9:00, meeting 15:00.
        assert_eq!(titles(DAY, 1), ["Планёрка", "Встреча с командой, важно"]);
        // Saturday 7th: the birthday (all day), nothing else.
        assert_eq!(titles(DAY + 86_400, 1), ["День рождения Маши"]);
        // Wednesday 11th is excluded; Friday 13th moved to 10:00.
        assert_eq!(titles(DAY + 5 * 86_400, 1), Vec::<String>::new());
        let fri = expand(&parse(ICS, MSK), DAY + 7 * 86_400, DAY + 8 * 86_400, MSK);
        assert_eq!(fri.len(), 1);
        assert_eq!(fri[0].title, "Планёрка (перенесена)");
        assert_eq!(fri[0].start, DAY + 7 * 86_400 + 10 * 3600);
        // COUNT=3 from Nov 1: 1, 2, 3 — none on the 6th.
        let pills = expand(&parse(ICS, MSK), DAY - 6 * 86_400, DAY + 86_400, MSK).into_iter().filter(|e| e.title == "Таблетка").count();
        assert_eq!(pills, 3);
        // Next year's birthday.
        assert_eq!(titles(DAY + 366 * 86_400, 1), ["День рождения Маши"]);
        // A broken rule does not hang or invent events.
        let r = Raw { uid: "x".into(), title: "x".into(), start: DAY, rrule: "FREQ=SECONDLY".into(), ..Raw::default() };
        assert_eq!(occurrences(&r, DAY, DAY + 86_400, MSK), vec![DAY]);
    }
}
