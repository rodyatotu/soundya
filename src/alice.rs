//! Local Alice (Quasar) IPC on 127.0.0.1:9875.
//!
//! The wire format and message numbers are taken from spotiya
//! (https://github.com/klockky/spotiya, MIT): frames are a u32 little-endian
//! length followed by a protobuf-like QuasarMessage {252: payload, 253: type}.

use std::{
    collections::VecDeque,
    io::{self, Read, Write},
    net::{SocketAddr, TcpStream},
    sync::mpsc::Sender,
    thread,
    time::Duration,
};

use crate::{Cmd, Shared};

pub const ADDRESS: &str = "127.0.0.1:9875";
const MAX_FRAME: usize = 512 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Intent {
    Play(String),
    Playlist(String),
    /// «Включи альбом …»: a SoundCloud album (falls back to a playlist).
    Album(String),
    /// A music request for Alice's own service («… в яндекс музыке», or any music
    /// request in «только по слову саундклауд» mode): she plays it, SoundCloud pauses.
    ToAlice,
    Likes,
    MyPlaylist(String),
    Resume,
    Pause,
    Next,
    Previous,
    /// Shuffle the queue on/off.
    Shuffle(bool),
    /// Load something and play it shuffled («включи мои лайки вперемешку»).
    Shuffled(Box<Intent>),
    /// Repeat the current track on/off.
    Repeat(bool),
    /// Play the current track from the start.
    Restart,
    /// Tracks similar to the current one.
    Similar,
    /// Like the current track on SoundCloud.
    Like,
    /// Say what is playing.
    WhatPlaying,
    /// «Включи саундклауд» with nothing else: continue, or play the likes.
    Default,
    /// Personal recommendations.
    Recommend,
    /// Remove the current track from likes.
    Unlike,
    /// Add the current track to one of the account's playlists.
    AddToPlaylist(String),
    /// Jump within the track by this many ms (negative: back).
    Seek(i64),
    /// A command for the Windows PC (see pc.rs).
    Pc(PcAction),
    /// A command for the Android TV / projector (see tv.rs).
    Tv(TvAction),
    /// Smart Ducking by voice (ducking.rs).
    Duck(DuckCmd),
    /// «Повтори адрес»: where the panel is (hello.rs).
    Address,
    /// «Включи музыку в наушниках» (true) / «верни музыку на колонку» (false): Bluetooth output (bt.rs).
    BtOut(bool),
    /// «Подключись к новому устройству блютус»: find Bluetooth devices nearby, read their
    /// names aloud and ask which one to connect.
    BtFind,
    /// The device picked from that list: (address, name).
    BtConnect(String, String),
    /// A phrase that fits several devices and the settings say «спрашивать» (or there is no
    /// other way to tell): soundya asks «На каком устройстве?» and the answer runs it there.
    Ask(Ambig),
    /// «Говори тише / громче»: only her voice (voice.rs), by this many %.
    VoiceLevel(i8),
    /// «Да» / «нет» to an update sent from the app (update.rs: installed only after «да»).
    UpdateConfirm(bool),
    /// soundya's timer and pomodoro (timer.rs).
    Timer(crate::timer::Voice),
    /// The planner (planner.rs), when switched on.
    Planner(crate::planner::Voice),
    /// «Ответь …» to the last Telegram message (telegram.rs), asked to confirm.
    TgReply(String),
    /// «Да» / «нет» to a Telegram reply waiting for it.
    TgConfirm(bool),
    /// soundya's alarm that cannot be slept through (alarm.rs).
    Alarm(crate::alarm::Voice),
    /// A TV or PC command for the one with this name («выключи телевизор в спальне»).
    On(String, Box<Intent>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DuckCmd {
    /// «Приглуши звук»: soundya's music down until «верни громкость».
    Down,
    /// «Верни громкость»: ends manual and automatic ducking now.
    Restore,
    /// «Включи / отключи автоматическое приглушение».
    Auto(bool),
}

/// «Приглуши звук», «верни громкость», «отключи автоматическое приглушение».
/// «Сделай потише» stays Alice's: that is her volume, soundya does not touch it.
fn duck_command(text: &str) -> Option<DuckCmd> {
    let auto = text.contains("автоматическое приглушение") || text.contains("автоприглушение") || text.contains("умное приглушение");
    if auto {
        if text.starts_with("включи") || text.starts_with("верни") {
            return Some(DuckCmd::Auto(true));
        }
        if ["выключи", "отключи", "убери", "не "].iter().any(|v| text.starts_with(v)) {
            return Some(DuckCmd::Auto(false));
        }
        return None;
    }
    match text {
        "приглуши" | "приглуши звук" | "приглуши музыку" | "притуши музыку" | "притуши звук" => Some(DuckCmd::Down),
        "верни громкость" | "верни звук" | "верни громкость музыки" | "сними приглушение" | "убери приглушение" => {
            Some(DuckCmd::Restore)
        }
        _ => None,
    }
}

/// What was asked without saying where (see `Routes`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Ambig {
    /// «Сделай тише на пять»: (louder, the steps said). None: the device's usual step — one
    /// level on the speaker (as Alice's own «тише»), three presses on a TV.
    Volume(bool, Option<u8>),
    /// «Открой ютуб» / «закрой ютуб»: (open, the app as said, the whole phrase for a computer).
    App(bool, String, String),
}

/// Where a family of phrases goes when no device is named (settings route_volume, route_apps).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Route {
    /// Volume: the speaker itself (Alice's own, not taken). Apps: the only screen set up, a question if both are.
    #[default]
    Auto,
    Tv,
    Pc,
    Ask,
}

