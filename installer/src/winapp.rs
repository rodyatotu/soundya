//! soundya on Windows, as soundya.app on a Mac: one soundya.exe that is
//!   `soundya.exe`          — the installer's wizard in its own window (first run), or, once
//!                            this PC is connected, the tray icon with the panel's window;
//!   `soundya.exe tray`     — the tray icon (started at logon): what plays, play/pause,
//!                            next, the panel, the to-do list over other windows, …;
//!   `soundya.exe agent`    — the PC agent (started at logon): the speaker's commands.
//!
//! Windows show the speaker's own page in WebView2 (winwv2.rs); without WebView2, in an
//! Edge window («--app»). The tray process sleeps in GetMessage; the agent in accept().

use std::{
    cell::RefCell,
    collections::HashMap,
    ffi::c_void,
    path::PathBuf,
    ptr::null_mut,
    rc::Rc,
    sync::Arc,
    time::Duration,
};

use serde_json::{json, Value};

use crate::winagent::{self, Agent, Config};
use crate::winffi::*;
use crate::winsys::{self, WinSys};
use crate::winwv2::{self, Event, View};

const WM_APP_SHOW: u32 = 0x8000 + 1;
const WM_APP_TRAY: u32 = 0x8000 + 2;
const WM_APP_RUN: u32 = 0x8000 + 3;
/// The speaker was found (or changed): the windows go to it.
pub const WM_APP_SPEAKER: u32 = 0x8000 + 4;

/// The speaker found in the background, for the UI thread (WM_APP_SPEAKER).
pub static NEW_SPEAKER: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);
const TRAY_CLASS: &str = "SoundyaTray";
const WIN_CLASS: &str = "SoundyaWindow";
const AGENT_MUTEX: &str = "Local\\soundya-pc-agent";
const TRAY_MUTEX: &str = "Local\\soundya-tray";
/// The page's colours behind it: the panel's background, the to-do card's.
const PANEL_BG: u32 = 0x0e0c16;
const TODO_BG: u32 = 0x14121c;

/// %LOCALAPPDATA%\soundya-pc: the agent's config.json, status.json, log.txt (as soundya-pc.ps1).
pub fn pc_dir() -> PathBuf {
    std::env::var("LOCALAPPDATA").map(PathBuf::from).unwrap_or_else(|_| std::env::temp_dir()).join("soundya-pc")
}

/// This PC is connected to a speaker: opening soundya.exe shows soundya, not the installer.
pub fn set_up() -> bool {
    pc_dir().join("config.json").exists()
}

/// The speaker this PC was connected to; soundya.local if none.
pub fn speaker() -> String {
    Config::load(&pc_dir())
        .map(|c| c.speaker)
        .filter(|s| !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == ':'))
        .unwrap_or_else(|| "soundya.local".into())
}

fn mutex_exists(name: &str) -> bool {
    unsafe {
        let h = OpenMutexW(0x0010_0000, 0, wide(name).as_ptr()); // SYNCHRONIZE
        if h.is_null() {
            return false;
        }
        CloseHandle(h);
        true
    }
}

/// Holds a named mutex for the life of the process; false if another process has it.
fn own_mutex(name: &str) -> bool {
    unsafe {
        let h = CreateMutexW(null_mut(), 1, wide(name).as_ptr());
        !h.is_null() && GetLastError() != 183 // ERROR_ALREADY_EXISTS
    }
}

fn exe() -> String {
    std::env::current_exe().map(|p| p.to_string_lossy().into_owned()).unwrap_or_else(|_| "soundya.exe".into())
}

/// This is the installed soundya.exe (%LOCALAPPDATA%\soundya), not a download.
pub fn installed_copy() -> bool {
    let target = winwv2::app_dir().join("soundya.exe");
    std::env::current_exe().is_ok_and(|me| me.to_string_lossy().eq_ignore_ascii_case(&target.to_string_lossy()))
}

pub fn tray_running() -> bool {
    mutex_exists(TRAY_MUTEX)
}

pub fn start_agent() {
    if !mutex_exists(AGENT_MUTEX) {
        let _ = winsys::spawn(&exe(), &["agent".into()]);
    }
}

// ------------------------------------------------------------------ the agent

/// `soundya.exe agent`: the speaker's commands until the PC shuts down.
pub fn agent() -> ! {
    if !own_mutex(AGENT_MUTEX) {
        std::process::exit(0);
    }
    winsys::dpi_aware();
    let dir = pc_dir();
    let _ = std::fs::create_dir_all(&dir);
    let sys = WinSys::new(std::env::var("SOUNDYA_PC_DRYRUN").is_ok_and(|v| v == "1"));
    let log = |t: &str| winagent::log_to(&dir, winagent::Sys::local_time(&sys), t);
    let Some(config) = Config::load(&dir).filter(|c| c.key.len() >= 16) else {
        log("no key in config.json; connect this PC again from soundya");
        std::process::exit(1);
    };
    // soundya-pc.ps1 from before: its autostart goes, its copies stop (they hold the port).
    crate::winsetup::stop_legacy();
    let mut listener = None;
    for attempt in 0..10 {
        match std::net::TcpListener::bind(("0.0.0.0", config.port)) {
            Ok(l) => {
                listener = Some(l);
                break;
            }
            Err(e) => {
                if attempt == 0 {
                    log(&format!("cannot listen on port {}: {e}; waiting", config.port));
                }
                crate::winsetup::free_port(config.port);
                std::thread::sleep(Duration::from_secs(1));
            }
        }
    }
    let Some(listener) = listener else {
        log(&format!("port {} stays busy: another program uses it", config.port));
        std::process::exit(1);
    };
    log(&format!("listening on port {} (soundya {})", config.port, crate::version()));
    let duck = config.duck;
    let agent = Arc::new(Agent::new(config, Box::new(sys), dir.clone()));
    if duck {
        let a = agent.clone();
        std::thread::spawn(move || a.duck_loop());
    }
    // «Найдены компьютеры» on the speaker's panel: this PC says hello at start and every 5 minutes.
    let a = agent.clone();
    std::thread::spawn(move || a.hello_loop());
    unsafe { SetProcessWorkingSetSize(GetCurrentProcess(), usize::MAX, usize::MAX) };
    agent.serve(listener);
    std::process::exit(0)
}

