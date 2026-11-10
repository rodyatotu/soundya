//! The installer's own window on macOS: an NSWindow with a WKWebView showing
//! the wizard (served on 127.0.0.1 by this same program), an app menu and an
//! Edit menu (⌘C/⌘V/⌘A work in it). No browser is involved.
//!
//! AppKit and WebKit are opened at run time and driven through the
//! Objective-C runtime (objc_msgSend with the exact C signature of each call),
//! so the program is built on Linux without Apple's SDK. If anything is
//! missing the caller falls back to the browser.

#![allow(non_upper_case_globals, clippy::missing_safety_doc)]

use std::ffi::{c_char, c_void, CString};

type Id = *mut c_void;
type Sel = *mut c_void;

extern "C" {
    fn dlopen(path: *const c_char, mode: i32) -> *mut c_void;
    fn dlsym(handle: *mut c_void, symbol: *const c_char) -> *mut c_void;
}

const RTLD_NOW: i32 = 2;

#[repr(C)]
#[derive(Clone, Copy)]
struct Rect {
    x: f64,
    y: f64,
    w: f64,
    h: f64,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct Size {
    w: f64,
    h: f64,
}

struct Rt {
    get_class: unsafe extern "C" fn(*const c_char) -> Id,
    sel: unsafe extern "C" fn(*const c_char) -> Sel,
    msg: *mut c_void,
    alloc_class: unsafe extern "C" fn(Id, *const c_char, usize) -> Id,
    add_method: unsafe extern "C" fn(Id, Sel, *mut c_void, *const c_char) -> i8,
    register: unsafe extern "C" fn(Id),
}

unsafe fn sym(h: *mut c_void, name: &str) -> Result<*mut c_void, String> {
    let c = CString::new(name).unwrap();
    let p = dlsym(h, c.as_ptr());
    if p.is_null() {
        Err(format!("нет {name}"))
    } else {
        Ok(p)
    }
}

impl Rt {
    unsafe fn load() -> Result<Rt, String> {
        let objc = dlopen(c"/usr/lib/libobjc.A.dylib".as_ptr(), RTLD_NOW);
        if objc.is_null() {
            return Err("нет libobjc".into());
        }
        for f in [
            c"/System/Library/Frameworks/Foundation.framework/Foundation",
            c"/System/Library/Frameworks/AppKit.framework/AppKit",
            c"/System/Library/Frameworks/WebKit.framework/WebKit",
        ] {
            if dlopen(f.as_ptr(), RTLD_NOW).is_null() {
                return Err(format!("нет {}", f.to_string_lossy()));
            }
        }
        Ok(Rt {
            get_class: std::mem::transmute::<*mut c_void, _>(sym(objc, "objc_getClass")?),
            sel: std::mem::transmute::<*mut c_void, _>(sym(objc, "sel_registerName")?),
            msg: sym(objc, "objc_msgSend")?,
            alloc_class: std::mem::transmute::<*mut c_void, _>(sym(objc, "objc_allocateClassPair")?),
            add_method: std::mem::transmute::<*mut c_void, _>(sym(objc, "class_addMethod")?),
            register: std::mem::transmute::<*mut c_void, _>(sym(objc, "objc_registerClassPair")?),
        })
    }

    unsafe fn class(&self, name: &str) -> Result<Id, String> {
        let c = CString::new(name).unwrap();
        let k = (self.get_class)(c.as_ptr());
        if k.is_null() {
            Err(format!("нет класса {name}"))
        } else {
            Ok(k)
        }
    }

    unsafe fn s(&self, name: &str) -> Sel {
        let c = CString::new(name).unwrap();
        (self.sel)(c.as_ptr())
    }

