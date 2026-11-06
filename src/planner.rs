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

fn now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

pub fn configure(enabled: bool, offset_min: i32) {
    ENABLED.store(enabled, std::sync::atomic::Ordering::Release);
    OFFSET.store(offset_min.clamp(-720, 840), std::sync::atomic::Ordering::Release);
}

fn offset() -> i64 {
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
fn civil(days: i64) -> (i64, u32, u32) {
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

fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let mp = (i64::from(m) + 9) % 12;
    let doy = (153 * mp + 2) / 5 + i64::from(d) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Local day number and seconds into it.
fn local(t: i64, off: i64) -> (i64, i64) {
    let l = t + off;
    (l.div_euclid(86_400), l.rem_euclid(86_400))
}

/// Monday = 0.
fn weekday(day: i64) -> i64 {
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
            "в" | "во" | "к" => {
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
    Some((due, repeat, if title.is_empty() { "напоминание".into() } else { title }))
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

/// «Какие планы на сегодня / завтра»: day offset 0 or 1; None = everything open.
pub fn agenda(day: Option<i64>) -> String {
    let (now, off) = (now(), offset());
    let (today, _) = local(now, off);
    let mut items: Vec<Task> = with(|l| l.iter().filter(|t| !t.done).cloned().collect());
    items.sort_by_key(|t| (t.due.unwrap_or(i64::MAX), t.priority != "high"));
    let pick: Vec<&Task> = items
        .iter()
        .filter(|t| match day {
            Some(dd) => t.due.is_some_and(|d| local(d, off).0 == today + dd) || (dd == 0 && t.due.is_none() && t.priority == "high"),
            None => true,
        })
        .take(8)
        .collect();
    let when = match day {
        Some(0) => "на сегодня",
        Some(1) => "на завтра",
        _ => "",
    };
    if pick.is_empty() {
        return if when.is_empty() { "Задач нет".into() } else { format!("Планов {when} нет") };
    }
    let list = pick
        .iter()
        .map(|t| match t.due {
            Some(d) => format!("{} {}", say_time(d, now, off), t.title),
            None => t.title.clone(),
        })
        .collect::<Vec<_>>()
        .join(". ");
    format!("{} {}: {list}.", if when.is_empty() { "Задачи" } else { "Планы" }, when).replace("  ", " ").replace(" :", ":")
}

pub fn json() -> Value {
    let (now, off) = (now(), offset());
    let mut items: Vec<Task> = with(|l| l.clone());
    items.sort_by_key(|t| (t.done, t.due.unwrap_or(i64::MAX), t.id));
    json!({
        "enabled": ENABLED.load(std::sync::atomic::Ordering::Acquire),
        "offset_min": off / 60,
        "tasks": items.iter().map(|t| {
            let mut v = serde_json::to_value(t).unwrap_or_default();
            v["when"] = t.due.map(|d| say_time(d, now, off)).into();
            v["overdue"] = (!t.done && t.due.is_some_and(|d| d < now)).into();
            v
        }).collect::<Vec<_>>(),
    })
}

// ---------------------------------------------------------------- voice

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Voice {
    Remind(String),
    AddTask(String),
    Agenda(Option<i64>),
    Done(String),
    Delete(String),
}

/// Planner phrases (normalised text). Only asked when the planner is on.
pub fn voice(text: &str) -> Option<Voice> {
    for p in ["напомни мне ", "напомни ", "напоминай мне ", "напоминай ", "поставь напоминание ", "создай напоминание "] {
        if let Some(rest) = text.strip_prefix(p) {
            return (!rest.trim().is_empty()).then(|| Voice::Remind(rest.trim().to_owned()));
        }
    }
    for p in ["добавь задачу ", "добавь в список дел ", "добавь в планы ", "запиши задачу ", "новая задача "] {
        if let Some(rest) = text.strip_prefix(p) {
            return (!rest.trim().is_empty()).then(|| Voice::AddTask(rest.trim().to_owned()));
        }
    }
    for p in ["отметь выполненной ", "отметь как выполненную ", "отметь выполненным ", "выполнено ", "я сделал ", "я сделала ", "задача выполнена "] {
        if let Some(rest) = text.strip_prefix(p) {
            return (!rest.trim().is_empty()).then(|| Voice::Done(rest.trim().trim_start_matches("задачу ").to_owned()));
        }
    }
    for p in ["удали задачу ", "удали напоминание ", "убери задачу "] {
        if let Some(rest) = text.strip_prefix(p) {
            return (!rest.trim().is_empty()).then(|| Voice::Delete(rest.trim().to_owned()));
        }
    }
    let plans = ["какие планы", "какие у меня планы", "что у меня", "какие задачи", "какие у меня задачи", "что запланировано", "мои планы", "мои задачи"];
    if plans.iter().any(|p| text.starts_with(p)) {
        let day = if text.contains("послезавтра") {
            Some(2)
        } else if text.contains("завтра") {
            Some(1)
        } else if text.contains("сегодня") || text.starts_with("что у меня") {
            Some(0)
        } else {
            None
        };
        return Some(Voice::Agenda(day));
    }
    None
}

/// Does what was said; the answer is what Alice says.
pub fn handle(v: Voice) -> String {
    let (now, off) = (now(), offset());
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
        Voice::AddTask(rest) => {
            let (due, repeat, title) = parse_when(&rest, now, off).map_or((None, String::new(), rest.clone()), |(d, r, t)| (Some(d), r, t));
            add(&title, due, &repeat, false, "", "normal");
            format!("Добавила задачу: {title}")
        }
        Voice::Agenda(day) => agenda(day),
        Voice::Done(title) => match find(&title) {
            Some(id) => {
                set_done(id, true);
                "Отметила".into()
            }
            None => format!("Не нашла задачу «{title}»"),
        },
        Voice::Delete(title) => match find(&title) {
            Some(id) => {
                delete(id);
                "Удалила".into()
            }
            None => format!("Не нашла задачу «{title}»"),
        },
    }
}

// ---------------------------------------------------------------- reminders

/// Loads the tasks and says due reminders (also ones missed while off, under 6 h).
pub fn run(dir: &Path) {
    let list: Vec<Task> = crate::fsx::read_string(&file(dir)).and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default();
    *TASKS.lock().unwrap() = Some(list);
    *DIR.lock().unwrap() = Some(dir.to_owned());
    thread::spawn(|| loop {
        thread::sleep(Duration::from_secs(10));
        // Voice reminders only with the planner on; tasks are kept either way.
        if !ENABLED.load(std::sync::atomic::Ordering::Acquire) {
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
                crate::alice::say(text);
            }
            set_done(id, true);
        }
    });
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
        assert_eq!(voice("какие планы на завтра"), Some(Voice::Agenda(Some(1))));
        assert_eq!(voice("что у меня сегодня"), Some(Voice::Agenda(Some(0))));
        assert_eq!(voice("какие у меня задачи"), Some(Voice::Agenda(None)));
        assert_eq!(voice("отметь выполненной купить молоко"), Some(Voice::Done("купить молоко".into())));
        assert_eq!(voice("включи bonobo"), None);
    }

    #[test]
    fn tasks_done_and_found() {
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