// ------------------------------------------------------------------ windows

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Kind {
    /// The small player by the tray icon (the page's «#glass», as under the Mac's menu bar icon).
    Panel,
    /// «Открыть в большом окне»: the whole panel in an ordinary window.
    Big,
    Todo,
    /// The installer's wizard: closing it ends the program.
    Wizard,
}

struct Win {
    kind: Kind,
    url: String,
    view: Option<Rc<View>>,
    /// The page failed: shown once the view exists.
    offline: bool,
}

#[derive(Default)]
struct App {
    tray: HWND,
    wins: HashMap<isize, Win>,
    panel: Option<HWND>,
    big: Option<HWND>,
    todo: Option<HWND>,
    speaker: String,
    status: Option<(Option<u16>, String)>,
    /// When the panel last lost the focus.
    panel_left: Option<std::time::Instant>,
}

thread_local! {
    static APP: RefCell<App> = RefCell::new(App::default());
}

fn app<R>(f: impl FnOnce(&mut App) -> R) -> R {
    APP.with(|a| f(&mut a.borrow_mut()))
}

fn view_of(h: HWND) -> Option<Rc<View>> {
    app(|a| a.wins.get(&(h as isize)).and_then(|w| w.view.clone()))
}

/// Where the windows were: %LOCALAPPDATA%\soundya\windows.json.
fn layout_file() -> PathBuf {
    winwv2::app_dir().join("windows.json")
}

fn layout() -> Value {
    std::fs::read_to_string(layout_file()).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_else(|| json!({}))
}

fn save_layout(key: &str, v: Value) {
    let mut all = layout();
    if !all.is_object() {
        all = json!({});
    }
    all[key] = v;
    let _ = std::fs::write(layout_file(), all.to_string());
}

fn rect_of(h: HWND) -> RECT {
    let mut p: WINDOWPLACEMENT = unsafe { std::mem::zeroed() };
    p.length = std::mem::size_of::<WINDOWPLACEMENT>() as u32;
    unsafe { GetWindowPlacement(h, &mut p) };
    p.rcNormalPosition
}

fn remember(h: HWND, kind: Kind) {
    let r = rect_of(h);
    if r.right - r.left < 50 || r.bottom - r.top < 50 {
        return;
    }
    let key = match kind {
        Kind::Panel => "panel",
        Kind::Big => "big",
        Kind::Todo => "todo",
        Kind::Wizard => "wizard",
    };
    save_layout(key, json!([r.left, r.top, r.right - r.left, r.bottom - r.top]));
}

fn work_area() -> RECT {
    let mut r = RECT::default();
    unsafe { SystemParametersInfoW(0x30, 0, &mut r as *mut RECT as *mut c_void, 0) }; // SPI_GETWORKAREA
    r
}

fn scale() -> f64 {
    let t = app(|a| a.tray);
    // The tray's window, else the whole screen's DPI (the wizard has no tray).
    let dpi = unsafe { if t.is_null() { GetDpiForSystem() } else { GetDpiForWindow(t) } };
    f64::from(if dpi == 0 { 96 } else { dpi }) / 96.0
}

/// The saved place of a window, if it is still on a screen; else `default`.
fn placed(key: &str, default: RECT) -> RECT {
    let v = &layout()[key];
    let n = |i: usize| v[i].as_i64().map(|x| x as i32);
    if let (Some(x), Some(y), Some(w), Some(h)) = (n(0), n(1), n(2), n(3)) {
        let screen = unsafe { (GetSystemMetrics(76), GetSystemMetrics(77), GetSystemMetrics(78), GetSystemMetrics(79)) };
        let inside = x + w > screen.0 + 40 && y > screen.1 - 20 && x < screen.0 + screen.2 - 40 && y < screen.1 + screen.3 - 40;
        if w > 100 && h > 100 && inside {
            return RECT { left: x, top: y, right: x + w, bottom: y + h };
        }
    }
    default
}

/// The panel's sizes from the menu (logical pixels), as on the Mac: small, medium, large.
const SIZES: [(f64, f64); 3] = [(380.0, 560.0), (430.0, 780.0), (560.0, 900.0)];

fn panel_default(i: usize) -> RECT {
    let (s, wa) = (scale(), work_area());
    let (w, h) = ((SIZES[i].0 * s) as i32, ((SIZES[i].1 * s) as i32).min(wa.bottom - wa.top - 16));
    // Bottom right, by the tray.
    RECT { left: wa.right - w - 12, top: wa.bottom - h - 12, right: wa.right - 12, bottom: wa.bottom - 12 }
}

fn todo_default() -> RECT {
    let (s, wa) = (scale(), work_area());
    let (w, h) = ((300.0 * s) as i32, (380.0 * s) as i32);
    // Top right corner, a little in from the edges.
    RECT { left: wa.right - w - 16, top: wa.top + 16, right: wa.right - 16, bottom: wa.top + 16 + h }
}