impl Route {
    pub fn from_setting(s: &str) -> Route {
        match s.trim() {
            "tv" => Route::Tv,
            "pc" => Route::Pc,
            "ask" => Route::Ask,
            _ => Route::Auto,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Routes {
    pub volume: Route,
    pub apps: Route,
    /// A computer is set up (the TV is in TvState.set_up).
    pub pc: bool,
}

#[cfg(not(test))]
static ROUTES: std::sync::Mutex<Routes> = std::sync::Mutex::new(Routes { volume: Route::Auto, apps: Route::Auto, pc: false });

#[cfg(not(test))]
fn routes_state<R>(f: impl FnOnce(&mut Routes) -> R) -> R {
    f(&mut ROUTES.lock().unwrap())
}

// As TV_STATE: each test has its own.
#[cfg(test)]
thread_local! {
    static ROUTES: std::cell::RefCell<Routes> = std::cell::RefCell::new(Routes::default());
}

#[cfg(test)]
fn routes_state<R>(f: impl FnOnce(&mut Routes) -> R) -> R {
    ROUTES.with(|s| f(&mut s.borrow_mut()))
}

pub fn set_routes(r: Routes) {
    routes_state(|s| *s = r);
}

fn routes() -> Routes {
    routes_state(|s| *s)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TvAction {
    /// A phrase about the TV that soundya did not understand: it says so itself, Alice's
    /// «не нашла умных устройств» / «не могу на этом устройстве» is cut off.
    Unclear,
    Wake,
    Sleep,
    /// Open an app: «открой ютуб на проекторе».
    Open(String),
    /// Search films and shows across apps: «найди фильм дюна на проекторе».
    Search(String),
    /// «Найди маму в 16 на рутубе»: search inside that app (spoken app name, query).
    SearchIn(String, String),
    /// Search YouTube: «включи на ютубе котиков на проекторе».
    YouTube(String),
    /// Remote-control keys: the Android key code and how many presses.
    Key(&'static str, u8),
    /// Connection check from the settings page.
    Ping,
    /// The panel's macro editor: launchable apps (package names).
    Apps,
    /// Open exactly this package (picked from Apps).
    OpenPackage(String),
    /// Close an app: spoken name or package.
    Close(String),
    /// Press what is on the screen by its words: «нажми историю на проекторе».
    Tap(String),
    /// Open an app, then a section of it: «открой историю в смарт тьюб» → (app, section).
    OpenIn(String, String),
    /// Play the first video of the list on the screen: «включи последнее видео из истории».
    PlayFirst,
    /// Open an app and press its main button: «открой впн и включи его».
    OpenAndStart(String),
    /// «Режим пульта» on / off: every short command goes to the TV until «выключи режим пульта».
    RemoteMode(bool),
}

/// Remote keys the macro editor offers: (name in a script, Android key code, label).
pub const TV_KEYS: [(&str, &str, &str); 14] = [
    ("home", "KEYCODE_HOME", "Домой"),
    ("back", "KEYCODE_BACK", "Назад"),
    ("up", "KEYCODE_DPAD_UP", "Вверх"),
    ("down", "KEYCODE_DPAD_DOWN", "Вниз"),
    ("left", "KEYCODE_DPAD_LEFT", "Влево"),
    ("right", "KEYCODE_DPAD_RIGHT", "Вправо"),
    ("ok", "KEYCODE_DPAD_CENTER", "ОК"),
    ("play", "KEYCODE_MEDIA_PLAY", "Воспроизвести"),
    ("pause", "KEYCODE_MEDIA_PAUSE", "Пауза"),
    ("playpause", "KEYCODE_MEDIA_PLAY_PAUSE", "Пауза/пуск"),
    ("next", "KEYCODE_MEDIA_NEXT", "Следующее"),
    ("volup", "KEYCODE_VOLUME_UP", "Громче"),
    ("voldown", "KEYCODE_VOLUME_DOWN", "Тише"),
    ("mute", "KEYCODE_VOLUME_MUTE", "Без звука"),
];

/// «на проекторе», «на телевизоре», «на тв», «на приставке» — or the device named
/// as the object («включи проектор»). The phrase without it, if it was there.
fn strip_tv(text: &str) -> Option<(String, bool)> {
    const PLACES: [&str; 13] = [
        " на телевизоре ", " на телеке ", " на телике ", " на тв ", " на проекторе ", " на приставке ",
        " в телевизоре ", " в проекторе ", " на телевизор ", " на проектор ", " на тиви ", " на андроид тв ", " на смарт тв ",
    ];
    const NOUNS: [&str; 14] = [
        "телевизор", "телевизора", "телевизору", "телек", "телик", "тв", "тиви", "проектор", "проектора",
        "проектору", "приставку", "приставка", "приставке", "приставки",
    ];
    let mut rest = format!(" {text} ");
    for place in PLACES {
        if rest.contains(place) {
            rest = rest.replacen(place, " ", 1);
            return Some((rest.split_whitespace().collect::<Vec<_>>().join(" "), false));
        }
    }
    let words: Vec<&str> = rest.split_whitespace().collect();
    let kept: Vec<&str> = words.iter().copied().filter(|w| !NOUNS.contains(w)).collect();
    (kept.len() < words.len()).then(|| (kept.join(" "), true))
}

const UP: &str = "KEYCODE_DPAD_UP";
const DOWN: &str = "KEYCODE_DPAD_DOWN";
const LEFT: &str = "KEYCODE_DPAD_LEFT";
const RIGHT: &str = "KEYCODE_DPAD_RIGHT";
const CENTER: &str = "KEYCODE_DPAD_CENTER";

/// «Выше», «ещё ниже», «наведись левее», «вниз на три»: (key, presses). Any other word: not this.
fn nav_key(words: &[&str]) -> Option<(&'static str, u8)> {
    const FILL: [&str; 25] = [
        "наведись", "наведи", "перейди", "сдвинься", "сдвинь", "подвинь", "подвинься", "иди", "двигайся", "курсор", "фокус", "еще", "на",
        "чуть", "немного", "раза", "раз", "повыше", "кнопку", "стрелка", "нажми", "жми", "стрелку", "клавишу", "листай",
    ];
    let mut code = None;
    let mut n = 1u8;
    for w in words {
        match *w {
            "выше" | "вверх" | "наверх" | "повыше" if code.is_none() => code = Some(UP),
            "ниже" | "вниз" | "пониже" => code = Some(DOWN),
            "левее" | "влево" | "налево" => code = Some(LEFT),
            "правее" | "вправо" | "направо" => code = Some(RIGHT),
            "два" | "2" | "дважды" => n = 2,
            "три" | "3" | "трижды" => n = 3,
            "четыре" | "4" => n = 4,
            "пять" | "5" => n = 5,
            w if FILL.contains(&w) => {}
            _ => return None,
        }
    }
    code.map(|c| (c, n))
}

/// «тише на 10», «сделай громче на пять делений»: (key, presses); None if any other word is there.
fn volume_steps(words: &[&str]) -> Option<(&'static str, u8)> {
    volume_said(words).map(|(code, n)| (code, n.unwrap_or(3)))
}

/// As volume_steps, with the number as said (None: no number).
fn volume_said(words: &[&str]) -> Option<(&'static str, Option<u8>)> {
    let mut code = None;
    let mut n: Option<u8> = None;
    for w in words {
        match *w {
            "тише" | "потише" | "убавь" | "уменьши" | "ниже" => code = Some("KEYCODE_VOLUME_DOWN"),
            "громче" | "погромче" | "прибавь" | "увеличь" | "добавь" => code = Some("KEYCODE_VOLUME_UP"),
            "сделай" | "на" | "звук" | "громкость" | "деления" | "делений" | "деление" | "пунктов" | "пункта" | "единиц" | "шагов" | "шага" | "еще" => {}
            w => n = Some(number_word(w)?),
        }
    }
    Some((code?, n.map(|n| n.clamp(1, 30))))
}

fn number_word(w: &str) -> Option<u8> {
    if let Ok(v) = w.parse::<u8>() {
        return Some(v);
    }
    const WORDS: [(&str, u8); 17] = [
        ("один", 1), ("одно", 1), ("два", 2), ("две", 2), ("три", 3), ("четыре", 4), ("пять", 5), ("шесть", 6), ("семь", 7),
        ("восемь", 8), ("девять", 9), ("десять", 10), ("пятнадцать", 15), ("двадцать", 20), ("тридцать", 30), ("пару", 2), ("парочку", 2),
    ];
    WORDS.iter().find(|(k, _)| *k == w).map(|(_, v)| *v)
}

/// First words of a question or talk about TVs: Alice's, not a command to the TV.
const TV_HERS: [&str; 16] = [
    "что", "какой", "какая", "какие", "как", "почему", "зачем", "сколько", "когда", "где", "кто", "расскажи", "посоветуй", "купи",
    "закажи", "найди",
];

/// Volume and apps said without a device. Volume: the speaker's own by default (Alice does
/// it, soundya does not take the phrase), the TV or a question by the setting. Apps («открой
/// ютуб», «включи кинопоиск», «закрой ютуб»): the TV, the computer or a question; with only
/// one of them set up — that one. Before this «включи ютуб» searched SoundCloud for «ютуб».
fn routed(n: &str) -> Option<Intent> {
    let r = routes();
    let tv_up = tv_state(|s| s.set_up);
    let words: Vec<&str> = n.split_whitespace().filter(|w| *w != "пожалуйста").collect();
    if let Some((code, said)) = volume_said(&words) {
        // «ниже» / «выше» alone are the remote's arrows (tv_unnamed took them already).
        let up = code == "KEYCODE_VOLUME_UP";
        return match r.volume {
            Route::Tv if tv_up => Some(Intent::Tv(TvAction::Key(code, said.unwrap_or(3)))),
            Route::Ask if tv_up => Some(Intent::Ask(Ambig::Volume(up, said))),
            _ => None,
        };
    }
    // «Включи премьеру» is Alice's own playlist, not the app PREMIER.
    if yandex_only(n).is_some() {
        return None;
    }
    let (verb, rest) = words.split_first()?;
    let open = match *verb {
        "открой" | "запусти" | "включи" | "открыть" | "вруби" => true,
        "закрой" | "закрыть" | "выруби" => false,
        _ => return None,
    };
    let app = rest.join(" ");
    // Only a known app's name and nothing after it: «включи ютуб», not «включи ютуб в профиле
    // работа», «открой ютуб на компьютере» (a named computer is its own rule below).
    if app.is_empty() || !crate::tv::screen_app(&app) || rest.len() > 2 || rest.iter().any(|w| matches!(*w, "на" | "в" | "во" | "и" | "с" | "для")) {
        return None;
    }
    let to_tv = || Some(Intent::Tv(if open { TvAction::Open(app.clone()) } else { TvAction::Close(app.clone()) }));
    // The computer's own parser knows «открой/закрой»; «включи ютуб» is said its way. What the
    // «Компьютер» menu leaves to Alice stays hers.
    let to_pc = || {
        let a = PcAction::Command(format!("{} {app}", if open { "открой" } else { "закрой" }));
        pc_allowed(n, &a, false, &pc_rules()).then_some(Intent::Pc(a))
    };
    let ask = || Some(Intent::Ask(Ambig::App(open, app.clone(), format!("{} {app}", if open { "открой" } else { "закрой" }))));
    match (r.apps, tv_up, r.pc) {
        (_, false, false) => None,
        (Route::Tv, true, _) | (Route::Auto, true, false) => to_tv(),
        (Route::Pc, _, true) | (Route::Auto, false, true) => to_pc(),
        (Route::Ask, true, true) | (Route::Auto, true, true) => ask(),
        (Route::Ask, true, false) | (Route::Pc, true, false) => to_tv(),
        (Route::Ask, false, true) | (Route::Tv, false, true) => to_pc(),
    }
}

/// A phrase that names the TV or the projector but means nothing soundya knows: still the
/// TV's (with a TV set up), so Alice does not answer about her smart home.
fn tv_unclear(text: &str) -> Option<Intent> {
    let (rest, _) = strip_tv(&normalize(text))?;
    // Questions and talk about TVs stay hers: «какой телевизор лучше», «что идёт по тв», «купи телевизор».
    rest.split_whitespace().next()?;
    (!tv_not_a_command(&rest)).then_some(Intent::Tv(TvAction::Unclear))
}

/// The remote's OK, said.
const OK_WORDS: [&str; 13] = ["ок", "окей", "ok", "нажми", "нажми ок", "выбери", "выбрать", "подтверди", "войди", "открой это", "жми", "выбери это", "нажми на это"];

/// The TV's follow-up state: after a command, for a minute and a half, «ещё», «выше», «нажми …»
/// without «на проекторе» are still the TV's (`session`: when, the last key for «ещё»);
/// «режим пульта» (`remote`: on since / last used; ends by «выключи режим пульта» or 20 minutes unused).
#[derive(Clone, Copy, Default)]
struct TvState {
    session: Option<(std::time::Instant, Option<&'static str>)>,
    remote: Option<std::time::Instant>,
    /// A TV is set up (settings): «подключи впн» is then the projector's, else the PC's.
    set_up: bool,
    /// The video on the TV was paused by a command (when): «продолжи» is then the TV's, not
    /// SoundCloud's — until it is continued or music is asked for.
    paused: Option<std::time::Instant>,
}
/// How long «продолжи» after «пауза на проекторе» still means the film.
const TV_PAUSED_FOR: std::time::Duration = std::time::Duration::from_secs(40 * 60);
const TV_SESSION_FOR: std::time::Duration = std::time::Duration::from_secs(90);
const TV_REMOTE_FOR: std::time::Duration = std::time::Duration::from_secs(20 * 60);

#[cfg(not(test))]
static TV_STATE: std::sync::Mutex<TvState> = std::sync::Mutex::new(TvState { session: None, remote: None, set_up: false, paused: None });

#[cfg(not(test))]
fn tv_state<R>(f: impl FnOnce(&mut TvState) -> R) -> R {
    f(&mut TV_STATE.lock().unwrap())
}

// Each test (its own thread) has its own: one test's «режим пульта» must not turn
// another's «пауза» into the TV's.
#[cfg(test)]
thread_local! {
    static TV_STATE: std::cell::RefCell<TvState> = std::cell::RefCell::new(TvState::default());
}

#[cfg(test)]
fn tv_state<R>(f: impl FnOnce(&mut TvState) -> R) -> R {
    TV_STATE.with(|s| f(&mut s.borrow_mut()))
}

/// A command for the TV went through: the follow-ups are its (`key`: what «ещё» repeats).
pub fn tv_used(key: Option<&'static str>) {
    tv_state(|s| {
        if s.remote.is_some_and(|at| at.elapsed() < TV_REMOTE_FOR) {
            s.remote = Some(std::time::Instant::now());
        }
        match key {
            Some("KEYCODE_MEDIA_PAUSE") => s.paused = Some(std::time::Instant::now()),
            // Continued, also by the remote's OK or the play/pause key.
            Some("KEYCODE_MEDIA_PLAY" | "KEYCODE_MEDIA_NEXT" | "KEYCODE_MEDIA_PREVIOUS" | "KEYCODE_DPAD_CENTER" | "KEYCODE_MEDIA_PLAY_PAUSE" | "KEYCODE_HOME") => s.paused = None,
            _ => {}
        }
        let key = key.or_else(|| s.session.and_then(|(_, k)| k));
        s.session = Some((std::time::Instant::now(), key));
    });
}

/// Music was asked for: «продолжи» is the music's again.
pub fn tv_forget_pause() {
    tv_state(|s| s.paused = None);
}

pub fn tv_remote(on: bool) {
    tv_state(|s| s.remote = on.then(std::time::Instant::now));
}

fn remote_on() -> bool {
    tv_state(|s| s.remote.is_some_and(|at| at.elapsed() < TV_REMOTE_FOR))
}

fn tv_session() -> Option<Option<&'static str>> {
    let s = tv_state(|s| s.session);
    if remote_on() {
        return Some(s.and_then(|(_, k)| k));
    }
    s.filter(|(at, _)| at.elapsed() < TV_SESSION_FOR).map(|(_, k)| k)
}

/// «Режим пульта» / «выключи режим пульта».
/// «пульт», «пульт ду», «открой пульт», «дай пульт телевизора», «режим пульта»…: a phrase
/// made only of «пульт» and words around it. Anything else in it («включи песню пульт»)
/// and it is not about the remote.
fn remote_phrase(n: &str) -> Option<bool> {
    const ON: [&str; 13] = ["включи", "открой", "дай", "покажи", "запусти", "давай", "режим", "переключись", "перейди", "стань", "сделай", "вруби", "активируй"];
    const OFF: [&str; 9] = ["выключи", "выйди", "отключи", "закончи", "хватит", "стоп", "убери", "закрой", "из"];
    const FILLER: [&str; 22] = [
        "алиса", "пожалуйста", "ду", "д", "у", "дистанционного", "управления", "на", "в", "от", "для", "мне",
        "телевизора", "телевизор", "телевизору", "тв", "проектора", "проектор", "телека", "телик", "телика", "мой",
    ];
    let words: Vec<&str> = n.split(|c: char| !c.is_alphanumeric()).filter(|w| !w.is_empty()).collect();
    if !words.iter().any(|w| w.starts_with("пульт")) {
        return None;
    }
    let known = |w: &&str| w.starts_with("пульт") || ON.contains(w) || OFF.contains(w) || FILLER.contains(w);
    if !words.iter().all(known) {
        return None;
    }
    Some(!words.iter().any(|w| OFF.contains(w)))
}

pub fn set_tv(on: bool) {
    tv_state(|s| s.set_up = on);
}

/// A TV's address is saved, but «Телевизор» is switched off in the settings: its commands
/// went to Alice, who answers «на этом устройстве не получится» (#SY-091FB968 — the owner
/// took it for soundya not working at all). Said in words instead.
static TV_OFF: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub fn set_tv_off(off: bool) {
    TV_OFF.store(off, std::sync::atomic::Ordering::Release);
}

/// Phrases for the TV that do not name it: «ещё выше» right after a TV command;
/// SmartTube (only the TV has it); «открой впн и включи его».
fn tv_unnamed(n: &str) -> Option<TvAction> {
    let as_tv = || tv(&format!("{n} на проекторе"));
    if let Some(on) = remote_phrase(n) {
        return Some(TvAction::RemoteMode(on));
    }
    // In the remote mode: «вправо», «ок», «пауза», «громче», «назад», «домой» — all the TV's.
    if remote_on() {
        if matches!(n, "еще" | "еще раз" | "снова" | "повтори") {
            return tv_session().flatten().map(|k| TvAction::Key(k, 1));
        }
        match n {
            "ок" | "окей" | "выбери" | "нажми" | "enter" => return Some(TvAction::Key(CENTER, 1)),
            "назад" => return Some(TvAction::Key("KEYCODE_BACK", 1)),
            "домой" => return Some(TvAction::Key("KEYCODE_HOME", 1)),
            _ => {}
        }
        // «включи bonobo», «поставь таймер», «запусти помодоро» stay music, Alice and
        // soundya's own: the remote mode took them all for «open an app» on the TV.
        let verb = n.split_whitespace().next().unwrap_or("");
        let not_an_app = ["включи", "поставь", "запусти", "играй", "сыграй", "напомни"].contains(&verb);
        match as_tv() {
            Some(TvAction::Open(_) | TvAction::YouTube(_)) if not_an_app => {}
            Some(a) => return Some(a),
            None => {}
        }
    }
    if crate::tv::tv_only_app(n) {
        return as_tv();
    }
    // «Найди маму в 16 на рутубе»: a video app only the TV has here — no «на телевизоре» needed.
    if crate::tv::search_in_phrase(n).is_some() && tv_state(|s| s.set_up) {
        return as_tv();
    }
    // The speaker has no screen: «нажми вверх», «наведись ниже», «влево» can only mean the TV's remote.
    let words: Vec<&str> = n.split_whitespace().collect();
    if let Some((code, k)) = nav_key(&words) {
        return Some(TvAction::Key(code, k));
    }
    // The film was paused by voice a while ago: «продолжи» continues it, not SoundCloud.
    if matches!(n, "продолжи" | "продолжай" | "продолжить" | "сними с паузы" | "играй дальше" | "продолжи воспроизведение" | "продолжи видео" | "продолжи фильм")
        && tv_state(|s| s.paused.is_some_and(|at| at.elapsed() < TV_PAUSED_FOR))
    {
        return Some(TvAction::Key("KEYCODE_MEDIA_PLAY", 1));
    }
    // Things only a screen has, with a TV set up: «включи первое видео», «следующая серия»,
    // «найди фильм дюна». Without them soundya searched SoundCloud for «первое видео». Not when
    // a computer is named or the browser is meant («найди фильм дюна в гугле»): the computer's.
    let pcs = matches!(pc_named(n), Some((_, true))) || words.iter().any(|w| ["гугл", "браузер", "интернет", "ютуб", "youtube"].iter().any(|s| w.starts_with(s)));
    if tv_state(|s| s.set_up) && !pcs {
        if let Some(a) = screen_only(&words, false) {
            return Some(a);
        }
        if matches!(as_tv(), Some(TvAction::PlayFirst)) {
            return Some(TvAction::PlayFirst);
        }
    }
    // «подключись к впн», «включи впн», «отключи впн»: the VPN app on the TV, its main button.
    if (n.contains("впн") || n.contains("vpn")) && tv_state(|s| s.set_up) {
        const VPN: [&str; 14] = [
            "подключись к впн", "подключи впн", "подключись к vpn", "включи впн", "включи vpn", "запусти впн", "открой впн и включи",
            "отключи впн", "выключи впн", "отключись от впн", "отключи vpn", "выключи vpn", "подключи vpn", "открой впн и подключи",
        ];
        if VPN.iter().any(|p| n.starts_with(p)) {
            return Some(TvAction::OpenAndStart("впн".into()));
        }
    }
    if (n.starts_with("открой ") || n.starts_with("запусти ")) && (n.contains(" и включи") || n.contains(" и подключи")) && n.contains("впн") {
        return as_tv();
    }
    let last = tv_session()?;
    if matches!(n, "еще" | "еще раз" | "снова" | "повтори" | "и еще" | "еще разок") {
        return last.map(|k| TvAction::Key(k, 1));
    }
    // In the session only the remote-like ones: not «пауза» or «дальше» (music has those too).
    match as_tv()? {
        a @ (TvAction::Tap(_) | TvAction::PlayFirst | TvAction::OpenIn(..)) => Some(a),
        a @ TvAction::Key(k, _) if [UP, DOWN, LEFT, RIGHT, CENTER, "KEYCODE_BACK"].contains(&k) => Some(a),
        _ => None,
    }
}

/// «Следующая серия», «предыдущее видео», «найди фильм дюна», «включи сериал друзья»: a screen's.
/// `named`: the TV was named in the phrase. Without it «включи кино …» is the band Кино on
/// SoundCloud and «включи аниме опенинги» is music; a search verb or the TV's name makes them films.
fn screen_only(words: &[&str], named: bool) -> Option<TvAction> {
    let video = |w: &&str| ["сери", "эпизод", "видео", "ролик", "выпуск"].iter().any(|s| w.starts_with(s));
    let dir = |w: &&str| w.starts_with("следующ") || w.starts_with("предыдущ") || w.starts_with("прошл");
    let filler = |w: &&str| ["включи", "поставь", "запусти", "покажи", "давай", "пожалуйста"].contains(w);
    let all = words.join(" ");
    // Only these words: «какой следующий выпуск новостей» is a question, not the remote.
    if words.iter().any(video) && words.iter().any(dir) && words.iter().all(|w| video(w) || dir(w) || filler(w)) {
        if words.iter().any(|w| w.starts_with("следующ")) {
            return Some(TvAction::Key("KEYCODE_MEDIA_NEXT", 1));
        }
        return Some(TvAction::Key("KEYCODE_MEDIA_PREVIOUS", 1));
    }
    // «найди/включи/поставь/покажи фильм … »: the title after the noun.
    let (verb, rest) = words.split_first()?;
    if !matches!(*verb, "найди" | "поищи" | "покажи" | "включи" | "поставь" | "запусти") {
        return None;
    }
    let search_verb = matches!(*verb, "найди" | "поищи" | "покажи");
    let noun = |w: &&str| ["фильм", "сериал", "мультфильм", "мультик"].contains(w) || ((named || search_verb) && ["кино", "аниме"].contains(w));
    let at = rest.iter().position(|w| noun(&w))?;
    // «включи музыку из фильма …» is music: the noun comes right after the verb.
    if at != 0 || all.contains("музык") || all.contains("песн") || all.contains("саундтрек") {
        return None;
    }
    let title = rest[1..].join(" ");
    // «поставь фильм на паузу», «включи фильм дальше» are the player's, not a title.
    if title.is_empty() || title.contains("пауз") || matches!(title.as_str(), "дальше" | "заново" | "сначала" | "снова" | "с начала" | "еще раз") {
        return None;
    }
    Some(TvAction::Search(title))
}

/// Questions, reminders and timers that only mention the TV are not commands to it.
fn tv_not_a_command(rest: &str) -> bool {
    const NOT_COMMANDS: [&str; 8] = ["напомни", "напоминай", "добавь", "запиши", "разбуди", "заведи", "напиши", "отправь"];
    let first = rest.split_whitespace().next().unwrap_or("");
    TV_HERS.contains(&first) || NOT_COMMANDS.contains(&first) || rest.contains("таймер") || rest.contains("будильник") || rest.contains("напомин")
}

/// The TV is named as a place («… на проекторе») but `tv()` knows no such command: still the
/// TV's — its pause, seek and «закрой», else Unclear. Before this the phrase went on to the
/// music rules: «перемотай на 30 секунд на проекторе» wound SoundCloud, «закрой ютуб на
/// проекторе» was sent to the computer.
fn tv_named_rest(n: &str, or_unclear: bool) -> Option<TvAction> {
    let (rest, as_object) = strip_tv(n)?;
    if as_object {
        return None;
    }
    let words: Vec<&str> = rest.split_whitespace().filter(|w| *w != "пожалуйста").collect();
    let first = *words.first()?;
    let all = words.join(" ");
    if tv_not_a_command(&all) {
        return None;
    }
    let _ = first;
    let key = |code: &'static str, n: u8| Some(TvAction::Key(code, n));
    let media = |w: &&str| ["видео", "фильм", "сери", "ролик", "воспроизведени", "просмотр", "кино", "эпизод", "его", "ее"].iter().any(|s| w.starts_with(s));
    if all.contains("на паузу") || all.contains("паузу") || (matches!(first, "останови" | "остановить" | "стоп" | "пауза") && words[1..].iter().all(media)) {
        return key("KEYCODE_MEDIA_PAUSE", 1);
    }
    if all.contains("с паузы") || (matches!(first, "продолжи" | "продолжай" | "продолжить" | "возобнови") && words[1..].iter().all(media)) {
        return key("KEYCODE_MEDIA_PLAY", 1);
    }
    if let Some(a) = screen_only(&words, true) {
        return Some(a);
    }
    if let Some(ms) = seek_command(&all) {
        // One press of the remote's wind key is about 10-15 s in the players.
        let presses = (ms.unsigned_abs() / 15_000).clamp(1, 10) as u8;
        return key(if ms >= 0 { "KEYCODE_MEDIA_FAST_FORWARD" } else { "KEYCODE_MEDIA_REWIND" }, presses);
    }
    if matches!(first, "закрой" | "закрыть" | "выйди") && words.len() > 1 {
        let app = words[1..].iter().filter(|w| **w != "из").copied().collect::<Vec<_>>().join(" ");
        return (!app.is_empty()).then_some(TvAction::Close(app));
    }
    or_unclear.then_some(TvAction::Unclear)
}

/// Commands for the Android TV. `None` when the phrase does not name it.
fn tv(text: &str) -> Option<TvAction> {
    let (rest, as_object) = strip_tv(text)?;
    let rest = rest.replace("пожалуйста", "");
    let words: Vec<&str> = rest.split_whitespace().collect();
    let all = words.join(" ");
    let first = words.first().copied().unwrap_or("");
    let after = |n: usize| words.iter().skip(n).copied().collect::<Vec<_>>().join(" ");
    // «включи проектор», «выключи телевизор»
    if as_object && words.len() == 1 {
        match first {
            "включи" | "включить" | "разбуди" => return Some(TvAction::Wake),
            "выключи" | "выключить" | "усыпи" | "отключи" => return Some(TvAction::Sleep),
            _ => {}
        }
    }
    let key = |code: &'static str, n: u8| Some(TvAction::Key(code, n));
    // Inside apps, like the remote: «наведись выше», «ещё ниже», «нажми ок».
    if let Some((code, n)) = nav_key(&words) {
        return key(code, n);
    }
    if OK_WORDS.contains(&all.as_str()) {
        return key(CENTER, 1);
    }
    // «включи последнее видео (из истории)», «воспроизведи первое видео»
    if matches!(first, "включи" | "воспроизведи" | "запусти" | "поставь" | "открой")
        && words.iter().any(|w| w.starts_with("последн") || w.starts_with("перв") || w.starts_with("верхн"))
        && words.iter().any(|w| w.starts_with("видео") || w.starts_with("ролик") || w.starts_with("серию"))
    {
        return Some(TvAction::PlayFirst);
    }
    // «открой впн и включи его»
    if matches!(first, "открой" | "запусти") {
        for sep in [" и включи", " и подключи", " и запусти"] {
            if let Some((app, _)) = after(1).split_once(sep) {
                if !app.trim().is_empty() {
                    return Some(TvAction::OpenAndStart(app.trim().to_owned()));
                }
            }
        }
    }
    // «включи впн»: a VPN is switched on by its button.
    if matches!(first, "включи" | "подключи") && matches!(after(1).as_str(), "впн" | "vpn") {
        return Some(TvAction::OpenAndStart("впн".into()));
    }
    // «нажми историю», «выбери настройки», «нажми на кнопку подписаться»
    if matches!(first, "нажми" | "выбери" | "ткни" | "кликни" | "нажать") {
        let mut q = after(1);
        for p in ["на кнопку ", "кнопку ", "на пункт ", "пункт ", "на "] {
            if let Some(r) = q.strip_prefix(p) {
                q = r.to_owned();
                break;
            }
        }
        if !q.is_empty() {
            return Some(TvAction::Tap(q));
        }
    }
    // «открой историю просмотров в смарт тьюб», «открой раздел подписки»
    if matches!(first, "открой" | "зайди" | "перейди") {
        let rest = after(1);
        let rest = rest.strip_prefix("в ").unwrap_or(&rest).to_owned();
        for sep in [" в приложении ", " в приложение ", " в "] {
            if let Some((section, app)) = rest.rsplit_once(sep) {
                if crate::tv::app_named(app) && !section.trim().is_empty() {
                    let section = section.trim_start_matches("раздел ").trim_start_matches("вкладку ").trim();
                    return Some(TvAction::OpenIn(app.trim().to_owned(), section.to_owned()));
                }
            }
        }
        for p in ["раздел ", "вкладку ", "пункт ", "меню "] {
            if let Some(r) = rest.strip_prefix(p) {
                return (!r.trim().is_empty()).then(|| TvAction::Tap(r.trim().to_owned()));
            }
        }
    }
    // «Сделай проектор тише на 10», «громче на пять», «убавь звук на 3»: that many presses.
    if let Some((code, n)) = volume_steps(&words) {
        return key(code, n);
    }
    match all.as_str() {
        "пауза" | "поставь на паузу" | "останови" | "стоп" | "на паузу" | "поставь паузу" => return key("KEYCODE_MEDIA_PAUSE", 1),
        "продолжи" | "играй" | "воспроизведи" | "сними с паузы" | "продолжай" => return key("KEYCODE_MEDIA_PLAY", 1),
        "громче" | "сделай громче" | "прибавь звук" | "прибавь громкость" => return key("KEYCODE_VOLUME_UP", 3),
        "тише" | "сделай тише" | "убавь звук" | "убавь громкость" => return key("KEYCODE_VOLUME_DOWN", 3),
        "выключи звук" | "без звука" | "включи звук" => return key("KEYCODE_VOLUME_MUTE", 1),
        "домой" | "на главную" | "главный экран" | "открой главный экран" => return key("KEYCODE_HOME", 1),
        "назад" | "вернись назад" => return key("KEYCODE_BACK", 1),
        "дальше" | "следующая серия" | "следующую серию" | "включи следующую серию" | "следующее" => {
            return key("KEYCODE_MEDIA_NEXT", 1)
        }
        "перемотай вперед" | "вперед" => return key("KEYCODE_MEDIA_FAST_FORWARD", 1),
        "перемотай назад" | "отмотай назад" => return key("KEYCODE_MEDIA_REWIND", 1),
        _ => {}
    }
    // «найди маму в 16 на рутубе», «включи дюну в кинопоиске»: inside that app.
    if let Some((app, q)) = crate::tv::search_in_phrase(&all) {
        return Some(TvAction::SearchIn(app, q));
    }
    let youtube = |w: &str| w.starts_with("ютуб") || w.starts_with("youtube") || w.starts_with("ютьюб");
    // «найди на ютубе котиков», «включи на ютубе …»
    if matches!(first, "найди" | "поищи" | "включи" | "поставь" | "открой" | "запусти") {
        let mut i = 1;
        if matches!(words.get(i), Some(&"на") | Some(&"в")) && words.get(i + 1).is_some_and(|w| youtube(w)) {
            i += 2;
            let q = after(i);
            return Some(if q.is_empty() { TvAction::Open("ютуб".into()) } else { TvAction::YouTube(q) });
        }
    }
    match first {
        "найди" | "поищи" | "покажи" | "включи" | "поставь" => {
            let mut q = after(1);
            for noun in ["фильм ", "сериал ", "мультфильм ", "мультик ", "кино ", "фильмы ", "сериалы "] {
                if let Some(r) = q.strip_prefix(noun) {
                    q = r.to_owned();
                    return (!q.is_empty()).then_some(TvAction::Search(q));
                }
            }
            if q.is_empty() {
                return None;
            }
            // «включи ютуб на проекторе»: an app; «найди дюну»: a search.
            if matches!(first, "включи" | "поставь") {
                return Some(TvAction::Open(q));
            }
            Some(TvAction::Search(q))
        }
        "открой" | "запусти" | "открыть" | "запустить" => {
            // «запусти приложение лифт»: the app is «лифт»; stray «на» left from the places go.
            let q = after(1);
            let q = q.strip_prefix("приложение ").unwrap_or(&q).trim();
            let q: Vec<&str> = q.split_whitespace().filter(|w| *w != "на" && *w != "в").collect();
            let q = q.join(" ");
            (!q.is_empty()).then_some(TvAction::Open(q))
        }
        _ => None,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PcAction {
    Wake,
    Sleep,
    Shutdown,
    Restart,
    Lock,
    Open(String),
    Close(String),
    /// Any other phrase for the PC, as said («открой ютуб в профиле работа»,
    /// «создай документ и напиши …»): the PC program works out what to do.
    Command(String),
    /// Connection check from the settings page.
    Ping,
    /// «Какая температура компьютера», «загрузка процессора».
    Metrics,
    /// «Запущен ли стим на компьютере».
    Process(String),
    /// A mouse click at screen pixels, from a scenario: "x,y" / "x,y,double" / "x,y,right" (pc::click_spec).
    Click(String),
    /// A key or a shortcut, from a scenario: "enter", "cmd+t", "ctrl+shift+esc" (pc::key_spec).
    Key(String),
    /// Text typed on the keyboard, from a scenario («pc type привет»; pc::type_spec).
    Type(String),
    /// Where the mouse pointer is now ("x,y"): for setting up a scenario's click.
    Mouse,
}

/// Which PC commands soundya takes («Компьютер» menu); the rest stay Alice's.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PcRules {
    pub power: bool,
    pub apps: bool,
    pub tg: bool,
    pub search: bool,
    /// Commands without «на компьютере».
    pub plain: bool,
    /// Normalized words and phrases always left to Alice.
    pub keep: Vec<String>,
}

impl Default for PcRules {
    fn default() -> Self {
        PcRules { power: true, apps: true, tg: true, search: true, plain: true, keep: Vec::new() }
    }
}

static PC_RULES: std::sync::Mutex<Option<PcRules>> = std::sync::Mutex::new(None);

pub fn set_pc_rules(rules: PcRules) {
    *PC_RULES.lock().unwrap() = Some(rules);
}

fn pc_rules() -> PcRules {
    PC_RULES.lock().unwrap().clone().unwrap_or_default()
}

/// «окно, Выключи компьютер; шторы» → ["окно", "выключи компьютер", "шторы"].
pub fn keep_list(text: &str) -> Vec<String> {
    text.split([',', ';', '\n']).map(normalize).filter(|w| !w.is_empty()).collect()
}

/// The phrase has one of the user's «всегда оставлять Алисе» words or phrases.
fn kept_for_alice(norm: &str, rules: &PcRules) -> bool {
    let padded = format!(" {norm} ");
    rules.keep.iter().any(|k| {
        let k = format!(" {k} ");
        padded.contains(&k) || (!k.trim().contains(' ') && padded.split_whitespace().any(|w| w.starts_with(k.trim())))
    })
}

/// A PC command is soundya's under the user's rules: `named` — the PC was said.
fn pc_allowed(norm: &str, action: &PcAction, named: bool, rules: &PcRules) -> bool {
    if kept_for_alice(norm, rules) || (!named && !rules.plain) {
        return false;
    }
    match action {
        PcAction::Wake | PcAction::Sleep | PcAction::Shutdown | PcAction::Restart | PcAction::Lock => rules.power,
        PcAction::Command(text) => {
            let words: Vec<&str> = text.split_whitespace().collect();
            let verb = words.first().copied().unwrap_or("");
            let has = |stems: &[&str]| words.iter().any(|w| stems.iter().any(|s| w.starts_with(s)));
            if matches!(verb, "отправь" | "скинь" | "пошли" | "напиши") && has(&["телег", "телеграм", "сообщени"]) {
                rules.tg
            } else if matches!(verb, "загугли" | "погугли" | "найди" | "поищи" | "запиши" | "допиши" | "напиши" | "создай" | "сделай" | "добавь") {
                rules.search
            } else {
                rules.apps
            }
        }
        _ => true,
    }
}

/// «напиши лизе я скоро буду» under the rules: Telegram may be left to Alice.
pub fn contact_allowed(text: &str) -> bool {
    let rules = pc_rules();
    rules.tg && !kept_for_alice(&normalize(text), &rules)
}

/// Questions about the PC's state: only with the PC named.
fn pc_question(text: &str) -> Option<PcAction> {
    let pc = ["компьютер", "компа", "компе", "комп ", "пк", "процессор", "видеокарт"].iter().any(|w| format!("{text} ").contains(w));
    if !pc {
        return None;
    }
    for p in ["запущен ли ", "работает ли ", "открыт ли ", "запущена ли ", "открыта ли "] {
        if let Some(rest) = text.strip_prefix(p) {
            let name: String = rest
                .split_whitespace()
                .take_while(|w| !matches!(*w, "на" | "в"))
                .collect::<Vec<_>>()
                .join(" ");
            return (!name.is_empty()).then_some(PcAction::Process(name));
        }
    }
    ["температур", "загрузк", "нагрузк", "загружен", "памят", "как себя чувствует", "состояние"]
        .iter()
        .any(|w| text.contains(w))
        .then_some(PcAction::Metrics)
}

/// Verbs of a command for the PC; the PC itself works out what exactly to do.
const PC_VERBS: [&str; 25] = [
    "открой", "закрой", "запусти", "включи", "выключи", "выруби", "найди", "поищи", "загугли",
    "погугли", "запиши", "напиши", "создай", "сделай", "открыть", "запустить", "закрыть",
    "допиши", "добавь", "отправь", "скинь", "пошли", "подключи", "отключи", "подключись",
];

/// Words that make «запиши…», «создай…» a job for the PC.
const PC_THINGS: [&str; 9] = ["заметк", "документ", "ворд", "блокнот", "файл", "папк", "текст", "браузер", "гугл"];

/// Things of a smart home: «закрой окно», «открой шторы», «выключи свет» are
/// Alice's (Home Assistant and the like), not a window or a program on the PC —
/// unless the PC is named («закрой окно на компьютере»).
const HOME_THINGS: [&str; 22] = [
    "окн", "форточк", "штор", "жалюз", "рольставн", "ворот", "калитк", "двер", "замок", "замк", "кран", "вентил",
    "розетк", "ламп", "люстр", "гирлянд", "кондиционер", "обогревател", "увлажнител", "пылесос", "чайник", "бойлер",
];

fn home_thing(words: &[&str]) -> bool {
    words.iter().any(|w| HOME_THINGS.iter().any(|s| w.starts_with(s)) || matches!(*w, "свет" | "света" | "подсветку" | "подсветка"))
}

/// «включи компьютер», «открой телеграм на компьютере», «усыпи комп»,
/// «открой ютуб в профиле работа», «запиши в заметки купить молоко», …
fn pc(text: &str) -> Option<PcAction> {
    pc_named(text).map(|(a, _)| a)
}

/// The PC command and whether the PC was named in it («на компьютере», «комп»).
fn pc_named(text: &str) -> Option<(PcAction, bool)> {
    const PLACES: [&str; 8] = [
        " на компьютере ",
        " на компе ",
        " на пк ",
        " в компьютере ",
        " на компьютер ",
        " на комп ",
        " на моем компьютере ",
        " на моем компе ",
    ];
    const NOUNS: [&str; 7] = ["компьютер", "компьютера", "компьютеру", "комп", "компа", "компу", "пк"];
    let mut rest = format!(" {text} ");
    let mut found = false;
    for place in PLACES {
        if rest.contains(place) {
            rest = rest.replacen(place, " ", 1);
            found = true;
            break;
        }
    }
    let mut words: Vec<&str> = rest.split_whitespace().collect();
    if !found {
        let before = words.len();
        words.retain(|w| !NOUNS.contains(w));
        found = words.len() < before;
    }
    words.retain(|w| !matches!(*w, "мой" | "моего" | "пожалуйста"));
    let (&verb, object) = words.split_first()?;
    let all = words.join(" ");
    if !found {
        // Without «на компьютере»: things Alice cannot do on this speaker anyway.
        let has = |stems: &[&str]| words.iter().skip(1).any(|w| stems.iter().any(|s| w.starts_with(s)));
        // «закрой окно», «открой шторы»: the smart home's, unless it is the browser's window.
        if home_thing(&words[1..]) && !has(&PC_THINGS) {
            return None;
        }
        let plain = match verb {
            // «открой стим», «закрой хром», «открой ютуб в профиле работа»; her skills stay hers.
            "открой" | "закрой" => !object.is_empty() && !has(&["навык"]),
            "загугли" | "погугли" => !object.is_empty(),
            // «найди в гугле …», «поищи в браузере …»
            "найди" | "поищи" => has(&["гугл", "браузер", "интернет", "ютуб", "youtube", "ютюб", "ютьюб"]),
            // «запиши в заметки …», «создай документ …», «создай папку …»
            "запиши" | "напиши" | "создай" | "сделай" => {
                has(&PC_THINGS) || has(&["телег", "телеграм", "сообщени"]) || verb == "запиши"
            }
            // «допиши в блокнот …»
            "допиши" | "добавь" => verb == "допиши" || has(&["блокнот", "заметк"]),
            // «отправь маме в телеграм …»
            "отправь" | "скинь" | "пошли" => has(&["телег", "телеграм", "сообщени"]),
            // «подключи впн», «отключи впн»
            _ if has(&["впн", "vpn"]) => matches!(verb, "подключи" | "подключись" | "включи" | "отключи" | "выключи" | "запусти"),
            _ => false,
        };
        return plain.then(|| (PcAction::Command(all), false));
    }
    pc_said(verb, object, all).map(|a| (a, true))
}

fn pc_said(verb: &str, object: &[&str], all: String) -> Option<PcAction> {
    if all.contains("спящ") || all.contains(" сон") || verb == "усыпи" {
        return Some(PcAction::Sleep);
    }
    if object.is_empty() {
        return match verb {
            "включи" | "включить" | "запусти" | "разбуди" => Some(PcAction::Wake),
            "выключи" | "выключить" | "отключи" | "выруби" => Some(PcAction::Shutdown),
            "перезагрузи" | "перезапусти" | "перезагрузить" => Some(PcAction::Restart),
            "заблокируй" | "заблокировать" | "блокируй" => Some(PcAction::Lock),
            _ => None,
        };
    }
    PC_VERBS.contains(&verb).then(|| PcAction::Command(all))
}

fn normalize(text: &str) -> String {
    let text = text
        .trim()
        .trim_matches(['.', '!', '?'])
        .to_lowercase()
        .replace('ё', "е");
    text.strip_prefix("алиса, ")
        .or_else(|| text.strip_prefix("алиса "))
        .unwrap_or(&text)
        .trim()
        .to_owned()
}

/// How a macro phrase is compared with what was heard.
pub fn normalize_phrase(text: &str) -> String {
    normalize(text)
        .split(|c: char| !c.is_alphanumeric() && c != '-')
        .filter(|w| !w.is_empty() && *w != "пожалуйста")
        .collect::<Vec<_>>()
        .join(" ")
}

// ---------------------------------------------------------------- a question and its answer

/// What an answer does: a voice intent or a panel command, ready to run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    Voice(Intent),
    Control(String),
}

/// One thing to choose: what is read aloud, the words that pick it and what it runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Choice {
    pub label: String,
    pub words: Vec<String>,
    pub step: Step,
}

/// soundya asked something aloud («На каком устройстве?», «Какое подключить?») and waits for
/// the answer. Alice does not listen by herself (no such call is known), so the answer is a
/// new phrase with her name: «Алиса, на проекторе», «Алиса, первое», «Алиса, отмена».
#[derive(Debug, Clone)]
struct Question {
    id: u64,
    options: Vec<Choice>,
    until: std::time::Instant,
}

/// How long the answer is waited for after the question was said.
const ANSWER_FOR: Duration = Duration::from_secs(40);

#[cfg(not(test))]
static QUESTION: std::sync::Mutex<Option<Question>> = std::sync::Mutex::new(None);

#[cfg(not(test))]
fn question<R>(f: impl FnOnce(&mut Option<Question>) -> R) -> R {
    f(&mut QUESTION.lock().unwrap())
}

// As TV_STATE: each test has its own.
#[cfg(test)]
thread_local! {
    static QUESTION: std::cell::RefCell<Option<Question>> = const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
fn question<R>(f: impl FnOnce(&mut Option<Question>) -> R) -> R {
    QUESTION.with(|s| f(&mut s.borrow_mut()))
}

static QUESTION_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

/// Keeps the question for `wait`; the id tells a later `ask` from this one.
fn put_question(options: Vec<Choice>, wait: Duration) -> u64 {
    let id = QUESTION_ID.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
    question(|q| *q = Some(Question { id, options, until: std::time::Instant::now() + wait }));
    id
}

/// Says `text` in Alice's voice and waits for one of `options`. The window is counted from
/// the end of the speech (an answer during it counts too).
pub fn ask(text: String, options: Vec<Choice>) {
    // About 13 letters a second (say_wait's own estimate), plus the pause before speaking.
    let speaking = Duration::from_millis(1600 + text.chars().count() as u64 * 1000 / 13);
    let id = put_question(options, speaking + ANSWER_FOR);
    thread::spawn(move || {
        thread::sleep(Duration::from_millis(600));
        say_wait(&text);
        question(|q| {
            if let Some(q) = q.as_mut().filter(|q| q.id == id) {
                q.until = std::time::Instant::now() + ANSWER_FOR;
            }
        });
    });
}

/// The options of the question that is still waiting.
fn pending_options() -> Option<Vec<Choice>> {
    question(|q| {
        if q.as_ref().is_some_and(|q| q.until < std::time::Instant::now()) {
            *q = None;
        }
        q.as_ref().map(|q| q.options.clone())
    })
}

pub fn clear_question() {
    question(|q| *q = None);
}

/// For the panel: what is being asked (labels) and how many seconds are left.
pub fn question_json() -> serde_json::Value {
    let left = question(|q| q.as_ref().map(|q| q.until.saturating_duration_since(std::time::Instant::now()).as_secs()));
    match (pending_options(), left) {
        (Some(o), Some(left)) => serde_json::json!({ "options": o.iter().map(|c| c.label.clone()).collect::<Vec<_>>(), "left": left }),
        _ => serde_json::Value::Null,
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum Answer {
    Pick(usize),
    Cancel,
    /// Not an answer at all: the phrase goes on as any other.
    No,
}

/// «Первое», «второй», «3», «последнее» → the option's index.
fn ordinal(word: &str, len: usize) -> Option<usize> {
    const ORD: [&[&str]; 6] = [
        &["1", "один", "одно", "первое", "первый", "первая", "первую", "первого", "первом", "первой", "первому"],
        &["2", "два", "две", "второе", "второй", "вторая", "вторую", "второго", "втором", "второму"],
        &["3", "три", "третье", "третий", "третья", "третью", "третьего", "третьем", "третьему", "третьей"],
        &["4", "четыре", "четвертое", "четвертый", "четвертая", "четвертую", "четвертого", "четвертом", "четвертому", "четвертой"],
        &["5", "пять", "пятое", "пятый", "пятая", "пятую", "пятого", "пятом", "пятому", "пятой"],
        &["6", "шесть", "шестое", "шестой", "шестая", "шестую", "шестого", "шестом", "шестому"],
    ];
    if matches!(word, "последнее" | "последний" | "последняя" | "последнюю") {
        return len.checked_sub(1);
    }
    ORD.iter().position(|w| w.contains(&word)).filter(|i| *i < len)
}

/// Is the phrase (normalize_phrase) an answer to these options? Only a phrase that is wholly
/// an answer: «на проекторе», «проектор», «первое», «на мак», «отмена» — never «включи первое
/// видео» or «включи музыку».
pub fn answer(options: &[Choice], phrase: &str) -> Answer {
    const FILL: [&str; 20] = [
        "на", "в", "во", "к", "ко", "пожалуйста", "давай", "выбери", "выбираю", "подключи", "подключись", "вариант", "номер",
        "устройство", "устройству", "устройстве", "это", "пусть", "будет", "выполни",
    ];
    let phrase = phrase.replace('ё', "е");
    let words: Vec<&str> = phrase.split_whitespace().filter(|w| !FILL.contains(w)).collect();
    if words.is_empty() || words.len() > 4 {
        return Answer::No;
    }
    let all = words.join(" ");
    if matches!(
        all.as_str(),
        "отмена" | "отмени" | "отменить" | "никакое" | "никакой" | "никакую" | "никуда" | "ничего" | "не надо" | "не нужно" | "нет"
            | "ни каком" | "никаком" | "передумал" | "передумала" | "не важно" | "неважно"
    ) {
        return Answer::Cancel;
    }
    if let [one] = words[..] {
        if let Some(i) = ordinal(one, options.len()) {
            return Answer::Pick(i);
        }
    }
    // Every word said is a word of exactly one option («проектор», «мак», «джбл флип»).
    // A short name in another case too («на маке» for «мак», «в зале» for «зал»): same_word
    // needs four letters for its stem.
    let short = |said: &str, name: &str| {
        let len = name.chars().count();
        (2..=3).contains(&len) && said.starts_with(name) && said.chars().count() <= len + 2
    };
    let fits = |c: &Choice| words.iter().all(|w| c.words.iter().any(|n| same_word(w, n) || short(w, n)));
    let mut picks = options.iter().enumerate().filter(|(_, c)| fits(c)).map(|(i, _)| i);
    match (picks.next(), picks.next()) {
        (Some(i), None) => Answer::Pick(i),
        _ => Answer::No,
    }
}

/// The heard phrase as an answer to the pending question: Some(Some(step)) — picked,
/// Some(None) — «отмена», None — not an answer (the question stays).
pub fn take_answer(text: &str) -> Option<Option<Step>> {
    let options = pending_options()?;
    match answer(&options, &normalize_phrase(text)) {
        Answer::Pick(i) => {
            clear_question();
            Some(Some(options[i].step.clone()))
        }
        Answer::Cancel => {
            clear_question();
            Some(None)
        }
        Answer::No => None,
    }
}

/// Names Alice writes in Latin letters: the scenario's phrase was typed in Russian.
const LATIN_NAMES: [(&str, &str); 16] = [
    ("youtube", "ютуб"), ("deadlock", "дедлок"), ("discord", "дискорд"), ("telegram", "телеграм"),
    ("steam", "стим"), ("twitch", "твич"), ("netflix", "нетфликс"), ("spotify", "спотифай"),
    ("chrome", "хром"), ("google", "гугл"), ("whatsapp", "ватсап"), ("tiktok", "тикток"),
    ("smarttube", "смарттьюб"), ("kinopoisk", "кинопоиск"), ("dota", "дота"), ("cs", "кс"),
];

fn canon_words(phrase: &str) -> String {
    phrase
        .split(' ')
        .map(|w| LATIN_NAMES.iter().find(|(l, _)| *l == w).map_or(w, |(_, r)| *r))
        .collect::<Vec<_>>()
        .join(" ")
        .replace('ё', "е")
}

/// Does a heard phrase (normalize_phrase) run the scenario `saved` (its normalized phrase)?
/// The whole phrase, as before, but «YouTube» is «ютуб», and «выполни …» / «запусти сценарий …»
/// in front only says «run it»: «Алиса, выполни буду смотреть ютуб».
pub fn macro_matches(heard: &str, saved: &str) -> bool {
    let (h, s) = (canon_words(heard), canon_words(saved));
    if h == s {
        return true;
    }
    const RUN: [&str; 7] = ["выполни сценарий ", "запусти сценарий ", "включи сценарий ", "сценарий ", "выполни ", "запусти ", "активируй "];
    RUN.iter().any(|p| h.strip_prefix(p).is_some_and(|rest| rest == s))
}

/// Removes "в саундклауде", "на soundcloud" and similar; returns whether it was present.
pub fn strip_service(text: &str) -> (String, bool) {
    const NAMES: [&str; 8] = [
        "саундклауде",
        "саундклауда",
        "саундклауд",
        "саунд клауде",
        "саунд клауда",
        "саунд клауд",
        "soundcloud",
        "sound cloud",
    ];
    let mut out = format!(" {text} ");
    let mut found = false;
    for name in NAMES {
        for prep in [" в ", " на ", " из ", " с ", " "] {
            let needle = format!("{prep}{name} ");
            if out.contains(&needle) {
                out = out.replace(&needle, " ");
                found = true;
            }
        }
    }
    (out.split_whitespace().collect::<Vec<_>>().join(" "), found)
}

/// Playback control phrased in any of the usual ways:
/// «продолжи воспроизведение», «поставь музыку на паузу», «включи следующую песню», …
fn transport(text: &str) -> Option<Intent> {
    let words: Vec<&str> = text.split_whitespace().collect();
    let first = *words.first()?;
    let has = |stems: &[&str]| words.iter().any(|w| stems.iter().any(|s| w.starts_with(s)));
    // Questions and requests for something new are not transport commands.
    if has(&["почему", "что", "какой", "какая", "сколько", "когда", "где", "кто"]) || words.len() > 6 {
        return None;
    }
    let music = has(&["музык", "воспроизвед", "трек", "песн", "плейлист", "композици", "проигрыв"]);
    if first.starts_with("продолж") || first == "возобнови" || (first == "сними" && has(&["пауз"])) {
        return Some(Intent::Resume);
    }
    if first == "пауза" || has(&["пауз"]) && !has(&["сними"]) {
        return Some(Intent::Pause);
    }
    if matches!(first, "стоп" | "хватит" | "останови" | "остановись" | "приостанови" | "замолчи")
        || (matches!(first, "выключи" | "останови" | "прекрати") && music)
    {
        return Some(Intent::Pause);
    }
    if has(&["следующ", "дальше", "пропусти", "переключи", "другую", "другой"])
        && (music || words.len() <= 2 || has(&["следующ", "дальше", "пропусти"]))
    {
        return Some(Intent::Next);
    }
    if has(&["предыдущ", "назад", "прошл"]) && (music || words.len() <= 3) {
        return Some(Intent::Previous);
    }
    None
}

/// Shuffle words: Some((text without them, on)) when present.
fn shuffle_words(text: &str) -> Option<(String, bool)> {
    const OFF: [&str; 10] = [
        "не перемешивай",
        "по порядку",
        "выключи перемешивание",
        "отключи перемешивание",
        "без перемешивания",
        "выключи шафл",
        "отключи шафл",
        "выключи случайный порядок",
        "отключи случайный порядок",
        "выключи рандом",
    ];
    if OFF.iter().any(|p| text.contains(p)) {
        return Some((String::new(), false));
    }
    const ON: [&str; 15] = [
        "в случайном порядке",
        "случайный порядок",
        "вперемешку",
        "перемешанно",
        "перемешай",
        "перемешать",
        "перемешивай",
        "перемешивание",
        "решаффл",
        "решафл",
        "шаффл",
        "шафл",
        "рандомно",
        "в рандоме",
        "рандом",
    ];
    let mut out = format!(" {text} ");
    let mut found = false;
    for w in ON {
        let needle = format!(" {w} ");
        while out.contains(&needle) {
            out = out.replacen(&needle, " ", 1);
            found = true;
        }
    }
    found.then(|| (out.split_whitespace().collect::<Vec<_>>().join(" "), true))
}

/// Commands about the current track: what is it, like it, repeat it, more like it.
fn extras(text: &str) -> Option<Intent> {
    let words: Vec<&str> = text.split_whitespace().collect();
    let first = *words.first()?;
    let has = |stems: &[&str]| words.iter().any(|w| stems.iter().any(|s| w.starts_with(s)));
    const WHAT: [&str; 13] = [
        "что играет",
        "что сейчас играет",
        "что это играет",
        "что за песня",
        "что за трек",
        "что это за песня",
        "что это за трек",
        "как называется песня",
        "как называется трек",
        "как называется эта песня",
        "как называется этот трек",
        "кто поет",
        "кто это поет",
    ];
    if WHAT.iter().any(|p| text == *p || text.starts_with(&format!("{p} "))) {
        return Some(Intent::WhatPlaying);
    }
    if matches!(first, "убери" | "сними" | "удали") && has(&["лайк"]) || text.starts_with("удали из лайков") || text.starts_with("убери из лайков") {
        return Some(Intent::Unlike);
    }
    for p in [
        "добавь в плейлист ",
        "добавь этот трек в плейлист ",
        "добавь эту песню в плейлист ",
        "добавь трек в плейлист ",
        "добавь песню в плейлист ",
        "сохрани в плейлист ",
        "сохрани трек в плейлист ",
        "сохрани песню в плейлист ",
        "закинь в плейлист ",
    ] {
        if let Some(name) = text.strip_prefix(p) {
            let name = name.trim().trim_matches('"').trim();
            if !name.is_empty() {
                return Some(Intent::AddToPlaylist(name.to_owned()));
            }
        }
    }
    if text.starts_with("мне не нравится") || first == "дизлайк" || text.starts_with("поставь дизлайк") {
        return Some(Intent::Next);
    }
    if first == "лайкни"
        || text == "лайк"
        || text.starts_with("поставь лайк")
        || text.starts_with("мне нравится")
        || (first == "добавь" && has(&["лайк", "избранн", "любим"]))
    {
        return Some(Intent::Like);
    }
    if (matches!(first, "выключи" | "отключи" | "убери") && has(&["повтор"]))
        || text.contains("без повтора")
        || text.starts_with("хватит повторять")
        || text.starts_with("не повторяй")
    {
        return Some(Intent::Repeat(false));
    }
    if (has(&["повтор"]) && (matches!(first, "поставь" | "включи") || text.contains("на повтор")))
        || matches!(first, "повторяй" | "зацикли")
    {
        return Some(Intent::Repeat(true));
    }
    if matches!(text, "повтори трек" | "повтори песню" | "заново" | "сначала" | "с начала" | "включи заново" | "включи сначала" | "включи трек заново" | "включи песню заново") {
        return Some(Intent::Restart);
    }
    if has(&["похож"]) && words.len() <= 5 && !has(&["почему", "на что"]) {
        return Some(Intent::Similar);
    }
    None
}

/// `keyword_only`: intercept music requests only when SoundCloud is named.
/// «перемотай на 30 секунд», «перемотай вперед на минуту», «отмотай назад на полминуты»,
/// «промотай на 2 минуты 30 секунд». Returns the jump in ms (negative: back).
fn seek_command(text: &str) -> Option<i64> {
    let words: Vec<&str> = text.split_whitespace().collect();
    let verb = *words.first()?;
    let back_verb = verb.starts_with("отмота") || verb.starts_with("отмотк");
    if !(back_verb
        || verb.starts_with("перемота")
        || verb.starts_with("перемотк")
        || verb.starts_with("промота")
        || verb.starts_with("прокрути")
        || verb.starts_with("мотни"))
    {
        return None;
    }
    let back = back_verb || words.iter().any(|w| *w == "назад");
    fn number(word: &str) -> Option<f64> {
        if let Ok(n) = word.replace(',', ".").parse::<f64>() {
            return Some(n);
        }
        Some(match word {
            "одну" | "одна" | "один" => 1.0,
            "полторы" | "полтора" => 1.5,
            "две" | "два" => 2.0,
            "три" => 3.0,
            "четыре" => 4.0,
            "пять" => 5.0,
            "десять" => 10.0,
            "пятнадцать" => 15.0,
            "двадцать" => 20.0,
            "тридцать" => 30.0,
            "сорок" => 40.0,
            "пятьдесят" => 50.0,
            _ => return None,
        })
    }
    let at = |i: Option<usize>| i.map(|j| words[j]).and_then(number);
    let mut ms = 0f64;
    let mut found = false;
    for (i, w) in words.iter().enumerate().skip(1) {
        let unit = if w.starts_with("полминут") {
            ms += 30_000.0;
            found = true;
            continue;
        } else if w.starts_with("секунд") || *w == "сек" {
            1_000.0
        } else if w.starts_with("минут") || *w == "мин" {
            60_000.0
        } else {
            continue;
        };
        // «минуту» alone is one minute; «2 минуты», «двадцать секунд» take the number before.
        let mut n = at(i.checked_sub(1)).unwrap_or(1.0);
        // «двадцать пять секунд»
        if let (Some(a), Some(b)) = (at(i.checked_sub(2)), at(i.checked_sub(1))) {
            if a >= 20.0 && b < 10.0 {
                n = a + b;
            }
        }
        ms += n * unit;
        found = true;
    }
    if !found {
        // «перемотай на 30»: seconds; «перемотай вперед»: 30 seconds.
        ms = words.iter().skip(1).find_map(|w| number(w)).unwrap_or(30.0) * 1_000.0;
    }
    let ms = ms.round() as i64;
    Some(if back { -ms } else { ms })
}

/// «напиши лизе я скоро буду», «отправь контакту кошка привет»: a message to one of
/// the Telegram contacts from the settings page, even without «в телеграм».
pub fn message_to_contact(text: &str, contacts: &[String]) -> Option<Intent> {
    if contacts.is_empty() {
        return None;
    }
    let norm = normalize(text);
    let words: Vec<&str> = norm.split_whitespace().collect();
    let (&verb, rest) = words.split_first()?;
    if !matches!(verb, "напиши" | "отправь" | "скинь" | "пошли" | "передай" | "написать") {
        return None;
    }
    // The first word that is not «сообщение», «в телеграм», «контакту» must be a contact.
    let skip = |w: &&str| {
        matches!(*w, "сообщение" | "сообщения" | "в" | "во" | "контакту" | "пользователю" | "мне")
            || w.starts_with("телег")
            || w.starts_with("телеграм")
    };
    let first = rest.iter().copied().find(|w| !skip(w))?;
    let first = first.trim_matches(|c: char| !c.is_alphanumeric());
    contacts
        .iter()
        .any(|c| c == first || (c.contains(' ') && rest.join(" ").contains(c.as_str())))
        .then(|| Intent::Pc(PcAction::Command(norm.clone())))
}

/// «… в яндекс музыке», «… в яндексе», «… из яндекса»: Alice's own music service.
fn names_yandex(text: &str) -> bool {
    let t = format!(" {text} ");
    [" яндекс музык", " яндекс-музык", " яндексмузык", " в яндексе ", " из яндекса ", " на яндексе ", " яндекса "]
        .iter()
        .any(|n| t.contains(n))
}

/// Things only Alice plays: Yandex Music's editorial playlists, podcasts,
/// audiobooks. `Some(ToAlice)` so a playing SoundCloud steps aside.
fn yandex_only(text: &str) -> Option<Intent> {
    let (rest, named) = strip_service(text);
    if named {
        return None;
    }
    let what = PLAY_VERBS.iter().find_map(|v| rest.strip_prefix(v))?.trim();
    const OWN: [&str; 9] = [
        "плейлист дня", "дежавю", "премьер", "тайник", "подкаст", "аудиокниг", "книгу", "радио", "плейлист с алисой",
    ];
    OWN.iter().any(|p| what.starts_with(p) || what.starts_with(&format!("мой {p}")) || what.starts_with(&format!("мою {p}")))
        .then_some(Intent::ToAlice)
}

/// «Включи музыку в наушниках», «переключи звук на наушники» / «верни музыку на колонку»:
/// soundya's music to the Bluetooth device chosen on the page, or back.
fn bt_out(n: &str) -> Option<bool> {
    let verb = ["включи ", "переключи ", "переведи ", "выведи ", "пусти ", "верни "].iter().any(|v| n.starts_with(v));
    let what = n.contains("музык") || n.contains("звук");
    if !verb || !what {
        return None;
    }
    if n.contains("наушник") || n.contains("блютуз колонк") || n.contains("bluetooth") {
        return Some(!n.starts_with("верни "));
    }
    (n.starts_with("верни ") && (n.ends_with("на колонку") || n.ends_with("на станцию") || n.ends_with("с наушников"))).then_some(false)
}

/// «блютуз» as people and the recogniser write it.
fn bt_word(n: &str) -> bool {
    ["блютуз", "блютус", "блютуф", "блутуз", "bluetooth"].iter().any(|w| n.contains(w))
}

/// Bluetooth by voice. «Переключись на блютус», «включи блютуз колонку», «музыку в наушники» →
/// BtOut(true): the saved device, connected again if need be. «Подключись к новому устройству
/// блютус», «найди блютуз устройства», «добавь блютуз колонку» → BtFind. «Отключи блютуз
/// колонку», «верни звук на колонку» → BtOut(false). Bare «включи блютуз» / «выключи блютуз»
/// stay Alice's (the Station as a Bluetooth speaker for a phone) — unless soundya's music is
/// in a Bluetooth device right now, then «выключи блютуз» brings it back.
fn bt_voice(n: &str) -> Option<Intent> {
    if let Some(on) = bt_out(n) {
        return Some(Intent::BtOut(on));
    }
    let words: Vec<&str> = n.split_whitespace().filter(|w| !matches!(*w, "пожалуйста" | "мне" | "мою" | "мои" | "мой")).collect();
    let (&verb, rest) = words.split_first()?;
    let bt = bt_word(n);
    let has = |stems: &[&str]| rest.iter().any(|w| stems.iter().any(|s| w.starts_with(s)));
    let device = has(&["колонк", "наушник", "устройств", "гарнитур", "динамик"]);
    let new = has(&["нов", "друг"]);
    // Only Bluetooth and device words after the verb: «найди блютуз наушники в интернете»,
    // «добавь в список блютуз колонку» are not a search for devices.
    let only_bt = rest.iter().all(|w| {
        bt_word(w) || ["колонк", "наушник", "устройств", "гарнитур", "динамик", "нов", "друг"].iter().any(|s| w.starts_with(s))
            || matches!(*w, "к" | "по" | "рядом" | "поблизости" | "вокруг" | "все" | "еще")
    });
    // «Включи блютуз на колонке»: the Station's own Bluetooth, Alice's.
    let on_station = n.contains(" на колонке") || n.contains(" на станции") || n.contains(" на алисе");
    let to_speaker = n.ends_with("на колонку") || n.ends_with("на станцию") || n.ends_with("на саму колонку") || n.ends_with("на алису");
    match verb {
        "найди" | "поищи" | "ищи" | "сопряги" | "добавь" if bt && only_bt => Some(Intent::BtFind),
        "подключи" | "подключись" | "подключить" | "подключиться" | "привяжи" if new && (bt || device) => Some(Intent::BtFind),
        "подключи" | "подключись" | "подключить" | "подключиться" if bt => Some(Intent::BtOut(true)),
        "переключись" | "переключи" | "переведи" | "выведи" | "пусти" | "перекинь" if to_speaker && (has(&["музык", "звук"]) || rest.len() <= 2) => {
            Some(Intent::BtOut(false))
        }
        "переключись" | "переключи" | "переведи" | "выведи" | "пусти" | "перекинь" if bt || has(&["наушник"]) => Some(Intent::BtOut(true)),
        // «Вернись на колонку»: Alice answers «я и так на колонке» herself.
        "вернись" | "верни" | "возвращайся" | "вернуться" if to_speaker && rest.len() <= 3 => Some(Intent::BtOut(false)),
        "включи" | "запусти" if bt && device && !on_station => Some(Intent::BtOut(true)),
        "отключи" | "выключи" | "отключись" | "отсоедини" if bt && !on_station && (device || crate::bt::output().is_some()) => Some(Intent::BtOut(false)),
        _ => None,
    }
}

/// Smart-home words: «включи свет» is never music.
const HOME: [&str; 31] = [
    "свет", "освещение", "ламп", "люстр", "торшер", "ночник", "розетк", "телевизор", "тв",
    "кондиционер", "вентилятор", "обогревател", "пылесос", "робот", "чайник", "будильник",
    "таймер", "блютуз", "блютус", "bluetooth", "режим", "экран", "микрофон", "громк", "звук",
    "отоплен", "увлажнител", "очистител", "зарядк", "радио", "сказк",
];

/// The verbs of a music request.
const PLAY_VERBS: [&str; 5] = ["включи ", "поставь ", "запусти ", "сыграй ", "играй "];

/// The alarm's phrases are not music (they stay soundya's with SoundCloud switched off).
fn is_alarm(intent: &Intent) -> bool {
    matches!(intent, Intent::Alarm(_))
}

/// The names of the TVs (true) and computers (false) the user gave them, as said
/// («спальня», «ноутбук»); set from the settings.
static DEVICES: std::sync::Mutex<Vec<(String, bool)>> = std::sync::Mutex::new(Vec::new());

pub fn set_devices(names: Vec<(String, bool)>) {
    *DEVICES.lock().unwrap() = names.into_iter().map(|(n, tv)| (normalize_phrase(&n), tv)).filter(|(n, _)| !n.is_empty()).collect();
}

/// A spoken word is this word of a name: the same, or the same stem in another case
/// («спальня» / «спальне», «ноутбук» / «ноутбуке», «кухня» / «кухне»).
fn same_word(said: &str, name: &str) -> bool {
    if said == name {
        return true;
    }
    let (s, n): (Vec<char>, Vec<char>) = (said.chars().collect(), name.chars().collect());
    let stem = n.len().saturating_sub(2).max(3);
    n.len() >= 4 && s.len() >= stem && s.len() <= n.len() + 2 && s[..stem] == n[..stem.min(n.len())]
}

/// The phrase with a device's name in it, made into one the TV / PC parsers know:
/// «выключи телевизор в спальне» → «выключи телевизор», «включи ютуб в спальне» →
/// «включи ютуб на телевизоре», «усыпи ноутбук» → «усыпи компьютер». (name, is TV, phrase)
fn device_in(n: &str, devices: &[(String, bool)]) -> Option<(String, bool, String)> {
    let words: Vec<&str> = n.split_whitespace().collect();
    // Longer names first: «большая спальня» before «спальня».
    let mut sorted: Vec<&(String, bool)> = devices.iter().collect();
    sorted.sort_by_key(|(name, _)| std::cmp::Reverse(name.split_whitespace().count()));
    for (name, tv) in sorted {
        let parts: Vec<&str> = name.split_whitespace().collect();
        if parts.is_empty() || parts.len() > words.len() {
            continue;
        }
        let Some(at) = (0..=words.len() - parts.len()).find(|&i| parts.iter().enumerate().all(|(j, p)| same_word(words[i + j], p))) else {
            continue;
        };
        let prep = at > 0 && matches!(words[at - 1], "в" | "во" | "на");
        let start = if prep { at - 1 } else { at };
        let before = words[..start].join(" ");
        let after = words[at + parts.len()..].join(" ");
        let rest = format!("{before} {after}").split_whitespace().collect::<Vec<_>>().join(" ");
        let kind_named = if *tv { strip_tv(&rest).is_some() || remote_phrase(&rest).is_some() } else { pc(&rest).is_some() && rest.split_whitespace().any(|w| w.starts_with("комп") || w == "пк") };
        // «Включи музыку на кухне» is about music in the kitchen, not the kitchen TV's app,
        // unless the TV is said too («включи музыку на телевизоре на кухне»).
        let music = rest.split_whitespace().any(|w| ["музык", "песн", "трек", "радио", "саундклауд", "плейлист", "альбом"].iter().any(|m| w.starts_with(m)));
        if *tv && !kind_named && music {
            continue;
        }
        // «Включи свет в спальне» with a TV called «спальня»: the lamp, not an app «свет».
        if *tv && !kind_named && home_thing(&rest.split_whitespace().collect::<Vec<_>>()) {
            continue;
        }
        let phrase = if kind_named {
            rest
        } else if prep {
            format!("{rest} {}", if *tv { "на телевизоре" } else { "на компьютере" })
        } else {
            format!("{before} {} {after}", if *tv { "телевизор" } else { "компьютер" }).split_whitespace().collect::<Vec<_>>().join(" ")
        };
        return Some((name.clone(), *tv, phrase));
    }
    None
}

/// The way people really say it → the plain command the rules below know. Only the start of
/// the phrase: «слушай, а ну-ка вруби проектор» → «включи проектор», «можешь открыть ютуб на
/// телевизоре» → «открой ютуб на телевизоре», «выруби телек» → «выключи телек». A phrase that
/// does not start with a command after the small talk is left as it was (answers, questions).
/// Without this every such wording went to Alice, and she answered in her own way.
pub fn plain_command(text: &str) -> String {
    const SMALL_TALK: [&str; 30] = [
        "пожалуйста", "давай", "давайте", "ну", "ну-ка", "нука", "а", "и", "слушай", "слышь", "можешь", "можете", "сможешь", "можно",
        "ты", "мне", "быстро", "быстренько", "сейчас", "сначала", "сперва", "теперь", "тогда", "будь", "добра", "любезна", "хочу", "надо", "нужно", "срочно",
    ];
    // Said → the rule's word. Slang, «-ай» forms, the polite plural and «можешь включить».
    const VERBS: [(&str, &str); 75] = [
        ("вруби", "включи"), ("врубай", "включи"), ("включай", "включи"), ("включите", "включи"), ("включить", "включи"), ("включим", "включи"),
        ("вруби-ка", "включи"), ("зажги", "включи"), ("активируй", "включи"),
        ("выруби", "выключи"), ("вырубай", "выключи"), ("выключай", "выключи"), ("выключите", "выключи"), ("выключить", "выключи"), ("выключим", "выключи"),
        ("отруби", "выключи"), ("погаси", "выключи"), ("гаси", "выключи"), ("туши", "выключи"), ("потуши", "выключи"),
        ("открывай", "открой"), ("откройте", "открой"), ("открыть", "открой"), ("откроем", "открой"), ("раскрой", "открой"),
        ("запускай", "запусти"), ("запустите", "запусти"), ("запустить", "запусти"), ("запустим", "запусти"), ("стартани", "запусти"), ("стартуй", "запусти"),
        ("закрывай", "закрой"), ("закройте", "закрой"), ("закрыть", "закрой"), ("закроем", "закрой"), ("прикрой", "закрой"),
        ("отыщи", "найди"), ("разыщи", "найди"), ("ищи", "найди"), ("найдите", "найди"), ("найти", "найди"), ("найдем", "найди"),
        ("поискать", "найди"),
        ("поставьте", "поставь"), ("поставить", "поставь"), ("поставим", "поставь"), ("ставь", "поставь"),
        ("сделайте", "сделай"), ("сделать", "сделай"), ("делай", "сделай"),
        ("нажмите", "нажми"), ("жми", "нажми"), ("тыкни", "нажми"), ("клацни", "нажми"),
        ("переключай", "переключи"), ("переключите", "переключи"), ("переключить", "переключи"),
        ("перемотать", "перемотай"), ("мотай", "перемотай"), ("промотай", "перемотай"),
        ("остановите", "останови"), ("остановить", "останови"), ("тормози", "останови"), ("стопни", "останови"),
        ("продолжай", "продолжи"), ("продолжить", "продолжи"), ("продолжим", "продолжи"),
        ("убавить", "убавь"), ("убавляй", "убавь"), ("прибавить", "прибавь"), ("прибавляй", "прибавь"), ("добавь-ка", "прибавь"),
        ("покажите", "покажи"), ("показать", "покажи"), ("показывай", "покажи"),
    ];
    // The rules' own first words: after small talk they are a command too («давай открой ютуб»).
    const KNOWN: [&str; 26] = [
        "включи", "выключи", "открой", "запусти", "закрой", "найди", "поищи", "поставь", "сделай", "нажми", "переключи", "перемотай",
        "останови", "продолжи", "убавь", "прибавь", "покажи", "подключи", "отключи", "усыпи", "разбуди", "верни", "вернись", "отмотай", "выбери", "зайди",
    ];
    let n = normalize(text);
    let words: Vec<&str> = n.split_whitespace().map(|w| w.trim_matches(',')).filter(|w| !w.is_empty()).collect();
    let skip = words.iter().take_while(|w| SMALL_TALK.contains(w)).count();
    let Some(first) = words.get(skip) else { return n };
    let verb = VERBS.iter().find(|(said, _)| said == first).map(|(_, plain)| *plain).or_else(|| KNOWN.contains(first).then_some(*first));
    match verb {
        // Nothing to ease: the phrase as it was (the commas too: «ответь да, приду»).
        Some(v) if skip == 0 && v == *first => n,
        Some(v) => std::iter::once(v).chain(words[skip + 1..].iter().copied()).collect::<Vec<_>>().join(" "),
        None => n,
    }
}

/// «Включи проектор, открой впн и потом найди на рутубе фильм»: several commands in one
/// phrase → each of them, in order. None unless there are two or more and every one is a
/// command soundya knows — then the phrase is handled as before (one command, or Alice's).
/// - Cut at «потом», «затем», «после этого», «а ещё», commas; at «и» only before a command's
///   verb («включи битлз и роллинг стоунз» is one request).
/// - The device carries over: after «включи проектор», «открой впн» and «найди фильм» are
///   the projector's without naming it again; after a computer's command, the computer's.
pub fn chain(text: &str, keyword_only: bool) -> Option<Vec<Intent>> {
    let n = normalize(text);
    // «Ответь …», «напомни …», «запиши …»: the rest is the user's own words, never commands.
    const FREE_TEXT: [&str; 9] = ["ответь", "напиши", "напомни", "запиши", "добавь", "скажи", "отправь", "передай", "сообщи"];
    if n.split_whitespace().next().is_some_and(|w| FREE_TEXT.contains(&w.trim_matches(','))) {
        return None;
    }
    let mut cut = format!(" {} ", n.replace([',', ';', '.'], " | "));
    for sep in [
        " а потом ", " и потом ", " потом ", " а затем ", " и затем ", " затем ", " после этого ", " после чего ", " а после ", " и после ",
        " далее ", " а еще ", " и еще ", " а также ", " и также ", " также ", " следом ",
    ] {
        cut = cut.replace(sep, " | ");
    }
    let mut parts: Vec<String> = Vec::new();
    for piece in cut.split('|').map(str::trim).filter(|p| !p.is_empty()) {
        // «… и открой ютуб»: «и» before a command's verb. And no word between at all, as the
        // recogniser writes it («включи проектор открой впн»): a verb after a whole command.
        let words: Vec<&str> = piece.split_whitespace().collect();
        let mut from = 0;
        for i in 1..words.len() {
            if i <= from {
                continue;
            }
            if words[i] == "и" && i + 1 < words.len() && starts_command(&words[i + 1..].join(" ")) {
                parts.push(words[from..i].join(" "));
                from = i + 1;
            } else if starts_command(words[i]) && words[i - 1] != "и" {
                let whole = intent(&words[from..i].join(" "), keyword_only);
                if whole.is_some_and(|w| !matches!(w, Intent::Ask(_) | Intent::Tv(TvAction::Unclear))) {
                    parts.push(words[from..i].join(" "));
                    from = i;
                }
            }
        }
        parts.push(words[from..].join(" "));
    }
    let parts: Vec<String> = parts.into_iter().filter(|p| !p.is_empty() && p != "и").collect();
    if parts.len() < 2 {
        return None;
    }
    let names_device = |p: &str| strip_tv(p).is_some() || [" компьютер", " комп", " пк", " ноутбук"].iter().any(|w| format!(" {p}").contains(w));
    let vague = |i: &Option<Intent>| matches!(i, None | Some(Intent::Ask(_)) | Some(Intent::Tv(TvAction::Unclear)));
    let mut out: Vec<Intent> = Vec::new();
    // The device the previous command was for: (a TV?, its name if it was named).
    let mut on: Option<(bool, Option<String>)> = None;
    for p in &parts {
        let alone = intent(p, keyword_only);
        let carried = match &on {
            Some((tv, name)) if !names_device(p) => {
                let place = if *tv { "на телевизоре" } else { "на компьютере" };
                let there = intent(&format!("{p} {place}"), keyword_only).filter(|i| !matches!(i, Intent::Tv(TvAction::Unclear)));
                // Aimed at the same named one («…в спальне»).
                there.map(|i| match (name, i) {
                    (Some(name), i @ (Intent::Tv(_) | Intent::Pc(_))) => Intent::On(name.clone(), Box::new(i)),
                    (_, i) => i,
                })
            }
            _ => None,
        };
        // The device's reading wins when the phrase alone says nothing clear, or would go to
        // the other kind of device only because the settings send unnamed commands there.
        let step = match (&on, alone, carried) {
            (Some((true, _)), a, Some(c)) if vague(&a) || matches!(a, Some(Intent::Pc(_))) => c,
            (Some((false, _)), a, Some(c)) if vague(&a) || matches!(a, Some(Intent::Tv(_))) => c,
            (_, Some(a), _) if !matches!(a, Intent::Ask(_) | Intent::Tv(TvAction::Unclear) | Intent::ToAlice) => a,
            _ => return None,
        };
        on = match &step {
            Intent::Tv(_) => Some((true, None)),
            Intent::Pc(_) => Some((false, None)),
            Intent::On(name, inner) => Some((matches!(**inner, Intent::Tv(_)), Some(name.clone()))),
            _ => on,
        };
        out.push(step);
    }
    Some(out)
}

/// Does it begin like a command (after `plain_command`'s easing)?
fn starts_command(part: &str) -> bool {
    const VERBS: [&str; 30] = [
        "включи", "выключи", "открой", "запусти", "закрой", "найди", "поищи", "поставь", "сделай", "нажми", "переключи", "перемотай",
        "останови", "продолжи", "убавь", "прибавь", "покажи", "подключи", "отключи", "усыпи", "разбуди", "верни", "вернись", "отмотай",
        "выбери", "зайди", "перейди", "воспроизведи", "приглуши", "набери",
    ];
    plain_command(part).split_whitespace().next().is_some_and(|w| VERBS.contains(&w))
}

pub fn intent(text: &str, keyword_only: bool) -> Option<Intent> {
    let text = &plain_command(text);
    // A TV or computer named by the user: its command, aimed at it.
    let devices = DEVICES.lock().unwrap().clone();
    if !devices.is_empty() {
        if let Some((name, tv, phrase)) = device_in(&normalize(text), &devices) {
            match intent_plain(&phrase, keyword_only) {
                Some(i @ Intent::Tv(_)) if tv => return Some(Intent::On(name, Box::new(i))),
                Some(i @ Intent::Pc(_)) if !tv => return Some(Intent::On(name, Box::new(i))),
                _ => {}
            }
        }
    }
    intent_plain(text, keyword_only).or_else(|| tv_unclear(text))
}

fn intent_plain(text: &str, keyword_only: bool) -> Option<Intent> {
    // A ringing alarm first: its «стоп», «я проснулся» and the answer to its sum.
    if let Some(v) = crate::alarm::voice(&normalize(text), crate::alarm::ringing(), crate::planner::now(), crate::planner::offset()) {
        return Some(Intent::Alarm(v));
    }
    // «Алиса, пульт» before all: «включи пульт телевизора» is the remote mode, not an app.
    if let Some(on) = remote_phrase(&normalize(text)) {
        return Some(Intent::Tv(TvAction::RemoteMode(on)));
    }
    // Bluetooth before the TV's follow-ups: in «режим пульта» «найди блютуз устройства» was a
    // search on the TV, and «переключи блютуз» skipped the track. Not when the TV is named.
    if strip_tv(&normalize(text)).is_none() {
        if let Some(i) = bt_voice(&normalize(text)) {
            return Some(i);
        }
    }
    // The TV first: «перемотай вперёд на проекторе», «открой ютуб на проекторе».
    let n = normalize(text);
    let named = tv(&n);
    // «Поставь видео на паузу на проекторе» is its pause, not an app called «видео на паузу»:
    // `tv()` takes «включи / поставь / открой X» as «open X» when nothing else fits.
    let action = match named {
        // «Поставь таймер на десять минут на телевизоре» is Alice's timer, not an app «таймер…».
        Some(TvAction::Open(ref app)) if app.contains("таймер") || app.contains("будильник") || app.contains("напомин") => None,
        // «Поставь фильм на паузу на проекторе»: `tv()` reads «на паузу» as a film's title.
        Some(TvAction::Search(ref q)) if q.contains("пауз") => tv_named_rest(&n, false).or(named),
        // Unclear comes later (below), after Telegram, timers, the planner and the computer:
        // «напиши маме что на телевизоре идёт фильм» only mentions the TV.
        Some(TvAction::Open(_)) | None => tv_named_rest(&n, false).or(named).or_else(|| tv_unnamed(&n)),
        known => known,
    };
    if let Some(action) = action {
        return Some(Intent::Tv(action));
    }
    // No device named and it fits several: the settings say where («Куда отправлять команды»).
    if let Some(i) = routed(&normalize(text)) {
        return Some(i);
    }
    if matches!(
        normalize(text).as_str(),
        "повтори адрес" | "повтори адрес саундя" | "скажи адрес" | "скажи адрес саундя" | "какой адрес саундя"
            | "какой адрес у саундя" | "адрес саундя" | "адрес панели саундя" | "повтори адрес панели"
    ) {
        return Some(Intent::Address);
    }
    match normalize(text).as_str() {
        "говори тише" | "говори потише" | "сделай голос тише" | "сделай свой голос тише" | "говори тише пожалуйста" => {
            return Some(Intent::VoiceLevel(-15))
        }
        "говори громче" | "сделай голос громче" | "сделай свой голос громче" | "говори громче пожалуйста" => {
            return Some(Intent::VoiceLevel(15))
        }
        _ => {}
    }
    if crate::telegram::active() {
        let n = normalize(text);
        if crate::telegram::awaiting() {
            match n.as_str() {
                "да" | "отправь" | "отправляй" | "подтверждаю" | "да отправь" => return Some(Intent::TgConfirm(true)),
                "нет" | "отмена" | "не отправляй" | "не надо" => return Some(Intent::TgConfirm(false)),
                _ => {}
            }
        }
        for p in ["ответь в телеграм ", "ответь в телеграме ", "ответь "] {
            if let Some(rest) = n.strip_prefix(p) {
                if !rest.trim().is_empty() {
                    return Some(Intent::TgReply(rest.trim().to_owned()));
                }
            }
        }
    }
    if crate::update::awaiting() {
        match normalize(text).as_str() {
            "да" | "установи" | "ставь" | "да установи" | "обнови" | "да обнови" => return Some(Intent::UpdateConfirm(true)),
            "нет" | "отмена" | "не надо" | "не устанавливай" | "не ставь" => return Some(Intent::UpdateConfirm(false)),
            _ => {}
        }
    }
    if let Some(v) = crate::timer::voice(&normalize(text), crate::timer::current().is_some()) {
        return Some(Intent::Timer(v));
    }
    if crate::planner::ENABLED.load(std::sync::atomic::Ordering::Acquire) {
        // The answer to «сделали?» / «через сколько уточнить?» asked a moment ago.
        if crate::planner::awaiting() {
            let n = normalize(text);
            if crate::planner::answer(&n, crate::planner::now(), crate::planner::offset()) != crate::planner::Answer::Unknown {
                return Some(Intent::Planner(crate::planner::Voice::CheckAnswer(n)));
            }
        }
        if let Some(v) = crate::planner::voice(&normalize(text)) {
            return Some(Intent::Planner(v));
        }
    } else if crate::calendar::active() {
        // Only a calendar on: questions are ours; with Google also «добавь задачу / в календарь»,
        // «отметь выполненной»; «напомни …» stays Alice's.
        if let Some(v) = crate::planner::voice(&normalize(text)) {
            let ours = v.is_query() || (crate::calendar::google_on() && !matches!(v, crate::planner::Voice::Remind(_)));
            if ours {
                return Some(Intent::Planner(v));
            }
        }
    }
    if let Some(d) = duck_command(&normalize(text)) {
        return Some(Intent::Duck(d));
    }
    if let Some(ms) = seek_command(&normalize(text)) {
        return Some(Intent::Seek(ms));
    }
    let rules = pc_rules();
    if let Some(action) = pc_question(&normalize(text)).filter(|a| pc_allowed(&normalize(text), a, true, &rules)) {
        return Some(Intent::Pc(action));
    }
    if let Some((action, named)) = pc_named(&normalize(text)) {
        // «Закрой окно» for Home Assistant, «выключи компьютер» by his own automation:
        // what the «Компьютер» menu leaves to Alice stays hers.
        if pc_allowed(&normalize(text), &action, named, &rules) {
            return Some(Intent::Pc(action));
        }
    }
    // The TV is named and nothing above took the phrase: still the TV's, never the music's.
    if let Some(a) = tv_named_rest(&normalize(text), true) {
        return Some(Intent::Tv(a));
    }
    // «Включи бонобо в яндекс музыке», «продолжи в яндекс музыке»: Alice's, whatever the mode.
    if names_yandex(&normalize(text)) {
        let n = normalize(text);
        let music = PLAY_VERBS.iter().any(|v| n.starts_with(v)) || transport(&n).is_some();
        return music.then_some(Intent::ToAlice);
    }
    // Yandex Music's own playlists, podcasts and books: never SoundCloud's
    // unless SoundCloud is named («включи плейлист дня», «включи подкаст …»).
    if let Some(i) = yandex_only(&normalize(text)) {
        return Some(i);
    }
    let (norm, named) = strip_service(&normalize(text));
    if named
        && matches!(
            norm.as_str(),
            "" | "включи" | "запусти" | "открой" | "поставь" | "играй" | "включи музыку" | "запусти музыку"
                | "поставь музыку" | "включи что-нибудь" | "поставь что-нибудь" | "включи музыку из"
        )
    {
        return Some(Intent::Default);
    }
    const RECS: [&str; 16] = [
        "включи рекомендации",
        "поставь рекомендации",
        "включи мои рекомендации",
        "рекомендации",
        "порекомендуй музыку",
        "порекомендуй что-нибудь",
        "посоветуй музыку",
        "включи что-нибудь новое",
        "включи что нибудь новое",
        "включи новое",
        "включи новую музыку",
        "включи что-нибудь на мой вкус",
        "включи на мой вкус",
        "включи музыку на мой вкус",
        "включи мою волну",
        "включи волну",
    ];
    if RECS.contains(&norm.as_str()) {
        return (named || !keyword_only).then_some(Intent::Recommend);
    }
    if let Some(i) = extras(&norm) {
        return Some(i);
    }
    if let Some((core, on)) = shuffle_words(&norm) {
        if !on {
            return Some(Intent::Shuffle(false));
        }
        let generic = matches!(
            core.as_str(),
            "" | "включи" | "поставь" | "музыку" | "включи музыку" | "треки" | "песни" | "очередь" | "все" | "включи все" | "плейлист" | "этот плейлист"
        );
        if generic {
            return Some(Intent::Shuffle(true));
        }
        let cmd = if ["включи ", "поставь ", "запусти ", "сыграй "].iter().any(|p| core.starts_with(p)) {
            core
        } else {
            format!("включи {core}")
        };
        let cmd = if named { format!("{cmd} в саундклауде") } else { cmd };
        return match intent_core(&cmd, keyword_only) {
            Some(i @ (Intent::Play(_) | Intent::Playlist(_) | Intent::Album(_) | Intent::Likes | Intent::MyPlaylist(_))) => {
                Some(Intent::Shuffled(Box::new(i)))
            }
            // «Включи мои лайки вперемешку» with Yandex Music by default: hers.
            Some(Intent::ToAlice) => Some(Intent::ToAlice),
            _ => Some(Intent::Shuffle(true)),
        };
    }
    intent_core(text, keyword_only)
}

fn intent_core(text: &str, keyword_only: bool) -> Option<Intent> {
    let (text, named) = strip_service(&normalize(text));
    match text.as_str() {
        "стоп" | "пауза" | "останови музыку" | "выключи музыку" | "поставь на паузу"
        | "останови" | "хватит" => return Some(Intent::Pause),
        "дальше" | "следующий" | "следующий трек" | "следующая песня" | "переключи трек"
        | "переключи" => return Some(Intent::Next),
        "назад" | "предыдущий трек" | "предыдущая песня" => return Some(Intent::Previous),
        "продолжи" | "продолжи музыку" | "продолжай музыку" | "продолжай" => {
            return Some(Intent::Resume)
        }
        "включи музыку" if !named => return Some(Intent::Resume),
        _ => {}
    }
    if let Some(t) = transport(&text) {
        return Some(t);
    }
    let rest = ["включи ", "поставь ", "запусти ", "сыграй ", "играй "]
        .iter()
        .find_map(|p| text.strip_prefix(p))?
        .trim();
    // With Yandex Music as the default service (keyword mode) likes and own
    // playlists are hers too — she has both; 0.4.40 took them for SoundCloud.
    if !keyword_only || named {
        if matches!(rest, "мои лайки" | "лайки" | "мои лайкнутые треки") {
            return Some(Intent::Likes);
        }
        for p in ["мой плейлист ", "мою подборку ", "мой альбом "] {
            if let Some(q) = rest.strip_prefix(p) {
                let q = q.trim().trim_matches('"').trim();
                return (!q.is_empty()).then(|| Intent::MyPlaylist(q.to_owned()));
            }
        }
    }
    if keyword_only && !named {
        // A music request for Alice: SoundCloud pauses so the two do not play at once.
        let home = rest.split_whitespace().any(|w| HOME.iter().any(|p| w.starts_with(p)));
        return (!home && !rest.is_empty()).then_some(Intent::ToAlice);
    }
    if matches!(rest, "мою музыку" | "мои треки" | "мое любимое" | "любимое" | "мои любимые треки") {
        return Some(Intent::Likes);
    }
    for p in ["альбом ", "пластинку "] {
        if let Some(q) = rest.strip_prefix(p) {
            let q = q.trim().trim_matches('"').trim();
            return (!q.is_empty()).then(|| Intent::Album(q.to_owned()));
        }
    }
    for p in ["плейлист ", "подборку "] {
        if let Some(q) = rest.strip_prefix(p) {
            let q = q.trim().trim_matches('"').trim();
            return (!q.is_empty()).then(|| Intent::Playlist(q.to_owned()));
        }
    }
    let explicit = ["песню ", "трек ", "музыку ", "группу ", "исполнителя ", "микс "]
        .iter()
        .find_map(|p| rest.strip_prefix(p));
    let query = explicit.unwrap_or(rest).trim().trim_matches('"').trim();
    if query.is_empty() || query.len() > 400 {
        return None;
    }
    if query == "музыку" {
        return Some(Intent::Resume);
    }

    if explicit.is_none()
        && !named
        && query
            .split_whitespace()
            .any(|w| HOME.iter().any(|p| w.starts_with(p)))
    {
        return None;
    }
    Some(Intent::Play(query.to_owned()))
}

/// «Включи радио», «поставь сказку», «включи яндекс музыку»: Alice starts
/// something to listen to (not the light, a timer or the TV).
fn starts_music(text: &str) -> bool {
    let n = normalize(text);
    let Some(rest) = PLAY_VERBS.iter().find_map(|v| n.strip_prefix(v)) else { return false };
    !rest.split_whitespace().any(|w| HOME.iter().any(|p| w.starts_with(p) && *p != "радио" && *p != "сказк"))
}

// ---- minimal protobuf reader/writer ----

fn varint(data: &[u8], offset: &mut usize) -> Option<u64> {
    let mut value = 0u64;
    for shift in (0..70).step_by(7) {
        let byte = *data.get(*offset)?;
        *offset += 1;
        if shift == 63 && byte > 1 {
            return None;
        }
        value |= u64::from(byte & 127) << shift;
        if byte < 128 {
            return Some(value);
        }
    }
    None
}

#[derive(Clone, Copy)]
enum Value<'a> {
    Number(u64),
    Bytes(&'a [u8]),
}

fn field(data: &[u8], wanted: u64) -> Option<Value<'_>> {
    let mut offset = 0;
    while offset < data.len() {
        let key = varint(data, &mut offset)?;
        if key >> 3 == 0 {
            return None;
        }
        let value = match key & 7 {
            0 => Value::Number(varint(data, &mut offset)?),
            2 => {
                let len = usize::try_from(varint(data, &mut offset)?).ok()?;
                let end = offset.checked_add(len)?;
                let bytes = data.get(offset..end)?;
                offset = end;
                Value::Bytes(bytes)
            }
            1 | 5 => {
                offset = offset.checked_add(if key & 7 == 1 { 8 } else { 4 })?;
                if offset > data.len() {
                    return None;
                }
                continue;
            }
            _ => return None,
        };
        if key >> 3 == wanted {
            return Some(value);
        }
    }
    None
}

fn bytes(data: &[u8], n: u64) -> Option<&[u8]> {
    match field(data, n)? {
        Value::Bytes(v) => Some(v),
        Value::Number(_) => None,
    }
}

fn number(data: &[u8], n: u64) -> Option<u64> {
    match field(data, n)? {
        Value::Number(v) => Some(v),
        Value::Bytes(_) => None,
    }
}

/// Alice's state from an AliceState frame (2 = speaking).
fn alice_state(data: &[u8]) -> Option<u64> {
    if number(data, 253) != Some(3) {
        return None;
    }
    Some(number(bytes(data, 252)?, 1).unwrap_or(0))
}

/// Set while soundya itself is talking through Alice, so it is not cut off.
static OWN_SPEECH: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Until when (ms since the Unix epoch) Alice's own answers are cut off; set by `quiet_for`.
static QUIET_UNTIL: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Cuts off whatever Alice starts saying in the next `time` (e.g. her answer to
/// the ▶‖ button, which soundya handles itself).
pub fn quiet_for(time: Duration) {
    QUIET_UNTIL.store(now_ms() + time.as_millis() as u64, std::sync::atomic::Ordering::Release);
    cancel_alice();
}

/// Alice is speaking now (her AliceState frames: 2 = speaking).
static SPEAKING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
/// A cut-off is being repeated (one at a time).
static CUTTING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Cuts off Alice's answer and repeats it until she is quiet: one cancel is sometimes
/// dropped by the firmware (she then says «нужна подписка Плюс…» in full, 9 s).
fn cut_off() {
    use std::sync::atomic::Ordering;
    cancel_alice();
    if CUTTING.swap(true, Ordering::AcqRel) {
        return;
    }
    thread::spawn(|| {
        for wait in [250, 400, 600, 800, 1000] {
            thread::sleep(Duration::from_millis(wait));
            if !SPEAKING.load(Ordering::Acquire) || OWN_SPEECH.load(Ordering::Acquire) {
                break;
            }
            eprintln!("voice: Alice still speaking, cutting off again");
            cancel_alice();
        }
        CUTTING.store(false, Ordering::Release);
    });
}

fn quiet() -> bool {
    QUIET_UNTIL.load(std::sync::atomic::Ordering::Acquire) > now_ms()
}

fn alice_busy(data: &[u8]) -> Option<bool> {
    if number(data, 253) != Some(3) {
        return None;
    }
    let state = bytes(data, 252)?;
    match number(state, 1).unwrap_or(0) {
        0 | 6 => Some(false),
        1 | 2 | 3 | 5 => Some(true),
        _ => None,
    }
}

/// Final recognized phrase: (request id, text).
fn recognized(data: &[u8]) -> Option<(String, String)> {
    if number(data, 253) != Some(3) {
        return None;
    }
    let state = bytes(data, 252)?;
    // Dev build, while the alarm rings: every recognition frame in the log — its answer to
    // the sum («восемь») once never got through, and this shows what Alice sent instead.
    #[cfg(feature = "dev")]
    if crate::alarm::ringing() {
        if let Some(r) = bytes(state, 15) {
            let t = bytes(r, 1).map(String::from_utf8_lossy).unwrap_or_default();
            let raw = bytes(state, 2).map(String::from_utf8_lossy).unwrap_or_default();
            if !t.is_empty() || !raw.is_empty() {
                eprintln!("dev: alarm heard: state {:?}, flags {:?}/{:?}, text {t:?}, raw {raw:?}", number(state, 1), number(r, 2), number(r, 4));
            }
        }
    }
    if !matches!(number(state, 1), Some(1 | 3)) {
        return None;
    }
    let result = bytes(state, 15)?;
    if number(result, 2) != Some(1) || number(result, 4) != Some(1) {
        return None;
    }
    let text = std::str::from_utf8(bytes(result, 1)?).ok()?;
    if text.is_empty() || text.len() > 512 {
        return None;
    }
    // The final text is the same as the request's own; a number may be written differently
    // there («восемь» / «8»), so an alarm answer passes either way.
    if Some(text.as_bytes()) != bytes(state, 2) && !alarm_answer(text) && !question_answer(text) {
        return None;
    }
    let request = std::str::from_utf8(bytes(state, 7)?).ok()?;
    if request.len() != 36 {
        return None;
    }
    Some((request.to_owned(), text.to_owned()))
}

/// While soundya's own question waits: is this phrase an answer to it («первое» comes as «1»)?
fn question_answer(text: &str) -> bool {
    pending_options().is_some_and(|o| answer(&o, &normalize_phrase(text)) != Answer::No)
}

/// While the alarm waits for the answer to its sum: is this phrase a number?
fn alarm_answer(text: &str) -> bool {
    crate::alarm::ringing() && matches!(crate::alarm::voice(&normalize(text), true, 0, 0), Some(crate::alarm::Voice::Answer(_)))
}

fn put_var(out: &mut Vec<u8>, mut v: u64) {
    while v >= 128 {
        out.push((v as u8 & 127) | 128);
        v >>= 7;
    }
    out.push(v as u8);
}
fn put_number(out: &mut Vec<u8>, field: u64, value: u64) {
    put_var(out, field << 3);
    put_var(out, value);
}
fn put_bytes(out: &mut Vec<u8>, field: u64, value: &[u8]) {
    put_var(out, (field << 3) | 2);
    put_var(out, value.len() as u64);
    out.extend_from_slice(value);
}

fn rpc(capability: &str, field: u64, method: &[u8]) -> Vec<u8> {
    let mut remoting = Vec::new();
    put_bytes(&mut remoting, 7, capability.as_bytes());
    put_bytes(&mut remoting, field, method);
    let mut message = Vec::new();
    put_bytes(&mut message, 252, &remoting);
    put_number(&mut message, 253, 210);
    let mut frame = (message.len() as u32).to_le_bytes().to_vec();
    frame.extend_from_slice(&message);
    frame
}

fn send_native(frame: &[u8]) -> io::Result<()> {
    let address: SocketAddr = ADDRESS.parse().expect("address");
    let mut socket = TcpStream::connect_timeout(&address, Duration::from_millis(500))?;
    socket.set_write_timeout(Some(Duration::from_millis(500)))?;
    socket.write_all(frame)?;
    socket.shutdown(std::net::Shutdown::Write)
}

/// Stops Alice's own answer to the phrase we took over.
pub fn cancel_alice() {
    let _ = send_native(&rpc("AliceCapability", 8, &[8, 3]));
}

/// Says `text` in Alice's voice (START_VOCALIZE, as in yandex-station-extended's alice-say.py).
/// Runs a moment later so it is not swallowed by the cancel of the user's own request.
pub fn say(text: String) {
    thread::spawn(move || {
        thread::sleep(Duration::from_millis(600));
        let since = since_heard();
        let start = std::time::Instant::now();
        match speak(&text) {
            Ok(()) => eprintln!("say: {text:?} (from the phrase: {since}, speaking: {} ms)", start.elapsed().as_millis()),
            Err(e) => eprintln!("say: {text:?}: {e} ({since})"),
        }
    });
}

/// Says it and returns once she is done (or gave up): for things said one after another.
pub fn say_wait(text: &str) {
    let start = std::time::Instant::now();
    match speak(text) {
        Ok(()) => eprintln!("say: {text:?} (speaking: {} ms)", start.elapsed().as_millis()),
        Err(e) => {
            eprintln!("say: {text:?}: {e}");
            // Some firmwares speak without telling when they are done: give the phrase the
            // time it takes to say (about 13 letters a second), so the next one does not cut in.
            let takes = Duration::from_millis(1000 + text.chars().count() as u64 * 1000 / 13);
            if let Some(left) = takes.checked_sub(start.elapsed()) {
                thread::sleep(left);
            }
        }
    }
}

/// When the last phrase soundya took over was recognized: the log says how long answers take.
static HEARD_AT: std::sync::Mutex<Option<std::time::Instant>> = std::sync::Mutex::new(None);

pub fn since_heard() -> String {
    HEARD_AT.lock().unwrap().map_or("?".into(), |t| format!("{} ms", t.elapsed().as_millis()))
}

/// Subscribes to Alice's events and asks her to speak on the same connection,
/// keeping it open until the speech ends: closing early can cut the speech off.
fn speak(text: &str) -> io::Result<()> {
    let address: SocketAddr = ADDRESS.parse().expect("address");
    let mut socket = TcpStream::connect_timeout(&address, Duration::from_secs(1))?;
    socket.set_read_timeout(Some(Duration::from_secs(15)))?;
    let mut directive = Vec::new();
    put_bytes(&mut directive, 1, text.as_bytes());
    put_number(&mut directive, 2, 1);
    let mut method = Vec::new();
    put_number(&mut method, 1, 18); // START_VOCALIZE
    put_number(&mut method, 3, 1); // send events
    put_bytes(&mut method, 13, &directive);
    let mut subscribe = Vec::new();
    put_bytes(&mut subscribe, 1, b"AliceCapability");
    let mut frames = rpc("AliceCapability", 20, &subscribe);
    frames.extend(rpc("AliceCapability", 8, &method));
    OWN_SPEECH.store(true, std::sync::atomic::Ordering::Release);
    struct Done;
    impl Drop for Done {
        fn drop(&mut self) {
            OWN_SPEECH.store(false, std::sync::atomic::Ordering::Release);
        }
    }
    let _done = Done;
    socket.write_all(&frames)?;
    let started = std::time::Instant::now();
    // Waiting for her to start: 5 s (not 15) — a phrase she took starts within a second or two.
    socket.set_read_timeout(Some(Duration::from_secs(5)))?;
    let mut speaking = false;
    let mut data = Vec::new();
    // A read timeout (EAGAIN on Linux) is not a failure once she started speaking: some
    // firmwares never send «completed». Before that it means she did not take the phrase.
    let quiet = |e: io::Error, speaking: bool| -> io::Result<()> {
        match e.kind() {
            io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut if speaking => Ok(()),
            io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut => Err(io::Error::other("Алиса не сказала, что говорит (5 с)")),
            _ => Err(e),
        }
    };
    while started.elapsed() < Duration::from_secs(30) {
        if speaking {
            // Speaking: a long phrase may send nothing for a while — it is not cut short.
            let _ = socket.set_read_timeout(Some(Duration::from_secs(15)));
        }
        let mut len = [0u8; 4];
        if let Err(e) = socket.read_exact(&mut len) {
            return quiet(e, speaking);
        }
        let size = u32::from_le_bytes(len) as usize;
        if size == 0 || size > MAX_FRAME {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "frame length"));
        }
        data.resize(size, 0);
        if let Err(e) = socket.read_exact(&mut data) {
            return quiet(e, speaking);
        }
        if number(&data, 253) != Some(3) {
            continue;
        }
        // Alice's state on the same connection: some firmwares (the Lite) send no TTS events,
        // and every phrase waited 15 s for one — the long pauses between the morning's phrases.
        // Speaking (2), then back to idle (0 / 6): done, the next phrase goes at once.
        match alice_state(&data) {
            Some(2) => speaking = true,
            Some(0 | 6) if speaking => return Ok(()),
            _ => {}
        }
        let Some(event) = bytes(&data, 252).and_then(|r| bytes(r, 18)) else {
            continue;
        };
        match number(event, 1) {
            Some(6) => speaking = true,          // TTS_STARTED
            Some(1) if speaking => return Ok(()), // TTS_COMPLETED
            Some(4) => return Err(io::Error::other("Alice reported an error")),
            _ => {}
        }
    }
    Ok(())
}

/// Random UUID v4 string (no extra crate needed).
fn request_id() -> String {
    use std::hash::{BuildHasher, Hasher};
    let mut bytes = [0u8; 16];
    for chunk in bytes.chunks_mut(8) {
        let mut h = std::collections::hash_map::RandomState::new().build_hasher();
        h.write_u128(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0),
        );
        chunk.copy_from_slice(&h.finish().to_le_bytes());
    }
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    format!("{}-{}-{}-{}-{}", &hex[..8], &hex[8..12], &hex[12..16], &hex[16..20], &hex[20..])
}

/// Pauses the built-in music player so the tracks do not overlap.
pub fn pause_native() {
    let mut source = Vec::new();
    put_number(&mut source, 1, 14);
    put_number(&mut source, 2, 3);
    put_number(&mut source, 3, 1);
    put_bytes(&mut source, 4, request_id().as_bytes());
    put_bytes(&mut source, 6, b"SoundyaVoice");
    let mut method = vec![8, 1];
    put_bytes(&mut method, 5, &source);
    put_number(&mut method, 7, 0);
    let _ = send_native(&rpc("PlaybackControlCapability", 13, &method));
}

fn read_frames(
    socket: &mut TcpStream,
    tx: &Sender<Cmd>,
    shared: &Shared,
    seen: &mut VecDeque<String>,
) -> io::Result<()> {
    let mut data = Vec::with_capacity(4096);
    // After taking over a phrase, Alice's own late answer («нужна подписка…») is cut off.
    let mut suppress_until: Option<std::time::Instant> = None;
    let mut last_cancel = std::time::Instant::now();
    loop {
        let mut len = [0u8; 4];
        socket.read_exact(&mut len)?;
        let size = u32::from_le_bytes(len) as usize;
        if size == 0 || size > MAX_FRAME {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "frame length"));
        }
        data.resize(size, 0);
        socket.read_exact(&mut data)?;
        if let Some(busy) = alice_busy(&data) {
            shared.duck.set_busy(busy);
            // «Громкость голоса Алисы»: the mixer down while she listens and speaks.
            crate::voice::alice_busy(busy);
        }
        if let Some(state) = alice_state(&data) {
            SPEAKING.store(state == 2, std::sync::atomic::Ordering::Release);
            #[cfg(feature = "dev")]
            eprintln!("dev: alice state {state}");
        }
        // A phrase we took over: her answer is cancelled while she is still preparing it
        // (state 3, «думаю»). The cancel sent with the phrase itself comes while she is still
        // listening and is ignored; at «speaking» she has already said «Чтобы…» of «Чтобы
        // слушать музыку, надо оформить подписку Плюс».
        if alice_state(&data) == Some(3)
            && (suppress_until.is_some_and(|t| std::time::Instant::now() < t) || quiet())
            && !OWN_SPEECH.load(std::sync::atomic::Ordering::Acquire)
        {
            eprintln!("voice: cancelling Alice's own answer before she speaks");
            cancel_alice();
        }
        if alice_state(&data) == Some(2)
            && (suppress_until.is_some_and(|t| std::time::Instant::now() < t) || quiet())
            && !OWN_SPEECH.load(std::sync::atomic::Ordering::Acquire)
            && last_cancel.elapsed() > Duration::from_millis(400)
        {
            eprintln!("voice: cutting off Alice's own answer");
            cut_off();
            last_cancel = std::time::Instant::now();
        }
        // Dev build: what Alice's alarm looks like on the wire (for «сводка вместе с будильником»).
        #[cfg(feature = "dev")]
        if let Some(seen) = alarm_trace(&data) {
            eprintln!("dev: alarm frame: {seen}");
        }
        if let Some((request, text)) = recognized(&data) {
            if seen.contains(&request) {
                continue;
            }
            seen.push_back(request);
            if seen.len() > 64 {
                seen.pop_front();
            }
            // For programs on the PC (Ditoo Plus), if allowed («share_heard»).
            shared.set_heard(&text);
            // The phrase is soundya's: Alice's own answer to it is cancelled now, again when she
            // starts preparing it and cut off if she speaks (the window below). Scenarios and
            // answers used to send the first cancel only — she still listens then and ignores
            // it, so «не могу сделать это на вашем устройстве» came before the scenario ran.
            let mut take = || {
                *HEARD_AT.lock().unwrap() = Some(std::time::Instant::now());
                // The owner spoke over soundya's own speech (a list being read): his phrase wins.
                OWN_SPEECH.store(false, std::sync::atomic::Ordering::Release);
                cancel_alice();
                suppress_until = Some(std::time::Instant::now() + Duration::from_secs(6));
            };
            // An answer to soundya's own question first («Алиса, на проекторе», «Алиса, первое»).
            // Not while the alarm rings: its «стоп» and its sum come before everything.
            if !crate::alarm::ringing() {
                if let Some(step) = take_answer(&text) {
                    eprintln!("voice: {text:?} -> answer: {step:?}");
                    take();
                    match step {
                        Some(Step::Voice(i)) => {
                            let _ = tx.send(Cmd::Voice(i));
                        }
                        Some(Step::Control(c)) => {
                            let (dtx, _drx) = std::sync::mpsc::channel();
                            let _ = tx.send(Cmd::Control(c, dtx));
                        }
                        None => say("Хорошо, отменила".into()),
                    }
                    continue;
                }
            }
            // A user macro whose phrase is heard wins over built-in handling —
            // the whole phrase, not a part of it (a macro «свет» must not take
            // «включи свет на кухне» from Alice).
            let normalized = normalize_phrase(&text);
            if let Some((phrase, _)) = shared.macros().into_iter().find(|(p, _)| macro_matches(&normalized, p)) {
                eprintln!("voice: {text:?} -> macro {phrase:?}");
                take();
                clear_question();
                let (dtx, _drx) = std::sync::mpsc::channel();
                let _ = tx.send(Cmd::Control(format!("macro run {phrase}"), dtx));
                continue;
            }
            // Several commands in one phrase («включи проектор, открой впн и найди фильм»):
            // all of them soundya's, or it is not a chain.
            if !crate::alarm::ringing() {
                if let Some(steps) = chain(&text, shared.keyword_only()).filter(|c| c.iter().all(|i| owns(i, &Who::of(shared)))) {
                    eprintln!("voice: {text:?} -> chain {steps:?}");
                    take();
                    clear_question();
                    let _ = tx.send(Cmd::Chain(steps));
                    continue;
                }
            }
            let intent = contact_allowed(&text)
                .then(|| message_to_contact(&text, &shared.contact_names()))
                .flatten()
                .or_else(|| intent(&text, shared.keyword_only()));
            let Some(intent) = intent else {
                // «Включи радио», «включи яндекс музыку»…: her music from now on.
                if starts_music(&text) {
                    shared.set_alice_music();
                }
                eprintln!("voice: {text:?} left to Alice");
                // Her own «громче» / «тише»: soundya only follows the level.
                crate::volume::heard(&text);
                continue;
            };
            if intent == Intent::ToAlice {
                shared.set_alice_music();
                // Alice plays it; SoundCloud steps aside instead of playing over her.
                if shared.playing.load(std::sync::atomic::Ordering::Acquire) {
                    eprintln!("voice: {text:?} -> Alice's music; pausing SoundCloud");
                    let _ = tx.send(Cmd::Voice(Intent::Pause));
                } else {
                    eprintln!("voice: {text:?} left to Alice (her music)");
                }
                tv_forget_pause();
                crate::volume::heard(&text);
                continue;
            }
            // SoundCloud switched off in the settings: only the PC, the TV and
            // messages are ours; all music stays Alice's.
            let music = !matches!(
                intent,
                Intent::Pc(_) | Intent::Tv(_) | Intent::On(..) | Intent::Address | Intent::BtOut(_) | Intent::BtFind | Intent::BtConnect(..) | Intent::Ask(_) | Intent::Duck(_) | Intent::VoiceLevel(_) | Intent::UpdateConfirm(_) | Intent::Timer(_) | Intent::Planner(_) | Intent::TgReply(_) | Intent::TgConfirm(_)
            ) && !is_alarm(&intent);
            if music && !shared.soundcloud() {
                eprintln!("voice: {text:?} left to Alice (SoundCloud is off)");
                crate::volume::heard(&text);
                continue;
            }
            let ours = owns(&intent, &Who::of(shared));
            // The TV named, its address saved, the block switched off: say so.
            if !ours && matches!(intent, Intent::Tv(_)) && TV_OFF.load(std::sync::atomic::Ordering::Acquire) && strip_tv(&normalize(&text)).is_some() {
                eprintln!("voice: {text:?} -> {intent:?}: the TV is switched off in the settings");
                take();
                say("Управление телевизором выключено в настройках саунди. Откройте страницу саунди и включите блок Телевизор".into());
                continue;
            }
            if !ours {
                let why = match &intent {
                    Intent::Tv(_) => "no TV is set up",
                    Intent::Pc(_) => "no computer is set up",
                    _ => "SoundCloud is not playing",
                };
                eprintln!("voice: {text:?} -> {intent:?}, left to Alice ({why})");
                crate::volume::heard(&text);
                continue;
            }
            eprintln!("voice: {text:?} -> {intent:?}");
            take();
            // The owner moved on: the question he did not answer is over.
            clear_question();
            if music {
                tv_forget_pause();
            }
            let _ = tx.send(Cmd::Voice(intent));
        }
        if data.capacity() > 64 * 1024 {
            data = Vec::with_capacity(4096);
        }
    }
}

/// The printable runs (ASCII, ≥ 4 chars) of a frame that mentions an alarm, None for any other frame.
#[cfg(any(feature = "dev", test))]
fn alarm_trace(data: &[u8]) -> Option<String> {
    let lower = data.to_ascii_lowercase();
    if !lower.windows(5).any(|w| w == b"alarm") {
        return None;
    }
    let runs: Vec<String> = data
        .split(|b| !(0x20..0x7f).contains(b))
        .filter(|r| r.len() >= 4)
        .map(|r| String::from_utf8_lossy(r).into_owned())
        .collect();
    Some(runs.join(" | ").chars().take(400).collect())
}

/// Who has the speaker right now, as far as soundya knows.
#[derive(Debug, Clone, Copy, Default)]
pub struct Who {
    /// SoundCloud is playing (not paused).
    pub playing: bool,
    /// SoundCloud has a track loaded (playing or paused).
    pub active: bool,
    pub has_queue: bool,
    /// The last music started was Alice's own.
    pub alice_music: bool,
    pub airplay: bool,
    /// SoundCloud only when named (Yandex Music is the default service).
    pub keyword_only: bool,
    pub pc: bool,
    pub tv: bool,
}

impl Who {
    fn of(s: &Shared) -> Who {
        use std::sync::atomic::Ordering::Acquire;
        Who {
            playing: s.playing.load(Acquire),
            active: s.is_active(),
            has_queue: s.has_queue(),
            alice_music: s.alice_music(),
            airplay: s.airplay.is_active(),
            keyword_only: s.keyword_only(),
            pc: s.pc_configured(),
            tv: s.tv_configured(),
        }
    }

    /// SoundCloud is the music the user last dealt with: playing, or paused
    /// with nothing of Alice's started since.
    fn ours(&self) -> bool {
        self.playing || (self.active && !self.alice_music)
    }
}

/// Is this phrase soundya's (true) or Alice's (false)?
pub fn owns(intent: &Intent, w: &Who) -> bool {
    match intent {
        // On pause Alice's own music may be playing: «что играет», «стоп» are about hers.
        Intent::WhatPlaying => w.playing || w.airplay,
        Intent::Pause => w.playing,
        Intent::Next | Intent::Previous if w.keyword_only => w.playing,
        Intent::Play(_)
        | Intent::Album(_)
        | Intent::Playlist(_)
        | Intent::Likes
        | Intent::MyPlaylist(_)
        | Intent::Shuffled(_)
        | Intent::Default
        | Intent::Recommend => true,
        // «Продолжи» goes to whoever played last: Alice after her own music,
        // otherwise SoundCloud — even with an empty queue after a restart
        // (where Alice could only answer «нужна подписка»), but not when
        // Yandex Music is the default service: there it is hers unless
        // SoundCloud has something to continue.
        Intent::Resume => w.playing || (!w.alice_music && (w.has_queue || !w.keyword_only)),
        // «Приглуши/верни» only while our music plays (else it is about Alice's);
        // switching the automatic mode is always ours.
        Intent::Duck(DuckCmd::Auto(_)) | Intent::Address | Intent::BtOut(_) | Intent::BtFind | Intent::BtConnect(..) | Intent::Ask(_) | Intent::VoiceLevel(_) | Intent::UpdateConfirm(_) | Intent::Timer(_) | Intent::Planner(_) | Intent::TgReply(_) | Intent::TgConfirm(_) => true,
        Intent::Alarm(_) => true,
        Intent::Duck(_) => w.playing || w.airplay,
        // Only with a PC / TV set up in the settings; otherwise the phrase stays Alice's.
        Intent::Pc(_) => w.pc,
        Intent::Tv(_) => w.tv,
        Intent::On(_, inner) => owns(inner, w),
        // A paused SoundCloud track under Alice's music is not what «перемешай»,
        // «перемотай», «повтори» mean (0.4.40 took them and cut her off).
        Intent::Shuffle(_) | Intent::Repeat(_) => w.has_queue && (w.playing || !w.alice_music),
        _ => w.ours(),
    }
}

/// `owns` for what plays right now (the panel's «hear» in the developer build).
#[cfg(feature = "dev")]
pub fn owns_now(intent: &Intent, shared: &Shared) -> bool {
    owns(intent, &Who::of(shared))
}

/// Where the music comes from, for the page: "soundcloud", "airplay",
/// "alice" (her own player; soundya cannot see it, only that she started it
/// last) or "none".
pub fn source(w: &Who) -> &'static str {
    if w.airplay {
        "airplay"
    } else if w.playing {
        "soundcloud"
    } else if w.alice_music {
        "alice"
    } else if w.active {
        "soundcloud_paused"
    } else {
        "none"
    }
}

pub fn source_of(s: &Shared) -> &'static str {
    source(&Who::of(s))
}

