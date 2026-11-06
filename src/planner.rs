//! Soundya's own planner: tasks and reminders by voice and on the page.
//!
//! «Алиса, напомни через 10 минут выключить чайник», «напомни завтра в 9
//! позвонить маме», «напоминай по будням в 8 выпить таблетки», «добавь задачу
//! купить молоко», «какие планы на сегодня», «отметь выполненной купить молоко».
//! Taken from Alice only with «Планировщик soundya» switched on (her own
//! reminders stay hers otherwise). Due reminders are said in Alice's voice;
//! one missed while the speaker was off is said at start if it is under 6 hours
//! late. Stored in <state dir>/planner.json (written like config.json).
//!
//! With a calendar (calendar.rs: iCal, Google via google.rs) it is the whole
//! day: «какие планы на сегодня» / «что нужно сделать» / «расписание» / «что
//! дальше» / «дедлайны» / «на неделю» answer from events, Google Tasks and
//! soundya's own tasks together; deadlines are warned about `plan_warn`
//! minutes before; a morning brief at `brief_at`; «добавь задачу …» and
//! «добавь в календарь …» write to Google when it is connected.
//!
//! Times are local: the speaker's clock is UTC, the offset comes from the
//! settings (the page fills it in from the phone).

use std::{
    path::{Path, PathBuf},
    sync::Mutex,
    thread,
    time::Duration,
};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Task {
    pub id: u64,
    pub title: String,
    /// Unix time, UTC; None = no date.
    #[serde(default)]
    pub due: Option<i64>,
    /// "" | "daily" | "weekdays" | "weekly" | "monthly"
    #[serde(default)]
    pub repeat: String,
    #[serde(default)]
    pub category: String,
    /// "low" | "normal" | "high"
    #[serde(default = "normal")]
    pub priority: String,
    #[serde(default)]
    pub done: bool,
    /// Say it at `due`.
    #[serde(default)]
    pub remind: bool,
    #[serde(default)]
    pub created: i64,
}

fn normal() -> String {
    "normal".into()
}

static TASKS: Mutex<Option<Vec<Task>>> = Mutex::new(None);
static DIR: Mutex<Option<PathBuf>> = Mutex::new(None);
/// Local time offset from UTC, minutes.
static OFFSET: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(180);
pub static ENABLED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub(crate) fn now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

pub fn configure(enabled: bool, offset_min: i32) {
    ENABLED.store(enabled, std::sync::atomic::Ordering::Release);
    OFFSET.store(offset_min.clamp(-720, 840), std::sync::atomic::Ordering::Release);
}

pub(crate) fn offset() -> i64 {
    i64::from(OFFSET.load(std::sync::atomic::Ordering::Acquire)) * 60
}

fn file(dir: &Path) -> PathBuf {
    dir.join("planner.json")
}

fn with<T>(f: impl FnOnce(&mut Vec<Task>) -> T) -> T {
    let mut g = TASKS.lock().unwrap();
    let list = g.get_or_insert_with(Vec::new);
    f(list)
}

fn save() {
    let Some(dir) = DIR.lock().unwrap().clone() else { return };
    let data = with(|l| serde_json::to_vec_pretty(l).unwrap_or_default());
    let tmp = dir.join("planner.json.tmp");
    if crate::settings::write_synced(&tmp, &data).is_ok() {
        let _ = std::fs::rename(tmp, file(&dir));
    }
}

// ---------------------------------------------------------------- dates