/// Makes the page's view of a window (again, after a crash).
fn make_view(h: HWND) {
    let Some((kind, url)) = app(|a| a.wins.get(&(h as isize)).map(|w| (w.kind, w.url.clone()))) else { return };
    let script = (kind == Kind::Todo).then(|| DRAG_SCRIPT.to_owned());
    let bg = match kind {
        Kind::Todo => TODO_BG,
        _ => PANEL_BG,
    };
    View::create(h, url.clone(), script, Some(bg), on_event, move |r| match r {
        Ok(v) => {
            let v = Rc::new(v);
            let offline = app(|a| {
                a.wins.get_mut(&(h as isize)).map(|w| {
                    w.view = Some(v.clone());
                    std::mem::take(&mut w.offline)
                })
            });
            if offline == Some(true) {
                v.navigate_to_string(&winwv2::offline_page(&url, OFFLINE));
            }
            if unsafe { IsWindowVisible(h) } == 0 {
                v.set_visible(false);
            }
        }
        Err(e) => {
            // No WebView2: Edge in its app mode instead (the wizard: the browser, main.rs), and
            // this window goes.
            winagent::log_to(&winwv2::app_dir(), local_time(), &format!("window: {e}"));
            let edge = kind != Kind::Wizard && edge_app(&url, kind);
            app(|a| {
                a.wins.remove(&(h as isize));
                if a.panel == Some(h) {
                    a.panel = None;
                }
                if a.big == Some(h) {
                    a.big = None;
                }
                if a.todo == Some(h) {
                    a.todo = None;
                }
            });
            unsafe { DestroyWindow(h) };
            if kind == Kind::Wizard {
                unsafe { PostQuitMessage(1) };
                return;
            }
            if !edge {
                message_box(&format!("Окно soundya не открылось: {e}.\nОткройте в браузере: {url}"), "soundya");
                let _ = winsys::shell_open(&url);
            }
        }
    });
}

const OFFLINE: &str = "Колонка не отвечает. Включена ли она и в той же ли сети этот компьютер?";

fn on_event(h: HWND, e: Event) {
    match e {
        Event::Message(m) if m == "drag" => unsafe {
            // A drag on the page's title (its «app-region: drag»): Windows moves the window.
            let mut p = POINT::default();
            GetCursorPos(&mut p);
            ReleaseCapture();
            let at = ((p.y as u16 as u32) << 16 | p.x as u16 as u32) as LPARAM;
            PostMessageW(h, 0x00A1, 2, at); // WM_NCLBUTTONDOWN, HTCAPTION
        },
        Event::Message(m) if m == "close" => hide(h),
        Event::Message(_) => {}
        Event::LoadFailed => {
            let url = app(|a| a.wins.get(&(h as isize)).map(|w| w.url.clone())).unwrap_or_default();
            if let Some(v) = view_of(h) {
                v.navigate_to_string(&winwv2::offline_page(&url, OFFLINE));
            }
        }
        Event::Crashed => {
            // A new view a moment later (not inside the engine's own call).
            if let Some(v) = app(|a| a.wins.get_mut(&(h as isize)).and_then(|w| w.view.take())) {
                v.close();
            }
            unsafe { PostMessageW(h, WM_APP_RUN, 1, 0) };
        }
    }
}

/// The to-do page's title drags the window (its CSS says «app-region: drag»).
const DRAG_SCRIPT: &str = r#"
document.addEventListener('mousedown', function (e) {
  if (e.button !== 0 || !window.chrome || !chrome.webview) return;
  var t = e.target;
  if (t.closest && t.closest('button,a,input,textarea,select,label,[contenteditable],.dot')) return;
  for (var el = t; el && el.nodeType === 1; el = el.parentElement) {
    var cs = getComputedStyle(el), r = cs.getPropertyValue('app-region') || cs.getPropertyValue('-webkit-app-region');
    if (r === 'no-drag') return;
    if (r === 'drag' || el.tagName === 'HEADER') { e.preventDefault(); chrome.webview.postMessage('drag'); return; }
  }
}, true);
"#;

unsafe fn new_window(kind: Kind, url: &str, r: RECT, title: &str) -> HWND {
    let inst = GetModuleHandleW(std::ptr::null());
    let (style, ex) = match kind {
        // WS_POPUP | WS_THICKFRAME | WS_CLIPCHILDREN; WS_EX_TOPMOST | WS_EX_TOOLWINDOW (no taskbar button).
        Kind::Todo | Kind::Panel => (0x8000_0000 | 0x0004_0000 | 0x0200_0000, 0x8 | 0x80),
        // WS_OVERLAPPEDWINDOW | WS_CLIPCHILDREN
        _ => (0x00CF_0000 | 0x0200_0000, 0),
    };
    let h = CreateWindowExW(ex, wide(WIN_CLASS).as_ptr(), wide(title).as_ptr(), style, r.left, r.top, r.right - r.left, r.bottom - r.top, null_mut(), null_mut(), inst, null_mut());
    if h.is_null() {
        return h;
    }
    // Dark title bar; Windows 11: rounded corners on the to-do window too.
    let on: i32 = 1;
    DwmSetWindowAttribute(h, 20, &on as *const i32 as *const c_void, 4);
    let round: i32 = 2;
    DwmSetWindowAttribute(h, 33, &round as *const i32 as *const c_void, 4);
    app(|a| a.wins.insert(h as isize, Win { kind, url: url.to_owned(), view: None, offline: false }));
    h
}