pub fn watch(tx: Sender<Cmd>, shared: std::sync::Arc<Shared>) {
    thread::spawn(move || {
        let mut seen = VecDeque::new();
        let address: SocketAddr = ADDRESS.parse().expect("address");
        loop {
            match TcpStream::connect_timeout(&address, Duration::from_secs(2)) {
                Ok(mut socket) => {
                    eprintln!("voice: connected to Alice");
                    let result = read_frames(&mut socket, &tx, &shared, &mut seen);
                    eprintln!("voice: connection lost ({result:?}); retrying");
                }
                Err(_) => {}
            }
            shared.duck.set_busy(false);
            crate::voice::alice_busy(false);
            thread::sleep(Duration::from_secs(2));
        }
    });
}

#[cfg(test)]
mod tests {
    /// docs/voice-priority.md, one phrase per row, in the order the rules are checked.
    #[test]
    fn priority_table() {
        let kind = |i: Option<Intent>| -> String {
            match i {
                None => "Alice".into(),
                Some(Intent::Tv(_)) => "Tv".into(),
                Some(Intent::Pc(_)) => "Pc".into(),
                Some(Intent::Duck(_)) => "Duck".into(),
                Some(Intent::Seek(_)) => "Seek".into(),
                Some(Intent::VoiceLevel(_)) => "VoiceLevel".into(),
                Some(Intent::Shuffled(_)) => "Shuffled".into(),
                Some(Intent::Play(_)) => "Play".into(),
                Some(Intent::Playlist(_)) => "Playlist".into(),
                Some(Intent::Album(_)) => "Album".into(),
                Some(Intent::MyPlaylist(_)) => "MyPlaylist".into(),
                Some(Intent::Shuffle(_)) => "Shuffle".into(),
                Some(i) => format!("{i:?}"),
            }
        };
        // (phrase, «только по слову саундклауд» / Яндекс Музыка по умолчанию, expected)
        let rows: &[(&str, bool, &str)] = &[
            ("открой ютуб на телевизоре", false, "Tv"),
            ("повтори адрес", false, "Address"),
            ("включи музыку в наушниках", false, "BtOut(true)"),
            ("переключи звук на наушники", true, "BtOut(true)"),
            ("верни музыку на колонку", false, "BtOut(false)"),
            ("верни звук с наушников", false, "BtOut(false)"),
            ("включи блютуз", false, "Alice"),
            ("говори тише", false, "VoiceLevel"),
            ("приглуши музыку", false, "Duck"),
            ("перемотай вперед на 30 секунд", false, "Seek"),
            ("включи компьютер", false, "Pc"),
            ("включи бонобо в яндекс музыке", false, "ToAlice"),
            ("включи плейлист дня", false, "ToAlice"),
            ("включи подкаст", false, "ToAlice"),
            ("включи саундклауд", true, "Default"),
            ("включи мою волну", false, "Recommend"),
            ("включи мою волну", true, "Alice"),
            ("включи мою волну в саундклауде", true, "Recommend"),
            ("включи мои лайки вперемешку", false, "Shuffled"),
            ("пауза", false, "Pause"),
            ("дальше", false, "Next"),
            ("продолжи", false, "Resume"),
            ("включи мои лайки", false, "Likes"),
            ("включи мои лайки", true, "ToAlice"),
            ("включи альбом discovery", false, "Album"),
            ("включи плейлист для бега", false, "Playlist"),
            ("включи бонобо", false, "Play"),
            ("включи бонобо", true, "ToAlice"),
            ("включи бонобо в саундклауде", true, "Play"),
            ("включи свет", false, "Alice"),
            ("включи свет", true, "Alice"),
            ("какая погода", false, "Alice"),
        ];
        let mut bad = Vec::new();
        for (phrase, keyword_only, want) in rows {
            let got = kind(super::intent(phrase, *keyword_only));
            if got != *want {
                bad.push(format!("{phrase:?} (keyword_only={keyword_only}): {got}, want {want}"));
            }
        }
        assert!(bad.is_empty(), "{}", bad.join("\n"));
    }

