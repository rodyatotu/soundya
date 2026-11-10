//! soundya: SoundCloud for Yandex Station Mini, controlled by Alice's voice.

mod adb;
mod adbkeep;
mod airplay;
mod alice;
mod alsa;
mod bt;
mod auth;
mod buffer;
mod buttons;
mod calendar;
mod bypass;
mod clock;
mod display;
mod dns;
mod ducking;
mod alarm;
mod morning;
#[cfg(feature = "dev")]
mod devpage;
mod eq;
mod fsx;
mod google;
mod hello;
mod history;
mod ipt;
mod led;
mod luna;
mod mdns;
mod pc;
mod planner;
mod probe;
mod player;
mod report;
mod sc;
mod settings;
mod sha256;
mod stats;
mod telegram;
mod timer;
mod tv;
mod update;
mod vis;
mod voice;
mod volume;
mod zapret;
mod web;
mod mods;

use std::{
    collections::HashSet,
    io::{BufRead, BufReader, Write},
    net::{TcpListener, TcpStream},
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, Sender},
        Arc, Mutex,
    },
    thread,
    time::Duration,
};

use alice::Intent;
use player::{Duck, Flags, Outcome};
use sc::{Client, Track};
use settings::Settings;

/// Likes and playlist edits. SoundCloud's bot protection (DataDome) answers
/// these writes with a captcha unless they come from a real browser, so they are
/// switched off; reading (search, likes list, playlists) is not affected.
const WRITES: bool = false;
const WRITES_OFF: &str = "Лайки и добавление в плейлисты недоступны: SoundCloud разрешает их только с сайта или из приложения";

// 9895 is taken by the Yandex firmware.
const CONTROL: &str = "127.0.0.1:47895";

/// The version everywhere (status, reports, /run/soundya.ready, `soundya version`).
/// The developer build (`--features dev`) is «0.5.0+dev»: install.sh checks the
/// running program against VERSION, and the page shows the build for what it is.
#[cfg(not(feature = "dev"))]
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
#[cfg(feature = "dev")]
pub const VERSION: &str = concat!(env!("CARGO_PKG_VERSION"), "+dev");

pub enum Cmd {
    Voice(Intent),
    /// AirPlay sound started (true) or stopped (false).
    AirPlay(bool),
    /// «Обход блокировок»: the strategy that got through ("" = switched off).
    Bypass(String),
    /// zapret arguments pasted from a .bat on the page.
    ZapretArgs(String),
    /// A strategy switched on by hand, without a check.
    BypassManual(String),
    Control(String, Sender<String>),
    Ended(u64, Outcome),
    /// A settings change from the page: only the fields it sends (merged into the current ones).
    Settings(serde_json::Value, Sender<String>),
    TestProxy(String, Sender<String>),
    /// Search results from a background search, kept for "pick".
    Results(Vec<Track>),
    /// Logged-in username (None = not logged in or token rejected).
    User(Option<String>),
    /// A background load (search, playlist, likes…) finished.
    Loaded {
        what: String,
        result: sc::Res<Vec<Track>>,
        reply: Option<Sender<String>>,
    },
    /// Covers found for history entries saved without one: (link, cover, track id).
    HistoryArt(Vec<(String, String, u64)>),
    /// Related tracks for the end of the queue; the number is the generation when asked.
    Related(u64, sc::Res<Vec<Track>>),
    /// Liked track ids after login.
    Liked(HashSet<u64>),
    /// A like was added (true) or removed (false).
    LikeChanged(u64, bool),
    /// Result of a connection check.
    Net(Result<u64, String>),
}

pub struct Shared {
    pub duck: Duck,
    /// Orange ring on (setting) and music actually playing (not paused).
    pub ring: AtomicBool,
    pub playing: AtomicBool,
    /// A SoundCloud request (search, likes, …) is running.
    pub fetching: AtomicBool,
    /// The current track's controls, so the ring can tell when sound is stuck loading.
    pub track: Mutex<Option<Arc<Flags>>>,
    keyword_only: AtomicBool,
    /// A Windows PC is set up for voice commands.
    pc: AtomicBool,
    /// Telegram contact names as said («лиза», «лизе», «кошка»), for «напиши лизе …».
    contacts: Mutex<Vec<String>>,
    /// Voice macros as (normalized phrase, script), for matching spoken lines.
    macros: Mutex<Vec<(String, String)>>,
    active: AtomicBool,
    queue: AtomicBool,
    status: Mutex<String>,
    /// Latest JSON for the web page, so it never waits for a busy controller.
    pub status_json: Mutex<String>,
    /// The playing track's cover on SoundCloud, for the page's /cover.
    pub cover: Mutex<Option<String>>,
    /// The track playing now, for the browser audio stream (/api/stream).
    pub now_track: Mutex<Option<Track>>,
    pub airplay: airplay::AirPlay,
    /// Anonymous statistics switched on (settings page).
    pub stats: AtomicBool,
    /// New releases on GitHub (settings page → «Обновления»).
    pub updates: update::Updates,
    /// The proxy from the settings, for threads that do not own the settings.
    pub proxy: Mutex<String>,
    /// An Android TV is set up for voice commands.
    tv: AtomicBool,
    /// «Обход блокировок»: what the running (or last) check has tried, for the page.
    pub bypass_log: Mutex<Vec<String>>,
    pub bypass_busy: AtomicBool,
    /// The last music started was Alice's own (Yandex Music, radio…), not
    /// SoundCloud: «продолжи» then belongs to her.
    pub alice_music: AtomicBool,
    /// SoundCloud by voice (off: music requests stay Alice's, PC and TV still work).
    pub soundcloud: AtomicBool,
    /// «Подсветка»: mode, colours, brightness.
    pub light: Mutex<led::Light>,
    /// «Фразы Алисы для программ на ПК» (setting share_heard).
    pub share_heard: AtomicBool,
    /// The last phrase Alice recognised and when (unix s); only while share_heard is on.
    heard: Mutex<(String, u64)>,
}

impl Shared {
    /// Remembers what Alice heard, for /api/status — only if the owner allowed it.
    pub fn set_heard(&self, text: &str) {
        if !self.share_heard.load(Ordering::Acquire) {
            return;
        }
        let at = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
        *self.heard.lock().unwrap() = (text.to_owned(), at);
    }
    /// `{"text", "at"}` for /api/status, null while sharing is off.
    pub fn heard_json(&self) -> serde_json::Value {
        if !self.share_heard.load(Ordering::Acquire) {
            return serde_json::Value::Null;
        }
        let (text, at) = self.heard.lock().unwrap().clone();
        serde_json::json!({ "text": text, "at": at })
    }
    pub fn is_active(&self) -> bool {
        self.active.load(Ordering::Acquire)
    }
    pub fn has_queue(&self) -> bool {
        self.queue.load(Ordering::Acquire)
    }
    pub fn soundcloud(&self) -> bool {
        self.soundcloud.load(Ordering::Acquire)
    }
    pub fn alice_music(&self) -> bool {
        self.alice_music.load(Ordering::Acquire)
    }
    pub fn set_alice_music(&self) {
        self.alice_music.store(true, Ordering::Release);
    }
    pub fn keyword_only(&self) -> bool {
        self.keyword_only.load(Ordering::Acquire)
    }
    pub fn pc_configured(&self) -> bool {
        self.pc.load(Ordering::Acquire)
    }
    pub fn tv_configured(&self) -> bool {
        self.tv.load(Ordering::Acquire)
    }
    pub fn contact_names(&self) -> Vec<String> {
        self.contacts.lock().unwrap().clone()
    }
    pub fn macros(&self) -> Vec<(String, String)> {
        self.macros.lock().unwrap().clone()
    }
}


struct Controller {
    client: Client,
    shared: Arc<Shared>,
    tx: Sender<Cmd>,
    device: String,
    queue: Vec<Track>,
    pos: usize,
    generation: u64,
    flags: Option<Arc<Flags>>,
    failures: u32,
    /// The sound card refused sound this many times in a row.
    audio_failures: u32,
    settings: Settings,
    dir: PathBuf,
    results: Vec<Track>,
    user: Option<String>,
    shuffle: bool,
    repeat: bool,
    /// Queue order before shuffling, to go back to «по порядку».
    original: Option<Vec<Track>>,
    /// Last connection check: round trip in ms, or the error.
    net: Option<Result<u64, String>>,
    net_checked: Option<std::time::Instant>,
    net_checking: bool,
    rng: u64,
    liked: HashSet<u64>,
    /// Attempts to restart the current track after a broken download.
    retries: u32,
    /// What is being fetched from SoundCloud right now, for the page's spinner.
    loading: Mutex<Option<String>>,
    /// «Недавно играло» on the page.
    history: history::History,
    /// Connection checks failed in a row while a bypass was on, and when the
    /// bypass was last re-picked by itself.
    net_fails: u32,
    repicked: Option<std::time::Instant>,
    /// What the running fetch was asked for, as the page shows it, and the
    /// command that asks for it again: goes to the history once it plays.
    asked: Option<(String, String)>,
    /// When a refused SoundCloud token was last asked again (every 10 minutes).
    token_checked: Option<std::time::Instant>,
    /// The command that loaded the queue now («play bonobo», «likes»…), for resuming.
    source_cmd: Option<String>,
    /// After a restart: (queue position, ms) to start at when the queue is loaded again.
    resume_at: Option<(usize, u64)>,
    resume_saved: Option<std::time::Instant>,
}

fn fmt_ms(ms: u64) -> String {
    format!("{}:{:02}", ms / 60_000, ms / 1000 % 60)
}

/// Sends the answer to the web page or the command line; for voice commands
/// (no reply channel) optionally says it in Alice's voice.
fn respond(reply: Option<Sender<String>>, text: &str, speech: Option<String>) {
    match reply {
        Some(r) => {
            let _ = r.send(text.to_owned());
        }
        None => {
            eprintln!("voice result: {text}");
            if let Some(s) = speech {
                alice::say(s);
            }
        }
    }
}

/// The connection's verdict from the last check of SoundCloud (None: not yet; Some(None): it
/// failed) and the download speed while music plays (kbit/s, only when fresh). Music downloading
/// is the better witness: a failed check alone does not make it «нет связи» while it plays.
fn net_level(ping: Option<Option<u64>>, speed: Option<u64>) -> &'static str {
    match (ping, speed) {
        (None, None) => "unknown",
        (Some(None), None) => "down",
        (Some(None) | None, Some(s)) => if s >= 600 { "good" } else if s >= 200 { "slow" } else { "bad" },
        (Some(Some(p)), s) if p <= 500 && s.is_none_or(|s| s >= 600) => "good",
        (Some(Some(p)), s) if p <= 1200 && s.is_none_or(|s| s >= 200) => "slow",
        _ => "bad",
    }
}

/// Every 10 s while Bluetooth output is on: is the device connected? A change moves the music.
/// After a restart of the speaker BlueALSA is back to receiving only: made to send again first.
fn bt_watch(tx: Sender<Cmd>, mac: Option<String>) {
    bt::set_output(mac);
    thread::spawn(move || {
        let mut source_ok = false;
        loop {
            thread::sleep(Duration::from_secs(10));
            let Some(mac) = bt::output() else { continue };
            if !source_ok {
                match bt::ensure_source() {
                    Ok(()) => source_ok = true,
                    Err(e) => {
                        eprintln!("bt: {e}");
                        thread::sleep(Duration::from_secs(50));
                        continue;
                    }
                }
            }
            let now = bt::info(&mac).is_some_and(|d| d.connected);
            if now != bt::connected(&mac) {
                eprintln!("bt: {mac} {}", if now { "connected" } else { "disconnected" });
                bt::set_connected(&mac, now);
                let (rtx, _rrx) = mpsc::channel();
                let _ = tx.send(Cmd::Control("bt reroute".into(), rtx));
            }
        }
    });
}

