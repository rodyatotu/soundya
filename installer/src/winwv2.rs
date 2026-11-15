//! WebView2 (the Edge engine built into Windows 10/11) in soundya's own windows, through
//! its COM interface by hand: no crates. The loader DLL (Microsoft's, signed, BSD
//! licence) is carried inside soundya.exe and written next to it in %LOCALAPPDATA%\soundya.
//!
//! One environment (one set of msedgewebview2 processes) serves every window: the panel,
//! the to-do list, the installer. Everything here runs on the UI thread.

use std::{
    cell::RefCell,
    ffi::c_void,
    ptr::null_mut,
    sync::atomic::{AtomicU32, Ordering},
};

use crate::winffi::*;

static LOADER: &[u8] = include_bytes!("winwebview2loader.dll");

/// The browser engine's switches: music may start without a click (the next track after
/// the previous one, a track chosen on the speaker), and a hidden window keeps playing
/// at full speed (MediaSource keeps getting its data).
const BROWSER_ARGS: &str = "--autoplay-policy=no-user-gesture-required --disable-background-timer-throttling \
    --disable-renderer-backgrounding --disable-backgrounding-occluded-windows";

type Callback = Box<dyn FnMut(usize, *mut c_void) -> HRESULT>;

/// A COM object with one method (Invoke): completion handlers get (HRESULT, result), event
/// handlers get (sender, args); both fit (usize, pointer) in the x64 calling convention.
#[repr(C)]
struct Handler {
    vtbl: *const HandlerVtbl,
    refs: AtomicU32,
    cb: Callback,
}

#[repr(C)]
struct HandlerVtbl {
    qi: unsafe extern "system" fn(*mut Handler, *const GUID, *mut *mut c_void) -> HRESULT,
    add_ref: unsafe extern "system" fn(*mut Handler) -> u32,
    release: unsafe extern "system" fn(*mut Handler) -> u32,
    invoke: unsafe extern "system" fn(*mut Handler, usize, *mut c_void) -> HRESULT,
}

unsafe extern "system" fn h_qi(this: *mut Handler, _iid: *const GUID, out: *mut *mut c_void) -> HRESULT {
    // The engine asks only for the handler's own interface (or IUnknown).
    *out = this as *mut c_void;
    h_add_ref(this);
    0
}
unsafe extern "system" fn h_add_ref(this: *mut Handler) -> u32 {
    (*this).refs.fetch_add(1, Ordering::AcqRel) + 1
}
unsafe extern "system" fn h_release(this: *mut Handler) -> u32 {
    let left = (*this).refs.fetch_sub(1, Ordering::AcqRel) - 1;
    if left == 0 {
        drop(Box::from_raw(this));
    }
    left
}
unsafe extern "system" fn h_invoke(this: *mut Handler, a: usize, b: *mut c_void) -> HRESULT {
    ((*this).cb)(a, b)
}

static VTBL: HandlerVtbl = HandlerVtbl { qi: h_qi, add_ref: h_add_ref, release: h_release, invoke: h_invoke };

/// A new handler with one reference, which the caller gives away by releasing it after the call.
fn handler(cb: impl FnMut(usize, *mut c_void) -> HRESULT + 'static) -> *mut c_void {
    Box::into_raw(Box::new(Handler { vtbl: &VTBL, refs: AtomicU32::new(1), cb: Box::new(cb) })) as *mut c_void
}

/// Calls `f(handler)` and drops this side's reference.
unsafe fn with_handler(cb: impl FnMut(usize, *mut c_void) -> HRESULT + 'static, f: impl FnOnce(*mut c_void) -> HRESULT) -> HRESULT {
    let h = handler(cb);
    let r = f(h);
    h_release(h as *mut Handler);
    r
}

// ------------------------------------------------------------------ the environment

enum Env {
    None,
    Pending(Vec<Box<dyn FnOnce(Result<*mut c_void, String>)>>),
    Ready(*mut c_void),
    Failed(String),
}

thread_local! {
    static ENV: RefCell<Env> = const { RefCell::new(Env::None) };
}

