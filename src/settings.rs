//! Settings edited from the web page, stored in <state dir>/config.json.
//!
//! Durability (0.5.00): the file is written to a temporary file, flushed to
//! flash (fsync) and renamed over the old one, and the previous good copy is
//! kept as config.json.bak. A file that cannot be read is never silently
//! replaced by defaults: it is kept as config.json.corrupt and the backup is
//! used instead. Fields written by a newer soundya are kept (`extra`), so a
//! rollback to an older version does not drop them either.

use std::{
    io::Write,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};

/// Version of the config.json layout; see [`Settings::migrate`].
pub const CONFIG_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Settings {
    /// Layout version of the file (0: written before 0.5.00).
    #[serde(default)]
    pub config_version: u32,
    /// "" = direct; "http://user:pass@host:port", "socks5://host:port" or "host:port".
    #[serde(default)]
    pub proxy: String,
    /// Take over only phrases that name SoundCloud.
    #[serde(default)]
    pub keyword_only: bool,
    /// SoundCloud `oauth_token` cookie value; "" = not logged in.
    #[serde(default)]
    pub token: String,
    /// SoundCloud `oauth_refresh_token` cookie: renews the day-long `token`; "" = none.
    #[serde(default)]
    pub sc_refresh: String,
    /// Orange ring light while SoundCloud plays.
    #[serde(default = "yes")]
    pub ring: bool,
    /// Windows PC with soundya-pc: its address ("192.168.1.50" or "host:port").
    #[serde(default)]
    pub pc_host: String,
    /// The key soundya-pc's installer printed.
    #[serde(default)]
    pub pc_token: String,
    /// The PC's wired network card, for switching it on (wake-on-LAN).
    #[serde(default)]
    pub pc_mac: String,
    /// Telegram contacts for «напиши маме в телеграм …».
    #[serde(default)]
    pub pc_contacts: Vec<Contact>,
    /// The name the computer above is called by («ноутбук»); "" = just «компьютер».
    #[serde(default)]
    pub pc_name: String,
    /// More computers, each by its name: «усыпи ноутбук», «выключи компьютер в спальне».
    #[serde(default)]
    pub pcs_more: Vec<PcDevice>,
    /// The name of the TV below («гостиная»); "" = just «телевизор».
    #[serde(default)]
    pub tv_name: String,
    /// More TVs / projectors, each by its name.
    #[serde(default)]
    pub tvs_more: Vec<TvDevice>,
    /// The TV the remote and the commands without a name go to: its name, "" = the first one.
    #[serde(default)]
    pub tv_active: String,
    /// AirPlay receiver on.
    #[serde(default = "yes")]
    pub airplay: bool,
    /// Bluetooth output: the headphones / speaker soundya's music goes to (bt.rs).
    #[serde(default)]
    pub bt_mac: String,
    #[serde(default)]
    pub bt_name: String,
    /// Music to that device now (when it is connected); off: on the speaker.
    #[serde(default)]
    pub bt_on: bool,
    /// Name in the AirPlay list on the iPhone; "" = «Яндекс Станция».
    #[serde(default)]
    pub airplay_name: String,
    /// Picture next to the name: "speaker", "homepod", "homepod-big", "airport", "appletv".
    #[serde(default = "speaker")]
    pub airplay_icon: String,
    /// Anonymous daily «I'm running» ping for the speaker count (src/stats.rs).
    #[serde(default = "yes")]
    pub stats: bool,
    /// The last phrase Alice heard in /api/status (`heard`), for programs on the
    /// PC such as the Ditoo Plus launcher. Off: phrases are private, and the
    /// status is readable by anyone in the home network.
    #[serde(default)]
    pub share_heard: bool,
    /// «Обход блокировок без прокси»: the strategy found by bypass::pick ("" = none yet).
    #[serde(default)]
    pub bypass: String,
    /// The switches on the page: the proxy and the bypass can be turned off
    /// without forgetting the proxy address or the found strategy.
    #[serde(default = "yes")]
    pub proxy_on: bool,
    #[serde(default = "yes")]
    pub bypass_on: bool,
    /// The strategy was chosen by hand («Включить без проверки»): soundya does
    /// not replace it with one it picks itself.
    #[serde(default)]
    pub bypass_manual: bool,
    /// «Подсветка»: "solid" (ровный свет), "pulse" (дышит в такт), "eq"
    /// (эквалайзер по зонам кольца); two colours #rrggbb (quiet → loud) and
    /// brightness 10..=100 %.
    #[serde(default = "light_mode")]
    pub light_mode: String,
    #[serde(default = "light_color")]
    pub light_color: String,
    #[serde(default = "light_color2")]
    pub light_color2: String,
    #[serde(default = "light_bright")]
    pub light_bright: u8,
    /// Animation speed and how strongly the light follows the music, %.
    #[serde(default = "hundred")]
    pub light_speed: u16,
    #[serde(default = "hundred")]
    pub light_intensity: u16,
    /// Equaliser colours: bass, middle, treble.
    #[serde(default = "light_bass")]
    pub light_bass: String,
    #[serde(default = "light_mid")]
    pub light_mid: String,
    #[serde(default = "light_high")]
    pub light_high: String,
    /// Parts switched off at install («что поставить») or later on the page:
    /// SoundCloud by voice, the PC and the TV. AirPlay has `airplay`, the
    /// bypass `bypass_on`.
    #[serde(default = "yes")]
    pub soundcloud: bool,
    /// Equalizer for SoundCloud and AirPlay: on, and dB per band (eq.rs).
    #[serde(default)]
    pub eq_on: bool,
    #[serde(default)]
    pub eq: Vec<i8>,
    #[serde(default = "yes")]
    pub pc_on: bool,
    #[serde(default = "yes")]
    pub tv_on: bool,
    /// zapret arguments pasted from a Windows .bat (strategy "nfqc").
    #[serde(default)]
    pub zapret_args: String,
    /// Android TV / projector with network ADB: "192.168.0.50" or "host:port".
    #[serde(default)]
    pub tv_host: String,
    /// «Искать фильмы в», per TV: its address → the app «найди фильм … на телевизоре»
    /// searches in (package); a TV not here: its own search.
    #[serde(default)]
    pub tv_search: std::collections::BTreeMap<String, String>,
    /// Music service for requests that name none: "soundcloud" (as before
    /// 0.5.00) or "yandex" (Yandex Music; SoundCloud only when it is named).
    #[serde(default = "music_default")]
    pub music_default: String,
    /// The ring's colour channel order as the LED driver expects it: "rgb"
    /// (as before), "grb", "brg", "bgr", "rbg", "gbr". Only for sysfs rings
    /// (Station Lite, Mini 2), set from the ring test on the page.
    #[serde(default = "led_order")]
    pub led_order: String,
    /// Smart Ducking (ducking.rs): "smart", "calls", "notify", "manual", "off".
    #[serde(default = "duck_mode")]
    pub duck_mode: String,
    /// Share of the music left, %: during a call / a notification.
    #[serde(default = "duck_call_level")]
    pub duck_call_level: u8,
    #[serde(default = "duck_notify_level")]
    pub duck_notify_level: u8,
    /// How long a notification keeps the music down, s.
    #[serde(default = "duck_notify_secs")]
    pub duck_notify_secs: u16,
    /// Apps whose events are ignored.
    #[serde(default)]
    pub duck_exclude: Vec<String>,
    /// «Громкость голоса Алисы» (voice.rs): her voice as % of the volume
    /// (100 = as it is) and never above this level of hers (10 = no cap).
    #[serde(default = "hundred")]
    pub alice_voice: u8,
    #[serde(default = "ten")]
    pub alice_voice_max: u8,
    /// Soundya's planner takes «напомни …», «какие планы» from Alice.
    #[serde(default)]
    pub planner_on: bool,
    /// Calendar by its secret iCal address (calendar.rs). The address is a secret.
    #[serde(default)]
    pub cal_on: bool,
    #[serde(default)]
    pub cal_url: String,
    /// Say events this many minutes before; 0 = no reminders.
    #[serde(default = "ten_u16")]
    pub cal_remind: u16,
    /// Planner: warn this many minutes before a deadline (0 = no).
    #[serde(default = "thirty")]
    pub plan_warn: u16,
    /// A deadline with a date only is at this minute of the day (1440 = no time).
    #[serde(default = "eighteen")]
    pub plan_due_time: u16,
    /// Morning brief: on, minute of the day, "all" | "weekdays".
    #[serde(default)]
    pub brief_on: bool,
    /// The brief also when told «Алиса, доброе утро».
    #[serde(default)]
    pub brief_phrase: bool,
    #[serde(default = "eight")]
    pub brief_at: u16,
    #[serde(default = "all_days")]
    pub brief_days: String,
    /// New tasks by voice: "google" (when connected) or "local".
    #[serde(default = "google_target")]
    pub plan_target: String,
    /// «Сделали?» without an answer: again after this many minutes, this many times.
    #[serde(default = "thirty")]
    pub plan_check_every: u16,
    #[serde(default = "three")]
    pub plan_check_times: u8,
    /// Working hours (minutes of the day) and days: "weekdays" | "all".
    #[serde(default = "ten_hours")]
    pub work_from: u16,
    #[serde(default = "nineteen_hours")]
    pub work_to: u16,
    #[serde(default = "weekdays")]
    pub work_days: String,
    /// The planner's mode: "simple" (one list, nothing about work) | "work" (work tasks
    /// apart: «сделали?» in working hours, to Google in «Рабочие — в Google»). "" = an
    /// older settings file: "work" if it chose «Рабочие — в Google», else "simple".
    #[serde(default)]
    pub plan_mode: String,
    /// Pomodoro (timer.rs): minutes of work, break, long break; rounds before the long one.
    #[serde(default = "pomo_work")]
    pub pomo_work: u16,
    #[serde(default = "pomo_break")]
    pub pomo_break: u16,
    #[serde(default = "pomo_long")]
    pub pomo_long: u16,
    #[serde(default = "pomo_rounds")]
    pub pomo_rounds: u8,
    /// Scenarios (macro phrases) run when a pomodoro's work starts, a break starts, it ends.
    #[serde(default)]
    pub pomo_on_work: String,
    #[serde(default)]
    pub pomo_on_break: String,
    #[serde(default)]
    pub pomo_on_done: String,
    /// Teams for «созвон … пригласи всю команду» (Google Meet + Telegram group).
    #[serde(default)]
    pub teams: Vec<Team>,
    /// The bot that posts meeting links (e.g. the team's bot running on a VPS): the
    /// speaker only sends with it (sendMessage), never reads its updates, so it does not
    /// fight the VPS for them. Empty: the speaker's own Telegram bot. A secret.
    #[serde(default)]
    pub meet_bot_token: String,
    /// A meeting without a length said: this many minutes.
    #[serde(default = "thirty")]
    pub meet_minutes: u16,
    /// The panel's home screen: widgets switched off, and their order (empty = the usual one).
    /// Kept on the speaker, so every browser and phone shows the same; chosen in the installer.
    #[serde(default = "home_hidden")]
    pub home_hidden: Vec<String>,
    #[serde(default)]
    pub home_order: Vec<String>,
    /// The panel's own «Пульт» tab (a big TV remote), switched on in «Устройства».
    #[serde(default)]
    pub remote_tab: bool,
    /// The morning after the alarm (morning.rs): on, the greeting, the news feed ("" = none),
    /// an intro track (a SoundCloud link or search), and whether to tell the weather and plans.
    #[serde(default = "yes")]
    pub morning_on: bool,
    /// soundya's own alarm takes «разбуди меня…» (off: Alice's stock alarm does).
    #[serde(default = "yes")]
    pub alarm_own: bool,
    /// The alarm starts at this volume (1–10)…
    #[serde(default = "alarm_volume")]
    pub alarm_volume: i32,
    /// …and gets one louder each round nobody answers.
    #[serde(default = "yes")]
    pub alarm_louder: bool,
    /// «Компьютер» menu: which commands soundya takes for the PC; the rest
    /// stay Alice's (a Home Assistant user: «выключи компьютер» is his automation).
    /// «Включи / выключи / усыпи / перезагрузи компьютер».
    #[serde(default = "yes")]
    pub pc_power: bool,
    /// «Открой / закрой / запусти …» programs and windows.
    #[serde(default = "yes")]
    pub pc_apps: bool,
    /// «Напиши / отправь … в телеграм», messages to the contacts.
    #[serde(default = "yes")]
    pub pc_tg: bool,
    /// «Загугли …», «найди в браузере …», «запиши в заметки …».
    #[serde(default = "yes")]
    pub pc_search: bool,
    /// Commands without «на компьютере» («открой стим»); off: only with the PC named.
    #[serde(default = "yes")]
    pub pc_plain: bool,
    /// Words and phrases always left to Alice, comma separated («окно, выключи компьютер»).
    #[serde(default)]
    pub pc_keep: String,
    #[serde(default = "default_greet")]
    pub morning_greet: String,
    #[serde(default = "default_news")]
    pub morning_news: String,
    #[serde(default)]
    pub morning_intro: String,
    #[serde(default = "yes")]
    pub morning_weather: bool,
    #[serde(default = "yes")]
    pub morning_plans: bool,
    /// Keep the speaker's network ADB (port 5555) on, also after a restart (adbkeep.rs).
    #[serde(default = "yes")]
    pub adb_keep: bool,
    /// soundya's timer on the clock display and the ring.
    #[serde(default = "yes")]
    pub timer_face: bool,
    /// Google Calendar + Tasks with the user's own OAuth client (google.rs).
    /// The secret and the refresh token are secrets.
    #[serde(default)]
    pub g_on: bool,
    #[serde(default)]
    pub g_client_id: String,
    #[serde(default)]
    pub g_client_secret: String,
    #[serde(default)]
    pub g_refresh: String,
    /// Calendars to read (empty = those shown in Google Calendar itself).
    #[serde(default)]
    pub g_read: Vec<String>,
    /// Calendar for new events ("" = primary) and the list for new tasks.
    #[serde(default)]
    pub g_write: String,
    #[serde(default)]
    pub g_list: String,
    /// Local time offset from UTC, minutes (the speaker's clock is UTC).
    #[serde(default = "msk")]
    pub tz_offset: i32,
    /// Telegram on the speaker (telegram.rs): the user's own bots, who may
    /// write, read the text or only who wrote, length, quiet hours (minutes).
    #[serde(default)]
    pub tg_on: bool,
    #[serde(default)]
    pub tg_bots: Vec<crate::telegram::Bot>,
    #[serde(default)]
    pub tg_allowed: Vec<crate::telegram::Sender>,
    #[serde(default = "yes")]
    pub tg_read: bool,
    #[serde(default = "two_hundred")]
    pub tg_max: u16,
    #[serde(default)]
    pub tg_quiet_on: bool,
    #[serde(default = "quiet_from")]
    pub tg_quiet_from: u16,
    #[serde(default = "quiet_to")]
    pub tg_quiet_to: u16,
    /// Smart Display (display.rs, stations with a clock display): widgets in
    /// turn, seconds each, brightness %, the weather's place.
    #[serde(default)]
    pub display_on: bool,
    #[serde(default = "display_widgets")]
    pub display_widgets: Vec<String>,
    #[serde(default = "ten_u16")]
    pub display_secs: u16,
    #[serde(default = "hundred")]
    pub display_bright: u8,
    /// Own brightness by the time of day (display_bright by day, display_night_bright at
    /// night, from..to in minutes); off — as bright as the firmware's clock is.
    #[serde(default)]
    pub display_light_own: bool,
    #[serde(default = "night_bright")]
    pub display_night_bright: u8,
    #[serde(default = "night_from")]
    pub display_night_from: u16,
    #[serde(default = "night_to")]
    pub display_night_to: u16,
    #[serde(default)]
    pub display_city: String,
    #[serde(default)]
    pub display_lat: Option<f64>,
    #[serde(default)]
    pub display_lon: Option<f64>,
    /// Autostart: scenarios (macro phrases) run after start, in this order.
    #[serde(default)]
    pub autostart: Vec<Autostart>,
    /// Music that was playing when soundya stopped (reboot, crash, update)
    /// goes on where it was, if it is under 20 minutes ago.
    #[serde(default)]
    pub resume_music: bool,
    /// Optional PIN for changing anything from the panel ("" = no PIN).
    /// Never sent to the page; `soundya pin off` on the speaker removes it.
    #[serde(default)]
    pub panel_pin: String,
    /// The panel's first-run wizard was gone through (or skipped).
    #[serde(default)]
    pub onboarded: bool,
    /// The «Нас уже 100+ · Сообщество в Telegram» note was seen. Only a fresh install
    /// shows it: a config written before it existed (an update) counts as seen.
    #[serde(default = "yes")]
    pub party_seen: bool,
    /// Say the panel's address after install and the version after an update.
    #[serde(default = "yes")]
    pub voice_hello: bool,
    /// Bug reports: also attach the soundya log (secrets are removed first).
    #[serde(default = "yes")]
    pub report_logs: bool,
    /// Keys this version does not know (written by a newer soundya): kept as is.
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
    /// Voice macros: a phrase that runs a script of panel commands.
    #[serde(default)]
    pub macros: Vec<Macro>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct Autostart {
    /// The scenario's phrase (settings.macros).
    #[serde(default)]
    pub phrase: String,
    /// Seconds after the previous one (or after start).
    #[serde(default)]
    pub delay: u16,
    /// "boot": only after the speaker was switched on; "always": every start.
    #[serde(default)]
    pub when: String,
}

fn default_greet() -> String {
    crate::morning::DEFAULT_GREET.into()
}
fn alarm_volume() -> i32 {
    5
}
fn default_news() -> String {
    "ria".into()
}

/// One more TV or projector (the first is `tv_host` / `tv_name`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct TvDevice {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub host: String,
}

