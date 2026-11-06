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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
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
    /// AirPlay receiver on.
    #[serde(default = "yes")]
    pub airplay: bool,
    /// Name in the AirPlay list on the iPhone; "" = «Яндекс Станция».
    #[serde(default)]
    pub airplay_name: String,
    /// Picture next to the name: "speaker", "homepod", "homepod-big", "airport", "appletv".
    #[serde(default = "speaker")]
    pub airplay_icon: String,
    /// Anonymous daily «I'm running» ping for the speaker count (src/stats.rs).
    #[serde(default = "yes")]
    pub stats: bool,
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
    /// Optional PIN for changing anything from the panel ("" = no PIN).
    /// Never sent to the page; `soundya pin off` on the speaker removes it.
    #[serde(default)]
    pub panel_pin: String,
    /// The panel's first-run wizard was gone through (or skipped).
    #[serde(default)]
    pub onboarded: bool,
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
pub struct Contact {
    /// How it is said: "мама|маме|маму".
    #[serde(default)]
    pub names: String,
    /// "@username" or a phone number.
    #[serde(default)]
    pub target: String,
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
            ring: true,
            pc_host: String::new(),
            pc_token: String::new(),
            pc_mac: String::new(),
            pc_contacts: Vec::new(),
            airplay: true,
            airplay_name: String::new(),
            airplay_icon: speaker(),
            stats: true,
            tv_host: String::new(),
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
            panel_pin: String::new(),
            onboarded: false,
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
            let _ = std::fs::copy(&main, backup_path(dir)).and_then(|_| sync_file(&backup_path(dir)));
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

    pub fn masked_pc_token(&self) -> String {
        mask(&self.pc_token)
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
        self.pc_on && (!self.pc_host.trim().is_empty() || !self.pc_mac.trim().is_empty())
    }

    pub fn tv_configured(&self) -> bool {
        self.tv_on && !self.tv_host.trim().is_empty()
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
    use super::*;

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
}