/// Where soundya keeps its own files: %LOCALAPPDATA%\soundya.
pub fn app_dir() -> std::path::PathBuf {
    let d = std::env::var("LOCALAPPDATA").map(std::path::PathBuf::from).unwrap_or_else(|_| std::env::temp_dir()).join("soundya");
    let _ = std::fs::create_dir_all(&d);
    d
}

/// Is the WebView2 runtime there at all? (Windows 11 has it; Windows 10 nearly always.)
pub fn runtime_version() -> Option<String> {
    let id = "{F3017226-FE2A-4295-8BDF-00C3A9A7E4C5}";
    [
        (HKEY_LOCAL_MACHINE, format!("SOFTWARE\\WOW6432Node\\Microsoft\\EdgeUpdate\\Clients\\{id}")),
        (HKEY_LOCAL_MACHINE, format!("SOFTWARE\\Microsoft\\EdgeUpdate\\Clients\\{id}")),
        (HKEY_CURRENT_USER, format!("Software\\Microsoft\\EdgeUpdate\\Clients\\{id}")),
    ]
    .into_iter()
    .find_map(|(root, key)| crate::winsys::reg_string(root, &key, "pv"))
    .filter(|v| v != "0.0.0.0")
}

type CreateEnv = unsafe extern "system" fn(*const u16, *const u16, *mut c_void, *mut c_void) -> HRESULT;

fn load_loader() -> Result<CreateEnv, String> {
    let path = app_dir().join("WebView2Loader.dll");
    // Written once (and again when soundya brings a newer one).
    if std::fs::metadata(&path).map(|m| m.len() as usize).ok() != Some(LOADER.len()) {
        let tmp = path.with_extension("dll.tmp");
        std::fs::write(&tmp, LOADER).map_err(|e| format!("не записался WebView2Loader.dll: {e}"))?;
        let _ = std::fs::rename(&tmp, &path);
    }
    unsafe {
        let m = LoadLibraryW(wide(&path.to_string_lossy()).as_ptr());
        if m.is_null() {
            return Err(format!("WebView2Loader.dll не загрузился ({})", GetLastError()));
        }
        let f = GetProcAddress(m, b"CreateCoreWebView2EnvironmentWithOptions\0".as_ptr());
        if f.is_null() {
            return Err("в WebView2Loader.dll нет нужной функции".into());
        }
        Ok(std::mem::transmute::<*mut c_void, CreateEnv>(f))
    }
}

/// Runs `then` with the environment (made on the first call; later calls wait for it).
pub fn with_env(then: impl FnOnce(Result<*mut c_void, String>) + 'static) {
    let start = ENV.with(|e| {
        let mut e = e.borrow_mut();
        match &mut *e {
            Env::Ready(p) => {
                let p = *p;
                drop(e);
                then(Ok(p));
                false
            }
            Env::Failed(err) => {
                let err = err.clone();
                drop(e);
                then(Err(err));
                false
            }
            Env::Pending(list) => {
                list.push(Box::new(then));
                false
            }
            Env::None => {
                *e = Env::Pending(vec![Box::new(then)]);
                true
            }
        }
    });
    if !start {
        return;
    }
    let finish = |r: Result<*mut c_void, String>| {
        let waiting = ENV.with(|e| {
            let old = std::mem::replace(&mut *e.borrow_mut(), match &r {
                Ok(p) => Env::Ready(*p),
                Err(s) => Env::Failed(s.clone()),
            });
            match old {
                Env::Pending(list) => list,
                _ => Vec::new(),
            }
        });
        for w in waiting {
            w(r.clone());
        }
    };
    if runtime_version().is_none() {
        return finish(Err("на компьютере нет WebView2 Runtime".into()));
    }
    let create = match load_loader() {
        Ok(f) => f,
        Err(e) => return finish(Err(e)),
    };
    let (name, value) = (wide("WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS"), wide(BROWSER_ARGS));
    unsafe { SetEnvironmentVariableW(name.as_ptr(), value.as_ptr()) };
    let data = wide(&app_dir().join("webview").to_string_lossy());
    let hr = unsafe {
        with_handler(
            move |hr, env| {
                let hr = hr as u32 as i32;
                if hr < 0 || env.is_null() {
                    finish(Err(format!("WebView2 не запустился (0x{:08X})", hr as u32)));
                } else {
                    // Kept for the life of the program.
                    let add_ref: unsafe extern "system" fn(*mut c_void) -> u32 = std::mem::transmute(vt(env, 1));
                    add_ref(env);
                    finish(Ok(env));
                }
                0
            },
            |h| create(std::ptr::null(), data.as_ptr(), null_mut(), h),
        )
    };
    if hr < 0 {
        // The handler is not called then.
        let waiting = ENV.with(|e| std::mem::replace(&mut *e.borrow_mut(), Env::Failed(format!("WebView2 не запустился (0x{:08X})", hr as u32))));
        if let Env::Pending(list) = waiting {
            for w in list {
                w(Err(format!("WebView2 не запустился (0x{:08X})", hr as u32)));
            }
        }
    }
}