/// Days since 1970-01-01 → (year, month, day).
pub(crate) fn civil(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

pub(crate) fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let mp = (i64::from(m) + 9) % 12;
    let doy = (153 * mp + 2) / 5 + i64::from(d) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Local day number and seconds into it.
pub(crate) fn local(t: i64, off: i64) -> (i64, i64) {
    let l = t + off;
    (l.div_euclid(86_400), l.rem_euclid(86_400))
}

/// Monday = 0.
pub(crate) fn weekday(day: i64) -> i64 {
    (day + 3).rem_euclid(7)
}

/// The next time of a repeating task after `due`.
pub fn next_due(due: i64, repeat: &str, off: i64) -> Option<i64> {
    let (day, secs) = local(due, off);
    let at = |d: i64| d * 86_400 + secs - off;
    Some(match repeat {
        "daily" => at(day + 1),
        "weekly" => at(day + 7),
        "weekdays" => {
            let mut d = day + 1;
            while weekday(d) >= 5 {
                d += 1;
            }
            at(d)
        }
        "monthly" => {
            let (y, m, dd) = civil(day);
            let (y2, m2) = if m == 12 { (y + 1, 1) } else { (y, m + 1) };
            let dim = (days_from_civil(if m2 == 12 { y2 + 1 } else { y2 }, if m2 == 12 { 1 } else { m2 + 1 }, 1) - days_from_civil(y2, m2, 1)) as u32;
            at(days_from_civil(y2, m2, dd.min(dim)))
        }
        _ => return None,
    })
}

// ---------------------------------------------------------------- Russian

fn num(w: &str) -> Option<i64> {
    if let Ok(n) = w.parse::<i64>() {
        return Some(n);
    }
    Some(match w {
        "одну" | "одна" | "один" | "час" => 1,
        "две" | "два" => 2,
        "три" => 3,
        "четыре" => 4,
        "пять" => 5,
        "шесть" => 6,
        "семь" => 7,
        "восемь" => 8,
        "девять" => 9,
        "десять" => 10,
        "пятнадцать" => 15,
        "двадцать" => 20,
        "тридцать" => 30,
        "сорок" => 40,
        _ => return None,
    })
}

const WEEKDAYS: [&str; 7] = ["понедельник", "вторник", "сред", "четверг", "пятниц", "суббот", "воскресень"];

/// When + what from a phrase after «напомни»: (due, repeat, title).
/// None when no time is said.
pub fn parse_when(text: &str, now: i64, off: i64) -> Option<(i64, String, String)> {
    parse_when_full(text, now, off).map(|w| (w.due, w.repeat, w.title))
}

/// What was said about time, in full.
#[derive(Debug, Clone, PartialEq)]
pub struct When {
    pub due: i64,
    pub repeat: String,
    pub title: String,
    /// A clock time or «через …» was said (else only a day: 9:00 is assumed).
    pub has_time: bool,
}

const MONTHS_GEN: [&str; 12] = ["январ", "феврал", "март", "апрел", "ма", "июн", "июл", "август", "сентябр", "октябр", "ноябр", "декабр"];

/// «12 марта», «1 мая»: the month after a day number.
fn month(w: &str) -> Option<u32> {
    if w == "мая" || w == "май" {
        return Some(5);
    }
    MONTHS_GEN.iter().position(|m| *m != "ма" && w.starts_with(m)).map(|i| i as u32 + 1)
}

pub fn parse_when_full(text: &str, now: i64, off: i64) -> Option<When> {
    let words: Vec<&str> = text.split_whitespace().collect();
    let (today, now_secs) = local(now, off);
    let mut used = vec![false; words.len()];
    let mut day: Option<i64> = None;
    let mut secs: Option<i64> = None;
    let mut repeat = String::new();
    let mut relative: Option<i64> = None;
    let mut i = 0;
    while i < words.len() {
        let w = words[i];
        let take = |n: usize, used: &mut Vec<bool>| (i..(i + n).min(words.len())).for_each(|k| used[k] = true);
        match w {
            "через" => {
                // «через 10 минут», «через час», «через полчаса», «через 2 часа 30 минут»
                let mut total = 0i64;
                let mut j = i + 1;
                while j < words.len() {
                    let (n, unit_at) = match num(words[j]) {
                        Some(n) if words[j] != "час" => (n, j + 1),
                        _ => (1, j),
                    };
                    let unit = words.get(unit_at).copied().unwrap_or("");
                    let mult = if unit.starts_with("минут") || unit == "мин" {
                        60
                    } else if unit.starts_with("час") {
                        3600
                    } else if unit.starts_with("полчас") {
                        total += 1800;
                        j = unit_at + 1;
                        continue;
                    } else if unit.starts_with("секунд") {
                        1
                    } else if unit.starts_with("день") || unit.starts_with("дня") || unit.starts_with("дней") {
                        86_400
                    } else if unit.starts_with("недел") {
                        7 * 86_400
                    } else {
                        break;
                    };
                    total += n * mult;
                    j = unit_at + 1;
                    if words.get(j) == Some(&"и") {
                        j += 1;
                    }
                }
                if total > 0 {
                    relative = Some(total);
                    take(j - i, &mut used);
                    i = j;
                    continue;
                }
            }
            "сегодня" => {
                day = Some(today);
                take(1, &mut used);
            }
            // «сегодня вечером», «завтра утром»: a usual hour for the part of the day.
            "утром" | "днем" | "днём" | "вечером" | "ночью" if secs.is_none() => {
                secs = Some(match w {
                    "утром" => 9,
                    "вечером" => 19,
                    "ночью" => 23,
                    _ => 13,
                } * 3600);
                take(1, &mut used);
            }
            "на" if words.get(i + 1) == Some(&"следующей") && words.get(i + 2).is_some_and(|x| x.starts_with("недел")) => {
                day = Some(today - weekday(today) + 7);
                take(3, &mut used);
            }
            "завтра" => {
                day = Some(today + 1);
                take(1, &mut used);
            }
            "послезавтра" => {
                day = Some(today + 2);
                take(1, &mut used);
            }
            "каждый" | "каждую" | "каждое" | "по" | "ежедневно" => {
                let next = words.get(i + 1).copied().unwrap_or("");
                if w == "ежедневно" || next == "день" {
                    repeat = "daily".into();
                    take(if w == "ежедневно" { 1 } else { 2 }, &mut used);
                } else if next.starts_with("будн") {
                    repeat = "weekdays".into();
                    take(2, &mut used);
                } else if next == "неделю" {
                    repeat = "weekly".into();
                    take(2, &mut used);
                } else if next == "месяц" {
                    repeat = "monthly".into();
                    take(2, &mut used);
                } else if let Some(wd) = WEEKDAYS.iter().position(|d| next.starts_with(d)) {
                    repeat = "weekly".into();
                    let delta = (wd as i64 - weekday(today)).rem_euclid(7);
                    day = Some(today + delta);
                    take(2, &mut used);
                }
            }
            // «12 марта», «1 мая в 10»
            _ if w.parse::<u32>().is_ok_and(|d| (1..=31).contains(&d)) && words.get(i + 1).and_then(|m| month(m)).is_some() => {
                let d = w.parse::<u32>().unwrap_or(1);
                let m = month(words[i + 1]).unwrap_or(1);
                let (y, _, _) = civil(today);
                let mut dd = days_from_civil(y, m, d);
                if dd < today {
                    dd = days_from_civil(y + 1, m, d);
                }
                day = Some(dd);
                take(2, &mut used);
                i += 2;
                continue;
            }
            "в" | "во" | "к" | "до" => {
                let next = words.get(i + 1).copied().unwrap_or("");
                if let Some(wd) = WEEKDAYS.iter().position(|d| next.starts_with(d)) {
                    let mut delta = (wd as i64 - weekday(today)).rem_euclid(7);
                    if delta == 0 {
                        delta = 7;
                    }
                    day = Some(today + delta);
                    take(2, &mut used);
                } else if let Some((h, m)) = clock(next) {
                    let mut h = h;
                    let after = words.get(i + 2).copied().unwrap_or("");
                    let mut n = 2;
                    if after == "вечера" || after == "дня" {
                        if h < 12 {
                            h += 12;
                        }
                        n = 3;
                    } else if after == "утра" || after == "ночи" {
                        if h == 12 {
                            h = 0;
                        }
                        n = 3;
                    }
                    secs = Some(h * 3600 + m * 60);
                    take(n, &mut used);
                }
            }
            _ => {}
        }
        i += 1;
    }
    let has_time = relative.is_some() || secs.is_some();
    let due = if let Some(r) = relative {
        now + r
    } else if day.is_some() || secs.is_some() {
        let s = secs.unwrap_or(9 * 3600);
        let mut d = day.unwrap_or(today);
        // «в 7» when it is 9 already: tomorrow.
        if day.is_none() && s <= now_secs {
            d += 1;
        }
        d * 86_400 + s - off
    } else {
        return None;
    };
    let title: String = words
        .iter()
        .zip(&used)
        .filter(|(_, u)| !**u)
        .map(|(w, _)| *w)
        .collect::<Vec<_>>()
        .join(" ");
    let title = title.trim_start_matches("что ").trim_start_matches("о ").trim_start_matches("про ").trim().to_owned();
    Some(When { due, repeat, title: if title.is_empty() { "напоминание".into() } else { title }, has_time })
}

/// «18:30», «18.30», «9», «18 30» → (hours, minutes).
fn clock(w: &str) -> Option<(i64, i64)> {
    let w = w.trim_end_matches('.');
    if let Some((h, m)) = w.split_once([':', '.']) {
        let (h, m) = (h.parse().ok()?, m.parse().ok()?);
        return (h < 24 && m < 60).then_some((h, m));
    }
    let h = num(w)?;
    (0..24).contains(&h).then_some((h, 0))
}

// ---------------------------------------------------------------- tasks

pub fn add(title: &str, due: Option<i64>, repeat: &str, remind: bool, category: &str, priority: &str) -> Task {
    let t = Task {
        id: with(|l| l.iter().map(|t| t.id).max().unwrap_or(0) + 1),
        title: title.trim().chars().take(200).collect(),
        due,
        repeat: if ["daily", "weekdays", "weekly", "monthly"].contains(&repeat) { repeat.into() } else { String::new() },
        category: category.trim().chars().take(40).collect(),
        priority: if ["low", "high"].contains(&priority) { priority.into() } else { "normal".into() },
        done: false,
        remind,
        created: now(),
    };
    with(|l| l.push(t.clone()));
    save();
    t
}

/// The best match for a spoken title among open tasks.
fn find(title: &str) -> Option<u64> {
    let t = title.to_lowercase().replace('ё', "е");
    with(|l| {
        let open = l.iter().filter(|x| !x.done);
        let norm = |s: &str| s.to_lowercase().replace('ё', "е");
        open.clone()
            .find(|x| norm(&x.title) == t)
            .or_else(|| open.clone().find(|x| norm(&x.title).contains(&t) || t.contains(&norm(&x.title))))
            .or_else(|| open.clone().find(|x| t.split_whitespace().all(|w| norm(&x.title).contains(w))))
            .map(|x| x.id)
    })
}

pub fn set_done(id: u64, done: bool) -> bool {
    let ok = with(|l| match l.iter_mut().find(|t| t.id == id) {
        Some(t) => {
            // A repeating one moves on instead of being finished.
            match (done, t.due.and_then(|d| next_due(d, &t.repeat, offset()))) {
                (true, Some(next)) => t.due = Some(next),
                _ => t.done = done,
            }
            true
        }
        None => false,
    });
    save();
    ok
}

pub fn delete(id: u64) -> bool {
    let ok = with(|l| {
        let before = l.len();
        l.retain(|t| t.id != id);
        before != l.len()
    });
    save();
    ok
}

/// «19:30», «завтра в 9:00», «12 марта в 18:00».
pub fn say_time(t: i64, now: i64, off: i64) -> String {
    let (d, s) = local(t, off);
    let (today, _) = local(now, off);
    const MONTHS: [&str; 12] = ["января", "февраля", "марта", "апреля", "мая", "июня", "июля", "августа", "сентября", "октября", "ноября", "декабря"];
    let hm = format!("{}:{:02}", s / 3600, s / 60 % 60);
    match d - today {
        0 => format!("в {hm}"),
        1 => format!("завтра в {hm}"),
        2 => format!("послезавтра в {hm}"),
        _ => {
            let (_, m, dd) = civil(d);
            format!("{dd} {} в {hm}", MONTHS[(m - 1) as usize])
        }
    }
}

// ---------------------------------------------------------------- settings

/// The planner's settings (settings.rs: plan_*, brief_*).
#[derive(Debug, Clone, PartialEq)]
pub struct Plan {
    /// Warn this many minutes before a deadline (0 = no).
    pub warn: u16,
    /// A deadline with a date only is at this minute of the day (1440 = none).
    pub due_time: u16,
    pub brief_on: bool,
    /// Minute of the day for the morning brief.
    pub brief_at: u16,
    /// "all" | "weekdays"
    pub brief_days: String,
    /// New tasks by voice: "google" (when connected) or "local".
    pub target: String,
}

impl Default for Plan {
    fn default() -> Self {
        Plan { warn: 30, due_time: 18 * 60, brief_on: false, brief_at: 8 * 60, brief_days: "all".into(), target: "google".into() }
    }
}

static PLAN: Mutex<Option<Plan>> = Mutex::new(None);

pub fn configure_plan(p: Plan) {
    *PLAN.lock().unwrap() = Some(p);
}

fn plan() -> Plan {
    PLAN.lock().unwrap().clone().unwrap_or_default()
}

// ---------------------------------------------------------------- speech helpers

/// 1 событие, 2 события, 5 событий.
pub fn plural(n: usize, one: &str, few: &str, many: &str) -> String {
    let w = match (n % 10, n % 100) {
        (1, x) if x != 11 => one,
        (2..=4, x) if !(12..=14).contains(&x) => few,
        _ => many,
    };
    format!("{n} {w}")
}

/// «Через полчаса», «Через час», «Через 5 минут», «Через 21 минуту».
pub fn in_minutes(m: i64) -> String {
    match m {
        ..=1 => "Через минуту".into(),
        30 => "Через полчаса".into(),
        60 => "Через час".into(),
        90 => "Через полтора часа".into(),
        120 => "Через два часа".into(),
        m => {
            let (n10, n100) = (m % 10, m % 100);
            let w = if n10 == 1 && n100 != 11 {
                "минуту"
            } else if (2..=4).contains(&n10) && !(12..=14).contains(&n100) {
                "минуты"
            } else {
                "минут"
            };
            format!("Через {m} {w}")
        }
    }
}

const WEEKDAY_NAMES: [&str; 7] = ["понедельник", "вторник", "среда", "четверг", "пятница", "суббота", "воскресенье"];
/// «в понедельник», «во вторник», «в среду»…
const WEEKDAY_ON: [&str; 7] = ["в понедельник", "во вторник", "в среду", "в четверг", "в пятницу", "в субботу", "в воскресенье"];
const MONTHS: [&str; 12] = ["января", "февраля", "марта", "апреля", "мая", "июня", "июля", "августа", "сентября", "октября", "ноября", "декабря"];

fn hm(t: i64, off: i64) -> String {
    let (_, s) = local(t, off);
    format!("{}:{:02}", s / 3600, s / 60 % 60)
}

/// «Сегодня пятница, 6 ноября», «Завтра суббота, 7 ноября», «Понедельник, 9 ноября».
fn day_head(day: i64, today: i64) -> String {
    let (_, m, d) = civil(day);
    let wd = WEEKDAY_NAMES[weekday(day) as usize];
    let date = format!("{d} {}", MONTHS[(m - 1) as usize]);
    match day - today {
        0 => format!("Сегодня {wd}, {date}"),
        1 => format!("Завтра {wd}, {date}"),
        _ => {
            let mut c = wd.chars();
            format!("{}{}, {date}", c.next().map(|x| x.to_uppercase().collect::<String>()).unwrap_or_default(), c.as_str())
        }
    }
}

/// «сегодня», «завтра», «в пятницу», «12 ноября».
fn day_word(day: i64, today: i64) -> String {
    match day - today {
        0 => "сегодня".into(),
        1 => "завтра".into(),
        2 => "послезавтра".into(),
        3..=6 => WEEKDAY_ON[weekday(day) as usize].into(),
        _ => {
            let (_, m, d) = civil(day);
            format!("{d} {}", MONTHS[(m - 1) as usize])
        }
    }
}

/// A time in a task's title or notes: «до 18:00», «к 9», «в 14.30», «18:00».
pub fn deadline_time(text: &str) -> Option<i64> {
    let t = text.to_lowercase();
    let words: Vec<&str> = t.split(|c: char| c.is_whitespace() || c == ',' || c == '(' || c == ')').filter(|w| !w.is_empty()).collect();
    for (i, w) in words.iter().enumerate() {
        let w = w.trim_end_matches(['.', ';', '!']);
        if let Some((h, m)) = w.split_once([':', '.']) {
            if let (Ok(h), Ok(m)) = (h.parse::<i64>(), m.parse::<i64>()) {
                if h < 24 && m < 60 && m.to_string().len() <= 2 {
                    return Some(h * 3600 + m * 60);
                }
            }
        }
        if i > 0 && ["до", "к", "в"].contains(&words[i - 1]) {
            if let Ok(h) = w.parse::<i64>() {
                if (0..24).contains(&h) && words.get(i + 1).is_none_or(|n| !n.starts_with("мин") && month(n).is_none()) {
                    let pm = words.get(i + 1).is_some_and(|n| *n == "вечера" || *n == "дня") && h < 12;
                    return Some((h + if pm { 12 } else { 0 }) * 3600);
                }
            }
        }
    }
    None
}

// ---------------------------------------------------------------- agenda

/// One line of the day: an event, a task (deadline) or a reminder, from soundya or Google.
#[derive(Debug, Clone, PartialEq)]
pub struct Item {
    /// "event" | "task" | "reminder"
    pub kind: &'static str,
    pub title: String,
    /// Start / deadline / reminder time; None = some time that day (or no date).
    pub at: Option<i64>,
    pub end: Option<i64>,
    pub all_day: bool,
    /// Local day; None = no date.
    pub day: Option<i64>,
    /// "local", "google" (tasks) or the calendar's name (events).
    pub source: String,
    /// soundya task id, or the Google task id.
    pub id: String,
    /// Google task list.
    pub list: String,
    pub priority: String,
}

impl Item {
    fn spoken(&self, off: i64) -> String {
        match (self.kind, self.at) {
            ("event", Some(t)) if !self.all_day => format!("в {} {}", hm(t, off), self.title),
            ("event", _) => format!("{} (весь день)", self.title),
            ("reminder", Some(t)) => format!("в {} {}", hm(t, off), self.title),
            (_, Some(t)) => format!("{} до {}", self.title, hm(t, off)),
            _ => self.title.clone(),
        }
    }
}

/// Everything known, from local day `from` to `to` (exclusive), open only.
/// Google tasks are on their date; their time comes from «до 18:00» in the title or notes.
pub fn collect(from: i64, to: i64, off: i64) -> Vec<Item> {
    let mut out = Vec::new();
    for e in crate::calendar::between(from * 86_400 - off, to * 86_400 - off) {
        out.push(Item {
            kind: "event",
            title: e.title.clone(),
            at: Some(e.start),
            end: Some(e.end),
            all_day: e.all_day,
            day: Some(local(e.start, off).0.max(from)),
            source: e.cal.clone(),
            id: e.uid.clone(),
            list: String::new(),
            priority: "normal".into(),
        });
    }
    for t in with(|l| l.iter().filter(|t| !t.done).cloned().collect::<Vec<_>>()) {
        let Some(due) = t.due else { continue };
        let d = local(due, off).0;
        if d >= from && d < to {
            out.push(Item {
                kind: if t.remind { "reminder" } else { "task" },
                title: t.title.clone(),
                at: Some(due),
                end: None,
                all_day: false,
                day: Some(d),
                source: "local".into(),
                id: t.id.to_string(),
                list: String::new(),
                priority: t.priority.clone(),
            });
        }
    }
    for g in crate::calendar::google_tasks() {
        let Some(d) = g.due_day else { continue };
        if d >= from && d < to {
            out.push(Item {
                kind: "task",
                title: g.title.clone(),
                at: deadline_time(&format!("{} {}", g.title, g.notes)).map(|s| d * 86_400 + s - off),
                end: None,
                all_day: false,
                day: Some(d),
                source: "google".into(),
                id: g.id.clone(),
                list: g.list.clone(),
                priority: "normal".into(),
            });
        }
    }
    out.sort_by_key(|i| (i.day, !(i.kind == "event" && i.all_day), i.at.unwrap_or(i64::MAX), i.kind != "event"));
    out
}

/// Open tasks past their deadline (soundya's and Google's).
pub fn overdue(now: i64, off: i64) -> Vec<Item> {
    let (today, _) = local(now, off);
    let mut out: Vec<Item> = with(|l| {
        l.iter()
            .filter(|t| !t.done && !t.remind && t.due.is_some_and(|d| d < now))
            .map(|t| Item {
                kind: "task",
                title: t.title.clone(),
                at: t.due,
                end: None,
                all_day: false,
                day: t.due.map(|d| local(d, off).0),
                source: "local".into(),
                id: t.id.to_string(),
                list: String::new(),
                priority: t.priority.clone(),
            })
            .collect()
    });
    for g in crate::calendar::google_tasks() {
        let Some(d) = g.due_day else { continue };
        let at = deadline_time(&format!("{} {}", g.title, g.notes)).map(|s| d * 86_400 + s - off);
        if d < today || at.is_some_and(|a| a < now) {
            out.push(Item {
                kind: "task",
                title: g.title.clone(),
                at,
                end: None,
                all_day: false,
                day: Some(d),
                source: "google".into(),
                id: g.id.clone(),
                list: g.list.clone(),
                priority: "normal".into(),
            });
        }
    }
    out.sort_by_key(|i| (i.day, i.at));
    out
}

/// Open tasks without a date: soundya's important ones and all of Google's.
pub fn undated() -> Vec<Item> {
    let mut out: Vec<Item> = with(|l| {
        l.iter()
            .filter(|t| !t.done && t.due.is_none())
            .map(|t| Item {
                kind: "task",
                title: t.title.clone(),
                at: None,
                end: None,
                all_day: false,
                day: None,
                source: "local".into(),
                id: t.id.to_string(),
                list: String::new(),
                priority: t.priority.clone(),
            })
            .collect()
    });
    for g in crate::calendar::google_tasks().into_iter().filter(|g| g.due_day.is_none()) {
        out.push(Item {
            kind: "task",
            title: g.title,
            at: None,
            end: None,
            all_day: false,
            day: None,
            source: "google".into(),
            id: g.id,
            list: g.list,
            priority: "normal".into(),
        });
    }
    out.sort_by_key(|i| i.priority != "high");
    out
}

/// «a, b и c» with «и ещё N» past `max`.
fn list_of(items: &[String], max: usize) -> String {
    let shown: Vec<&str> = items.iter().take(max).map(String::as_str).collect();
    if items.len() > max {
        // «a, b, c и ещё 3»
        return format!("{} и ещё {}", shown.join(", "), items.len() - max);
    }
    match shown.len() {
        0 => String::new(),
        1 => shown[0].to_owned(),
        n => format!("{} и {}", shown[..n - 1].join(", "), shown[n - 1]),
    }
}

/// What is asked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ask {
    /// Everything of a day: events, tasks, overdue.
    Plans(i64),
    /// Only what has to be done.
    Todo(i64),
    /// Only what has a time.
    Schedule(i64),
    /// The next thing.
    Next,
    /// Deadlines this week.
    Deadlines,
    /// The week ahead.
    Week,
    /// The morning brief.
    Brief,
}