/// Lower case, «ё» → «е», punctuation dropped and numbers as digits: speech
/// recognition writes «первый» as «1», playlist titles may use either.
fn canon(s: &str) -> String {
    const NUMBERS: [(&str, &[&str]); 10] = [
        ("1", &["один", "одна", "одно", "первый", "первая", "первое", "первую", "первого"]),
        ("2", &["два", "две", "второй", "вторая", "второе", "вторую"]),
        ("3", &["три", "третий", "третья", "третье", "третью"]),
        ("4", &["четыре", "четвертый", "четвертая", "четвертое", "четвертую"]),
        ("5", &["пять", "пятый", "пятая", "пятое", "пятую"]),
        ("6", &["шесть", "шестой", "шестая", "шестое", "шестую"]),
        ("7", &["семь", "седьмой", "седьмая", "седьмое", "седьмую"]),
        ("8", &["восемь", "восьмой", "восьмая", "восьмое", "восьмую"]),
        ("9", &["девять", "девятый", "девятая", "девятое", "девятую"]),
        ("10", &["десять", "десятый", "десятая", "десятое", "десятую"]),
    ];
    s.to_lowercase()
        .replace('ё', "е")
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(|w| {
            NUMBERS
                .iter()
                .find(|(_, words)| words.contains(&w))
                .map(|(digit, _)| (*digit).to_owned())
                .unwrap_or_else(|| w.to_owned())
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Picks a playlist by (part of) its spoken name.
fn find_playlist<'a>(lists: &'a [sc::PlaylistInfo], name: &str) -> Option<&'a sc::PlaylistInfo> {
    let norm = canon;
    let want = norm(name);
    lists
        .iter()
        .find(|p| norm(&p.title) == want)
        .or_else(|| lists.iter().find(|p| norm(&p.title).contains(&want)))
        .or_else(|| {
            // Every spoken word appears in the title, in any order.
            lists.iter().find(|p| {
                let t = norm(&p.title);
                want.split_whitespace().all(|w| t.contains(w))
            })
        })
}

/// Finds one of the account's playlists by (part of) its name.
fn my_playlist(client: &Client, name: &str) -> sc::Res<Vec<Track>> {
    let lists = client.library()?;
    match find_playlist(&lists, name) {
        Some(p) => client.open_playlist(&p.id, &p.link),
        None => Err(format!("в ваших плейлистах нет «{name}»").into()),
    }
}

fn send_to(reply: &Sender<String>, text: String) {
    let _ = reply.send(text);
}

/// "Title, artist" for speech.
fn spoken(track: &Track) -> String {
    match &track.user {
        Some(u) if !u.username.is_empty() => format!("{}, {}", track.title, u.username),
        _ => track.title.clone(),
    }
}

impl Controller {
    fn publish(&self) {
        let active = self.flags.is_some();
        let playing = self.flags.as_ref().is_some_and(|f| !f.paused.load(Ordering::Acquire));
        self.shared.playing.store(playing, Ordering::Release);
        if playing {
            // SoundCloud is the music now: «продолжи» is ours again.
            self.shared.alice_music.store(false, Ordering::Release);
        }
        self.shared.active.store(active, Ordering::Release);
        self.shared.queue.store(!self.queue.is_empty(), Ordering::Release);
        self.shared.fetching.store(self.loading.lock().unwrap().is_some(), Ordering::Release);
        *self.shared.track.lock().unwrap() = self.flags.clone();
    }

    fn current(&self) -> Option<&Track> {
        self.flags.as_ref().and_then(|_| self.queue.get(self.pos))
    }

    fn status(&self) -> String {
        let Some(flags) = &self.flags else {
            return "stopped".into();
        };
        let track = &self.queue[self.pos];
        let state = if flags.paused.load(Ordering::Acquire) { "paused" } else { "playing" };
        format!(
            "{state}: {} [{}/{}] ({}/{})",
            track.name(),
            fmt_ms(flags.position_ms.load(Ordering::Acquire)),
            fmt_ms(track.duration.unwrap_or(0)),
            self.pos + 1,
            self.queue.len()
        )
    }

    fn json(&self) -> String {
        let (state, position) = match &self.flags {
            Some(f) if f.paused.load(Ordering::Acquire) => ("paused", f.position_ms.load(Ordering::Acquire)),
            Some(f) => ("playing", f.position_ms.load(Ordering::Acquire)),
            None => ("stopped", 0),
        };
        let current = self.current();
        // Its own macro call: one big json! runs out of the macro recursion limit.
        let mut settings = serde_json::json!({
            "proxy": self.settings.masked_proxy(),
            "keyword_only": self.settings.keyword_only,
            "ring": self.settings.ring,
            "token": self.settings.masked_token(),
            "sc_refresh": self.settings.masked_sc_refresh(),
            "token_exp": self.client.token_exp(),
            "sc_renew_error": self.client.renew_error().map(|e| e.trim_start_matches("refused: ").to_owned()),
            "pc_host": self.settings.pc_host,
            "pc_token": self.settings.masked_pc_token(),
            "pc_mac": self.settings.pc_mac,
            "pc_contacts": self.settings.pc_contacts,
            "airplay": self.settings.airplay,
            "airplay_name": self.settings.airplay_name,
            "airplay_icon": self.settings.airplay_icon,
            "stats": self.settings.stats,
            "share_heard": self.settings.share_heard,
            "tv_host": self.settings.tv_host,
            "tv_name": self.settings.tv_name,
            "tvs_more": self.settings.tvs_more,
            "tv_active": self.settings.active_tv().map(|t| t.name).unwrap_or_default(),
            "pc_name": self.settings.pc_name,
            "pcs_more": self.settings.masked_pcs_more(),
            "macros": self.settings.macros,
            "bypass": self.settings.bypass,
            "bypass_on": self.settings.bypass_on,
            "bypass_manual": self.settings.bypass_manual,
            "bypass_name": bypass::name_of(&self.settings.bypass),
            "zapret_args": self.settings.zapret_args,
            "proxy_on": self.settings.proxy_on,
            "soundcloud": self.settings.soundcloud,
            "light_mode": self.settings.light_mode,
            "light_color": self.settings.light_color,
            "light_color2": self.settings.light_color2,
            "light_bright": self.settings.light_bright,
            "light_bass": self.settings.light_bass,
            "light_mid": self.settings.light_mid,
            "light_high": self.settings.light_high,
            "pc_on": self.settings.pc_on,
            "tv_on": self.settings.tv_on,
        });
        settings["eq_on"] = self.settings.eq_on.into();
        settings["led_order"] = self.settings.led_order.clone().into();
        settings["light_speed"] = self.settings.light_speed.into();
        settings["light_intensity"] = self.settings.light_intensity.into();
        settings["onboarded"] = self.settings.onboarded.into();
        settings["pin_on"] = (!self.settings.panel_pin.is_empty()).into();
        settings["alice_voice"] = self.settings.alice_voice.into();
        settings["planner_on"] = self.settings.planner_on.into();
        settings["tg_on"] = self.settings.tg_on.into();
        settings["autostart"] = serde_json::to_value(&self.settings.autostart).unwrap_or_default();
        settings["resume_music"] = self.settings.resume_music.into();
        settings["display_on"] = self.settings.display_on.into();
        settings["display_widgets"] = self.settings.display_widgets.clone().into();
        settings["display_secs"] = self.settings.display_secs.into();
        settings["display_bright"] = self.settings.display_bright.into();
        settings["display_light_own"] = self.settings.display_light_own.into();
        settings["display_night_bright"] = self.settings.display_night_bright.into();
        settings["display_night_from"] = self.settings.display_night_from.into();
        settings["display_night_to"] = self.settings.display_night_to.into();
        settings["display_city"] = self.settings.display_city.clone().into();
        settings["tg_bots"] = self.settings.tg_bots.iter().map(|b| serde_json::json!({
            "name": b.name,
            "token": format!("••••••••{}", b.token.chars().rev().take(4).collect::<Vec<_>>().into_iter().rev().collect::<String>()),
        })).collect::<Vec<_>>().into();
        settings["tg_allowed"] = serde_json::to_value(&self.settings.tg_allowed).unwrap_or_default();
        settings["tg_read"] = self.settings.tg_read.into();
        settings["tg_max"] = self.settings.tg_max.into();
        settings["tg_quiet_on"] = self.settings.tg_quiet_on.into();
        settings["tg_quiet_from"] = self.settings.tg_quiet_from.into();
        settings["tg_quiet_to"] = self.settings.tg_quiet_to.into();
        settings["tz_offset"] = self.settings.tz_offset.into();
        settings["bt_mac"] = self.settings.bt_mac.clone().into();
        settings["bt_name"] = self.settings.bt_name.clone().into();
        settings["bt_on"] = self.settings.bt_on.into();
        settings["cal_on"] = self.settings.cal_on.into();
        settings["cal_url"] = if self.settings.cal_url.is_empty() { String::new() } else { "•••••••• (секретный адрес сохранён)".into() }.into();
        settings["cal_remind"] = self.settings.cal_remind.into();
        settings["plan_warn"] = self.settings.plan_warn.into();
        settings["plan_due_time"] = self.settings.plan_due_time.into();
        settings["brief_on"] = self.settings.brief_on.into();
        settings["brief_phrase"] = self.settings.brief_phrase.into();
        settings["brief_at"] = self.settings.brief_at.into();
        settings["brief_days"] = self.settings.brief_days.clone().into();
        settings["plan_target"] = self.settings.plan_target.clone().into();
        settings["plan_mode"] = self.settings.plan_mode().into();
        settings["work_from"] = self.settings.work_from.into();
        settings["work_to"] = self.settings.work_to.into();
        settings["work_days"] = self.settings.work_days.clone().into();
        settings["pomo_work"] = self.settings.pomo_work.into();
        settings["pomo_break"] = self.settings.pomo_break.into();
        settings["pomo_long"] = self.settings.pomo_long.into();
        settings["pomo_rounds"] = self.settings.pomo_rounds.into();
        settings["timer_face"] = self.settings.timer_face.into();
        settings["adb_keep"] = self.settings.adb_keep.into();
        settings["remote_tab"] = self.settings.remote_tab.into();
        settings["morning_on"] = self.settings.morning_on.into();
        settings["alarm_own"] = self.settings.alarm_own.into();
        settings["alarm_volume"] = self.settings.alarm_volume.into();
        settings["alarm_louder"] = self.settings.alarm_louder.into();
        settings["pc_power"] = self.settings.pc_power.into();
        settings["pc_apps"] = self.settings.pc_apps.into();
        settings["pc_tg"] = self.settings.pc_tg.into();
        settings["pc_search"] = self.settings.pc_search.into();
        settings["pc_plain"] = self.settings.pc_plain.into();
        settings["pc_keep"] = self.settings.pc_keep.clone().into();
        settings["morning_greet"] = self.settings.morning_greet.clone().into();
        settings["morning_news"] = self.settings.morning_news.clone().into();
        settings["morning_intro"] = self.settings.morning_intro.clone().into();
        settings["morning_weather"] = self.settings.morning_weather.into();
        settings["morning_plans"] = self.settings.morning_plans.into();
        settings["home_hidden"] = self.settings.home_hidden.clone().into();
        settings["home_order"] = self.settings.home_order.clone().into();
        settings["pomo_on_work"] = self.settings.pomo_on_work.clone().into();
        settings["teams"] = serde_json::to_value(&self.settings.teams).unwrap_or_default();
        settings["meet_minutes"] = self.settings.meet_minutes.into();
        settings["meet_bot_token"] = mask_tail(&self.settings.meet_bot_token).into();
        settings["pomo_on_break"] = self.settings.pomo_on_break.clone().into();
        settings["pomo_on_done"] = self.settings.pomo_on_done.clone().into();
        settings["g_on"] = self.settings.g_on.into();
        settings["g_client_id"] = self.settings.g_client_id.clone().into();
        settings["g_client_secret"] = if self.settings.g_client_secret.is_empty() { String::new() } else { "••••••••".into() }.into();
        settings["g_connected"] = (!self.settings.g_refresh.is_empty()).into();
        settings["g_read"] = self.settings.g_read.clone().into();
        settings["g_write"] = self.settings.g_write.clone().into();
        settings["g_list"] = self.settings.g_list.clone().into();
        settings["alice_voice_max"] = self.settings.alice_voice_max.into();
        settings["duck_mode"] = self.settings.duck_mode.clone().into();
        settings["duck_call_level"] = self.settings.duck_call_level.into();
        settings["duck_notify_level"] = self.settings.duck_notify_level.into();
        settings["duck_notify_secs"] = self.settings.duck_notify_secs.into();
        settings["duck_exclude"] = self.settings.duck_exclude.clone().into();
        settings["voice_hello"] = self.settings.voice_hello.into();
        settings["report_logs"] = self.settings.report_logs.into();
        settings["music_default"] = self.settings.music_default.clone().into();
        settings["eq"] = self.settings.eq.clone().into();
        let mut out = serde_json::json!({
            "state": state,
            "track": current.map(Track::name),
            "position_ms": position,
            "duration_ms": current.and_then(|t| t.duration).unwrap_or(0),
            "pos": self.pos,
            "queue": self.queue.iter().map(Track::name).collect::<Vec<_>>(),
            "queue_ids": self.queue.iter().map(|t| t.id).collect::<Vec<_>>(),
            "queue_liked": self.queue.iter().map(|t| self.liked.contains(&t.id)).collect::<Vec<_>>(),
            "current_id": current.map(|t| t.id),
            "current_url": current.and_then(|t| t.permalink_url.clone()),
            "writes": WRITES,
            "net": self.net_json(),
            "current_liked": current.is_some_and(|t| self.liked.contains(&t.id)),
            "shuffle": self.shuffle,
            "repeat": self.repeat,
            "settings": settings,
            "user": self.user,
            // none | ok | checking (saved, not confirmed yet) | refused (sign in again)
            "login": login_state(self.client.has_token(), self.client.token_refused() || self.client.token_expired(), self.user.is_some()),
            "loading": self.loading.lock().unwrap().clone(),
            "airplay": self.shared.airplay.json(),
            "update": self.shared.updates.json(),
            "bypass_log": *self.shared.bypass_log.lock().unwrap(),
            "bypass_busy": self.shared.bypass_busy.load(Ordering::Acquire),
            "volume": volume::level(),
            "led": led::info(),
            "device": device_info(),
            "pc_legacy": pc::LEGACY.load(Ordering::Acquire),
            "ducking": ducking::json(),
            "voice": voice::json(),
            "telegram": telegram::json(),
            "display": display::json(),
            "timer": timer::json(),
            // The Mac/Windows app may send updates here (web.rs /api/update/upload), confirmed by «Алиса, да».
            "net_update": true,
            "calendar": calendar::json(),
            "listen": serde_json::json!({
                "listeners": web::listeners(),
                "muted": self.shared.duck.listening(),
                "gen": web::LISTEN_GEN.load(Ordering::Acquire),
                // MP3/AAC start in the browser where the speaker is; MP4 from the beginning.
                "seekable": current.is_some_and(Track::stream_seekable),
            }),
            "onboarding": serde_json::json!({
                "stage": format!("{:?}", hello::stage()),
                "phase": led::install_phase().map(|p| format!("{p:?}")),
            }),
            "source": alice::source_of(&self.shared),
            "zapret_presets": zapret::PRESETS.iter().enumerate().map(|(i, (name, _))| { let id = zapret::id_of(i); let e = zapret::engine(&id, "").to_string(); [id, (*name).to_owned(), e] }).collect::<Vec<_>>(),
        });
        // Through the speaker: the phone may not reach sndcdn.com itself.
        out["heard"] = self.shared.heard_json();
        out["cover"] = current.filter(|t| t.cover().is_some()).map(|t| format!("/cover?{}", t.id)).into();
        out.to_string()
    }

    /// Measures the round trip to SoundCloud in the background; `reply` is answered when done.
    fn check_net(&mut self, reply: Option<Sender<String>>) {
        if self.net_checking {
            if let Some(r) = reply {
                let _ = r.send("OK".into());
            }
            return;
        }
        self.net_checking = true;
        let client = self.client.clone();
        let tx = self.tx.clone();
        thread::spawn(move || {
            let result = client.ping().map_err(|e| e.to_string());
            if let Err(e) = &result {
                eprintln!("net: {e}");
            }
            let _ = tx.send(Cmd::Net(result));
            if let Some(r) = reply {
                let _ = r.send("OK".into());
            }
        });
    }

    /// Connection quality for the page: ping, download speed, buffer and a verdict.
    fn net_json(&self) -> serde_json::Value {
        let (speed_kbps, buffer_s) = {
            let st = buffer::STATS.lock().unwrap();
            let fresh = st.updated.is_some_and(|t| t.elapsed() < Duration::from_secs(90));
            let speed = fresh.then(|| (st.speed * 8.0 / 1000.0).round() as u64);
            // ~160 kbit/s AAC = 20 kB per second of music.
            let buffer = self.flags.is_some().then(|| (st.ahead / 20_000) as u64);
            (speed, buffer)
        };
        let ping = self.net.as_ref().and_then(|r| r.as_ref().ok().copied());
        let error = self.net.as_ref().and_then(|r| r.as_ref().err().cloned());
        let level = net_level(self.net.as_ref().map(|r| r.as_ref().ok().copied()), speed_kbps);
        let warning = match level {
            "slow" => Some(
                "Соединение медленное: треки будут дольше загружаться, а команды SoundCloud — дольше выполняться. \
                 Если пользуетесь прокси, проверьте его скорость или попробуйте другой."
                    .to_owned(),
            ),
            "bad" => Some(
                "Соединение плохое: треки будут долго загружаться и могут прерываться, Алиса будет долго \
                 «думать» над командами SoundCloud. Проверьте прокси или смените его."
                    .to_owned(),
            ),
            "down" => Some(format!(
                "Нет связи с SoundCloud ({}). Проверьте прокси в настройках ниже.",
                error.clone().unwrap_or_default()
            )),
            // Music comes, but the last check of SoundCloud's API failed: search and likes may be slow.
            _ if error.is_some() => Some(format!(
                "Музыка загружается, но последняя проверка SoundCloud не прошла ({}): поиск и лайки могут отвечать медленно.",
                error.clone().unwrap_or_default()
            )),
            _ => None,
        };
        serde_json::json!({
            "level": level,
            "ping_ms": ping,
            "error": error,
            "speed_kbps": speed_kbps,
            "buffer_s": buffer_s,
            "warning": warning,
        })
    }

    /// The page shows the password masked; a masked value sent back means "unchanged".
    fn real_proxy(&self, proxy: &str) -> String {
        if proxy.contains('•') && proxy == self.settings.masked_proxy() {
            self.settings.proxy.clone()
        } else {
            proxy.trim().to_owned()
        }
    }

    /// Checks the login in the background and reports the username.
    fn check_login(&self) {
        let client = self.client.clone();
        let tx = self.tx.clone();
        thread::spawn(move || {
            // A token marked «refused» is asked again first: it may be fine.
            if client.token_refused() {
                client.verify_token();
            }
            let user = if client.has_token() {
                match client.me() {
                    Ok((_, name)) => {
                        if WRITES {
                            if let Ok(ids) = client.liked_ids() {
                                let _ = tx.send(Cmd::Liked(ids));
                            }
                        }
                        client.warm_recommendations();
                        Some(name)
                    }
                    Err(e) => {
                        eprintln!("login: {e}");
                        None
                    }
                }
            } else {
                None
            };
            let _ = tx.send(Cmd::User(user));
        });
    }

    /// Switches the bypass to `id` ("" = off). `manual`: chosen by hand, so
    /// soundya does not replace it by itself; a check hands it back.
    fn switch_bypass(&mut self, id: String, manual: bool) {
        self.settings.bypass_manual = manual && !id.is_empty();
        self.settings.bypass_on = !id.is_empty();
        self.settings.bypass = id;
        bypass::set(bypass_strategy(self.settings.active_bypass()));
        if !zapret::is_zapret(self.settings.active_bypass()) {
            zapret::stop();
        }
        // New connections with (or without) the trick from now on.
        let _ = self.client.set_proxy(self.settings.use_proxy());
        if self.user.is_none() && self.client.has_token() {
            self.check_login();
        }
        if let Err(e) = self.settings.save(&self.dir) {
            eprintln!("bypass: settings not saved: {e}");
        }
        self.net = None;
        self.net_checked = None;
    }

    fn apply_settings(&mut self, mut new: Settings) -> String {
        if new.display_night_from >= 1440 || new.display_night_to >= 1440 {
            return "error: время ночи — от 00:00 до 23:59".into();
        }
        if !led::MODES.contains(&new.light_mode.as_str()) {
            new.light_mode = "solid".into();
        }
        for c in [&mut new.light_color, &mut new.light_color2, &mut new.light_bass, &mut new.light_mid, &mut new.light_high] {
            if led::led_colour(c).is_none() {
                return "error: цвет вида #ff7a00".into();
            }
            *c = c.trim().to_lowercase();
        }
        new.light_bright = new.light_bright.clamp(10, 100);
        new.light_speed = new.light_speed.clamp(25, 300);
        new.light_intensity = new.light_intensity.clamp(30, 200);
        new.sync_music(&self.settings);
        // Only «pin set/off» changes the PIN, never the settings form.
        new.panel_pin = self.settings.panel_pin.clone();
        if !["smart", "calls", "notify", "manual", "off"].contains(&new.duck_mode.as_str()) {
            return "error: режим приглушения — smart, calls, notify, manual или off".into();
        }
        new.duck_call_level = new.duck_call_level.min(100);
        new.duck_notify_level = new.duck_notify_level.min(100);
        new.duck_notify_secs = new.duck_notify_secs.clamp(1, 120);
        new.duck_exclude = new.duck_exclude.iter().map(|a| a.trim().to_owned()).filter(|a| !a.is_empty()).take(30).collect();
        ducking::configure(ducking::Config::from_settings(&new));
        hello::set_voice(new.voice_hello);
        new.alice_voice = new.alice_voice.clamp(10, 100);
        new.alice_voice_max = new.alice_voice_max.min(10);
        voice::configure(new.alice_voice, new.alice_voice_max);
        new.tz_offset = new.tz_offset.clamp(-720, 840);
        // Bot tokens come back masked from the page: masked = unchanged.
        for (i, b) in new.tg_bots.iter_mut().enumerate() {
            b.name = b.name.trim().to_owned();
            b.token = b.token.trim().to_owned();
            if b.token.contains('•') {
                if let Some(old) = self.settings.tg_bots.get(i) {
                    b.token = old.token.clone();
                }
            }
        }
        new.tg_bots.retain(|b| !b.token.is_empty() && !b.token.contains('•'));
        new.tg_allowed.retain(|a| a.id != 0);
        for a in &new.tg_allowed {
            telegram::forget_unknown(a.id);
        }
        telegram::configure(telegram::Config::from_settings(&new), &self.shared);
        new.display_widgets.retain(|w| ["clock", "pc", "weather"].contains(&w.as_str()));
        new.display_widgets.dedup();
        new.display_secs = new.display_secs.clamp(3, 120);
        new.display_bright = new.display_bright.clamp(5, 100);
        new.display_night_bright = new.display_night_bright.clamp(5, 100);
        display::configure(display_widgets(&new), new.display_bright, &new);
        new.autostart.retain(|a| !a.phrase.trim().is_empty());
        new.autostart.truncate(10);
        for a in &mut new.autostart {
            a.delay = a.delay.min(600);
            if a.when != "boot" {
                a.when = "always".into();
            }
        }
        planner::configure(new.planner_on, new.tz_offset);
        // The secret address comes back masked from the page: masked = unchanged.
        new.cal_url = new.cal_url.trim().to_owned();
        if new.cal_url.contains('•') {
            new.cal_url = self.settings.cal_url.clone();
        }
        if !new.cal_url.is_empty() && !calendar::valid_url(&new.cal_url) {
            return "error: нужен адрес вида https://calendar.google.com/calendar/ical/…/basic.ics".into();
        }
        new.cal_remind = new.cal_remind.min(180);
        calendar::configure(new.cal_on, &new.cal_url, new.cal_remind);
        // Google: the secret comes back masked; the refresh token only from «google code/off».
        new.g_client_id = new.g_client_id.trim().to_owned();
        new.g_client_secret = new.g_client_secret.trim().to_owned();
        if new.g_client_secret.contains('•') {
            new.g_client_secret = self.settings.g_client_secret.clone();
        }
        if new.g_client_id != self.settings.g_client_id || new.g_client_secret != self.settings.g_client_secret {
            // Another client: the old sign-in is of no use to it.
            if !self.settings.g_client_id.is_empty() && new.g_refresh == self.settings.g_refresh {
                new.g_refresh.clear();
            }
        }
        if !new.g_client_id.is_empty() && !new.g_client_id.ends_with(".apps.googleusercontent.com") {
            return "error: ID клиента Google заканчивается на .apps.googleusercontent.com".into();
        }
        new.plan_warn = new.plan_warn.min(240);
        new.plan_due_time = new.plan_due_time.min(1440);
        new.brief_at = new.brief_at.min(1439);
        if new.brief_days != "weekdays" {
            new.brief_days = "all".into();
        }
        if new.plan_target != "local" && new.plan_target != "work" {
            new.plan_target = "google".into();
        }
        new.plan_mode = new.plan_mode().into();
        new.home_hidden.retain(|w| settings::HOME_WIDGETS.contains(&w.as_str()));
        new.home_order.retain(|w| settings::HOME_WIDGETS.contains(&w.as_str()));
        // Not only neighbours: «a, b, a» kept the second «a» (a widget shown twice).
        let mut seen = std::collections::HashSet::new();
        new.home_order.retain(|w| seen.insert(w.clone()));
        new.meet_minutes = new.meet_minutes.clamp(5, 480);
        // 0 would ask «сделали?» again every 10 s after «нет».
        new.plan_check_every = new.plan_check_every.clamp(1, 24 * 60);
        new.plan_check_times = new.plan_check_times.max(1);
        // Comes back masked from the page: masked = unchanged.
        new.meet_bot_token = new.meet_bot_token.trim().to_owned();
        if new.meet_bot_token.contains('•') {
            new.meet_bot_token = self.settings.meet_bot_token.clone();
        } else if !new.meet_bot_token.is_empty() && !valid_bot_token(&new.meet_bot_token) {
            return "error: токен бота вида 123456789:AA… (его даёт @BotFather)".into();
        }
        for t in &mut new.teams {
            t.name = t.name.trim().chars().take(40).collect();
            t.emails = t.emails.iter().map(|e| e.trim().to_lowercase()).filter(|e| !e.is_empty()).collect();
            if let Some(bad) = t.emails.iter().find(|e| !valid_email(e)) {
                return format!("error: «{bad}» — не адрес почты");
            }
            let mut seen = std::collections::HashSet::new();
            t.emails.retain(|e| seen.insert(e.clone()));
            t.emails.truncate(100);
        }
        new.teams.truncate(20);
        new.work_from = new.work_from.min(1439);
        new.work_to = new.work_to.clamp(new.work_from + 1, 1440);
        if new.work_days != "all" {
            new.work_days = "weekdays".into();
        }
        let p = new.pomo();
        (new.pomo_work, new.pomo_break, new.pomo_long, new.pomo_rounds) = (p.work, p.brk, p.long, p.rounds);
        apply_planner(&new);
        new.led_order = new.led_order.trim().to_lowercase();
        if !led::set_order(&new.led_order) {
            return "error: порядок цветов — rgb, grb, brg, bgr, rbg или gbr".into();
        }
        // A masked value is «unchanged», also when it is an older mask (the key was replaced
        // meanwhile, e.g. by the installer): saving any other setting failed with «ключ компьютера…».
        if new.token.contains('•') {
            new.token = self.settings.token.clone();
        }
        new.token = new.token.trim().trim_start_matches("OAuth ").trim().to_owned();
        if new.sc_refresh.contains('•') {
            new.sc_refresh = self.settings.sc_refresh.clone();
        }
        new.sc_refresh = new.sc_refresh.trim().to_owned();
        // Logging out forgets the renewal too.
        if new.token.is_empty() {
            new.sc_refresh.clear();
        }
        if new.pc_token.contains('•') {
            new.pc_token = self.settings.pc_token.clone();
        }
        new.pc_host = new.pc_host.trim().to_owned();
        new.pc_token = new.pc_token.trim().to_owned();
        new.pc_mac = new.pc_mac.trim().to_owned();
        if new.pc_token.chars().any(|c| c.is_whitespace() || !c.is_ascii()) {
            return "error: ключ компьютера — одно слово из латинских букв и цифр, как его показал установщик".into();
        }
        if !new.pc_mac.is_empty() && pc::parse_mac(&new.pc_mac).is_none() {
            return "error: MAC-адрес вида AA:BB:CC:DD:EE:FF".into();
        }
        // More computers and TVs: a masked key is the one kept (same place in the list, or
        // the same address); rows with nothing in them are dropped.
        let old_pcs = self.settings.pcs_more.clone();
        for (i, p) in new.pcs_more.iter_mut().enumerate() {
            p.name = p.name.trim().to_owned();
            p.host = p.host.trim().to_owned();
            p.mac = p.mac.trim().to_owned();
            if p.token.contains('•') {
                p.token = old_pcs.get(i).filter(|o| o.host == p.host || old_pcs.len() == 1).or_else(|| old_pcs.iter().find(|o| o.host == p.host)).map(|o| o.token.clone()).unwrap_or_default();
            }
            p.token = p.token.trim().to_owned();
        }
        new.pcs_more.retain(|p| !p.host.is_empty() || !p.mac.is_empty());
        for p in &new.pcs_more {
            if p.token.chars().any(|c| c.is_whitespace() || !c.is_ascii()) {
                return format!("error: ключ компьютера «{}» — одно слово из латинских букв и цифр", p.name);
            }
            if !p.mac.is_empty() && pc::parse_mac(&p.mac).is_none() {
                return format!("error: MAC-адрес компьютера «{}» вида AA:BB:CC:DD:EE:FF", p.name);
            }
        }
        new.pc_name = new.pc_name.trim().to_owned();
        new.tv_name = new.tv_name.trim().to_owned();
        for t in new.tvs_more.iter_mut() {
            t.name = t.name.trim().to_owned();
            t.host = t.host.trim().to_owned();
        }
        new.tvs_more.retain(|t| !t.host.is_empty());
        // Voice picks a device by its name: the extra ones need one, all different.
        {
            let mut names = std::collections::HashSet::new();
            let extra = new.tvs_more.iter().map(|t| &t.name).chain(new.pcs_more.iter().map(|p| &p.name));
            for n in extra {
                if n.is_empty() {
                    return "error: дайте имя каждому дополнительному телевизору и компьютеру — по нему их зовут голосом".into();
                }
            }
            for n in new.tvs().iter().map(|d| d.name.clone()).chain(new.pcs().iter().map(|d| d.name.clone())).filter(|n| !n.is_empty()) {
                if !names.insert(alice::normalize_phrase(&n)) {
                    return format!("error: имя «{n}» уже занято — у каждого устройства своё");
                }
            }
        }
        if !new.tvs().iter().any(|t| t.name == new.tv_active) {
            new.tv_active = String::new();
        }
        // «мама, маме, маму» -> "мама|маме|маму"; rows without a name or a target are dropped.
        new.pc_contacts = new
            .pc_contacts
            .iter()
            .map(|c| settings::Contact {
                names: c
                    .names
                    .to_lowercase()
                    .replace('ё', "е")
                    .split([',', '|', ';'])
                    .map(str::trim)
                    .filter(|n| !n.is_empty())
                    .collect::<Vec<_>>()
                    .join("|"),
                target: c.target.trim().to_owned(),
            })
            .filter(|c| !c.names.is_empty() && !c.target.is_empty())
            .collect();
        self.shared.pc.store(new.pc_configured(), Ordering::Release);
        *self.shared.contacts.lock().unwrap() = contact_names(&new);
        // Macros are managed by the «macro» command, not this form: keep them.
        new.macros = self.settings.macros.clone();
        *self.shared.macros.lock().unwrap() = macro_list(&new);
        let token_changed = new.token != self.settings.token;
        let refresh_changed = new.sc_refresh != self.settings.sc_refresh;
        new.proxy = self.real_proxy(&new.proxy);
        // Set only by «bypass auto/off», not by the settings form.
        new.bypass = self.settings.bypass.clone();
        new.zapret_args = self.settings.zapret_args.clone();
        new.bypass_manual = self.settings.bypass_manual;
        let bypass_changed = new.active_bypass() != self.settings.active_bypass();
        if bypass_changed {
            bypass::set(bypass_strategy(new.active_bypass()));
            zapret::restore(&self.dir, new.active_bypass(), &new.zapret_args);
        }
        if new.use_proxy() != self.settings.use_proxy() || bypass_changed {
            if let Err(e) = self.client.set_proxy(new.use_proxy()) {
                return format!("error: неверный прокси: {e}");
            }
            self.net = None;
            self.net_checked = None;
        }
        self.shared.keyword_only.store(new.keyword_only, Ordering::Release);
        self.shared.ring.store(new.ring, Ordering::Release);
        *self.shared.light.lock().unwrap() = led::Light::from_settings(&new);
        eq::set(new.eq_on, &new.eq);
        new.airplay_name = new.airplay_name.trim().to_owned();
        self.shared.airplay.configure(new.airplay, &new.airplay_name, &new.airplay_icon);
        self.shared.stats.store(new.stats, Ordering::Release);
        self.shared.share_heard.store(new.share_heard, Ordering::Release);
        if !new.share_heard {
            *self.shared.heard.lock().unwrap() = (String::new(), 0);
        }
        *self.shared.proxy.lock().unwrap() = new.use_proxy().to_owned();
        new.tv_host = new.tv_host.trim().to_owned();
        self.shared.tv.store(new.tv_configured(), Ordering::Release);
        alice::set_tv(new.tv_configured());
        alice::set_devices(device_names(&new));
        new.pc_keep = new.pc_keep.trim().chars().take(500).collect();
        alice::set_pc_rules(pc_rules(&new));
        new.morning_greet = new.morning_greet.trim().chars().take(200).collect();
        new.morning_intro = new.morning_intro.trim().chars().take(300).collect();
        new.morning_news = new.morning_news.trim().to_owned();
        if !new.morning_news.is_empty() && morning::feed_url(&new.morning_news).is_none() && morning::tg_channels(&new.morning_news).is_none() {
            return "error: новости — один из источников списка, ссылка на RSS (https://…) или каналы Telegram (@канал, @канал)".into();
        }
        alarm::set_morning(new.morning_on);
        alarm::set_own(new.alarm_own);
        new.alarm_volume = new.alarm_volume.clamp(1, volume::MAX);
        alarm::set_volume(new.alarm_volume, new.alarm_louder);
        self.shared.soundcloud.store(new.soundcloud, Ordering::Release);
        let proxy_changed = new.use_proxy() != self.settings.use_proxy();
        // Only a save that passed all checks switches ADB (turning it off touches the device).
        adbkeep::configure(new.adb_keep);
        self.settings = new;
        let mut msg = match self.settings.save(&self.dir) {
            Ok(()) => "OK: сохранено".to_owned(),
            Err(e) => format!("применено, но не сохранено: {e}"),
        };
        if refresh_changed {
            self.client.set_refresh(&self.settings.sc_refresh);
        }
        if token_changed || proxy_changed || refresh_changed {
            self.client.set_token(&self.settings.token);
            if self.settings.token.is_empty() {
                self.user = None;
                if token_changed {
                    msg.push_str(", вы вышли из SoundCloud");
                }
            } else {
                // Checked in the background; the page shows the name when it is confirmed.
                self.user = None;
                self.check_login();
                msg.push_str(", проверяю вход…");
            }
        }
        msg
    }

    fn rand(&mut self) -> u64 {
        // xorshift64: plenty for shuffling a playlist.
        self.rng ^= self.rng << 13;
        self.rng ^= self.rng >> 7;
        self.rng ^= self.rng << 17;
        self.rng
    }

    fn shuffle_slice(&mut self, v: &mut [Track]) {
        for i in (1..v.len()).rev() {
            let j = (self.rand() % (i as u64 + 1)) as usize;
            v.swap(i, j);
        }
    }

    /// Runs a slow SoundCloud request off the controller thread; the result comes back as `Loaded`.
    fn fetch(
        &self,
        what: String,
        reply: Option<Sender<String>>,
        load: impl FnOnce(&Client) -> sc::Res<Vec<Track>> + Send + 'static,
    ) {
        *self.loading.lock().unwrap() = Some(what.clone());
        self.publish();
        self.share_status();
        let client = self.client.clone();
        let tx = self.tx.clone();
        thread::spawn(move || {
            let result = load(&client);
            let _ = tx.send(Cmd::Loaded { what, result, reply });
        });
    }

    fn on_loaded(&mut self, what: String, result: sc::Res<Vec<Track>>, reply: Option<Sender<String>>) {
        *self.loading.lock().unwrap() = None;
        if result.as_ref().map_or(true, |t| t.is_empty()) {
            self.asked = None;
        }
        self.publish();
        match result {
            Ok(mut tracks) if !tracks.is_empty() => {
                if self.shuffle {
                    self.original = Some(tracks.clone());
                    self.shuffle_slice(&mut tracks);
                } else {
                    self.original = None;
                }
                self.queue = tracks;
                self.failures = 0;
                if let Some((name, cmd)) = self.asked.take() {
                    self.source_cmd = Some(cmd.clone());
                    self.history.set(name, cmd);
                }
                match self.resume_at.take() {
                    // After a restart: where it was.
                    Some((pos, ms)) if pos < self.queue.len() => {
                        eprintln!("resume: track {} at {}", pos + 1, fmt_ms(ms));
                        self.retries = 0;
                        self.start_at(pos, ms.saturating_sub(2_000), 0);
                    }
                    _ => self.start(0),
                }
                pause_native_again(&self.shared);
                respond(reply, &format!("OK {}", self.status()), None);
            }
            Ok(_) => respond(
                reply,
                &format!("nothing found for {what:?}"),
                Some(format!("Не нашла «{what}» на SoundCloud")),
            ),
            Err(e) => {
                let e = blocked_hint(&e.to_string());
                let speech = if e.contains("плейлистах нет") {
                    e.clone()
                } else if e.contains("токен") {
                    "Вход в SoundCloud устарел. Обновите токен на странице колонки".into()
                } else {
                    "SoundCloud не ответил, попробуйте ещё раз".into()
                };
                respond(reply, &format!("error: {e}"), Some(speech));
            }
        }
    }

    /// The sound card refused the sound. Not the network, so retrying the track
    /// does not help; a fresh start of soundya does (seen on a Station Mini 1
    /// right after install: «Operation not supported», error 95).
    /// Where the music plays: the Bluetooth device when it is on and connected, else the speaker.
    fn out_device(&self) -> String {
        let s = &self.settings;
        if s.bt_on && bt::valid_mac(&s.bt_mac) && bt::connected(&s.bt_mac) {
            bt::pcm_name(&s.bt_mac)
        } else {
            self.device.clone()
        }
    }

    /// The music playing now moves to where it should play (Bluetooth on/off, connected/lost).
    fn reroute(&mut self) {
        // The track goes on in its own download: only the sound card changes (it was restarted
        // before: slow, and from a wrong place when Bluetooth had swallowed sound ahead).
        if let Some(f) = self.flags.as_ref() {
            f.move_to(&self.out_device());
        }
    }

    fn bt_save(&mut self) -> String {
        bt::set_output(self.settings.bt_on.then(|| self.settings.bt_mac.clone()));
        let r = match self.settings.save(&self.dir) {
            Ok(()) => String::new(),
            Err(e) => format!(" (настройки не сохранились: {e})"),
        };
        self.reroute();
        self.publish();
        self.share_status();
        r
    }

    fn bt_cmd(&mut self, arg: &str, reply: Sender<String>) {
        let (what, rest) = arg.split_once(' ').unwrap_or((arg, ""));
        let rest = rest.trim().to_owned();
        match what {
            "" | "status" => send_to(&reply, bt::status_json(&self.settings.bt_mac, self.settings.bt_on).to_string()),
            "scan" => {
                thread::spawn(move || {
                    let r = match bt::scan(10) {
                        Ok(list) => serde_json::json!({ "devices": list }),
                        Err(e) => serde_json::json!({ "error": e }),
                    };
                    send_to(&reply, r.to_string());
                });
            }
            "connect" => {
                let tx = self.tx.clone();
                thread::spawn(move || match bt::connect(&rest) {
                    Ok(d) => {
                        bt::set_connected(&d.mac, true);
                        let name = if d.name.is_empty() { d.mac.clone() } else { d.name.clone() };
                        let _ = tx.send(Cmd::Control(format!("bt chosen {} {name}", d.mac), reply));
                    }
                    Err(e) => send_to(&reply, format!("error: {e}")),
                });
            }
            // From «connect»: this device is the output from now on.
            "chosen" => {
                let (mac, name) = rest.split_once(' ').unwrap_or((rest.as_str(), ""));
                if !bt::valid_mac(mac) {
                    return send_to(&reply, "error: неверный адрес".into());
                }
                self.settings.bt_mac = mac.to_ascii_uppercase();
                self.settings.bt_name = name.chars().take(60).collect();
                self.settings.bt_on = true;
                let note = self.bt_save();
                send_to(&reply, format!("OK: музыка soundya теперь в «{}»{note}", if name.is_empty() { mac } else { name }));
            }
            "on" => {
                if !bt::valid_mac(&self.settings.bt_mac) {
                    return send_to(&reply, "error: сначала подключите устройство".into());
                }
                self.settings.bt_on = true;
                let note = self.bt_save();
                let connected = bt::connected(&self.settings.bt_mac);
                // A plain «OK» (the page shows nothing): the switch itself shows where it plays.
                send_to(&reply, if connected { if note.is_empty() { "OK".into() } else { format!("OK{note}") } }
                    else { format!("OK: музыка пойдёт в «{}», как только оно подключится{note}", self.settings.bt_name) });
            }
            "off" => {
                self.settings.bt_on = false;
                let note = self.bt_save();
                send_to(&reply, if note.is_empty() { "OK".into() } else { format!("OK{note}") });
            }
            "disconnect" | "forget" => {
                let mac = if rest.is_empty() { self.settings.bt_mac.clone() } else { rest.to_ascii_uppercase() };
                if !bt::valid_mac(&mac) {
                    return send_to(&reply, "error: нет такого устройства".into());
                }
                bt::set_connected(&mac, false);
                let forget = what == "forget";
                if mac == self.settings.bt_mac {
                    self.settings.bt_on = false;
                    if forget {
                        self.settings.bt_mac.clear();
                        self.settings.bt_name.clear();
                    }
                    self.bt_save();
                }
                thread::spawn(move || {
                    let r = if forget { bt::forget(&mac) } else { bt::disconnect(&mac) };
                    send_to(&reply, r.map_or_else(|e| format!("error: {e}"), |()| if forget { "OK: устройство забыто".into() } else { "OK: отключено, музыка на колонке".into() }));
                });
            }
            // From the watcher: the device came or went.
            "reroute" => {
                self.reroute();
                self.publish();
                send_to(&reply, "OK".into());
            }
            _ => send_to(&reply, "usage: bt status | scan | connect MAC | on | off | disconnect [MAC] | forget MAC".into()),
        }
    }

    fn audio_failed(&mut self, e: &str, position: u64) {
        // Headphones gone mid-track: on the speaker from where it stopped, not a failure.
        if self.settings.bt_on && bt::connected(&self.settings.bt_mac) {
            eprintln!("bt: {} lost ({e}); back to the speaker", self.settings.bt_mac);
            bt::set_connected(&self.settings.bt_mac, false);
            let p = self.pos;
            self.start_at(p, position.saturating_sub(2_000), 300);
            return;
        }
        self.audio_failures += 1;
        eprintln!("audio output failed ({} in a row): {e}", self.audio_failures);
        note_audio_error(&self.dir, &self.device, e);
        if self.audio_failures == 1 {
            // Once more, in case the card was just busy.
            let p = self.pos;
            self.start_at(p, position.saturating_sub(2_000), 1_500);
            return;
        }
        // run.sh starts soundya again 3 s after it exits; at most once in 10 minutes.
        let marker = std::path::Path::new("/tmp/soundya-audio-restart");
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let last = fsx::read_string(marker).and_then(|t| t.trim().parse::<u64>().ok()).unwrap_or(0);
        if now.saturating_sub(last) > 600 {
            let _ = std::fs::write(marker, now.to_string());
            eprintln!("audio output keeps failing; restarting soundya");
            alice::say("Звук не выводится, перезапускаю саундя. Включите музыку ещё раз через несколько секунд".into());
            thread::sleep(Duration::from_secs(6));
            std::process::exit(75);
        }
        eprintln!("audio output keeps failing after a restart; stopping");
        alice::say("Колонка не выводит звук. Перезагрузите её и попробуйте ещё раз".into());
        self.audio_failures = 0;
        self.halt();
        self.publish();
    }

    /// What plays now, for «продолжить музыку после перезагрузки»: every 15 s while
    /// it plays; a pause or a stop by the user is saved too (then nothing resumes).
    fn save_resume(&mut self) {
        // While a resume is still loading (resume_saved is set 2 minutes ahead),
        // the old file stays as it was; a load that never came is forgotten.
        if !self.settings.resume_music || self.resume_saved.is_some_and(|t| t.elapsed() < Duration::from_secs(15)) {
            return;
        }
        self.resume_at = None;
        self.resume_saved = Some(std::time::Instant::now());
        let playing = self.flags.as_ref().is_some_and(|f| !f.paused.load(Ordering::Acquire));
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
        let v = serde_json::json!({
            "playing": playing && self.source_cmd.is_some(),
            "cmd": self.source_cmd,
            "pos": self.pos,
            "track": self.current().map(|t| t.id),
            "ms": self.flags.as_ref().map_or(0, |f| f.position_ms.load(Ordering::Acquire)),
            "at": now,
        });
        let _ = settings::write_synced(&self.dir.join("resume.json"), v.to_string().as_bytes());
    }

    /// At start: music that was playing under 20 minutes ago goes on.
    fn resume_after_restart(&mut self) {
        if !self.settings.resume_music {
            return;
        }
        let Some(v) = fsx::read_string(&self.dir.join("resume.json")).and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok()) else { return };
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
        let fresh = now.saturating_sub(v["at"].as_u64().unwrap_or(0)) < 20 * 60;
        let (Some(cmd), true, true) = (v["cmd"].as_str(), v["playing"].as_bool().unwrap_or(false), fresh) else { return };
        eprintln!("resume: «{cmd}» was playing before the restart; going on");
        self.resume_at = Some((v["pos"].as_u64().unwrap_or(0) as usize, v["ms"].as_u64().unwrap_or(0)));
        self.resume_saved = Some(std::time::Instant::now() + Duration::from_secs(120));
        let (tx, _rx) = mpsc::channel();
        self.control(cmd, tx);
    }

    /// Autostart scenarios, in order, each after its delay; «boot» ones only
    /// when the speaker itself was just switched on.
    fn autostart(&self) {
        let boot = fsx::read_string(std::path::Path::new("/proc/uptime"))
            .and_then(|u| u.split('.').next().and_then(|s| s.parse::<u64>().ok()))
            .is_some_and(|up| up < 600);
        let list: Vec<settings::Autostart> =
            self.settings.autostart.iter().filter(|a| !a.phrase.trim().is_empty() && (a.when != "boot" || boot)).cloned().collect();
        if list.is_empty() {
            return;
        }
        let tx = self.tx.clone();
        thread::spawn(move || {
            // Let the network and Alice come up first.
            thread::sleep(Duration::from_secs(20));
            for a in list {
                thread::sleep(Duration::from_secs(u64::from(a.delay.min(600))));
                eprintln!("autostart: «{}»", a.phrase);
                let (rtx, rrx) = mpsc::channel();
                if tx.send(Cmd::Control(format!("macro run {}", a.phrase), rtx)).is_err() {
                    break;
                }
                let _ = rrx.recv_timeout(Duration::from_secs(30));
            }
        });
    }

    fn halt(&mut self) {
        // A stop is written at once, so a reboot right after it plays nothing.
        if self.resume_at.is_none() {
            self.resume_saved = None;
        }
        if let Some(f) = self.flags.take() {
            f.stop.store(true, Ordering::Release);
        }
        self.generation += 1;
    }

    fn start(&mut self, pos: usize) {
        self.retries = 0;
        self.start_at(pos, 0, 0);
    }

    /// Starts track `pos`, silently skipping its first `skip_ms` (resume after a
    /// broken download), after waiting `delay_ms`.
    fn start_at(&mut self, pos: usize, skip_ms: u64, delay_ms: u64) {
        self.halt();
        if pos >= self.queue.len() {
            self.publish();
            return;
        }
        self.pos = pos;
        // SoundCloud asked for by voice or the page wins over AirPlay.
        self.shared.airplay.interrupt();
        let flags = player::new_flags();
        self.flags = Some(flags.clone());
        let generation = self.generation;
        let track = self.queue[pos].clone();
        let client = self.client.clone();
        let shared = self.shared.clone();
        let tx = self.tx.clone();
        let device = self.out_device();
        let prefetch = self.client.prefetch.clone();
        eprintln!("play: {} → {device}", track.name());
        // Not again for a resume after a broken download.
        if skip_ms == 0 {
            self.history.track(track.name(), track.permalink_url.as_deref(), track.cover(), track.id);
        }
        let duration_ms = track.duration.unwrap_or(0);
        thread::spawn(move || {
            if delay_ms > 0 {
                thread::sleep(Duration::from_millis(delay_ms));
            }
            let ready = {
                let mut slot = prefetch.lock().unwrap();
                match slot.take() {
                    Some((id, reader, kind)) if id == track.id => Some((reader, kind)),
                    other => {
                        *slot = other;
                        None
                    }
                }
            };
            let opened = match ready {
                Some(r) => Ok(r),
                None => client.open(&track),
            };
            let outcome = match opened {
                Ok((source, kind)) => {
                    player::play(source, kind, &device, &flags, &shared.duck, skip_ms, duration_ms)
                }
                Err(e) => Outcome::Failed(e.to_string()),
            };
            let _ = tx.send(Cmd::Ended(generation, outcome));
        });
        // Open the next track while this one plays, so skipping is instant.
        if let Some(next) = self.queue.get(pos + 1).cloned() {
            let client = self.client.clone();
            let prefetch = self.client.prefetch.clone();
            thread::spawn(move || {
                thread::sleep(Duration::from_secs(3));
                if let Ok((reader, kind)) = client.open(&next) {
                    *prefetch.lock().unwrap() = Some((next.id, reader, kind));
                }
            });
        }
        self.publish();
    }

    /// Jumps within the current track: to `to_ms`, or by `by_ms` from where it is.
    fn seek(&mut self, to_ms: Option<u64>, by_ms: i64) -> Result<u64, String> {
        let Some(flags) = self.flags.clone() else {
            return Err("сейчас ничего не играет".into());
        };
        let now = flags.position_ms.load(Ordering::Acquire);
        let duration = self.current().and_then(|t| t.duration).unwrap_or(0);
        let mut target = to_ms.unwrap_or_else(|| now.saturating_add_signed(by_ms));
        if duration > 0 {
            target = target.min(duration.saturating_sub(2_000));
        }
        flags.seek(target);
        // Shown right away; the player catches up in a moment.
        flags.position_ms.store(target, Ordering::Release);
        self.publish();
        Ok(target)
    }

    fn next(&mut self) {
        if self.pos + 1 < self.queue.len() {
            let p = self.pos + 1;
            self.start(p);
            return;
        }
        // End of queue: continue with related tracks, like SoundCloud's autoplay.
        let Some(last) = self.queue.get(self.pos) else {
            self.halt();
            self.publish();
            return;
        };
        let id = last.id;
        self.halt();
        self.publish();
        let token = self.generation;
        let client = self.client.clone();
        let tx = self.tx.clone();
        thread::spawn(move || {
            let _ = tx.send(Cmd::Related(token, client.related(id)));
        });
    }

    fn on_related(&mut self, token: u64, result: sc::Res<Vec<Track>>) {
        if token != self.generation {
            return; // something else started meanwhile
        }
        match result {
            Ok(more) => {
                let seen: HashSet<u64> = self.queue.iter().map(|t| t.id).collect();
                let mut more: Vec<Track> = more.into_iter().filter(|t| !seen.contains(&t.id)).collect();
                if self.shuffle {
                    self.shuffle_slice(&mut more);
                }
                if !more.is_empty() {
                    self.queue.extend(more);
                    let p = self.pos + 1;
                    self.start(p);
                }
            }
            Err(e) => eprintln!("related: {e}"),
        }
        self.publish();
    }

    fn previous(&mut self) {
        let played = self
            .flags
            .as_ref()
            .map(|f| f.position_ms.load(Ordering::Acquire))
            .unwrap_or(0);
        let p = if played > 5000 || self.pos == 0 { self.pos } else { self.pos - 1 };
        self.start(p);
    }

    fn set_paused(&mut self, paused: bool) {
        match &self.flags {
            Some(f) => {
                if !paused {
                    alice::pause_native();
                    self.shared.airplay.interrupt();
                }
                f.paused.store(paused, Ordering::Release);
                self.publish();
            }
            None if !paused && !self.queue.is_empty() => {
                alice::pause_native();
                let p = self.pos;
                self.start(p);
            }
            None => {}
        }
    }

    fn set_shuffle(&mut self, on: bool) -> String {
        if on {
            self.shuffle = true;
            if self.queue.len() > self.pos + 1 {
                if self.original.is_none() {
                    self.original = Some(self.queue.clone());
                }
                let mut rest = self.queue.split_off(self.pos + 1);
                self.shuffle_slice(&mut rest);
                self.queue.extend(rest);
                self.client.prefetch.lock().unwrap().take();
            }
            "Перемешала".into()
        } else {
            self.shuffle = false;
            if let Some(original) = self.original.take() {
                let current = self.queue.get(self.pos).map(|t| t.id);
                if let Some(i) = current.and_then(|id| original.iter().position(|t| t.id == id)) {
                    self.queue = original;
                    self.pos = i;
                    self.client.prefetch.lock().unwrap().take();
                }
            }
            "Играю по порядку".into()
        }
    }

    /// Likes or unlikes a track in the background; the hearts update when SoundCloud confirms.
    fn set_like(&self, id: u64, on: bool, reply: Option<Sender<String>>) {
        let client = self.client.clone();
        let tx = self.tx.clone();
        thread::spawn(move || match client.set_like(id, on) {
            Ok(()) => {
                let _ = tx.send(Cmd::LikeChanged(id, on));
                let (text, speech) = if on {
                    ("OK: ♥ в лайках", "Добавила в лайки")
                } else {
                    ("OK: лайк убран", "Убрала из лайков")
                };
                respond(reply, text, Some(speech.into()));
            }
            Err(e) => respond(
                reply,
                &format!("error: не получилось: {e}"),
                Some(if on { "Не получилось поставить лайк" } else { "Не получилось убрать лайк" }.into()),
            ),
        });
    }

    fn intent(&mut self, intent: Intent, reply: Option<Sender<String>>) {
        match intent {
            Intent::Likes => {
                self.asked = Some(("Мои лайки".into(), "likes".into()));
                self.fetch("лайки".into(), reply, |c| c.likes())
            }
            Intent::MyPlaylist(name) => {
                self.asked = Some((format!("Мой плейлист «{name}»"), format!("myplaylist {name}")));
                let n = name.clone();
                self.fetch(name, reply, move |c| my_playlist(c, &n));
            }
            Intent::Play(q) => {
                self.asked = Some((format!("Поиск «{q}»"), format!("play {q}")));
                let query = q.clone();
                self.fetch(q, reply, move |c| c.search_tracks(&query));
            }
            Intent::Album(q) => {
                self.asked = Some((format!("Альбом «{q}»"), format!("album {q}")));
                let query = q.clone();
                self.fetch(q, reply, move |c| c.search_album(&query));
            }
            Intent::ToAlice => {
                self.set_paused(true);
                respond(reply, "OK", None);
            }
            Intent::Playlist(q) => {
                // «Включи плейлист X»: the account's own playlists first, then SoundCloud search.
                self.asked = Some((format!("Плейлист «{q}»"), format!("playlist {q}")));
                let query = q.clone();
                let mine = self.client.has_token();
                self.fetch(q, reply, move |c| {
                    if mine {
                        if let Ok(tracks) = my_playlist(c, &query) {
                            if !tracks.is_empty() {
                                return Ok(tracks);
                            }
                        }
                    }
                    c.search_playlist(&query)
                });
            }
            Intent::Recommend => {
                if reply.is_none() && !self.client.has_fresh_recommendations() {
                    alice::say("Подбираю рекомендации, секунду".into());
                }
                let seed = self.current().map(|t| t.id);
                self.asked = Some(("Рекомендации".into(), "recs".into()));
                self.fetch("рекомендации".into(), reply, move |c| c.recommendations(seed));
            }
            Intent::Default => {
                if !self.queue.is_empty() {
                    self.set_paused(false);
                    pause_native_again(&self.shared);
                    respond(reply, "OK", None);
                } else if self.client.has_token() {
                    self.intent(Intent::Likes, reply);
                } else {
                    respond(
                        reply,
                        "error: скажите, что включить",
                        Some("Скажите, что включить. Например: включи Bonobo в саундклауде".into()),
                    );
                }
            }
                        Intent::Shuffled(inner) => {
                self.shuffle = true;
                self.intent(*inner, reply);
            }
            // An alarm's music: «пауза», ▶‖ and the panel's pause do not stop it (alarm.rs).
            Intent::Pause if alarm_music_on() => {
                let text = "Будильник так не выключается. Скажите: Алиса, я проснулся";
                respond(reply, &format!("error: {text}"), Some(text.into()));
            }
            Intent::Pause => {
                self.set_paused(true);
                respond(reply, "OK", None);
            }
            Intent::Alarm(v) => {
                let text = alarm::command(&v);
                respond(reply, &format!("OK: {text}"), Some(text));
            }
            Intent::Resume if self.queue.is_empty() => {
                // Nothing to go back to (soundya was restarted): same as «включи саундклауд».
                eprintln!("voice: nothing to resume; starting likes instead");
                self.intent(Intent::Default, reply);
            }
            Intent::Resume => {
                // «Продолжи» reaches Alice's own player too: her paused Yandex
                // Music would quietly start under SoundCloud. Pause it now and
                // once more when her own answer to the phrase has arrived.
                self.set_paused(false);
                pause_native_again(&self.shared);
                respond(reply, "OK", None);
            }
            Intent::Next => {
                self.next();
                respond(reply, "OK", None);
            }
            Intent::Previous => {
                self.previous();
                respond(reply, "OK", None);
            }
            Intent::Seek(by) => match self.seek(None, by) {
                Ok(_) => respond(reply, "OK", None),
                Err(e) => respond(reply, &format!("error: {e}"), None),
            },
            Intent::Restart => {
                let p = self.pos;
                self.start(p);
                respond(reply, "OK", None);
            }
            Intent::Shuffle(on) => {
                let text = self.set_shuffle(on);
                respond(reply, &format!("OK: {text}"), Some(text));
            }
            Intent::Repeat(on) => {
                self.repeat = on;
                let text = if on { "Буду повторять этот трек" } else { "Повтор выключен" };
                respond(reply, &format!("OK: {text}"), Some(text.into()));
            }
            Intent::Similar => match self.current().map(|t| t.id) {
                Some(id) => self.fetch("похожие".into(), reply, move |c| c.related(id)),
                None => respond(reply, "error: сейчас ничего не играет", None),
            },
            Intent::Pc(action) => {
                // The PC may take a while (or be off): never hold up the music.
                let settings = self.settings.clone();
                thread::spawn(move || match pc::run(&settings, &action) {
                    Ok(text) => respond(reply, &format!("OK: {text}"), Some(text)),
                    Err(text) => respond(reply, &format!("error: {text}"), Some(text)),
                });
            }
            // «Выключи телевизор в спальне», «пульт кухня», «усыпи ноутбук»: that device.
            Intent::On(name, inner) => {
                let tv = matches!(*inner, Intent::Tv(_));
                let list = if tv { self.settings.tvs() } else { self.settings.pcs() };
                let Some(d) = list.into_iter().find(|d| alice::normalize_phrase(&d.name) == name) else {
                    let text = format!("Не нашла устройство {name}. Проверьте имена в настройках, в разделе Устройства");
                    respond(reply, &format!("error: {text}"), Some(text));
                    return;
                };
                // The remote by name: that TV stays the remote's (and the panel's) from now on.
                if tv && matches!(*inner, Intent::Tv(alice::TvAction::RemoteMode(true))) && self.settings.tv_active != d.name {
                    self.settings.tv_active = d.name.clone();
                    let _ = self.settings.save(&self.dir);
                }
                // TV and PC commands take their device from the settings when they start
                // (and work on in their own threads): aimed at this one just for the call.
                let saved = self.settings.clone();
                self.settings = if tv { Settings { tv_active: d.name.clone(), ..saved.with_tv(&d) } } else { saved.with_pc(&d) };
                self.intent(*inner, reply);
                self.settings = saved;
            }
            Intent::Tv(alice::TvAction::RemoteMode(true)) if !self.settings.tv_configured() => {
                let text = "Пульт работает с телевизором, а он ещё не добавлен. Добавьте его на странице колонки, в разделе Устройства";
                respond(reply, &format!("error: {text}"), Some(text.into()));
            }
            Intent::Tv(alice::TvAction::RemoteMode(on)) => {
                alice::tv_remote(on);
                let text = if on {
                    "Режим пульта. Говорите: Алиса, вправо, влево, ок, назад, пауза. Выключить: Алиса, выключи режим пульта"
                } else {
                    "Режим пульта выключен"
                };
                eprintln!("tv: remote mode {on}");
                respond(reply, &format!("OK: {text}"), Some(text.into()));
            }
            Intent::Tv(action) => {
                let host = self.settings.active_tv().map(|t| t.host).unwrap_or_default();
                if host.is_empty() {
                    respond(reply, "error: адрес телевизора не задан в настройках", None);
                    return;
                }
                // ADB (and making the key the first time) takes a moment: off the controller.
                let dir = self.dir.clone();
                thread::spawn(move || match tv::run(&host, &dir, &action) {
                    Ok(json) if action == alice::TvAction::Apps => respond(reply, &json, None),
                    Ok(text) => {
                        // «Ещё», «ниже», «нажми …» without «на проекторе» for the next minute and a half.
                        alice::tv_used(match action {
                            alice::TvAction::Key(k, _) => Some(k),
                            _ => None,
                        });
                        // Short confirmations are not worth Alice's voice for keys.
                        let say = (!matches!(action, alice::TvAction::Key(..) | alice::TvAction::OpenPackage(_))).then(|| text.clone());
                        respond(reply, &format!("OK: {text}"), say);
                    }
                    Err(text) => respond(reply, &format!("error: {text}"), Some(text)),
                });
            }
            Intent::Like | Intent::Unlike | Intent::AddToPlaylist(_) if !WRITES => respond(
                reply,
                &format!("error: {WRITES_OFF}"),
                Some("Лайки и плейлисты можно менять только в приложении SoundCloud".into()),
            ),
            Intent::Like | Intent::Unlike => match self.current().map(|t| t.id) {
                Some(id) => self.set_like(id, intent == Intent::Like, reply),
                None => respond(reply, "error: сейчас ничего не играет", None),
            },
            Intent::AddToPlaylist(name) => match self.current().map(|t| t.id) {
                Some(id) => {
                    let client = self.client.clone();
                    thread::spawn(move || {
                        let result = client.owned_playlists().and_then(|lists| {
                            let p = find_playlist(&lists, &name)
                                .ok_or_else(|| format!("у вас нет плейлиста «{name}»"))?;
                            let pid: u64 = p.id.parse().map_err(|_| "этот плейлист нельзя изменить")?;
                            client.add_to_playlist(pid, id)?;
                            Ok(p.title.clone())
                        });
                        match result {
                            Ok(title) => respond(
                                reply,
                                &format!("OK: добавлено в «{title}»"),
                                Some(format!("Добавила в плейлист {title}")),
                            ),
                            Err(e) => {
                                let e = e.to_string();
                                // Spoken errors stay human: no HTTP codes.
                                let speech = if e.contains("нет плейлиста") || e.contains("уже есть") {
                                    format!("Не получилось: {e}")
                                } else {
                                    "Не получилось добавить в плейлист".to_owned()
                                };
                                respond(reply, &format!("error: {e}"), Some(speech));
                            }
                        }
                    });
                }
                None => respond(reply, "error: сейчас ничего не играет", None),
            },
            Intent::Duck(cmd) => {
                let text = match cmd {
                    alice::DuckCmd::Down => {
                        ducking::manual(Some(f32::from(self.settings.duck_call_level.min(100)) / 100.0));
                        "Приглушила"
                    }
                    alice::DuckCmd::Restore => {
                        ducking::manual(None);
                        "Вернула громкость"
                    }
                    alice::DuckCmd::Auto(on) => {
                        let mut s = self.settings.clone();
                        s.duck_mode = if on { "smart" } else { "off" }.into();
                        let r = self.apply_settings(s);
                        if r.starts_with("error") {
                            respond(reply, &r, None);
                            return;
                        }
                        if on { "Автоматическое приглушение включено" } else { "Автоматическое приглушение выключено" }
                    }
                };
                respond(reply, &format!("OK: {text}"), Some(text.into()));
            }
            Intent::VoiceLevel(by) => {
                let mut s = self.settings.clone();
                s.alice_voice = (i16::from(s.alice_voice) + i16::from(by)).clamp(10, 100) as u8;
                let pct = s.alice_voice;
                let r = self.apply_settings(s);
                let text = if r.starts_with("error") {
                    "Не получилось поменять громкость голоса".to_owned()
                } else if pct >= 100 {
                    "Говорю как обычно".to_owned()
                } else {
                    format!("Хорошо. Мой голос — {pct} процентов от громкости")
                };
                respond(reply, &format!("OK: {text}"), Some(text));
            }
            Intent::TgReply(text) => {
                let say = telegram::reply_draft(&text);
                respond(reply, &format!("OK: {say}"), Some(say));
            }
            Intent::TgConfirm(yes) => {
                let shared = self.shared.clone();
                thread::spawn(move || {
                    let say = telegram::confirm(yes, &shared);
                    respond(reply, &format!("OK: {say}"), Some(say));
                });
            }
            // Google may take a few seconds: not on the controller's thread.
            Intent::Planner(v) => {
                thread::spawn(move || {
                    let start = std::time::Instant::now();
                    let text = planner::handle(v);
                    eprintln!("planner: answered in {} ms (from the phrase: {})", start.elapsed().as_millis(), alice::since_heard());
                    respond(reply, &format!("OK: {text}"), Some(text));
                });
            }
            Intent::UpdateConfirm(yes) => {
                let text = self.shared.updates.confirm_uploaded(yes);
                eprintln!("update: from the app, answer {yes} → {text}");
                respond(reply, &format!("OK: {text}"), Some(text));
            }
            Intent::Timer(v) => {
                let text = timer::command(&v, self.settings.pomo());
                eprintln!("timer: {v:?} → {text}");
                self.share_status();
                respond(reply, &format!("OK: {text}"), Some(text));
            }
            Intent::Address => {
                let text = hello::repeat();
                respond(reply, &format!("OK: {text}"), None);
            }
            Intent::BtOut(on) => {
                let name = self.settings.bt_name.clone();
                let text = if !bt::valid_mac(&self.settings.bt_mac) {
                    "Наушники не выбраны: подключите их на странице колонки, раздел «Bluetooth»".to_owned()
                } else {
                    self.settings.bt_on = on;
                    self.bt_save();
                    match (on, bt::connected(&self.settings.bt_mac)) {
                        (false, _) => "Музыка снова на колонке".to_owned(),
                        (true, true) => format!("Музыка в {name}"),
                        (true, false) => format!("Включите {name}: музыка перейдёт туда, как только они подключатся"),
                    }
                };
                respond(reply, &format!("OK: {text}"), Some(text));
            }
            Intent::WhatPlaying if self.shared.airplay.is_active() => {
                let text = "Сейчас играет музыка с телефона по AirPlay".to_owned();
                respond(reply, &text, Some(text.clone()));
            }
            Intent::WhatPlaying => match self.current() {
                Some(track) => {
                    let text = format!("Сейчас играет {}", spoken(track));
                    respond(reply, &text, Some(text.clone()));
                }
                None => respond(reply, "сейчас ничего не играет", Some("Сейчас ничего не играет".into())),
            },
        }
    }

    /// Re-syncs macros to the shared state and saves settings.
    fn save_macros(&mut self) {
        *self.shared.macros.lock().unwrap() = macro_list(&self.settings);
        if let Err(e) = self.settings.save(&self.dir) {
            eprintln!("macros: not saved: {e}");
        }
    }

    /// Runs a macro's script: one panel command per line; «wait N» pauses N ms.
    /// On its own thread (0.4.41 slept on the controller thread, freezing voice
    /// and the page for the whole script); each command goes through the
    /// controller like one from the page, and the next waits for its answer.
    fn run_macro(&mut self, script: &str) -> Result<(), String> {
        let lines = macro_lines(script)?;
        let tx = self.tx.clone();
        thread::spawn(move || {
            let started = std::time::Instant::now();
            for line in lines {
                if started.elapsed() > Duration::from_secs(120) {
                    eprintln!("macro: stopped after 2 minutes");
                    break;
                }
                if let Some(ms) = line.strip_prefix("wait ").or_else(|| line.strip_prefix("sleep ")) {
                    thread::sleep(Duration::from_millis(ms.trim().parse::<u64>().unwrap_or(0).min(30_000)));
                    continue;
                }
                let (rtx, rrx) = mpsc::channel();
                if tx.send(Cmd::Control(line.clone(), rtx)).is_err() {
                    break;
                }
                let answer = rrx.recv_timeout(Duration::from_secs(60)).unwrap_or_else(|_| "timeout".into());
                eprintln!("macro: {line} -> {answer}");
            }
        });
        Ok(())
    }

    /// The scenario chosen for a pomodoro event («work» | «break» | «done»), if any.
    fn pomodoro_scenario(&mut self, event: &str) {
        let phrase = match event {
            "work" => self.settings.pomo_on_work.clone(),
            "break" => self.settings.pomo_on_break.clone(),
            _ => self.settings.pomo_on_done.clone(),
        };
        if phrase.trim().is_empty() {
            return;
        }
        let key = norm_phrase(&phrase);
        match self.settings.macros.iter().find(|m| norm_phrase(&m.phrase) == key).map(|m| m.script.clone()) {
            Some(script) => {
                // Its own «timer …» steps would start the pomodoro again and again: skipped here.
                let script = without_timer(&script);
                eprintln!("timer: pomodoro {event} → scenario «{phrase}»");
                if let Err(e) = self.run_macro(&script) {
                    eprintln!("timer: scenario «{phrase}»: {e}");
                }
            }
            None => eprintln!("timer: pomodoro {event}: scenario «{phrase}» is gone"),
        }
    }

    fn control(&mut self, line: &str, reply: Sender<String>) {
        let (cmd, arg) = line.trim().split_once(' ').unwrap_or((line.trim(), ""));
        let arg = arg.trim().to_owned();
        let on_off = |a: &str| !matches!(a, "off" | "0" | "false" | "выкл");
        match cmd {
            "play" if arg.starts_with("https://") => {
                // «Поделиться» adds ?utm_source=…: not part of the link.
                let link = clean_link(&arg);
                let name = link_name(&link);
                self.asked = Some((format!("Ссылка: {name}"), format!("play {link}")));
                let l = link.clone();
                self.fetch(format!("ссылку «{name}»"), Some(reply), move |c| c.resolve(&l));
            }
            "play" if !arg.is_empty() => self.intent(Intent::Play(arg), Some(reply)),
            "play" | "resume" => self.intent(Intent::Resume, Some(reply)),
            "playlist" if !arg.is_empty() => self.intent(Intent::Playlist(arg), Some(reply)),
            "album" if !arg.is_empty() => self.intent(Intent::Album(arg), Some(reply)),
            "pause" => self.intent(Intent::Pause, Some(reply)),
            "toggle" => {
                let paused = self.flags.as_ref().is_some_and(|f| f.paused.load(Ordering::Acquire));
                let i = if paused || self.flags.is_none() { Intent::Resume } else { Intent::Pause };
                self.intent(i, Some(reply));
            }
            "next" => self.intent(Intent::Next, Some(reply)),
            "prev" | "previous" => self.intent(Intent::Previous, Some(reply)),
            "shuffle" => self.intent(Intent::Shuffle(on_off(&arg)), Some(reply)),
            "repeat" => self.intent(Intent::Repeat(on_off(&arg)), Some(reply)),
            "like" | "unlike" | "addpl" if !WRITES => send_to(&reply, format!("error: {WRITES_OFF}")),
            "like" | "unlike" if !arg.is_empty() => match arg.parse::<u64>() {
                Ok(id) => self.set_like(id, cmd == "like", Some(reply)),
                Err(_) => send_to(&reply, "error: неверный id трека".into()),
            },
            "like" => self.intent(Intent::Like, Some(reply)),
            "unlike" => self.intent(Intent::Unlike, Some(reply)),
            "addpl" => {
                // "addpl <playlist id> [track id]"; the current track by default.
                let mut parts = arg.split_whitespace();
                let pid = parts.next().and_then(|p| p.parse::<u64>().ok());
                let tid = parts.next().and_then(|t| t.parse::<u64>().ok()).or_else(|| self.current().map(|t| t.id));
                match (pid, tid) {
                    (Some(pid), Some(tid)) => {
                        let client = self.client.clone();
                        thread::spawn(move || {
                            let r = match client.add_to_playlist(pid, tid) {
                                Ok(()) => "OK: добавлено в плейлист".to_owned(),
                                Err(e) => format!("error: {e}"),
                            };
                            let _ = reply.send(r);
                        });
                    }
                    _ => send_to(&reply, "error: нужен плейлист и играющий трек".into()),
                }
            }
            "similar" => self.intent(Intent::Similar, Some(reply)),
            "likes" => self.intent(Intent::Likes, Some(reply)),
            // A track from «Недавно играло»: plays again without a new history line for the link.
            "replay" if arg.starts_with("https://") => {
                let link = clean_link(&arg);
                let l = link.clone();
                self.fetch(format!("«{}»", link_name(&link)), Some(reply), move |c| c.resolve(&l));
            }
            "myplaylist" if !arg.is_empty() => self.intent(Intent::MyPlaylist(arg), Some(reply)),
            // «Недавно играло» on the page; «history clear» empties it.
            // «Недавно играло» → play it all again.
            "history" if arg == "play" => {
                let ids = self.history.recent_ids(100);
                if ids.is_empty() {
                    send_to(&reply, "error: история пуста".into());
                } else {
                    self.asked = Some(("Недавно играло".into(), "history play".into()));
                    self.fetch("«Недавно играло»".into(), Some(reply), move |c| c.by_ids(&ids));
                }
            }
            "history" => {
                if arg == "clear" {
                    self.history.clear();
                }
                send_to(&reply, self.history.json().to_string());
                // Entries from before covers were kept: look them up in the background, a few at a
                // time; the page shows them on its next look at the history.
                let links = self.history.missing_art(40);
                if !links.is_empty() {
                    let client = self.client.clone();
                    let tx = self.tx.clone();
                    thread::spawn(move || {
                        let found: Vec<_> = links
                            .into_iter()
                            .filter_map(|l| client.cover_of(&l).ok().filter(|(a, _)| !a.is_empty()).map(|(a, id)| (l, a, id)))
                            .collect();
                        if !found.is_empty() {
                            let _ = tx.send(Cmd::HistoryArt(found));
                        }
                    });
                }
            }
            "recs" => self.intent(Intent::Recommend, Some(reply)),
            // seek 95 | seek 1:35 (to that point) | seek +30 | seek -10 (by seconds)
            "seek" => {
                let secs = |t: &str| -> Option<f64> {
                    match t.split_once(':') {
                        Some((m, s)) => Some(m.trim().parse::<f64>().ok()? * 60.0 + s.trim().parse::<f64>().ok()?),
                        None => t.trim().parse::<f64>().ok(),
                    }
                };
                let result = if let Some(rest) = arg.strip_prefix('+') {
                    secs(rest).map(|v| self.seek(None, (v * 1000.0) as i64))
                } else if let Some(rest) = arg.strip_prefix('-') {
                    secs(rest).map(|v| self.seek(None, -(v * 1000.0) as i64))
                } else {
                    secs(&arg).filter(|v| *v >= 0.0).map(|v| self.seek(Some((v * 1000.0) as u64), 0))
                };
                match result {
                    Some(Ok(ms)) => send_to(&reply, format!("OK {}", fmt_ms(ms))),
                    Some(Err(e)) => send_to(&reply, format!("error: {e}")),
                    None => send_to(&reply, "usage: seek 95 | seek 1:35 | seek +30 | seek -10".into()),
                }
            }
            // tv ping | do <фраза> | wake | sleep | open <имя> | openpkg <пакет> |
            // close <имя|пакет> | key <home|back|ok…> [N] | apps | search <q> | youtube <q>
            // — from the settings page, the macro editor and the command line.
            // «on <имя>\t<tv …|pc …>»: the panel's «Проверить» for one of several TVs / PCs.
            "on" => {
                let Some((name, line)) = arg.split_once('\t') else {
                    send_to(&reply, "usage: on <имя устройства>\t<tv …|pc …>".into());
                    return;
                };
                let tv = line.trim_start().starts_with("tv ");
                let list = if tv { self.settings.tvs() } else { self.settings.pcs() };
                let Some(d) = list.into_iter().find(|d| d.name == name.trim()) else {
                    send_to(&reply, format!("error: нет устройства «{}» (сначала сохраните настройки)", name.trim()));
                    return;
                };
                let saved = self.settings.clone();
                self.settings = if tv { Settings { tv_active: d.name.clone(), ..saved.with_tv(&d) } } else { saved.with_pc(&d) };
                self.control(line, reply);
                self.settings = saved;
            }
            "tv" => {
                let (what, rest) = arg.split_once(' ').unwrap_or((arg.as_str(), ""));
                let rest = rest.trim();
                let action = match what {
                    "ping" => Some(alice::TvAction::Ping),
                    "wake" | "on" => Some(alice::TvAction::Wake),
                    "sleep" | "off" => Some(alice::TvAction::Sleep),
                    "apps" => Some(alice::TvAction::Apps),
                    "open" if !rest.is_empty() => Some(alice::TvAction::Open(rest.to_owned())),
                    "openpkg" if !rest.is_empty() => Some(alice::TvAction::OpenPackage(rest.to_owned())),
                    "close" if !rest.is_empty() => Some(alice::TvAction::Close(rest.to_owned())),
                    "search" if !rest.is_empty() => Some(alice::TvAction::Search(rest.to_owned())),
                    "youtube" if !rest.is_empty() => Some(alice::TvAction::YouTube(rest.to_owned())),
                    // A scenario's «Нажать на экране»: what is written on it.
                    "tap" if !rest.is_empty() => Some(alice::TvAction::Tap(rest.to_owned())),
                    "first" => Some(alice::TvAction::PlayFirst),
                    "key" => {
                        let (name, n) = rest.split_once(' ').unwrap_or((rest, "1"));
                        alice::TV_KEYS
                            .iter()
                            .find(|(k, _, _)| *k == name)
                            .map(|(_, code, _)| alice::TvAction::Key(code, n.trim().parse::<u8>().unwrap_or(1).clamp(1, 20)))
                    }
                    "do" | "say" => match alice::intent(&format!("{rest} на телевизоре"), false) {
                        Some(Intent::Tv(a)) => Some(a),
                        _ => None,
                    },
                    _ => None,
                };
                match action {
                    Some(a) => self.intent(Intent::Tv(a), Some(reply)),
                    None => send_to(&reply, "usage: tv ping | wake | sleep | apps | open <имя> | openpkg <пакет> | close <имя> | key <кнопка> [N] | tap <надпись> | first | search <q> | youtube <q> | do <фраза>".into()),
                }
            }
            "pc" => {
                let (what, name) = arg.split_once(' ').unwrap_or((arg.as_str(), ""));
                let name = name.trim().to_owned();
                let action = match what {
                    "ping" => Some(alice::PcAction::Ping),
                    "wake" | "on" => Some(alice::PcAction::Wake),
                    "sleep" => Some(alice::PcAction::Sleep),
                    "shutdown" | "off" => Some(alice::PcAction::Shutdown),
                    "restart" => Some(alice::PcAction::Restart),
                    "lock" => Some(alice::PcAction::Lock),
                    "metrics" => Some(alice::PcAction::Metrics),
                    "process" if !name.is_empty() => Some(alice::PcAction::Process(name)),
                    "open" if !name.is_empty() => Some(alice::PcAction::Open(name)),
                    "close" if !name.is_empty() => Some(alice::PcAction::Close(name)),
                    // pc do открой ютуб в профиле работа — as if said to Alice
                    "do" | "say" if !name.is_empty() => Some(alice::PcAction::Command(name)),
                    // From scenarios: «pc click 500 300», «pc key cmd+t».
                    "click" => pc::click_spec(&name).map(alice::PcAction::Click),
                    "key" => pc::key_spec(&name).map(alice::PcAction::Key),
                    _ => None,
                };
                match action {
                    Some(a) => self.intent(Intent::Pc(a), Some(reply)),
                    None => send_to(&reply, "usage: pc ping|wake|sleep|shutdown|restart|lock|open <программа>|close <программа>|do <фраза>|click <x> <y> [double|right]|key <клавиша, например cmd+t>".into()),
                }
            }
            "stop" => {
                self.halt();
                self.publish();
                send_to(&reply, "OK".into());
            }
            // «macro list | add <фраза> ||| <команды> | del <фраза> | run <фраза>»
            "macro" => {
                let (sub, rest) = arg.split_once(' ').unwrap_or((arg.as_str(), ""));
                let rest = rest.trim();
                match sub {
                    "json" => send_to(&reply, serde_json::json!({
                        "macros": self.settings.macros,
                        // Names for the TV apps the saved scenarios open (no TV needed).
                        "tv_names": self.settings.macros.iter()
                            .flat_map(|m| m.script.lines())
                            .filter_map(|l| l.trim().strip_prefix("tv openpkg "))
                            .map(|p| (p.trim().to_owned(), serde_json::Value::from(tv::label(p.trim()))))
                            .collect::<serde_json::Map<_, _>>(),
                        "commands": MACRO_COMMANDS,
                        "tv_keys": alice::TV_KEYS.iter().map(|(k, _, l)| [k, l]).collect::<Vec<_>>(),
                    }).to_string()),
                    // Runs a script from the editor without saving it.
                    "try" => match self.run_macro(rest) {
                        Ok(()) => send_to(&reply, "OK: выполняю".into()),
                        Err(e) => send_to(&reply, format!("error: {e}")),
                    },
                    "list" => {
                        let list = self
                            .settings
                            .macros
                            .iter()
                            .map(|m| format!("{} => {}", m.phrase, m.script.replace('\n', "; ")))
                            .collect::<Vec<_>>()
                            .join("\n");
                        send_to(&reply, if list.is_empty() { "нет макросов".into() } else { list });
                    }
                    "add" => match rest.split_once("|||") {
                        Some((_, script)) if macro_lines(script).is_err() => {
                            send_to(&reply, format!("error: {}", macro_lines(script).unwrap_err()));
                        }
                        Some((phrase, script)) if !phrase.trim().is_empty() && !script.trim().is_empty() => {
                            let phrase = phrase.trim().to_owned();
                            let script = script.trim().to_owned();
                            let key = norm_phrase(&phrase);
                            self.settings.macros.retain(|m| norm_phrase(&m.phrase) != key);
                            self.settings.macros.push(settings::Macro { phrase, script });
                            self.save_macros();
                            send_to(&reply, "OK: макрос сохранён".into());
                        }
                        _ => send_to(&reply, "usage: macro add <фраза> ||| <команды через перенос строки>".into()),
                    },
                    "del" | "delete" | "remove" => {
                        let key = norm_phrase(rest);
                        let before = self.settings.macros.len();
                        self.settings.macros.retain(|m| norm_phrase(&m.phrase) != key);
                        if self.settings.macros.len() == before {
                            send_to(&reply, "макрос не найден".into());
                        } else {
                            self.save_macros();
                            send_to(&reply, "OK: удалён".into());
                        }
                    }
                    "run" => {
                        let key = norm_phrase(rest);
                        let script =
                            self.settings.macros.iter().find(|m| norm_phrase(&m.phrase) == key).map(|m| m.script.clone());
                        match script {
                            Some(script) => match self.run_macro(&script) {
                                Ok(()) => send_to(&reply, "OK".into()),
                                Err(e) => send_to(&reply, format!("error: {e}")),
                            },
                            None => send_to(&reply, "макрос не найден".into()),
                        }
                    }
                    _ => send_to(&reply, "usage: macro list | add <фраза> ||| <команды> | del <фраза> | run <фраза>".into()),
                }
            }
            "queue" => send_to(&reply,
                self.queue
                    .iter()
                    .enumerate()
                    .map(|(i, t)| format!("{}{:>3}. {}", if i == self.pos { ">" } else { " " }, i + 1, t.name()))
                    .collect::<Vec<_>>()
                    .join("\n"),
            ),
            "status" => send_to(&reply, self.status()),
            // Bluetooth output (bt.rs): «bt status | scan | connect MAC | on | off | disconnect | forget MAC».
            "bt" => self.bt_cmd(&arg, reply),
            // soundya's timer (timer.rs): «timer start <секунды> [название] | pomodoro [название] |
            // pause | resume | skip | stop | left | json».
            // «meet test <id группы>»: the page's «Проверить» — a test message with the meeting bot.
            "meet" => {
                let chat = arg.strip_prefix("test").map(str::trim).and_then(|c| c.parse::<i64>().ok());
                let Some(chat) = chat.filter(|c| *c != 0) else {
                    send_to(&reply, "error: нужен id группы, например -1001234567890".into());
                    return;
                };
                let token = self.settings.meet_bot_token.clone();
                thread::spawn(move || {
                    let text = "soundya: проверка — сюда будут приходить ссылки на созвоны";
                    let r = if token.is_empty() { telegram::send_to_chat(0, chat, text) } else { telegram::send_with(&token, chat, text) };
                    send_to(&reply, r.map_or_else(|e| format!("error: Telegram: {e}"), |()| "OK: сообщение отправлено в группу".into()));
                });
            }
            "alarm" => send_to(&reply, alarm::panel(&arg)),
            // «morning run» after the alarm, «morning test» from the panel's «Проверить».
            "morning" => {
                if arg != "run" && arg != "test" {
                    send_to(&reply, "usage: morning test".into());
                    return;
                }
                // Developer build only for now.
                if !cfg!(feature = "dev") || (arg == "run" && !self.settings.morning_on) {
                    send_to(&reply, "error: утро после будильника — только в dev-сборке".into());
                    return;
                }
                morning::run(self.settings.clone(), self.shared.clone(), self.tx.clone(), arg == "test");
                send_to(&reply, "OK: утро началось — слушайте колонку".into());
            }
            "timer" => {
                let (what, rest) = arg.split_once(' ').unwrap_or((arg.as_str(), ""));
                let r = match what {
                    "json" => timer::json().to_string(),
                    "start" => {
                        let (secs, label) = rest.trim().split_once(' ').unwrap_or((rest.trim(), ""));
                        match secs.parse::<u64>() {
                            Ok(n) if (1..=24 * 3600).contains(&n) => format!("OK: {}", timer::start_plain(n, label)),
                            _ => "error: время таймера — от 1 секунды до 24 часов".into(),
                        }
                    }
                    "pomodoro" => format!("OK: {}", timer::start_pomodoro(self.settings.pomo(), rest)),
                    "pause" => format!("OK: {}", timer::command(&timer::Voice::Pause, self.settings.pomo())),
                    "resume" => format!("OK: {}", timer::command(&timer::Voice::Resume, self.settings.pomo())),
                    "skip" => format!("OK: {}", timer::command(&timer::Voice::Skip, self.settings.pomo())),
                    "stop" => format!("OK: {}", timer::command(&timer::Voice::Stop, self.settings.pomo())),
                    "left" => format!("OK: {}", timer::command(&timer::Voice::Left, self.settings.pomo())),
                    _ => "usage: timer start <секунды> [название] | pomodoro | pause | resume | skip | stop | left | json".into(),
                };
                self.share_status();
                send_to(&reply, r);
            }
            // «volume set 6» (the slider), «volume up» / «volume down 3»: Alice's own volume.
            "volume" => {
                let (what, n) = arg.split_once(' ').unwrap_or((arg.as_str(), "1"));
                let n: i32 = n.trim().parse().unwrap_or(1);
                let what = what.to_owned();
                // Up to ten presses with pauses: not on the controller thread.
                thread::spawn(move || {
                    let r = match what.as_str() {
                        "set" => volume::set(n).map(|l| format!("OK: громкость {l}")),
                        "up" | "down" => volume::press(what == "up", n.max(1) as u32).map(|()| "OK".to_owned()),
                        _ => Err("volume set N | up | down".to_owned()),
                    };
                    let _ = reply.send(r.unwrap_or_else(|e| format!("error: {e}")));
                });
            }
            "netcheck" => self.check_net(Some(reply)),
            // The panel's «Проверить»: Alice says a sentence at the chosen voice volume.
            "voicetest" => {
                alice::say("Так звучит мой голос. Музыка при этом остаётся на прежней громкости.".into());
                send_to(&reply, "OK: слушайте".into());
            }
            // «duck down | restore | auto on|off» — the page's buttons, same as the voice commands.
            "duck" => {
                let cmd = match arg.as_str() {
                    "down" => Some(alice::DuckCmd::Down),
                    "restore" | "up" => Some(alice::DuckCmd::Restore),
                    "auto on" => Some(alice::DuckCmd::Auto(true)),
                    "auto off" => Some(alice::DuckCmd::Auto(false)),
                    _ => None,
                };
                match cmd {
                    Some(c) => self.intent(Intent::Duck(c), Some(reply)),
                    None => send_to(&reply, "usage: duck down | restore | auto on | auto off".into()),
                }
            }
            // The home screen's widgets that ask the outside: «widgets json» → {weather, pc}.
            "widgets" => {
                let shared = self.shared.clone();
                let s = self.settings.clone();
                thread::spawn(move || {
                    let weather = match (s.display_lat, s.display_lon) {
                        (Some(la), Some(lo)) => display::home_weather(&shared, la, lo)
                            .map(|mut w| {
                                w["city"] = s.display_city.clone().into();
                                w
                            })
                            .unwrap_or_else(|e| serde_json::json!({ "error": e })),
                        _ => serde_json::json!({ "error": "no city" }),
                    };
                    // Every computer with an address, asked at once (one may be off and slow).
                    let pcs: Vec<serde_json::Value> = if s.pc_on {
                        let list: Vec<_> = s.pcs().into_iter().filter(|d| !d.host.is_empty()).collect();
                        thread::scope(|sc| {
                            let asks: Vec<_> = list
                                .iter()
                                .map(|d| {
                                    let one = s.with_pc(d);
                                    sc.spawn(move || pc::metrics_raw(&one).unwrap_or_else(|e| serde_json::json!({ "error": e })))
                                })
                                .collect();
                            list.iter()
                                .zip(asks)
                                .map(|(d, h)| {
                                    let mut m = h.join().unwrap_or_else(|_| serde_json::json!({ "error": "сбой" }));
                                    m["name"] = d.name.clone().into();
                                    m
                                })
                                .collect()
                        })
                    } else {
                        Vec::new()
                    };
                    let pc = pcs.first().cloned().unwrap_or(serde_json::Value::Null);
                    let _ = reply.send(serde_json::json!({ "weather": weather, "pc": pc, "pcs": pcs }).to_string());
                });
            }
            // «display city <город>»: the weather's place, found by name.
            "display" => {
                let (what, rest) = arg.split_once(' ').unwrap_or((arg.as_str(), ""));
                match what {
                    "city" if !rest.trim().is_empty() => {
                        let shared = self.shared.clone();
                        let tx = self.tx.clone();
                        let name = rest.trim().to_owned();
                        thread::spawn(move || {
                            let r = match display::find_city(&shared, &name) {
                                Ok((n, la, lo)) => {
                                    let (rtx, rrx) = mpsc::channel();
                                    let patch = serde_json::json!({ "display_city": n, "display_lat": la, "display_lon": lo });
                                    let _ = tx.send(Cmd::Settings(patch, rtx));
                                    let _ = rrx.recv_timeout(Duration::from_secs(10));
                                    format!("OK: {n} ({la:.2}, {lo:.2})")
                                }
                                Err(e) => format!("error: {e}"),
                            };
                            let _ = reply.send(r);
                        });
                    }
                    "test" => {
                        display::notify("tg");
                        send_to(&reply, "OK".into());
                    }
                    _ => send_to(&reply, "usage: display city <город> | display test".into()),
                }
            }
            // «tg reply <текст> | tg yes | tg no» — the voice flow from the command line.
            "tg" => {
                let (what, rest) = arg.split_once(' ').unwrap_or((arg.as_str(), ""));
                match what {
                    "reply" if !rest.trim().is_empty() => self.intent(Intent::TgReply(rest.trim().to_owned()), Some(reply)),
                    "yes" => self.intent(Intent::TgConfirm(true), Some(reply)),
                    "no" => self.intent(Intent::TgConfirm(false), Some(reply)),
                    _ => send_to(&reply, "usage: tg reply <текст> | tg yes | tg no".into()),
                }
            }
            // The panel's «Планы»: «plan json», «plan add {json}», «plan done|undone|del <id>».
            "plan" => {
                let (what, rest) = arg.split_once(' ').unwrap_or((arg.as_str(), ""));
                let id = rest.trim().parse::<u64>().ok();
                let r = match (what, id) {
                    ("json", _) => planner::json().to_string(),
                    ("add", _) => match serde_json::from_str::<serde_json::Value>(rest) {
                        Ok(v) if v["title"].as_str().is_some_and(|t| !t.trim().is_empty()) => {
                            let t = planner::add(
                                v["title"].as_str().unwrap_or(""),
                                v["due"].as_i64(),
                                v["repeat"].as_str().unwrap_or(""),
                                v["remind"].as_bool().unwrap_or(v["due"].is_i64()),
                                v["category"].as_str().unwrap_or(""),
                                v["priority"].as_str().unwrap_or("normal"),
                            );
                            if v["profile"] == "work" {
                                // Linking to Google is a network call: not on the controller
                                // (voice and music would wait for it), like «edit» below.
                                thread::spawn(move || send_to(&reply, planner::make_work(t.id)));
                                return;
                            } else {
                                format!("OK: добавлено «{}»", t.title)
                            }
                        }
                        _ => "error: нужен заголовок".into(),
                    },
                    ("done", Some(id)) => if planner::set_done(id, true) { "OK".into() } else { "error: нет такой задачи".into() },
                    ("undone", Some(id)) => if planner::set_done(id, false) { "OK".into() } else { "error: нет такой задачи".into() },
                    ("del", Some(id)) => if planner::delete(id) { "OK".into() } else { "error: нет такой задачи".into() },
                    // The page's pencil: {"id","title","due","repeat","priority"}; a work task's copy in Google changes too.
                    ("edit", _) => {
                        let v: serde_json::Value = serde_json::from_str(rest).unwrap_or_default();
                        match planner::edit(v["id"].as_u64().unwrap_or(0), v["title"].as_str().unwrap_or(""), v["due"].as_i64(),
                                            v["repeat"].as_str().unwrap_or(""), v["priority"].as_str().unwrap_or("normal")) {
                            None => "error: нет такой задачи или пустое название".into(),
                            Some(t) if t.g_id.is_empty() => format!("OK: сохранено «{}»", t.title),
                            Some(t) => {
                                thread::spawn(move || {
                                    let off = planner::offset();
                                    let notes = t.due.map(|d| format!("до {}", planner::hm(d, off))).unwrap_or_default();
                                    let r = calendar::update_task(&t.g_list, &t.g_id, &t.title, t.due.map(|d| planner::local(d, off).0), &notes);
                                    send_to(&reply, r.map_or_else(|e| format!("error: на колонке сохранено, Google не принял: {e}"), |_| format!("OK: сохранено «{}», и в Google", t.title)));
                                });
                                return;
                            }
                        }
                    }
                    // A Google task from the page's pencil: {"list","id","title","day":"YYYY-MM-DD"|null,"time":"HH:MM"|null}.
                    ("gedit", _) => {
                        let v: serde_json::Value = serde_json::from_str(rest).unwrap_or_default();
                        let title = v["title"].as_str().unwrap_or("").trim().to_owned();
                        if title.is_empty() {
                            send_to(&reply, "error: нужно название".into());
                            return;
                        }
                        thread::spawn(move || {
                            let day = v["day"].as_str().and_then(|d| {
                                let p: Vec<i64> = d.split('-').filter_map(|x| x.parse().ok()).collect();
                                (p.len() == 3).then(|| planner::days_from_civil(p[0], p[1] as u32, p[2] as u32))
                            });
                            let notes = v["time"].as_str().and_then(|t| t.split_once(':')).and_then(|(h, m)| Some(format!("до {}:{:02}", h.parse::<u32>().ok()?, m.parse::<u32>().ok()?))).unwrap_or_default();
                            let r = calendar::update_task(v["list"].as_str().unwrap_or(""), v["id"].as_str().unwrap_or(""), &title, day, &notes);
                            send_to(&reply, r.map_or_else(|e| format!("error: {e}"), |_| format!("OK: сохранено «{title}»")));
                        });
                        return;
                    }
                    ("say", _) => match planner::voice(&alice::normalize_phrase(rest)) {
                        Some(v) => {
                            // Google may take a few seconds.
                            thread::spawn(move || send_to(&reply, format!("OK: {}", planner::handle(v))));
                            return;
                        }
                        None => "error: не поняла фразу".into(),
                    },
                    // The page's day card, and «Сказать вслух».
                    ("text" | "speak", _) => {
                        let text = planner::agenda_text(match rest.trim() {
                            "tomorrow" => planner::Ask::Plans(1),
                            "week" => planner::Ask::Week,
                            "deadlines" => planner::Ask::Deadlines,
                            "brief" => planner::Ask::Brief,
                            _ => planner::Ask::Plans(0),
                        }, planner::now(), planner::offset());
                        if what == "speak" {
                            alice::say(text.clone());
                        }
                        format!("OK: {text}")
                    }
                    // Google tasks from the page: «plan gdone <list> <id>», «plan gdel <list> <id>».
                    ("gdone" | "gdel", _) => {
                        let mut it = rest.split_whitespace();
                        let (list, id) = (it.next().unwrap_or("").to_owned(), it.next().unwrap_or("").to_owned());
                        let del = what == "gdel";
                        thread::spawn(move || {
                            let r = if del { calendar::delete_task(&list, &id) } else { calendar::complete_task(&list, &id) };
                            send_to(&reply, r.map_or_else(|e| format!("error: {e}"), |_| "OK".into()));
                        });
                        return;
                    }
                    // New Google task or event from the page: {"title","day","time","event":bool,"minutes"}.
                    ("gadd", _) => {
                        let v: serde_json::Value = serde_json::from_str(rest).unwrap_or_default();
                        let title = v["title"].as_str().unwrap_or("").trim().to_owned();
                        if title.is_empty() {
                            send_to(&reply, "error: нужно название".into());
                            return;
                        }
                        thread::spawn(move || {
                            let off = planner::offset();
                            let day = v["day"].as_str().and_then(|d| {
                                let p: Vec<i64> = d.split('-').filter_map(|x| x.parse().ok()).collect();
                                (p.len() == 3).then(|| planner::days_from_civil(p[0], p[1] as u32, p[2] as u32))
                            });
                            let time = v["time"].as_str().and_then(|t| t.split_once(':')).and_then(|(h, m)| Some(h.parse::<i64>().ok()? * 3600 + m.parse::<i64>().ok()? * 60));
                            let r = if v["event"].as_bool().unwrap_or(false) {
                                match (day, time) {
                                    (Some(d), Some(t)) => {
                                        let start = d * 86_400 + t - off;
                                        calendar::add_event(&title, start, start + v["minutes"].as_i64().unwrap_or(60).clamp(5, 1440) * 60, false)
                                    }
                                    (Some(d), None) => calendar::add_event(&title, d * 86_400 - off, 0, true),
                                    _ => Err("для события нужна дата".into()),
                                }
                            } else {
                                let notes = time.map(|t| format!("до {}:{:02}", t / 3600, t / 60 % 60)).unwrap_or_default();
                                calendar::add_task(&title, day, &notes)
                            };
                            send_to(&reply, r.map_or_else(|e| format!("error: {e}"), |w| format!("OK: добавлено в «{w}»")));
                        });
                        return;
                    }
                    _ => "usage: plan json | add {json} | done|undone|del <id> | edit {json} | say <фраза> | speak today|tomorrow|week|deadlines|brief | gadd {json} | gdone|gdel <list> <id> | gedit {json}".into(),
                };
                send_to(&reply, r);
            }
            // The panel's wizard: «hello start | step N | done», «hello address».
            "hello" => {
                let (what, n) = arg.split_once(' ').unwrap_or((arg.as_str(), ""));
                match what {
                    "start" => hello::start(),
                    "step" => hello::step(n.trim().parse().unwrap_or(1)),
                    "done" => hello::done(),
                    "address" => {
                        self.intent(Intent::Address, Some(reply));
                        return;
                    }
                    _ => {
                        send_to(&reply, "usage: hello start | step N | done | address".into());
                        return;
                    }
                }
                send_to(&reply, "OK".into());
            }
            // «pin set <PIN> | pin off» — from the panel (behind the current PIN) or
            // `soundya pin off` over adb when the PIN is forgotten.
            "pin" => {
                let pin = match arg.split_once(' ') {
                    Some(("set", p)) => p.trim().to_owned(),
                    _ if arg == "off" => String::new(),
                    _ => {
                        send_to(&reply, "usage: pin set <4–12 цифр> | pin off".into());
                        return;
                    }
                };
                if !pin.is_empty() && !(4..=12).contains(&pin.len()) || !pin.chars().all(|c| c.is_ascii_digit()) {
                    send_to(&reply, "error: PIN — от 4 до 12 цифр".into());
                    return;
                }
                self.settings.panel_pin = pin.clone();
                auth::set_pin(&self.dir, &pin);
                let r = match self.settings.save(&self.dir) {
                    Ok(()) if pin.is_empty() => "OK: PIN выключен".to_owned(),
                    Ok(()) => "OK: PIN установлен".to_owned(),
                    Err(e) => format!("error: не сохранилось: {e}"),
                };
                send_to(&reply, r);
            }
            // «ledtest 0|1|2»: the ring test on the page (one raw channel file).
            // Google sign-in: «google url», «google code <адрес из браузера>», «google off».
            "google" => {
                let (what, rest) = arg.split_once(' ').unwrap_or((arg.as_str(), ""));
                match what {
                    "url" => send_to(&reply, if self.settings.g_client_id.is_empty() || self.settings.g_client_secret.is_empty() {
                        "error: сначала сохраните ID и секрет клиента".into()
                    } else {
                        format!("OK {}", google::auth_url(&self.settings.g_client_id))
                    }),
                    "code" => {
                        let (id, secret) = (self.settings.g_client_id.clone(), self.settings.g_client_secret.clone());
                        let (shared, tx, pasted) = (self.shared.clone(), self.tx.clone(), rest.to_owned());
                        thread::spawn(move || {
                            let proxy = shared.proxy.lock().unwrap().clone();
                            let r = sc::build_agent(&proxy).map_err(|e| e.to_string()).and_then(|a| google::exchange(&a, &id, &secret, &pasted));
                            let text = match r {
                                Ok(refresh) => {
                                    let (rtx, rrx) = mpsc::channel();
                                    let _ = tx.send(Cmd::Settings(serde_json::json!({ "g_refresh": refresh, "g_on": true }), rtx));
                                    let _ = rrx.recv_timeout(Duration::from_secs(10));
                                    eprintln!("google: signed in");
                                    "OK: Google подключён".to_owned()
                                }
                                Err(e) => format!("error: {e}"),
                            };
                            send_to(&reply, text);
                        });
                    }
                    "off" => {
                        let mut new = self.settings.clone();
                        new.g_refresh.clear();
                        new.g_on = false;
                        let r = self.apply_settings(new);
                        send_to(&reply, if r.starts_with("error") { r } else { "OK: Google отключён".into() });
                    }
                    _ => send_to(&reply, "usage: google url | code <адрес> | off".into()),
                }
            }
            // «Обновить сейчас» in «Планы» → «Календарь».
            "calendar" => send_to(&reply, if calendar::active() || calendar::google_on() {
                calendar::wake();
                "OK: обновляю календарь".into()
            } else {
                "error: календарь выключен или адрес не задан".into()
            }),
            "ledtest" => send_to(&reply, match arg.parse::<usize>() {
                Ok(c) => led::test(c).unwrap_or_else(|e| format!("error: {e}")),
                Err(_) => "usage: ledtest 0|1|2".into(),
            }),
            // «Удалить soundya» on the settings page: the speaker becomes plain Alice again.
            "uninstall" => {
                let r = if luna::active() {
                    "error: soundya стоит как пакет Luna: удалите его через YAM — yam remove rodyatotu/soundya".into()
                } else {
                    uninstall()
                };
                send_to(&reply, r);
            }
            // «Обход блокировок»: «bypass auto» tries every way and switches the
            // fastest on, «bypass try nfq2» (or «bypass try nfqc <args from a .bat>»)
            // checks the chosen one and switches it on if it works, «bypass off».
            "bypass" => {
                // «Включить без проверки»: «bypass set nfq3» or «bypass set nfqc <args>».
                if let Some(rest) = arg.strip_prefix("set ") {
                    let (id, args) = rest.trim().split_once(' ').unwrap_or((rest.trim(), ""));
                    if id == zapret::CUSTOM && !args.trim().is_empty() {
                        let _ = self.tx.send(Cmd::ZapretArgs(args.trim().to_owned()));
                    }
                    let custom = if id == zapret::CUSTOM && !args.trim().is_empty() { args.trim().to_owned() } else { self.settings.zapret_args.clone() };
                    if bypass_strategy(id).is_none() {
                        send_to(&reply, "error: нет такой стратегии".into());
                        return;
                    }
                    if zapret::is_zapret(id) {
                        if let Err(e) = zapret::start(&self.dir, id, &custom) {
                            send_to(&reply, format!("error: {e}"));
                            return;
                        }
                    }
                    let _ = self.tx.send(Cmd::BypassManual(id.to_owned()));
                    send_to(&reply, format!("OK: включено без проверки: {}. Колонка не будет менять её сама", bypass::name_of(id)));
                    return;
                }
                if arg == "cancel" {
                    if self.shared.bypass_busy.load(Ordering::Acquire) {
                        bypass::CANCEL.store(true, Ordering::Release);
                        send_to(&reply, "OK: останавливаю подбор…".into());
                    } else {
                        send_to(&reply, "OK: подбор не идёт".into());
                    }
                    return;
                }
                if arg == "off" {
                    let _ = self.tx.send(Cmd::Bypass(String::new()));
                    send_to(&reply, "OK: обход выключен, снова через прокси (если задан) или напрямую".into());
                    return;
                }
                if self.shared.bypass_busy.swap(true, Ordering::AcqRel) {
                    send_to(&reply, "error: проверка уже идёт, дождитесь её конца".into());
                    return;
                }
                let (what, rest) = arg.split_once(' ').unwrap_or((arg.as_str(), ""));
                let (id, custom) = match what {
                    "try" => {
                        let (id, args) = rest.split_once(' ').unwrap_or((rest, ""));
                        let custom = if id == zapret::CUSTOM { args.trim().to_owned() } else { self.settings.zapret_args.clone() };
                        (Some(id.to_owned()), custom)
                    }
                    _ => (None, self.settings.zapret_args.clone()),
                };
                if id.as_deref() == Some(zapret::CUSTOM) {
                    if custom.is_empty() {
                        self.shared.bypass_busy.store(false, Ordering::Release);
                        send_to(&reply, "error: вставьте аргументы из .bat в поле".into());
                        return;
                    }
                    let _ = self.tx.send(Cmd::ZapretArgs(custom.clone()));
                }
                let was = self.settings.active_bypass().to_owned();
                let tx = self.tx.clone();
                let dir = self.dir.clone();
                let shared = self.shared.clone();
                shared.bypass_log.lock().unwrap().clear();
                bypass::CANCEL.store(false, Ordering::Release);
                thread::spawn(move || {
                    let log = |line: String| {
                        eprintln!("bypass: {line}");
                        shared.bypass_log.lock().unwrap().push(line);
                    };
                    let found = match &id {
                        Some(id) => bypass::measure(&dir, id, &custom, &mut None, &log).map(|k| (id.clone(), k)),
                        None => bypass::pick(&dir, &custom, &log),
                    };
                    let cancelled = bypass::CANCEL.swap(false, Ordering::AcqRel);
                    let r = match found {
                        _ if cancelled => {
                            zapret::restore(&dir, &was, &custom);
                            "error: подбор отменён — оставлен прежний способ".to_owned()
                        }
                        Some((id, kbps)) if kbps >= bypass::ENOUGH_KBPS => {
                            let _ = tx.send(Cmd::Bypass(id.clone()));
                            if id == "direct" {
                                format!("OK: SoundCloud и так открывается напрямую ({}) — обход не нужен, колонка ходит к нему без прокси. Проверьте: «Алиса, включи …»", mbit(kbps))
                            } else {
                                format!("OK: ✅ включено: {} — {}. Прокси не используется. Проверьте: «Алиса, включи …»", bypass::name_of(&id), mbit(kbps))
                            }
                        }
                        other => {
                            // Back to what worked before the check.
                            zapret::restore(&dir, &was, &custom);
                            match (other, id) {
                                (Some((found, kbps)), _) => format!(
                                    "error: {} открывает SoundCloud только на {kbps:.0} кбит/с — для музыки мало, не включаю. {}",
                                    bypass::name_of(&found),
                                    "Попробуйте другую стратегию или прокси/VPN"
                                ),
                                (None, Some(id)) => format!("error: {} не помогает: SoundCloud не открывается. Попробуйте другую стратегию", bypass::name_of(&id)),
                                (None, None) => "error: ни один способ не прошёл: похоже, провайдер блокирует сами адреса SoundCloud. Тут поможет только прокси или VPN".to_owned(),
                            }
                        }
                    };
                    shared.bypass_log.lock().unwrap().push(r.trim_start_matches("OK: ").trim_start_matches("error: ").to_owned());
                    shared.bypass_busy.store(false, Ordering::Release);
                    let _ = reply.send(r);
                });
            }
            // «update check» / «update install» from the settings page.
            "update" => {
                let shared = self.shared.clone();
                let proxy = self.settings.use_proxy().to_owned();
                let install = arg == "install";
                // «update auto»: the page was opened; ask GitHub unless asked a moment ago.
                if arg == "auto" && shared.updates.fresh(120) {
                    send_to(&reply, "OK".into());
                    return;
                }
                thread::spawn(move || {
                    let r = if install {
                        shared.updates.install(&proxy).map(|m| format!("OK: {m}"))
                    } else {
                        shared.updates.check(&proxy).map(|r| match r {
                            Some(r) => format!("OK: доступна версия {}", r.version),
                            None => format!("OK: стоит последняя версия {}", VERSION),
                        })
                    };
                    let _ = reply.send(r.unwrap_or_else(|e| format!("error: {e}")));
                });
            }
            "json" => send_to(&reply, self.json()),
            "pick" => match arg.parse::<usize>() {
                Ok(i) if i < self.results.len() => {
                    alice::pause_native();
                    self.queue = self.results.clone();
                    self.original = None;
                    self.failures = 0;
                    self.start(i);
                    send_to(&reply, format!("OK {}", self.status()));
                }
                _ => send_to(&reply, "error: результат не найден, повторите поиск".into()),
            },
            "openpl" if !arg.is_empty() => {
                // "openpl <id> [link]"
                let (id, link) = arg.split_once(' ').unwrap_or((arg.as_str(), ""));
                let (id, link) = (id.to_owned(), link.trim().to_owned());
                let label = id.clone();
                if !link.is_empty() {
                    let name = link.trim_end_matches('/').rsplit('/').next().unwrap_or("").replace('-', " ");
                    self.asked = Some((format!("Плейлист «{name}»"), format!("openpl {id} {link}")));
                }
                self.fetch(label, Some(reply), move |c| c.open_playlist(&id, &link));
            }
            "goto" => match arg.parse::<usize>() {
                Ok(i) if i < self.queue.len() => {
                    alice::pause_native();
                    self.start(i);
                    send_to(&reply, "OK".into());
                }
                _ => send_to(&reply, "error: нет такого номера в очереди".into()),
            },
            _ => send_to(&reply,
                "unknown command; use play|playlist|likes|pause|resume|toggle|next|prev|stop|shuffle|repeat|like|similar|status|queue"
                    .into(),
            ),
        }
    }

    fn start_up(mut self, rx: Receiver<Cmd>) {
        self.check_login();
        self.resume_after_restart();
        self.autostart();
        self.run(rx);
    }

    fn run(mut self, rx: Receiver<Cmd>) {
        loop {
            if self.net_checked.is_none_or(|t| t.elapsed() > Duration::from_secs(60)) && !self.net_checking {
                self.net_checked = Some(std::time::Instant::now());
                self.check_net(None);
            }
            self.save_resume();
            // A refused SoundCloud token is asked again now and then: when it
            // works (it was a passing 401), the login comes back by itself.
            // SoundCloud changes its site key (client_id) every day or two: read it ahead, not when a command fails.
            self.client.keep_fresh();
            // SoundCloud's login lives a day now: renewed ahead, and the new pair saved at once
            // (the old refresh token is spent the moment SoundCloud answers).
            self.client.keep_token();
            if let Some((token, refresh)) = self.client.take_renewed() {
                self.settings.token = token;
                self.settings.sc_refresh = refresh;
                if let Err(e) = self.settings.save(&self.dir) {
                    eprintln!("soundcloud: the renewed login is not saved: {e}");
                }
                if self.user.is_none() {
                    self.check_login();
                }
            }
            if self.client.token_refused() && self.token_checked.is_none_or(|t| t.elapsed() > Duration::from_secs(600)) {
                self.token_checked = Some(std::time::Instant::now());
                self.check_login();
            }
            for e in timer::take_events() {
                self.pomodoro_scenario(e);
            }
            let Ok(cmd) = rx.recv_timeout(Duration::from_secs(1)) else {
                self.history.flush(false);
                self.share_status();
                continue;
            };
            match cmd {
                Cmd::Voice(intent) => self.intent(intent, None),
                Cmd::Control(line, reply) => {
                    let (cmd, arg) = line.trim().split_once(' ').unwrap_or((line.trim(), ""));
                    let arg = arg.trim().to_owned();
                    match cmd {
                        // Read-only requests answer the page directly from a worker thread.
                        // «Проверить возможности колонки» (probe.rs): only reads, a few seconds.
                        "probe" => {
                            let dir = self.dir.clone();
                            thread::spawn(move || send_to(&reply, probe::run(&dir).to_string()));
                        }
                        "search" if !arg.is_empty() => {
                            let client = self.client.clone();
                            let tx = self.tx.clone();
                            let liked = self.liked.clone();
                            thread::spawn(move || {
                                let r = match client.search_all(&arg) {
                                    Ok((tracks, lists)) => {
                                        let json = serde_json::json!({
                                            "tracks": tracks.iter().map(|t| serde_json::json!({
                                                "name": t.name(),
                                                "id": t.id,
                                                "liked": liked.contains(&t.id),
                                                "duration_ms": t.duration.unwrap_or(0),
                                            })).collect::<Vec<_>>(),
                                            "playlists": lists,
                                        })
                                        .to_string();
                                        let _ = tx.send(Cmd::Results(tracks));
                                        json
                                    }
                                    Err(e) => serde_json::json!({"error": e.to_string()}).to_string(),
                                };
                                let _ = reply.send(r);
                            });
                        }
                        "myplaylists" => {
                            let client = self.client.clone();
                            thread::spawn(move || {
                                let r = match client.owned_playlists() {
                                    Ok(lists) => serde_json::json!({"playlists": lists}).to_string(),
                                    Err(e) => serde_json::json!({"error": e.to_string()}).to_string(),
                                };
                                let _ = reply.send(r);
                            });
                        }
                        // soundcloud.com's home page shelves for the panel's «Главная».
                        "home" => {
                            let client = self.client.clone();
                            thread::spawn(move || {
                                let r = match client.home() {
                                    Ok(shelves) => serde_json::json!({"shelves": shelves}).to_string(),
                                    Err(e) => serde_json::json!({"error": e.to_string()}).to_string(),
                                };
                                let _ = reply.send(r);
                            });
                        }
                        "library" => {
                            let client = self.client.clone();
                            thread::spawn(move || {
                                let r = match client.library_with_covers() {
                                    Ok(lists) => serde_json::json!({"playlists": lists}).to_string(),
                                    Err(e) => serde_json::json!({"error": e.to_string()}).to_string(),
                                };
                                let _ = reply.send(r);
                            });
                        }
                        _ => self.control(&line, reply),
                    }
                }
                Cmd::Ended(generation, outcome) if generation == self.generation => {
                    let position = self
                        .flags
                        .as_ref()
                        .map(|f| f.position_ms.load(Ordering::Acquire))
                        .unwrap_or(0);
                    self.flags = None;
                    match outcome {
                        Outcome::Finished => {
                            self.failures = 0;
                            self.audio_failures = 0;
                            if self.repeat {
                                let p = self.pos;
                                self.start(p);
                            } else {
                                self.next();
                            }
                        }
                        Outcome::Stopped => self.publish(),
                        Outcome::Seek(target) => {
                            // A jump back: the same track again, silently up to the target.
                            let p = self.pos;
                            let retries = self.retries;
                            self.start_at(p, target, 0);
                            self.retries = retries;
                        }
                        Outcome::Failed(e) if e.starts_with("alsa") => self.audio_failed(&e, position),
                        Outcome::Failed(e) if self.retries < 3 => {
                            // Same track again, from where it stopped, instead of skipping it.
                            self.retries += 1;
                            let resume = position.saturating_sub(2_000);
                            eprintln!(
                                "track failed: {e}; retry {} from {}:{:02}",
                                self.retries,
                                resume / 60_000,
                                resume / 1000 % 60
                            );
                            let p = self.pos;
                            self.start_at(p, resume, 1_500 * u64::from(self.retries));
                        }
                        Outcome::Failed(e) => {
                            eprintln!("track failed: {e}; giving up on this track");
                            self.retries = 0;
                            self.failures += 1;
                            if self.failures >= 5 {
                                eprintln!("too many failures in a row; stopping");
                                alice::say("Не получается включить треки. Проверьте прокси на странице саундя".into());
                                self.halt();
                                self.publish();
                            } else {
                                self.next();
                            }
                        }
                    }
                }
                Cmd::Ended(..) => {}
                Cmd::Loaded { what, result, reply } => self.on_loaded(what, result, reply),
                Cmd::Related(token, result) => self.on_related(token, result),
                Cmd::HistoryArt(found) => {
                    for (link, art, id) in found {
                        self.history.set_art(&link, art, id);
                    }
                    self.history.flush(false);
                }
                Cmd::Settings(patch, reply) => {
                    let r = match self.settings.merged(&patch) {
                        Ok(s) => self.apply_settings(s),
                        Err(e) => format!("error: {e}"),
                    };
                    let _ = reply.send(r);
                }
                Cmd::Results(tracks) => self.results = tracks,
                Cmd::Net(result) => {
                    // The provider's DPI changes and a strategy that worked stops
                    // getting through: after two failed checks in a row (about two
                    // minutes) soundya picks a working one by itself, at most every
                    // 10 minutes.
                    if result.is_ok() || self.settings.active_bypass().is_empty() || self.settings.bypass_manual {
                        self.net_fails = 0;
                    } else {
                        self.net_fails += 1;
                        let due = self.repicked.is_none_or(|t| t.elapsed() > Duration::from_secs(600));
                        if self.net_fails >= 2 && due && !self.shared.bypass_busy.load(Ordering::Acquire) {
                            eprintln!("bypass: {} stopped getting through; picking again", self.settings.active_bypass());
                            self.repicked = Some(std::time::Instant::now());
                            self.net_fails = 0;
                            let (otx, _orx) = mpsc::channel();
                            self.control("bypass auto", otx);
                        }
                    }
                    // SoundCloud answers again (bypass or proxy fixed): a login
                    // check that failed while it did not is done again.
                    if result.is_ok() && self.user.is_none() && self.client.has_token() {
                        self.check_login();
                    }
                    self.net = Some(result);
                    self.net_checking = false;
                    self.net_checked = Some(std::time::Instant::now());
                }
                Cmd::Liked(ids) => self.liked = ids,
                Cmd::LikeChanged(id, on) => {
                    if on {
                        self.liked.insert(id);
                    } else {
                        self.liked.remove(&id);
                    }
                }
                Cmd::User(user) => self.user = user,
                Cmd::ZapretArgs(args) => {
                    self.settings.zapret_args = args;
                    if let Err(e) = self.settings.save(&self.dir) {
                        eprintln!("bypass: settings not saved: {e}");
                    }
                }
                Cmd::Bypass(id) => self.switch_bypass(id, false),
                Cmd::BypassManual(id) => self.switch_bypass(id, true),
                Cmd::AirPlay(true) => {
                    // AirPlay took over: pause SoundCloud and Alice's own music.
                    alice::pause_native();
                    if self.flags.as_ref().is_some_and(|f| !f.paused.load(Ordering::Acquire)) {
                        eprintln!("airplay: pausing SoundCloud");
                        if let Some(f) = &self.flags {
                            f.paused.store(true, Ordering::Release);
                        }
                    }
                    self.publish();
                }
                Cmd::AirPlay(false) => self.publish(),
                Cmd::TestProxy(proxy, reply) => {
                    let proxy = self.real_proxy(&proxy);
                    thread::spawn(move || {
                        let r = match sc::test_proxy(&proxy) {
                            Ok(m) => format!("OK: {m}"),
                            Err(e) => format!("error: {e}"),
                        };
                        let _ = reply.send(r);
                    });
                }
            }
            self.share_status();
        }
    }

    fn share_status(&self) {
        *self.shared.status.lock().unwrap() = self.status();
        *self.shared.status_json.lock().unwrap() = self.json();
        *self.shared.now_track.lock().unwrap() = self.current().cloned();
        let cover = self.current().and_then(Track::cover);
        let changed = {
            let mut now = self.shared.cover.lock().unwrap();
            let changed = *now != cover;
            *now = cover;
            changed
        };
        // Fetched as the track starts: the page shows it at once.
        if changed {
            web::prefetch_cover(self.shared.clone());
        }
    }
}