fn show(h: HWND, activate: bool) {
    unsafe {
        ShowWindow(h, if activate { 5 } else { 4 }); // SW_SHOW / SW_SHOWNOACTIVATE
        if activate {
            SetForegroundWindow(h);
        }
    }
    if let Some(v) = view_of(h) {
        v.set_visible(true);
        v.fit(h);
        if activate {
            v.focus();
        }
        // Asleep while hidden: the status at once, not at the page's next turn (as on the Mac).
        v.execute("try { refresh(); } catch (e) {}");
    }
}

fn hide(h: HWND) {
    let kind = app(|a| a.wins.get(&(h as isize)).map(|w| w.kind));
    if let Some(k) = kind {
        remember(h, k);
    }
    unsafe { ShowWindow(h, 0) };
    if let Some(v) = view_of(h) {
        v.set_visible(false);
    }
    if kind == Some(Kind::Todo) {
        save_layout("todo_on", json!(false));
    }
}

fn visible(h: Option<HWND>) -> bool {
    h.is_some_and(|h| unsafe { IsWindowVisible(h) } != 0)
}

fn page_url(kind: Kind) -> String {
    let host = app(|a| a.speaker.clone());
    match kind {
        Kind::Panel => format!("http://{host}:8765/#glass"),
        Kind::Todo => format!("http://{host}:8765/todo"),
        _ => format!("http://{host}:8765/"),
    }
}

/// The small player's place: right above the tray (bottom right), its size as last left.
fn by_tray() -> RECT {
    let saved = placed("panel", panel_default(1));
    let (w, h) = (saved.right - saved.left, saved.bottom - saved.top);
    let wa = work_area();
    let h = h.min(wa.bottom - wa.top - 16);
    RECT { left: wa.right - w - 12, top: wa.bottom - h - 12, right: wa.right - 12, bottom: wa.bottom - 12 }
}

/// The small player by the tray icon, as soundya.app's window under the menu bar icon: a click
/// on the icon shows or hides it, a click elsewhere hides it (the music goes on).
fn open_panel(toggle: bool) {
    let existing = app(|a| a.panel);
    if let Some(h) = existing {
        if visible(Some(h)) {
            if toggle {
                hide(h);
            } else {
                show(h, true);
            }
        } else if toggle && app(|a| a.panel_left.is_some_and(|t| t.elapsed() < Duration::from_millis(500))) {
            // This very click took the focus away and hid it: it stays hidden.
        } else {
            let r = by_tray();
            unsafe { MoveWindow(h, r.left, r.top, r.right - r.left, r.bottom - r.top, 0) };
            show(h, true);
        }
        return;
    }
    let url = page_url(Kind::Panel);
    let h = unsafe { new_window(Kind::Panel, &url, by_tray(), "soundya") };
    if h.is_null() {
        let _ = edge_app(&page_url(Kind::Big), Kind::Big);
        return;
    }
    app(|a| a.panel = Some(h));
    make_view(h);
    show(h, true);
}

/// «Размер окна»: one of SIZES, by the tray.
fn panel_size(i: usize) {
    let r = panel_default(i);
    save_layout("panel", json!([r.left, r.top, r.right - r.left, r.bottom - r.top]));
    match app(|a| a.panel) {
        Some(h) => {
            unsafe { MoveWindow(h, r.left, r.top, r.right - r.left, r.bottom - r.top, 1) };
            show(h, true);
        }
        None => open_panel(false),
    }
}

/// «Открыть в большом окне»: the whole panel in an ordinary window, in the middle of the screen.
fn open_big() {
    if let Some(h) = app(|a| a.panel) {
        if visible(Some(h)) {
            hide(h);
        }
    }
    if let Some(h) = app(|a| a.big) {
        show(h, true);
        return;
    }
    let (s, wa) = (scale(), work_area());
    let (w, hh) = (((1100.0 * s) as i32).min(wa.right - wa.left - 40), ((860.0 * s) as i32).min(wa.bottom - wa.top - 40));
    let (x, y) = (wa.left + (wa.right - wa.left - w) / 2, wa.top + (wa.bottom - wa.top - hh) / 2);
    let r = placed("big", RECT { left: x, top: y, right: x + w, bottom: y + hh });
    let url = page_url(Kind::Big);
    let h = unsafe { new_window(Kind::Big, &url, r, "soundya") };
    if h.is_null() {
        let _ = edge_app(&url, Kind::Big);
        return;
    }
    app(|a| a.big = Some(h));
    make_view(h);
    show(h, true);
}

/// The speaker found in the background: every window goes to it.
fn speaker_changed() {
    let Some(host) = NEW_SPEAKER.lock().unwrap().take() else { return };
    if app(|a| a.speaker == host) {
        return;
    }
    let wins = app(|a| {
        a.speaker = host;
        a.status = None;
        a.wins.iter().filter(|(_, w)| w.kind != Kind::Wizard).map(|(h, w)| (*h, w.kind)).collect::<Vec<_>>()
    });
    for (h, kind) in wins {
        let url = page_url(kind);
        app(|a| {
            if let Some(w) = a.wins.get_mut(&h) {
                w.url = url.clone();
            }
        });
        if let Some(v) = view_of(h as HWND) {
            v.navigate(&url);
        }
    }
}

/// «Задачи поверх окон»: shows or hides the small window with /todo above the others.
fn toggle_todo(on: Option<bool>) {
    let existing = app(|a| a.todo);
    let shown = visible(existing);
    let want = on.unwrap_or(!shown);
    if !want {
        if let Some(h) = existing {
            hide(h);
        }
        save_layout("todo_on", json!(false));
        return;
    }
    save_layout("todo_on", json!(true));
    if let Some(h) = existing {
        show(h, false);
        return;
    }
    let url = page_url(Kind::Todo);
    let r = placed("todo", todo_default());
    let h = unsafe { new_window(Kind::Todo, &url, r, "Задачи") };
    if h.is_null() {
        return;
    }
    app(|a| a.todo = Some(h));
    make_view(h);
    show(h, false);
}