/// One more computer (the first is `pc_host` / `pc_token` / `pc_mac` / `pc_name`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct PcDevice {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub host: String,
    #[serde(default)]
    pub token: String,
    #[serde(default)]
    pub mac: String,
}

/// A TV or computer as voice and the panel pick it: its name and where it is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Device {
    pub name: String,
    pub host: String,
    pub token: String,
    pub mac: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct Contact {
    /// How it is said: "мама|маме|маму".
    #[serde(default)]
    pub names: String,
    /// "@username" or a phone number.
    #[serde(default)]
    pub target: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct Team {
    /// «команда», «маркетинг»: «пригласи команду маркетинг»; the first one is «вся команда».
    #[serde(default)]
    pub name: String,
    /// Invited to the meeting in Google Calendar.
    #[serde(default)]
    pub emails: Vec<String>,
    /// Telegram group the meeting link goes to (0 = none) and its title, by bot number.
    #[serde(default)]
    pub tg_chat: i64,
    #[serde(default)]
    pub tg_chat_name: String,
    #[serde(default)]
    pub tg_bot: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct Macro {
    /// Spoken trigger, e.g. "включи вечерний режим".
    #[serde(default)]
    pub phrase: String,
    /// Script: one panel command per line; «wait N» pauses N ms.
    #[serde(default)]
    pub script: String,
}

fn yes() -> bool {
    true
}

fn light_mode() -> String {
    "solid".into()
}
fn light_color() -> String {
    "#ff7a00".into()
}
fn light_color2() -> String {
    "#ffd000".into()
}
fn light_bright() -> u8 {
    100
}
fn light_bass() -> String {
    "#ff3d00".into()
}
fn light_mid() -> String {
    "#ff9a00".into()
}
fn light_high() -> String {
    "#ffe14d".into()
}

fn speaker() -> String {
    "speaker".into()
}

fn hundred<T: From<u8>>() -> T {
    T::from(100)
}
fn ten() -> u8 {
    10
}

fn display_widgets() -> Vec<String> {
    vec!["clock".into(), "weather".into(), "pc".into()]
}
fn three() -> u8 {
    3
}

fn ten_hours() -> u16 {
    10 * 60
}

fn nineteen_hours() -> u16 {
    19 * 60
}

fn weekdays() -> String {
    "weekdays".into()
}

/// The home widgets the panel knows.
pub const HOME_WIDGETS: [&str; 9] = ["weather", "timer", "plans", "remote", "scenes", "foryou", "sc", "recent", "pc"];

/// The timer is the owner's own tool: off on the home screen unless chosen.
fn home_hidden() -> Vec<String> {
    vec!["timer".into()]
}

fn pomo_work() -> u16 {
    25
}
fn pomo_break() -> u16 {
    5
}
fn pomo_long() -> u16 {
    15
}
fn pomo_rounds() -> u8 {
    4
}

impl Settings {
    /// "work" | "simple", also for a settings file from before the mode existed.
    pub fn plan_mode(&self) -> &'static str {
        match self.plan_mode.as_str() {
            "work" => "work",
            "" if self.plan_target == "work" => "work",
            _ => "simple",
        }
    }

    /// Where new tasks go in this mode: «Рабочие — в Google» only in the work mode
    /// (in the simple one there are no work tasks: those stay on the speaker).
    pub fn plan_target_now(&self) -> String {
        if self.plan_target == "work" && self.plan_mode() == "simple" { "local".into() } else { self.plan_target.clone() }
    }

    pub fn pomo(&self) -> crate::timer::Pomo {
        crate::timer::Pomo { work: self.pomo_work, brk: self.pomo_break, long: self.pomo_long, rounds: self.pomo_rounds }.clamped()
    }
}

fn thirty() -> u16 {
    30
}

fn eighteen() -> u16 {
    18 * 60
}

fn eight() -> u16 {
    8 * 60
}

fn all_days() -> String {
    "all".into()
}

fn google_target() -> String {
    "google".into()
}

fn night_bright() -> u8 {
    20
}

fn night_from() -> u16 {
    22 * 60
}

fn night_to() -> u16 {
    7 * 60
}

fn ten_u16() -> u16 {
    10
}

fn two_hundred() -> u16 {
    200
}
fn quiet_from() -> u16 {
    23 * 60
}
fn quiet_to() -> u16 {
    8 * 60
}

fn msk() -> i32 {
    180
}

fn duck_mode() -> String {
    "smart".into()
}
fn duck_call_level() -> u8 {
    15
}
fn duck_notify_level() -> u8 {
    40
}
fn duck_notify_secs() -> u16 {
    6
}

fn music_default() -> String {
    "soundcloud".into()
}

fn led_order() -> String {
    "rgb".into()
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            proxy: String::new(),
            keyword_only: false,
            token: String::new(),
            sc_refresh: String::new(),
            ring: true,
            pc_host: String::new(),
            pc_token: String::new(),
            pc_mac: String::new(),
            pc_contacts: Vec::new(),
            pc_name: String::new(),
            pcs_more: Vec::new(),
            tv_name: String::new(),
            tvs_more: Vec::new(),
            tv_active: String::new(),
            airplay: true,
            bt_mac: String::new(),
            bt_name: String::new(),
            bt_on: false,
            airplay_name: String::new(),
            airplay_icon: speaker(),
            stats: true,
            share_heard: false,
            tv_host: String::new(),
            tv_search: Default::default(),
            macros: Vec::new(),
            bypass: String::new(),
            proxy_on: true,
            bypass_on: true,
            bypass_manual: false,
            zapret_args: String::new(),
            light_mode: light_mode(),
            light_color: light_color(),
            light_color2: light_color2(),
            light_bright: light_bright(),
            light_speed: 100,
            light_intensity: 100,
            light_bass: light_bass(),
            light_mid: light_mid(),
            light_high: light_high(),
            soundcloud: true,
            eq_on: false,
            eq: Vec::new(),
            pc_on: true,
            tv_on: true,
            config_version: CONFIG_VERSION,
            music_default: music_default(),
            led_order: led_order(),
            report_logs: true,
            duck_mode: duck_mode(),
            duck_call_level: duck_call_level(),
            duck_notify_level: duck_notify_level(),
            duck_notify_secs: duck_notify_secs(),
            duck_exclude: Vec::new(),
            alice_voice: 100,
            alice_voice_max: 10,
            autostart: Vec::new(),
            resume_music: false,
            display_on: false,
            display_widgets: display_widgets(),
            display_secs: 10,
            display_bright: 100,
            display_light_own: false,
            display_night_bright: night_bright(),
            display_night_from: night_from(),
            display_night_to: night_to(),
            display_city: String::new(),
            display_lat: None,
            display_lon: None,
            tg_on: false,
            tg_bots: Vec::new(),
            tg_allowed: Vec::new(),
            tg_read: true,
            tg_max: 200,
            tg_quiet_on: false,
            tg_quiet_from: quiet_from(),
            tg_quiet_to: quiet_to(),
            planner_on: false,
            cal_on: false,
            cal_url: String::new(),
            cal_remind: 10,
            plan_warn: 30,
            plan_due_time: 18 * 60,
            brief_on: false,
            brief_phrase: false,
            brief_at: 8 * 60,
            brief_days: "all".into(),
            plan_target: "google".into(),
            plan_check_every: 30,
            plan_check_times: 3,
            work_from: 10 * 60,
            work_to: 19 * 60,
            work_days: "weekdays".into(),
            plan_mode: "simple".into(),
            pomo_work: 25,
            pomo_break: 5,
            pomo_long: 15,
            pomo_rounds: 4,
            timer_face: true,
            teams: Vec::new(),
            meet_minutes: 30,
            meet_bot_token: String::new(),
            adb_keep: true,
            home_hidden: home_hidden(),
            home_order: Vec::new(),
            remote_tab: false,
            morning_on: true,
            alarm_own: true,
            alarm_volume: alarm_volume(),
            alarm_louder: true,
            pc_power: true,
            pc_apps: true,
            pc_tg: true,
            pc_search: true,
            pc_plain: true,
            pc_keep: String::new(),
            morning_greet: default_greet(),
            morning_news: default_news(),
            morning_intro: String::new(),
            morning_weather: true,
            morning_plans: true,
            pomo_on_work: String::new(),
            pomo_on_break: String::new(),
            pomo_on_done: String::new(),
            g_on: false,
            g_client_id: String::new(),
            g_client_secret: String::new(),
            g_refresh: String::new(),
            g_read: Vec::new(),
            g_write: String::new(),
            g_list: String::new(),
            tz_offset: 180,
            panel_pin: String::new(),
            onboarded: false,
            party_seen: false,
            voice_hello: true,
            extra: serde_json::Map::new(),
        }
    }
}

