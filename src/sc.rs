//! SoundCloud api-v2 client (the same unofficial API the website uses).

use std::{
    io::{self, Read},
    sync::{Arc, Mutex},
    thread,
    time::Duration,
};

use serde::Deserialize;
use serde_json::Value;

pub type Res<T> = Result<T, Box<dyn std::error::Error + Send + Sync>>;

const API: &str = "https://api-v2.soundcloud.com";
/// soundya talks to SoundCloud the way its web player in Chrome does: same
/// User-Agent and client hints, same Origin/Referer and Accept headers, the
/// site's own client_id and app_version, and it never re-reads the site more
/// than needed. (The TLS handshake is rustls's, not Chrome's; that cannot be
/// changed without a much bigger library.)
const UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/138.0.0.0 Safari/537.36";
const SEC_CH_UA: &str = "\"Not)A;Brand\";v=\"8\", \"Chromium\";v=\"138\", \"Google Chrome\";v=\"138\"";
/// Re-reading soundcloud.com for a client_id at most this often.
/// Where soundcloud.com renews its day-long login.
const TOKEN_URL: &str = "https://secure.soundcloud.com/oauth/token";

fn unix_now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

/// One claim of a JWT token (its middle part, base64url JSON); None for the old, opaque tokens.
pub fn jwt_claim(token: &str, key: &str) -> Option<Value> {
    let mut parts = token.trim().trim_start_matches("OAuth ").split('.');
    let (_, body, _) = (parts.next()?, parts.next()?, parts.next()?);
    let bytes = base64url(body)?;
    serde_json::from_slice::<Value>(&bytes).ok()?.get(key).cloned()
}

fn base64url(s: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    let (mut acc, mut bits) = (0u32, 0u32);
    for c in s.trim_end_matches('=').bytes() {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'-' | b'+' => 62,
            b'_' | b'/' => 63,
            _ => return None,
        };
        acc = (acc << 6) | u32::from(v);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
            acc &= (1 << bits) - 1;
        }
    }
    Some(out)
}

const SCRAPE_EVERY: Duration = Duration::from_secs(10 * 60);

/// Adds the headers Chrome sends to SoundCloud's site, API and CDN.
fn browser_headers(req: ureq::Request, next: ureq::MiddlewareNext) -> Result<ureq::Response, ureq::Error> {
    let url = req.url().to_owned();
    let host = url.split("://").nth(1).and_then(|r| r.split(['/', '?', ':']).next()).unwrap_or("").to_owned();
    if !(host.ends_with("soundcloud.com") || host.ends_with("sndcdn.com") || host.ends_with("soundcloud.cloud")) {
        return note_stub(&host, next.handle(req));
    }
    let page = host == "soundcloud.com";
    let mut req = req
        .set("Accept-Language", "ru-RU,ru;q=0.9,en-US;q=0.8,en;q=0.7")
        .set("sec-ch-ua", SEC_CH_UA)
        .set("sec-ch-ua-mobile", "?0")
        .set("sec-ch-ua-platform", "\"Windows\"");
    if page {
        // Opening the site itself, as a tab would.
        if req.header("Accept").is_none() {
            req = req.set(
                "Accept",
                "text/html,application/xhtml+xml,application/xml;q=0.9,image/avif,image/webp,*/*;q=0.8",
            );
        }
        req = req.set("Sec-Fetch-Site", "none").set("Sec-Fetch-Mode", "navigate").set("Sec-Fetch-Dest", "document");
    } else {
        // Everything else is fetched by the player page.
        if req.header("Origin").is_none() {
            req = req.set("Origin", "https://soundcloud.com");
        }
        if req.header("Referer").is_none() {
            req = req.set("Referer", "https://soundcloud.com/");
        }
        if req.header("Accept").is_none() {
            req = req.set("Accept", "*/*");
        }
        req = req.set("Sec-Fetch-Site", "same-site").set("Sec-Fetch-Mode", "cors").set("Sec-Fetch-Dest", "empty");
    }
    note_stub(&host, next.handle(req))
}

/// Someone else's certificate: the provider's DNS stub, not the site (dns.rs takes it from there).
fn note_stub(host: &str, r: Result<ureq::Response, ureq::Error>) -> Result<ureq::Response, ureq::Error> {
    if let Err(ureq::Error::Transport(t)) = &r {
        if crate::dns::wrong_certificate(&t.to_string()) {
            crate::dns::stub(host);
        }
    }
    r
}

