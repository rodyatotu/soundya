//! soundya's own timer and pomodoro: one at a time, shown on the page, on the
//! clock display (stations that have one) and on the ring; said aloud when it
//! ends. Alice's own timers («поставь таймер на 5 минут») stay hers.
//!
//! Kept in `state_dir/timer.json`, so a restart does not lose it: the end time
//! is wall-clock, a paused one keeps what was left.

use std::{
    path::{Path, PathBuf},
    sync::Mutex,
    thread,
    time::{Duration, Instant},
};

use serde::{Deserialize, Serialize};

/// Pomodoro lengths in minutes and the work rounds before the long break.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Pomo {
    pub work: u16,
    pub brk: u16,
    pub long: u16,
    pub rounds: u8,
}

impl Default for Pomo {
    fn default() -> Self {
        Pomo { work: 25, brk: 5, long: 15, rounds: 4 }
    }
}

impl Pomo {
    pub fn clamped(self) -> Pomo {
        Pomo { work: self.work.clamp(1, 180), brk: self.brk.clamp(1, 60), long: self.long.clamp(1, 90), rounds: self.rounds.clamp(1, 12) }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Timer {
    /// "timer" | "pomodoro"
    pub kind: String,
    pub label: String,
    /// Pomodoro: "work" | "break" | "long"; "" for a plain timer.
    pub phase: String,
    /// Pomodoro: the work round, from 1.
    pub round: u8,
    /// Length of this phase, seconds.
    pub total: u64,
    /// When it ends (unix seconds) while running.
    pub ends: i64,
    /// Seconds left while paused.
    pub paused: Option<u64>,
    pub cfg: Pomo,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
struct Saved {
    timer: Option<Timer>,
    /// Finished work rounds: (local day number, count).
    #[serde(default)]
    today: (i64, u32),
}

pub fn plain(secs: u64, label: &str, now: i64) -> Timer {
    let secs = secs.clamp(1, 24 * 3600);
    Timer { kind: "timer".into(), label: label.trim().chars().take(60).collect(), phase: String::new(), round: 0, total: secs, ends: now + secs as i64, paused: None, cfg: Pomo::default() }
}

pub fn pomodoro(cfg: Pomo, label: &str, now: i64) -> Timer {
    let cfg = cfg.clamped();
    let total = cfg.work as u64 * 60;
    Timer { kind: "pomodoro".into(), label: label.trim().chars().take(60).collect(), phase: "work".into(), round: 1, total, ends: now + total as i64, paused: None, cfg }
}

pub fn left(t: &Timer, now: i64) -> u64 {
    t.paused.unwrap_or_else(|| (t.ends - now).max(0) as u64)
}

pub fn pause(t: &mut Timer, now: i64) {
    if t.paused.is_none() {
        t.paused = Some(left(t, now));
    }
}

pub fn resume(t: &mut Timer, now: i64) {
    if let Some(l) = t.paused.take() {
        t.ends = now + l as i64;
    }
}

/// The phase after this one: (next timer or None, what to say, a work round was finished).
pub fn next(t: &Timer, now: i64) -> (Option<Timer>, String, bool) {
    let what = if t.label.is_empty() { String::new() } else { format!(" «{}»", t.label) };
    if t.kind != "pomodoro" {
        return (None, format!("Таймер{what} вышел"), false);
    }
    let c = t.cfg;
    let phase = |phase: &str, round: u8, min: u16| {
        let total = min as u64 * 60;
        Timer { phase: phase.into(), round, total, ends: now + total as i64, paused: None, ..t.clone() }
    };
    match t.phase.as_str() {
        "work" if t.round >= c.rounds => (Some(phase("long", t.round, c.long)), format!("Помодоро{what}: {} из {} готово. Большой перерыв {}", t.round, c.rounds, minutes(c.long)), true),
        "work" => (Some(phase("break", t.round, c.brk)), format!("Помодоро{what}: круг {} готов. Перерыв {}", t.round, minutes(c.brk)), true),
        "break" => (Some(phase("work", t.round.saturating_add(1), c.work)), format!("Перерыв окончен. Круг {} из {}, {}", t.round.saturating_add(1), c.rounds, minutes(c.work)), false),
        _ => (None, format!("Помодоро{what} закончено: {} {}. Отличная работа", t.round, rounds_word(t.round as u32)), false),
    }
}

pub fn minutes(m: u16) -> String {
    let w = match (m % 10, m % 100) {
        (1, x) if x != 11 => "минута",
        (2..=4, x) if !(12..=14).contains(&x) => "минуты",
        _ => "минут",
    };
    format!("{m} {w}")
}

fn rounds_word(n: u32) -> &'static str {
    match (n % 10, n % 100) {
        (1, x) if x != 11 => "круг",
        (2..=4, x) if !(12..=14).contains(&x) => "круга",
        _ => "кругов",
    }
}

/// How it is said: «12 минут 30 секунд».
pub fn left_words(secs: u64) -> String {
    let (h, m, s) = (secs / 3600, secs / 60 % 60, secs % 60);
    let mut parts = Vec::new();
    if h > 0 {
        parts.push(format!("{h} ч"));
    }
    if m > 0 {
        parts.push(minutes(m as u16));
    }
    if h == 0 && (s > 0 || parts.is_empty()) {
        let w = match (s % 10, s % 100) {
            (1, x) if x != 11 => "секунда",
            (2..=4, x) if !(12..=14).contains(&x) => "секунды",
            _ => "секунд",
        };
        parts.push(format!("{s} {w}"));
    }
    parts.join(" ")
}

/// Four digits for the clock display and whether the colon is lit: MM:SS under
/// 100 minutes, HH:MM above.
pub fn face(secs: u64) -> String {
    if secs < 6000 {
        format!("{:02}{:02}", secs / 60, secs % 60)
    } else {
        format!("{:02}{:02}", (secs / 3600).min(99), secs / 60 % 60)
    }
}

/// What a change means for the scenarios (settings «при начале работы / перерыва / в конце»):
/// "work" | "break" | "done". A plain timer has none; pausing is not a change.
pub fn event(old: Option<&Timer>, new: Option<&Timer>) -> Option<&'static str> {
    match (old, new) {
        (_, Some(n)) if n.kind == "pomodoro" && old.is_none_or(|o| o.phase != n.phase || o.round != n.round || o.kind != n.kind) => {
            Some(if n.phase == "work" { "work" } else { "break" })
        }
        (Some(o), None) if o.kind == "pomodoro" => Some("done"),
        _ => None,
    }
}

// ------------------------------------------------------------------ voice

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Voice {
    Pomodoro,
    Stop,
    Pause,
    Resume,
    Skip,
    Left,
}

/// Phrases that are ours. «Поставь таймер…» is not: Alice's timers stay hers.
/// `running`: soundya's timer is on (then «сколько осталось» is about it).
pub fn voice(n: &str, running: bool) -> Option<Voice> {
    let n = n.trim();
    let pomo = n.contains("помодоро") || n.contains("pomodoro");
    if pomo {
        if ["стоп", "останови", "выключи", "хватит", "отмени", "закончи", "заверши"].iter().any(|w| n.contains(w)) {
            return Some(Voice::Stop);
        }
        if n.contains("пауз") {
            return Some(Voice::Pause);
        }
        if ["продолж", "возобнов", "сними с паузы"].iter().any(|w| n.contains(w)) {
            return Some(Voice::Resume);
        }
        if n.contains("сколько") {
            return Some(Voice::Left);
        }
        if n.contains("пропусти") || n.contains("следующ") {
            return Some(Voice::Skip);
        }
        return Some(Voice::Pomodoro);
    }
    if running {
        match n {
            "сколько осталось" | "сколько ещё осталось" | "сколько еще осталось" | "сколько времени осталось" => return Some(Voice::Left),
            "пропусти перерыв" | "закончи перерыв" => return Some(Voice::Skip),
            _ => {}
        }
    }
    None
}

// ------------------------------------------------------------------ state

static STATE: Mutex<Option<Saved>> = Mutex::new(None);
static DIR: Mutex<Option<PathBuf>> = Mutex::new(None);
/// Pomodoro events for the controller to run the chosen scenarios (event()).
static EVENTS: Mutex<Vec<&'static str>> = Mutex::new(Vec::new());
/// A phase just ended: the ring and the display flash for a few seconds.
static FLASH: Mutex<Option<Instant>> = Mutex::new(None);
static FACE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(true);
const FLASH_FOR: Duration = Duration::from_secs(5);

fn file(dir: &Path) -> PathBuf {
    dir.join("timer.json")
}

fn now() -> i64 {
    crate::planner::now()
}

fn day(now: i64) -> i64 {
    (now + crate::planner::offset()).div_euclid(86_400)
}

fn with<R>(f: impl FnOnce(&mut Saved) -> R) -> R {
    let mut g = STATE.lock().unwrap();
    f(g.get_or_insert_with(Saved::default))
}

/// One writer at a time (the tick thread and the controller both save).
static SAVING: Mutex<()> = Mutex::new(());

fn save() {
    let Some(dir) = DIR.lock().unwrap().clone() else { return };
    let _one = SAVING.lock().unwrap_or_else(|e| e.into_inner());
    let s = with(|s| s.clone());
    let tmp = dir.join("timer.json.tmp");
    if crate::settings::write_synced(&tmp, &serde_json::to_vec(&s).unwrap_or_default()).is_ok() {
        let _ = std::fs::rename(tmp, file(&dir));
    }
}

/// Show it on the display and the ring (settings «таймер на колонке»).
pub fn set_face(on: bool) {
    FACE.store(on, std::sync::atomic::Ordering::Release);
}

pub fn current() -> Option<Timer> {
    with(|s| s.timer.clone())
}

/// For the display: four digits, or None when there is nothing to show.
pub fn display_text() -> Option<String> {
    if !FACE.load(std::sync::atomic::Ordering::Acquire) {
        return None;
    }
    if FLASH.lock().unwrap().is_some_and(|t| t.elapsed() < FLASH_FOR) {
        return Some("0000".into());
    }
    current().map(|t| face(left(&t, now())))
}

/// For the ring: (fraction left 0..1, pomodoro work phase, paused, ended just now).
pub fn ring_state() -> Option<(f32, bool, bool, bool)> {
    if !FACE.load(std::sync::atomic::Ordering::Acquire) {
        return None;
    }
    if FLASH.lock().unwrap().is_some_and(|t| t.elapsed() < FLASH_FOR) {
        return Some((0.0, false, false, true));
    }
    current().map(|t| {
        let l = left(&t, now());
        (l as f32 / t.total.max(1) as f32, t.phase != "break" && t.phase != "long", t.paused.is_some(), false)
    })
}

pub fn json() -> serde_json::Value {
    let n = now();
    let today = with(|s| if s.today.0 == day(n) { s.today.1 } else { 0 });
    match current() {
        Some(t) => serde_json::json!({
            "on": true, "kind": t.kind, "label": t.label, "phase": t.phase, "round": t.round, "rounds": t.cfg.rounds,
            "total": t.total, "left": left(&t, n), "paused": t.paused.is_some(), "today": today,
        }),
        None => serde_json::json!({"on": false, "today": today}),
    }
}

fn set(t: Option<Timer>) {
    let old = with(|s| std::mem::replace(&mut s.timer, t.clone()));
    if let Some(e) = event(old.as_ref(), t.as_ref()) {
        EVENTS.lock().unwrap().push(e);
    }
    save();
}

/// Events since the last call, oldest first.
pub fn take_events() -> Vec<&'static str> {
    std::mem::take(&mut *EVENTS.lock().unwrap())
}

