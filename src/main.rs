//! soundya: SoundCloud for Yandex Station Mini, controlled by Alice's voice.

mod airplay;
mod alice;
mod alsa;
mod buffer;
mod buttons;
mod dns;
mod fsx;
mod led;
mod mdns;
mod pc;
mod player;
mod sc;
mod settings;
mod web;

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

pub enum Cmd {
    Voice(Intent),
    /// AirPlay sound started (true) or stopped (false).
    AirPlay(bool),
    Control(String, Sender<String>),
    Ended(u64, Outcome),
    Settings(Settings, Sender<String>),
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
    active: AtomicBool,
    queue: AtomicBool,
    status: Mutex<String>,
    /// Latest JSON for the web page, so it never waits for a busy controller.
    pub status_json: Mutex<String>,
    pub airplay: airplay::AirPlay,
}

impl Shared {
    pub fn is_active(&self) -> bool {
        self.active.load(Ordering::Acquire)
    }
    pub fn has_queue(&self) -> bool {
        self.queue.load(Ordering::Acquire)
    }
    pub fn keyword_only(&self) -> bool {
        self.keyword_only.load(Ordering::Acquire)
    }
    pub fn pc_configured(&self) -> bool {
        self.pc.load(Ordering::Acquire)
    }
    pub fn contact_names(&self) -> Vec<String> {
        self.contacts.lock().unwrap().clone()
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
        serde_json::json!({
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
            "settings": {
                "proxy": self.settings.masked_proxy(),
                "keyword_only": self.settings.keyword_only,
                "ring": self.settings.ring,
                "token": self.settings.masked_token(),
                "pc_host": self.settings.pc_host,
                "pc_token": self.settings.masked_pc_token(),
                "pc_mac": self.settings.pc_mac,
                "pc_contacts": self.settings.pc_contacts,
                "airplay": self.settings.airplay,
                "airplay_name": self.settings.airplay_name,
            },
            "user": self.user,
            "loading": self.loading.lock().unwrap().clone(),
            "airplay": self.shared.airplay.json(),
        })
        .to_string()
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
        let level = match (&self.net, speed_kbps) {
            (None, _) => "unknown",
            (Some(Err(_)), _) => "down",
            (Some(Ok(p)), s) if *p <= 500 && s.is_none_or(|s| s >= 600) => "good",
            (Some(Ok(p)), s) if *p <= 1200 && s.is_none_or(|s| s >= 200) => "slow",
            _ => "bad",
        };
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

    fn apply_settings(&mut self, mut new: Settings) -> String {
        if new.token.contains('•') && new.token == self.settings.masked_token() {
            new.token = self.settings.token.clone();
        }
        new.token = new.token.trim().to_owned();
        if new.pc_token.contains('•') && new.pc_token == self.settings.masked_pc_token() {
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
        let token_changed = new.token != self.settings.token;
        new.proxy = self.real_proxy(&new.proxy);
        if new.proxy != self.settings.proxy {
            if let Err(e) = self.client.set_proxy(&new.proxy) {
                return format!("error: неверный прокси: {e}");
            }
            self.net = None;
            self.net_checked = None;
        }
        self.shared.keyword_only.store(new.keyword_only, Ordering::Release);
        self.shared.ring.store(new.ring, Ordering::Release);
        new.airplay_name = new.airplay_name.trim().to_owned();
        self.shared.airplay.configure(new.airplay, &new.airplay_name);
        let proxy_changed = new.proxy != self.settings.proxy;
        self.settings = new;
        let mut msg = match self.settings.save(&self.dir) {
            Ok(()) => "OK: сохранено".to_owned(),
            Err(e) => format!("применено, но не сохранено: {e}"),
        };
        if token_changed || proxy_changed {
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
        self.publish();
        match result {
            Ok(mut tracks) if !tracks.is_empty() => {
                if self.shuffle {
                    self.original = Some(tracks.clone());
                    self.shuffle_slice(&mut tracks);
                } else {
                    self.original = None;
                }
                alice::pause_native();
                self.queue = tracks;
                self.failures = 0;
                self.start(0);
                respond(reply, &format!("OK {}", self.status()), None);
            }
            Ok(_) => respond(
                reply,
                &format!("nothing found for {what:?}"),
                Some(format!("Не нашла «{what}» на SoundCloud")),
            ),
            Err(e) => {
                let e = e.to_string();
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

    fn halt(&mut self) {
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
        let device = self.device.clone();
        let prefetch = self.client.prefetch.clone();
        eprintln!("play: {}", track.name());
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
            Intent::Likes => self.fetch("лайки".into(), reply, |c| c.likes()),
            Intent::MyPlaylist(name) => {
                let n = name.clone();
                self.fetch(name, reply, move |c| my_playlist(c, &n));
            }
            Intent::Play(q) => {
                let query = q.clone();
                self.fetch(q, reply, move |c| c.search_tracks(&query));
            }
            Intent::Playlist(q) => {
                // «Включи плейлист X»: the account's own playlists first, then SoundCloud search.
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
                self.fetch("рекомендации".into(), reply, move |c| c.recommendations(seed));
            }
            Intent::Default => {
                if !self.queue.is_empty() {
                    alice::pause_native();
                    self.set_paused(false);
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
            Intent::Pause => {
                self.set_paused(true);
                respond(reply, "OK", None);
            }
            Intent::Resume if self.queue.is_empty() => {
                // Nothing to go back to (soundya was restarted): same as «включи саундклауд».
                eprintln!("voice: nothing to resume; starting likes instead");
                self.intent(Intent::Default, reply);
            }
            Intent::Resume => {
                self.set_paused(false);
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
            Intent::WhatPlaying => match self.current() {
                Some(track) => {
                    let text = format!("Сейчас играет {}", spoken(track));
                    respond(reply, &text, Some(text.clone()));
                }
                None => respond(reply, "сейчас ничего не играет", Some("Сейчас ничего не играет".into())),
            },
        }
    }

    fn control(&mut self, line: &str, reply: Sender<String>) {
        let (cmd, arg) = line.trim().split_once(' ').unwrap_or((line.trim(), ""));
        let arg = arg.trim().to_owned();
        let on_off = |a: &str| !matches!(a, "off" | "0" | "false" | "выкл");
        match cmd {
            "play" if arg.starts_with("https://") => {
                let link = arg.clone();
                self.fetch(arg, Some(reply), move |c| c.resolve(&link));
            }
            "play" if !arg.is_empty() => self.intent(Intent::Play(arg), Some(reply)),
            "play" | "resume" => self.intent(Intent::Resume, Some(reply)),
            "playlist" if !arg.is_empty() => self.intent(Intent::Playlist(arg), Some(reply)),
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
                    "open" if !name.is_empty() => Some(alice::PcAction::Open(name)),
                    "close" if !name.is_empty() => Some(alice::PcAction::Close(name)),
                    // pc do открой ютуб в профиле работа — as if said to Alice
                    "do" | "say" if !name.is_empty() => Some(alice::PcAction::Command(name)),
                    _ => None,
                };
                match action {
                    Some(a) => self.intent(Intent::Pc(a), Some(reply)),
                    None => send_to(&reply, "usage: pc ping|wake|sleep|shutdown|restart|lock|open <программа>|close <программа>|do <фраза>".into()),
                }
            }
            "stop" => {
                self.halt();
                self.publish();
                send_to(&reply, "OK".into());
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
            "netcheck" => self.check_net(Some(reply)),
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

    fn start_up(self, rx: Receiver<Cmd>) {
        self.check_login();
        self.run(rx);
    }

    fn run(mut self, rx: Receiver<Cmd>) {
        loop {
            if self.net_checked.is_none_or(|t| t.elapsed() > Duration::from_secs(60)) && !self.net_checking {
                self.net_checked = Some(std::time::Instant::now());
                self.check_net(None);
            }
            let Ok(cmd) = rx.recv_timeout(Duration::from_secs(1)) else {
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
                        "library" => {
                            let client = self.client.clone();
                            thread::spawn(move || {
                                let r = match client.library() {
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
                Cmd::Settings(s, reply) => {
                    let r = self.apply_settings(s);
                    let _ = reply.send(r);
                }
                Cmd::Results(tracks) => self.results = tracks,
                Cmd::Net(result) => {
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
    }
}

fn state_dir() -> PathBuf {
    PathBuf::from(std::env::var("SOUNDYA_DIR").unwrap_or_else(|_| "/data/opt/soundya".into()))
}

fn client_with(settings: &Settings) -> Client {
    let dir = state_dir();
    let cache = fsx::is_dir(&dir).then(|| dir.join("client_id"));
    let client = Client::new(cache, &settings.proxy);
    client.set_token(&settings.token);
    client
}

fn client() -> Client {
    client_with(&Settings::load(&state_dir()))
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
    let settings = Settings::load(&dir);
    let shared = Arc::new(Shared {
        duck: Duck::new(),
        ring: AtomicBool::new(settings.ring),
        playing: AtomicBool::new(false),
        fetching: AtomicBool::new(false),
        track: Mutex::new(None),
        keyword_only: AtomicBool::new(settings.keyword_only),
        pc: AtomicBool::new(settings.pc_configured()),
        contacts: Mutex::new(contact_names(&settings)),
        active: AtomicBool::new(false),
        queue: AtomicBool::new(false),
        status: Mutex::new("stopped".into()),
        status_json: Mutex::new(String::new()),
        airplay: airplay::AirPlay::new(settings.airplay, &settings.airplay_name),
    });
    let (tx, rx) = mpsc::channel();
    led::run(shared.clone());
    // Starts the sound card if it hangs, so neither soundya nor Alice goes silent.
    alsa::watch(device());
    // http://soundya.local:8765 — works whatever IP the router hands out.
    mdns::run(&std::env::var("SOUNDYA_NAME").unwrap_or_else(|_| "soundya".into()));
    // The speaker in the AirPlay list of iPhones and Macs.
    airplay::run(dir.clone(), device(), tx.clone(), shared.clone());
    if std::env::var("SOUNDYA_NO_VOICE").is_err() {
        alice::watch(tx.clone(), shared.clone());
        buttons::run(tx.clone(), shared.clone());
    }
    let port = std::env::var("SOUNDYA_WEB_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(8765);
    if port != 0 {
        web::serve(port, tx.clone(), shared.clone());
    }
    let control_tx = tx.clone();
    let status_shared = shared.clone();
    thread::spawn(move || {
        for conn in listener.incoming().flatten() {
            let tx = control_tx.clone();
            let shared = status_shared.clone();
            thread::spawn(move || serve(conn, tx, &shared));
        }
    });
    eprintln!(
        "soundya {} running; device {:?}, keyword only: {}, proxy: {}",
        env!("CARGO_PKG_VERSION"),
        device(),
        settings.keyword_only,
        if settings.proxy.is_empty() { "none".into() } else { settings.masked_proxy() }
    );
    Controller {
        client: client_with(&settings),
        shared,
        tx,
        device: device(),
        queue: Vec::new(),
        pos: 0,
        generation: 0,
        flags: None,
        failures: 0,
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
        rng: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0x9E37_79B9)
            | 1,
    }
    .start_up(rx);
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
        | "status" | "queue" | "shuffle" | "repeat" | "like" | "unlike" | "similar" | "recs" | "seek" | "pc" => send(&format!("{cmd} {rest}")),
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
                env!("CARGO_PKG_VERSION")
            );
            0
        }
    };
    std::process::exit(code);
}