/// What happened when config.json was read, for the log and bug reports.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Loaded {
    /// Read normally.
    Ok,
    /// No file yet: first start.
    Fresh,
    /// config.json was missing or unreadable; config.json.bak was used.
    FromBackup(String),
    /// Neither could be read: defaults, the unreadable file kept aside.
    Defaults(String),
}

fn parse(text: &str) -> Result<Settings, String> {
    serde_json::from_str::<Settings>(text).map_err(|e| e.to_string())
}

fn path(dir: &Path) -> PathBuf {
    dir.join("config.json")
}

fn backup_path(dir: &Path) -> PathBuf {
    dir.join("config.json.bak")
}

/// Writes a file and flushes it to flash: the speaker is often unplugged.
pub fn write_synced(path: &Path, data: &[u8]) -> std::io::Result<()> {
    let mut f = std::fs::File::create(path)?;
    f.write_all(data)?;
    f.sync_all()
}

fn sync_file(path: &Path) -> std::io::Result<()> {
    std::fs::File::open(path)?.sync_all()
}

/// The rename itself reaches flash only with the directory synced.
fn sync_dir(dir: &Path) {
    if let Ok(d) = std::fs::File::open(dir) {
        let _ = d.sync_all();
    }
}

impl Settings {
    pub fn load(dir: &Path) -> Self {
        let (s, how) = Self::load_checked(dir);
        match &how {
            Loaded::Ok | Loaded::Fresh => {}
            Loaded::FromBackup(why) => eprintln!("settings: config.json unreadable ({why}); restored from config.json.bak"),
            Loaded::Defaults(why) => eprintln!("settings: config.json unreadable ({why}) and no good backup; using defaults, the bad file is kept as config.json.corrupt"),
        }
        s
    }