// ------------------------------------------------------------------ a view in a window

/// What a view tells its window.
pub enum Event {
    /// window.chrome.webview.postMessage("…") from the page.
    Message(String),
    /// The page failed to load (the speaker is off, say).
    LoadFailed,
    /// The engine's process died: the window makes a new view.
    Crashed,
}

pub struct View {
    controller: *mut c_void,
    webview: *mut c_void,
}

#[repr(C)]
struct Color {
    a: u8,
    r: u8,
    g: u8,
    b: u8,
}

impl View {
    /// A view filling `hwnd`, showing `url`. `on` gets the view's events; `script` runs in
    /// every page before its own scripts. `background`: 0xRRGGBB behind transparent pages.
    pub fn create(
        hwnd: HWND,
        url: String,
        script: Option<String>,
        background: Option<u32>,
        on: impl Fn(HWND, Event) + 'static,
        done: impl FnOnce(Result<View, String>) + 'static,
    ) {
        let on = std::rc::Rc::new(on);
        with_env(move |env| {
            let env = match env {
                Ok(e) => e,
                Err(e) => return done(Err(e)),
            };
            let mut done = Some(done);
            let hr = unsafe {
                let create: unsafe extern "system" fn(*mut c_void, HWND, *mut c_void) -> HRESULT = std::mem::transmute(vt(env, 3));
                with_handler(
                    move |hr, controller| {
                        let hr = hr as u32 as i32;
                        let Some(done) = done.take() else { return 0 };
                        if hr < 0 || controller.is_null() {
                            done(Err(format!("окно WebView2 не создалось (0x{:08X})", hr as u32)));
                            return 0;
                        }
                        done(View::setup(hwnd, controller, &url, script.as_deref(), background, on.clone()));
                        0
                    },
                    |h| create(env, hwnd, h),
                )
            };
            let _ = hr;
        });
    }

