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
const UA: &str = "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/130.0 Safari/537.36";

#[derive(Debug, Clone, Deserialize)]
pub struct User {
    #[serde(default)]
    pub username: String,
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
}

impl Track {
    pub fn name(&self) -> String {
        match &self.user {
            Some(u) if !u.username.is_empty() => format!("{} — {}", u.username, self.title),
            _ => self.title.clone(),
        }
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
    token: Arc<Mutex<Option<String>>>,
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
    })
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
            token: Arc::new(Mutex::new(None)),
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
    }

    pub fn has_token(&self) -> bool {
        self.token.lock().unwrap().is_some()
    }

    /// Switches all later requests (API and audio) to a new proxy; "" means direct.
    pub fn set_proxy(&self, proxy: &str) -> Res<()> {
        let agent = build_agent(proxy)?;
        *self.agent.lock().unwrap() = agent;
        Ok(())
    }

    fn client_id(&self, refresh: bool) -> Res<String> {
        let mut guard = self.client_id.lock().unwrap();
        if !refresh {
            if let Some(id) = guard.as_ref() {
                return Ok(id.clone());
            }
        }
        let id = self.scrape_client_id()?;
        eprintln!("soundcloud: new client_id");
        if let Some(p) = &self.cache {
            let _ = std::fs::write(p, &id);
        }
        *guard = Some(id.clone());
        Ok(id)
    }

    fn scrape_client_id(&self) -> Res<String> {
        let html = self
            .agent()
            .get("https://soundcloud.com/")
            .call()?
            .into_string()?;
        let scripts = script_urls(&html);
        for url in scripts.iter().rev() {
            let Ok(resp) = self.agent().get(url).call() else {
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
            format!("{API}{path_or_url}")
        };
        let mut refreshed = false;
        let mut retried = false;
        // The token was refused: try once without it (search and public pages work anonymously).
        let mut anonymous = false;
        loop {
            let id = self.client_id(refreshed)?;
            let mut req = self
                .agent()
                .get(&url)
                .set("Accept", "application/json")
                .set("Origin", "https://soundcloud.com")
                .set("Referer", "https://soundcloud.com/")
                .query("client_id", &id);
            if !anonymous {
                if let Some(t) = self.token.lock().unwrap().as_ref() {
                    req = req.set("Authorization", &format!("OAuth {t}"));
                }
            }
            for (k, v) in query {
                req = req.query(k, v);
            }
            let short = url.split('?').next().unwrap_or(&url).to_owned();
            match req.call() {
                Ok(resp) => return Ok(resp.into_json()?),
                Err(e) if !refreshed && matches!(status_of(&e), Some(401 | 403)) => {
                    refreshed = true;
                    continue;
                }
                Err(e) if matches!(status_of(&e), Some(401)) && self.has_token() && !anonymous => {
                    eprintln!("soundcloud: token refused on {short}; trying without login");
                    anonymous = true;
                    continue;
                }
                Err(e) if matches!(status_of(&e), Some(401)) && self.has_token() => {
                    return Err("SoundCloud не принял токен входа: обновите его на странице".into())
                }
                // Keep tokens out of messages and logs.
                Err(ureq::Error::Status(code, _)) => return Err(format!("HTTP {code} на {short}").into()),
                Err(ureq::Error::Transport(t)) if !retried => {
                    // Proxies drop connections now and then: one quiet retry.
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
        Ok(c.collection.into_iter().filter(|t| t.transcoding().is_some()).collect())
    }

    /// Tracks and playlists for the web page.
    pub fn search_all(&self, q: &str) -> Res<(Vec<Track>, Vec<PlaylistInfo>)> {
        let tracks = self.search_tracks(q)?;
        let v = self.api("/search/playlists", &[("q", q), ("limit", "10")])?;
        let c: Collection<Value> = serde_json::from_value(v)?;
        let lists = c.collection.iter().filter_map(playlist_info).collect();
        Ok((tracks, lists))
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

    pub fn related(&self, id: u64) -> Res<Vec<Track>> {
        let v = self.api(&format!("/tracks/{id}/related"), &[("limit", "20")])?;
        let c: Collection<Track> = serde_json::from_value(v)?;
        Ok(c.collection.into_iter().filter(|t| t.transcoding().is_some()).collect())
    }

    /// A soundcloud.com link to a track, playlist or user.
    pub fn resolve(&self, link: &str) -> Res<Vec<Track>> {
        let v = self.api("/resolve", &[("url", link)])?;
        match v.get("kind").and_then(Value::as_str) {
            Some("track") => Ok(serde_json::from_value::<Track>(v)
                .map(|t| vec![t])?
                .into_iter()
                .filter(|t| t.transcoding().is_some())
                .collect()),
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
            match self.open_one(track, t, kind) {
                Ok(reader) => {
                    eprintln!("stream: {name}");
                    return Ok((Box::new(crate::buffer::Buffered::new(reader)), kind));
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

    fn open_one(&self, track: &Track, t: &Transcoding, _kind: Kind) -> Res<Box<dyn Read + Send + Sync>> {
        let url = self.media_url(track, t)?;
        let refresh: Refresh = {
            let client = self.clone();
            let track = track.clone();
            let t = t.clone();
            Arc::new(move || client.media_url(&track, &t).ok())
        };
        if t.format.protocol == "progressive" {
            Ok(Box::new(Http::new(self.agent(), url, Some(refresh))))
        } else {
            Ok(Box::new(Hls::new(self.agent(), &url, refresh)?))
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
        .resolver(crate::dns::Resolver::new());
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

fn script_urls(html: &str) -> Vec<String> {
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

impl Hls {
    fn new(agent: ureq::Agent, url: &str, refresh: Refresh) -> Res<Self> {
        let text = agent.get(url).call()?.into_string()?;
        let urls = parse_m3u8(url, &text)?;
        Ok(Self { agent, urls, next: 0, current: None, refresh })
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
                if self.next >= self.urls.len() {
                    return Ok(0);
                }
                self.current = Some(Http::new(self.agent.clone(), self.urls[self.next].clone(), None));
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
                        let mut fresh = Http::new(self.agent.clone(), self.urls[self.next - 1].clone(), None);
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
    use super::*;

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
    fn m3u8_with_map_and_relative_segments() {
        let text = "#EXTM3U\n#EXT-X-MAP:URI=\"init.mp4\"\n#EXTINF:10,\nseg1.m4s?x=1\n#EXTINF:10,\nhttps://cdn/seg2\n#EXT-X-ENDLIST\n";
        let urls = parse_m3u8("https://host/a/b/playlist.m3u8?sig=1", text).unwrap();
        assert_eq!(urls, ["https://host/a/b/init.mp4", "https://host/a/b/seg1.m4s?x=1", "https://cdn/seg2"]);
        assert!(parse_m3u8("https://h/p", "#EXT-X-KEY:METHOD=AES-128,URI=\"k\"\nseg").is_err());
    }
}
