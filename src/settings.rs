//! Settings edited from the web page, stored in <state dir>/config.json.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Settings {
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
        }
    }
}

fn path(dir: &Path) -> PathBuf {
    dir.join("config.json")
}

impl Settings {
    pub fn load(dir: &Path) -> Self {
        let mut s: Settings = crate::fsx::read_string(&path(dir))
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default();
        // First run: take the defaults from soundya.env.
        if !crate::fsx::exists(&path(dir)) {
            s.keyword_only = std::env::var("SOUNDYA_KEYWORD_ONLY").is_ok_and(|v| v == "1");
            s.proxy = std::env::var("SOUNDYA_PROXY").unwrap_or_default();
        }
        s
    }

    pub fn save(&self, dir: &Path) -> std::io::Result<()> {
        let tmp = dir.join("config.json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(self).map_err(std::io::Error::other)?)?;
        std::fs::rename(tmp, path(dir))
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