pub fn agenda_text(ask: Ask, now: i64, off: i64) -> String {
    let (today, _) = local(now, off);
    match ask {
        Ask::Plans(d) | Ask::Todo(d) | Ask::Schedule(d) => {
            let day = today + d;
            let items = collect(day, day + 1, off);
            // Today: events already over are not news.
            let events: Vec<&Item> = items.iter().filter(|i| i.kind == "event" && (d != 0 || i.all_day || i.end.unwrap_or(0) > now)).collect();
            let tasks: Vec<&Item> = items.iter().filter(|i| i.kind != "event" && (d != 0 || i.kind == "task" || i.at.is_none_or(|a| a >= now))).collect();
            let late: Vec<Item> = if d == 0 { overdue(now, off).into_iter().filter(|i| i.day < Some(today) || tasks.iter().all(|t| t.id != i.id)).collect() } else { vec![] };
            let mut out = vec![day_head(day, today)];
            let ev = |out: &mut Vec<String>| {
                if events.is_empty() {
                    out.push(if d == 0 && items.iter().any(|i| i.kind == "event") { "Встреч больше нет".into() } else { "В календаре пусто".into() });
                } else {
                    let what = events.iter().map(|e| e.spoken(off)).collect::<Vec<_>>();
                    out.push(format!("В календаре {}: {}", plural(events.len(), "событие", "события", "событий"), list_of(&what, 8)));
                }
            };
            let tk = |out: &mut Vec<String>| {
                if tasks.is_empty() {
                    out.push(if d == 0 { "Задач на сегодня нет".into() } else { "Задач нет".into() });
                } else {
                    let what = tasks.iter().map(|t| t.spoken(off)).collect::<Vec<_>>();
                    out.push(format!("{}: {}", if d == 0 { "Сделать сегодня" } else { "Сделать" }, list_of(&what, 8)));
                }
                if !late.is_empty() {
                    let what = late.iter().map(|t| t.title.clone()).collect::<Vec<_>>();
                    out.push(format!("Просрочено: {}", list_of(&what, 4)));
                }
            };
            match ask {
                Ask::Schedule(_) => {
                    let mut timed: Vec<&Item> = events.clone();
                    timed.extend(tasks.iter().filter(|t| t.at.is_some()));
                    timed.sort_by_key(|i| (!i.all_day, i.at));
                    if timed.is_empty() {
                        out.push(if d == 0 { "Расписание на сегодня свободно".into() } else { "Расписание свободно".into() });
                    } else {
                        out.push(format!("Расписание: {}", list_of(&timed.iter().map(|i| i.spoken(off)).collect::<Vec<_>>(), 10)));
                    }
                }
                Ask::Todo(_) => {
                    tk(&mut out);
                    if d == 0 {
                        let rest = undated();
                        if !rest.is_empty() && tasks.len() + late.len() < 5 {
                            let what = rest.iter().map(|t| t.title.clone()).collect::<Vec<_>>();
                            out.push(format!("Без срока: {}", list_of(&what, 3)));
                        }
                    }
                    if tasks.is_empty() && late.is_empty() && !events.is_empty() {
                        out.push(format!("Но в календаре {}", plural(events.len(), "событие", "события", "событий")));
                    }
                }
                _ => {
                    if events.is_empty() && tasks.is_empty() && late.is_empty() {
                        out.push(if d == 0 { "Планов нет, день свободен".into() } else { "Планов нет".into() });
                    } else {
                        ev(&mut out);
                        tk(&mut out);
                    }
                }
            }
            format!("{}.", out.join(". "))
        }
        Ask::Next => {
            let items = collect(today, today + 8, off);
            match items.iter().filter(|i| !i.all_day && i.at.is_some_and(|a| a > now)).min_by_key(|i| i.at) {
                Some(i) => {
                    let at = i.at.unwrap_or(now);
                    let left = (at - now + 30) / 60;
                    let what = match i.kind {
                        "task" => format!("срок: {}", i.title),
                        _ => i.title.clone(),
                    };
                    if local(at, off).0 == today && left < 180 {
                        format!("Дальше в {} {what} — {}.", hm(at, off), in_minutes(left).to_lowercase())
                    } else {
                        format!("Следующее — {} в {}: {what}.", day_word(local(at, off).0, today), hm(at, off))
                    }
                }
                None => "Ближайших планов со временем нет.".into(),
            }
        }
        Ask::Deadlines => {
            let late = overdue(now, off);
            let soon: Vec<Item> = collect(today, today + 8, off).into_iter().filter(|i| i.kind == "task" && i.at.is_none_or(|a| a >= now)).collect();
            if late.is_empty() && soon.is_empty() {
                return "На неделе сроков нет.".into();
            }
            let mut parts = Vec::new();
            if !late.is_empty() {
                parts.push(format!("Просрочено: {}", list_of(&late.iter().map(|t| t.title.clone()).collect::<Vec<_>>(), 4)));
            }
            let lines: Vec<String> = soon
                .iter()
                .map(|t| {
                    let d = t.day.unwrap_or(today);
                    match t.at {
                        Some(a) => format!("{} до {} — {}", day_word(d, today), hm(a, off), t.title),
                        None => format!("{} — {}", day_word(d, today), t.title),
                    }
                })
                .collect();
            if !lines.is_empty() {
                parts.push(format!("Сроки: {}", list_of(&lines, 6)));
            }
            format!("{}.", parts.join(". "))
        }
        Ask::Week => {
            let items = collect(today, today + 7, off);
            let mut parts = Vec::new();
            for day in today..today + 7 {
                let of: Vec<&Item> = items.iter().filter(|i| i.day == Some(day) && (day != today || i.kind != "event" || i.all_day || i.end.unwrap_or(0) > now)).collect();
                if of.is_empty() {
                    continue;
                }
                let titles: Vec<String> = of.iter().map(|i| i.title.clone()).collect();
                let w = day_word(day, today);
                let mut c = w.chars();
                let w = format!("{}{}", c.next().map(|x| x.to_uppercase().collect::<String>()).unwrap_or_default(), c.as_str());
                parts.push(format!("{w}: {}", list_of(&titles, 3)));
            }
            if parts.is_empty() {
                "На неделе планов нет.".into()
            } else {
                format!("На неделе. {}.", parts.join(". "))
            }
        }
        Ask::Brief => {
            let mut s = format!("Доброе утро! {}", agenda_text(Ask::Plans(0), now, off));
            let tomorrow: Vec<String> = collect(today + 1, today + 2, off).into_iter().filter(|i| i.kind == "task").map(|i| i.title).collect();
            if !tomorrow.is_empty() {
                s.push_str(&format!(" Завтра срок: {}.", list_of(&tomorrow, 3)));
            }
            s
        }
    }
}