    /// Reads config.json (or its backup) and migrates it to [`CONFIG_VERSION`].
    pub fn load_checked(dir: &Path) -> (Self, Loaded) {
        let main = path(dir);
        let bak = backup_path(dir);
        let (mut s, how) = match crate::fsx::read_string(&main) {
            Some(text) => match parse(&text) {
                Ok(s) => (s, Loaded::Ok),
                Err(e) => {
                    // Keep the unreadable file for diagnosis; never overwrite it with defaults.
                    let _ = std::fs::rename(&main, dir.join("config.json.corrupt"));
                    match crate::fsx::read_string(&bak).map(|t| parse(&t)) {
                        Some(Ok(s)) => (s, Loaded::FromBackup(e)),
                        _ => (Settings::default(), Loaded::Defaults(e)),
                    }
                }
            },
            // Missing: maybe power went off between the steps of a save.
            None => match crate::fsx::read_string(&bak).map(|t| parse(&t)) {
                Some(Ok(s)) => (s, Loaded::FromBackup("config.json missing".into())),
                _ => (Settings::default(), Loaded::Fresh),
            },
        };
        if how == Loaded::Fresh {
            // First run: take the defaults from soundya.env.
            s.keyword_only = std::env::var("SOUNDYA_KEYWORD_ONLY").is_ok_and(|v| v == "1");
            s.proxy = std::env::var("SOUNDYA_PROXY").unwrap_or_default();
            s.config_version = CONFIG_VERSION;
        }
        if s.config_version < CONFIG_VERSION {
            // A copy of the file as the old version left it, once per version.
            let keep = dir.join(format!("config.v{}.json", s.config_version));
            if !crate::fsx::exists(&keep) {
                if let Some(text) = crate::fsx::read_string(&main) {
                    let _ = write_synced(&keep, text.as_bytes());
                }
            }
            let from = s.config_version;
            s.migrate();
            eprintln!("settings: config.json migrated from layout {from} to {CONFIG_VERSION}");
        }
        (s, how)
    }