/// Without WebView2: the page in Edge's app window (no tabs, no address bar).
fn edge_app(url: &str, kind: Kind) -> bool {
    let edge = winsys::reg_string(HKEY_LOCAL_MACHINE, "SOFTWARE\\Microsoft\\Windows\\CurrentVersion\\App Paths\\msedge.exe", "")
        .or_else(|| winsys::reg_string(HKEY_CURRENT_USER, "SOFTWARE\\Microsoft\\Windows\\CurrentVersion\\App Paths\\msedge.exe", ""))
        .or_else(|| {
            [std::env::var("ProgramFiles(x86)").ok(), std::env::var("ProgramFiles").ok()]
                .into_iter()
                .flatten()
                .map(|p| format!("{p}\\Microsoft\\Edge\\Application\\msedge.exe"))
                .find(|p| std::path::Path::new(p).exists())
        });
    let Some(edge) = edge else { return false };
    let (w, h) = if kind == Kind::Todo { (300, 380) } else { (SIZES[1].0 as i32, SIZES[1].1 as i32) };
    let profile = winwv2::app_dir().join(if kind == Kind::Todo { "edge-todo" } else { "edge" });
    let args = vec![
        format!("--app={url}"),
        format!("--user-data-dir={}", profile.display()),
        format!("--window-size={w},{h}"),
        "--autoplay-policy=no-user-gesture-required".into(),
        "--no-first-run".into(),
    ];
    winsys::spawn(&edge, &args).is_ok()
}

// ------------------------------------------------------------------ the tray icon

/// soundya's icon drawn in code: an orange pill with two white «x» eyes. BGRA, top-down.
pub fn icon_pixels(size: usize) -> Vec<u32> {
    let s = size as f64;
    let (cx, cy, hw, hh) = (s / 2.0, s / 2.0, s * 0.47, s * 0.33);
    let r = hh;
    let eye = |x: f64, y: f64, ex: f64| {
        let (dx, dy) = ((x - ex).abs(), (y - cy).abs());
        let k = s * 0.13;
        dx <= k && dy <= k && (dx - dy).abs() <= s * 0.055
    };
    let mut out = vec![0u32; size * size];
    for py in 0..size {
        for px in 0..size {
            let (mut fill, mut white) = (0u32, 0u32);
            for sy in 0..4 {
                for sx in 0..4 {
                    let x = px as f64 + (sx as f64 + 0.5) / 4.0;
                    let y = py as f64 + (sy as f64 + 0.5) / 4.0;
                    // A rounded rectangle (a capsule).
                    let qx = ((x - cx).abs() - (hw - r)).max(0.0);
                    let qy = ((y - cy).abs() - (hh - r)).max(0.0);
                    if qx * qx + qy * qy <= r * r {
                        fill += 1;
                        if eye(x, y, cx - s * 0.17) || eye(x, y, cx + s * 0.17) {
                            white += 1;
                        }
                    }
                }
            }
            if fill == 0 {
                continue;
            }
            let a = fill * 255 / 16;
            let t = |c: u32, w: u32| (c * (fill - white) + w * white) / fill;
            let (rr, gg, bb) = (t(0xFF, 0xFF), t(0x5A, 0xFF), t(0x1F, 0xFF));
            out[py * size + px] = (a << 24) | (rr << 16) | (gg << 8) | bb;
        }
    }
    out
}

fn make_icon(size: i32) -> HICON {
    unsafe {
        let bi = BITMAPINFOHEADER { biSize: 40, biWidth: size, biHeight: -size, biPlanes: 1, biBitCount: 32, biCompression: 0, biSizeImage: 0, biXPelsPerMeter: 0, biYPelsPerMeter: 0, biClrUsed: 0, biClrImportant: 0 };
        let mut bits: *mut c_void = null_mut();
        let color = CreateDIBSection(null_mut(), &bi, 0, &mut bits, null_mut(), 0);
        if color.is_null() || bits.is_null() {
            return null_mut();
        }
        let px = icon_pixels(size as usize);
        std::ptr::copy_nonoverlapping(px.as_ptr(), bits as *mut u32, px.len());
        let mask = CreateBitmap(size, size, 1, 1, null_mut());
        let info = ICONINFO { fIcon: 1, xHotspot: 0, yHotspot: 0, hbmMask: mask, hbmColor: color };
        let icon = CreateIconIndirect(&info);
        DeleteObject(color);
        DeleteObject(mask);
        icon
    }
}

fn tray_icon(add: bool) {
    let h = app(|a| a.tray);
    unsafe {
        let mut d: NOTIFYICONDATAW = std::mem::zeroed();
        d.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
        d.hWnd = h;
        d.uID = 1;
        d.uFlags = 0x1 | 0x2 | 0x4; // NIF_MESSAGE | NIF_ICON | NIF_TIP
        d.uCallbackMessage = WM_APP_TRAY;
        d.hIcon = make_icon(GetSystemMetrics(49).max(16)); // SM_CXSMICON
        let tip = wide("soundya");
        d.szTip[..tip.len()].copy_from_slice(&tip);
        Shell_NotifyIconW(if add { 0 } else { 2 }, &mut d); // NIM_ADD / NIM_DELETE
    }
}