    unsafe fn setup(hwnd: HWND, controller: *mut c_void, url: &str, script: Option<&str>, background: Option<u32>, on: std::rc::Rc<dyn Fn(HWND, Event)>) -> Result<View, String> {
        let add_ref: unsafe extern "system" fn(*mut c_void) -> u32 = std::mem::transmute(vt(controller, 1));
        add_ref(controller);
        let mut webview: *mut c_void = null_mut();
        let get: unsafe extern "system" fn(*mut c_void, *mut *mut c_void) -> HRESULT = std::mem::transmute(vt(controller, 25));
        if get(controller, &mut webview) < 0 || webview.is_null() {
            release(controller);
            return Err("WebView2 не отдал страницу".into());
        }
        let view = View { controller, webview };
        // Transparent pages (the to-do card) show the window's colour, not white.
        if let Some(rgb) = background {
            let iid = GUID::parse("{C979903E-D4CA-4228-92EB-47EE3FA96EAB}");
            let qi: unsafe extern "system" fn(*mut c_void, *const GUID, *mut *mut c_void) -> HRESULT = std::mem::transmute(vt(controller, 0));
            let mut c2: *mut c_void = null_mut();
            if qi(controller, &iid, &mut c2) >= 0 && !c2.is_null() {
                let put: unsafe extern "system" fn(*mut c_void, Color) -> HRESULT = std::mem::transmute(vt(c2, 27));
                put(c2, Color { a: 255, r: (rgb >> 16) as u8, g: (rgb >> 8) as u8, b: rgb as u8 });
                release(c2);
            }
        }
        // Settings: no status bar, no developer tools, no browser zoom by Ctrl+wheel.
        let mut settings: *mut c_void = null_mut();
        let get_settings: unsafe extern "system" fn(*mut c_void, *mut *mut c_void) -> HRESULT = std::mem::transmute(vt(webview, 3));
        if get_settings(webview, &mut settings) >= 0 && !settings.is_null() {
            let put = |i: usize, v: bool| {
                let f: unsafe extern "system" fn(*mut c_void, BOOL) -> HRESULT = std::mem::transmute(vt(settings, i));
                f(settings, v as BOOL);
            };
            put(10, false); // status bar
            put(12, cfg!(feature = "dev")); // developer tools
            put(18, false); // zoom control
            release(settings);
        }
        let mut token = 0i64;
        let ev = |i: usize, cb: Box<dyn FnMut(usize, *mut c_void) -> HRESULT>, token: &mut i64| {
            let add: unsafe extern "system" fn(*mut c_void, *mut c_void, *mut i64) -> HRESULT = std::mem::transmute(vt(webview, i));
            let mut cb = cb;
            with_handler(move |a, b| cb(a, b), |h| add(webview, h, token));
        };
        // Links that open a new window: in the user's browser.
        ev(
            44,
            Box::new(|_, args| {
                if args.is_null() {
                    return 0;
                }
                let get_uri: unsafe extern "system" fn(*mut c_void, *mut *mut u16) -> HRESULT = std::mem::transmute(vt(args, 3));
                let put_handled: unsafe extern "system" fn(*mut c_void, BOOL) -> HRESULT = std::mem::transmute(vt(args, 6));
                let mut p: *mut u16 = null_mut();
                if get_uri(args, &mut p) >= 0 {
                    let uri = from_wide_ptr(p);
                    CoTaskMemFree(p as *mut c_void);
                    if uri.starts_with("http://") || uri.starts_with("https://") {
                        let _ = crate::winsys::shell_open(&uri);
                    }
                }
                put_handled(args, 1);
                0
            }),
            &mut token,
        );
        // Messages from the page (drag the window, …).
        let on1 = on.clone();
        ev(
            34,
            Box::new(move |_, args| {
                if args.is_null() {
                    return 0;
                }
                let try_get: unsafe extern "system" fn(*mut c_void, *mut *mut u16) -> HRESULT = std::mem::transmute(vt(args, 5));
                let mut p: *mut u16 = null_mut();
                if try_get(args, &mut p) >= 0 {
                    let s = from_wide_ptr(p);
                    CoTaskMemFree(p as *mut c_void);
                    on1(hwnd, Event::Message(s));
                }
                0
            }),
            &mut token,
        );
        // A page that did not load: the window says so and offers to try again.
        let on2 = on.clone();
        ev(
            15,
            Box::new(move |_, args| {
                if args.is_null() {
                    return 0;
                }
                let is_success: unsafe extern "system" fn(*mut c_void, *mut BOOL) -> HRESULT = std::mem::transmute(vt(args, 3));
                let get_status: unsafe extern "system" fn(*mut c_void, *mut i32) -> HRESULT = std::mem::transmute(vt(args, 4));
                let (mut ok, mut status) = (1, 0);
                is_success(args, &mut ok);
                get_status(args, &mut status);
                // COREWEBVIEW2_WEB_ERROR_STATUS_OPERATION_CANCELED (14): a navigation replaced by another.
                if ok == 0 && status != 14 {
                    on2(hwnd, Event::LoadFailed);
                }
                0
            }),
            &mut token,
        );
        let on3 = on.clone();
        ev(
            25,
            Box::new(move |_, args| {
                if args.is_null() {
                    return 0;
                }
                let kind: unsafe extern "system" fn(*mut c_void, *mut i32) -> HRESULT = std::mem::transmute(vt(args, 3));
                let mut k = 0;
                kind(args, &mut k);
                // 0: the browser process, 1: the page's renderer, 2: the page stopped responding.
                if k == 0 || k == 1 || k == 2 {
                    on3(hwnd, Event::Crashed);
                }
                0
            }),
            &mut token,
        );
        if let Some(s) = script {
            let add: unsafe extern "system" fn(*mut c_void, *const u16, *mut c_void) -> HRESULT = std::mem::transmute(vt(webview, 27));
            add(webview, wide(s).as_ptr(), null_mut());
        }
        view.fit(hwnd);
        view.navigate(url);
        Ok(view)
    }