    /// Brings an older layout up to date. Every step keeps what the user set.
    pub fn migrate(&mut self) {
        if self.config_version < 1 {
            // 0 → 1 (0.5.00): new fields get their defaults from serde; the
            // music service follows the old «только по слову саундклауд»
            // switch, which stays as it was.
            self.music_default = if self.keyword_only { "yandex" } else { "soundcloud" }.into();
            // Already set up by hand before 0.5.00: no first-run wizard.
            self.onboarded = true;
            self.config_version = 1;
        }
        // Written by a newer soundya: left as it is (unknown keys are kept in `extra`).
    }

    /// Writes config.json durably: temporary file, fsync, the old file kept as
    /// config.json.bak, rename.
    pub fn save(&self, dir: &Path) -> std::io::Result<()> {
        let data = serde_json::to_vec_pretty(self).map_err(std::io::Error::other)?;
        let tmp = dir.join("config.json.tmp");
        write_synced(&tmp, &data)?;
        let main = path(dir);
        // The current file is good (it was read or written by us): keep it as the backup.
        if crate::fsx::read_string(&main).is_some_and(|t| parse(&t).is_ok()) {
            let _ = crate::fsx::copy(&main, &backup_path(dir)).and_then(|_| sync_file(&backup_path(dir)));
        }
        std::fs::rename(&tmp, &main)?;
        sync_dir(dir);
        Ok(())
    }