/// A short call to the speaker's panel (play/pause, next, the status for the menu).
fn speaker_call(host: &str, method: &str, path: &str, body: &str) -> Option<(u16, String)> {
    use std::io::{Read, Write};
    use std::net::ToSocketAddrs;
    let addr = format!("{host}:8765").to_socket_addrs().ok()?.next()?;
    let mut s = std::net::TcpStream::connect_timeout(&addr, Duration::from_millis(800)).ok()?;
    s.set_read_timeout(Some(Duration::from_secs(3))).ok();
    write!(s, "{method} {path} HTTP/1.1\r\nHost: {host}:8765\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).ok()?;
    let mut out = String::new();
    s.read_to_string(&mut out).ok()?;
    let code = out.split_whitespace().nth(1).and_then(|c| c.parse().ok()).unwrap_or(0);
    Some((code, out.split_once("\r\n\r\n").map_or("", |(_, b)| b).to_owned()))
}

/// What the menu shows on top: (the line, playing?) — as on the Mac.
pub fn now_line(code: Option<u16>, status: &str) -> (String, bool) {
    match code {
        None => ("Колонка не отвечает".into(), false),
        Some(401) => ("На панели PIN — откройте soundya".into(), false),
        Some(200) => {
            let v: Value = serde_json::from_str(status).unwrap_or_default();
            let playing = v["state"] == "playing";
            let track = v["track"].as_str().unwrap_or("").trim();
            let line = if track.is_empty() { "Ничего не играет".to_owned() } else { format!("{} {track}", if playing { "▶" } else { "❚❚" }) };
            (line.chars().take(60).collect(), playing)
        }
        Some(c) => (format!("Колонка ответила {c}"), false),
    }
}

fn in_background(f: impl FnOnce() + Send + 'static) {
    std::thread::spawn(f);
}

/// The speaker's status for the menu: asked in the background, waited for 700 ms at most.
fn fresh_status() -> (Option<u16>, String) {
    let host = app(|a| a.speaker.clone());
    let (tx, rx) = std::sync::mpsc::channel();
    in_background(move || {
        let _ = tx.send(speaker_call(&host, "GET", "/api/status", ""));
    });
    match rx.recv_timeout(Duration::from_millis(700)) {
        Ok(r) => {
            let s = (r.as_ref().map(|r| r.0), r.map(|r| r.1).unwrap_or_default());
            app(|a| a.status = Some(s.clone()));
            s
        }
        Err(_) => app(|a| a.status.clone()).unwrap_or((None, String::new())),
    }
}

mod cmd {
    pub const NOW: usize = 1;
    pub const PLAY: usize = 2;
    pub const NEXT: usize = 3;
    pub const OPEN: usize = 4;
    pub const BIG: usize = 5;
    pub const TODO: usize = 6;
    pub const SMALL: usize = 7;
    pub const MEDIUM: usize = 8;
    pub const LARGE: usize = 9;
    pub const INSTALLER: usize = 10;
    pub const CONNECT: usize = 11;
    pub const SETUP: usize = 12;
    pub const LOG: usize = 13;
    pub const FOLDER: usize = 14;
    pub const RESTART: usize = 15;
    pub const QUIT: usize = 16;
}

fn menu() {
    let (code, body) = fresh_status();
    let (line, playing) = now_line(code, &body);
    let todo_on = visible(app(|a| a.todo));
    unsafe {
        let m = CreatePopupMenu();
        let sizes = CreatePopupMenu();
        let add = |m: HMENU, flags: u32, id: usize, t: &str| AppendMenuW(m, flags, id, wide(t).as_ptr());
        // MF_STRING 0, MF_GRAYED 1, MF_CHECKED 8, MF_POPUP 0x10, MF_SEPARATOR 0x800
        add(m, 1, cmd::NOW, &line);
        add(m, 0x800, 0, "");
        add(m, if code == Some(200) { 0 } else { 1 }, cmd::PLAY, if playing { "Пауза" } else { "Играть" });
        add(m, if code == Some(200) { 0 } else { 1 }, cmd::NEXT, "Следующий трек");
        add(m, 0x800, 0, "");
        add(m, 0, cmd::OPEN, "Открыть soundya");
        add(m, 0, cmd::BIG, "Открыть в большом окне");
        add(m, if todo_on { 8 } else { 0 }, cmd::TODO, "Задачи поверх окон");
        add(sizes, 0, cmd::SMALL, "Маленький");
        add(sizes, 0, cmd::MEDIUM, "Средний");
        add(sizes, 0, cmd::LARGE, "Большой");
        AppendMenuW(m, 0x10, sizes as usize, wide("Размер окна").as_ptr());
        add(m, 0x800, 0, "");
        add(m, 0, cmd::INSTALLER, "Установщик и обновление…");
        add(m, 0, cmd::CONNECT, "Подключить этот компьютер к колонке");
        add(m, 0, cmd::SETUP, "Данные для колонки (адрес, ключ, MAC)…");
        add(m, 0, cmd::LOG, "Открыть лог");
        add(m, 0, cmd::FOLDER, "Открыть папку настроек");
        add(m, 0, cmd::RESTART, "Перезапустить агент");
        add(m, 0x800, 0, "");
        add(m, 0, cmd::QUIT, "Выйти");
        let tray = app(|a| a.tray);
        let mut p = POINT::default();
        GetCursorPos(&mut p);
        // The menu goes away on a click elsewhere only if its window is in front.
        SetForegroundWindow(tray);
        // TPM_RETURNCMD | TPM_NONOTIFY | TPM_RIGHTBUTTON
        let id = TrackPopupMenu(m, 0x100 | 0x80 | 0x2, p.x, p.y, 0, tray, std::ptr::null()) as usize;
        PostMessageW(tray, 0, 0, 0);
        DestroyMenu(m);
        command(id);
    }
}

fn command(id: usize) {
    let host = app(|a| a.speaker.clone());
    match id {
        cmd::PLAY => in_background(move || drop(speaker_call(&host, "POST", "/api/cmd", "toggle"))),
        cmd::NEXT => in_background(move || drop(speaker_call(&host, "POST", "/api/cmd", "next"))),
        cmd::OPEN => open_panel(false),
        cmd::BIG => open_big(),
        cmd::TODO => toggle_todo(None),
        cmd::SMALL => panel_size(0),
        cmd::MEDIUM => panel_size(1),
        cmd::LARGE => panel_size(2),
        cmd::INSTALLER => {
            use std::os::windows::process::CommandExt;
            let _ = std::process::Command::new(exe()).env("SOUNDYA_INSTALLER", "1").creation_flags(0x0800_0000).spawn();
        }
        cmd::CONNECT => in_background(move || {
            use std::net::ToSocketAddrs;
            let ip = format!("{host}:8765").to_socket_addrs().ok().and_then(|mut a| a.next()).map(|a| a.ip().to_string());
            let text = match ip {
                Some(ip) => crate::pc_setup(&ip).unwrap_or_else(|e| format!("Не получилось: {e}")),
                None => "Колонка не нашлась в сети".to_owned(),
            };
            message_box(&text, "soundya");
        }),
        cmd::SETUP => in_background(move || {
            let key = Config::load(&pc_dir()).map(|c| c.key).unwrap_or_default();
            let (ip, mac) = crate::winsetup::address_for(&host);
            let copied = !key.is_empty() && set_clipboard(&key);
            message_box(
                &format!(
                    "Впишите в панели колонки (Устройства → Компьютер):\n\nАдрес компьютера:  {ip}\nMAC-адрес:  {}\n\n{}",
                    if mac.is_empty() { "нет сетевого кабеля" } else { &mac },
                    if copied { "Ключ скопирован — вставьте его в поле «Ключ»." } else { "Ключа ещё нет: подключите компьютер к колонке." }
                ),
                "soundya: данные для колонки",
            );
        }),
        cmd::LOG => {
            let log = pc_dir().join("log.txt");
            if winsys::shell_open(&log.to_string_lossy()).is_err() {
                message_box("Лога пока нет", "soundya");
            }
        }
        cmd::FOLDER => {
            let _ = winsys::spawn("explorer.exe", &[pc_dir().to_string_lossy().into_owned()]);
        }
        cmd::RESTART => in_background(|| {
            let st = winagent::Status::load(&pc_dir());
            if st.pid > 0 && winsys::process_path(st.pid).is_some_and(|p| p.to_lowercase().ends_with("soundya.exe")) {
                winsys::kill(st.pid);
                std::thread::sleep(Duration::from_millis(500));
            }
            start_agent();
        }),
        cmd::QUIT => {
            for h in app(|a| a.wins.keys().copied().collect::<Vec<_>>()) {
                if let Some(k) = app(|a| a.wins.get(&h).map(|w| w.kind)) {
                    if unsafe { IsWindowVisible(h as HWND) } != 0 {
                        remember(h as HWND, k);
                    }
                }
            }
            tray_icon(false);
            unsafe { PostQuitMessage(0) };
        }
        _ => {}
    }
}

fn local_time() -> (i32, u32, u32, u32, u32, u32) {
    winagent::Sys::local_time(&WinSys::new(true))
}

static TASKBAR_CREATED: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

unsafe extern "system" fn wndproc(h: HWND, msg: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    let kind = APP.with(|a| a.try_borrow().ok().and_then(|a| a.wins.get(&(h as isize)).map(|w| w.kind)));
    match msg {
        WM_APP_SHOW => {
            open_panel(false);
            return 0;
        }
        WM_APP_TRAY => {
            match l as u32 {
                0x0202 => open_panel(true), // WM_LBUTTONUP
                0x0205 | 0x007B => menu(),  // WM_RBUTTONUP, WM_CONTEXTMENU
                _ => {}
            }
            return 0;
        }
        WM_APP_SPEAKER => {
            speaker_changed();
            return 0;
        }
        WM_APP_RUN => {
            if w == 1 {
                make_view(h);
            }
            return 0;
        }
        0x0113 if kind.is_none() => {
            // WM_TIMER on the tray's window: the agent is kept running.
            start_agent();
            return 0;
        }
        0x0005 | 0x0003 => {
            // WM_SIZE / WM_MOVE
            if let Some(v) = view_of(h) {
                if msg == 0x0005 {
                    v.fit(h);
                } else {
                    v.moved();
                }
            }
        }
        0x0232 => {
            // WM_EXITSIZEMOVE: the place is kept.
            if let Some(k) = kind {
                remember(h, k);
            }
        }
        0x02E0 => {
            // WM_DPICHANGED: the size Windows suggests for the new screen.
            let r = &*(l as *const RECT);
            SetWindowPos(h, null_mut(), r.left, r.top, r.right - r.left, r.bottom - r.top, 0x4 | 0x10);
            return 0;
        }
        0x0006 if kind == Some(Kind::Panel) && w & 0xFFFF == 0 => {
            // WM_ACTIVATE, WA_INACTIVE: a click elsewhere hides the small player (as on the Mac).
            app(|a| a.panel_left = Some(std::time::Instant::now()));
            hide(h);
        }
        0x0007 => {
            // WM_SETFOCUS: to the page.
            if let Some(v) = view_of(h) {
                v.focus();
            }
        }
        0x0010 => {
            // WM_CLOSE: the panel and the to-do list hide (the music goes on); the wizard ends.
            match kind {
                Some(Kind::Wizard) => {
                    remember(h, Kind::Wizard);
                    PostQuitMessage(0);
                }
                Some(_) => hide(h),
                None => {}
            }
            return 0;
        }
        _ => {}
    }
    if msg != 0 && msg == TASKBAR_CREATED.load(std::sync::atomic::Ordering::Relaxed) {
        // Explorer restarted: the icon again.
        tray_icon(true);
    }
    DefWindowProcW(h, msg, w, l)
}

unsafe fn register_classes() {
    let inst = GetModuleHandleW(std::ptr::null());
    let big = make_icon(GetSystemMetrics(11).max(32));
    let small = make_icon(GetSystemMetrics(49).max(16));
    for (name, bg) in [(TRAY_CLASS, 0x000000u32), (WIN_CLASS, PANEL_BG)] {
        let n = wide(name);
        // COLORREF is 0x00BBGGRR.
        let brush = CreateSolidBrush(((bg & 0xFF) << 16) | (bg & 0xFF00) | ((bg >> 16) & 0xFF));
        let c = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            style: 0,
            lpfnWndProc: Some(wndproc),
            cbClsExtra: 0,
            cbWndExtra: 0,
            hInstance: inst,
            hIcon: big,
            hCursor: LoadCursorW(null_mut(), 32512 as *const u16), // IDC_ARROW
            hbrBackground: brush,
            lpszMenuName: std::ptr::null(),
            lpszClassName: n.as_ptr(),
            hIconSm: small,
        };
        RegisterClassExW(&c);
    }
}