pub fn start_plain(secs: u64, label: &str) -> String {
    let t = plain(secs, label, now());
    let text = format!("Таймер на {}", left_words(t.total));
    set(Some(t));
    text
}

pub fn start_pomodoro(cfg: Pomo, label: &str) -> String {
    let t = pomodoro(cfg, label, now());
    let text = format!("Помодоро: {} работы, потом перерыв {}", minutes(t.cfg.work), minutes(t.cfg.brk));
    set(Some(t));
    text
}

/// Voice and page commands; the answer to say / show.
pub fn command(v: &Voice, cfg: Pomo) -> String {
    let n = now();
    match v {
        Voice::Pomodoro => start_pomodoro(cfg, ""),
        Voice::Stop => match current() {
            None => "Таймер не запущен".into(),
            Some(t) => {
                set(None);
                if t.kind == "pomodoro" { "Помодоро остановлено".into() } else { "Таймер выключен".into() }
            }
        },
        Voice::Pause => match current() {
            None => "Таймер не запущен".into(),
            Some(mut t) => {
                pause(&mut t, n);
                let text = format!("Пауза, осталось {}", left_words(left(&t, n)));
                set(Some(t));
                text
            }
        },
        Voice::Resume => match current() {
            None => "Таймер не запущен".into(),
            Some(mut t) => {
                resume(&mut t, n);
                let text = format!("Продолжаю, осталось {}", left_words(left(&t, n)));
                set(Some(t));
                text
            }
        },
        Voice::Skip => match current() {
            None => "Таймер не запущен".into(),
            Some(t) => {
                let (nt, text, _) = next(&t, n);
                set(nt);
                text
            }
        },
        Voice::Left => match current() {
            None => "Таймер не запущен".into(),
            Some(t) => {
                let what = match t.phase.as_str() {
                    "work" => format!("До перерыва ({} круг из {})", t.round, t.cfg.rounds),
                    "break" | "long" => "До конца перерыва".into(),
                    _ => "На таймере".into(),
                };
                format!("{what} {}{}", left_words(left(&t, n)), if t.paused.is_some() { ", он на паузе" } else { "" })
            }
        },
    }
}