    #[test]
    fn what_starts_alices_music() {
        assert!(starts_music("включи радио"));
        assert!(starts_music("Алиса, поставь сказку"));
        assert!(starts_music("включи песни в яндекс музыке"));
        assert!(!starts_music("включи свет на кухне"));
        assert!(!starts_music("поставь будильник на 7"));
        assert_eq!(alarm_trace(b"\x08\x01AliceCapability\x12\x05xx\x1aAlarmRinging\x00"), Some("AliceCapability | AlarmRinging".into()));
        assert_eq!(alarm_trace(b"\x08\x01AliceCapability state"), None);
        assert!(!starts_music("какая погода"));
    }

    use super::*;

    /// The TV switch (set_tv) is shared: the tests that read it take turns.
    static TV_TESTS: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn seek_commands() {
        let s = |t: &str| intent(t, false);
        assert_eq!(s("Алиса, перемотай на 30 секунд"), Some(Intent::Seek(30_000)));
        assert_eq!(s("перемотай на минуту"), Some(Intent::Seek(60_000)));
        assert_eq!(s("перемотай вперед на 2 минуты"), Some(Intent::Seek(120_000)));
        assert_eq!(s("перемотай назад на 10 секунд"), Some(Intent::Seek(-10_000)));
        assert_eq!(s("отмотай на полминуты"), Some(Intent::Seek(-30_000)));
        assert_eq!(s("промотай на минуту 30 секунд"), Some(Intent::Seek(90_000)));
        assert_eq!(s("перемотай на двадцать пять секунд"), Some(Intent::Seek(25_000)));
        assert_eq!(s("перемотай на полторы минуты"), Some(Intent::Seek(90_000)));
        assert_eq!(s("перемотай вперед"), Some(Intent::Seek(30_000)));
        assert_eq!(s("перемотай на 45"), Some(Intent::Seek(45_000)));
    }