fn display_widgets(s: &Settings) -> display::Widgets {
    display::Widgets {
        on: s.display_on,
        list: s.display_widgets.clone(),
        secs: u64::from(s.display_secs),
        lat: s.display_lat,
        lon: s.display_lon,
        light: display::Light {
            own: s.display_light_own,
            day: s.display_bright,
            night: s.display_night_bright,
            night_from: s.display_night_from,
            night_to: s.display_night_to,
            tz: s.tz_offset,
        },
    }
}

/// The speaker, for the panel's home page (read once).
/// Planner, Google sign-in and which calendars to read, from the settings.
fn apply_planner(s: &Settings) {
    planner::configure_plan(planner::Plan {
        warn: s.plan_warn,
        due_time: s.plan_due_time,
        brief_on: s.brief_on,
        brief_phrase: s.brief_phrase,
        brief_at: s.brief_at,
        brief_days: s.brief_days.clone(),
        target: s.plan_target_now(),
        check_every: s.plan_check_every,
        check_times: s.plan_check_times,
        work_from: s.work_from,
        work_to: s.work_to,
        work_days: s.work_days.clone(),
        work_mode: s.plan_mode() == "work",
        teams: s.teams.clone(),
        meet_minutes: s.meet_minutes,
        meet_bot: s.meet_bot_token.clone(),
    });
    timer::set_face(s.timer_face);
    google::configure(google::Config { client_id: s.g_client_id.clone(), secret: s.g_client_secret.clone(), refresh: s.g_refresh.clone() });
    calendar::configure_google(s.g_on && !s.g_refresh.is_empty(), &s.g_read, &s.g_write, &s.g_list);
}