/// Loads the saved timer and watches it: at the end of a phase the speaker says
/// so and the ring / display flash. Ended while the speaker was off: said once
/// if under 10 minutes ago, otherwise dropped quietly.
pub fn run(dir: &Path) {
    let saved: Saved = crate::fsx::read_string(&file(dir)).and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default();
    *STATE.lock().unwrap() = Some(saved);
    *DIR.lock().unwrap() = Some(dir.to_owned());
    if let Some(t) = current().filter(|t| t.paused.is_none() && now() - t.ends > 600) {
        eprintln!("timer: {} ended while off, dropped", t.kind);
        // Quietly: no «done» scenario long after the fact.
        with(|s| s.timer = None);
        save();
    }
    thread::spawn(|| loop {
        thread::sleep(Duration::from_millis(500));
        let n = now();
        let Some(t) = current() else { continue };
        if t.paused.is_some() || n < t.ends {
            continue;
        }
        let (nt, text, worked) = next(&t, n);
        // Only if nobody changed it meanwhile: «стоп помодоро» or «пропусти» between
        // the read above and this write brought a stopped pomodoro back or skipped twice.
        let old = with(|s| {
            if s.timer.as_ref() != Some(&t) {
                return None;
            }
            if worked {
                let d = day(n);
                s.today = if s.today.0 == d { (d, s.today.1 + 1) } else { (d, 1) };
            }
            Some(std::mem::replace(&mut s.timer, nt.clone()))
        });
        let Some(old) = old else { continue };
        if let Some(e) = event(old.as_ref(), nt.as_ref()) {
            EVENTS.lock().unwrap().push(e);
        }
        save();
        *FLASH.lock().unwrap() = Some(Instant::now());
        eprintln!("timer: {} {} ended", t.kind, t.phase);
        crate::alice::say(text);
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_pomodoro_goes_work_break_and_a_long_break_at_the_end() {
        let cfg = Pomo { work: 25, brk: 5, long: 15, rounds: 2 };
        let t = pomodoro(cfg, "отчёт", 1000);
        assert_eq!((t.phase.as_str(), t.round, t.total, t.ends), ("work", 1, 1500, 2500));
        let (b, say, worked) = next(&t, 2500);
        let b = b.unwrap();
        assert!(worked && say.contains("Перерыв 5 минут") && say.contains("«отчёт»"), "{say}");
        assert_eq!((b.phase.as_str(), b.total), ("break", 300));
        let (w2, _, worked) = next(&b, 2800);
        let w2 = w2.unwrap();
        assert!(!worked);
        assert_eq!((w2.phase.as_str(), w2.round), ("work", 2));
        let (l, say, _) = next(&w2, 4300);
        let l = l.unwrap();
        assert_eq!((l.phase.as_str(), l.total), ("long", 900));
        assert!(say.contains("Большой перерыв 15 минут"), "{say}");
        let (end, say, _) = next(&l, 5200);
        assert!(end.is_none() && say.contains("2 круга"), "{say}");
    }

    #[test]
    fn scenarios_run_at_the_start_of_work_a_break_and_the_end() {
        let cfg = Pomo { work: 25, brk: 5, long: 15, rounds: 1 };
        let w = pomodoro(cfg, "", 0);
        assert_eq!(event(None, Some(&w)), Some("work"), "started");
        let mut p = w.clone();
        pause(&mut p, 10);
        assert_eq!(event(Some(&w), Some(&p)), None, "a pause is not a new phase");
        let (l, _, _) = next(&w, 1500);
        assert_eq!(event(Some(&w), l.as_ref()), Some("break"), "the long break too");
        let (end, _, _) = next(l.as_ref().unwrap(), 2400);
        assert_eq!(event(l.as_ref(), end.as_ref()), Some("done"));
        assert_eq!(event(Some(&w), None), Some("done"), "stopped by hand");
        let t = plain(60, "", 0);
        assert_eq!((event(None, Some(&t)), event(Some(&t), None)), (None, None), "a plain timer runs no scenarios");
    }

    #[test]
    fn pause_keeps_what_was_left() {
        let mut t = plain(600, "", 0);
        pause(&mut t, 100);
        assert_eq!(left(&t, 5000), 500, "time does not run while paused");
        resume(&mut t, 5000);
        assert_eq!((t.ends, left(&t, 5100)), (5500, 400));
        let (none, say, _) = next(&t, 5500);
        assert!(none.is_none() && say == "Таймер вышел");
    }

    #[test]
    fn display_and_words() {
        assert_eq!(face(25 * 60), "2500");
        assert_eq!(face(9), "0009");
        assert_eq!(face(2 * 3600 + 5 * 60), "0205", "over 100 minutes: hours and minutes");
        assert_eq!(left_words(61), "1 минута 1 секунда");
        assert_eq!(left_words(22 * 60), "22 минуты");
        assert_eq!(left_words(3600 + 11 * 60 + 30), "1 ч 11 минут");
        assert_eq!(minutes(21), "21 минута");
        assert_eq!(minutes(12), "12 минут");
    }

    #[test]
    fn only_pomodoro_phrases_are_ours() {
        assert_eq!(voice("запусти помодоро", false), Some(Voice::Pomodoro));
        assert_eq!(voice("останови помодоро", false), Some(Voice::Stop));
        assert_eq!(voice("поставь помодоро на паузу", true), Some(Voice::Pause));
        assert_eq!(voice("продолжи помодоро", true), Some(Voice::Resume));
        assert_eq!(voice("сколько осталось", true), Some(Voice::Left));
        assert_eq!(voice("сколько осталось", false), None, "Alice's timer then");
        assert_eq!(voice("поставь таймер на 5 минут", true), None, "Alice's timers stay hers");
        assert_eq!(voice("пропусти перерыв", true), Some(Voice::Skip));
    }
}