/// «Какие планы на сегодня / завтра»: day offset 0 or 1; None = everything open.
pub fn agenda(day: Option<i64>) -> String {
    agenda_text(match day {
        Some(d) => Ask::Plans(d),
        None => Ask::Todo(0),
    }, now(), offset())
}

pub fn json() -> Value {
    let (now, off) = (now(), offset());
    let (today, _) = local(now, off);
    let mut timeline: Vec<Value> = overdue(now, off).iter().map(|i| item_json(i, now, off, true)).collect();
    timeline.extend(collect(today, today + 8, off).iter().map(|i| item_json(i, now, off, false)));
    timeline.extend(undated().iter().map(|i| item_json(i, now, off, false)));
    let mut items: Vec<Task> = with(|l| l.clone());
    items.sort_by_key(|t| (t.done, t.due.unwrap_or(i64::MAX), t.id));
    json!({
        "enabled": ENABLED.load(std::sync::atomic::Ordering::Acquire),
        "offset_min": off / 60,
        "google": crate::calendar::google_on(),
        "plan": {
            "warn": plan().warn, "due_time": plan().due_time, "brief_on": plan().brief_on,
            "brief_at": plan().brief_at, "brief_days": plan().brief_days, "target": plan().target,
        },
        "today": agenda_text(Ask::Plans(0), now, off),
        "items": timeline,
        "tasks": items.iter().map(|t| {
            let mut v = serde_json::to_value(t).unwrap_or_default();
            v["when"] = t.due.map(|d| say_time(d, now, off)).into();
            v["overdue"] = (!t.done && t.due.is_some_and(|d| d < now)).into();
            v
        }).collect::<Vec<_>>(),
    })
}