/// GET with one more try on a network error, on another address (a stub, a dropped connection).
pub fn get_retry(agent: &ureq::Agent, url: &str, timeout: Duration) -> Result<ureq::Response, ureq::Error> {
    match agent.get(url).timeout(timeout).call() {
        Err(ureq::Error::Transport(t)) => {
            eprintln!("net: {}: {t}; one more try", url.split('?').next().unwrap_or(url));
            crate::dns::failed(url);
            agent.get(url).timeout(timeout).call()
        }
        r => r,
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct User {
    #[serde(default)]
    pub username: String,
    #[serde(default)]
    pub avatar_url: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Format {
    pub protocol: String,
    pub mime_type: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Transcoding {
    pub url: String,
    #[serde(default)]
    pub snipped: bool,
    pub format: Format,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Media {
    #[serde(default)]
    pub transcodings: Vec<Transcoding>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Track {
    pub id: u64,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub user: Option<User>,
    #[serde(default)]
    pub policy: Option<String>,
    #[serde(default)]
    pub track_authorization: Option<String>,
    #[serde(default)]
    pub media: Option<Media>,
    #[serde(default)]
    pub duration: Option<u64>,
    #[serde(default)]
    pub permalink_url: Option<String>,
    #[serde(default)]
    pub artwork_url: Option<String>,
}

impl Track {
    /// A track known only by id, filled in by `hydrate`.
    pub fn from_id(id: u64) -> Track {
        Track {
            id,
            title: String::new(),
            user: None,
            policy: None,
            track_authorization: None,
            media: None,
            duration: None,
            permalink_url: None,
            artwork_url: None,
        }
    }

    pub fn name(&self) -> String {
        match &self.user {
            Some(u) if !u.username.is_empty() => format!("{} — {}", u.username, self.title),
            _ => self.title.clone(),
        }
    }
    /// The cover, 300×300 (the API gives the 100×100 "large" one): the
    /// track's own, or its author's picture when it has none.
    pub fn cover(&self) -> Option<String> {
        let url = self
            .artwork_url
            .clone()
            .filter(|u| !u.is_empty())
            .or_else(|| self.user.as_ref().and_then(|u| u.avatar_url.clone()).filter(|u| !u.is_empty()))?;
        Some(url.replace("-large.", "-t300x300."))
    }

    fn hydrated(&self) -> bool {
        self.media.is_some() && !self.title.is_empty()
    }
    /// Streams we can decode, best first: progressive MP3, HLS MP3, then AAC.
    /// SoundCloud still lists formats it no longer serves (404), so callers try them in turn.
    pub fn candidates(&self) -> Vec<(&Transcoding, Kind)> {
        if matches!(self.policy.as_deref(), Some("BLOCK" | "SNIP")) {
            return Vec::new();
        }
        let Some(media) = self.media.as_ref() else { return Vec::new() };
        let mut out: Vec<(u8, &Transcoding, Kind)> = Vec::new();
        for t in &media.transcodings {
            if t.snipped {
                continue;
            }
            let mime = t.format.mime_type.to_ascii_lowercase();
            let (rank, kind) = match t.format.protocol.as_str() {
                "progressive" if mime.contains("mpeg") => (0, Kind::Mp3),
                "hls" if mime.contains("mpeg") && !mime.contains("mpegurl") => (1, Kind::Mp3),
                "progressive" if mime.contains("mp4") => (2, Kind::Mp4),
                "hls" if mime.contains("mp4") => (3, Kind::Mp4),
                "hls" if mime.contains("aac") => (4, Kind::Adts),
                _ => continue, // opus and encrypted streams are not supported
            };
            out.push((rank, t, kind));
        }
        out.sort_by_key(|c| c.0);
        out.into_iter().map(|(_, t, k)| (t, k)).collect()
    }

    pub fn transcoding(&self) -> Option<(&Transcoding, Kind)> {
        self.candidates().into_iter().next()
    }

    /// «Слушать здесь» can start where the speaker is: MP3/AAC by skipping bytes,
    /// HLS from the segment holding the moment. Only a progressive MP4 cannot.
    pub fn stream_seekable(&self) -> bool {
        self.transcoding().is_some_and(|(t, k)| t.format.protocol == "hls" || k != Kind::Mp4)
    }

    /// Why the speaker cannot play it, in words for the page and the log.
    pub fn why_not_playable(&self) -> String {
        match self.policy.as_deref() {
            Some("BLOCK") => {
                return "SoundCloud не отдаёт этот трек в вашей стране (ограничение правообладателя): поможет прокси или VPN другой страны".into()
            }
            Some("SNIP") => return "SoundCloud отдаёт только 30-секундное превью: трек доступен лишь по подписке Go+".into(),
            _ => {}
        }
        if self.media.as_ref().map_or(true, |m| m.transcodings.is_empty()) {
            return "SoundCloud не прислал ни одного звукового потока".into();
        }
        format!("у трека только форматы, которые колонка пока не умеет: {}", self.formats())
    }

    /// All formats SoundCloud lists for the track, for error messages.
    pub fn formats(&self) -> String {
        let Some(media) = self.media.as_ref() else { return "нет данных".into() };
        media
            .transcodings
            .iter()
            .map(|t| {
                format!(
                    "{} {}{}",
                    t.format.protocol,
                    t.format.mime_type,
                    if t.snipped { " (превью)" } else { "" }
                )
            })
            .collect::<Vec<_>>()
            .join(", ")
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Mp3,
    Mp4,
    Adts,
}

impl Kind {
    pub fn extension(self) -> &'static str {
        match self {
            Kind::Mp3 => "mp3",
            Kind::Mp4 => "mp4",
            Kind::Adts => "aac",
        }
    }
}

#[derive(Deserialize)]
struct Collection<T> {
    #[serde(default = "Vec::new")]
    collection: Vec<T>,
}

#[derive(Clone)]
pub struct Client {
    agent: Arc<Mutex<ureq::Agent>>,
    client_id: Arc<Mutex<Option<String>>>,
    /// The site's build number (`window.__sc_version`), sent as `app_version` like the web player.
    app_version: Arc<Mutex<Option<String>>>,
    last_scrape: Arc<Mutex<Option<std::time::Instant>>>,
    /// When the site was last read successfully (the client_id is the site's current one).
    scraped_ok: Arc<Mutex<Option<std::time::Instant>>>,
    /// api-v2 and the site; other servers only in tests.
    api_base: String,
    site: String,
    token: Arc<Mutex<Option<String>>>,
    /// SoundCloud refused the token: requests go without it until a new one is entered.
    token_bad: Arc<std::sync::atomic::AtomicBool>,
    /// The site's `oauth_refresh_token` cookie: since 2026 `oauth_token` is a JWT that lives a
    /// day, and this (single use) gets the next one. "" — none given.
    refresh: Arc<Mutex<Option<String>>>,
    /// A renewed (token, refresh token) pair the controller has yet to save: the old refresh
    /// token no longer works, so it must reach the settings file at once.
    renewed: Arc<Mutex<Option<(String, String)>>>,
    /// Renewal in progress / last attempt / why the last one failed.
    renewing: Arc<Mutex<Option<std::time::Instant>>>,
    renew_error: Arc<Mutex<Option<String>>>,
    /// Where tokens are renewed: secure.soundcloud.com; another server only in tests.
    token_url: String,
    cache: Option<std::path::PathBuf>,
    /// Formats that answered 404 this session; tried last from then on.
    dead: Arc<Mutex<std::collections::HashSet<String>>>,
    /// Recommendations built ahead of time, so «включи рекомендации» starts at once.
    recs: Arc<Mutex<Option<(std::time::Instant, Vec<Track>)>>>,
    /// A track's stream opened ahead of time (the next track, or the first
    /// recommendation), so it starts without waiting for the proxy.
    pub prefetch: Prefetch,
}

pub type Prefetch = Arc<Mutex<Option<(u64, Box<dyn Read + Send + Sync>, Kind)>>>;

/// Prepared recommendations stay usable this long.
const RECS_FRESH: Duration = Duration::from_secs(30 * 60);

/// A playlist as shown in search results and the library.
#[derive(Debug, Clone, serde::Serialize)]
pub struct PlaylistInfo {
    pub id: String,
    pub title: String,
    pub user: String,
    pub count: u64,
    pub link: String,
    /// Cover (t300x300): the playlist's own, else its first track's; "" if none.
    pub art: String,
}

fn playlist_info(v: &Value) -> Option<PlaylistInfo> {
    let id = match v.get("id") {
        Some(Value::Number(n)) => n.to_string(),
        Some(Value::String(s)) => s.clone(),
        _ => v.get("urn").and_then(Value::as_str)?.to_owned(),
    };
    Some(PlaylistInfo {
        id,
        title: v.get("title").and_then(Value::as_str).unwrap_or("").to_owned(),
        user: v
            .pointer("/user/username")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned(),
        count: v.get("track_count").and_then(Value::as_u64).unwrap_or(0),
        link: v.get("permalink_url").and_then(Value::as_str).unwrap_or("").to_owned(),
        art: v
            .get("artwork_url")
            .and_then(Value::as_str)
            .filter(|u| !u.is_empty())
            .or_else(|| v.get("calculated_artwork_url").and_then(Value::as_str).filter(|u| !u.is_empty()))
            // As soundcloud.com shows it: the first track that has a cover (the first may have none).
            .or_else(|| {
                v.get("tracks")?.as_array()?.iter().find_map(|t| t.get("artwork_url").and_then(Value::as_str).filter(|u| !u.is_empty()))
            })
            .or_else(|| v.pointer("/user/avatar_url").and_then(Value::as_str))
            .unwrap_or("")
            .replace("-large.", "-t300x300."),
    })
}

/// No cover of its own: none, or its author's picture standing in.
fn bare_cover(art: &str) -> bool {
    art.is_empty() || art.contains("/avatars-")
}

/// The first track cover in a list of tracks, t300x300.
fn first_cover(tracks: &Value) -> Option<String> {
    tracks
        .as_array()?
        .iter()
        .find_map(|t| t.get("artwork_url").and_then(Value::as_str).filter(|u| !u.is_empty()))
        .map(|u| u.replace("-large.", "-t300x300."))
}

fn status_of(e: &ureq::Error) -> Option<u16> {
    match e {
        ureq::Error::Status(code, _) => Some(*code),
        _ => None,
    }
}

impl Client {
    pub fn new(cache: Option<std::path::PathBuf>, proxy: &str) -> Self {
        let agent = build_agent(proxy).unwrap_or_else(|e| {
            eprintln!("proxy {proxy:?} ignored: {e}");
            build_agent("").expect("direct agent")
        });
        let cached = std::env::var("SOUNDYA_CLIENT_ID")
            .ok()
            .or_else(|| {
                cache
                    .as_ref()
                    .and_then(|p| crate::fsx::read_string(p))
                    .map(|s| s.trim().to_owned())
            })
            .filter(|s| s.len() >= 20);
        Self {
            agent: Arc::new(Mutex::new(agent)),
            client_id: Arc::new(Mutex::new(cached)),
            app_version: Arc::new(Mutex::new(None)),
            last_scrape: Arc::new(Mutex::new(None)),
            scraped_ok: Arc::new(Mutex::new(None)),
            api_base: API.to_owned(),
            site: "https://soundcloud.com/".to_owned(),
            token: Arc::new(Mutex::new(None)),
            token_bad: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            refresh: Arc::new(Mutex::new(None)),
            renewed: Arc::new(Mutex::new(None)),
            renewing: Arc::new(Mutex::new(None)),
            renew_error: Arc::new(Mutex::new(None)),
            token_url: TOKEN_URL.to_owned(),
            cache,
            dead: Arc::new(Mutex::new(std::collections::HashSet::new())),
            recs: Arc::new(Mutex::new(None)),
            prefetch: Arc::new(Mutex::new(None)),
        }
    }

    pub fn agent(&self) -> ureq::Agent {
        self.agent.lock().unwrap().clone()
    }

    /// OAuth token from the soundcloud.com `oauth_token` cookie; "" logs out.
    pub fn set_token(&self, token: &str) {
        let t = token.trim().trim_start_matches("OAuth ").trim();
        *self.token.lock().unwrap() = (!t.is_empty()).then(|| t.to_owned());
        self.token_bad.store(false, std::sync::atomic::Ordering::Release);
    }

    pub fn has_token(&self) -> bool {
        self.token.lock().unwrap().is_some()
    }

    /// SoundCloud stopped accepting the saved token (expired, logged out on the site).
    pub fn token_refused(&self) -> bool {
        self.has_token() && self.token_bad.load(std::sync::atomic::Ordering::Acquire)
    }

    /// The `oauth_refresh_token` cookie; "" forgets it.
    pub fn set_refresh(&self, refresh: &str) {
        let r = refresh.trim();
        *self.refresh.lock().unwrap() = (!r.is_empty()).then(|| r.to_owned());
        *self.renew_error.lock().unwrap() = None;
        *self.renewing.lock().unwrap() = None;
    }

    pub fn has_refresh(&self) -> bool {
        self.refresh.lock().unwrap().is_some()
    }

    /// When the saved token stops working (unix seconds): only the new, JWT tokens say it.
    pub fn token_exp(&self) -> Option<u64> {
        self.token.lock().unwrap().as_deref().and_then(|t| jwt_claim(t, "exp")).and_then(|v| v.as_u64())
    }

    /// The saved token has run out (a JWT past its time).
    pub fn token_expired(&self) -> bool {
        self.token_exp().is_some_and(|exp| exp <= unix_now())
    }

    /// Why the last renewal failed, for the page («SoundCloud не принял…»).
    pub fn renew_error(&self) -> Option<String> {
        self.renew_error.lock().unwrap().clone()
    }

    /// A renewed (token, refresh token) to save, once.
    pub fn take_renewed(&self) -> Option<(String, String)> {
        self.renewed.lock().unwrap().take()
    }

    /// From the controller's loop: renews the token in the background 10 minutes before it
    /// runs out (or at once if it already has), a minute between tries after a network error.
    pub fn keep_token(&self) {
        if !self.renew_due() {
            return;
        }
        let me = self.clone();
        thread::spawn(move || {
            let _ = me.renew();
        });
    }

    fn renew_due(&self) -> bool {
        let Some(exp) = self.token_exp() else { return false };
        if !self.has_refresh() || exp > unix_now() + 600 {
            return false;
        }
        // A refused refresh token is not tried again until a new one is given.
        if self.renew_error.lock().unwrap().as_deref().is_some_and(|e| e.starts_with("refused")) {
            return false;
        }
        let mut last = self.renewing.lock().unwrap();
        if last.is_some_and(|t| t.elapsed() < Duration::from_secs(60)) {
            return false;
        }
        *last = Some(std::time::Instant::now());
        true
    }

    /// Exchanges the refresh token for a new pair, as soundcloud.com does every day.
    pub fn renew(&self) -> Result<(), String> {
        let Some(refresh) = self.refresh.lock().unwrap().clone() else {
            return Err("нет oauth_refresh_token".into());
        };
        // The site renews with the client the token was issued to.
        let client_id = self
            .token
            .lock()
            .unwrap()
            .as_deref()
            .and_then(|t| jwt_claim(t, "client_id"))
            .and_then(|v| v.as_str().map(str::to_owned))
            .or_else(|| self.client_id.lock().unwrap().clone())
            .unwrap_or_default();
        let r = self.agent().post(&self.token_url).set("Accept", "application/json").send_form(&[
            ("client_id", client_id.as_str()),
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh.as_str()),
        ]);
        let v: Value = match r {
            Ok(resp) => resp.into_json().map_err(|e| e.to_string())?,
            Err(e) if matches!(status_of(&e), Some(400 | 401 | 403)) => {
                let code = status_of(&e).unwrap_or(0);
                eprintln!("soundcloud: the refresh token was refused ({code})");
                *self.renew_error.lock().unwrap() = Some(format!("refused: SoundCloud не принял oauth_refresh_token ({code})"));
                return Err(format!("SoundCloud не принял oauth_refresh_token ({code})"));
            }
            Err(ureq::Error::Status(code, _)) => {
                let e = format!("HTTP {code} на secure.soundcloud.com");
                eprintln!("soundcloud: renewing the token: {e}");
                *self.renew_error.lock().unwrap() = Some(e.clone());
                return Err(e);
            }
            Err(ureq::Error::Transport(t)) => {
                let e = format!("secure.soundcloud.com: {}", t.message().unwrap_or(&t.kind().to_string()));
                eprintln!("soundcloud: renewing the token: {e}");
                *self.renew_error.lock().unwrap() = Some(e.clone());
                return Err(e);
            }
        };
        let Some(token) = v.get("access_token").and_then(Value::as_str).filter(|t| !t.is_empty()) else {
            *self.renew_error.lock().unwrap() = Some("SoundCloud ответил без токена".into());
            return Err("SoundCloud ответил без токена".into());
        };
        // Single use: the old refresh token is spent, the new one goes to the settings at once.
        let next = v.get("refresh_token").and_then(Value::as_str).filter(|t| !t.is_empty()).unwrap_or(&refresh).to_owned();
        *self.token.lock().unwrap() = Some(token.to_owned());
        *self.refresh.lock().unwrap() = Some(next.clone());
        *self.renewed.lock().unwrap() = Some((token.to_owned(), next));
        *self.renew_error.lock().unwrap() = None;
        self.token_bad.store(false, std::sync::atomic::Ordering::Release);
        eprintln!("soundcloud: the token is renewed");
        Ok(())
    }

    /// Switches all later requests (API and audio) to a new proxy; "" means direct.
    pub fn set_proxy(&self, proxy: &str) -> Res<()> {
        let agent = build_agent(proxy)?;
        *self.agent.lock().unwrap() = agent;
        Ok(())
    }

    fn client_id(&self, refresh: bool) -> Res<String> {
        let mut guard = self.client_id.lock().unwrap();
        // A browser reads the site once, not before every request.
        let recent = self.last_scrape.lock().unwrap().is_some_and(|t| t.elapsed() < SCRAPE_EVERY);
        if !refresh || recent {
            if let Some(id) = guard.as_ref() {
                return Ok(id.clone());
            }
        }
        *self.last_scrape.lock().unwrap() = Some(std::time::Instant::now());
        let id = self.scrape_client_id()?;
        self.keep_client_id(&mut guard, &id);
        Ok(id)
    }

    fn keep_client_id(&self, guard: &mut Option<String>, id: &str) {
        eprintln!("soundcloud: new client_id");
        if let Some(p) = &self.cache {
            let _ = std::fs::write(p, id);
        }
        *guard = Some(id.to_owned());
        *self.scraped_ok.lock().unwrap() = Some(std::time::Instant::now());
    }

    /// Reads the site for the current client_id now and then (every 6 hours) before a command
    /// meets an old one. Called from the controller's loop; the reading runs in its own thread.
    pub fn keep_fresh(&self) {
        let due = self.scraped_ok.lock().unwrap().is_none_or(|t| t.elapsed() > Duration::from_secs(6 * 3600));
        let tried = self.last_scrape.lock().unwrap().is_some_and(|t| t.elapsed() < Duration::from_secs(15 * 60));
        if due && !tried {
            *self.last_scrape.lock().unwrap() = Some(std::time::Instant::now());
            let c = self.clone();
            thread::spawn(move || {
                let _ = c.fresh_client_id_now();
            });
        }
    }

    fn fresh_client_id_now(&self) -> Option<String> {
        // The site is read without the lock: every command takes it (client_id), and
        // reading can take minutes on a slow network, which froze music commands.
        let scraped = self.scrape_client_id();
        let mut guard = self.client_id.lock().unwrap();
        match scraped {
            Ok(id) => {
                if guard.as_deref() != Some(id.as_str()) {
                    self.keep_client_id(&mut guard, &id);
                } else {
                    *self.scraped_ok.lock().unwrap() = Some(std::time::Instant::now());
                }
                Some(id)
            }
            Err(e) => {
                eprintln!("soundcloud: soundcloud.com not read for a client_id: {e}");
                None
            }
        }
    }

    /// The site's current client_id: the one in use if the site was read lately, else read
    /// now. None: the site could not be read (then nothing can be said about the token).
    fn fresh_client_id(&self) -> Option<String> {
        let mut guard = self.client_id.lock().unwrap();
        if self.scraped_ok.lock().unwrap().is_some_and(|t| t.elapsed() < SCRAPE_EVERY) {
            if let Some(id) = guard.as_ref() {
                return Some(id.clone());
            }
        }
        *self.last_scrape.lock().unwrap() = Some(std::time::Instant::now());
        match self.scrape_client_id() {
            Ok(id) => {
                self.keep_client_id(&mut guard, &id);
                Some(id)
            }
            Err(e) => {
                eprintln!("soundcloud: soundcloud.com not read for a client_id: {e}");
                None
            }
        }
    }

    fn scrape_client_id(&self) -> Res<String> {
        let html = get_retry(&self.agent(), &self.site, Duration::from_secs(30))?.into_string()?;
        if let Some(v) = app_version(&html) {
            *self.app_version.lock().unwrap() = Some(v);
        }
        let scripts = script_urls(&html);
        for url in scripts.iter().rev() {
            let Ok(resp) = get_retry(&self.agent(), url, Duration::from_secs(30)) else {
                continue;
            };
            let mut js = String::new();
            if resp.into_reader().take(8 << 20).read_to_string(&mut js).is_err() {
                continue;
            }
            if let Some(id) = find_client_id(&js) {
                return Ok(id);
            }
        }
        Err("client_id not found on soundcloud.com".into())
    }

    /// GET an api-v2 endpoint as JSON, refreshing client_id once on 401/403.
    fn api(&self, path_or_url: &str, query: &[(&str, &str)]) -> Res<Value> {
        let url = if path_or_url.starts_with("http") {
            path_or_url.to_owned()
        } else {
            format!("{}{path_or_url}", self.api_base)
        };
        let mut refreshed = false;
        let mut retried = false;
        let bad = std::sync::atomic::Ordering::Acquire;
        // A token that has run out: a new one first (from the refresh token), if there is one.
        if self.has_token() && self.token_expired() && self.has_refresh() && self.renew_due() {
            let _ = self.renew();
        }
        // Without the token: it was refused (search and public pages work anonymously).
        // «/me» is only ever about the login, so it always goes with it.
        let mut anonymous = self.token_bad.load(bad) && !url.ends_with("/me");
        loop {
            let id = self.client_id(refreshed)?;
            let mut req = self
                .agent()
                .get(&url)
                .set("Accept", "application/json, text/javascript, */*; q=0.01")
                .query("client_id", &id);
            if !anonymous {
                if let Some(t) = self.token.lock().unwrap().as_ref() {
                    req = req.set("Authorization", &format!("OAuth {t}"));
                }
            }
            for (k, v) in query {
                req = req.query(k, v);
            }
            if let Some(v) = self.app_version.lock().unwrap().as_ref() {
                req = req.query("app_version", v).query("app_locale", "ru");
            }
            let short = url.split('?').next().unwrap_or(&url).to_owned();
            match req.call() {
                Ok(resp) => {
                    // Worked without the token: maybe the token is what failed — but
                    // a single 401 is also what SoundCloud's bot protection or a stale
                    // client_id gives. 0.4.x dropped the login for good here, and the
                    // user had to paste the very same, still valid token again. Now
                    // the token itself is asked (/me) before it is given up on.
                    if anonymous && !refreshed && self.has_token() && !self.token_bad.load(bad) {
                        match self.verify_token() {
                            Some(false) => {
                                self.token_bad.store(true, std::sync::atomic::Ordering::Release);
                                eprintln!("soundcloud: token refused by /me; going on without login, re-checking every 10 minutes");
                            }
                            Some(true) => eprintln!("soundcloud: one 401 on {short}, but the token is still valid; keeping it"),
                            None => eprintln!("soundcloud: one 401 on {short}; token not checked (network), keeping it"),
                        }
                    }
                    return Ok(resp.into_json()?);
                }
                // Refused with a token: the token is the likely cause, try without it
                // first (re-reading the site for a new client_id is the slow, unusual path).
                Err(e) if matches!(status_of(&e), Some(401)) && self.has_token() && !anonymous => {
                    eprintln!("soundcloud: token refused on {short}; trying without login");
                    anonymous = true;
                    continue;
                }
                // Refused even without the token: the client_id is old (SoundCloud changes it every day
                // or two). The site's current one, then the token again with it.
                Err(e) if !refreshed && matches!(status_of(&e), Some(401 | 403)) => {
                    refreshed = true;
                    if self.fresh_client_id().is_none() {
                        return Err("SoundCloud не пустил: его ключ сайта сменился, а soundcloud.com не открылся за новым. Вход при этом цел".into());
                    }
                    anonymous = self.token_bad.load(bad);
                    continue;
                }
                // Refused with the site's current client_id and without the token: not the login
                // (0.4–0.5 said «токен не принят» here, and Alice told to log in again — the token was fine).
                Err(e) if matches!(status_of(&e), Some(401 | 403)) => {
                    return Err(format!("SoundCloud не пустил запрос ({}) — вход при этом цел", status_of(&e).unwrap_or(0)).into())
                }
                // Keep tokens out of messages and logs.
                Err(ureq::Error::Status(code, _)) => return Err(format!("HTTP {code} на {short}").into()),
                Err(ureq::Error::Transport(t)) if !retried => {
                    // Proxies drop connections now and then: one quiet retry,
                    // on another of SoundCloud's servers.
                    crate::dns::failed(&short);
                    eprintln!("soundcloud: {}: {}; retrying", t.kind(), t.message().unwrap_or(""));
                    retried = true;
                    thread::sleep(Duration::from_millis(500));
                    continue;
                }
                Err(ureq::Error::Transport(t)) => {
                    return Err(format!("{}: {}", t.kind(), t.message().unwrap_or("")).into())
                }
            }
        }
    }

    pub fn search_tracks(&self, q: &str) -> Res<Vec<Track>> {
        let v = self.api("/search/tracks", &[("q", q), ("limit", "30"), ("offset", "0")])?;
        let c: Collection<Track> = serde_json::from_value(v)?;
        let (ok, skipped): (Vec<Track>, Vec<Track>) = c.collection.into_iter().partition(|t| t.transcoding().is_some());
        for t in skipped.iter().take(5) {
            eprintln!("search: skipped {}: {}", t.name(), t.why_not_playable());
        }
        Ok(ok)
    }

    /// Tracks and playlists for the web page.
    pub fn search_all(&self, q: &str) -> Res<(Vec<Track>, Vec<PlaylistInfo>)> {
        let tracks = self.search_tracks(q)?;
        let v = self.api("/search/playlists", &[("q", q), ("limit", "10")])?;
        let c: Collection<Value> = serde_json::from_value(v)?;
        let lists = c.collection.iter().filter_map(playlist_info).collect();
        Ok((tracks, lists))
    }

    /// Asks SoundCloud whether the saved token is good: Some(true) yes,
    /// Some(false) refused (401/403), None — no answer (network). Clears the
    /// «refused» mark when it is good again.
    pub fn verify_token(&self) -> Option<bool> {
        if self.token_expired() {
            if self.has_refresh() && self.renew().is_ok() {
                return Some(true);
            }
            self.token_bad.store(true, std::sync::atomic::Ordering::Release);
            return Some(false);
        }
        let token = self.token.lock().unwrap().clone()?;
        let id = self.client_id(false).ok()?;
        let r = self
            .agent()
            .get(&format!("{}/me", self.api_base))
            .set("Accept", "application/json")
            .set("Authorization", &format!("OAuth {token}"))
            .query("client_id", &id)
            .call();
        match r {
            Ok(_) => {
                if self.token_bad.swap(false, std::sync::atomic::Ordering::AcqRel) {
                    eprintln!("soundcloud: the saved token works again");
                }
                Some(true)
            }
            // Refused: the token, or a client_id SoundCloud no longer takes (after a restart the
            // site could not be read for a fresh one). With a fresh client_id once more first.
            Err(e) if matches!(status_of(&e), Some(401 | 403)) => {
                let fresh = match self.fresh_client_id() {
                    Some(f) if f != id => f,
                    // Refused with the site's current client_id: the token itself.
                    Some(_) => return Some(false),
                    None => {
                        eprintln!("soundcloud: token refused, but the client_id could not be checked: not dropping the login");
                        return None;
                    }
                };
                let r = self
                    .agent()
                    .get(&format!("{}/me", self.api_base))
                    .set("Accept", "application/json")
                    .set("Authorization", &format!("OAuth {token}"))
                    .query("client_id", &fresh)
                    .call();
                match r {
                    Ok(_) => {
                        eprintln!("soundcloud: the token works with the new client_id");
                        self.token_bad.store(false, std::sync::atomic::Ordering::Release);
                        Some(true)
                    }
                    Err(e) if matches!(status_of(&e), Some(401 | 403)) => Some(false),
                    Err(_) => None,
                }
            }
            Err(_) => None,
        }
    }

    /// (user id, username) of the logged-in account.
    pub fn me(&self) -> Res<(u64, String)> {
        if !self.has_token() {
            return Err("не выполнен вход в SoundCloud".into());
        }
        let v = self.api("/me", &[])?;
        let id = v.get("id").and_then(Value::as_u64).ok_or("нет id в /me")?;
        let name = v.get("username").and_then(Value::as_str).unwrap_or("").to_owned();
        Ok((id, name))
    }

    /// Own and liked playlists, albums and SoundCloud's personal mixes.
    pub fn library(&self) -> Res<Vec<PlaylistInfo>> {
        let (uid, _) = self.me()?;
        let mut out: Vec<PlaylistInfo> = Vec::new();
        if let Ok(v) = self.api("/me/library/all", &[("limit", "100")]) {
            if let Some(items) = v.get("collection").and_then(Value::as_array) {
                for item in items {
                    let obj = item.get("playlist").or_else(|| item.get("system_playlist"));
                    if let Some(info) = obj.and_then(playlist_info) {
                        out.push(info);
                    }
                }
            }
        }
        if out.is_empty() {
            let v = self.api(&format!("/users/{uid}/playlists"), &[("limit", "100")])?;
            let c: Collection<Value> = serde_json::from_value(v)?;
            out = c.collection.iter().filter_map(playlist_info).collect();
        }
        let mut seen = std::collections::HashSet::new();
        out.retain(|p| seen.insert(p.id.clone()));
        Ok(out)
    }

    /// The library for the page: with the covers soundcloud.com shows (a few more requests,
    /// so not for voice commands, which only need the names).
    pub fn library_with_covers(&self) -> Res<Vec<PlaylistInfo>> {
        let mut out = self.library()?;
        if let Ok((uid, _)) = self.me() {
            self.track_covers(uid, &mut out);
        }
        Ok(out)
    }

    /// The library lists playlists without their tracks, so one without its own cover shows
    /// its author's picture; soundcloud.com shows the first track's cover there. Own playlists
    /// come with their tracks in one request, the others (a few) one by one.
    fn track_covers(&self, uid: u64, out: &mut [PlaylistInfo]) {
        let bare = |p: &PlaylistInfo| (p.art.is_empty() || p.art.contains("/avatars-")) && p.id.bytes().all(|b| b.is_ascii_digit());
        if !out.iter().any(bare) {
            return;
        }
        let mut found = std::collections::HashMap::new();
        if let Ok(v) = self.api(&format!("/users/{uid}/playlists"), &[("limit", "100")]) {
            for p in v.get("collection").and_then(Value::as_array).into_iter().flatten().filter_map(playlist_info) {
                if !p.art.contains("/avatars-") && !p.art.is_empty() {
                    found.insert(p.id, p.art);
                }
            }
        }
        let mut asked = 0;
        for p in out.iter_mut().filter(|p| bare(p)) {
            if let Some(art) = found.get(&p.id) {
                p.art = art.clone();
            } else if asked < 12 {
                asked += 1;
                if let Some(info) = self.api(&format!("/playlists/{}", p.id), &[]).ok().as_ref().and_then(playlist_info) {
                    if !info.art.is_empty() && !info.art.contains("/avatars-") {
                        p.art = info.art;
                    }
                }
            }
        }
    }

    pub fn likes(&self) -> Res<Vec<Track>> {
        let (uid, _) = self.me()?;
        let v = self.api(&format!("/users/{uid}/track_likes"), &[("limit", "200")])?;
        let items = v.get("collection").and_then(Value::as_array).cloned().unwrap_or_default();
        let tracks: Vec<Track> = items
            .into_iter()
            .filter_map(|i| i.get("track").cloned())
            .filter_map(|t| serde_json::from_value(t).ok())
            .collect();
        self.hydrate(tracks)
    }

    pub fn has_fresh_recommendations(&self) -> bool {
        self.recs
            .lock()
            .unwrap()
            .as_ref()
            .is_some_and(|(at, t)| at.elapsed() < RECS_FRESH && !t.is_empty())
    }

    /// Recommendations: the prepared set if there is one (and a new one is built
    /// in the background for next time), otherwise built now.
    pub fn recommendations(&self, seed: Option<u64>) -> Res<Vec<Track>> {
        let cached = {
            let mut slot = self.recs.lock().unwrap();
            match slot.take() {
                Some((at, tracks)) if at.elapsed() < RECS_FRESH && !tracks.is_empty() => Some(tracks),
                _ => None,
            }
        };
        match cached {
            Some(tracks) => {
                self.warm_recommendations();
                Ok(tracks)
            }
            None => self.build_recommendations(seed),
        }
    }

    /// Builds a recommendation set in the background if none is ready.
    pub fn warm_recommendations(&self) {
        if !self.has_token() {
            return;
        }
        let client = self.clone();
        thread::spawn(move || {
            if client.has_fresh_recommendations() {
                return;
            }
            match client.build_recommendations(None) {
                Ok(tracks) if !tracks.is_empty() => {
                    eprintln!("recommendations: {} tracks ready", tracks.len());
                    let first = tracks[0].clone();
                    *client.recs.lock().unwrap() = Some((std::time::Instant::now(), tracks));
                    // Open the first track too: then «включи рекомендации» plays at once.
                    let mut slot = client.prefetch.lock().unwrap();
                    if slot.is_none() {
                        drop(slot);
                        if let Ok((reader, kind)) = client.open(&first) {
                            slot = client.prefetch.lock().unwrap();
                            if slot.is_none() {
                                *slot = Some((first.id, reader, kind));
                            }
                        }
                    }
                }
                Ok(_) => {}
                Err(e) => eprintln!("recommendations: {e}"),
            }
        });
    }

    /// A personal "wave": tracks related to a few random recent likes (and to
    /// `seed`, usually the current track), minus tracks already liked, shuffled.
    /// The related-track requests run in parallel: each is a round trip through the proxy.
    fn build_recommendations(&self, seed: Option<u64>) -> Res<Vec<Track>> {
        let liked: Vec<u64> = if self.has_token() { self.liked_list().unwrap_or_default() } else { Vec::new() };
        let mut rng = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(7)
            | 1;
        let mut next = move || {
            rng ^= rng << 13;
            rng ^= rng >> 7;
            rng ^= rng << 17;
            rng
        };
        // What soundcloud.com itself offers on the home page («More of what you
        // like», then the daily/weekly mixes): the same picks as in the browser.
        if self.has_token() {
            match self.personalized() {
                Ok(ids) if ids.len() >= 10 => {
                    let liked: std::collections::HashSet<u64> = liked.iter().copied().collect();
                    let mut ids: Vec<u64> = ids.into_iter().filter(|id| !liked.contains(id)).collect();
                    for i in (1..ids.len()).rev() {
                        let j = (next() % (i as u64 + 1)) as usize;
                        ids.swap(i, j);
                    }
                    ids.truncate(150);
                    let tracks = self.hydrate(ids.into_iter().map(Track::from_id).collect())?;
                    if !tracks.is_empty() {
                        eprintln!("recommendations: {} tracks from soundcloud.com's own picks", tracks.len());
                        return Ok(tracks);
                    }
                }
                Ok(ids) => eprintln!("recommendations: soundcloud.com offered only {} tracks, using related", ids.len()),
                Err(e) => eprintln!("recommendations: home page picks: {e}"),
            }
        }
        let mut seeds: Vec<u64> = seed.into_iter().collect();
        // Recent likes say most about current taste: pick from the newest 50.
        let mut pool: Vec<u64> = liked.iter().take(50).copied().collect();
        while seeds.len() < 6 && !pool.is_empty() {
            let i = (next() % pool.len() as u64) as usize;
            seeds.push(pool.swap_remove(i));
        }
        if seeds.is_empty() {
            return Err("для рекомендаций нужен вход в SoundCloud или играющий трек".into());
        }
        let workers: Vec<_> = seeds
            .into_iter()
            .map(|id| {
                let client = self.clone();
                thread::spawn(move || client.related(id))
            })
            .collect();
        let mut seen: std::collections::HashSet<u64> = liked.into_iter().collect();
        let mut out = Vec::new();
        for w in workers {
            match w.join() {
                Ok(Ok(tracks)) => out.extend(tracks.into_iter().filter(|t| seen.insert(t.id))),
                Ok(Err(e)) => eprintln!("recommendations: {e}"),
                Err(_) => {}
            }
        }
        for i in (1..out.len()).rev() {
            let j = (next() % (i as u64 + 1)) as usize;
            out.swap(i, j);
        }
        Ok(out)
    }

    /// Round trip to SoundCloud through the proxy, in milliseconds: the second of
    /// two requests, so it measures the connection, not the TLS handshake.
    pub fn ping(&self) -> Res<u64> {
        let once = || -> Res<u64> {
            let start = std::time::Instant::now();
            match self.agent().get(&format!("{API}/")).call() {
                Ok(_) | Err(ureq::Error::Status(..)) => Ok(start.elapsed().as_millis() as u64),
                Err(ureq::Error::Transport(t)) => {
                    Err(format!("{}: {}", t.kind(), t.message().unwrap_or("")).into())
                }
            }
        };
        let first = once()?;
        Ok(once().map(|second| second.min(first)).unwrap_or(first))
    }

    /// An authorised write (PUT/DELETE/POST) to api-v2.
    fn write(&self, method: &str, path: &str, body: Option<Value>) -> Res<()> {
        let token = self
            .token
            .lock()
            .unwrap()
            .clone()
            .ok_or("нужен вход в SoundCloud")?;
        let id = self.client_id(false)?;
        // Sent the way the website sends it.
        let req = self
            .agent()
            .request(method, &format!("{API}{path}"))
            .set("Origin", "https://soundcloud.com")
            .set("Referer", "https://soundcloud.com/")
            .set("Accept", "application/json, text/javascript, */*; q=0.1")
            .set("Authorization", &format!("OAuth {token}"))
            .query("client_id", &id)
            .query("app_locale", "en");
        let result = match body {
            Some(b) => req.send_json(b),
            None if method == "DELETE" => req.call(),
            None => req.set("Content-Type", "application/json").send_bytes(b"{}"),
        };
        match result {
            Ok(_) => Ok(()),
            Err(ureq::Error::Status(code, resp)) => {
                // Logged so a refusal can be diagnosed; the token is not in the body.
                let text: String = resp.into_string().unwrap_or_default().chars().take(300).collect();
                eprintln!("soundcloud: {method} {path}: HTTP {code}: {text}");
                let lower = text.to_lowercase();
                let hint = if lower.contains("captcha") || lower.contains("datadome") {
                    ": SoundCloud принял запрос за бота (капча)"
                } else if code == 401 {
                    ": токен входа не подходит"
                } else {
                    ""
                };
                Err(format!("HTTP {code}{hint}").into())
            }
            Err(ureq::Error::Transport(t)) => Err(t.kind().to_string().into()),
        }
    }

    /// Adds a track to the account's likes, or removes it.
    pub fn set_like(&self, track_id: u64, on: bool) -> Res<()> {
        let (uid, _) = self.me()?;
        let path = format!("/users/{uid}/track_likes/{track_id}");
        self.write(if on { "PUT" } else { "DELETE" }, &path, None)
    }

    /// Ids of liked tracks, newest first (up to 200): one request, no track details.
    pub fn liked_list(&self) -> Res<Vec<u64>> {
        let (uid, _) = self.me()?;
        let v = self.api(&format!("/users/{uid}/track_likes"), &[("limit", "200")])?;
        Ok(v.get("collection")
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(|i| i.pointer("/track/id").and_then(Value::as_u64))
                    .collect()
            })
            .unwrap_or_default())
    }

    /// Liked track ids, for the hearts on the page.
    pub fn liked_ids(&self) -> Res<std::collections::HashSet<u64>> {
        Ok(self.liked_list()?.into_iter().collect())
    }

    /// Playlists the account owns (the ones tracks can be added to).
    pub fn owned_playlists(&self) -> Res<Vec<PlaylistInfo>> {
        let (uid, name) = self.me()?;
        let v = self.api(&format!("/users/{uid}/playlists"), &[("limit", "100")])?;
        let c: Collection<Value> = serde_json::from_value(v)?;
        let mut out: Vec<PlaylistInfo> = c.collection.iter().filter_map(playlist_info).collect();
        if out.is_empty() {
            out = self
                .library()?
                .into_iter()
                .filter(|p| p.user == name && p.id.parse::<u64>().is_ok())
                .collect();
        }
        Ok(out)
    }

    /// Appends a track to one of the account's playlists.
    pub fn add_to_playlist(&self, playlist: u64, track_id: u64) -> Res<()> {
        let v = self.api(&format!("/playlists/{playlist}"), &[])?;
        let mut ids: Vec<u64> = v
            .get("tracks")
            .and_then(Value::as_array)
            .map(|t| t.iter().filter_map(|x| x.get("id").and_then(Value::as_u64)).collect())
            .unwrap_or_default();
        if ids.contains(&track_id) {
            return Err("трек уже есть в этом плейлисте".into());
        }
        ids.push(track_id);
        let body = serde_json::json!({ "playlist": { "tracks": ids } });
        self.write("PUT", &format!("/playlists/{playlist}"), Some(body))
    }

    /// A playlist from search or the library: numeric id, or a system playlist via its link.
    pub fn open_playlist(&self, id: &str, link: &str) -> Res<Vec<Track>> {
        match id.parse::<u64>() {
            Ok(n) => self.playlist(n),
            Err(_) if link.starts_with("https://") => self.resolve(link),
            Err(_) => {
                let v = self.api(&format!("/system-playlists/{id}"), &[])?;
                self.tracks_of(&v)
            }
        }
    }

    /// «Включи альбом …»: the best matching album; a playlist if no album has tracks.
    pub fn search_album(&self, q: &str) -> Res<Vec<Track>> {
        let v = self.api("/search/albums", &[("q", q), ("limit", "5")])?;
        let c: Collection<Value> = serde_json::from_value(v)?;
        for p in c.collection {
            let Some(id) = p.get("id").and_then(Value::as_u64) else {
                continue;
            };
            let tracks = self.playlist(id)?;
            if !tracks.is_empty() {
                return Ok(tracks);
            }
        }
        self.search_playlist(q)
    }

    pub fn search_playlist(&self, q: &str) -> Res<Vec<Track>> {
        let v = self.api("/search/playlists", &[("q", q), ("limit", "5")])?;
        let c: Collection<Value> = serde_json::from_value(v)?;
        for p in c.collection {
            let Some(id) = p.get("id").and_then(Value::as_u64) else {
                continue;
            };
            let tracks = self.playlist(id)?;
            if !tracks.is_empty() {
                return Ok(tracks);
            }
        }
        Ok(Vec::new())
    }

    fn playlist(&self, id: u64) -> Res<Vec<Track>> {
        let v = self.api(&format!("/playlists/{id}"), &[])?;
        self.tracks_of(&v)
    }

    fn tracks_of(&self, playlist: &Value) -> Res<Vec<Track>> {
        let raw = playlist
            .get("tracks")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let tracks: Vec<Track> = raw
            .into_iter()
            .filter_map(|t| serde_json::from_value(t).ok())
            .collect();
        self.hydrate(tracks)
    }

    /// Playlists return full objects only for the first few tracks; fetch the rest by id.
    /// Tracks by their ids, in that order (unavailable ones left out).
    pub fn by_ids(&self, ids: &[u64]) -> Res<Vec<Track>> {
        self.hydrate(ids.iter().map(|id| Track::from_id(*id)).collect())
    }

    fn hydrate(&self, tracks: Vec<Track>) -> Res<Vec<Track>> {
        let missing: Vec<u64> = tracks.iter().filter(|t| !t.hydrated()).map(|t| t.id).collect();
        let mut full = std::collections::HashMap::new();
        for chunk in missing.chunks(50) {
            let ids = chunk.iter().map(u64::to_string).collect::<Vec<_>>().join(",");
            let v = self.api("/tracks", &[("ids", ids.as_str())])?;
            let got: Vec<Track> = serde_json::from_value(v)?;
            for t in got {
                full.insert(t.id, t);
            }
        }
        Ok(tracks
            .into_iter()
            .filter_map(|t| if t.hydrated() { Some(t) } else { full.remove(&t.id) })
            .filter(|t| t.transcoding().is_some())
            .collect())
    }

    /// Track ids from the logged-in home page of soundcloud.com (/mixed-selections):
    /// «More of what you like» first; without it, the daily and weekly mixes.
    fn personalized(&self) -> Res<Vec<u64>> {
        let v = self.api("/mixed-selections", &[("limit", "20")])?;
        let selections = v.get("collection").and_then(Value::as_array).cloned().unwrap_or_default();
        let text = |s: &Value, k: &str| s.get(k).and_then(Value::as_str).unwrap_or("").to_lowercase();
        let names: Vec<String> = selections.iter().map(|s| text(s, "title")).collect();
        eprintln!("recommendations: home page has {names:?}");
        let wanted = |s: &Value| {
            let all = format!("{} {} {}", text(s, "title"), text(s, "id"), text(s, "urn"));
            all.contains("more of what you like") || all.contains("personalized") || all.contains("больше того")
        };
        let mixes = |s: &Value| {
            let all = format!("{} {} {}", text(s, "title"), text(s, "id"), text(s, "urn"));
            all.contains("mix") || all.contains("микс") || all.contains("daily") || all.contains("weekly")
        };
        let mut ids = Vec::new();
        let picks: [&dyn Fn(&Value) -> bool; 2] = [&wanted, &mixes];
        for pick in picks {
            let mut urns = Vec::new();
            for s in selections.iter().filter(|s| pick(s)) {
                collect_picks(s, &mut ids, &mut urns);
            }
            for urn in urns.into_iter().take(12) {
                match self.api(&format!("/system-playlists/{urn}"), &[]) {
                    Ok(p) => collect_picks(&p, &mut ids, &mut Vec::new()),
                    Err(e) => eprintln!("recommendations: {urn}: {e}"),
                }
            }
            if ids.len() >= 10 {
                break;
            }
        }
        let mut seen = std::collections::HashSet::new();
        ids.retain(|id| seen.insert(*id));
        Ok(ids)
    }

    /// The shelves of soundcloud.com's home page for the logged-in user («Mixed for …»,
    /// «More of what you like», daily / weekly mixes…), each with its playlists.
    pub fn home(&self) -> Res<Vec<Shelf>> {
        let v = self.api("/mixed-selections", &[("limit", "20")])?;
        let mut out = shelves(&v);
        self.fill_covers(&mut out);
        Ok(out)
    }

    /// SoundCloud sends some shelf items («Related tracks: …», the genre charts, your own
    /// playlists) with neither a cover nor their tracks, so they showed their author's picture:
    /// SoundCloud's logo, or your avatar. soundcloud.com shows the first track's cover there,
    /// so it is looked up (several at once) and remembered for the next visit.
    fn fill_covers(&self, shelves: &mut [Shelf]) {
        static KNOWN: std::sync::OnceLock<Mutex<std::collections::HashMap<String, String>>> = std::sync::OnceLock::new();
        let known = KNOWN.get_or_init(Default::default);
        let mut wanted = Vec::new();
        for p in shelves.iter_mut().flat_map(|s| s.items.iter_mut()).filter(|p| bare_cover(&p.art)) {
            match known.lock().unwrap().get(&p.id) {
                Some(art) => p.art = art.clone(),
                None if !wanted.contains(&p.id) => wanted.push(p.id.clone()),
                None => {}
            }
        }
        wanted.truncate(30);
        if wanted.is_empty() {
            return;
        }
        let found: Vec<(String, String)> = std::thread::scope(|scope| {
            let jobs: Vec<_> = wanted
                .chunks(wanted.len().div_ceil(6))
                .map(|ids| scope.spawn(move || ids.iter().filter_map(|id| Some((id.clone(), self.cover_of_playlist(id)?))).collect::<Vec<_>>()))
                .collect();
            jobs.into_iter().flat_map(|j| j.join().unwrap_or_default()).collect()
        });
        let mut known = known.lock().unwrap();
        for (id, art) in found {
            known.insert(id, art);
        }
        for p in shelves.iter_mut().flat_map(|s| s.items.iter_mut()).filter(|p| bare_cover(&p.art)) {
            if let Some(art) = known.get(&p.id) {
                p.art = art.clone();
            }
        }
    }

    /// A playlist's cover as soundcloud.com shows it: its own, else its first track's.
    fn cover_of_playlist(&self, id: &str) -> Option<String> {
        let path = if id.bytes().all(|b| b.is_ascii_digit()) { format!("/playlists/{id}") } else { format!("/system-playlists/{id}") };
        let v = self.api(&path, &[]).ok()?;
        if let Some(p) = playlist_info(&v).filter(|p| !bare_cover(&p.art)) {
            return Some(p.art);
        }
        // The tracks come as bare ids: the first few are fetched for their covers.
        let ids: Vec<String> = v.get("tracks")?.as_array()?.iter().filter_map(|t| t.get("id")?.as_u64()).take(5).map(|n| n.to_string()).collect();
        if ids.is_empty() {
            return None;
        }
        let tracks = self.api("/tracks", &[("ids", ids.join(",").as_str())]).ok()?;
        first_cover(&tracks)
    }

    pub fn related(&self, id: u64) -> Res<Vec<Track>> {
        let v = self.api(&format!("/tracks/{id}/related"), &[("limit", "20")])?;
        let c: Collection<Track> = serde_json::from_value(v)?;
        Ok(c.collection.into_iter().filter(|t| t.transcoding().is_some()).collect())
    }

    /// A track's cover and id by its soundcloud.com link (for history entries saved without them).
    pub fn cover_of(&self, link: &str) -> Res<(String, u64)> {
        let v = self.api("/resolve", &[("url", link)])?;
        if v.get("kind").and_then(Value::as_str) != Some("track") {
            return Err("not a track".into());
        }
        let t = serde_json::from_value::<Track>(v)?;
        Ok((t.cover().unwrap_or_default(), t.id))
    }

    /// A soundcloud.com link to a track, playlist or user.
    pub fn resolve(&self, link: &str) -> Res<Vec<Track>> {
        let v = self.api("/resolve", &[("url", link)])?;
        match v.get("kind").and_then(Value::as_str) {
            Some("track") => {
                let t = serde_json::from_value::<Track>(v)?;
                if t.transcoding().is_none() {
                    // Say why instead of «nothing found».
                    eprintln!("resolve: {} not playable: {}", t.name(), t.why_not_playable());
                    return Err(format!("«{}» не включить: {}", t.name(), t.why_not_playable()).into());
                }
                Ok(vec![t])
            }
            Some("playlist" | "system-playlist") => self.tracks_of(&v),
            Some("user") => {
                let id = v.get("id").and_then(Value::as_u64).ok_or("user without id")?;
                let t = self.api(&format!("/users/{id}/tracks"), &[("limit", "50")])?;
                let c: Collection<Track> = serde_json::from_value(t)?;
                Ok(c.collection.into_iter().filter(|t| t.transcoding().is_some()).collect())
            }
            other => Err(format!("unsupported link kind {other:?}").into()),
        }
    }

    /// Signed media URL (progressive file or HLS playlist) for a transcoding.
    pub fn media_url(&self, track: &Track, t: &Transcoding) -> Res<String> {
        let auth = track.track_authorization.clone().unwrap_or_default();
        let v = self.api(&t.url, &[("track_authorization", auth.as_str())])?;
        Ok(v.get("url")
            .and_then(Value::as_str)
            .ok_or("no url in stream response")?
            .to_owned())
    }

    /// Opens a streaming reader for the track's audio.
    pub fn open(&self, track: &Track) -> Res<(Box<dyn Read + Send + Sync>, Kind)> {
        self.open_from(track, 0).map(|(r, k, _)| (r, k))
    }

    /// Like `open`, but an HLS stream starts at the segment holding `start_ms`
    /// (not downloaded from the beginning); the third value is where its sound
    /// starts (ms; 0 for a progressive file, which starts at the beginning).
    pub fn open_from(&self, track: &Track, start_ms: u64) -> Res<(Box<dyn Read + Send + Sync>, Kind, u64)> {
        let mut errors = Vec::new();
        let label = |t: &Transcoding| format!("{} {}", t.format.protocol, t.format.mime_type);
        let mut candidates = track.candidates();
        {
            // Skip straight to formats that worked: each 404 costs a round trip through the proxy.
            let dead = self.dead.lock().unwrap();
            candidates.sort_by_key(|c| dead.contains(&label(c.0)));
        }
        for (t, kind) in candidates {
            let name = label(t);
            match self.open_one(track, t, kind, start_ms) {
                Ok((reader, started)) => {
                    eprintln!("stream: {name}");
                    return Ok((Box::new(crate::buffer::Buffered::new(reader)), kind, started));
                }
                Err(e) => {
                    let e = e.to_string();
                    if e.contains("HTTP 404") {
                        self.dead.lock().unwrap().insert(name.clone());
                    }
                    errors.push(format!("{name}: {e}"));
                }
            }
        }
        Err(format!(
            "нет рабочего потока. Форматы: [{}]. Ошибки: [{}]",
            track.formats(),
            errors.join("; ")
        )
        .into())
    }

    fn open_one(&self, track: &Track, t: &Transcoding, _kind: Kind, start_ms: u64) -> Res<(Box<dyn Read + Send + Sync>, u64)> {
        let url = self.media_url(track, t)?;
        let refresh: Refresh = {
            let client = self.clone();
            let track = track.clone();
            let t = t.clone();
            Arc::new(move || client.media_url(&track, &t).ok())
        };
        if t.format.protocol == "progressive" {
            Ok((Box::new(Http::new(self.agent(), url, Some(refresh))), 0))
        } else {
            let (hls, started) = Hls::new(self.agent(), &url, refresh, start_ms)?;
            Ok((Box::new(hls), started))
        }
    }
}

/// Builds an HTTP agent; `proxy` is "" (direct), "http://[user:pass@]host:port",
/// "socks5://[user:pass@]host:port" or a bare "host:port" (treated as HTTP).
pub fn build_agent(proxy: &str) -> Res<ureq::Agent> {
    let mut b = ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(10))
        .timeout_read(Duration::from_secs(30))
        // Reuse connections: every new one costs several round trips through the proxy.
        .max_idle_connections_per_host(8)
        .user_agent(UA)
        .middleware(browser_headers)
        .resolver(crate::dns::Resolver::new());
    // «Обход блокировок» on: the site name is hidden from DPI, no proxy needed.
    match crate::bypass::active() {
        Some(crate::bypass::Strategy::Direct) => return Ok(b.build()),
        Some(strategy) => return Ok(b.tls_connector(Arc::new(crate::bypass::Connector::new(strategy))).build()),
        None => {}
    }
    let proxy = proxy.trim();
    if !proxy.is_empty() {
        let full = if proxy.contains("://") { proxy.to_owned() } else { format!("http://{proxy}") };
        b = b.proxy(ureq::Proxy::new(&full)?);
    }
    Ok(b.build())
}

/// Checks that soundcloud.com and its API answer through the proxy.
pub fn test_proxy(proxy: &str) -> Res<String> {
    let agent = build_agent(proxy)?;
    let started = std::time::Instant::now();
    let resp = agent.get("https://soundcloud.com/").call()?;
    let site = resp.status();
    let api = match agent.get(&format!("{API}/search/tracks?q=test")).call() {
        Ok(r) => r.status(),
        Err(ureq::Error::Status(code, _)) => code, // 401 without client_id means the API is reachable
        Err(e) => return Err(format!("API недоступен: {e}").into()),
    };
    Ok(format!(
        "soundcloud.com: HTTP {site}, api-v2: HTTP {api}, {} мс",
        started.elapsed().as_millis()
    ))
}

type Refresh = Arc<dyn Fn() -> Option<String> + Send + Sync>;

/// Track ids and system playlists («soundcloud:system-playlists:…») anywhere in
/// a home page selection or a playlist.
#[derive(Debug, Clone, serde::Serialize)]
pub struct Shelf {
    pub title: String,
    pub items: Vec<PlaylistInfo>,
}

/// /mixed-selections → shelves of playlists (system playlists — the mixes — and plain ones).
fn shelves(v: &Value) -> Vec<Shelf> {
    fn walk(v: &Value, out: &mut Vec<PlaylistInfo>) {
        match v {
            Value::Array(a) => a.iter().for_each(|i| walk(i, out)),
            Value::Object(o) => {
                let kind = o.get("kind").and_then(Value::as_str).unwrap_or("");
                if kind == "playlist" || kind == "system-playlist" {
                    if let Some(p) = playlist_info(v).filter(|p| !p.title.is_empty()) {
                        out.push(p);
                    }
                    return;
                }
                for (k, child) in o {
                    if k != "user" {
                        walk(child, out);
                    }
                }
            }
            _ => {}
        }
    }
    let mut out = Vec::new();
    for s in v.get("collection").and_then(Value::as_array).into_iter().flatten() {
        let title = s.get("title").and_then(Value::as_str).unwrap_or("").trim().to_owned();
        let mut items = Vec::new();
        walk(s.get("items").unwrap_or(s), &mut items);
        let mut seen = std::collections::HashSet::new();
        items.retain(|p| seen.insert(p.id.clone()));
        items.truncate(12);
        if !title.is_empty() && !items.is_empty() {
            out.push(Shelf { title, items });
        }
    }
    out.truncate(8);
    out
}

fn collect_picks(v: &Value, ids: &mut Vec<u64>, urns: &mut Vec<String>) {
    match v {
        Value::Array(items) => items.iter().for_each(|i| collect_picks(i, ids, urns)),
        Value::Object(o) => {
            let kind = o.get("kind").and_then(Value::as_str).unwrap_or("");
            if kind == "track" {
                if let Some(id) = o.get("id").and_then(Value::as_u64) {
                    ids.push(id);
                }
                return;
            }
            if let Some(urn) = o.get("urn").and_then(Value::as_str) {
                // A mix whose tracks are not inside: fetched by urn.
                if urn.starts_with("soundcloud:system-playlists:") && !o.contains_key("tracks") {
                    urns.push(urn.to_owned());
                }
            }
            for (k, child) in o {
                if k != "user" {
                    collect_picks(child, ids, urns);
                }
            }
        }
        _ => {}
    }
}

pub(crate) fn script_urls(html: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = html;
    while let Some(i) = rest.find("src=\"") {
        rest = &rest[i + 5..];
        let Some(end) = rest.find('"') else { break };
        let url = &rest[..end];
        if url.contains("sndcdn.com/assets/") && url.ends_with(".js") {
            out.push(url.to_owned());
        }
        rest = &rest[end..];
    }
    out
}

/// `window.__sc_version="1761234567"` from the site's HTML.
fn app_version(html: &str) -> Option<String> {
    let i = html.find("__sc_version")?;
    let v: String = html[i..].chars().skip_while(|c| !c.is_ascii_digit()).take_while(char::is_ascii_digit).collect();
    (v.len() >= 6 && v.len() <= 16).then_some(v)
}

fn find_client_id(js: &str) -> Option<String> {
    for pat in ["client_id:\"", "client_id=\"", "client_id=", "\"client_id\":\""] {
        let mut rest = js;
        while let Some(i) = rest.find(pat) {
            rest = &rest[i + pat.len()..];
            let id: String = rest.chars().take_while(|c| c.is_ascii_alphanumeric()).collect();
            if id.len() == 32 {
                return Some(id);
            }
        }
    }
    None
}

/// HTTP body reader that resumes with a Range request after a dropped connection
/// and asks for a fresh signed URL if the old one has expired.
pub struct Http {
    agent: ureq::Agent,
    url: String,
    pos: u64,
    inner: Option<Box<dyn Read + Send + Sync>>,
    refresh: Option<Refresh>,
    finished: bool,
}

impl Http {
    fn new(agent: ureq::Agent, url: String, refresh: Option<Refresh>) -> Self {
        Self { agent, url, pos: 0, inner: None, refresh, finished: false }
    }

    fn connect(&mut self) -> io::Result<()> {
        let mut req = self.agent.get(&self.url);
        if self.pos > 0 {
            req = req.set("Range", &format!("bytes={}-", self.pos));
        }
        match req.call() {
            Ok(resp) => {
                if self.pos > 0 && resp.status() == 200 {
                    // Server ignored Range: skip what we already have.
                    let mut r = resp.into_reader();
                    io::copy(&mut (&mut r).take(self.pos), &mut io::sink())?;
                    self.inner = Some(r);
                } else {
                    self.inner = Some(resp.into_reader());
                }
                Ok(())
            }
            Err(ureq::Error::Status(416, _)) => {
                self.finished = true;
                Ok(())
            }
            Err(ureq::Error::Status(code @ (401 | 403 | 404 | 410), _)) => {
                if let Some(url) = self.refresh.as_ref().and_then(|f| f()) {
                    self.url = url;
                }
                Err(io::Error::other(format!("HTTP {code}")))
            }
            Err(e) => Err(io::Error::other(e.to_string())),
        }
    }
}

/// Attempts per read before an error is passed up: a flaky proxy drops
/// connections now and then, and a dropped download should not end the track.
const ATTEMPTS: u32 = 6;

fn backoff(failures: u32) {
    thread::sleep(Duration::from_millis(500 * u64::from(failures.min(6))));
}

impl Read for Http {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let mut failures = 0;
        loop {
            if self.finished {
                return Ok(0);
            }
            if self.inner.is_none() {
                if let Err(e) = self.connect() {
                    failures += 1;
                    if failures >= ATTEMPTS {
                        return Err(e);
                    }
                    eprintln!("download: {e}; retry {failures}");
                    backoff(failures);
                    continue;
                }
                continue;
            }
            match self.inner.as_mut().unwrap().read(buf) {
                Ok(n) => {
                    self.pos += n as u64;
                    if n == 0 {
                        self.finished = true;
                    }
                    return Ok(n);
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => {
                    // Resumes from the same byte with a Range request.
                    self.inner = None;
                    failures += 1;
                    if failures >= ATTEMPTS {
                        return Err(e);
                    }
                    eprintln!("download: {e}; retry {failures}");
                    backoff(failures);
                }
            }
        }
    }
}

/// Concatenates the segments of an HLS media playlist into one byte stream.
pub struct Hls {
    agent: ureq::Agent,
    urls: Vec<String>,
    /// Which of `urls` to read, in turn (all of them, or the init segment and
    /// those from a moment on: «Слушать здесь» from where the speaker is).
    plan: Vec<usize>,
    next: usize,
    current: Option<Http>,
    refresh: Refresh,
}

pub fn parse_m3u8(base: &str, text: &str) -> Res<Vec<String>> {
    let join = |u: &str| -> String {
        if u.starts_with("http://") || u.starts_with("https://") {
            u.to_owned()
        } else if let Some(stripped) = u.strip_prefix('/') {
            let origin_end = base.find("://").map(|i| i + 3).unwrap_or(0);
            let host_end = base[origin_end..].find('/').map(|i| origin_end + i).unwrap_or(base.len());
            format!("{}/{}", &base[..host_end], stripped)
        } else {
            let dir = base.split('?').next().unwrap_or(base);
            let dir = &dir[..dir.rfind('/').map(|i| i + 1).unwrap_or(dir.len())];
            format!("{dir}{u}")
        }
    };
    let mut urls = Vec::new();
    for line in text.lines().map(str::trim) {
        if let Some(attrs) = line.strip_prefix("#EXT-X-KEY:") {
            if !attrs.contains("METHOD=NONE") {
                return Err("encrypted HLS is not supported".into());
            }
        } else if let Some(attrs) = line.strip_prefix("#EXT-X-MAP:") {
            if let Some(i) = attrs.find("URI=\"") {
                let rest = &attrs[i + 5..];
                if let Some(end) = rest.find('"') {
                    urls.insert(0, join(&rest[..end]));
                }
            }
        } else if !line.is_empty() && !line.starts_with('#') {
            urls.push(join(line));
        }
    }
    if urls.is_empty() {
        return Err("empty HLS playlist".into());
    }
    Ok(urls)
}

/// What to read of a media playlist (indices as `parse_m3u8` returns its URLs)
/// to start at `start_ms`, and where that really starts: the beginning of the
/// segment holding the moment (MP4 can only start on a segment; its init segment
/// always comes first). Without one duration per segment: everything, from 0.
pub fn segment_plan(text: &str, start_ms: u64) -> (Vec<usize>, u64) {
    let mut durations = Vec::new();
    let mut segments = 0;
    let mut map = false;
    for line in text.lines().map(str::trim) {
        if let Some(d) = line.strip_prefix("#EXTINF:") {
            let secs: f64 = d.split(',').next().unwrap_or("").trim().parse().unwrap_or(-1.0);
            durations.push(if secs >= 0.0 { (secs * 1000.0).round() as u64 } else { u64::MAX });
        } else if line.starts_with("#EXT-X-MAP:") {
            map = true;
        } else if !line.is_empty() && !line.starts_with('#') {
            segments += 1;
        }
    }
    let first = usize::from(map);
    let all = (0..first + segments).collect();
    if segments == 0 || durations.len() != segments || durations.contains(&u64::MAX) {
        return (all, 0);
    }
    let (mut k, mut at) = (0, 0u64);
    for (i, d) in durations.iter().enumerate() {
        k = i;
        if at + d > start_ms || i + 1 == segments {
            break;
        }
        at += d;
    }
    if k == 0 {
        return (all, 0);
    }
    let mut plan: Vec<usize> = if map { vec![0] } else { Vec::new() };
    plan.extend(first + k..first + segments);
    (plan, at)
}

impl Hls {
    /// The playlist at `url`, read from the segment holding `start_ms`; also
    /// where the sound really starts (ms).
    fn new(agent: ureq::Agent, url: &str, refresh: Refresh, start_ms: u64) -> Res<(Self, u64)> {
        let text = agent.get(url).call()?.into_string()?;
        let urls = parse_m3u8(url, &text)?;
        let (plan, started) = segment_plan(&text, start_ms);
        if plan.iter().any(|&i| i >= urls.len()) {
            let plan = (0..urls.len()).collect();
            return Ok((Self { agent, urls, plan, next: 0, current: None, refresh }, 0));
        }
        Ok((Self { agent, urls, plan, next: 0, current: None, refresh }, started))
    }

    fn reload(&mut self) -> bool {
        let Some(url) = (self.refresh)() else { return false };
        let Ok(resp) = self.agent.get(&url).call() else { return false };
        let Ok(text) = resp.into_string() else { return false };
        match parse_m3u8(&url, &text) {
            Ok(urls) if urls.len() == self.urls.len() => {
                self.urls = urls;
                true
            }
            _ => false,
        }
    }
}

impl Read for Hls {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        loop {
            if self.current.is_none() {
                if self.next >= self.plan.len() {
                    return Ok(0);
                }
                self.current = Some(Http::new(self.agent.clone(), self.urls[self.plan[self.next]].clone(), None));
                self.next += 1;
            }
            let seg = self.current.as_mut().unwrap();
            match seg.read(buf) {
                Ok(0) => self.current = None,
                Ok(n) => return Ok(n),
                Err(e) => {
                    // Segment URLs are signed and expire: get fresh ones and continue
                    // the same segment from the same byte, a few times over.
                    let pos = seg.pos;
                    let mut last = e;
                    let mut resumed = false;
                    for attempt in 1..=3u32 {
                        eprintln!("download: segment {}: {last}; fresh links, attempt {attempt}", self.next);
                        backoff(attempt);
                        self.reload();
                        let mut fresh = Http::new(self.agent.clone(), self.urls[self.plan[self.next - 1]].clone(), None);
                        fresh.pos = pos;
                        match fresh.read(buf) {
                            Ok(n) => {
                                self.current = if n == 0 { None } else { Some(fresh) };
                                if n > 0 {
                                    return Ok(n);
                                }
                                resumed = true;
                                break;
                            }
                            Err(e) => last = e,
                        }
                    }
                    if !resumed {
                        return Err(last);
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    /// A fake SoundCloud: /me takes only the token "good" with the client_id "NEW…"; the site
    /// gives that id from its script. The speaker starts with yesterday's id.
    fn fake_soundcloud() -> (String, std::sync::Arc<std::sync::atomic::AtomicUsize>) {
        use std::io::{Read as _, Write as _};
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://127.0.0.1:{}", l.local_addr().unwrap().port());
        let scrapes = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let (b, n) = (base.clone(), scrapes.clone());
        thread::spawn(move || {
            for c in l.incoming().flatten() {
                let mut c = c;
                let mut buf = vec![0u8; 8192];
                let k = c.read(&mut buf).unwrap_or(0);
                let req = String::from_utf8_lossy(&buf[..k]).to_string();
                let line = req.lines().next().unwrap_or("").to_owned();
                let new_id = line.contains("client_id=NEWNEWNEWNEWNEWNEWNEWNEWNEWNEW12");
                let token = req.to_lowercase().contains("authorization: oauth good");
                let (code, body) = if line.starts_with("GET / ") {
                    n.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    ("200 OK", format!("<script src=\"{b}/a-v2.sndcdn.com/assets/app.js\"></script>"))
                } else if line.contains("/assets/app.js") {
                    ("200 OK", "x={client_id:\"NEWNEWNEWNEWNEWNEWNEWNEWNEWNEW12\"}".to_owned())
                } else if line.contains("/me") && new_id && token {
                    ("200 OK", "{\"id\":7,\"username\":\"demo\"}".to_owned())
                } else {
                    ("401 Unauthorized", "{}".to_owned())
                };
                let _ = write!(c, "HTTP/1.1 {code}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
            }
        });
        (base, scrapes)
    }

    #[test]
    fn a_new_client_id_is_not_a_lost_login() {
        // Since the first versions: once a day or two SoundCloud changes its client_id; the old
        // one is refused with and without the token, and soundya said «вход устарел», although
        // the token was fine. Now: the site's new id, the token with it, and the login stays.
        let (base, scrapes) = fake_soundcloud();
        let dir = std::env::temp_dir().join(format!("soundya-scid-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("client_id"), "OLDOLDOLDOLDOLDOLDOLDOLDOLDOLD12").unwrap();
        let mut c = Client::new(Some(dir.join("client_id")), "");
        c.api_base = base.clone();
        c.site = format!("{base}/");
        c.set_token("good");
        let (id, name) = c.me().expect("the login must survive a new client_id");
        assert_eq!((id, name.as_str()), (7, "demo"));
        assert!(!c.token_refused());
        assert_eq!(scrapes.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(std::fs::read_to_string(dir.join("client_id")).unwrap(), "NEWNEWNEWNEWNEWNEWNEWNEWNEWNEW12");
        // A really dead token: refused with the current id too — then, and only then, it is the login.
        c.set_token("revoked");
        assert!(c.me().is_err());
        assert_eq!(c.verify_token(), Some(false));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    fn b64url(data: &[u8]) -> String {
        const A: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
        let mut out = String::new();
        for chunk in data.chunks(3) {
            let n = chunk.iter().enumerate().fold(0u32, |n, (i, b)| n | (u32::from(*b) << (16 - 8 * i)));
            for i in 0..=chunk.len() {
                out.push(A[((n >> (18 - 6 * i)) & 63) as usize] as char);
            }
        }
        out
    }

    fn jwt(exp: u64, client_id: &str) -> String {
        let body = serde_json::json!({ "exp": exp, "client_id": client_id, "iss": "https://secure.soundcloud.com" });
        format!("{}.{}.sig", b64url(br#"{"alg":"RS256","typ":"JWT"}"#), b64url(body.to_string().as_bytes()))
    }

    #[test]
    fn the_day_long_login_renews_itself() {
        // Since 2026 soundcloud.com's oauth_token is a JWT that lives 24 hours; the site renews it
        // with oauth_refresh_token. soundya kept the old one and said «вход устарел» every day.
        use std::io::{Read as _, Write as _};
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", l.local_addr().unwrap());
        let fresh = jwt(unix_now() + 86400, "WEBCLIENT");
        let fresh2 = fresh.clone();
        let seen = Arc::new(Mutex::new(Vec::<String>::new()));
        let seen2 = seen.clone();
        thread::spawn(move || {
            for c in l.incoming().flatten() {
                let mut c = c;
                let mut buf = vec![0u8; 8192];
                let k = c.read(&mut buf).unwrap_or(0);
                let req = String::from_utf8_lossy(&buf[..k]).to_string();
                let body = req.split("\r\n\r\n").nth(1).unwrap_or("").to_owned();
                seen2.lock().unwrap().push(body.clone());
                let (code, out) = if body.contains("refresh_token=R1") {
                    ("200 OK", format!("{{\"access_token\":\"{fresh2}\",\"refresh_token\":\"R2\",\"expires_in\":86400}}"))
                } else {
                    ("400 Bad Request", "{\"error\":\"invalid_grant\"}".to_owned())
                };
                let _ = write!(c, "HTTP/1.1 {code}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{out}", out.len());
            }
        });
        let mut c = Client::new(None, "");
        c.token_url = format!("{base}/oauth/token");
        // Old, opaque tokens never run out and are never renewed.
        c.set_token("2-123456-789-abcdef");
        assert_eq!((c.token_exp(), c.token_expired(), c.renew_due()), (None, false, false));
        // A day-old JWT with its refresh token: due, renewed with the token's own client_id.
        c.set_token(&jwt(unix_now() - 60, "WEBCLIENT"));
        c.set_refresh("R1");
        assert!(c.token_expired());
        assert!(c.renew_due());
        c.renew().expect("renewed");
        assert!(seen.lock().unwrap()[0].contains("client_id=WEBCLIENT") && seen.lock().unwrap()[0].contains("grant_type=refresh_token"));
        assert!(!c.token_expired());
        assert_eq!(c.take_renewed(), Some((fresh.clone(), "R2".to_owned())));
        assert_eq!(c.take_renewed(), None);
        // Not due again until 10 minutes before the new one runs out.
        assert!(!c.renew_due());
        // A spent refresh token: said once, not hammered.
        c.set_token(&jwt(unix_now() - 60, "WEBCLIENT"));
        c.set_refresh("SPENT");
        assert!(c.renew().is_err());
        assert!(c.renew_error().unwrap().contains("400"));
        assert!(!c.renew_due());
        assert_eq!(c.verify_token(), Some(false));
        assert!(c.token_refused());
    }

    #[test]
    fn a_dropped_connection_is_tried_once_more() {
        use std::io::{Read as _, Write as _};
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        thread::spawn(move || {
            // First connection: closed at once (the stub / a dropped proxy). Second: answers.
            drop(l.accept().unwrap().0);
            let (mut c, _) = l.accept().unwrap();
            let mut buf = [0u8; 1024];
            let _ = c.read(&mut buf);
            let _ = c.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok");
        });
        let agent = ureq::AgentBuilder::new().build();
        let r = get_retry(&agent, &format!("http://127.0.0.1:{port}/art.jpg"), Duration::from_secs(5)).unwrap();
        assert_eq!(r.into_string().unwrap(), "ok");
    }

    #[test]
    fn home_shelves_from_mixed_selections() {
        let v: Value = serde_json::from_str(r#"{"collection":[
            {"title":"Mixed for Rodion","items":{"collection":[
                {"kind":"system-playlist","urn":"soundcloud:system-playlists:daily-mix:1","title":"Daily Mix 1",
                 "artwork_url":"https://i1.sndcdn.com/artworks-a-large.jpg","user":{"username":"SoundCloud"}},
                {"kind":"playlist","id":77,"title":"Chill","track_count":20,"permalink_url":"https://soundcloud.com/x/sets/chill",
                 "tracks":[{"artwork_url":"https://i1.sndcdn.com/artworks-t-large.jpg"}]}]}},
            {"title":"Empty shelf","items":{"collection":[]}},
            {"title":"","items":{"collection":[{"kind":"playlist","id":1,"title":"x"}]}}]}"#).unwrap();
        let s = shelves(&v);
        assert_eq!(s.len(), 1);
        assert_eq!(s[0].title, "Mixed for Rodion");
        let ids: Vec<&str> = s[0].items.iter().map(|p| p.id.as_str()).collect();
        assert_eq!(ids, ["soundcloud:system-playlists:daily-mix:1", "77"]);
        assert_eq!(s[0].items[0].art, "https://i1.sndcdn.com/artworks-a-t300x300.jpg");
        assert_eq!(s[0].items[1].art, "https://i1.sndcdn.com/artworks-t-t300x300.jpg");
    }

    use super::*;

    #[test]
    fn shelf_items_without_a_cover_get_their_first_tracks() {
        assert!(bare_cover(""));
        assert!(bare_cover("https://i1.sndcdn.com/avatars-69H9-t300x300.jpg"));
        assert!(!bare_cover("https://i1.sndcdn.com/artworks-a-t300x300.jpg"));
        let tracks: Value = serde_json::from_str(r#"[{"id":1,"artwork_url":null},{"id":2,"artwork_url":"https://i1.sndcdn.com/artworks-b-large.jpg"}]"#).unwrap();
        assert_eq!(first_cover(&tracks).as_deref(), Some("https://i1.sndcdn.com/artworks-b-t300x300.jpg"));
        let chart: Value = serde_json::from_str(r#"{"kind":"system-playlist","urn":"soundcloud:system-playlists:charts-top:all-music","title":"Funk",
            "calculated_artwork_url":"https://i1.sndcdn.com/artworks-c-large.jpg","user":{"avatar_url":"https://i1.sndcdn.com/avatars-x-large.jpg"}}"#).unwrap();
        assert_eq!(playlist_info(&chart).unwrap().art, "https://i1.sndcdn.com/artworks-c-t300x300.jpg");
    }

    #[test]
    fn finds_home_page_picks() {
        let v: Value = serde_json::from_str(r#"{"title":"More of what you like","urn":"soundcloud:selections:personalized-tracks","items":{"collection":[
            {"kind":"system-playlist","urn":"soundcloud:system-playlists:personalized-tracks:1:2","tracks":[{"id":11,"kind":"track"},{"id":12,"kind":"track","user":{"kind":"user","id":5}}]},
            {"kind":"system-playlist","urn":"soundcloud:system-playlists:weekly:99"}]}}"#).unwrap();
        let (mut ids, mut urns) = (Vec::new(), Vec::new());
        collect_picks(&v, &mut ids, &mut urns);
        assert_eq!(ids, vec![11, 12]);
        assert_eq!(urns, vec!["soundcloud:system-playlists:weekly:99"]);
    }

    #[test]
    fn finds_client_id_and_scripts() {
        let html = r#"<script crossorigin src="https://a-v2.sndcdn.com/assets/0-abc.js"></script>
            <script src="https://a-v2.sndcdn.com/assets/50-def.js"></script><img src="x.png">"#;
        assert_eq!(script_urls(html).len(), 2);
        let js = r#"foo({client_id:"AbCdEfGhIjKlMnOpQrStUvWxYz012345",env:"production"})"#;
        assert_eq!(find_client_id(js).as_deref(), Some("AbCdEfGhIjKlMnOpQrStUvWxYz012345"));
        assert_eq!(find_client_id("client_id:\"short\""), None);
    }

    #[test]
    fn picks_mp3_over_opus_and_skips_previews() {
        let t: Track = serde_json::from_value(serde_json::json!({
            "id": 1, "title": "x", "policy": "ALLOW",
            "media": {"transcodings": [
                {"url": "a", "snipped": false, "format": {"protocol": "hls", "mime_type": "audio/ogg; codecs=\"opus\""}},
                {"url": "b", "snipped": false, "format": {"protocol": "hls", "mime_type": "audio/mpeg"}},
                {"url": "c", "snipped": true, "format": {"protocol": "progressive", "mime_type": "audio/mpeg"}},
                {"url": "d", "snipped": false, "format": {"protocol": "hls", "mime_type": "audio/mp4; codecs=\"mp4a.40.2\""}}
            ]}
        })).unwrap();
        let (tr, kind) = t.transcoding().unwrap();
        assert_eq!((tr.url.as_str(), kind), ("b", Kind::Mp3));
        let mut snip = t.clone();
        snip.policy = Some("SNIP".into());
        assert!(snip.transcoding().is_none());
    }

    #[test]
    fn hls_mp4_can_start_in_the_middle() {
        // SoundCloud now often has only HLS AAC in MP4 (init.mp4 + .m4s): «Слушать здесь»
        // starts it at a segment. Only a progressive MP4 file has to start at 0.
        let only = |protocol: &str, mime: &str| -> Track {
            serde_json::from_value(serde_json::json!({"id": 1, "title": "x", "policy": "ALLOW",
                "media": {"transcodings": [{"url": "u", "snipped": false, "format": {"protocol": protocol, "mime_type": mime}}]}})).unwrap()
        };
        assert!(only("hls", "audio/mp4; codecs=\"mp4a.40.2\"").stream_seekable());
        assert!(only("hls", "audio/mpeg").stream_seekable());
        assert!(only("progressive", "audio/mpeg").stream_seekable());
        assert!(!only("progressive", "audio/mp4").stream_seekable());
    }

    #[test]
    fn starts_at_the_segment_with_the_asked_moment() {
        let text = "#EXTM3U\n#EXT-X-MAP:URI=\"init.mp4\"\n#EXTINF:10,\ns0\n#EXTINF:10.5,\ns1\n#EXTINF:9.5,\ns2\n#EXT-X-ENDLIST\n";
        // The init segment always first (MP4 needs it), then the segment that holds the moment.
        assert_eq!(segment_plan(text, 0), (vec![0, 1, 2, 3], 0));
        assert_eq!(segment_plan(text, 9_999), (vec![0, 1, 2, 3], 0));
        assert_eq!(segment_plan(text, 10_000), (vec![0, 2, 3], 10_000));
        assert_eq!(segment_plan(text, 25_000), (vec![0, 3], 20_500));
        // Past the end: the last segment, not nothing.
        assert_eq!(segment_plan(text, 99_000), (vec![0, 3], 20_500));
        // MP3/AAC segments stand alone: no init.
        let plain = "#EXTINF:5,\na\n#EXTINF:5,\nb\n";
        assert_eq!(segment_plan(plain, 6_000), (vec![1], 5_000));
        // Durations missing or not one per segment: from the start, as before.
        assert_eq!(segment_plan("#EXTINF:5,\na\nb\n", 6_000), (vec![0, 1], 0));
    }

    #[test]
    fn m3u8_with_map_and_relative_segments() {
        let text = "#EXTM3U\n#EXT-X-MAP:URI=\"init.mp4\"\n#EXTINF:10,\nseg1.m4s?x=1\n#EXTINF:10,\nhttps://cdn/seg2\n#EXT-X-ENDLIST\n";
        let urls = parse_m3u8("https://host/a/b/playlist.m3u8?sig=1", text).unwrap();
        assert_eq!(urls, ["https://host/a/b/init.mp4", "https://host/a/b/seg1.m4s?x=1", "https://cdn/seg2"]);
        assert!(parse_m3u8("https://h/p", "#EXT-X-KEY:METHOD=AES-128,URI=\"k\"\nseg").is_err());
    }
}