fn pump() {
    unsafe {
        let mut m: MSG = std::mem::zeroed();
        while GetMessageW(&mut m, null_mut(), 0, 0) > 0 {
            TranslateMessage(&m);
            DispatchMessageW(&m);
        }
    }
}

/// `soundya.exe tray` (and soundya.exe opened by hand on a connected PC): the tray icon.
/// One per user: a second start shows the first one's window.
pub fn tray(open_now: bool) -> ! {
    if !own_mutex(TRAY_MUTEX) {
        unsafe {
            let h = FindWindowW(wide(TRAY_CLASS).as_ptr(), std::ptr::null());
            if !h.is_null() && open_now {
                PostMessageW(h, WM_APP_SHOW, 0, 0);
            }
            std::process::exit(0);
        }
    }
    winsys::dpi_aware();
    unsafe {
        CoInitializeEx(null_mut(), 0x2); // WebView2 wants a single-threaded apartment
        register_classes();
        let inst = GetModuleHandleW(std::ptr::null());
        let tray = CreateWindowExW(0, wide(TRAY_CLASS).as_ptr(), wide("soundya").as_ptr(), 0, 0, 0, 0, 0, null_mut(), null_mut(), inst, null_mut());
        if tray.is_null() {
            message_box("Значок soundya не создался", "soundya");
            std::process::exit(1);
        }
        TASKBAR_CREATED.store(RegisterWindowMessageW(wide("TaskbarCreated").as_ptr()), std::sync::atomic::Ordering::Relaxed);
        app(|a| {
            a.tray = tray;
            a.speaker = speaker();
        });
        tray_icon(true);
        // The agent: started if it is not running, checked again every 30 s.
        if set_up() {
            start_agent();
            SetTimer(tray, 1, 30_000, null_mut());
        }
        if layout()["todo_on"].as_bool() == Some(true) {
            toggle_todo(Some(true));
        }
        if open_now {
            open_panel(false);
        }
        // This PC: connected quietly in the background if it is not yet (the speaker found
        // first if this PC does not know it).
        crate::winsetup::connect_in_background(tray as usize);
    }
    pump();
    tray_icon(false);
    std::process::exit(0)
}

