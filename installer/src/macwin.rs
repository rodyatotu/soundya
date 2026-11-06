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

        // Menus: «soundya» (hide, quit) and «Правка» (paste is needed for the Google address).
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

        // The window: dark, the title bar in the page's colour, draggable as usual.
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

        let config = rt.new("WKWebViewConfiguration")?;
        let web = rt.init_webview(rt.send(rt.class("WKWebView")?, "alloc"), frame, config);
        if web.is_null() {
            return Err("WebKit не создал окно страницы".into());
        }
        rt.send_ulong(web, "setAutoresizingMask:", 2 | 16); // width | height sizable
        // Transparent until the page paints: no white flash.
        let no = rt.send_bool_number(false)?;
        rt.send_id_id(web, "setValue:forKey:", no, rt.string("drawsBackground")?);
        rt.send_id(window, "setContentView:", web);
        let nsurl = rt.send_id(rt.class("NSURL")?, "URLWithString:", rt.string(url)?);
        let req = rt.send_id(rt.class("NSURLRequest")?, "requestWithURL:", nsurl);
        rt.send_id(web, "loadRequest:", req);

        rt.send(window, "center");
        rt.send_id(window, "makeKeyAndOrderFront:", std::ptr::null_mut());
        rt.send_bool(app, "activateIgnoringOtherApps:", true);
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