/// One line for the page's timeline.
fn item_json(i: &Item, now: i64, off: i64, late: bool) -> Value {
    let (today, _) = local(now, off);
    json!({
        "kind": i.kind, "title": i.title, "source": i.source, "id": i.id, "list": i.list,
        "at": i.at, "end": i.end, "all_day": i.all_day, "day": i.day.map(|d| d - today),
        "day_word": i.day.map(|d| day_word(d, today)),
        "time": i.at.filter(|_| !i.all_day).map(|a| hm(a, off)),
        "overdue": late, "priority": i.priority,
    })
}

// ---------------------------------------------------------------- voice

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Voice {
    Remind(String),
    AddTask(String),
    AddEvent(String),
    /// Kept for old callers: Some(day) = Plans(day), None = Todo(0).
    Agenda(Option<i64>),
    Ask(Ask),
    Done(String),
    Delete(String),
}

impl Voice {
    /// Only asks, changes nothing.
    pub fn is_query(&self) -> bool {
        matches!(self, Voice::Agenda(_) | Voice::Ask(_))
    }
}

/// «на завтра», «на пятницу», «в среду»: the day asked about (offset from today).
fn asked_day(text: &str, today: i64) -> i64 {
    if text.contains("послезавтра") {
        return 2;
    }
    if text.contains("завтра") {
        return 1;
    }
    let stems = ["понедельник", "вторник", "сред", "четверг", "пятниц", "суббот", "воскресен"];
    for (i, st) in stems.iter().enumerate() {
        if text.split_whitespace().any(|w| w.starts_with(st)) {
            return (i as i64 - weekday(today)).rem_euclid(7);
        }
    }
    0
}

/// Planner phrases (normalised text). Only asked when the planner is on.
pub fn voice(text: &str) -> Option<Voice> {
    let text = text.trim();
    for p in ["напомни мне ", "напомни ", "напоминай мне ", "напоминай ", "поставь напоминание ", "создай напоминание "] {
        if let Some(rest) = text.strip_prefix(p) {
            return (!rest.trim().is_empty()).then(|| Voice::Remind(rest.trim().to_owned()));
        }
    }
    for p in ["добавь в календарь ", "запиши в календарь ", "внеси в календарь ", "поставь в календарь ", "запланируй ", "создай событие ", "добавь событие "] {
        if let Some(rest) = text.strip_prefix(p) {
            return (!rest.trim().is_empty()).then(|| Voice::AddEvent(rest.trim().to_owned()));
        }
    }
    for p in ["добавь встречу ", "создай встречу ", "назначь встречу "] {
        if let Some(rest) = text.strip_prefix(p) {
            return (!rest.trim().is_empty()).then(|| Voice::AddEvent(format!("встреча {}", rest.trim())));
        }
    }
    for p in ["добавь задачу ", "добавь в список дел ", "добавь в планы ", "добавь в задачи ", "запиши задачу ", "новая задача ", "поставь задачу ", "создай задачу "] {
        if let Some(rest) = text.strip_prefix(p) {
            return (!rest.trim().is_empty()).then(|| Voice::AddTask(rest.trim().to_owned()));
        }
    }
    for p in ["добавь дедлайн ", "поставь дедлайн ", "дедлайн "] {
        if let Some(rest) = text.strip_prefix(p) {
            return (!rest.trim().is_empty()).then(|| Voice::AddTask(rest.trim().to_owned()));
        }
    }
    for p in ["отметь выполненной ", "отметь как выполненную ", "отметь выполненным ", "выполнено ", "я сделал ", "я сделала ", "задача выполнена ", "я выполнил ", "я выполнила ", "готово "] {
        if let Some(rest) = text.strip_prefix(p) {
            return (!rest.trim().is_empty()).then(|| Voice::Done(rest.trim().trim_start_matches("задачу ").to_owned()));
        }
    }
    for p in ["удали задачу ", "удали напоминание ", "убери задачу "] {
        if let Some(rest) = text.strip_prefix(p) {
            return (!rest.trim().is_empty()).then(|| Voice::Delete(rest.trim().to_owned()));
        }
    }
    let (today, _) = local(now(), offset());
    let day = asked_day(text, today);
    let has = |ws: &[&str]| ws.iter().any(|w| text.contains(w));
    // The next thing.
    if has(&["что у меня дальше", "что дальше по план", "что у меня следующ", "что следующее", "следующая встреча", "следующее событие", "куда мне дальше"]) {
        return Some(Voice::Ask(Ask::Next));
    }
    if has(&["дедлайн", "какие сроки", "какие у меня сроки", "что горит", "что просрочено", "что я просрочил", "что я просрочила"]) {
        return Some(Voice::Ask(Ask::Deadlines));
    }
    if has(&["на неделю", "на этой неделе", "на эту неделю", "что на неделе"]) && has(&["план", "задач", "что", "расписан"]) {
        return Some(Voice::Ask(Ask::Week));
    }
    if has(&["расскажи про мой день", "расскажи о моем дне", "сводка на день", "сводку на день", "утренняя сводка", "утреннюю сводку", "как выглядит мой день", "обзор дня"]) {
        return Some(Voice::Ask(Ask::Brief));
    }
    if has(&["расписание", "что у меня в календаре", "что в календаре", "какие встречи", "какие у меня встречи", "какие события"]) {
        return Some(Voice::Ask(Ask::Schedule(day)));
    }
    if has(&["нужно сделать", "надо сделать", "должен сделать", "должна сделать", "список дел", "какие задачи", "какие у меня задачи", "мои задачи", "что сделать"]) {
        return Some(Voice::Ask(Ask::Todo(day)));
    }
    let plans = ["какие планы", "какие у меня планы", "что у меня сегодня", "что у меня завтра", "что у меня на", "что запланировано", "мои планы", "что по планам", "какие дела", "какие у меня дела"];
    if plans.iter().any(|p| text.starts_with(p)) {
        return Some(Voice::Ask(Ask::Plans(day)));
    }
    None
}

/// «на час», «на 2 часа», «на полчаса», «на 45 минут» → seconds, and the phrase without it.
fn duration(text: &str) -> (i64, String) {
    let w: Vec<&str> = text.split_whitespace().collect();
    for i in 0..w.len() {
        if w[i] != "на" {
            continue;
        }
        let (secs, n) = match (w.get(i + 1).copied(), w.get(i + 2).copied()) {
            (Some("час"), _) => (3600, 2),
            (Some("полчаса"), _) => (1800, 2),
            (Some("полтора"), Some(u)) if u.starts_with("час") => (5400, 3),
            (Some(x), Some(u)) if num(x).is_some() && u.starts_with("час") => (num(x).unwrap_or(1) * 3600, 3),
            (Some(x), Some(u)) if num(x).is_some() && u.starts_with("минут") => (num(x).unwrap_or(30) * 60, 3),
            _ => continue,
        };
        let rest: Vec<&str> = w.iter().enumerate().filter(|(k, _)| *k < i || *k >= i + n).map(|(_, x)| *x).collect();
        return (secs, rest.join(" "));
    }
    (3600, text.to_owned())
}