    /// Applies a partial update from the page: fields it does not send keep
    /// their current values (before 0.5.00 they fell back to defaults).
    pub fn merged(&self, patch: &serde_json::Value) -> Result<Settings, String> {
        let mut base = serde_json::to_value(self).map_err(|e| e.to_string())?;
        let (Some(obj), Some(p)) = (base.as_object_mut(), patch.as_object()) else {
            return Err("ожидается объект JSON".into());
        };
        for (k, v) in p {
            // Never from the page.
            if k == "config_version" {
                continue;
            }
            obj.insert(k.clone(), v.clone());
        }
        serde_json::from_value(base).map_err(|e| e.to_string())
    }

    /// Token hidden except its last 4 characters.
    pub fn masked_token(&self) -> String {
        mask(&self.token)
    }

    pub fn masked_sc_refresh(&self) -> String {
        mask(&self.sc_refresh)
    }

    pub fn masked_pc_token(&self) -> String {
        mask(&self.pc_token)
    }

    /// Every TV: the first (`tv_host`) and the others; only those with an address.
    pub fn tvs(&self) -> Vec<Device> {
        let first = Device { name: self.tv_name.trim().to_owned(), host: self.tv_host.trim().to_owned(), token: String::new(), mac: String::new() };
        std::iter::once(first)
            .chain(self.tvs_more.iter().map(|t| Device { name: t.name.trim().to_owned(), host: t.host.trim().to_owned(), token: String::new(), mac: String::new() }))
            .filter(|d| !d.host.is_empty())
            .collect()
    }

    /// Every computer: the first (`pc_*`) and the others; those with an address or a MAC.
    pub fn pcs(&self) -> Vec<Device> {
        let first = Device { name: self.pc_name.trim().to_owned(), host: self.pc_host.trim().to_owned(), token: self.pc_token.trim().to_owned(), mac: self.pc_mac.trim().to_owned() };
        std::iter::once(first)
            .chain(self.pcs_more.iter().map(|p| Device { name: p.name.trim().to_owned(), host: p.host.trim().to_owned(), token: p.token.trim().to_owned(), mac: p.mac.trim().to_owned() }))
            .filter(|d| !d.host.is_empty() || !d.mac.is_empty())
            .collect()
    }

    /// The TV the remote works with: the chosen one, else the first.
    pub fn active_tv(&self) -> Option<Device> {
        let tvs = self.tvs();
        tvs.iter().find(|t| !self.tv_active.is_empty() && t.name == self.tv_active).cloned().or_else(|| tvs.into_iter().next())
    }

    /// The same settings aimed at another TV or computer: what `tv::run` / `pc::run` use.
    pub fn with_tv(&self, d: &Device) -> Settings {
        Settings { tv_host: d.host.clone(), tv_name: d.name.clone(), ..self.clone() }
    }
    pub fn with_pc(&self, d: &Device) -> Settings {
        Settings { pc_host: d.host.clone(), pc_token: d.token.clone(), pc_mac: d.mac.clone(), pc_name: d.name.clone(), ..self.clone() }
    }

    /// The extra computers for the page: their keys masked like the first one's.
    pub fn masked_pcs_more(&self) -> Vec<PcDevice> {
        self.pcs_more.iter().map(|p| PcDevice { token: mask(&p.token), ..p.clone() }).collect()
    }

    /// The proxy in use: "" when switched off.
    pub fn use_proxy(&self) -> &str {
        if self.proxy_on { &self.proxy } else { "" }
    }

    /// The bypass strategy in use: "" when switched off or not found yet.
    pub fn active_bypass(&self) -> &str {
        if self.bypass_on { &self.bypass } else { "" }
    }

    /// Commands can go to the PC (switching on needs only the MAC address).
    pub fn pc_configured(&self) -> bool {
        self.pc_on && !self.pcs().is_empty()
    }

    pub fn tv_configured(&self) -> bool {
        self.tv_on && !self.tvs().is_empty()
    }