    #[test]
    fn messages_to_contacts() {
        let names: Vec<String> = ["лиза", "лизе", "кошка", "мама", "маме"].iter().map(|s| s.to_string()).collect();
        let to = |t: &str| message_to_contact(t, &names);
        let cmd = |t: &str| Some(Intent::Pc(PcAction::Command(t.into())));
        assert_eq!(to("Алиса, напиши Лизе я скоро буду"), cmd("напиши лизе я скоро буду"));
        assert_eq!(to("напиши контакту кошка привет как дела"), cmd("напиши контакту кошка привет как дела"));
        assert_eq!(to("отправь сообщение маме в телеграм я дома"), cmd("отправь сообщение маме в телеграм я дома"));
        assert_eq!(to("напиши пете привет"), None);
        assert_eq!(to("напиши стих про осень"), None);
        assert_eq!(message_to_contact("напиши лизе привет", &[]), None);
    }

    #[test]
    fn smart_home_and_the_pc_menu_leave_phrases_to_alice() {
        // «Закрой окно» is Home Assistant's window, not Windows' («закрываются не физическое»).
        for t in ["закрой окно", "открой окно в спальне", "открой шторы", "закрой жалюзи", "открой ворота", "выключи свет"] {
            assert_eq!(pc_named(t), None, "{t}");
        }
        assert_eq!(pc_named("закрой окно на компьютере"), Some((PcAction::Command("закрой окно".into()), true)));
        assert_eq!(pc_named("закрой окно браузера"), Some((PcAction::Command("закрой окно браузера".into()), false)));
        assert_eq!(pc_named("открой стим"), Some((PcAction::Command("открой стим".into()), false)));

        let all = PcRules::default();
        let off = |f: fn(&mut PcRules)| {
            let mut r = PcRules::default();
            f(&mut r);
            r
        };
        let ok = |t: &str, r: &PcRules| pc_named(t).is_some_and(|(a, named)| pc_allowed(t, &a, named, r));
        // Everything as before by default.
        for t in ["выключи компьютер", "открой стим", "загугли курс доллара", "отправь маме в телеграм привет"] {
            assert!(ok(t, &all), "{t}");
        }
        // Power by Home Assistant: «выключи компьютер» stays Alice's, apps are still ours.
        let r = off(|r| r.power = false);
        assert!(!ok("выключи компьютер", &r) && !ok("включи комп", &r) && ok("открой стим", &r));
        let r = off(|r| r.apps = false);
        assert!(!ok("открой стим", &r) && !ok("закрой хром на компе", &r) && ok("выключи компьютер", &r));
        let r = off(|r| r.tg = false);
        assert!(!ok("отправь маме в телеграм привет", &r) && ok("открой телеграм", &r));
        let r = off(|r| r.search = false);
        assert!(!ok("загугли курс доллара", &r) && !ok("запиши в заметки купить хлеб", &r) && ok("открой стим", &r));
        // Only with the PC named.
        let r = off(|r| r.plain = false);
        assert!(!ok("открой стим", &r) && ok("открой стим на компьютере", &r) && ok("выключи компьютер", &r));
        // The user's own list.
        let r = PcRules { keep: keep_list("Окно, выключи компьютер; ТЕРМОСТАТ"), ..PcRules::default() };
        assert_eq!(r.keep, ["окно", "выключи компьютер", "термостат"]);
        assert!(!ok("выключи компьютер", &r) && ok("усыпи компьютер", &r) && !ok("закрой окно на компьютере", &r));
        assert!(!ok("открой термостат", &r) && ok("открой стим", &r));
    }