    // objc_msgSend, cast to the exact signature of each call.
    unsafe fn send(&self, o: Id, sel: &str) -> Id {
        let f: unsafe extern "C" fn(Id, Sel) -> Id = std::mem::transmute(self.msg);
        f(o, self.s(sel))
    }
    unsafe fn send_id(&self, o: Id, sel: &str, a: Id) -> Id {
        let f: unsafe extern "C" fn(Id, Sel, Id) -> Id = std::mem::transmute(self.msg);
        f(o, self.s(sel), a)
    }
    unsafe fn send_id_id(&self, o: Id, sel: &str, a: Id, b: Id) -> Id {
        let f: unsafe extern "C" fn(Id, Sel, Id, Id) -> Id = std::mem::transmute(self.msg);
        f(o, self.s(sel), a, b)
    }
    unsafe fn send_title_action_key(&self, o: Id, title: Id, action: Sel, key: Id) -> Id {
        let f: unsafe extern "C" fn(Id, Sel, Id, Sel, Id) -> Id = std::mem::transmute(self.msg);
        f(o, self.s("initWithTitle:action:keyEquivalent:"), title, action, key)
    }
    unsafe fn send_bool(&self, o: Id, sel: &str, b: bool) {
        let f: unsafe extern "C" fn(Id, Sel, i8) = std::mem::transmute(self.msg);
        f(o, self.s(sel), b as i8)
    }
    unsafe fn send_long(&self, o: Id, sel: &str, n: i64) -> Id {
        let f: unsafe extern "C" fn(Id, Sel, i64) -> Id = std::mem::transmute(self.msg);
        f(o, self.s(sel), n)
    }
    unsafe fn send_ulong(&self, o: Id, sel: &str, n: u64) {
        let f: unsafe extern "C" fn(Id, Sel, u64) = std::mem::transmute(self.msg);
        f(o, self.s(sel), n)
    }
    unsafe fn send_size(&self, o: Id, sel: &str, s: Size) {
        let f: unsafe extern "C" fn(Id, Sel, Size) = std::mem::transmute(self.msg);
        f(o, self.s(sel), s)
    }
    unsafe fn rect_of(&self, o: Id, sel: &str) -> Rect {
        let f: unsafe extern "C" fn(Id, Sel) -> Rect = std::mem::transmute(self.msg);
        f(o, self.s(sel))
    }
    unsafe fn send_sel(&self, o: Id, sel: &str, a: Sel) {
        let f: unsafe extern "C" fn(Id, Sel, Sel) = std::mem::transmute(self.msg);
        f(o, self.s(sel), a)
    }
    unsafe fn send_u64_ret(&self, o: Id, sel: &str) -> u64 {
        let f: unsafe extern "C" fn(Id, Sel) -> u64 = std::mem::transmute(self.msg);
        f(o, self.s(sel))
    }
    unsafe fn data(&self, bytes: &[u8]) -> Result<Id, String> {
        let f: unsafe extern "C" fn(Id, Sel, *const c_void, usize) -> Id = std::mem::transmute(self.msg);
        Ok(f(self.class("NSData")?, self.s("dataWithBytes:length:"), bytes.as_ptr() as *const c_void, bytes.len()))
    }
    unsafe fn send_f64(&self, o: Id, sel: &str, v: f64) -> Id {
        let f: unsafe extern "C" fn(Id, Sel, f64) -> Id = std::mem::transmute(self.msg);
        f(o, self.s(sel), v)
    }
    unsafe fn send_rgba(&self, o: Id, sel: &str, r: f64, g: f64, b: f64, a: f64) -> Id {
        let f: unsafe extern "C" fn(Id, Sel, f64, f64, f64, f64) -> Id = std::mem::transmute(self.msg);
        f(o, self.s(sel), r, g, b, a)
    }
    unsafe fn init_window(&self, o: Id, r: Rect, style: u64) -> Id {
        let f: unsafe extern "C" fn(Id, Sel, Rect, u64, u64, i8) -> Id = std::mem::transmute(self.msg);
        f(o, self.s("initWithContentRect:styleMask:backing:defer:"), r, style, 2, 0)
    }
    unsafe fn init_webview(&self, o: Id, r: Rect, config: Id) -> Id {
        let f: unsafe extern "C" fn(Id, Sel, Rect, Id) -> Id = std::mem::transmute(self.msg);
        f(o, self.s("initWithFrame:configuration:"), r, config)
    }

    unsafe fn string(&self, s: &str) -> Result<Id, String> {
        let c = CString::new(s).map_err(|e| e.to_string())?;
        let f: unsafe extern "C" fn(Id, Sel, *const c_char) -> Id = std::mem::transmute(self.msg);
        Ok(f(self.class("NSString")?, self.s("stringWithUTF8String:"), c.as_ptr()))
    }

    unsafe fn new(&self, class: &str) -> Result<Id, String> {
        let o = self.send(self.class(class)?, "alloc");
        Ok(self.send(o, "init"))
    }