    /// The installer's choice (/data/opt/soundya/features, «airplay=0» per line),
    /// taken once: true when something was applied.
    pub fn apply_features(&mut self, text: &str) -> bool {
        let mut any = false;
        for line in text.lines() {
            let Some((k, v)) = line.split_once('=') else { continue };
            let on = v.trim() == "1";
            match k.trim() {
                "soundcloud" => self.soundcloud = on,
                "airplay" => self.airplay = on,
                "pc" => self.pc_on = on,
                "tv" => self.tv_on = on,
                "bypass" => {
                    if !on {
                        self.bypass_on = false;
                    }
                }
                // «Что показывать на главной»: the widgets chosen, in their order.
                "home" => {
                    let shown: Vec<String> = v.split(',').map(|w| w.trim().to_owned()).filter(|w| HOME_WIDGETS.contains(&w.as_str())).collect();
                    self.home_hidden = HOME_WIDGETS.iter().filter(|w| !shown.iter().any(|s| s == *w)).map(|w| w.to_string()).collect();
                    self.home_order = shown;
                }
                _ => continue,
            }
            any = true;
        }
        any
    }

    /// Keeps «Музыка по умолчанию» and the older «только по слову саундклауд»
    /// switch in step: whichever of the two the page changed wins.
    pub fn sync_music(&mut self, old: &Settings) {
        if !matches!(self.music_default.as_str(), "soundcloud" | "yandex") {
            self.music_default = old.music_default.clone();
        }
        if self.music_default != old.music_default {
            self.keyword_only = self.music_default == "yandex";
        } else if self.keyword_only != old.keyword_only {
            self.music_default = if self.keyword_only { "yandex" } else { "soundcloud" }.into();
        }
    }

    /// Proxy with the password hidden, for showing on the page.
    pub fn masked_proxy(&self) -> String {
        let p = &self.proxy;
        let Some(scheme_end) = p.find("://").map(|i| i + 3) else {
            return p.clone();
        };
        let Some(at) = p[scheme_end..].find('@').map(|i| i + scheme_end) else {
            return p.clone();
        };
        match p[scheme_end..at].find(':') {
            Some(colon) => format!("{}:••••{}", &p[..scheme_end + colon], &p[at..]),
            None => p.clone(),
        }
    }
}

/// A secret hidden except its last 4 characters.
fn mask(secret: &str) -> String {
    let t = secret.trim();
    if t.is_empty() {
        return String::new();
    }
    let tail: String = t.chars().rev().take(4).collect::<Vec<_>>().into_iter().rev().collect();
    format!("••••••••{tail}")
}