    #[test]
    fn pc_commands() {
        let _one_at_a_time = TV_TESTS.lock().unwrap_or_else(|e| e.into_inner());
        set_tv(false);
        let pc = |t: &str| intent(t, false);
        let cmd = |t: &str| Some(Intent::Pc(PcAction::Command(t.into())));
        assert_eq!(pc("Алиса, включи компьютер"), Some(Intent::Pc(PcAction::Wake)));
        assert_eq!(pc("какая температура компьютера"), Some(Intent::Pc(PcAction::Metrics)));
        assert_eq!(pc("какая загрузка процессора"), Some(Intent::Pc(PcAction::Metrics)));
        assert_eq!(pc("запущен ли стим на компьютере"), Some(Intent::Pc(PcAction::Process("стим".into()))));
        assert_eq!(pc("какая температура на улице"), None);
        assert_eq!(pc("включи мой комп"), Some(Intent::Pc(PcAction::Wake)));
        assert_eq!(pc("Выключи компьютер."), Some(Intent::Pc(PcAction::Shutdown)));
        assert_eq!(pc("усыпи компьютер"), Some(Intent::Pc(PcAction::Sleep)));
        assert_eq!(pc("переведи компьютер в спящий режим"), Some(Intent::Pc(PcAction::Sleep)));
        assert_eq!(pc("перезагрузи пк"), Some(Intent::Pc(PcAction::Restart)));
        assert_eq!(pc("заблокируй компьютер"), Some(Intent::Pc(PcAction::Lock)));
        assert_eq!(pc("открой телеграм на компьютере"), cmd("открой телеграм"));
        assert_eq!(pc("запусти на компе стим"), cmd("запусти стим"));
        assert_eq!(pc("закрой хром на компе"), cmd("закрой хром"));
        assert_eq!(pc("выключи дискорд на компьютере"), cmd("выключи дискорд"));
        // Without «на компьютере».
        assert_eq!(pc("Открой steam"), cmd("открой steam"));
        assert_eq!(pc("открой мне в браузере профиль работа"), cmd("открой мне в браузере профиль работа"));
        assert_eq!(pc("открой ютуб в профиле работа"), cmd("открой ютуб в профиле работа"));
        assert_eq!(pc("найди в гугле как приготовить плов"), cmd("найди в гугле как приготовить плов"));
        assert_eq!(pc("загугли курс доллара"), cmd("загугли курс доллара"));
        assert_eq!(pc("найди на ютубе как установить виндоус"), cmd("найди на ютубе как установить виндоус"));
        assert_eq!(pc("допиши в блокнот купить хлеб"), cmd("допиши в блокнот купить хлеб"));
        assert_eq!(pc("напиши сообщение в телеграм маме привет"), cmd("напиши сообщение в телеграм маме привет"));
        assert_eq!(pc("отправь саше в телеграм я опоздаю"), cmd("отправь саше в телеграм я опоздаю"));
        assert_eq!(pc("подключи впн"), cmd("подключи впн"));
        assert_eq!(pc("включи впн"), cmd("включи впн"));
        assert_eq!(pc("отключи впн"), cmd("отключи впн"));
        assert_eq!(pc("открой блокнот и запиши купить молоко"), cmd("открой блокнот и запиши купить молоко"));
        assert_eq!(pc("запиши в заметки позвонить маме"), cmd("запиши в заметки позвонить маме"));
        assert_eq!(pc("создай документ и напиши привет"), cmd("создай документ и напиши привет"));
        assert_eq!(pc("создай папку проекты на рабочем столе"), cmd("создай папку проекты на рабочем столе"));
        // Not about the PC: music and Alice's own commands stay as before.
        assert_eq!(pc("включи компромат"), Some(Intent::Play("компромат".into())));
        assert_ne!(pc("включи свет"), Some(Intent::Pc(PcAction::Wake)));
        assert_eq!(super::pc("какая погода на компьютере"), None);
        assert_eq!(super::pc("найди рецепт плова"), None);
        assert_eq!(super::pc("сделай громче"), None);
        assert_eq!(super::pc("открой навык угадай мелодию"), None);
        assert_eq!(super::pc("открой"), None);
    }