    unsafe fn item(&self, title: &str, action: &str, key: &str) -> Result<Id, String> {
        let o = self.send(self.class("NSMenuItem")?, "alloc");
        Ok(self.send_title_action_key(o, self.string(title)?, self.s(action), self.string(key)?))
    }
}

/// `applicationShouldTerminateAfterLastWindowClosed:` → YES.
extern "C" fn quit_after_last_window(_this: Id, _sel: Sel, _app: Id) -> i8 {
    1
}

/// Shows the wizard at `url` in a native window and runs the app until it is closed.
pub fn run(url: &str, title: &str) -> Result<(), String> {
    unsafe {
        let rt = Rt::load()?;
        let app = rt.send(rt.class("NSApplication")?, "sharedApplication");
        rt.send_long(app, "setActivationPolicy:", 0); // regular: Dock icon and menu bar

        // Closing the window quits.
        let base = rt.class("NSObject")?;
        let name = CString::new("SoundyaAppDelegate").unwrap();
        let mut delegate_class = rt.class("SoundyaAppDelegate").unwrap_or(std::ptr::null_mut());
        if delegate_class.is_null() {
            delegate_class = (rt.alloc_class)(base, name.as_ptr(), 0);
            if delegate_class.is_null() {
                return Err("не создался класс делегата".into());
            }
            let types = CString::new("c@:@").unwrap();
            (rt.add_method)(delegate_class, rt.s("applicationShouldTerminateAfterLastWindowClosed:"), quit_after_last_window as *mut c_void, types.as_ptr());
            (rt.register)(delegate_class);
        }
        let delegate = rt.send(rt.send(delegate_class, "alloc"), "init");
        rt.send_id(app, "setDelegate:", delegate);

        main_menu(&rt, app, title)?;
        let window = panel_window(&rt, url, title)?;
        rt.send(window, "center");
        rt.send_id(window, "makeKeyAndOrderFront:", std::ptr::null_mut());
        rt.send_bool(app, "activateIgnoringOtherApps:", true);
        rt.send(app, "run");
    }
    Ok(())
}

/// The app menu (hide, quit) and «Правка»: ⌘C/⌘V/⌘A in the page.
unsafe fn main_menu(rt: &Rt, app: Id, title: &str) -> Result<(), String> {
    let bar = rt.new("NSMenu")?;
    let app_item = rt.new("NSMenuItem")?;
    rt.send_id(bar, "addItem:", app_item);
    let app_menu = rt.new("NSMenu")?;
    rt.send_id(app_menu, "addItem:", rt.item(&format!("Скрыть {title}"), "hide:", "h")?);
    rt.send_id(app_menu, "addItem:", rt.send(rt.class("NSMenuItem")?, "separatorItem"));
    rt.send_id(app_menu, "addItem:", rt.item(&format!("Завершить {title}"), "terminate:", "q")?);
    rt.send_id(app_item, "setSubmenu:", app_menu);
    let edit_item = rt.new("NSMenuItem")?;
    rt.send_id(bar, "addItem:", edit_item);
    let edit_menu = rt.send_id(rt.send(rt.class("NSMenu")?, "alloc"), "initWithTitle:", rt.string("Правка")?);
    for (t, a, k) in [
        ("Отменить", "undo:", "z"),
        ("Повторить", "redo:", "Z"),
        ("Вырезать", "cut:", "x"),
        ("Скопировать", "copy:", "c"),
        ("Вставить", "paste:", "v"),
        ("Выбрать всё", "selectAll:", "a"),
    ] {
        rt.send_id(edit_menu, "addItem:", rt.item(t, a, k)?);
    }
    rt.send_id(edit_item, "setSubmenu:", edit_menu);
    rt.send_id(app, "setMainMenu:", bar);
    Ok(())
}

/// The page at `url` in a WKWebView, transparent until it paints.
unsafe fn web_view(rt: &Rt, url: &str, frame: Rect) -> Result<Id, String> {
    let config = rt.new("WKWebViewConfiguration")?;
    // «Где играет → этот браузер»: the page starts the sound itself (no click in it), which
    // WebKit refuses by default — the music did not play in the Mac's window.
    rt.send_ulong(config, "setMediaTypesRequiringUserActionForPlayback:", 0);
    let web = rt.init_webview(rt.send(rt.class("WKWebView")?, "alloc"), frame, config);
    if web.is_null() {
        return Err("WebKit не создал окно страницы".into());
    }
    rt.send_ulong(web, "setAutoresizingMask:", 2 | 16); // width | height sizable
    let no = rt.send_bool_number(false)?;
    rt.send_id_id(web, "setValue:forKey:", no, rt.string("drawsBackground")?);
    let nsurl = rt.send_id(rt.class("NSURL")?, "URLWithString:", rt.string(url)?);
    let req = rt.send_id(rt.class("NSURLRequest")?, "requestWithURL:", nsurl);
    rt.send_id(web, "loadRequest:", req);
    Ok(web)
}

/// A dark window with the page at `url` in a WKWebView (not shown yet).
unsafe fn panel_window(rt: &Rt, url: &str, title: &str) -> Result<Id, String> {
    let frame = Rect { x: 0.0, y: 0.0, w: 920.0, h: 680.0 };
    // titled | closable | miniaturizable | resizable
    let window = rt.init_window(rt.send(rt.class("NSWindow")?, "alloc"), frame, 1 | 2 | 4 | 8);
    if window.is_null() {
        return Err("окно не создалось".into());
    }
    rt.send_bool(window, "setReleasedWhenClosed:", false);
    rt.send_id(window, "setTitle:", rt.string(title)?);
    rt.send_bool(window, "setTitlebarAppearsTransparent:", true);
    if let Ok(appearance) = rt.class("NSAppearance") {
        let dark = rt.send_id(appearance, "appearanceNamed:", rt.string("NSAppearanceNameDarkAqua")?);
        if !dark.is_null() {
            rt.send_id(window, "setAppearance:", dark);
        }
    }
    let bg = rt.send_rgba(rt.class("NSColor")?, "colorWithSRGBRed:green:blue:alpha:", 0.055, 0.047, 0.086, 1.0);
    rt.send_id(window, "setBackgroundColor:", bg);
    rt.send_size(window, "setContentMinSize:", Size { w: 640.0, h: 560.0 });
    let web = web_view(rt, url, frame)?;
    rt.send_id(window, "setContentView:", web);
    rt.send(window, "center");
    Ok(window)
}

// ------------------------------------------------------------------ menu bar

use std::sync::atomic::{AtomicUsize, Ordering};

/// The panel's window (made on the first «Открыть»), the «now playing» and play items.
static PANEL: AtomicUsize = AtomicUsize::new(0);
static NOW_ITEM: AtomicUsize = AtomicUsize::new(0);
static PLAY_ITEM: AtomicUsize = AtomicUsize::new(0);
static STATUS_ITEM: AtomicUsize = AtomicUsize::new(0);
static STATUS_MENU: AtomicUsize = AtomicUsize::new(0);
static SPEAKER: std::sync::Mutex<String> = std::sync::Mutex::new(String::new());

fn speaker() -> String {
    SPEAKER.lock().unwrap().clone()
}

/// A short HTTP call to the speaker's panel (an address or soundya.local).
fn speaker_call(method: &str, path: &str, body: &str) -> Option<(u16, String)> {
    use std::io::{Read, Write};
    use std::net::ToSocketAddrs;
    let host = speaker();
    let addr = format!("{host}:8765").to_socket_addrs().ok()?.next()?;
    let mut s = std::net::TcpStream::connect_timeout(&addr, std::time::Duration::from_millis(1500)).ok()?;
    s.set_read_timeout(Some(std::time::Duration::from_secs(3))).ok();
    write!(s, "{method} {path} HTTP/1.1\r\nHost: {host}:8765\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).ok()?;
    let mut out = String::new();
    s.read_to_string(&mut out).ok()?;
    let code = out.split_whitespace().nth(1).and_then(|c| c.parse().ok()).unwrap_or(0);
    Some((code, out.split_once("\r\n\r\n").map_or("", |(_, b)| b).to_owned()))
}

/// What the menu shows on top: (the line, playing?).
pub fn now_line(code: Option<u16>, status: &str) -> (String, bool) {
    match code {
        None => ("Колонка не отвечает".into(), false),
        Some(401) => ("На панели PIN — откройте soundya".into(), false),
        Some(200) => {
            let v: serde_json::Value = serde_json::from_str(status).unwrap_or_default();
            let playing = v["state"] == "playing";
            let track = v["track"].as_str().unwrap_or("").trim();
            let line = if track.is_empty() { "Ничего не играет".to_owned() } else { format!("{} {track}", if playing { "▶" } else { "❚❚" }) };
            (line.chars().take(60).collect(), playing)
        }
        Some(c) => (format!("Колонка ответила {c}"), false),
    }
}

extern "C" fn menu_needs_update(_this: Id, _sel: Sel, _menu: Id) {
    let r = speaker_call("GET", "/api/status", "");
    let (line, playing) = now_line(r.as_ref().map(|r| r.0), r.as_ref().map_or("", |r| r.1.as_str()));
    unsafe {
        let Ok(rt) = Rt::load() else { return };
        let (now, play) = (NOW_ITEM.load(Ordering::Acquire) as Id, PLAY_ITEM.load(Ordering::Acquire) as Id);
        if let Ok(t) = rt.string(&line) {
            rt.send_id(now, "setTitle:", t);
        }
        if let Ok(t) = rt.string(if playing { "Пауза" } else { "Играть" }) {
            rt.send_id(play, "setTitle:", t);
        }
    }
}

extern "C" fn toggle_play(_this: Id, _sel: Sel, _sender: Id) {
    std::thread::spawn(|| speaker_call("POST", "/api/cmd", "toggle"));
}

extern "C" fn next_track(_this: Id, _sel: Sel, _sender: Id) {
    std::thread::spawn(|| speaker_call("POST", "/api/cmd", "next"));
}

static GLASS: AtomicUsize = AtomicUsize::new(0);
static TARGET: AtomicUsize = AtomicUsize::new(0);
/// When the window hid itself (a click elsewhere): a click on the icon right then is that
/// same click, not «show again».
static HIDDEN_AT: std::sync::Mutex<Option<std::time::Instant>> = std::sync::Mutex::new(None);

/// A click outside soundya: the window goes, like the menu bar's own windows.
extern "C" fn window_resigned(_this: Id, _sel: Sel, _note: Id) {
    unsafe {
        let Ok(rt) = Rt::load() else { return };
        let win = GLASS.load(Ordering::Acquire) as Id;
        if !win.is_null() && rt.send_u64_ret(win, "isVisible") != 0 {
            // Not while its own sheet or alert has the focus.
            let app = rt.send(rt.class("NSApplication").unwrap_or(std::ptr::null_mut()), "sharedApplication");
            let key = rt.send(app, "keyWindow");
            if !key.is_null() && key != win {
                return;
            }
            rt.send_id(win, "orderOut:", std::ptr::null_mut());
            *HIDDEN_AT.lock().unwrap() = Some(std::time::Instant::now());
        }
    }
}
static GLASS_WEB: AtomicUsize = AtomicUsize::new(0);
/// Sizes from the icon's menu («Размер»): width, height.
const SIZES: [(f64, f64); 3] = [(380.0, 560.0), (430.0, 780.0), (560.0, 900.0)];

/// The window under the icon: the whole soundya on glass (the page's «#glass»), resized by
/// its edges (macOS keeps the size), a click on the icon shows or hides it.
unsafe fn toggle_popover(rt: &Rt) -> Result<(), String> {
    let mut win = GLASS.load(Ordering::Acquire) as Id;
    if !win.is_null() && rt.send_u64_ret(win, "isVisible") != 0 {
        rt.send_id(win, "orderOut:", std::ptr::null_mut());
        return Ok(());
    }
    if HIDDEN_AT.lock().unwrap().take().is_some_and(|t| t.elapsed() < std::time::Duration::from_millis(400)) {
        return Ok(());
    }
    if win.is_null() {
        let (w, h) = SIZES[1];
        let frame = Rect { x: 0.0, y: 0.0, w, h };
        // titled | closable | resizable | full size content view: no title bar to see, the edges resize.
        win = rt.init_window(rt.send(rt.class("NSWindow")?, "alloc"), frame, 1 | 2 | 8 | (1 << 15));
        if win.is_null() {
            return Err("окно не создалось".into());
        }
        rt.send_bool(win, "setReleasedWhenClosed:", false);
        rt.send_bool(win, "setTitlebarAppearsTransparent:", true);
        rt.send_long(win, "setTitleVisibility:", 1); // hidden
        rt.send_bool(win, "setMovableByWindowBackground:", true);
        rt.send_bool(win, "setOpaque:", false);
        rt.send_id(win, "setBackgroundColor:", rt.send(rt.class("NSColor")?, "clearColor"));
        rt.send_size(win, "setContentMinSize:", Size { w: 340.0, h: 420.0 });
        rt.send_long(win, "setLevel:", 3); // floating, like the menu bar's own windows
        rt.send_id(win, "setDelegate:", TARGET.load(Ordering::Acquire) as Id);
        if let Ok(appearance) = rt.class("NSAppearance") {
            let dark = rt.send_id(appearance, "appearanceNamed:", rt.string("NSAppearanceNameDarkAqua")?);
            if !dark.is_null() {
                rt.send_id(win, "setAppearance:", dark);
            }
        }
        // The glass: a visual effect view behind the page.
        let glass = rt.send(rt.class("NSVisualEffectView")?, "alloc");
        let f: unsafe extern "C" fn(Id, Sel, Rect) -> Id = std::mem::transmute(rt.msg);
        let glass = f(glass, rt.s("initWithFrame:"), frame);
        rt.send_long(glass, "setMaterial:", 13); // HUD window
        rt.send_long(glass, "setBlendingMode:", 0); // behind the window
        rt.send_long(glass, "setState:", 1); // always active
        rt.send_ulong(glass, "setAutoresizingMask:", 2 | 16);
        let web = web_view(rt, &format!("http://{}:8765/#glass", speaker()), frame)?;
        rt.send_id(glass, "addSubview:", web);
        rt.send_id(win, "setContentView:", glass);
        // macOS keeps the size between starts.
        rt.send_id(win, "setFrameAutosaveName:", rt.string("soundya.glass")?);
        GLASS.store(win as usize, Ordering::Release);
        GLASS_WEB.store(web as usize, Ordering::Release);
    } else {
        // Asleep while hidden: the status at once, not at the page's next turn.
        let web = GLASS_WEB.load(Ordering::Acquire) as Id;
        let js = rt.string("try { refresh(); } catch (e) {}")?;
        rt.send_id_id(web, "evaluateJavaScript:completionHandler:", js, std::ptr::null_mut());
    }
    place_under_icon(rt, win)?;
    let app = rt.send(rt.class("NSApplication")?, "sharedApplication");
    rt.send_bool(app, "activateIgnoringOtherApps:", true);
    rt.send_id(win, "makeKeyAndOrderFront:", std::ptr::null_mut());
    Ok(())
}

/// Right under the icon, inside the screen.
unsafe fn place_under_icon(rt: &Rt, win: Id) -> Result<(), String> {
    let item = STATUS_ITEM.load(Ordering::Acquire) as Id;
    let icon_win = rt.send(rt.send(item, "button"), "window");
    if icon_win.is_null() {
        rt.send(win, "center");
        return Ok(());
    }
    let icon = rt.rect_of(icon_win, "frame");
    let me = rt.rect_of(win, "frame");
    let screen = rt.send(icon_win, "screen");
    let area = if screen.is_null() { Rect { x: 0.0, y: 0.0, w: 1e5, h: 1e5 } } else { rt.rect_of(screen, "visibleFrame") };
    let x = (icon.x + icon.w / 2.0 - me.w / 2.0).clamp(area.x + 6.0, (area.x + area.w - me.w - 6.0).max(area.x));
    let y = (icon.y - me.h - 6.0).max(area.y);
    rt.send_size(win, "setFrameOrigin:", Size { w: x, h: y });
    Ok(())
}

/// «Размер → …»: one of SIZES, kept by macOS like a drag of the edges.
unsafe fn set_size(i: usize) {
    let Ok(rt) = Rt::load() else { return };
    if GLASS.load(Ordering::Acquire) == 0 && toggle_popover(&rt).is_err() {
        return;
    }
    let win = GLASS.load(Ordering::Acquire) as Id;
    let (w, h) = SIZES[i];
    let mut r = rt.rect_of(win, "frame");
    r.y += r.h - h;
    r.w = w;
    r.h = h;
    let f: unsafe extern "C" fn(Id, Sel, Rect, i8) = std::mem::transmute(rt.msg);
    f(win, rt.s("setFrame:display:"), r, 1);
    let _ = place_under_icon(&rt, win);
    rt.send_id(win, "makeKeyAndOrderFront:", std::ptr::null_mut());
}

extern "C" fn size_small(_this: Id, _sel: Sel, _sender: Id) {
    unsafe { set_size(0) }
}
extern "C" fn size_medium(_this: Id, _sel: Sel, _sender: Id) {
    unsafe { set_size(1) }
}
extern "C" fn size_large(_this: Id, _sel: Sel, _sender: Id) {
    unsafe { set_size(2) }
}

/// «Подключить этот Mac к колонке»: as the installer's «Только подключить этот компьютер»
/// (a Windows PC there stays first; the Mac joins as «мак»).
extern "C" fn connect_mac(_this: Id, _sel: Sel, _sender: Id) {
    use std::net::ToSocketAddrs;
    let ip = format!("{}:8765", speaker()).to_socket_addrs().ok().and_then(|mut a| a.next()).map(|a| a.ip().to_string());
    let text = match ip {
        Some(ip) => crate::pc_setup(&ip).unwrap_or_else(|e| format!("Не получилось: {e}")),
        None => "Колонка не нашлась в сети".to_owned(),
    };
    unsafe {
        let Ok(rt) = Rt::load() else { return };
        let Ok(alert) = rt.new("NSAlert") else { return };
        if let (Ok(t), Ok(m)) = (rt.string("soundya"), rt.string(&text)) {
            rt.send_id(alert, "setMessageText:", t);
            rt.send_id(alert, "setInformativeText:", m);
            let app = rt.send(rt.class("NSApplication").unwrap_or(std::ptr::null_mut()), "sharedApplication");
            rt.send_bool(app, "activateIgnoringOtherApps:", true);
            rt.send(alert, "runModal");
        }
    }
}

extern "C" fn open_log(_this: Id, _sel: Sel, _sender: Id) {
    let log = crate::agent::home().join("Library/Logs/soundya-pc.log");
    let _ = std::process::Command::new("/usr/bin/open").arg("-e").arg(log).status();
}

extern "C" fn open_folder(_this: Id, _sel: Sel, _sender: Id) {
    let _ = std::process::Command::new("/usr/bin/open").arg(crate::agent::dir(&crate::agent::home())).status();
}

/// «Данные для колонки»: this Mac's address and MAC, the key copied (for «Устройства»
/// by hand, as on Windows).
extern "C" fn show_setup(_this: Id, _sel: Sel, _sender: Id) {
    let home = crate::agent::home();
    let key = std::fs::read_to_string(crate::agent::dir(&home).join("config.json"))
        .ok()
        .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
        .and_then(|v| v["key"].as_str().map(str::to_owned))
        .unwrap_or_default();
    let me = std::net::UdpSocket::bind("0.0.0.0:0")
        .and_then(|s| s.connect((speaker().as_str(), 8765)).map(|()| s))
        .and_then(|s| s.local_addr())
        .map(|a| a.ip().to_string())
        .unwrap_or_default();
    let ifconfig = std::process::Command::new("/sbin/ifconfig").output().map(|o| String::from_utf8_lossy(&o.stdout).into_owned()).unwrap_or_default();
    let mac = crate::agent::mac_for(&ifconfig, &me).unwrap_or_default();
    if let Ok(mut c) = std::process::Command::new("/usr/bin/pbcopy").stdin(std::process::Stdio::piped()).spawn() {
        use std::io::Write;
        if let Some(i) = c.stdin.as_mut() {
            let _ = i.write_all(key.as_bytes());
        }
        let _ = c.wait();
    }
    unsafe {
        let Ok(rt) = Rt::load() else { return };
        let Ok(alert) = rt.new("NSAlert") else { return };
        if let (Ok(t), Ok(m)) = (
            rt.string("Данные этого Mac для колонки"),
            rt.string(&format!("Адрес: {me}\nMAC: {}\nКлюч скопирован — вставьте его в «Устройства → Компьютер».", if mac.is_empty() { "—" } else { &mac })),
        ) {
            rt.send_id(alert, "setMessageText:", t);
            rt.send_id(alert, "setInformativeText:", m);
            let app = rt.send(rt.class("NSApplication").unwrap_or(std::ptr::null_mut()), "sharedApplication");
            rt.send_bool(app, "activateIgnoringOtherApps:", true);
            rt.send(alert, "runModal");
        }
    }
}

/// A click on the icon: the panel at once; the right button (or ⌃-click) — the small menu.
extern "C" fn icon_clicked(this: Id, sel: Sel, sender: Id) {
    unsafe {
        let Ok(rt) = Rt::load() else { return };
        let app = rt.send(rt.class("NSApplication").unwrap_or(std::ptr::null_mut()), "sharedApplication");
        let event = rt.send(app, "currentEvent");
        let kind = if event.is_null() { 0 } else { rt.send_u64_ret(event, "type") };
        let ctrl = !event.is_null() && rt.send_u64_ret(event, "modifierFlags") & (1 << 18) != 0;
        // NSEventTypeRightMouseUp = 4
        if kind == 4 || ctrl {
            menu_needs_update(this, sel, std::ptr::null_mut());
            let (item, menu) = (STATUS_ITEM.load(Ordering::Acquire) as Id, STATUS_MENU.load(Ordering::Acquire) as Id);
            rt.send_id(item, "popUpStatusItemMenu:", menu);
            return;
        }
    }
    let _ = (this, sel, sender);
    unsafe {
        if let Ok(rt) = Rt::load() {
            if let Err(e) = toggle_popover(&rt) {
                eprintln!("soundya: панель под значком: {e}");
            }
        }
    }
}

extern "C" fn open_panel(_this: Id, _sel: Sel, _sender: Id) {
    unsafe {
        let Ok(rt) = Rt::load() else { return };
        let app = rt.send(rt.class("NSApplication").unwrap_or(std::ptr::null_mut()), "sharedApplication");
        let mut window = PANEL.load(Ordering::Acquire) as Id;
        if window.is_null() {
            // The panel's own address: the speakers before 0.5.8 answer «/?…» with «not found».
            match panel_window(&rt, &format!("http://{}:8765/", speaker()), "soundya") {
                Ok(w) => {
                    window = w;
                    PANEL.store(w as usize, Ordering::Release);
                }
                Err(e) => {
                    eprintln!("soundya: окно панели: {e}");
                    return;
                }
            }
        }
        rt.send_id(window, "makeKeyAndOrderFront:", std::ptr::null_mut());
        rt.send_bool(app, "activateIgnoringOtherApps:", true);
    }
}

/// The menu bar icon: what plays, play/pause, next, the panel in its own window.
/// Runs until «Выйти»; closing the panel's window keeps it.
pub fn tray(speaker_host: &str) -> Result<(), String> {
    *SPEAKER.lock().unwrap() = speaker_host.to_owned();
    unsafe {
        let rt = Rt::load()?;
        let app = rt.send(rt.class("NSApplication")?, "sharedApplication");
        rt.send_long(app, "setActivationPolicy:", 1); // accessory: no Dock icon
        main_menu(&rt, app, "soundya")?;

        let base = rt.class("NSObject")?;
        let mut k = rt.class("SoundyaTray").unwrap_or(std::ptr::null_mut());
        if k.is_null() {
            let name = CString::new("SoundyaTray").unwrap();
            k = (rt.alloc_class)(base, name.as_ptr(), 0);
            if k.is_null() {
                return Err("не создался класс меню".into());
            }
            let t = CString::new("v@:@").unwrap();
            (rt.add_method)(k, rt.s("openPanel:"), open_panel as *mut c_void, t.as_ptr());
            (rt.add_method)(k, rt.s("togglePlay:"), toggle_play as *mut c_void, t.as_ptr());
            (rt.add_method)(k, rt.s("nextTrack:"), next_track as *mut c_void, t.as_ptr());
            (rt.add_method)(k, rt.s("menuNeedsUpdate:"), menu_needs_update as *mut c_void, t.as_ptr());
            (rt.add_method)(k, rt.s("iconClicked:"), icon_clicked as *mut c_void, t.as_ptr());
            (rt.add_method)(k, rt.s("openLog:"), open_log as *mut c_void, t.as_ptr());
            (rt.add_method)(k, rt.s("openFolder:"), open_folder as *mut c_void, t.as_ptr());
            (rt.add_method)(k, rt.s("showSetup:"), show_setup as *mut c_void, t.as_ptr());
            (rt.add_method)(k, rt.s("connectMac:"), connect_mac as *mut c_void, t.as_ptr());
            (rt.add_method)(k, rt.s("sizeSmall:"), size_small as *mut c_void, t.as_ptr());
            (rt.add_method)(k, rt.s("windowDidResignKey:"), window_resigned as *mut c_void, t.as_ptr());
            (rt.add_method)(k, rt.s("sizeMedium:"), size_medium as *mut c_void, t.as_ptr());
            (rt.add_method)(k, rt.s("sizeLarge:"), size_large as *mut c_void, t.as_ptr());
            (rt.register)(k);
        }
        let target = rt.send(rt.send(k, "alloc"), "init");
        TARGET.store(target as usize, Ordering::Release);

        let bar = rt.send(rt.class("NSStatusBar")?, "systemStatusBar");
        let item = rt.send_f64(bar, "statusItemWithLength:", -1.0); // NSVariableStatusItemLength
        rt.send(item, "retain");
        let button = rt.send(item, "button");
        // soundya's own icon (the pill with the XX eyes), one colour: macOS tints it for
        // the light and the dark menu bar.
        let icon = rt.send_id(rt.send(rt.class("NSImage")?, "alloc"), "initWithData:", rt.data(include_bytes!("menubar.png"))?);
        if icon.is_null() {
            rt.send_id(button, "setTitle:", rt.string("♪")?);
        } else {
            rt.send_size(icon, "setSize:", Size { w: 22.0, h: 18.0 });
            rt.send_bool(icon, "setTemplate:", true);
            rt.send_id(button, "setImage:", icon);
        }

        let menu = rt.new("NSMenu")?;
        rt.send_bool(menu, "setAutoenablesItems:", false);
        let now = rt.item("…", "", "")?;
        rt.send_bool(now, "setEnabled:", false);
        NOW_ITEM.store(now as usize, Ordering::Release);
        rt.send_id(menu, "addItem:", now);
        rt.send_id(menu, "addItem:", rt.send(rt.class("NSMenuItem")?, "separatorItem"));
        let play = rt.item("Играть", "togglePlay:", "")?;
        PLAY_ITEM.store(play as usize, Ordering::Release);
        let next = rt.item("Следующий трек", "nextTrack:", "")?;
        let open = rt.item("Открыть в большом окне", "openPanel:", "o")?;
        for i in [play, next] {
            rt.send_id(i, "setTarget:", target);
            rt.send_id(menu, "addItem:", i);
        }
        rt.send_id(menu, "addItem:", rt.send(rt.class("NSMenuItem")?, "separatorItem"));
        rt.send_id(open, "setTarget:", target);
        rt.send_id(menu, "addItem:", open);
        // «Размер»: three steps; the window's edges resize it freely too.
        let size_item = rt.item("Размер окна", "", "")?;
        let sizes = rt.new("NSMenu")?;
        for (t, a) in [("Маленький", "sizeSmall:"), ("Средний", "sizeMedium:"), ("Большой", "sizeLarge:")] {
            let i = rt.item(t, a, "")?;
            rt.send_id(i, "setTarget:", target);
            rt.send_id(sizes, "addItem:", i);
        }
        rt.send_id(size_item, "setSubmenu:", sizes);
        rt.send_id(menu, "addItem:", size_item);
        // As on Windows: the data for the speaker, the log, the settings folder.
        for (t, a) in [("Подключить этот Mac к колонке", "connectMac:"), ("Данные для колонки (адрес, ключ, MAC)…", "showSetup:"), ("Открыть лог", "openLog:"), ("Открыть папку настроек", "openFolder:")] {
            let i = rt.item(t, a, "")?;
            rt.send_id(i, "setTarget:", target);
            rt.send_id(menu, "addItem:", i);
        }
        rt.send_id(menu, "addItem:", rt.send(rt.class("NSMenuItem")?, "separatorItem"));
        rt.send_id(menu, "addItem:", rt.item("Выйти", "terminate:", "q")?);
        rt.send_id(menu, "setDelegate:", target);
        // No menu on the icon itself: a click opens the panel (iconClicked:).
        STATUS_ITEM.store(item as usize, Ordering::Release);
        STATUS_MENU.store(menu as usize, Ordering::Release);
        rt.send_id(button, "setTarget:", target);
        rt.send_sel(button, "setAction:", rt.s("iconClicked:"));
        rt.send_ulong(button, "sendActionOn:", (1 << 2) | (1 << 4)); // left and right mouse up
        rt.send(app, "run");
    }
    Ok(())
}

impl Rt {
    unsafe fn send_bool_number(&self, b: bool) -> Result<Id, String> {
        let f: unsafe extern "C" fn(Id, Sel, i8) -> Id = std::mem::transmute(self.msg);
        Ok(f(self.class("NSNumber")?, self.s("numberWithBool:"), b as i8))
    }
}