/// «встречу с Петей» → «встреча с Петей»: the first word back to the nominative.
fn nominative(title: &str) -> String {
    const WORDS: [(&str, &str); 14] = [
        ("встречу", "встреча"), ("тренировку", "тренировка"), ("поездку", "поездка"), ("планерку", "планерка"),
        ("планёрку", "планёрка"), ("презентацию", "презентация"), ("консультацию", "консультация"), ("игру", "игра"),
        ("вечеринку", "вечеринка"), ("прогулку", "прогулка"), ("репетицию", "репетиция"), ("лекцию", "лекция"),
        ("конференцию", "конференция"), ("стрижку", "стрижка"),
    ];
    let (first, rest) = title.split_once(' ').unwrap_or((title, ""));
    match WORDS.iter().find(|(acc, _)| *acc == first.to_lowercase()) {
        Some((_, nom)) if rest.is_empty() => (*nom).to_owned(),
        Some((_, nom)) => format!("{nom} {rest}"),
        None => title.to_owned(),
    }
}

fn cap(s: &str) -> String {
    let mut c = s.chars();
    c.next().map(|x| x.to_uppercase().collect::<String>() + c.as_str()).unwrap_or_default()
}

/// Best match for a spoken title among Google's open tasks.
fn find_google(title: &str) -> Option<crate::google::GTask> {
    let norm = |s: &str| s.to_lowercase().replace('ё', "е");
    let t = norm(title);
    let all = crate::calendar::google_tasks();
    all.iter()
        .find(|x| norm(&x.title) == t)
        .or_else(|| all.iter().find(|x| norm(&x.title).contains(&t) || t.contains(&norm(&x.title))))
        .or_else(|| all.iter().find(|x| t.split_whitespace().all(|w| norm(&x.title).contains(w))))
        .cloned()
}

/// Does what was said; the answer is what Alice says. May call Google: not on the controller thread.
pub fn handle(v: Voice) -> String {
    let (now, off) = (now(), offset());
    let google = crate::calendar::google_on();
    match v {
        Voice::Remind(rest) => match parse_when(&rest, now, off) {
            Some((due, repeat, title)) => {
                add(&title, Some(due), &repeat, true, "", "normal");
                let rep = match repeat.as_str() {
                    "daily" => ", и так каждый день",
                    "weekdays" => ", и так по будням",
                    "weekly" => ", и так каждую неделю",
                    "monthly" => ", и так каждый месяц",
                    _ => "",
                };
                format!("Напомню {}: {title}{rep}", say_time(due, now, off))
            }
            None => "Когда напомнить? Скажите, например: напомни через 10 минут или напомни завтра в 9".into(),
        },
        Voice::AddEvent(rest) => {
            let (len, rest) = duration(&rest);
            let Some(w) = parse_when_full(&rest, now, off) else {
                return "Когда? Скажите, например: добавь в календарь встречу с Петей завтра в 15:00".into();
            };
            let title = cap(&nominative(&w.title));
            if google {
                let (start, end) = if w.has_time { (w.due, w.due + len) } else { (local(w.due, off).0 * 86_400 - off, 0) };
                match crate::calendar::add_event(&title, start, end, !w.has_time) {
                    Ok(cal) => {
                        let when = if w.has_time { say_time(w.due, now, off) } else { day_word(local(w.due, off).0, local(now, off).0) };
                        format!("Добавила в календарь «{cal}»: {title}, {when}")
                    }
                    Err(e) => {
                        add(&title, Some(w.due), "", w.has_time, "", "normal");
                        format!("Google не ответил, записала в планы саундя: {title}. Ошибка: {e}")
                    }
                }
            } else {
                add(&title, Some(w.due), "", true, "", "normal");
                format!("Google Календарь не подключён. Записала в планы саундя и напомню {}: {title}", say_time(w.due, now, off))
            }
        }
        Voice::AddTask(rest) => {
            let w = parse_when_full(&rest, now, off);
            let title = cap(&w.as_ref().map_or(rest.clone(), |w| w.title.clone()));
            let due_text = w.as_ref().map(|w| {
                let d = day_word(local(w.due, off).0, local(now, off).0);
                if w.has_time { format!(", срок {d} до {}", hm(w.due, off)) } else { format!(", срок {d}") }
            });
            if google && plan().target != "local" {
                let day = w.as_ref().map(|w| local(w.due, off).0);
                let notes = w.as_ref().filter(|w| w.has_time).map(|w| format!("до {}", hm(w.due, off))).unwrap_or_default();
                match crate::calendar::add_task(&title, day, &notes) {
                    Ok(list) => return format!("Добавила задачу в «{list}»: {title}{}", due_text.unwrap_or_default()),
                    Err(e) => eprintln!("planner: Google task not added: {e}"),
                }
            }
            // A deadline with a date only: at the usual deadline time of that day.
            let due = w.as_ref().map(|w| {
                if w.has_time || plan().due_time >= 1440 {
                    w.due
                } else {
                    local(w.due, off).0 * 86_400 + i64::from(plan().due_time) * 60 - off
                }
            });
            add(&title, due, w.as_ref().map_or("", |w| w.repeat.as_str()), false, "", "normal");
            format!("Добавила задачу: {title}{}", due_text.unwrap_or_default())
        }
        Voice::Agenda(day) => agenda(day),
        Voice::Ask(a) => agenda_text(a, now, off),
        Voice::Done(title) => {
            if let Some(id) = find(&title) {
                set_done(id, true);
                let t = with(|l| l.iter().find(|x| x.id == id).map(|x| x.title.clone())).unwrap_or(title);
                return format!("Отметила: {t}");
            }
            if let Some(g) = find_google(&title) {
                return match crate::calendar::complete_task(&g.list, &g.id) {
                    Ok(_) => format!("Отметила в Google: {}", g.title),
                    Err(e) => format!("Google не ответил: {e}"),
                };
            }
            format!("Не нашла задачу «{title}»")
        }
        Voice::Delete(title) => {
            if let Some(id) = find(&title) {
                delete(id);
                return "Удалила".into();
            }
            if let Some(g) = find_google(&title) {
                return match crate::calendar::delete_task(&g.list, &g.id) {
                    Ok(_) => format!("Удалила из Google: {}", g.title),
                    Err(e) => format!("Google не ответил: {e}"),
                };
            }
            format!("Не нашла задачу «{title}»")
        }
    }
}

// ---------------------------------------------------------------- reminders

/// Loads the tasks and says due reminders (also ones missed while off, under 6 h).
pub fn run(dir: &Path) {
    let list: Vec<Task> = crate::fsx::read_string(&file(dir)).and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default();
    *TASKS.lock().unwrap() = Some(list);
    *DIR.lock().unwrap() = Some(dir.to_owned());
    let brief_file = dir.join("brief-day");
    thread::spawn(move || loop {
        thread::sleep(Duration::from_secs(10));
        let on = ENABLED.load(std::sync::atomic::Ordering::Acquire);
        if on || crate::calendar::google_on() {
            warn_deadlines(now(), offset(), on);
        }
        if on || crate::calendar::active() {
            morning_brief(&brief_file);
        }
        // Voice reminders only with the planner on; tasks are kept either way.
        if !on {
            continue;
        }
        let t = now();
        let due: Vec<(u64, String)> = with(|l| {
            l.iter()
                .filter(|x| x.remind && !x.done && x.due.is_some_and(|d| d <= t))
                .map(|x| (x.id, x.title.clone()))
                .collect()
        });
        for (id, title) in due {
            let late = with(|l| l.iter().find(|x| x.id == id).and_then(|x| x.due)).map_or(0, |d| t - d);
            if late < 6 * 3600 {
                let text = if late > 600 { format!("Пропущенное напоминание: {title}") } else { format!("Напоминаю: {title}") };
                eprintln!("planner: {text}");
                crate::led::set_install(Some(crate::led::Install::Done));
                crate::display::notify("rem");
                crate::alice::say(text);
            }
            set_done(id, true);
        }
    });
}

/// Deadlines already warned about: (source:id, deadline).
static WARNED: Mutex<Vec<(String, i64)>> = Mutex::new(Vec::new());

