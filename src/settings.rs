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

fn yes() -> bool {
    true
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

    /// Commands can go to the PC (switching on needs only the MAC address).
    pub fn pc_configured(&self) -> bool {
        !self.pc_host.trim().is_empty() || !self.pc_mac.trim().is_empty()
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
    fn masks_password_only() {
        let s = Settings { proxy: "socks5://bob:secret@1.2.3.4:1080".into(), ..Default::default() };
        assert_eq!(s.masked_proxy(), "socks5://bob:••••@1.2.3.4:1080");
        let s = Settings { proxy: "http://1.2.3.4:3128".into(), ..Default::default() };
        assert_eq!(s.masked_proxy(), "http://1.2.3.4:3128");
        let s = Settings { token: "2-123456-abcdEFGH".into(), ..Default::default() };
        assert_eq!(s.masked_token(), "••••••••EFGH");
    }
}
