//! soundya's alarm clock that cannot be slept through (the developer build for now).
//!
//! «Алиса, разбуди меня в 7 под мои лайки». At the time:
//!   1. music — the chosen SoundCloud source (likes, recommendations, a playlist, a search),
//!      louder with every round;
//!   2. Alice — the music stops, Alice says it is time to get up;
//!   3. «Вы проснулись? Скажите: Алиса, я проснулся» — and if nobody answers, back to 1.
//! «Стоп», «пауза», the ▶‖ button do not end it. «Алиса, я проснулся» asks a small sum
//! («сколько будет 7 плюс 5?»): only the right answer switches it off, so a sleepy «да»
//! does not. «Ещё пять минут» snoozes, twice at most. A restart of soundya (power cut)
//! keeps ringing where it was; an alarm missed while the speaker was off rings if it is
//! less than half an hour late.
//!
//! The state machine (`tick`, `voice_command`) is pure: it returns actions, run by `run`
//! and by the controller (`execute`), so it is tested without a speaker.

use std::{
    path::{Path, PathBuf},
    sync::{mpsc::Sender, Mutex},
    thread,
    time::Duration,
};

use serde::{Deserialize, Serialize};

use crate::{alice::Intent, planner, Cmd};

/// Seconds of each stage.
pub const MUSIC_FOR: i64 = 240;
pub const ALICE_FOR: i64 = 45;
pub const CHECK_FOR: i64 = 90;
pub const QUESTION_FOR: i64 = 60;
pub const SNOOZE_FOR: i64 = 300;
pub const MAX_SNOOZES: u8 = 2;
/// A restart within this long after the alarm started goes on ringing.
const RESUME_FOR: i64 = 2 * 3600;
/// An alarm missed while soundya was off still rings when it is this late at most.
const LATE_OK: i64 = 30 * 60;
/// Volume (0..=10) of the first round; each next round is one step louder.
const FIRST_VOLUME: i32 = 5;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Alarm {
    pub id: u64,
    pub hour: u8,
    pub minute: u8,
    /// once | daily | weekdays | weekend
    pub days: String,
    /// "" (whatever soundya plays by default) | likes | recs | playlist:<name> | play:<query>
    pub source: String,
    /// When it rings next (unix seconds).
    pub next: i64,
    pub on: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Stage {
    Music,
    Alice,
    Check,
    Question,
    Snoozed,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Ringing {
    pub alarm: u64,
    pub started: i64,
    pub stage: Stage,
    pub until: i64,
    pub round: u8,
    pub snoozes: u8,
    /// The sum asked after «я проснулся».
    pub question: Option<(u8, u8)>,
    /// Alice's volume before it rang, given back when it is switched off.
    pub volume_before: Option<i32>,
    pub source: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Saved {
    pub alarms: Vec<Alarm>,
    pub ringing: Option<Ringing>,
}

/// What the state machine wants done.
#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    Music(String),
    StopMusic,
    Say(String),
    Volume(i32),
    Restore(Option<i32>),
    /// The morning after the right answer: greeting, weather, plans, news (morning.rs).
    Morning,
}

/// «Утро после будильника» switched on in the settings.
/// Developer build only for now (the user build switches the alarm off as before).
static MORNING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(cfg!(feature = "dev"));

pub fn set_morning(on: bool) {
    MORNING.store(on && cfg!(feature = "dev"), std::sync::atomic::Ordering::Release);
}

// ------------------------------------------------------------------ time

/// The next time `hour:minute` comes on one of `days`, after `after`.
pub fn next_time(hour: u8, minute: u8, days: &str, after: i64, off: i64) -> i64 {
    let (today, _) = planner::local(after, off);
    let secs = i64::from(hour) * 3600 + i64::from(minute) * 60;
    for d in 0..8 {
        let day = today + d;
        let t = day * 86_400 + secs - off;
        if t <= after {
            continue;
        }
        let wd = planner::weekday(day); // 0 = Monday
        let fits = match days {
            "weekdays" => wd < 5,
            "weekend" => wd >= 5,
            _ => true,
        };
        if fits {
            return t;
        }
    }
    after + 86_400
}

fn hm(hour: u8, minute: u8) -> String {
    format!("{hour}:{minute:02}")
}

fn days_word(days: &str) -> &'static str {
    match days {
        "daily" => "каждый день",
        "weekdays" => "по будням",
        "weekend" => "по выходным",
        _ => "",
    }
}

fn source_word(source: &str) -> String {
    match source {
        "" => "под вашу музыку".into(),
        "likes" => "под ваши лайки".into(),
        "recs" => "под рекомендации".into(),
        s if s.starts_with("playlist:") => format!("под плейлист {}", &s["playlist:".len()..]),
        s if s.starts_with("play:") => format!("под {}", &s["play:".len()..]),
        _ => String::new(),
    }
}

/// The SoundCloud command for a source.
pub fn source_intent(source: &str) -> Intent {
    match source {
        "likes" => Intent::Likes,
        "recs" => Intent::Recommend,
        s if s.starts_with("playlist:") => Intent::MyPlaylist(s["playlist:".len()..].to_owned()),
        s if s.starts_with("play:") => Intent::Play(s["play:".len()..].to_owned()),
        _ => Intent::Default,
    }
}

/// «под мои лайки» → likes, «под рекомендации» → recs, «под плейлист утро» → playlist:утро, else play:<it>.
pub fn parse_source(words: &str) -> String {
    let w = words.trim();
    // From the panel it comes typed by hand: «Плейлист Утро» is a playlist too.
    let l = w.to_lowercase();
    let after = |prefix: &str| l.starts_with(prefix).then(|| w.chars().skip(prefix.chars().count()).collect::<String>());
    match l.as_str() {
        "" => String::new(),
        "лайки" | "мои лайки" | "любимое" | "мою музыку" | "мои треки" => "likes".into(),
        "рекомендации" | "мои рекомендации" | "волну" | "мою волну" | "что нибудь новое" | "что-нибудь новое" => "recs".into(),
        _ => match after("плейлист ").or_else(|| after("мой плейлист ")) {
            Some(p) => format!("playlist:{}", p.trim()),
            None => format!("play:{w}"),
        },
    }
}

// ------------------------------------------------------------------ voice

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Voice {
    /// «Разбуди меня в 7 под мои лайки»: hour, minute, days, source.
    Set(u8, u8, String, String),
    /// «Во сколько?»: a request without a time.
    NoTime,
    List,
    /// Cancel: all (None) or the one at this time.
    Cancel(Option<(u8, u8)>),
    /// While ringing: «я проснулся», «выключи будильник».
    Awake,
    /// While ringing: a number (the answer to the sum).
    Answer(u32),
    Snooze,
    /// While ringing: «стоп», «пауза», «хватит» — not the way to switch it off.
    Stop,
}

fn number(n: &str) -> Option<u32> {
    let n = n.trim().trim_end_matches('.');
    if let Ok(v) = n.parse() {
        return Some(v);
    }
    const W: [&str; 21] = [
        "ноль", "один", "два", "три", "четыре", "пять", "шесть", "семь", "восемь", "девять", "десять", "одиннадцать",
        "двенадцать", "тринадцать", "четырнадцать", "пятнадцать", "шестнадцать", "семнадцать", "восемнадцать",
        "девятнадцать", "двадцать",
    ];
    // «равно восьми»: the oblique forms.
    const OBLIQUE: [&str; 21] = [
        "нуля", "одному", "двум", "трём", "четырём", "пяти", "шести", "семи", "восьми", "девяти", "десяти", "одиннадцати",
        "двенадцати", "тринадцати", "четырнадцати", "пятнадцати", "шестнадцати", "семнадцати", "восемнадцати",
        "девятнадцати", "двадцати",
    ];
    W.iter().position(|w| *w == n).or_else(|| OBLIQUE.iter().position(|w| *w == n || w.replace('ё', "е") == n)).map(|i| i as u32)
}

/// Phrases that are the alarm's. `ringing`: one is ringing now (then «стоп», numbers and
/// «я проснулся» are ours too).
pub fn voice(n: &str, ringing: bool, now: i64, off: i64) -> Option<Voice> {
    let n = n.trim().trim_end_matches(['.', '!', '?']).trim();
    if ringing {
        const AWAKE: [&str; 12] = [
            "я проснулся", "я проснулась", "проснулся", "проснулась", "встаю", "я встал", "я встала", "я встаю",
            "выключи будильник", "отключи будильник", "останови будильник", "я не сплю",
        ];
        if AWAKE.contains(&n) {
            return Some(Voice::Awake);
        }
        if ["еще 5 минут", "ещё 5 минут", "еще пять минут", "ещё пять минут", "отложи будильник", "дай поспать", "еще немного", "ещё немного"].contains(&n) {
            return Some(Voice::Snooze);
        }
        if ["стоп", "пауза", "хватит", "выключи", "выключись", "замолчи", "тише", "стоп музыка", "останови"].contains(&n) {
            return Some(Voice::Stop);
        }
        // After those («ещё пять минут» has a number too). «8», «восемь», «ответ восемь», «будет восемь», «это восемь», «восемь будет»: one number in the phrase.
        let nums: Vec<u32> = n.split_whitespace().filter_map(number).collect();
        let short = n.split_whitespace().count() <= 4;
        if let ([v], true) = (nums.as_slice(), short) {
            return Some(Voice::Answer(*v));
        }
    }
    let about = n.contains("будильник") || n.starts_with("разбуди");
    if !about {
        return None;
    }
    if ["какие", "список", "на когда", "на сколько", "когда будильник", "когда прозвенит"].iter().any(|w| n.contains(w)) {
        return Some(Voice::List);
    }
    if ["отмени", "удали", "убери", "выключи", "отключи", "сними"].iter().any(|w| n.starts_with(w)) {
        if n.contains("все") || n.contains("всё") {
            return Some(Voice::Cancel(None));
        }
        let at = planner::parse_when_full(&n.replace(" на ", " в "), now, off).filter(|w| w.has_time).map(|w| {
            let (_, s) = planner::local(w.due, off);
            ((s / 3600) as u8, (s % 3600 / 60) as u8)
        });
        return Some(Voice::Cancel(at));
    }
    // «Поставь будильник на 7 утра под мои лайки», «разбуди меня в 7:30 по будням».
    let (when_part, source) = match n.split_once(" под ") {
        Some((a, b)) => (a, parse_source(b)),
        None => (n, String::new()),
    };
    let when_text = when_part.replace(" на ", " в ").replace("будильник", " ");
    match planner::parse_when_full(&when_text, now, off).filter(|w| w.has_time) {
        Some(w) => {
            let (_, s) = planner::local(w.due, off);
            let days = match w.repeat.as_str() {
                "daily" => "daily",
                "weekdays" => "weekdays",
                _ if n.contains("выходн") => "weekend",
                _ => "once",
            };
            Some(Voice::Set((s / 3600) as u8, (s % 3600 / 60) as u8, days.into(), source))
        }
        None => Some(Voice::NoTime),
    }
}

// ------------------------------------------------------------------ the state machine

fn question(seed: i64) -> (u8, u8) {
    let s = seed.unsigned_abs();
    (3 + (s % 7) as u8, 2 + (s / 7 % 8) as u8)
}

fn round_volume(round: u8) -> i32 {
    (FIRST_VOLUME + i32::from(round.saturating_sub(1))).min(crate::volume::MAX)
}

fn wake_words(now: i64, off: i64) -> String {
    let (_, s) = planner::local(now, off);
    format!("Доброе утро! Уже {}. Пора вставать!", hm((s / 3600) as u8, (s % 3600 / 60) as u8))
}

fn music_round(r: &mut Ringing, now: i64) -> Vec<Action> {
    r.stage = Stage::Music;
    r.until = now + MUSIC_FOR;
    r.question = None;
    vec![Action::Volume(round_volume(r.round)), Action::Music(r.source.clone())]
}

/// One step of time: starts an alarm that is due, moves a ringing one to its next stage.
pub fn tick(s: &mut Saved, now: i64, off: i64, volume: Option<i32>) -> Vec<Action> {
    if let Some(r) = s.ringing.as_mut() {
        if now < r.until {
            return vec![];
        }
        return match r.stage {
            Stage::Music => {
                r.stage = Stage::Alice;
                r.until = now + ALICE_FOR;
                vec![Action::StopMusic, Action::Volume(round_volume(r.round + 1)), Action::Say(wake_words(now, off))]
            }
            Stage::Alice => {
                r.stage = Stage::Check;
                r.until = now + CHECK_FOR;
                vec![Action::Say("Вы проснулись? Скажите: Алиса, я проснулся".into())]
            }
            // Nobody answered, or the sum was left unanswered: again, louder.
            Stage::Check | Stage::Question => {
                r.round = r.round.saturating_add(1);
                music_round(r, now)
            }
            Stage::Snoozed => music_round(r, now),
        };
    }
    let mut out = vec![];
    for a in s.alarms.iter_mut().filter(|a| a.on && a.next <= now) {
        let late = now - a.next;
        let source = a.source.clone();
        let id = a.id;
        // The next time (a single alarm is done once it rang or was missed).
        if a.days == "once" {
            a.on = false;
        } else {
            a.next = next_time(a.hour, a.minute, &a.days, now, off);
        }
        if late <= LATE_OK && out.is_empty() {
            let mut r = Ringing { alarm: id, started: now, stage: Stage::Music, until: now, round: 1, snoozes: 0, question: None, volume_before: volume, source };
            out = music_round(&mut r, now);
            s.ringing = Some(r);
        }
    }
    out
}

/// After a restart: a ringing alarm goes on if it started not long ago (a power cut is
/// not a way to switch it off); an old one is dropped.
pub fn resume(s: &mut Saved, now: i64) -> Vec<Action> {
    match s.ringing.as_mut() {
        Some(r) if now - r.started <= RESUME_FOR => music_round(r, now),
        Some(_) => {
            s.ringing = None;
            vec![]
        }
        None => vec![],
    }
}

/// A voice command (or the panel's): what to say and what to do.
pub fn voice_command(s: &mut Saved, v: &Voice, now: i64, off: i64) -> (String, Vec<Action>) {
    match v {
        Voice::Set(h, m, days, source) => {
            let id = s.alarms.iter().map(|a| a.id).max().unwrap_or(0) + 1;
            let next = next_time(*h, *m, days, now, off);
            s.alarms.retain(|a| !(a.hour == *h && a.minute == *m && a.days == *days));
            s.alarms.push(Alarm { id, hour: *h, minute: *m, days: days.clone(), source: source.clone(), next, on: true });
            let when = match days.as_str() {
                "once" => {
                    let (d, _) = planner::local(next, off);
                    let (today, _) = planner::local(now, off);
                    if d == today { "сегодня".to_owned() } else { "завтра".to_owned() }
                }
                d => days_word(d).to_owned(),
            };
            (format!("Разбужу {when} в {} {}. Выключить можно будет только словами: Алиса, я проснулся", hm(*h, *m), source_word(source)), vec![])
        }
        Voice::NoTime => ("Во сколько разбудить? Скажите, например: Алиса, разбуди меня в 7 утра".into(), vec![]),
        Voice::List => {
            let on: Vec<String> = s.alarms.iter().filter(|a| a.on).map(|a| format!("{} {}", hm(a.hour, a.minute), days_word(&a.days)).trim().to_owned()).collect();
            (if on.is_empty() { "Будильников нет".into() } else { format!("Будильники: {}", on.join(", ")) }, vec![])
        }
        Voice::Cancel(at) => {
            if s.ringing.is_some() {
                return ("Сейчас будильник звонит. Чтобы выключить, скажите: Алиса, я проснулся".into(), vec![]);
            }
            let before = s.alarms.len();
            match at {
                None => s.alarms.clear(),
                Some((h, m)) => s.alarms.retain(|a| !(a.hour == *h && a.minute == *m)),
            }
            let n = before - s.alarms.len();
            (if n == 0 { "Такого будильника нет".into() } else if at.is_none() { "Все будильники удалены".into() } else { "Будильник удалён".into() }, vec![])
        }
        Voice::Awake => match s.ringing.as_mut() {
            None => ("Будильник сейчас не звонит".into(), vec![]),
            Some(r) => {
                let (a, b) = question(now + i64::from(r.round));
                r.stage = Stage::Question;
                r.until = now + QUESTION_FOR;
                r.question = Some((a, b));
                (format!("Проверим. Сколько будет {a} плюс {b}?"), vec![Action::StopMusic])
            }
        },
        Voice::Answer(x) => match s.ringing.clone() {
            Some(r) if r.stage == Stage::Question => {
                let (a, b) = r.question.unwrap_or((0, 0));
                if *x == u32::from(a) + u32::from(b) {
                    s.ringing = None;
                    if MORNING.load(std::sync::atomic::Ordering::Acquire) {
                        // The morning itself says «доброе утро».
                        ("Правильно! Будильник выключен".into(), vec![Action::StopMusic, Action::Restore(r.volume_before), Action::Morning])
                    } else {
                        ("Правильно! Доброе утро, будильник выключен".into(), vec![Action::StopMusic, Action::Restore(r.volume_before)])
                    }
                } else {
                    let (c, d) = question(now + 1);
                    if let Some(rr) = s.ringing.as_mut() {
                        rr.question = Some((c, d));
                        rr.until = now + QUESTION_FOR;
                    }
                    (format!("Неверно. Сколько будет {c} плюс {d}?"), vec![])
                }
            }
            Some(_) => ("Сначала скажите: Алиса, я проснулся".into(), vec![]),
            None => ("Будильник сейчас не звонит".into(), vec![]),
        },
        Voice::Snooze => match s.ringing.as_mut() {
            None => ("Будильник сейчас не звонит".into(), vec![]),
            Some(r) if r.snoozes >= MAX_SNOOZES => ("Больше откладывать нельзя. Скажите: Алиса, я проснулся".into(), vec![]),
            Some(r) => {
                r.snoozes += 1;
                r.stage = Stage::Snoozed;
                r.until = now + SNOOZE_FOR;
                let left = MAX_SNOOZES - r.snoozes;
                (format!("Хорошо, ещё пять минут. Отложить можно ещё {left} раз"), vec![Action::StopMusic])
            }
        },
        Voice::Stop => ("Будильник так не выключается. Скажите: Алиса, я проснулся".into(), vec![]),
    }
}

// ------------------------------------------------------------------ state and the thread

static STATE: Mutex<Option<Saved>> = Mutex::new(None);
static DIR: Mutex<Option<PathBuf>> = Mutex::new(None);
static TX: Mutex<Option<Sender<Cmd>>> = Mutex::new(None);
static SAVING: Mutex<()> = Mutex::new(());

fn with<T>(f: impl FnOnce(&mut Saved) -> T) -> T {
    let mut g = STATE.lock().unwrap_or_else(|e| e.into_inner());
    f(g.get_or_insert_with(Saved::default))
}

fn save() {
    let Some(dir) = DIR.lock().unwrap().clone() else { return };
    let _one = SAVING.lock().unwrap_or_else(|e| e.into_inner());
    let data = with(|s| serde_json::to_vec(s).unwrap_or_default());
    let tmp = dir.join("alarm.json.tmp");
    if crate::settings::write_synced(&tmp, &data).is_ok() {
        let _ = std::fs::rename(tmp, dir.join("alarm.json"));
    }
}

/// An alarm is ringing (any stage): «стоп», ▶‖ and the panel's pause do not stop it.
pub fn ringing() -> bool {
    with(|s| s.ringing.is_some())
}

/// The music stage is on: a pause there is undone.
pub fn music_on() -> bool {
    with(|s| s.ringing.as_ref().is_some_and(|r| r.stage == Stage::Music))
}

pub fn execute(actions: Vec<Action>) {
    let tx = TX.lock().unwrap().clone();
    for a in actions {
        match a {
            Action::Music(src) => {
                if let Some(tx) = &tx {
                    let _ = tx.send(Cmd::Voice(source_intent(&src)));
                }
            }
            Action::StopMusic => {
                if let Some(tx) = &tx {
                    let _ = tx.send(Cmd::Voice(Intent::Pause));
                }
            }
            Action::Say(t) => crate::alice::say(t),
            Action::Volume(v) => {
                if let Err(e) = crate::volume::set(v) {
                    eprintln!("alarm: volume: {e}");
                }
            }
            Action::Restore(Some(v)) => {
                let _ = crate::volume::set(v);
            }
            Action::Restore(None) => {}
            Action::Morning => {
                if let Some(tx) = &tx {
                    let (reply, _) = std::sync::mpsc::channel();
                    let _ = tx.send(Cmd::Control("morning run".into(), reply));
                }
            }
        }
    }
}

/// The controller's side of a voice command.
pub fn command(v: &Voice) -> String {
    let (text, actions) = with(|s| voice_command(s, v, planner::now(), planner::offset()));
    eprintln!("alarm: {v:?} → {text}");
    save();
    execute(actions);
    text
}

/// The panel: «alarm json», «alarm add {hour,minute,days,source}», «alarm del <id>», «alarm awake», «alarm answer <n>».
pub fn panel(arg: &str) -> String {
    let (what, rest) = arg.split_once(' ').unwrap_or((arg, ""));
    let v = match what {
        "json" => return with(|s| serde_json::json!({ "alarms": s.alarms, "ringing": s.ringing, "now": planner::now() })).to_string(),
        "add" => {
            let j: serde_json::Value = serde_json::from_str(rest).unwrap_or_default();
            let (h, m) = (j["hour"].as_u64().unwrap_or(99), j["minute"].as_u64().unwrap_or(99));
            if h > 23 || m > 59 {
                return "error: время — часы 0–23 и минуты 0–59".into();
            }
            let days = j["days"].as_str().filter(|d| ["once", "daily", "weekdays", "weekend"].contains(d)).unwrap_or("once");
            Voice::Set(h as u8, m as u8, days.into(), parse_source(j["source"].as_str().unwrap_or("")))
        }
        "del" => {
            let id: u64 = rest.trim().parse().unwrap_or(0);
            if ringing() {
                return "error: сейчас будильник звонит: выключить его можно, ответив на вопрос".into();
            }
            let n = with(|s| {
                let before = s.alarms.len();
                s.alarms.retain(|a| a.id != id);
                before - s.alarms.len()
            });
            save();
            return if n == 0 { "error: нет такого будильника".into() } else { "OK: удалён".into() };
        }
        "awake" => Voice::Awake,
        "answer" => Voice::Answer(rest.trim().parse().unwrap_or(u32::MAX)),
        "snooze" => Voice::Snooze,
        _ => return "usage: alarm json | add {…} | del <id> | awake | answer <n> | snooze".into(),
    };
    format!("OK: {}", command(&v))
}

pub fn run(dir: &Path, tx: Sender<Cmd>) {
    let saved: Saved = crate::fsx::read_string(&dir.join("alarm.json")).and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default();
    *STATE.lock().unwrap() = Some(saved);
    *DIR.lock().unwrap() = Some(dir.to_owned());
    *TX.lock().unwrap() = Some(tx);
    thread::spawn(|| {
        // Let the player come up after a restart, then go on ringing if it was.
        thread::sleep(Duration::from_secs(20));
        let actions = with(|s| resume(s, planner::now()));
        if !actions.is_empty() {
            eprintln!("alarm: ringing again after a restart");
            save();
            execute(actions);
        }
        loop {
            thread::sleep(Duration::from_secs(2));
            let volume = crate::volume::level();
            let actions = with(|s| tick(s, planner::now(), planner::offset(), volume));
            if !actions.is_empty() {
                eprintln!("alarm: {actions:?}");
                save();
                execute(actions);
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    // Monday 2026-11-09 06:00 MSK.
    const OFF: i64 = 3 * 3600;
    const MON_6: i64 = 1_794_214_800 - 6 * 3600;

    #[test]
    fn next_times() {
        // 07:00 today, 05:00 tomorrow.
        assert_eq!(next_time(7, 0, "once", MON_6, OFF), MON_6 + 3600);
        assert_eq!(next_time(5, 0, "daily", MON_6, OFF), MON_6 + 23 * 3600);
        // Weekend from a Monday: Saturday.
        assert_eq!(next_time(9, 0, "weekend", MON_6, OFF), MON_6 + 5 * 86_400 + 3 * 3600);
        // Weekdays on Friday evening: Monday.
        let fri_22 = MON_6 + 4 * 86_400 + 16 * 3600;
        assert_eq!(next_time(7, 0, "weekdays", fri_22, OFF), MON_6 + 7 * 86_400 + 3600);
    }

    #[test]
    fn phrases() {
        let v = |n: &str, r: bool| voice(n, r, MON_6, OFF);
        assert_eq!(v("разбуди меня в 7 под мои лайки", false), Some(Voice::Set(7, 0, "once".into(), "likes".into())));
        assert_eq!(v("поставь будильник на 7:30", false), Some(Voice::Set(7, 30, "once".into(), String::new())));
        assert_eq!(v("поставь будильник на 8 утра по будням", false), Some(Voice::Set(8, 0, "weekdays".into(), String::new())));
        assert_eq!(v("будильник на 9 каждый день под плейлист утро", false), Some(Voice::Set(9, 0, "daily".into(), "playlist:утро".into())));
        assert_eq!(v("разбуди меня в 7 под bonobo", false), Some(Voice::Set(7, 0, "once".into(), "play:bonobo".into())));
        assert_eq!(v("поставь будильник", false), Some(Voice::NoTime));
        assert_eq!(v("какие у меня будильники", false), Some(Voice::List));
        assert_eq!(v("отмени все будильники", false), Some(Voice::Cancel(None)));
        assert_eq!(v("отмени будильник на 7", false), Some(Voice::Cancel(Some((7, 0)))));
        // While ringing.
        assert_eq!(v("стоп", true), Some(Voice::Stop));
        assert_eq!(v("я проснулся", true), Some(Voice::Awake));
        assert_eq!(v("выключи будильник", true), Some(Voice::Awake));
        assert_eq!(v("12", true), Some(Voice::Answer(12)));
        assert_eq!(v("двенадцать", true), Some(Voice::Answer(12)));
        assert_eq!(v("ещё пять минут", true), Some(Voice::Snooze));
        // Not ringing: «стоп» and numbers are not the alarm's.
        assert_eq!(v("стоп", false), None);
        assert_eq!(v("12", false), None);
        assert_eq!(v("включи bonobo", false), None);
    }

    fn rung() -> Saved {
        let mut s = Saved::default();
        voice_command(&mut s, &Voice::Set(7, 0, "once".into(), "likes".into()), MON_6, OFF);
        s
    }

    #[test]
    fn rings_cycles_until_awake() {
        let mut s = rung();
        let t7 = MON_6 + 3600;
        assert!(tick(&mut s, t7 - 10, OFF, Some(3)).is_empty(), "not yet");
        let a = tick(&mut s, t7, OFF, Some(3));
        assert_eq!(a, vec![Action::Volume(5), Action::Music("likes".into())]);
        assert!(!s.alarms[0].on, "a single alarm is done once it rang");
        // Music → Alice → «Вы проснулись?» → music again, louder.
        let a = tick(&mut s, t7 + MUSIC_FOR, OFF, None);
        assert_eq!(a[0], Action::StopMusic);
        assert!(matches!(&a[2], Action::Say(t) if t.contains("Пора вставать") && t.contains("7:04")));
        let a = tick(&mut s, t7 + MUSIC_FOR + ALICE_FOR, OFF, None);
        assert!(matches!(&a[0], Action::Say(t) if t.contains("Вы проснулись")));
        let t = t7 + MUSIC_FOR + ALICE_FOR + CHECK_FOR;
        assert_eq!(tick(&mut s, t, OFF, None), vec![Action::Volume(6), Action::Music("likes".into())], "ignored: again, louder");
        assert_eq!(s.ringing.as_ref().unwrap().round, 2);
        // «Стоп» does not end it.
        let (text, actions) = voice_command(&mut s, &Voice::Stop, t + 5, OFF);
        assert!(text.contains("я проснулся") && actions.is_empty() && s.ringing.is_some());
        // «Я проснулся» → a sum; a wrong answer → another sum; the right one ends it.
        let (text, _) = voice_command(&mut s, &Voice::Awake, t + 10, OFF);
        let (a, b) = s.ringing.as_ref().unwrap().question.unwrap();
        assert!(text.contains(&format!("{a} плюс {b}")));
        let (text, _) = voice_command(&mut s, &Voice::Answer(999), t + 15, OFF);
        assert!(text.starts_with("Неверно") && s.ringing.is_some());
        let (c, d) = s.ringing.as_ref().unwrap().question.unwrap();
        let (text, actions) = voice_command(&mut s, &Voice::Answer(u32::from(c) + u32::from(d)), t + 20, OFF);
        assert!(text.starts_with("Правильно") && s.ringing.is_none());
        assert_eq!(&actions[..2], &[Action::StopMusic, Action::Restore(Some(3))], "the volume comes back");
        // With «утро после будильника» on (the default) the morning follows.
        assert_eq!(actions.get(2), MORNING.load(std::sync::atomic::Ordering::Acquire).then_some(&Action::Morning));
    }

    #[test]
    fn an_unanswered_sum_rings_again() {
        let mut s = rung();
        let t7 = MON_6 + 3600;
        tick(&mut s, t7, OFF, None);
        voice_command(&mut s, &Voice::Awake, t7 + 30, OFF);
        let a = tick(&mut s, t7 + 30 + QUESTION_FOR, OFF, None);
        assert!(matches!(a.last(), Some(Action::Music(_))), "fell asleep on the question: music again");
    }

    #[test]
    fn snooze_twice_at_most() {
        let mut s = rung();
        let t7 = MON_6 + 3600;
        tick(&mut s, t7, OFF, None);
        for i in 0..MAX_SNOOZES {
            let (t, a) = voice_command(&mut s, &Voice::Snooze, t7 + 10 + i64::from(i), OFF);
            assert!(t.starts_with("Хорошо") && a == vec![Action::StopMusic]);
        }
        let (t, a) = voice_command(&mut s, &Voice::Snooze, t7 + 20, OFF);
        assert!(t.contains("нельзя") && a.is_empty());
        let a = tick(&mut s, t7 + 20 + SNOOZE_FOR, OFF, None);
        assert!(matches!(a.last(), Some(Action::Music(_))));
    }

    #[test]
    fn a_restart_does_not_switch_it_off() {
        let mut s = rung();
        let t7 = MON_6 + 3600;
        tick(&mut s, t7, OFF, None);
        let saved: Saved = serde_json::from_str(&serde_json::to_string(&s).unwrap()).unwrap();
        let mut back = saved.clone();
        assert!(matches!(resume(&mut back, t7 + 600).last(), Some(Action::Music(_))), "power cut 10 minutes later: rings on");
        let mut old = saved;
        assert!(resume(&mut old, t7 + RESUME_FOR + 1).is_empty() && old.ringing.is_none(), "hours later: dropped");
    }

    #[test]
    fn missed_alarms() {
        let mut s = rung();
        let t7 = MON_6 + 3600;
        assert!(!tick(&mut s, t7 + 20 * 60, OFF, None).is_empty(), "20 minutes late (speaker was off): still rings");
        let mut s = rung();
        assert!(tick(&mut s, t7 + 3 * 3600, OFF, None).is_empty() && s.ringing.is_none(), "3 hours late: not now");
        // A daily one moves to tomorrow either way.
        let mut s = Saved::default();
        voice_command(&mut s, &Voice::Set(7, 0, "daily".into(), String::new()), MON_6, OFF);
        tick(&mut s, t7 + 3 * 3600, OFF, None);
        assert_eq!(s.alarms[0].next, t7 + 86_400);
        assert!(s.alarms[0].on);
    }

    #[test]
    fn cancel_is_refused_while_ringing() {
        let mut s = rung();
        tick(&mut s, MON_6 + 3600, OFF, None);
        let (t, _) = voice_command(&mut s, &Voice::Cancel(None), MON_6 + 3610, OFF);
        assert!(t.contains("я проснулся") && s.ringing.is_some());
    }

    #[test]
    fn source_typed_on_the_panel() {
        assert_eq!(parse_source("Плейлист Первый"), "playlist:Первый");
        assert_eq!(parse_source("Мои лайки"), "likes");
        assert_eq!(parse_source("Bonobo"), "play:Bonobo");
    }

    #[test]
    fn answers_said_in_different_ways() {
        for (p, v) in [("8", 8), ("восемь", 8), ("ответ восемь", 8), ("это восемь", 8), ("будет 14", 14), ("равно восьми", 8), ("четырнадцать", 14)] {
            assert_eq!(voice(p, true, 0, 0), Some(Voice::Answer(v)), "{p}");
        }
        // Two numbers or a long phrase is not an answer.
        assert_ne!(voice("поставь будильник на 7 30", true, 0, 0), Some(Voice::Answer(7)));
    }
}