    #[test]
    fn the_projector_is_followed_without_naming_it() {
        let t = |s: &str| match intent(s, false) {
            Some(Intent::Tv(a)) => Some(a),
            _ => None,
        };
        let _one_at_a_time = TV_TESTS.lock().unwrap_or_else(|e| e.into_inner());
        tv_state(|s| s.session = None);
        set_tv(true);
        // The owner's words: no «на проекторе», no command before.
        assert_eq!(t("Алиса, нажми вверх"), Some(TvAction::Key(UP, 1)));
        assert_eq!(t("наведись ниже"), Some(TvAction::Key(DOWN, 1)));
        assert_eq!(t("Алиса, подключись к впн"), Some(TvAction::OpenAndStart("впн".into())));
        assert_eq!(t("отключи впн"), Some(TvAction::OpenAndStart("впн".into())));
        assert_eq!(t("еще"), None, "a bare «ещё» only right after a TV command");
        // «Режим пульта»: everything short is the TV's until it is switched off.
        assert_eq!(t("Алиса, режим пульта"), Some(TvAction::RemoteMode(true)));
        // The owner's words: «Алиса, пульт» and the like (before, most got «Я не нашла»).
        for p in ["Алиса, пульт", "пульт ду", "включи пульт ду", "открой пульт", "пульт телевизора", "пульт от телевизора",
                  "дай пульт", "покажи пульт", "режим пульт ду", "пульт пожалуйста", "открой пульт ду", "переключись на пульт",
                  "включи пульт телевизора", "пульт дистанционного управления"] {
            assert_eq!(t(p), Some(TvAction::RemoteMode(true)), "{p}");
        }
        for p in ["выключи пульт", "закрой пульт", "выйди из пульта", "хватит пульт"] {
            assert_eq!(t(p), Some(TvAction::RemoteMode(false)), "{p}");
        }
        assert!(!matches!(t("включи песню пульт управления любовью"), Some(TvAction::RemoteMode(_))), "a song title stays music");
        tv_remote(true);
        assert_eq!(t("вправо"), Some(TvAction::Key(RIGHT, 1)));
        assert_eq!(t("ок"), Some(TvAction::Key(CENTER, 1)));
        assert_eq!(t("пауза"), Some(TvAction::Key("KEYCODE_MEDIA_PAUSE", 1)), "the TV's pause in the remote mode");
        assert_eq!(t("громче"), Some(TvAction::Key("KEYCODE_VOLUME_UP", 3)));
        assert_eq!(t("назад"), Some(TvAction::Key("KEYCODE_BACK", 1)));
        // Music, timers and plans are not «open an app» on the TV even in the remote mode.
        for p in ["включи bonobo", "поставь таймер на 5 минут", "запусти помодоро", "включи музыку"] {
            assert!(!matches!(t(p), Some(TvAction::Open(_) | TvAction::YouTube(_))), "{p}");
        }
        assert!(matches!(intent("запусти помодоро", false), Some(Intent::Timer(_))), "the pomodoro is soundya's");
        assert_eq!(t("открой ютуб"), Some(TvAction::Open("ютуб".into())), "«открой» still opens on the TV");
        assert_eq!(t("выключи режим пульта"), Some(TvAction::RemoteMode(false)));
        tv_remote(false);
        assert_eq!(t("пауза"), None, "after it, music words are music's again");
        tv_used(Some(UP));
        assert_eq!(t("еще выше"), Some(TvAction::Key(UP, 1)));
        assert_eq!(t("Алиса, ещё"), Some(TvAction::Key(UP, 1)), "«ещё» repeats the last key");
        assert_eq!(t("ниже"), Some(TvAction::Key(DOWN, 1)));
        assert_eq!(t("выбери"), Some(TvAction::Key(CENTER, 1)));
        assert_eq!(t("воспроизведи последнее видео из истории"), Some(TvAction::PlayFirst));
        assert_eq!(t("пауза"), None, "music words stay music's");
        tv_used(None);
        assert_eq!(t("еще"), Some(TvAction::Key(UP, 1)), "a tap keeps the last key");
        tv_state(|s| s.session = Some((std::time::Instant::now() - TV_SESSION_FOR, Some(UP))));
        assert_eq!(t("еще"), None, "a minute and a half later: «ещё» is not the TV's any more");
        assert_eq!(t("выбери"), None);
        tv_state(|s| s.session = None);
        set_tv(false);
    }

    #[test]
    fn tv_commands() {
        let t = |s: &str| match intent(s, false) {
            Some(Intent::Tv(a)) => Some(a),
            _ => None,
        };
        assert_eq!(t("Алиса, включи проектор"), Some(TvAction::Wake));
        assert_eq!(t("Сделай проектор тише на 10"), Some(TvAction::Key("KEYCODE_VOLUME_DOWN", 10)));
        assert_eq!(t("громче на пять на телевизоре"), Some(TvAction::Key("KEYCODE_VOLUME_UP", 5)));
        assert_eq!(t("убавь звук на проекторе"), Some(TvAction::Key("KEYCODE_VOLUME_DOWN", 3)));
        assert_eq!(intent("сделай проектор синим", false), Some(Intent::Tv(TvAction::Unclear)), "about the projector: never Alice's");
        assert_eq!(intent("какой телевизор лучше купить", false), None, "a question about TVs is hers");
        assert_eq!(intent("телевизор", false), None);
        assert!(macro_matches(&normalize_phrase("Выполни буду смотреть youtube"), &normalize_phrase("буду смотреть ютуб")));
        assert!(macro_matches(&normalize_phrase("Алиса, каточку в Deadlock"), &normalize_phrase("каточку в дедлок")));
        assert!(macro_matches(&normalize_phrase("запусти рабочий режим"), &normalize_phrase("запусти рабочий режим")));
        assert!(!macro_matches(&normalize_phrase("буду смотреть ютуб завтра"), &normalize_phrase("буду смотреть ютуб")), "still the whole phrase");
        // Inside apps, like the remote.
        assert_eq!(t("Алиса, наведись выше на проекторе"), Some(TvAction::Key(UP, 1)));
        assert_eq!(t("ниже на три на проекторе"), Some(TvAction::Key(DOWN, 3)));
        assert_eq!(t("нажми ок на проекторе"), Some(TvAction::Key(CENTER, 1)));
        assert_eq!(t("нажми историю на проекторе"), Some(TvAction::Tap("историю".into())));
        assert_eq!(t("нажми на кнопку подписаться на телевизоре"), Some(TvAction::Tap("подписаться".into())));
        assert_eq!(t("открой раздел подписки на проекторе"), Some(TvAction::Tap("подписки".into())));
        // The owner's phrases, without naming the projector.
        assert_eq!(t("Алиса, открой историю просмотров в приложении смарт тьюб"),
            Some(TvAction::OpenIn("смарт тьюб".into(), "историю просмотров".into())));
        assert_eq!(t("открой впн и включи его"), Some(TvAction::OpenAndStart("впн".into())));
        assert_eq!(t("открой ютуб на проекторе"), Some(TvAction::Open("ютуб".into())), "a plain app as before");
        assert_eq!(t("выключи телевизор"), Some(TvAction::Sleep));
        assert_eq!(t("открой ютуб на проекторе"), Some(TvAction::Open("ютуб".into())));
        assert_eq!(t("запусти кинопоиск на телевизоре"), Some(TvAction::Open("кинопоиск".into())));
        assert_eq!(t("найди фильм дюна на проекторе"), Some(TvAction::Search("дюна".into())));
        assert_eq!(t("найди маму в 16 на рутубе на телевизоре"), Some(TvAction::SearchIn("рутубе".into(), "маму в 16".into())));
        assert_eq!(t("найди фильм дюна в кинопоиске на проекторе"), Some(TvAction::SearchIn("кинопоиске".into(), "дюна".into())));
        assert_eq!(t("включи на ютубе котиков на проекторе"), Some(TvAction::YouTube("котиков".into())));
        assert_eq!(t("найди на ютубе как приготовить плов на телевизоре"), Some(TvAction::YouTube("как приготовить плов".into())));
        assert_eq!(t("пауза на проекторе"), Some(TvAction::Key("KEYCODE_MEDIA_PAUSE", 1)));
        assert_eq!(t("сделай громче на телевизоре"), Some(TvAction::Key("KEYCODE_VOLUME_UP", 3)));
        assert_eq!(t("перемотай вперед на проекторе"), Some(TvAction::Key("KEYCODE_MEDIA_FAST_FORWARD", 1)));
        assert_eq!(t("включи сериал друзья на проекторе"), Some(TvAction::Search("друзья".into())));
        // Without the TV named: not a TV command.
        assert_eq!(t("включи bonobo"), None);
        assert_eq!(t("открой ютуб"), None);
    }

    #[test]
    fn music_and_home_commands() {
        assert_eq!(
            intent("Включи radiohead", false),
            Some(Intent::Play("radiohead".into()))
        );
        assert_eq!(
            intent("Алиса, включи песню Всё идёт по плану", false),
            Some(Intent::Play("все идет по плану".into()))
        );
        assert_eq!(
            intent("включи bonobo в саундклауде", true),
            Some(Intent::Play("bonobo".into()))
        );
        // Keyword mode: Alice's music (SoundCloud steps aside), but «включи свет» stays plainly hers.
        assert_eq!(intent("включи bonobo", true), Some(Intent::ToAlice));
        assert_eq!(intent("включи свет", true), None);
        // «… в яндекс музыке» goes to Alice in either mode.
        for p in ["включи bonobo в яндекс музыке", "поставь мою волну в яндексе", "включи радиохед из яндекса"] {
            assert_eq!(intent(p, false), Some(Intent::ToAlice), "{p}");
        }
        assert_eq!(intent("какая погода в яндексе", false), None);
        // Albums.
        assert_eq!(intent("включи альбом OK Computer", false), Some(Intent::Album("ok computer".into())));
        assert_eq!(intent("включи альбом in rainbows в саундклауде", true), Some(Intent::Album("in rainbows".into())));
        assert_eq!(intent("включи мой альбом лето", false), Some(Intent::MyPlaylist("лето".into())));
        assert_eq!(
            intent("включи плейлист lofi beats", false),
            Some(Intent::Playlist("lofi beats".into()))
        );
        assert_eq!(intent("Алиса, включи мои лайки", false), Some(Intent::Likes));
        assert_eq!(intent("включи мой плейлист в дорогу", false), Some(Intent::MyPlaylist("в дорогу".into())));
        assert_eq!(intent("Останови музыку", false), Some(Intent::Pause));
        for (s, want) in [
            ("перемешай", Intent::Shuffle(true)),
            ("перемешай музыку", Intent::Shuffle(true)),
            ("включи случайный порядок", Intent::Shuffle(true)),
            ("алиса, решафл", Intent::Shuffle(true)),
            ("играй по порядку", Intent::Shuffle(false)),
            ("включи мои лайки вперемешку", Intent::ToAlice),
            ("перемешай мои лайки", Intent::ToAlice),
            ("поставь на повтор", Intent::Repeat(true)),
            ("включи повтор", Intent::Repeat(true)),
            ("выключи повтор", Intent::Repeat(false)),
            ("включи заново", Intent::Restart),
            ("включи похожее", Intent::Similar),
            ("включи что-то похожее", Intent::Similar),
            ("лайкни", Intent::Like),
            ("мне нравится этот трек", Intent::Like),
            ("мне не нравится", Intent::Next),
            ("что сейчас играет", Intent::WhatPlaying),
            ("убери лайк", Intent::Unlike),
            ("удали из лайков", Intent::Unlike),
            ("добавь в плейлист в дорогу", Intent::AddToPlaylist("в дорогу".into())),
            ("алиса, добавь эту песню в плейлист чилл осень", Intent::AddToPlaylist("чилл осень".into())),
            ("как называется эта песня", Intent::WhatPlaying),
        ] {
            assert_eq!(intent(s, true), Some(want), "{s}");
        }
        assert_eq!(intent("включи мои лайки вперемешку", false), Some(Intent::Shuffled(Box::new(Intent::Likes))));
        assert_eq!(intent("перемешай мои лайки", false), Some(Intent::Shuffled(Box::new(Intent::Likes))));
        // Yandex Music by default: likes and own playlists are hers unless SoundCloud is named.
        assert_eq!(intent("включи мои лайки", true), Some(Intent::ToAlice));
        assert_eq!(intent("включи мои лайки в саундклауде", true), Some(Intent::Likes));
        assert_eq!(intent("включи мой плейлист в дорогу", true), Some(Intent::ToAlice));
        assert_eq!(intent("включи мой плейлист в дорогу на саундклауде", true), Some(Intent::MyPlaylist("в дорогу".into())));
        assert_eq!(intent("включи мои лайки", false), Some(Intent::Likes));
        assert_eq!(intent("включи рекомендации в саундклауде", true), Some(Intent::Recommend));
        assert_eq!(intent("включи мою волну", false), Some(Intent::Recommend));
        assert_eq!(intent("включи мою волну", true), None);
        for s in ["Алиса, включи саундклауд", "включи музыку в саундклауде", "запусти soundcloud", "саундклауд"] {
            assert_eq!(intent(s, true), Some(Intent::Default), "{s}");
            assert_eq!(intent(s, false), Some(Intent::Default), "{s}");
        }
        assert_eq!(
            intent("включи bonobo в случайном порядке", false),
            Some(Intent::Shuffled(Box::new(Intent::Play("bonobo".into()))))
        );
        assert_eq!(
            intent("включи bonobo вперемешку в саундклауде", true),
            Some(Intent::Shuffled(Box::new(Intent::Play("bonobo".into()))))
        );
        for (s, want) in [
            ("продолжи воспроизведение", Intent::Resume),
            ("Алиса, продолжай играть", Intent::Resume),
            ("сними с паузы", Intent::Resume),
            ("поставь музыку на паузу", Intent::Pause),
            ("выключи воспроизведение", Intent::Pause),
            ("включи следующую песню", Intent::Next),
            ("пропусти трек", Intent::Next),
            ("переключи", Intent::Next),
            ("включи предыдущий трек", Intent::Previous),
        ] {
            assert_eq!(intent(s, true), Some(want), "{s}");
        }
        for s in ["что сейчас играет", "почему музыка на паузе", "выключи свет", "переключи канал на телевизоре"] {
            assert_eq!(transport(&normalize(s)), None, "{s}");
        }
        assert_eq!(intent("следующий трек", false), Some(Intent::Next));
        for s in ["включи свет", "поставь таймер на пять минут", "включи блютуз", "какая погода"] {
            assert_eq!(intent(s, false), None, "{s}");
        }
    }