fn device_info() -> serde_json::Value {
    static INFO: std::sync::OnceLock<serde_json::Value> = std::sync::OnceLock::new();
    INFO.get_or_init(|| {
        serde_json::json!({
            "model": stats::model(),
            "firmware": report::firmware(),
            "version": VERSION,
            "dev": cfg!(feature = "dev"),
            "display": display::Clock::detect().is_some(),
        })
    })
    .clone()
}

/// An alarm's music stage is on (only the developer build has alarms).
fn alarm_music_on() -> bool {
    alarm::music_on()
}

fn login_state(token: bool, refused: bool, confirmed: bool) -> &'static str {
    match (token, refused, confirmed) {
        (false, ..) => "none",
        (true, true, _) => "refused",
        (true, false, true) => "ok",
        (true, false, false) => "checking",
    }
}

fn state_dir() -> PathBuf {
    PathBuf::from(std::env::var("SOUNDYA_DIR").unwrap_or_else(|_| "/data/opt/soundya".into()))
}

fn client_with(settings: &Settings) -> Client {
    bypass::set(bypass_strategy(settings.active_bypass()));
    let dir = state_dir();
    let cache = fsx::is_dir(&dir).then(|| dir.join("client_id"));
    let client = Client::new(cache, settings.use_proxy());
    client.set_token(&settings.token);
    client.set_refresh(&settings.sc_refresh);
    client
}