/// The installer's wizard in its own window; returns when it is closed. Err: no window
/// (the caller opens the browser instead).
pub fn wizard(url: &str) -> Result<(), String> {
    winsys::dpi_aware();
    unsafe {
        CoInitializeEx(null_mut(), 0x2);
        register_classes();
        app(|a| a.speaker = speaker());
        let (s, wa) = (scale(), work_area());
        let (w, h) = (((900.0 * s) as i32).min(wa.right - wa.left - 40), ((780.0 * s) as i32).min(wa.bottom - wa.top - 40));
        let def = RECT { left: wa.left + (wa.right - wa.left - w) / 2, top: wa.top + (wa.bottom - wa.top - h) / 2, right: 0, bottom: 0 };
        let def = RECT { right: def.left + w, bottom: def.top + h, ..def };
        let r = placed("wizard", def);
        let hwnd = new_window(Kind::Wizard, url, r, "soundya — установка");
        if hwnd.is_null() {
            return Err("окно не создалось".into());
        }
        make_view(hwnd);
        // make_view may have found no WebView2 and closed the window: then the browser.
        if app(|a| a.wins.contains_key(&(hwnd as isize))) {
            show(hwnd, true);
        } else {
            return Err("нет WebView2".into());
        }
    }
    let mut m: MSG = unsafe { std::mem::zeroed() };
    let code = unsafe {
        loop {
            let r = GetMessageW(&mut m, null_mut(), 0, 0);
            if r <= 0 {
                break m.wParam;
            }
            TranslateMessage(&m);
            DispatchMessageW(&m);
        }
    };
    if code == 1 {
        Err("нет WebView2".into())
    } else {
        Ok(())
    }
}