/// Deadlines in the next `warn` minutes: soundya's tasks (with the planner on)
/// and Google's (their time from «до 18:00», else the usual deadline time).
pub fn deadlines_due(now: i64, off: i64, local_on: bool) -> Vec<(String, i64, String)> {
    let p = plan();
    if p.warn == 0 {
        return vec![];
    }
    let lead = i64::from(p.warn) * 60;
    let (today, _) = local(now, off);
    let mut out = Vec::new();
    if local_on {
        for t in with(|l| l.iter().filter(|t| !t.done && !t.remind).cloned().collect::<Vec<_>>()) {
            if let Some(d) = t.due.filter(|d| *d > now && *d - now <= lead) {
                out.push((format!("local:{}", t.id), d, t.title));
            }
        }
    }
    for g in crate::calendar::google_tasks() {
        let Some(day) = g.due_day.filter(|d| (today..=today + 1).contains(d)) else { continue };
        let secs = deadline_time(&format!("{} {}", g.title, g.notes)).or((p.due_time < 1440).then(|| i64::from(p.due_time) * 60));
        if let Some(at) = secs.map(|s| day * 86_400 + s - off).filter(|a| *a > now && *a - now <= lead) {
            out.push((format!("google:{}", g.id), at, g.title));
        }
    }
    out
}

fn warn_deadlines(now: i64, off: i64, local_on: bool) {
    for (key, at, title) in deadlines_due(now, off, local_on) {
        {
            let mut w = WARNED.lock().unwrap();
            if w.iter().any(|(k, a)| *k == key && *a == at) {
                continue;
            }
            w.push((key, at));
            if w.len() > 300 {
                w.drain(..100);
            }
        }
        let text = format!("{} срок: {title}", in_minutes((at - now + 30) / 60));
        eprintln!("planner: {text}");
        crate::display::notify("rem");
        crate::alice::say(text);
    }
}

/// The morning brief, once a day at its time (or up to two hours later if the
/// speaker was off then); the day it was said is kept on flash.
fn morning_brief(file: &Path) {
    let p = plan();
    if !p.brief_on {
        return;
    }
    let (n, off) = (now(), offset());
    let (today, secs) = local(n, off);
    let at = i64::from(p.brief_at) * 60;
    if secs < at || secs > at + 2 * 3600 || (p.brief_days == "weekdays" && weekday(today) >= 5) {
        return;
    }
    if crate::fsx::read_string(file).and_then(|t| t.trim().parse::<i64>().ok()) == Some(today) {
        return;
    }
    let _ = crate::settings::write_synced(file, today.to_string().as_bytes());
    let text = agenda_text(Ask::Brief, n, off);
    eprintln!("planner: brief: {text}");
    crate::alice::say(text);
}

#[cfg(test)]
mod tests {
    use super::*;

    // 2026-11-06 12:00 UTC = 15:00 Moscow (+3), a Friday.
    const NOW: i64 = 1_793_966_400; // 2026-11-06 12:00 UTC
    const MSK: i64 = 3 * 3600;

    fn at(rest: &str) -> (String, String, String) {
        let (d, r, t) = parse_when(rest, NOW, MSK).unwrap();
        (say_time(d, NOW, MSK), r, t)
    }

    #[test]
    fn dates() {
        assert_eq!(civil(0), (1970, 1, 1));
        assert_eq!(days_from_civil(2026, 11, 6), local(NOW, MSK).0);
        assert_eq!(weekday(local(NOW, MSK).0), 4, "Friday");
    }

    #[test]
    fn russian_times() {
        assert_eq!(at("через 10 минут выключить чайник"), ("в 15:10".into(), "".into(), "выключить чайник".into()));
        assert_eq!(at("через полчаса проверить духовку").0, "в 15:30");
        assert_eq!(at("через час позвонить").0, "в 16:00");
        assert_eq!(at("через 2 часа 30 минут встреча").0, "в 17:30");
        assert_eq!(at("завтра в 9 позвонить маме"), ("завтра в 9:00".into(), "".into(), "позвонить маме".into()));
        assert_eq!(at("в 18:30 забрать посылку").0, "в 18:30");
        assert_eq!(at("в 7 вечера ужин").0, "в 19:00");
        assert_eq!(at("в 10 утра выйти").0, "завтра в 10:00", "10:00 already passed today");
        assert_eq!(at("в понедельник в 9 планёрка").0, "9 ноября в 9:00");
        let (w, r, t) = at("каждый день в 8 выпить таблетки");
        assert_eq!((w.as_str(), r.as_str(), t.as_str()), ("завтра в 8:00", "daily", "выпить таблетки"));
        assert_eq!(at("по будням в 7:30 зарядка").1, "weekdays");
        assert_eq!(at("о том что надо в 20:00 полить цветы").2, "том что надо полить цветы");
        assert!(parse_when("купить молоко", NOW, MSK).is_none());
    }

    #[test]
    fn repeats_move_on() {
        let fri9 = days_from_civil(2026, 11, 6) * 86_400 + 9 * 3600 - MSK;
        assert_eq!(say_time(next_due(fri9, "weekdays", MSK).unwrap(), NOW, MSK), "9 ноября в 9:00", "Friday → Monday");
        assert_eq!(say_time(next_due(fri9, "daily", MSK).unwrap(), NOW, MSK), "завтра в 9:00");
        let jan31 = days_from_civil(2027, 1, 31) * 86_400 + 9 * 3600 - MSK;
        assert_eq!(civil(local(next_due(jan31, "monthly", MSK).unwrap(), MSK).0), (2027, 2, 28));
    }

    #[test]
    fn voice_phrases() {
        assert_eq!(voice("напомни через 10 минут выключить чайник"), Some(Voice::Remind("через 10 минут выключить чайник".into())));
        assert_eq!(voice("добавь задачу купить молоко"), Some(Voice::AddTask("купить молоко".into())));
        assert_eq!(voice("какие планы на завтра"), Some(Voice::Ask(Ask::Plans(1))));
        assert_eq!(voice("какие у меня планы на сегодня"), Some(Voice::Ask(Ask::Plans(0))));
        assert_eq!(voice("что у меня сегодня"), Some(Voice::Ask(Ask::Plans(0))));
        assert_eq!(voice("какие у меня задачи"), Some(Voice::Ask(Ask::Todo(0))));
        assert_eq!(voice("что мне сегодня нужно сделать"), Some(Voice::Ask(Ask::Todo(0))));
        assert_eq!(voice("что надо сделать завтра"), Some(Voice::Ask(Ask::Todo(1))));
        assert_eq!(voice("какое у меня расписание на завтра"), Some(Voice::Ask(Ask::Schedule(1))));
        assert_eq!(voice("что у меня в календаре"), Some(Voice::Ask(Ask::Schedule(0))));
        assert_eq!(voice("что у меня дальше"), Some(Voice::Ask(Ask::Next)));
        assert_eq!(voice("какая следующая встреча"), Some(Voice::Ask(Ask::Next)));
        assert_eq!(voice("какие у меня дедлайны"), Some(Voice::Ask(Ask::Deadlines)));
        assert_eq!(voice("какие планы на неделю"), Some(Voice::Ask(Ask::Week)));
        assert_eq!(voice("расскажи про мой день"), Some(Voice::Ask(Ask::Brief)));
        assert_eq!(voice("добавь в календарь встречу с петей завтра в 15"), Some(Voice::AddEvent("встречу с петей завтра в 15".into())));
        assert_eq!(voice("добавь встречу с петей завтра в 15"), Some(Voice::AddEvent("встреча с петей завтра в 15".into())));
        assert_eq!(voice("запланируй стоматолог в пятницу в 10"), Some(Voice::AddEvent("стоматолог в пятницу в 10".into())));
        assert_eq!(voice("дедлайн сдать отчет завтра в 18"), Some(Voice::AddTask("сдать отчет завтра в 18".into())));
        assert_eq!(voice("отметь выполненной купить молоко"), Some(Voice::Done("купить молоко".into())));
        assert!(voice("какие планы на завтра").unwrap().is_query());
        assert!(!voice("добавь задачу купить молоко").unwrap().is_query());
        assert_eq!(voice("включи bonobo"), None);
        assert_eq!(voice("какая погода"), None);
    }