    pub fn navigate(&self, url: &str) {
        unsafe {
            let f: unsafe extern "system" fn(*mut c_void, *const u16) -> HRESULT = std::mem::transmute(vt(self.webview, 5));
            f(self.webview, wide(url).as_ptr());
        }
    }

    pub fn navigate_to_string(&self, html: &str) {
        unsafe {
            let f: unsafe extern "system" fn(*mut c_void, *const u16) -> HRESULT = std::mem::transmute(vt(self.webview, 6));
            f(self.webview, wide(html).as_ptr());
        }
    }

    pub fn execute(&self, js: &str) {
        unsafe {
            let f: unsafe extern "system" fn(*mut c_void, *const u16, *mut c_void) -> HRESULT = std::mem::transmute(vt(self.webview, 29));
            f(self.webview, wide(js).as_ptr(), null_mut());
        }
    }

    /// The view fills the window's client area.
    pub fn fit(&self, hwnd: HWND) {
        unsafe {
            let mut r = RECT::default();
            GetClientRect(hwnd, &mut r);
            let f: unsafe extern "system" fn(*mut c_void, RECT) -> HRESULT = std::mem::transmute(vt(self.controller, 6));
            f(self.controller, r);
        }
    }

    /// Hidden: the engine may save memory and CPU (music keeps playing).
    pub fn set_visible(&self, on: bool) {
        unsafe {
            let f: unsafe extern "system" fn(*mut c_void, BOOL) -> HRESULT = std::mem::transmute(vt(self.controller, 4));
            f(self.controller, on as BOOL);
        }
    }

    /// The window moved: popups and the like follow it.
    pub fn moved(&self) {
        unsafe {
            let f: unsafe extern "system" fn(*mut c_void) -> HRESULT = std::mem::transmute(vt(self.controller, 23));
            f(self.controller);
        }
    }

    pub fn focus(&self) {
        unsafe {
            let f: unsafe extern "system" fn(*mut c_void, i32) -> HRESULT = std::mem::transmute(vt(self.controller, 12));
            f(self.controller, 0); // COREWEBVIEW2_MOVE_FOCUS_REASON_PROGRAMMATIC
        }
    }

    pub fn close(&self) {
        unsafe {
            let f: unsafe extern "system" fn(*mut c_void) -> HRESULT = std::mem::transmute(vt(self.controller, 24));
            f(self.controller);
            release(self.webview);
            release(self.controller);
        }
    }
}

/// The page shown when the speaker's page does not load.
pub fn offline_page(url: &str, text: &str) -> String {
    let esc = |s: &str| s.replace('&', "&amp;").replace('<', "&lt;").replace('"', "&quot;");
    format!(
        "<!doctype html><meta charset=utf-8><body style=\"margin:0;height:100vh;display:flex;align-items:center;justify-content:center;\
         background:#14121c;color:#f2eff8;font:14px 'Segoe UI',system-ui,sans-serif;text-align:center\"><div><div style=\"font-size:34px\">🔇</div>\
         <p style=\"max-width:300px;line-height:1.45\">{}</p><button onclick=\"location.href='{}'\" style=\"font:inherit;padding:8px 18px;\
         border-radius:10px;border:0;background:#ff5a1f;color:#fff;cursor:pointer\">Попробовать ещё раз</button></div>\
         <script>setTimeout(()=>location.href='{}',15000)</script>",
        esc(text),
        esc(url),
        esc(url)
    )
}