    #[test]
    fn yandex_music_requests_stay_hers() {
        for p in ["включи плейлист дня", "поставь дежавю", "включи подкаст про космос", "включи аудиокнигу",
                  "продолжи в яндекс музыке", "включи следующий трек в яндекс музыке"] {
            assert_eq!(intent(p, false), Some(Intent::ToAlice), "{p}");
        }
        // Named SoundCloud wins.
        assert_eq!(intent("включи подкаст в саундклауде", false), Some(Intent::Play("подкаст".into())));
        assert_eq!(intent("какая погода в яндексе", false), None);
    }

    #[test]
    fn who_owns_the_phrase() {
        let idle = Who::default();
        let yandex = Who { keyword_only: true, ..idle };
        // After a restart, nothing of anybody's: «продолжи» starts SoundCloud…
        assert!(owns(&Intent::Resume, &idle));
        // …but not when Yandex Music is the default service.
        assert!(!owns(&Intent::Resume, &yandex));
        // A paused SoundCloud track with Alice's music started after it.
        let under_alice = Who { active: true, has_queue: true, alice_music: true, ..idle };
        for i in [Intent::Seek(30_000), Intent::Shuffle(true), Intent::Repeat(true), Intent::Restart, Intent::Similar, Intent::Pause] {
            assert!(!owns(&i, &under_alice), "{i:?} belongs to Alice's music");
        }
        assert!(!owns(&Intent::Resume, &under_alice));
        // SoundCloud paused, nothing else since: it is ours.
        let paused = Who { active: true, has_queue: true, ..idle };
        for i in [Intent::Seek(30_000), Intent::Shuffle(true), Intent::Resume, Intent::Restart] {
            assert!(owns(&i, &paused), "{i:?}");
        }
        assert!(!owns(&Intent::Pause, &paused), "nothing to pause");
        let playing = Who { playing: true, active: true, has_queue: true, ..yandex };
        assert!(owns(&Intent::Next, &playing) && owns(&Intent::Pause, &playing));
        assert!(!owns(&Intent::Next, &yandex));
        assert!(!owns(&Intent::Pc(PcAction::Wake), &idle));
        assert!(owns(&Intent::Duck(DuckCmd::Down), &playing));
        assert!(!owns(&Intent::Duck(DuckCmd::Down), &under_alice));
        assert!(owns(&Intent::Duck(DuckCmd::Auto(false)), &idle));
        assert_eq!(source(&under_alice), "alice");
        assert_eq!(source(&playing), "soundcloud");
        assert_eq!(source(&Who { airplay: true, ..playing }), "airplay");
    }

    #[test]
    fn duck_phrases() {
        assert_eq!(intent("Алиса, приглуши звук", false), Some(Intent::Duck(DuckCmd::Down)));
        assert_eq!(intent("верни громкость", true), Some(Intent::Duck(DuckCmd::Restore)));
        assert_eq!(intent("отключи автоматическое приглушение", false), Some(Intent::Duck(DuckCmd::Auto(false))));
        assert_eq!(intent("включи автоматическое приглушение", false), Some(Intent::Duck(DuckCmd::Auto(true))));
        assert_eq!(intent("Алиса, повтори адрес", false), Some(Intent::Address));
        assert_eq!(intent("Алиса, говори тише", true), Some(Intent::VoiceLevel(-15)));
        assert_eq!(intent("сделай голос громче", false), Some(Intent::VoiceLevel(15)));
        assert_eq!(intent("какой адрес саундя", true), Some(Intent::Address));
        // Her own volume stays hers.
        assert_eq!(intent("сделай потише", false), None);
    }

    #[test]
    fn service_name_is_removed() {
        assert_eq!(strip_service("включи daft punk на soundcloud"), ("включи daft punk".into(), true));
        assert_eq!(strip_service("включи daft punk"), ("включи daft punk".into(), false));
    }

    #[test]
    fn parses_final_phrase() {
        let id = b"12345678-1234-1234-1234-123456789012";
        let text = "Включи radiohead".as_bytes();
        let mut asr = Vec::new();
        put_bytes(&mut asr, 1, text);
        put_number(&mut asr, 2, 1);
        put_number(&mut asr, 4, 1);
        let mut state = Vec::new();
        put_number(&mut state, 1, 3);
        put_bytes(&mut state, 2, text);
        put_bytes(&mut state, 7, id);
        put_bytes(&mut state, 15, &asr);
        let mut msg = Vec::new();
        put_bytes(&mut msg, 252, &state);
        put_number(&mut msg, 253, 3);
        let (req, got) = recognized(&msg).unwrap();
        assert_eq!(req.as_bytes(), id);
        assert_eq!(got, "Включи radiohead");
        assert_eq!(alice_busy(&msg), Some(true));
        for n in 0..msg.len() {
            assert_eq!(recognized(&msg[..n]), None);
        }
    }

    #[test]
    fn cancel_frame_matches_spotiya() {
        assert_eq!(
            &rpc("AliceCapability", 8, &[8, 3])[4..],
            &[
                0xe2, 0x0f, 0x15, 0x3a, 0x0f, b'A', b'l', b'i', b'c', b'e', b'C', b'a', b'p', b'a',
                b'b', b'i', b'l', b'i', b't', b'y', 0x42, 2, 8, 3, 0xe8, 0x0f, 0xd2, 1
            ]
        );
    }

    /// The device names are global: the tests that set them take turns.
    pub(super) static DEVICES_TEST: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn devices_by_their_names() {
        let _one = DEVICES_TEST.lock().unwrap_or_else(|e| e.into_inner());
        let d = vec![("спальня".to_owned(), true), ("кухня".to_owned(), true), ("ноутбук".to_owned(), false), ("большой комп".to_owned(), false)];
        let on = |t: &str| device_in(&normalize(t), &d).map(|(n, _, p)| (n, p));
        assert_eq!(on("выключи телевизор в спальне"), Some(("спальня".into(), "выключи телевизор".into())));
        assert_eq!(on("включи ютуб на кухне"), Some(("кухня".into(), "включи ютуб на телевизоре".into())));
        assert_eq!(on("усыпи ноутбук"), Some(("ноутбук".into(), "усыпи компьютер".into())));
        assert_eq!(on("пульт спальня"), Some(("спальня".into(), "пульт".into())));
        assert_eq!(on("выключи большой комп").map(|x| x.0), Some("большой комп".into()));
        assert_eq!(on("включи музыку"), None);
        // Whole intents: aimed at the device, or left alone when it is not a TV / PC phrase.
        set_devices(d.clone());
        let tv_off = Some(Intent::On("спальня".into(), Box::new(Intent::Tv(TvAction::Sleep))));
        assert_eq!(intent("Алиса, выключи телевизор в спальне", false), tv_off);
        assert_eq!(intent("усыпи ноутбук", false), Some(Intent::On("ноутбук".into(), Box::new(Intent::Pc(PcAction::Sleep)))));
        assert_eq!(intent("пульт спальня", false), Some(Intent::On("спальня".into(), Box::new(Intent::Tv(TvAction::RemoteMode(true))))));
        assert!(!matches!(intent("включи музыку на кухне", false), Some(Intent::On(..))));
        set_devices(Vec::new());
    }

    #[test]
    fn open_an_app_said_loosely() {
        let _one = DEVICES_TEST.lock().unwrap_or_else(|e| e.into_inner());
        let d = vec![("кабинет".to_owned(), true)];
        set_devices(d);
        assert_eq!(intent("Алиса запусти приложение лифт на в кабинете на смарт тв", false), Some(Intent::On("кабинет".into(), Box::new(Intent::Tv(TvAction::Open("лифт".into()))))));
        set_devices(Vec::new());
    }
}


#[cfg(test)]
mod routing_tests {
    use super::*;

    fn choice(label: &str, words: &[&str], cmd: &str) -> Choice {
        Choice { label: label.into(), words: words.iter().map(|w| w.to_string()).collect(), step: Step::Control(cmd.into()) }
    }

    #[test]
    fn bluetooth_by_voice() {
        let i = |t: &str| intent(t, false);
        // The saved device.
        for t in ["Алиса, переключись на блютус", "переключи звук на блютуз", "переключи музыку на блютуз", "включи блютуз колонку",
            "подключись к блютуз колонке", "переключись на наушники", "включи музыку в наушниках", "переключи блютуз"] {
            assert_eq!(i(t), Some(Intent::BtOut(true)), "{t}");
        }
        // A new one: look around and ask.
        for t in ["Алиса, подключись к новому устройству блютус", "найди блютуз устройства", "добавь блютуз колонку",
            "подключи новые наушники", "подключи новую колонку", "поищи bluetooth устройства"] {
            assert_eq!(i(t), Some(Intent::BtFind), "{t}");
        }
        // Back to the speaker.
        for t in ["верни звук на колонку", "вернись на колонку", "вернись на станцию", "возвращайся на колонку", "верни музыку на колонку", "отключи блютуз колонку", "переключи музыку на колонку", "выключи блютуз наушники"] {
            assert_eq!(i(t), Some(Intent::BtOut(false)), "{t}");
        }
        // The Station as a Bluetooth speaker for a phone is Alice's own.
        for t in ["включи блютуз", "выключи блютуз", "включи блютус"] {
            assert_eq!(i(t), None, "{t}");
        }
        assert_eq!(i("верни звук"), Some(Intent::Duck(DuckCmd::Restore)), "ducking keeps its phrase");
    }

    #[test]
    fn an_answer_is_the_whole_phrase() {
        let o = vec![
            choice("колонка", &["колонка", "колонке", "станция"], "volume down 3"),
            choice("телевизор", &["телевизор", "проектор", "проекторе", "тв"], "tv"),
            choice("мак", &["мак"], "pc"),
        ];
        let a = |t: &str| answer(&o, &normalize_phrase(t));
        assert_eq!(a("Алиса, на проекторе"), Answer::Pick(1));
        assert_eq!(a("проектор"), Answer::Pick(1));
        assert_eq!(a("на колонке"), Answer::Pick(0));
        assert_eq!(a("на мак"), Answer::Pick(2));
        assert_eq!(a("второе"), Answer::Pick(1));
        assert_eq!(a("3"), Answer::Pick(2));
        assert_eq!(a("последнее"), Answer::Pick(2));
        assert_eq!(a("четвертое"), Answer::No, "there are three");
        assert_eq!(a("ко второму"), Answer::Pick(1));
        assert_eq!(a("на маке"), Answer::Pick(2), "a short name in another case");
        assert_eq!(a("отмена"), Answer::Cancel);
        assert_eq!(a("не надо"), Answer::Cancel);
        for t in ["включи первое видео", "включи музыку", "какая погода", "поставь таймер на пять минут", "сделай тише на проекторе на пять"] {
            assert_eq!(a(t), Answer::No, "{t}");
        }
        // Bluetooth names: by number, or by the name's words.
        let bt = vec![choice("JBL Flip 5", &["jbl", "flip", "5"], "a"), choice("Sony WH", &["sony", "wh"], "b")];
        assert_eq!(answer(&bt, "первое"), Answer::Pick(0));
        assert_eq!(answer(&bt, "sony"), Answer::Pick(1));
        assert_eq!(answer(&bt, "никакое"), Answer::Cancel);
    }

    #[test]
    fn a_question_waits_and_is_taken_once() {
        put_question(vec![choice("телевизор", &["проекторе", "телевизор"], "tv key volup"), choice("колонка", &["колонке"], "volume up 1")], Duration::from_secs(30));
        assert_eq!(take_answer("какая погода"), None, "an unrelated phrase leaves the question");
        assert_eq!(take_answer("Алиса, на проекторе"), Some(Some(Step::Control("tv key volup".into()))));
        assert_eq!(take_answer("на проекторе"), None, "answered already");
        put_question(vec![choice("а", &["а"], "x")], Duration::from_secs(30));
        assert_eq!(take_answer("отмена"), Some(None));
        put_question(vec![choice("а", &["а"], "x")], Duration::ZERO);
        assert_eq!(take_answer("первое"), None, "too late");
    }

    #[test]
    fn a_named_tv_always_wins() {
        set_tv(true);
        let t = |s: &str| match intent(s, false) {
            Some(Intent::Tv(a)) => Some(a),
            _ => None,
        };
        assert_eq!(t("Алиса, сделай звук тише на проекторе"), Some(TvAction::Key("KEYCODE_VOLUME_DOWN", 3)));
        assert_eq!(t("перемотай на 30 секунд на проекторе"), Some(TvAction::Key("KEYCODE_MEDIA_FAST_FORWARD", 2)));
        assert_eq!(t("перемотай назад на минуту на проекторе"), Some(TvAction::Key("KEYCODE_MEDIA_REWIND", 4)));
        assert_eq!(t("продолжи видео на проекторе"), Some(TvAction::Key("KEYCODE_MEDIA_PLAY", 1)));
        assert_eq!(t("останови видео на проекторе"), Some(TvAction::Key("KEYCODE_MEDIA_PAUSE", 1)));
        assert_eq!(t("поставь видео на паузу на проекторе"), Some(TvAction::Key("KEYCODE_MEDIA_PAUSE", 1)));
        assert_eq!(t("следующее видео на проекторе"), Some(TvAction::Key("KEYCODE_MEDIA_NEXT", 1)));
        assert_eq!(t("закрой ютуб на проекторе"), Some(TvAction::Close("ютуб".into())));
        assert_eq!(t("сделай красиво на проекторе"), Some(TvAction::Unclear));
        // After its pause «продолжи» is the film's; after music is asked for — the music's.
        assert_eq!(intent("продолжи", false), Some(Intent::Resume));
        tv_used(Some("KEYCODE_MEDIA_PAUSE"));
        assert_eq!(t("продолжи"), Some(TvAction::Key("KEYCODE_MEDIA_PLAY", 1)));
        tv_forget_pause();
        assert_eq!(intent("продолжи", false), Some(Intent::Resume));
        // Not commands to the TV.
        assert_eq!(t("напомни в восемь посмотреть фильм на телевизоре"), None);
        assert_eq!(t("поставь таймер на десять минут на телевизоре"), None);
    }

    #[test]
    fn a_screen_only_phrase_is_the_tvs() {
        let t = |s: &str| intent(s, false);
        // No TV: as before (music or Alice).
        assert_ne!(t("включи первое видео"), Some(Intent::Tv(TvAction::PlayFirst)));
        set_tv(true);
        assert_eq!(t("Алиса, включи первое видео"), Some(Intent::Tv(TvAction::PlayFirst)));
        assert_eq!(t("следующая серия"), Some(Intent::Tv(TvAction::Key("KEYCODE_MEDIA_NEXT", 1))));
        assert_eq!(t("включи следующую серию"), Some(Intent::Tv(TvAction::Key("KEYCODE_MEDIA_NEXT", 1))));
        assert_eq!(t("предыдущая серия"), Some(Intent::Tv(TvAction::Key("KEYCODE_MEDIA_PREVIOUS", 1))));
        assert_eq!(t("найди фильм дюна"), Some(Intent::Tv(TvAction::Search("дюна".into()))));
        assert_eq!(t("включи сериал друзья"), Some(Intent::Tv(TvAction::Search("друзья".into()))));
        // Found by the review: the band Кино, a pause, a question and the computer keep their phrases.
        assert!(matches!(t("включи кино группа крови"), Some(Intent::Play(_))));
        assert!(matches!(t("включи аниме опенинги"), Some(Intent::Play(_))));
        assert_eq!(t("найди кино дюна"), Some(Intent::Tv(TvAction::Search("дюна".into()))));
        assert!(!matches!(t("поставь фильм на паузу"), Some(Intent::Tv(TvAction::Search(_)))));
        assert_eq!(t("поставь фильм на паузу на проекторе"), Some(Intent::Tv(TvAction::Key("KEYCODE_MEDIA_PAUSE", 1))));
        assert_eq!(t("поставь аниме на паузу на проекторе"), Some(Intent::Tv(TvAction::Key("KEYCODE_MEDIA_PAUSE", 1))));
        assert!(!matches!(t("какой следующий выпуск новостей"), Some(Intent::Tv(_))));
        assert!(!matches!(t("найди фильм дюна в гугле"), Some(Intent::Tv(_))));
        assert!(!matches!(t("включи премьеру"), Some(Intent::Tv(_) | Intent::Ask(_))));
        // Bluetooth words inside other phrases are not a search for devices.
        assert_ne!(t("найди блютуз наушники в интернете"), Some(Intent::BtFind));
        assert_ne!(t("добавь в список покупок блютуз колонку"), Some(Intent::BtFind));
        assert_eq!(t("включи блютуз на колонке"), None, "the Station's own Bluetooth is Alice's");
        // Music stays music.
        assert_eq!(t("следующий трек"), Some(Intent::Next));
        assert!(matches!(t("включи музыку из фильма интерстеллар"), Some(Intent::Play(_))));
        assert!(matches!(t("включи bonobo"), Some(Intent::Play(_))));
    }

    #[test]
    fn volume_and_apps_go_where_the_settings_say() {
        let i = |t: &str| intent(t, false);
        // The speaker's own volume is Alice's unless told otherwise.
        set_tv(true);
        assert_eq!(i("сделай тише"), None);
        assert_eq!(i("громче на пять"), None);
        set_routes(Routes { volume: Route::Tv, apps: Route::Auto, pc: false });
        assert_eq!(i("сделай тише"), Some(Intent::Tv(TvAction::Key("KEYCODE_VOLUME_DOWN", 3))));
        set_routes(Routes { volume: Route::Ask, apps: Route::Auto, pc: false });
        assert_eq!(i("громче на пять"), Some(Intent::Ask(Ambig::Volume(true, Some(5)))));
        assert_eq!(i("сделай потише"), Some(Intent::Ask(Ambig::Volume(false, None))));
        assert_eq!(i("говори тише"), Some(Intent::VoiceLevel(-15)), "her voice keeps its phrase");
        // Apps: the only screen, or a question.
        set_routes(Routes { volume: Route::Auto, apps: Route::Auto, pc: false });
        assert_eq!(i("открой ютуб"), Some(Intent::Tv(TvAction::Open("ютуб".into()))));
        assert_eq!(i("включи кинопоиск"), Some(Intent::Tv(TvAction::Open("кинопоиск".into()))));
        assert_eq!(i("закрой ютуб"), Some(Intent::Tv(TvAction::Close("ютуб".into()))));
        set_routes(Routes { volume: Route::Auto, apps: Route::Auto, pc: true });
        assert_eq!(i("открой ютуб"), Some(Intent::Ask(Ambig::App(true, "ютуб".into(), "открой ютуб".into()))));
        set_routes(Routes { volume: Route::Auto, apps: Route::Pc, pc: true });
        assert_eq!(i("включи ютуб"), Some(Intent::Pc(PcAction::Command("открой ютуб".into()))));
        set_routes(Routes { volume: Route::Auto, apps: Route::Tv, pc: true });
        assert_eq!(i("включи ютуб"), Some(Intent::Tv(TvAction::Open("ютуб".into()))));
        // A named device still wins, and programs that are not video apps are the computer's as before.
        assert_eq!(i("открой ютуб на компьютере"), Some(Intent::Pc(PcAction::Command("открой ютуб".into()))));
        assert_eq!(i("открой стим"), Some(Intent::Pc(PcAction::Command("открой стим".into()))));
        assert!(!matches!(i("включи яндекс музыку"), Some(Intent::Tv(_) | Intent::Ask(_))));
        // No TV and no computer: nothing changes.
        set_tv(false);
        set_routes(Routes::default());
        assert!(!matches!(i("открой ютуб"), Some(Intent::Tv(_) | Intent::Ask(_))));
    }

    #[test]
    fn a_room_named_like_a_tv_keeps_its_lamp() {
        let _g = super::tests::DEVICES_TEST.lock().unwrap_or_else(|e| e.into_inner());
        set_tv(true);
        set_devices(vec![("спальня".into(), true)]);
        assert_eq!(intent("включи свет в спальне", false), None, "the lamp is Alice's");
        assert!(matches!(intent("включи ютуб в спальне", false), Some(Intent::On(n, _)) if n == "спальня"));
        set_devices(Vec::new());
    }
}

#[cfg(test)]
mod chain_tests {
    use super::*;

    #[test]
    fn everyday_wordings_become_plain_commands() {
        for (said, plain) in [
            ("Алиса, вруби проектор", "включи проектор"),
            ("слушай а ну-ка выруби телек", "выключи телек"),
            ("можешь открыть ютуб на телевизоре", "открой ютуб на телевизоре"),
            ("давай запускай рутуб на проекторе", "запусти рутуб на проекторе"),
            ("сначала включи проектор", "включи проектор"),
            ("отыщи фильм дюна на проекторе", "найди фильм дюна на проекторе"),
            // Not commands: as they were.
            ("а что идёт по тв", "а что идет по тв"),
            ("да", "да"),
            ("ответь да, приду", "ответь да, приду"),
        ] {
            assert_eq!(plain_command(said), plain, "{said}");
        }
        assert_eq!(intent("вруби проектор", false), Some(Intent::Tv(TvAction::Wake)));
        assert_eq!(intent("давай выруби телевизор", false), Some(Intent::Tv(TvAction::Sleep)));
        assert_eq!(intent("можешь открыть ютуб на проекторе", false), Some(Intent::Tv(TvAction::Open("ютуб".into()))));
    }

    #[test]
    fn several_commands_in_one_phrase() {
        // The owner's own phrase, as the recogniser writes it (no commas) and with them.
        for said in [
            "включи проектор открой впн и потом найди на рутубе фильм дюна",
            "Алиса, включи проектор, открой впн, потом найди на рутубе фильм дюна",
            "вруби проектор затем открой впн и найди на рутубе фильм дюна",
        ] {
            let c = chain(said, false).unwrap_or_else(|| panic!("not a chain: {said}"));
            assert_eq!(c.len(), 3, "{said}: {c:?}");
            assert_eq!(c[0], Intent::Tv(TvAction::Wake), "{said}");
            assert!(matches!(&c[1], Intent::Tv(TvAction::Open(a)) if a == "впн"), "{said}: {:?}", c[1]);
            assert!(matches!(&c[2], Intent::Tv(TvAction::SearchIn(app, q)) if app.contains("рутуб") && q.contains("дюна")), "{said}: {:?}", c[2]);
        }
        let c = chain("поставь на паузу на проекторе и выключи проектор", false).unwrap();
        assert_eq!(c, [Intent::Tv(TvAction::Key("KEYCODE_MEDIA_PAUSE", 1)), Intent::Tv(TvAction::Sleep)]);
        // One command, or not all of it commands: not a chain.
        for said in ["включи проектор", "включи битлз и роллинг стоунз", "найди фильм война и мир на проекторе", "ответь маме потом открой дверь",
            "включи проектор и расскажи анекдот"] {
            assert_eq!(chain(said, false), None, "{said}");
        }
    }
}