fn client() -> Client {
    client_with(&Settings::load(&state_dir()))
}

/// The TVs' (true) and computers' (false) names, for voice.
/// The «Компьютер» menu's choice of what soundya takes for the PC.
fn pc_rules(s: &Settings) -> alice::PcRules {
    alice::PcRules {
        power: s.pc_power,
        apps: s.pc_apps,
        tg: s.pc_tg,
        search: s.pc_search,
        plain: s.pc_plain,
        keep: alice::keep_list(&s.pc_keep),
    }
}

fn device_names(settings: &Settings) -> Vec<(String, bool)> {
    let tvs = settings.tvs().into_iter().map(|d| (d.name, true));
    let pcs = settings.pcs().into_iter().map(|d| (d.name, false));
    tvs.chain(pcs).filter(|(n, _)| !n.trim().is_empty()).collect()
}

/// Every name of every Telegram contact: "мама|маме" -> ["мама", "маме"].
fn contact_names(settings: &Settings) -> Vec<String> {
    settings
        .pc_contacts
        .iter()
        .flat_map(|c| c.names.split('|').map(|n| n.trim().to_lowercase()))
        .filter(|n| !n.is_empty())
        .collect()
}

/// A spoken phrase to match on: as alice.rs normalises what is heard
/// (lowercase, «ё»→«е», no «Алиса,», no end punctuation, no «пожалуйста»).
fn norm_phrase(s: &str) -> String {
    alice::normalize_phrase(s)
}

