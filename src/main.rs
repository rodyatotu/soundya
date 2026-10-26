//! soundya: SoundCloud for Yandex Station Mini, controlled by Alice's voice.

mod alice;
mod alsa;
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

const CONTROL: &str = "127.0.0.1:9895";

pub enum Cmd {
    Voice(Intent),
    Control(String, Sender<String>),
    Ended(u64, Outcome),
    Settings(Settings, Sender<String>),
    TestProxy(String, Sender<String>),
    /// Search results from a background search, kept for "pick".
    Results(Vec<Track>),
    /// Logged-in username (None = not logged in or token rejected).
    User(Option<String>),
}

pub struct Shared {
    pub duck: Duck,
    keyword_only: AtomicBool,
    active: AtomicBool,
    queue: AtomicBool,
    status: Mutex<String>,
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
}

fn fmt_ms(ms: u64) -> String {
    format!("{}:{:02}", ms / 60_000, ms / 1000 % 60)
}

impl Controller {
    fn publish(&self) {
        let active = self.flags.is_some();
        self.shared.active.store(active, Ordering::Release);
        self.shared.queue.store(!self.queue.is_empty(), Ordering::Release);
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
        let current = self.flags.as_ref().map(|_| &self.queue[self.pos]);
        serde_json::json!({
            "state": state,
            "track": current.map(Track::name),
            "position_ms": position,
            "duration_ms": current.and_then(|t| t.duration).unwrap_or(0),
            "pos": self.pos,
            "queue": self.queue.iter().map(Track::name).collect::<Vec<_>>(),
            "settings": {
                "proxy": self.settings.masked_proxy(),
                "keyword_only": self.settings.keyword_only,
                "token": self.settings.masked_token(),
            },
            "user": self.user,
        })
        .to_string()
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
                    Ok((_, name)) => Some(name),
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
        let token_changed = new.token != self.settings.token;
        new.proxy = self.real_proxy(&new.proxy);
        if new.proxy != self.settings.proxy {
            if let Err(e) = self.client.set_proxy(&new.proxy) {
                return format!("error: неверный прокси: {e}");
            }
        }
        self.shared.keyword_only.store(new.keyword_only, Ordering::Release);
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
                // Verify right away so the page can say who is logged in.
                match self.client.me() {
                    Ok((_, name)) => {
                        msg.push_str(&format!(", вход: {name}"));
                        self.user = Some(name);
                    }
                    Err(e) => {
                        self.user = None;
                        return format!("error: сохранено, но вход не прошёл: {e}");
                    }
                }
            }
        }
        msg
    }

    fn halt(&mut self) {
        if let Some(f) = self.flags.take() {
            f.stop.store(true, Ordering::Release);
        }
        self.generation += 1;
    }

    fn start(&mut self, pos: usize) {
        self.halt();
        if pos >= self.queue.len() {
            self.publish();
            return;
        }
        self.pos = pos;
        let flags = player::new_flags();
        self.flags = Some(flags.clone());
        let generation = self.generation;
        let track = self.queue[pos].clone();
        let client = self.client.clone();
        let shared = self.shared.clone();
        let tx = self.tx.clone();
        let device = self.device.clone();
        eprintln!("play: {}", track.name());
        thread::spawn(move || {
            let outcome = match client.open(&track) {
                Ok((source, kind)) => player::play(source, kind, &device, &flags, &shared.duck),
                Err(e) => Outcome::Failed(e.to_string()),
            };
            let _ = tx.send(Cmd::Ended(generation, outcome));
        });
        self.publish();
    }

    fn load(&mut self, tracks: sc::Res<Vec<Track>>, what: &str) -> String {
        match tracks {
            Ok(t) if !t.is_empty() => {
                alice::pause_native();
                self.queue = t;
                self.failures = 0;
                self.start(0);
                format!("OK {}", self.status())
            }
            Ok(_) => format!("nothing found for {what:?}"),
            Err(e) => format!("error: {e}"),
        }
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
        let seen: HashSet<u64> = self.queue.iter().map(|t| t.id).collect();
        match self.client.related(last.id) {
            Ok(more) => {
                let more: Vec<Track> = more.into_iter().filter(|t| !seen.contains(&t.id)).collect();
                if more.is_empty() {
                    self.halt();
                } else {
                    self.queue.extend(more);
                    let p = self.pos + 1;
                    self.start(p);
                }
            }
            Err(e) => {
                eprintln!("related: {e}");
                self.halt();
            }
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
                }
                f.paused.store(paused, Ordering::Release);
            }
            None if !paused && !self.queue.is_empty() => {
                alice::pause_native();
                let p = self.pos;
                self.start(p);
            }
            None => {}
        }
    }

    /// Finds one of the account's playlists by (part of) its name.
    fn my_playlist(&self, name: &str) -> sc::Res<Vec<Track>> {
        let norm = |s: &str| s.to_lowercase().replace('ё', "е");
        let want = norm(name);
        let lists = self.client.library()?;
        let found = lists
            .iter()
            .find(|p| norm(&p.title) == want)
            .or_else(|| lists.iter().find(|p| norm(&p.title).contains(&want)))
            .or_else(|| {
                // Every spoken word appears in the title, in any order.
                lists.iter().find(|p| {
                    let t = norm(&p.title);
                    want.split_whitespace().all(|w| t.contains(w))
                })
            });
        match found {
            Some(p) => self.client.open_playlist(&p.id, &p.link),
            None => Err(format!("в ваших плейлистах нет «{name}»").into()),
        }
    }

    fn intent(&mut self, intent: Intent) -> String {
        match intent {
            Intent::Likes => {
                let r = self.client.likes();
                self.load(r, "лайки")
            }
            Intent::MyPlaylist(name) => {
                let r = self.my_playlist(&name);
                self.load(r, &name)
            }
            Intent::Play(q) => {
                let r = self.client.search_tracks(&q);
                self.load(r, &q)
            }
            Intent::Playlist(q) => {
                let r = self.client.search_playlist(&q);
                self.load(r, &q)
            }
            Intent::Pause => {
                self.set_paused(true);
                "OK".into()
            }
            Intent::Resume => {
                self.set_paused(false);
                "OK".into()
            }
            Intent::Next => {
                self.next();
                "OK".into()
            }
            Intent::Previous => {
                self.previous();
                "OK".into()
            }
        }
    }

    fn control(&mut self, line: &str) -> String {
        let (cmd, arg) = line.trim().split_once(' ').unwrap_or((line.trim(), ""));
        let arg = arg.trim();
        match cmd {
            "play" if arg.starts_with("https://") => {
                let r = self.client.resolve(arg);
                self.load(r, arg)
            }
            "play" if !arg.is_empty() => self.intent(Intent::Play(arg.to_owned())),
            "play" | "resume" => self.intent(Intent::Resume),
            "playlist" if !arg.is_empty() => self.intent(Intent::Playlist(arg.to_owned())),
            "pause" => self.intent(Intent::Pause),
            "toggle" => {
                let paused = self.flags.as_ref().is_some_and(|f| f.paused.load(Ordering::Acquire));
                self.intent(if paused || self.flags.is_none() { Intent::Resume } else { Intent::Pause })
            }
            "next" => self.intent(Intent::Next),
            "prev" | "previous" => self.intent(Intent::Previous),
            "stop" => {
                self.halt();
                self.publish();
                "OK".into()
            }
            "queue" => self
                .queue
                .iter()
                .enumerate()
                .map(|(i, t)| format!("{}{:>3}. {}", if i == self.pos { ">" } else { " " }, i + 1, t.name()))
                .collect::<Vec<_>>()
                .join("\n"),
            "status" => self.status(),
            "json" => self.json(),
            "likes" => self.intent(Intent::Likes),
            "pick" => match arg.parse::<usize>() {
                Ok(i) if i < self.results.len() => {
                    alice::pause_native();
                    self.queue = self.results.clone();
                    self.failures = 0;
                    self.start(i);
                    format!("OK {}", self.status())
                }
                _ => "error: результат не найден, повторите поиск".into(),
            },
            "openpl" if !arg.is_empty() => {
                // "openpl <id> [link]"
                let (id, link) = arg.split_once(' ').unwrap_or((arg, ""));
                let r = self.client.open_playlist(id, link.trim());
                self.load(r, id)
            }
            "goto" => match arg.parse::<usize>() {
                Ok(i) if i < self.queue.len() => {
                    alice::pause_native();
                    self.start(i);
                    "OK".into()
                }
                _ => "error: нет такого номера в очереди".into(),
            },
            _ => "unknown command; use play|playlist|pause|resume|toggle|next|prev|stop|status|queue".into(),
        }
    }

    fn start_up(self, rx: Receiver<Cmd>) {
        self.check_login();
        self.run(rx);
    }

    fn run(mut self, rx: Receiver<Cmd>) {
        loop {
            let Ok(cmd) = rx.recv_timeout(Duration::from_secs(1)) else {
                *self.shared.status.lock().unwrap() = self.status();
                continue;
            };
            match cmd {
                Cmd::Voice(intent) => {
                    let r = self.intent(intent);
                    eprintln!("voice result: {r}");
                }
                Cmd::Control(line, reply) => {
                    let (cmd, arg) = line.trim().split_once(' ').unwrap_or((line.trim(), ""));
                    let arg = arg.trim().to_owned();
                    match cmd {
                        // Slow read-only requests run aside so playback control stays responsive.
                        "search" if !arg.is_empty() => {
                            let client = self.client.clone();
                            let tx = self.tx.clone();
                            thread::spawn(move || {
                                let r = match client.search_all(&arg) {
                                    Ok((tracks, lists)) => {
                                        let json = serde_json::json!({
                                            "tracks": tracks.iter().map(|t| serde_json::json!({
                                                "name": t.name(),
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
                        _ => {
                            let r = self.control(&line);
                            let _ = reply.send(r);
                        }
                    }
                }
                Cmd::Ended(generation, outcome) if generation == self.generation => {
                    self.flags = None;
                    match outcome {
                        Outcome::Finished => {
                            self.failures = 0;
                            self.next();
                        }
                        Outcome::Stopped => self.publish(),
                        Outcome::Failed(e) => {
                            eprintln!("track failed: {e}");
                            self.failures += 1;
                            if self.failures >= 5 {
                                eprintln!("too many failures in a row; stopping");
                                self.halt();
                                self.publish();
                            } else {
                                self.next();
                            }
                        }
                    }
                }
                Cmd::Ended(..) => {}
                Cmd::Settings(s, reply) => {
                    let r = self.apply_settings(s);
                    let _ = reply.send(r);
                }
                Cmd::Results(tracks) => self.results = tracks,
                Cmd::User(user) => self.user = user,
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
            *self.shared.status.lock().unwrap() = self.status();
        }
    }
}

fn state_dir() -> PathBuf {
    PathBuf::from(std::env::var("SOUNDYA_DIR").unwrap_or_else(|_| "/data/opt/soundya".into()))
}

fn client_with(settings: &Settings) -> Client {
    let dir = state_dir();
    let cache = dir.is_dir().then(|| dir.join("client_id"));
    let client = Client::new(cache, &settings.proxy);
    client.set_token(&settings.token);
    client
}

fn client() -> Client {
    client_with(&Settings::load(&state_dir()))
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
    let _ = std::fs::create_dir_all(&dir);
    let settings = Settings::load(&dir);
    let shared = Arc::new(Shared {
        duck: Duck::new(),
        keyword_only: AtomicBool::new(settings.keyword_only),
        active: AtomicBool::new(false),
        queue: AtomicBool::new(false),
        status: Mutex::new("stopped".into()),
    });
    let (tx, rx) = mpsc::channel();
    if std::env::var("SOUNDYA_NO_VOICE").is_err() {
        alice::watch(tx.clone(), shared.clone());
    }
    let port = std::env::var("SOUNDYA_WEB_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(8765);
    if port != 0 {
        web::serve(port, tx.clone());
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
                    match player::play(source, kind, &device(), &flags, &Duck::new()) {
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
        | "status" | "queue" => send(&format!("{cmd} {rest}")),
        _ => {
            println!(
                "soundya {}\n\
                 \n  soundya daemon              run the player (autostarted)\
                 \n  soundya play <query|link>   search SoundCloud and play\
                 \n  soundya playlist <query>    play a playlist\
                 \n  soundya likes               play your SoundCloud likes (login on the web page)\
                 \n  soundya pause|resume|toggle|next|prev|stop\
                 \n  soundya status | queue\
                 \n  soundya search <query>      list results (no daemon needed)\
                 \n  soundya once <query>        play first result directly (test)\
                 \n\nVoice: «Алиса, включи <исполнитель/трек>», «включи плейлист <…>»,\
                 \n«включи мои лайки», «включи мой плейлист <…>»,\
                 \n«пауза», «продолжи», «следующий трек», «назад».",
                env!("CARGO_PKG_VERSION")
            );
            0
        }
    };
    std::process::exit(code);
}