    #[test]
    fn more_russian_times() {
        let full = |t: &str| parse_when_full(t, NOW, MSK).unwrap();
        // Friday 6 November, 15:00 MSK.
        assert_eq!(at("12 марта в 10 стоматолог"), ("12 марта в 10:00".into(), "".into(), "стоматолог".into()));
        assert_eq!(at("1 мая шашлыки").0, "1 мая в 9:00");
        assert_eq!(at("5 ноября оплатить").0, "5 ноября в 9:00", "the 5th has passed: next year (said without the year)");
        assert_eq!(at("через неделю отчет").0, "13 ноября в 15:00");
        assert_eq!(at("завтра вечером кино").0, "завтра в 19:00");
        assert_eq!(at("до пятницы сдать проект").0, "13 ноября в 9:00");
        assert_eq!(at("до 18:00 сдать отчет"), ("в 18:00".into(), "".into(), "сдать отчет".into()));
        assert_eq!(at("на следующей неделе позвонить").0, "9 ноября в 9:00");
        assert!(full("завтра купить хлеб").has_time == false);
        assert!(full("завтра в 9 купить хлеб").has_time);
        assert!(full("через 2 часа").has_time);
        assert_eq!(duration("встреча на 2 часа завтра в 15"), (7200, "встреча завтра в 15".into()));
        assert_eq!(duration("созвон на полчаса в 11"), (1800, "созвон в 11".into()));
        assert_eq!(duration("обед в 13").0, 3600);
        assert_eq!(nominative("встречу с петей"), "встреча с петей");
        assert_eq!(nominative("тренировку"), "тренировка");
        assert_eq!(nominative("стоматолог"), "стоматолог");
    }

    #[test]
    fn speech_helpers() {
        assert_eq!(plural(1, "событие", "события", "событий"), "1 событие");
        assert_eq!(plural(3, "событие", "события", "событий"), "3 события");
        assert_eq!(plural(11, "событие", "события", "событий"), "11 событий");
        assert_eq!(plural(22, "задача", "задачи", "задач"), "22 задачи");
        assert_eq!(in_minutes(30), "Через полчаса");
        assert_eq!(in_minutes(21), "Через 21 минуту");
        assert_eq!(in_minutes(15), "Через 15 минут");
        assert_eq!(in_minutes(60), "Через час");
        assert_eq!(deadline_time("Сдать отчёт до 18:00"), Some(18 * 3600));
        assert_eq!(deadline_time("созвон в 14.30"), Some(14 * 3600 + 1800));
        assert_eq!(deadline_time("отправить к 9"), Some(9 * 3600));
        assert_eq!(deadline_time("до 7 вечера"), Some(19 * 3600));
        assert_eq!(deadline_time("купить 2 литра молока"), None);
        assert_eq!(deadline_time("в 5 минут"), None);
        assert_eq!(deadline_time("подарок к 8 марта"), None);
    }

    /// One test for everything that touches the shared task list and Google's state.
    #[test]
    fn day_week_and_deadlines() {
        let _g = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        use crate::calendar::{test_google, Event};
        use crate::google::GTask;
        let today = local(NOW, MSK).0; // Friday 6 November
        let at = |d: i64, h: i64, m: i64| (today + d) * 86_400 + h * 3600 + m * 60 - MSK;
        let ev = |title: &str, start: i64, mins: i64, all_day: bool| Event {
            uid: title.into(), title: title.into(), start, end: start + mins * 60, all_day, cal: "Личный".into(),
        };
        test_google(
            vec![
                ev("Планёрка", at(0, 10, 0), 30, false),
                ev("Встреча с командой", at(0, 16, 0), 60, false),
                ev("Спортзал", at(0, 19, 0), 90, false),
                ev("День рождения Маши", at(1, 0, 0), 1440, true),
                ev("Стоматолог", at(3, 11, 0), 60, false),
            ],
            vec![
                GTask { id: "g1".into(), list: "L".into(), title: "Сдать отчёт".into(), notes: "до 18:00".into(), due_day: Some(today) },
                GTask { id: "g2".into(), list: "L".into(), title: "Позвонить в банк".into(), notes: String::new(), due_day: Some(today - 2) },
                GTask { id: "g3".into(), list: "L".into(), title: "Подготовить презентацию".into(), notes: String::new(), due_day: Some(today + 1) },
                GTask { id: "g4".into(), list: "L".into(), title: "Прочитать книгу".into(), notes: String::new(), due_day: None },
            ],
        );
        *TASKS.lock().unwrap() = Some(Vec::new());
        add("Купить продукты", Some(at(0, 20, 0)), "", false, "", "normal");
        add("Выпить таблетку", Some(at(0, 21, 0)), "", true, "", "normal");

        let plans = agenda_text(Ask::Plans(0), NOW, MSK);
        assert_eq!(
            plans,
            "Сегодня пятница, 6 ноября. В календаре 2 события: в 16:00 Встреча с командой и в 19:00 Спортзал. \
             Сделать сегодня: Сдать отчёт до 18:00, Купить продукты до 20:00 и в 21:00 Выпить таблетку. Просрочено: Позвонить в банк."
        );
        let todo = agenda_text(Ask::Todo(0), NOW, MSK);
        assert!(todo.contains("Сделать сегодня: Сдать отчёт до 18:00") && todo.contains("Без срока: Прочитать книгу") && !todo.contains("Спортзал"), "{todo}");
        let sched = agenda_text(Ask::Schedule(0), NOW, MSK);
        assert!(sched.starts_with("Сегодня пятница, 6 ноября. Расписание: в 16:00 Встреча с командой, Сдать отчёт до 18:00, в 19:00 Спортзал"), "{sched}");
        assert_eq!(agenda_text(Ask::Next, NOW, MSK), "Дальше в 16:00 Встреча с командой — через час.");
        let tomorrow = agenda_text(Ask::Plans(1), NOW, MSK);
        assert_eq!(tomorrow, "Завтра суббота, 7 ноября. В календаре 1 событие: День рождения Маши (весь день). Сделать: Подготовить презентацию.");
        let dl = agenda_text(Ask::Deadlines, NOW, MSK);
        assert_eq!(dl, "Просрочено: Позвонить в банк. Сроки: сегодня до 18:00 — Сдать отчёт, сегодня до 20:00 — Купить продукты и завтра — Подготовить презентацию.");
        let week = agenda_text(Ask::Week, NOW, MSK);
        assert!(week.contains("Сегодня: ") && week.contains("Завтра: День рождения Маши и Подготовить презентацию") && week.contains("В понедельник: Стоматолог"), "{week}");
        let brief = agenda_text(Ask::Brief, NOW, MSK);
        assert!(brief.starts_with("Доброе утро! Сегодня пятница") && brief.ends_with("Завтра срок: Подготовить презентацию."), "{brief}");

        // 30 minutes before «Сдать отчёт до 18:00» (Google) and «Купить продукты» (soundya).
        configure_plan(Plan::default());
        let due = deadlines_due(at(0, 17, 35), MSK, true);
        assert_eq!(due.iter().map(|d| d.2.as_str()).collect::<Vec<_>>(), ["Сдать отчёт"]);
        let due = deadlines_due(at(0, 19, 31), MSK, true);
        assert_eq!(due.iter().map(|d| d.2.as_str()).collect::<Vec<_>>(), ["Купить продукты"]);
        assert!(deadlines_due(at(0, 19, 31), MSK, false).is_empty(), "soundya's own only with the planner on");
        // A Google task with a date only: at the usual deadline time (18:00), else not at all.
        let due = deadlines_due(at(1, 17, 40), MSK, true);
        assert_eq!(due.iter().map(|d| d.2.as_str()).collect::<Vec<_>>(), ["Подготовить презентацию"]);
        configure_plan(Plan { due_time: 1440, ..Plan::default() });
        assert!(deadlines_due(at(1, 17, 40), MSK, true).is_empty());
        configure_plan(Plan { warn: 0, ..Plan::default() });
        assert!(deadlines_due(at(0, 17, 35), MSK, true).is_empty());
        configure_plan(Plan::default());

        // Nothing at all.
        test_google(vec![], vec![]);
        *TASKS.lock().unwrap() = Some(Vec::new());
        assert_eq!(agenda_text(Ask::Plans(0), NOW, MSK), "Сегодня пятница, 6 ноября. Планов нет, день свободен.");
        assert_eq!(agenda_text(Ask::Next, NOW, MSK), "Ближайших планов со временем нет.");
        *TASKS.lock().unwrap() = None;
    }

    static TEST_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn tasks_done_and_found() {
        let _g = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        *TASKS.lock().unwrap() = Some(Vec::new());
        let a = add("Купить молоко", None, "", false, "дом", "high");
        let r = add("таблетки", Some(NOW), "daily", true, "", "normal");
        assert_eq!(find("купить молоко"), Some(a.id));
        assert!(set_done(a.id, true));
        assert_eq!(find("купить молоко"), None, "done ones are not found");
        set_done(r.id, true);
        let moved = with(|l| l.iter().find(|t| t.id == r.id).cloned()).unwrap();
        assert!(!moved.done && moved.due == Some(NOW + 86_400), "a repeating one moves on");
        assert!(delete(r.id));
        *TASKS.lock().unwrap() = None;
    }
}