/// Commands a macro may run: playback, volume, the PC and the TV. Not
/// uninstall, update, bypass, settings or other macros — a spoken phrase
/// must not be able to remove or reconfigure soundya.
const MACRO_COMMANDS: [&str; 23] = [
    "play", "playlist", "album", "likes", "recs", "myplaylist", "openpl", "pause", "resume", "toggle", "next",
    "prev", "previous", "stop", "shuffle", "repeat", "similar", "seek", "volume", "pc", "tv", "goto", "timer",
];

/// «••••••••abcd» for the page; "" stays "".
fn mask_tail(secret: &str) -> String {
    if secret.is_empty() {
        return String::new();
    }
    format!("••••••••{}", secret.chars().rev().take(4).collect::<Vec<_>>().into_iter().rev().collect::<String>())
}

/// «123456789:AA…» as @BotFather gives it.
fn valid_bot_token(t: &str) -> bool {
    let Some((id, key)) = t.split_once(':') else { return false };
    !id.is_empty() && id.chars().all(|c| c.is_ascii_digit()) && key.len() >= 30 && key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// Good enough for an invitation: one «@», a dot in the domain, no spaces.
fn valid_email(e: &str) -> bool {
    let Some((user, domain)) = e.split_once('@') else { return false };
    !user.is_empty() && domain.contains('.') && !domain.starts_with('.') && !domain.ends_with('.') && !e.contains(char::is_whitespace) && !domain.contains('@')
}

/// A scenario run by a pomodoro event, without its «timer …» steps (no loop).
fn without_timer(script: &str) -> String {
    script.lines().filter(|l| l.split_whitespace().next() != Some("timer")).collect::<Vec<_>>().join("\n")
}

/// A script's lines, checked: every one is «wait N» or an allowed command.
fn macro_lines(script: &str) -> Result<Vec<String>, String> {
    let lines: Vec<String> = script
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(str::to_owned)
        .collect();
    if lines.len() > 50 {
        return Err("в сценарии больше 50 строк".into());
    }
    for l in &lines {
        let cmd = l.split_whitespace().next().unwrap_or("");
        if !(cmd == "wait" || cmd == "sleep" || MACRO_COMMANDS.contains(&cmd)) {
            return Err(format!("команда «{cmd}» в сценарии не разрешена (можно: wait, {})", MACRO_COMMANDS.join(", ")));
        }
    }
    Ok(lines)
}

/// (normalized phrase, script) for every macro that has both filled in.
fn macro_list(settings: &Settings) -> Vec<(String, String)> {
    settings
        .macros
        .iter()
        .map(|m| (norm_phrase(&m.phrase), m.script.clone()))
        .filter(|(p, s)| !p.is_empty() && !s.trim().is_empty())
        .collect()
}

/// Keeps sound-card errors on flash (`audio-errors.log`), with the ALSA setup,
/// so they can be diagnosed later even after a reboot.
fn note_audio_error(dir: &std::path::Path, device: &str, e: &str) {
    use std::io::Write;
    let path = dir.join("audio-errors.log");
    let old = fsx::read_string(&path).unwrap_or_default();
    // At most ~16 KB: keep the end.
    let mut text = if old.len() > 16_000 {
        let cut = (old.len() - 8_000..old.len()).find(|&i| old.is_char_boundary(i)).unwrap_or(old.len());
        old[cut..].to_owned()
    } else {
        old
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    text.push_str(&format!("{now} soundya {} device {device:?}: {e}\n", VERSION));
    if !text.contains("--- /etc/asound.conf") {
        let conf = fsx::read_string(std::path::Path::new("/etc/asound.conf")).unwrap_or_else(|| "(none)".into());
        let cards = fsx::read_string(std::path::Path::new("/proc/asound/cards")).unwrap_or_default();
        let ld = std::env::var("LD_LIBRARY_PATH").unwrap_or_default();
        text.push_str(&format!("--- /etc/asound.conf\n{conf}\n--- /proc/asound/cards\n{cards}\n--- LD_LIBRARY_PATH={ld}\n"));
    }
    if let Ok(mut f) = std::fs::File::create(&path) {
        let _ = f.write_all(text.as_bytes());
    }
}

/// soundya's own HTTPS behaviour for a stored bypass id: zapret (nfqws) works
/// below soundya, so soundya itself then just goes straight, without the proxy.
/// A connection that dies in the TLS handshake or times out is the
/// provider's block, not a soundya bug: say what helps.
fn blocked_hint(e: &str) -> String {
    let l = e.to_lowercase();
    if ["tls connection init failed", "os error 11", "timed out", "connection reset", "os error 104"].iter().any(|p| l.contains(p)) {
        format!(
            "SoundCloud не отвечает — похоже, провайдер его блокирует. Включите «Обход блокировок» \
             (Настройки → Соединение → «Подобрать автоматически») или прокси. Подробно: {e}"
        )
    } else {
        e.to_owned()
    }
}

/// Pauses Alice's own player now and a few more times while soundya starts:
/// she pauses Yandex Music to listen to a phrase and turns it back on when her
/// answer is over, which can be after soundya has already started, and then
/// both play at once.
fn pause_native_again(shared: &Arc<Shared>) {
    alice::pause_native();
    let shared = shared.clone();
    thread::spawn(move || {
        // The first track may still be loading: pause her player anyway, the
        // user asked for SoundCloud.
        for wait in [800, 1500, 2500, 4000] {
            thread::sleep(Duration::from_millis(wait));
            if !shared.alice_music() {
                alice::pause_native();
            }
        }
    });
}

/// A soundcloud.com link without the share tracking (?utm_source=… #…).
fn clean_link(link: &str) -> String {
    link.trim().split(['?', '#']).next().unwrap_or("").trim_end_matches('/').to_owned()
}

/// «chvrsi/adabiat» → «chvrsi — adabiat», for the page.
fn link_name(link: &str) -> String {
    let path = link.split("soundcloud.com/").nth(1).unwrap_or(link);
    path.split('/').filter(|p| !p.is_empty()).map(|p| p.replace('-', " ")).collect::<Vec<_>>().join(" — ")
}

fn mbit(kbps: f64) -> String {
    format!("{:.1} Мбит/с", kbps / 1000.0)
}

fn bypass_strategy(id: &str) -> Option<bypass::Strategy> {
    if zapret::is_zapret(id) {
        Some(bypass::Strategy::Direct)
    } else {
        bypass::Strategy::from_id(id)
    }
}

/// Runs packaging/uninstall.sh (built in) in its own session: it stops soundya,
/// this very process, so it must outlive it.
fn uninstall() -> String {
    const SCRIPT: &str = include_str!("../packaging/uninstall.sh");
    let path = "/tmp/soundya-uninstall.sh";
    if let Err(e) = std::fs::write(path, SCRIPT) {
        return format!("error: {e}");
    }
    eprintln!("uninstall: removing soundya");
    let started = std::process::Command::new("sh")
        .arg("-c")
        .arg(format!("(nohup setsid sh -c 'sleep 2; sh {path}' > /tmp/soundya-uninstall.log 2>&1 < /dev/null &) && echo started"))
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).contains("started"))
        .unwrap_or(false);
    if started {
        "OK: soundya удаляется. Через несколько секунд колонка снова будет обычной Алисой".into()
    } else {
        "error: не удалось запустить удаление".into()
    }
}

