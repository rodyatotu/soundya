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
    /// Best transcoding we can decode: progressive MP3, HLS MP3, then HLS/progressive AAC.
    pub fn transcoding(&self) -> Option<(&Transcoding, Kind)> {
        if matches!(self.policy.as_deref(), Some("BLOCK" | "SNIP")) {
            return None;
        }
        let mut best: Option<(u8, &Transcoding, Kind)> = None;
        for t in &self.media.as_ref()?.transcodings {
            if t.snipped {
                continue;
            }
            let mime = t.format.mime_type.to_ascii_lowercase();
            let (rank, kind) = match t.format.protocol.as_str() {
                "progressive" if mime.contains("mpeg") => (0, Kind::Mp3),
                "hls" if mime.contains("mpeg") => (1, Kind::Mp3),
                "progressive" if mime.contains("mp4") => (2, Kind::Mp4),
                "hls" if mime.contains("mp4") => (3, Kind::Mp4),
                "hls" if mime.contains("aac") => (4, Kind::Adts),
                _ => continue, // opus and encrypted streams are not supported
            };
            if best.as_ref().is_none_or(|b| rank < b.0) {
                best = Some((rank, t, kind));
            }
        }
        best.map(|(_, t, k)| (t, k))
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
}

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
                    .and_then(|p| std::fs::read_to_string(p).ok())
                    .map(|s| s.trim().to_owned())
            })
            .filter(|s| s.len() >= 20);
        Self {
            agent: Arc::new(Mutex::new(agent)),
            client_id: Arc::new(Mutex::new(cached)),
            token: Arc::new(Mutex::new(None)),
            cache,
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
        for attempt in 0..2 {
            let id = self.client_id(attempt == 1)?;
            let mut req = self
                .agent()
                .get(&url)
                .set("Accept", "application/json")
                .set("Origin", "https://soundcloud.com")
                .set("Referer", "https://soundcloud.com/")
                .query("client_id", &id);
            if let Some(t) = self.token.lock().unwrap().as_ref() {
                req = req.set("Authorization", &format!("OAuth {t}"));
            }
            for (k, v) in query {
                req = req.query(k, v);
            }
            match req.call() {
                Ok(resp) => return Ok(resp.into_json()?),
                Err(e) if attempt == 0 && matches!(status_of(&e), Some(401 | 403)) => continue,
                Err(e) if matches!(status_of(&e), Some(401)) && self.has_token() => {
                    return Err("SoundCloud не принял токен входа: обновите его на странице".into())
                }
                Err(e) => return Err(e.into()),
            }
        }
        unreachable!()
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
        let (t, kind) = track.transcoding().ok_or("no playable stream")?;
        let url = self.media_url(track, t)?;
        let refresh: Refresh = {
            let client = self.clone();
            let track = track.clone();
            let t = t.clone();
            Arc::new(move || client.media_url(&track, &t).ok())
        };
        if t.format.protocol == "progressive" {
            Ok((Box::new(Http::new(self.agent(), url, Some(refresh))), kind))
        } else {
            Ok((Box::new(Hls::new(self.agent(), &url, refresh)?), kind))
        }
    }
}

/// Builds an HTTP agent; `proxy` is "" (direct), "http://[user:pass@]host:port",
/// "socks5://[user:pass@]host:port" or a bare "host:port" (treated as HTTP).
pub fn build_agent(proxy: &str) -> Res<ureq::Agent> {
    let mut b = ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(10))
        .timeout_read(Duration::from_secs(30))
        .user_agent(UA);
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
                    if failures > 3 {
                        return Err(e);
                    }
                    thread::sleep(Duration::from_millis(700));
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
                    self.inner = None;
                    failures += 1;
                    if failures > 3 {
                        return Err(e);
                    }
                    thread::sleep(Duration::from_millis(700));
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
                    // Segment URLs are signed and expire: reload the playlist and retry once.
                    let pos = seg.pos;
                    if self.reload() {
                        let mut fresh = Http::new(self.agent.clone(), self.urls[self.next - 1].clone(), None);
                        fresh.pos = pos;
                        self.current = Some(fresh);
                        match self.current.as_mut().unwrap().read(buf) {
                            Ok(0) => self.current = None,
                            Ok(n) => return Ok(n),
                            Err(e) => return Err(e),
                        }
                    } else {
                        return Err(e);
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