#[cfg(test)]
mod tests {
    #[test]
    fn autostart_is_off_by_default() {
        // An old config without the new keys: nothing autostarts, nothing resumes.
        let s: super::Settings = serde_json::from_str(r#"{"device":"default"}"#).unwrap();
        assert!(s.autostart.is_empty());
        assert!(!s.resume_music);
        let s: super::Settings =
            serde_json::from_str(r#"{"autostart":[{"phrase":"доброе утро","delay":5,"when":"boot"}],"resume_music":true}"#).unwrap();
        assert_eq!(s.autostart[0].phrase, "доброе утро");
        assert_eq!(s.autostart[0].when, "boot");
        assert!(s.resume_music);
    }

    use super::*;

    #[test]
    fn the_planner_mode_survives_an_old_settings_file() {
        // Before the mode: «Рабочие — в Google» meant the work mode.
        let s: Settings = serde_json::from_str(r#"{"plan_target":"work"}"#).unwrap();
        assert_eq!((s.plan_mode(), s.plan_target_now().as_str()), ("work", "work"));
        let s: Settings = serde_json::from_str(r#"{"plan_target":"google"}"#).unwrap();
        assert_eq!(s.plan_mode(), "simple");
        // Switched to the simple one: no work tasks, so nothing «work» goes to Google.
        let s: Settings = serde_json::from_str(r#"{"plan_target":"work","plan_mode":"simple"}"#).unwrap();
        assert_eq!((s.plan_mode(), s.plan_target_now().as_str()), ("simple", "local"));
        assert_eq!(s.pomo(), crate::timer::Pomo { work: 25, brk: 5, long: 15, rounds: 4 });
        assert!(s.timer_face);
    }

    #[test]
    fn the_home_screen_chosen_in_the_installer() {
        let mut s = Settings::default();
        assert_eq!(s.home_hidden, vec!["timer"], "the timer only when asked for");
        assert!(s.apply_features("home=plans,timer,recent\n"));
        assert_eq!(s.home_order, vec!["plans", "timer", "recent"]);
        assert_eq!(s.home_hidden, vec!["weather", "remote", "scenes", "foryou", "sc", "pc"], "everything not chosen");
        let old: Settings = serde_json::from_str("{}").unwrap();
        assert_eq!(old.home_hidden, vec!["timer"]);
    }

    #[test]
    fn installer_choice() {
        let mut s = Settings::default();
        assert!(s.apply_features("soundcloud=1\nairplay=0\npc=0\ntv=1\nbypass=0\n"));
        assert!(s.soundcloud && !s.airplay && !s.pc_on && s.tv_on && !s.bypass_on);
        s.pc_host = "192.168.0.10".into();
        assert!(!s.pc_configured());
        assert!(!Settings::default().apply_features("junk"));
    }

    fn tmpdir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("soundya-settings-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn old_config_is_migrated_without_losing_anything() {
        let d = tmpdir("migrate");
        // A 0.4.40 file: no config_version, no new fields.
        let old = r##"{"proxy":"socks5://a:b@1.2.3.4:1080","keyword_only":true,"token":"2-1-x","ring":false,
            "pc_host":"192.168.0.5","pc_token":"k","pc_mac":"","pc_contacts":[{"names":"мама|маме","target":"@m"}],
            "airplay":false,"airplay_name":"Кухня","airplay_icon":"homepod","stats":false,"bypass":"nfq2",
            "proxy_on":false,"bypass_on":true,"bypass_manual":true,"light_mode":"eq","light_color":"#112233",
            "light_color2":"#445566","light_bright":40,"light_bass":"#010101","light_mid":"#020202",
            "light_high":"#030303","soundcloud":true,"eq_on":true,"eq":[1,2,3,4,5,6,7,8,9,10],"pc_on":false,
            "tv_on":true,"zapret_args":"--x","tv_host":"192.168.0.9"}"##;
        std::fs::write(d.join("config.json"), old).unwrap();
        let (s, how) = Settings::load_checked(&d);
        assert_eq!(how, Loaded::Ok);
        assert_eq!(s.config_version, CONFIG_VERSION);
        assert_eq!(s.token, "2-1-x");
        assert!(s.keyword_only && !s.ring && !s.airplay && !s.stats && s.bypass_manual && s.eq_on && !s.pc_on);
        assert_eq!(s.eq, vec![1, 2, 3, 4, 5, 6, 7, 8, 9, 10]);
        assert_eq!(s.pc_contacts[0].target, "@m");
        assert_eq!(s.airplay_name, "Кухня");
        assert_eq!(s.music_default, "yandex", "follows «только по слову саундклауд»");
        assert_eq!(s.led_order, "rgb");
        assert!(s.onboarded, "no wizard for existing users");
        assert!(!Settings::default().onboarded, "wizard on a fresh install");
        assert!(s.party_seen, "no Telegram community note after an update");
        assert!(!Settings::default().party_seen, "the note on a fresh install");
        assert!(!s.share_heard && !Settings::default().share_heard, "Alice's phrases are not shared unless allowed");
        // The old file is kept as it was.
        assert!(crate::fsx::read_string(&d.join("config.v0.json")).unwrap().contains("2-1-x"));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn unreadable_config_is_not_replaced_by_defaults() {
        let d = tmpdir("corrupt");
        let good = Settings { token: "2-9-good".into(), ..Default::default() };
        good.save(&d).unwrap();
        // A second save keeps the first as the backup.
        Settings { token: "2-9-good".into(), ring: false, ..Default::default() }.save(&d).unwrap();
        // Power cut mid-write: a truncated file.
        std::fs::write(d.join("config.json"), "{\"token\": \"2-9").unwrap();
        let (s, how) = Settings::load_checked(&d);
        assert!(matches!(how, Loaded::FromBackup(_)), "{how:?}");
        assert_eq!(s.token, "2-9-good");
        assert!(crate::fsx::exists(&d.join("config.json.corrupt")));
        // Missing main file, good backup.
        let _ = std::fs::remove_file(d.join("config.json"));
        let (s, how) = Settings::load_checked(&d);
        assert!(matches!(how, Loaded::FromBackup(_)));
        assert_eq!(s.token, "2-9-good");
        // Nothing readable: defaults, but the bad file stays for diagnosis.
        let _ = std::fs::remove_file(d.join("config.json.bak"));
        std::fs::write(d.join("config.json"), "garbage").unwrap();
        let (_, how) = Settings::load_checked(&d);
        assert!(matches!(how, Loaded::Defaults(_)));
        assert_eq!(crate::fsx::read_string(&d.join("config.json.corrupt")).unwrap(), "garbage");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn newer_fields_survive_and_page_updates_merge() {
        let mut s: Settings = serde_json::from_str(r#"{"token":"t","from_the_future":{"a":1}}"#).unwrap();
        assert_eq!(s.extra["from_the_future"]["a"], 1);
        let out = serde_json::to_value(&s).unwrap();
        assert_eq!(out["from_the_future"]["a"], 1);
        s.eq = vec![3; 10];
        // The page sends only some fields: the rest keep their values.
        let m = s.merged(&serde_json::json!({"ring": false, "config_version": 99})).unwrap();
        assert!(!m.ring);
        assert_eq!(m.token, "t");
        assert_eq!(m.eq, vec![3; 10]);
        assert_eq!(m.config_version, s.config_version);
        assert!(s.merged(&serde_json::json!([1])).is_err());
    }

    #[test]
    fn music_service_and_keyword_switch_agree() {
        let old = Settings::default();
        let mut new = Settings { music_default: "yandex".into(), ..old.clone() };
        new.sync_music(&old);
        assert!(new.keyword_only);
        let mut back = Settings { keyword_only: false, ..new.clone() };
        back.sync_music(&new);
        assert_eq!(back.music_default, "soundcloud");
        let mut junk = Settings { music_default: "spotify".into(), ..old.clone() };
        junk.sync_music(&old);
        assert_eq!(junk.music_default, "soundcloud");
    }

    #[test]
    fn masks_password_only() {
        let s = Settings { proxy: "socks5://bob:secret@1.2.3.4:1080".into(), ..Default::default() };
        assert_eq!(s.masked_proxy(), "socks5://bob:••••@1.2.3.4:1080");
        let s = Settings { proxy: "http://1.2.3.4:3128".into(), ..Default::default() };
        assert_eq!(s.masked_proxy(), "http://1.2.3.4:3128");
        let s = Settings { token: "2-123456-abcdEFGH".into(), ..Default::default() };
        assert_eq!(s.masked_token(), "••••••••EFGH");
    }

    #[test]
    fn several_tvs_and_pcs() {
        let mut s = Settings { tv_host: "10.0.0.2".into(), tv_name: "Гостиная".into(), pc_host: "10.0.0.5".into(), pc_token: "k1".into(), ..Default::default() };
        s.tvs_more.push(TvDevice { name: "Спальня".into(), host: "10.0.0.3".into() });
        s.pcs_more.push(PcDevice { name: "Ноутбук".into(), host: "10.0.0.6".into(), token: "secretkey9".into(), mac: String::new() });
        assert_eq!(s.tvs().len(), 2);
        assert_eq!(s.active_tv().unwrap().host, "10.0.0.2", "the first by default");
        s.tv_active = "Спальня".into();
        assert_eq!(s.active_tv().unwrap().host, "10.0.0.3");
        let n = s.pcs()[1].clone();
        assert_eq!((s.with_pc(&n).pc_host.as_str(), s.with_pc(&n).pc_token.as_str()), ("10.0.0.6", "secretkey9"));
        assert_eq!(s.masked_pcs_more()[0].token, "••••••••key9");
        // Settings from before: no names, one of each, as it was.
        let old: Settings = serde_json::from_str(r#"{"tv_host":"1.2.3.4","pc_host":"1.2.3.5"}"#).unwrap();
        assert_eq!((old.tvs().len(), old.pcs().len(), old.active_tv().unwrap().host.as_str()), (1, 1, "1.2.3.4"));
    }
}