fn device() -> String {
    std::env::var("SOUNDYA_DEVICE").unwrap_or_else(|_| "default".into())
}

fn daemon() {
    let listener = TcpListener::bind(CONTROL).unwrap_or_else(|e| {
        eprintln!("cannot listen on {CONTROL}: {e} (already running?)");
        std::process::exit(1);
    });
    let dir = state_dir();
    fsx::ensure_dir(&dir);
    let mut settings = Settings::load(&dir);
    // What the installer was told to set up («что поставить»): taken once.
    let features = dir.join("features");
    if let Some(text) = fsx::read_string(&features) {
        if settings.apply_features(&text) {
            eprintln!("install choice: {}", text.trim().replace('\n', ", "));
            if let Err(e) = settings.save(&dir) {
                eprintln!("install choice: settings not saved: {e}");
            }
        }
        let _ = std::fs::remove_file(&features);
    }
    let shared = Arc::new(Shared {
        duck: Duck::new(),
        ring: AtomicBool::new(settings.ring),
        playing: AtomicBool::new(false),
        fetching: AtomicBool::new(false),
        track: Mutex::new(None),
        keyword_only: AtomicBool::new(settings.keyword_only),
        pc: AtomicBool::new(settings.pc_configured()),
        contacts: Mutex::new(contact_names(&settings)),
        macros: Mutex::new(macro_list(&settings)),
        active: AtomicBool::new(false),
        queue: AtomicBool::new(false),
        status: Mutex::new("stopped".into()),
        status_json: Mutex::new(String::new()),
        cover: Mutex::new(None),
        now_track: Mutex::new(None),
        bypass_log: Mutex::new(Vec::new()),
        bypass_busy: AtomicBool::new(false),
        alice_music: AtomicBool::new(false),
        airplay: airplay::AirPlay::new(settings.airplay, &settings.airplay_name, &settings.airplay_icon),
        stats: AtomicBool::new(settings.stats),
        share_heard: AtomicBool::new(settings.share_heard),
        heard: Mutex::new((String::new(), 0)),
        updates: update::Updates::default(),
        proxy: Mutex::new(settings.use_proxy().to_owned()),
        tv: AtomicBool::new(settings.tv_configured()),
        soundcloud: AtomicBool::new(settings.soundcloud),
        light: Mutex::new({
            eq::set(settings.eq_on, &settings.eq);
            if !led::set_order(&settings.led_order) {
                eprintln!("led: unknown channel order {:?}; using rgb", settings.led_order);
            }
            led::Light::from_settings(&settings)
        }),
    });
    let (tx, rx) = mpsc::channel();
    led::run(shared.clone());
    // Starts the sound card if it hangs, so neither soundya nor Alice goes silent.
    alsa::watch(device());
    // http://soundya.local:8765 — works whatever IP the router hands out.
    mdns::run(&std::env::var("SOUNDYA_NAME").unwrap_or_else(|_| "soundya".into()));
    // The speaker in the AirPlay list of iPhones and Macs.
    airplay::run(dir.clone(), device(), tx.clone(), shared.clone());
    // Counts running speakers for the README badge (anonymous, can be switched off).
    stats::run(dir.clone(), shared.clone());
    // Started by Luna: its media provider (pause/resume/next from Luna).
    luna::run(tx.clone(), shared.clone());
    // zapret (nfqws) with the saved strategy, before anything talks to SoundCloud.
    zapret::restore(&dir, settings.active_bypass(), &settings.zapret_args);
    // «Громкость голоса Алисы»: a volume left lowered by a crash comes back first.
    voice::recover();
    voice::configure(settings.alice_voice, settings.alice_voice_max);
    // Telegram bots: messages from allowed people read out, replies by voice.
    telegram::configure(telegram::Config::from_settings(&settings), &shared);
    telegram::run_summary(shared.clone());
    // Tasks and reminders (said only with the planner on).
    planner::configure(settings.planner_on, settings.tz_offset);
    planner::run(&dir);
    timer::run(&dir);
    // The alarm that cannot be slept through.
    alarm::run(&dir, tx.clone());
    adbkeep::configure(settings.adb_keep);
    alice::set_tv(settings.tv_configured());
    alice::set_devices(device_names(&settings));
    alice::set_pc_rules(pc_rules(&settings));
    alarm::set_morning(settings.morning_on);
    alarm::set_own(settings.alarm_own);
    alarm::set_volume(settings.alarm_volume, settings.alarm_louder);
    adbkeep::run();
    calendar::configure(settings.cal_on, &settings.cal_url, settings.cal_remind);
    apply_planner(&settings);
    calendar::run(&dir, shared.clone());
    // Bluetooth output: is the chosen device connected (only while it is switched on).
    bt_watch(tx.clone(), settings.bt_on.then(|| settings.bt_mac.clone()));
    // Optional panel PIN and its saved browser sessions.
    auth::init(&dir, &settings.panel_pin);
    // Smart Ducking: calls and notifications from devices lower soundya's music.
    ducking::configure(ducking::Config::from_settings(&settings));
    ducking::run(shared.clone());
    // Bug reports queued on flash: delivered when the server answers.
    report::run(dir.clone(), shared.clone());
    // Looks for a newer soundya on GitHub; the page offers to install it.
    update::run(shared.clone());
    // A wrong clock breaks every HTTPS site (GitHub first): set it from the web.
    clock::run(shared.clone());
    if std::env::var("SOUNDYA_NO_VOICE").is_err() {
        alice::watch(tx.clone(), shared.clone());
        buttons::run(tx.clone(), shared.clone());
    }
    let port = std::env::var("SOUNDYA_WEB_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(8765);
    // One client for the player and the browser stream: proxy and token
    // changes from the settings reach both (they share their state).
    let client = client_with(&settings);
    // Universal Mod & Plugin Runtime
    mods::init();
    if port != 0 {
        web::serve(port, tx.clone(), shared.clone(), client.clone());
    }
    // After install: Alice says where the panel is (once per version).
    hello::run(&dir, shared.clone(), settings.voice_hello, settings.onboarded, port);
    // The clock display (Mini 3, Plum, Pickle): install steps, then widgets.
    display::configure(display_widgets(&settings), settings.display_bright, &settings);
    display::run();
    display::run_providers(shared.clone());
    let control_tx = tx.clone();
    let status_shared = shared.clone();
    thread::spawn(move || {
        for conn in listener.incoming().flatten() {
            let tx = control_tx.clone();
            let shared = status_shared.clone();
            thread::spawn(move || serve(conn, tx, &shared));
        }
    });
    mark_ready();
    eprintln!(
        "soundya {} running; device {:?}, keyword only: {}, proxy: {}",
        VERSION,
        device(),
        settings.keyword_only,
        if settings.proxy.is_empty() { "none".into() } else { settings.masked_proxy() }
    );
    Controller {
        client,
        shared,
        tx,
        device: device(),
        queue: Vec::new(),
        pos: 0,
        generation: 0,
        flags: None,
        failures: 0,
        audio_failures: 0,
        settings,
        dir,
        results: Vec::new(),
        user: None,
        shuffle: false,
        repeat: false,
        original: None,
        net: None,
        net_checked: None,
        net_checking: false,
        liked: HashSet::new(),
        retries: 0,
        loading: Mutex::new(None),
        history: history::History::load(&state_dir()),
        net_fails: 0,
        repicked: None,
        asked: None,
        token_checked: None,
        source_cmd: None,
        resume_at: None,
        resume_saved: None,
        rng: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0x9E37_79B9)
            | 1,
    }
    .start_up(rx);
}

/// «version pid» in /run/soundya.ready once everything is up: install.sh waits
/// for it after an update and rolls back if it does not come (or the process dies).
fn mark_ready() {
    let run = std::env::var("SOUNDYA_RUN").unwrap_or_else(|_| "/run".into());
    let path = std::path::Path::new(&run).join("soundya.ready");
    let text = format!("{} {}\n", VERSION, std::process::id());
    if let Err(e) = std::fs::write(&path, text) {
        eprintln!("ready: {}: {e}", path.display());
    }
}

fn serve(conn: TcpStream, tx: Sender<Cmd>, shared: &Shared) {
    let _ = conn.set_read_timeout(Some(Duration::from_secs(5)));
    let mut line = String::new();
    let mut reader = BufReader::new(&conn);
    if reader.read_line(&mut line).is_err() {
        return;
    }
    let reply = if line.trim() == "status" {
        // Answer from the cached status so it works even while a search is running.
        shared.status.lock().unwrap().clone()
    } else {
        let (rtx, rrx) = mpsc::channel();
        let _ = tx.send(Cmd::Control(line, rtx));
        rrx.recv_timeout(Duration::from_secs(60))
            .unwrap_or_else(|_| "timeout".into())
    };
    let mut conn = &conn;
    let _ = writeln!(conn, "{reply}");
}

fn send(line: &str) -> i32 {
    match TcpStream::connect(CONTROL) {
        Ok(mut s) => {
            let _ = writeln!(s, "{line}");
            let mut out = String::new();
            let _ = std::io::Read::read_to_string(&mut s, &mut out);
            print!("{out}");
            0
        }
        Err(e) => {
            eprintln!("soundya daemon is not running ({e}); start it with /etc/init.d/S99soundya start");
            1
        }
    }
}

fn main() {
    // Started by shairport-sync as `avahi-publish-service` (see airplay.rs).
    let all: Vec<String> = std::env::args().collect();
    if all.first().is_some_and(|a| a.rsplit('/').next() == Some("avahi-publish-service")) {
        std::process::exit(airplay::publisher_main(&all[1..]));
    }
    // Ctrl+C and kill stop soundya even when it was started from a shell or a
    // supervisor that ignores them (they are inherited as «ignored»).
    {
        extern "C" {
            fn signal(sig: i32, handler: usize) -> usize;
        }
        const SIG_DFL: usize = 0;
        unsafe {
            signal(2, SIG_DFL); // SIGINT
            signal(15, SIG_DFL); // SIGTERM
        }
    }
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (cmd, rest) = match args.split_first() {
        Some((c, r)) => (c.as_str(), r.join(" ")),
        None => ("help", String::new()),
    };
    let code = match cmd {
        "daemon" => {
            daemon();
            0
        }
        "uninstall" => {
            println!("{}", uninstall());
            0
        }
        // install.sh asks every binary this; older ones print the same first line in their help.
        "version" | "--version" | "-V" => {
            println!("soundya {}", VERSION);
            0
        }
        "search" => match client().search_tracks(&rest) {
            Ok(tracks) => {
                for t in tracks {
                    let (tr, kind) = t.transcoding().expect("filtered");
                    println!("{:>12}  {:?} {:<11}  {}", t.id, kind, tr.format.protocol, t.name());
                }
                0
            }
            Err(e) => {
                eprintln!("error: {e}");
                1
            }
        },
        "once" => {
            // Plays the first search result directly, without the daemon (for testing).
            let c = client();
            let result = c.search_tracks(&rest).and_then(|t| {
                let t = t.into_iter().next().ok_or("nothing found")?;
                println!("playing {}", t.name());
                c.open(&t)
            });
            match result {
                Ok((source, kind)) => {
                    let flags = Flags::default();
                    match player::play(source, kind, &device(), &flags, &Duck::new(), 0, 0) {
                        Outcome::Failed(e) => {
                            eprintln!("error: {e}");
                            1
                        }
                        _ => 0,
                    }
                }
                Err(e) => {
                    eprintln!("error: {e}");
                    1
                }
            }
        }
        "play" | "playlist" | "likes" | "pause" | "resume" | "toggle" | "next" | "prev" | "stop"
        | "status" | "queue" | "shuffle" | "repeat" | "like" | "unlike" | "similar" | "recs" | "seek" | "pc" | "tv"
        | "pin" | "macro" | "duck" => send(&format!("{cmd} {rest}")),
        _ => {
            println!(
                "soundya {}\n\
                 \n  soundya daemon              run the player (autostarted)\
                 \n  soundya play <query|link>   search SoundCloud and play\
                 \n  soundya playlist <query>    play a playlist\
                 \n  soundya likes               play your SoundCloud likes (login on the web page)\
                 \n  soundya pause|resume|toggle|next|prev|stop\
                 \n  soundya shuffle on|off | repeat on|off | like | similar\
                 \n  soundya status | queue\
                 \n  soundya seek 1:35 | seek +30 | seek -10\
                 \n  soundya pc ping|wake|sleep|shutdown|restart|lock|open <app>|close <app>\
                 \n  soundya search <query>      list results (no daemon needed)\
                 \n  soundya once <query>        play first result directly (test)\
                 \n\nVoice: «Алиса, включи <исполнитель/трек>», «включи плейлист <…>»,\
                 \n«включи мои лайки», «включи мой плейлист <…>»,\
                 \n«пауза», «продолжи», «следующий трек», «назад», «перемешай»,\
                 \n«поставь на повтор», «включи похожее», «лайкни», «что играет», «включи рекомендации»,\
                 \n«включи/выключи/усыпи компьютер», «открой <программу> на компьютере».",
                VERSION
            );
            0
        }
    };
    std::process::exit(code);
}

#[cfg(test)]
mod net_tests {
    use super::net_level;

    #[test]
    fn music_downloading_is_not_no_connection() {
        // The bug: the last API check failed while music played → «нет связи».
        assert_eq!(net_level(Some(None), Some(900)), "good");
        assert_eq!(net_level(Some(None), Some(300)), "slow");
        assert_eq!(net_level(Some(None), Some(50)), "bad");
        // Nothing plays and the check failed: really no connection.
        assert_eq!(net_level(Some(None), None), "down");
        assert_eq!(net_level(None, None), "unknown");
        assert_eq!(net_level(Some(Some(200)), None), "good");
        assert_eq!(net_level(Some(Some(200)), Some(100)), "bad");
        assert_eq!(net_level(Some(Some(900)), None), "slow");
    }
}

#[cfg(test)]
mod email_tests {
    #[test]
    fn invitations_need_an_address() {
        assert!(super::valid_email("ivan@company.ru"));
        assert!(super::valid_bot_token("123456789:AAHdqTcvCH1vGWJxfSeofSAs0K5PALDsaw"));
        for bad in ["123456789", "abc:AAHdqTcvCH1vGWJxfSeofSAs0K5PALDsaw", "123:short", "123:AAHdqTcvCH1vGWJxfSeofSAs0K5PALDs aw"] {
            assert!(!super::valid_bot_token(bad), "{bad}");
        }
        assert_eq!(super::mask_tail(""), "");
        assert!(super::mask_tail("123456789:AAHdqTcvCH1vGWJxfSeofSAs0K5PALDsaw").ends_with("Dsaw"));
        assert!(!super::mask_tail("123456789:AAHdqTcvCH1vGWJxfSeofSAs0K5PALDsaw").contains("12345"), "the secret stays on the speaker");
        for bad in ["ivan", "ivan@", "@x.ru", "ivan@x", "i van@x.ru", "a@b@c.ru", "ivan@.ru"] {
            assert!(!super::valid_email(bad), "{bad}");
        }
    }
}

#[cfg(test)]
mod macro_tests {
    #[test]
    fn macros_cannot_remove_soundya() {
        assert!(super::macro_lines("tv do открой ютуб\nwait 3000\nvolume set 4").is_ok());
        assert!(super::macro_lines("pc wake\ntimer pomodoro").is_ok(), "«я сажусь работать» may start a pomodoro");
        assert_eq!(super::without_timer("play lofi\ntimer pomodoro\nvolume set 3"), "play lofi\nvolume set 3", "no pomodoro loop");
        for bad in ["uninstall", "update install", "bypass off", "macro run x", "pc do x\nuninstall"] {
            assert!(super::macro_lines(bad).is_err(), "{bad}");
        }
        assert_eq!(super::login_state(true, true, true), "refused");
        assert_eq!(super::login_state(true, false, false), "checking");
        assert_eq!(super::login_state(false, true, false), "none");
        assert_eq!(super::norm_phrase("Алиса, Включи вечерний режим, пожалуйста!"), "включи вечерний режим");
    }
}
