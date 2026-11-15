//! The real `Sys` for the Windows agent: Windows API calls only, no PowerShell, no
//! console windows. Everything here runs on demand (a command from the speaker); the
//! only thing that waits in the background is the microphone watch, which sleeps on a
//! registry change notification (no polling).

use std::{
    collections::BTreeSet,
    ffi::c_void,
    path::PathBuf,
    ptr::null_mut,
    sync::Mutex,
    time::{Duration, Instant},
};

use serde_json::{json, Map, Value};

use crate::winagent::{mic_app_name, Button, Item, Power, Proc, Sys};
use crate::winffi::*;

const CREATE_NO_WINDOW: u32 = 0x0800_0000;

pub struct WinSys {
    dry_run: bool,
    cpu_last: Mutex<Option<(Instant, u64, u64)>>,
    gpu: Mutex<Option<Pdh>>,
    slow: Mutex<Option<(Instant, Map<String, Value>)>>,
    mic: Mutex<Option<MicWatch>>,
    start_apps: Mutex<Option<(Instant, Vec<Item>)>>,
}

// The handles inside are used under their mutexes only.
unsafe impl Send for WinSys {}
unsafe impl Sync for WinSys {}

impl WinSys {
    pub fn new(dry_run: bool) -> WinSys {
        WinSys { dry_run, cpu_last: Mutex::new(None), gpu: Mutex::new(None), slow: Mutex::new(None), mic: Mutex::new(None), start_apps: Mutex::new(None) }
    }
}

/// Clicks and /mouse in real screen pixels (the same ones both ways), not scaled ones.
pub fn dpi_aware() {
    unsafe {
        // DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2 (Windows 10 1703+, looked up: older
        // Windows 10 starts too), else the old call.
        let user32 = GetModuleHandleW(wide("user32.dll").as_ptr());
        let f = if user32.is_null() { null_mut() } else { GetProcAddress(user32, b"SetProcessDpiAwarenessContext\0".as_ptr()) };
        let done = !f.is_null() && {
            let set: unsafe extern "system" fn(isize) -> BOOL = std::mem::transmute(f);
            set(-4) != 0
        };
        if !done {
            SetProcessDPIAware();
        }
    }
}

fn hkey(hive: &str) -> HKEY {
    if hive.eq_ignore_ascii_case("HKLM") { HKEY_LOCAL_MACHINE } else { HKEY_CURRENT_USER }
}

pub fn reg_string(root: HKEY, key: &str, value: &str) -> Option<String> {
    let (k, v) = (wide(key), wide(value));
    let mut buf = vec![0u16; 1024];
    let mut size = (buf.len() * 2) as u32;
    // RRF_RT_REG_SZ | RRF_RT_REG_EXPAND_SZ | RRF_NOEXPAND
    let flags = 0x2 | 0x4 | 0x1000_0000;
    let r = unsafe { RegGetValueW(root, k.as_ptr(), if value.is_empty() { null_mut::<u16>() } else { v.as_ptr() as *mut u16 }, flags, null_mut(), buf.as_mut_ptr() as *mut c_void, &mut size) };
    (r == 0).then(|| crate::winagent::expand_env(&from_wide(&buf))).filter(|s| !s.is_empty())
}

fn known_folder(id: &str) -> Option<PathBuf> {
    let g = GUID::parse(id);
    let mut p: *mut u16 = null_mut();
    unsafe {
        let hr = SHGetKnownFolderPath(&g, 0, null_mut(), &mut p);
        let s = from_wide_ptr(p);
        CoTaskMemFree(p as *mut c_void);
        (hr >= 0 && !s.is_empty()).then(|| PathBuf::from(s))
    }
}

pub fn desktop() -> PathBuf {
    known_folder("{B4BFCC3A-DB2C-424C-B029-7FE99A87C641}").unwrap_or_else(|| PathBuf::from(std::env::var("USERPROFILE").unwrap_or_default()).join("Desktop"))
}

fn documents() -> PathBuf {
    known_folder("{FDD39AD0-238F-46AF-ADB4-6C85480369C7}").unwrap_or_else(|| PathBuf::from(std::env::var("USERPROFILE").unwrap_or_default()).join("Documents"))
}

/// Visible top-level windows (not owned by another window) of every process: (window, pid).
fn top_windows() -> Vec<(HWND, u32)> {
    unsafe extern "system" fn each(h: HWND, l: LPARAM) -> BOOL {
        let out = &mut *(l as *mut Vec<(HWND, u32)>);
        if IsWindowVisible(h) != 0 && GetWindow(h, 4).is_null() {
            let mut pid = 0;
            GetWindowThreadProcessId(h, &mut pid);
            out.push((h, pid));
        }
        1
    }
    let mut out: Vec<(HWND, u32)> = Vec::new();
    unsafe { EnumWindows(each, &mut out as *mut _ as LPARAM) };
    out
}

pub fn process_path(pid: u32) -> Option<String> {
    unsafe {
        let h = OpenProcess(0x1000, 0, pid); // PROCESS_QUERY_LIMITED_INFORMATION
        if h.is_null() {
            return None;
        }
        let mut buf = [0u16; 1024];
        let mut n = buf.len() as u32;
        let ok = QueryFullProcessImageNameW(h, 0, buf.as_mut_ptr(), &mut n) != 0;
        CloseHandle(h);
        ok.then(|| String::from_utf16_lossy(&buf[..n as usize]))
    }
}

/// Every process: (pid, exe file name with «.exe»).
pub fn process_list() -> Vec<(u32, String)> {
    let mut out = Vec::new();
    unsafe {
        let snap = CreateToolhelp32Snapshot(0x2, 0); // TH32CS_SNAPPROCESS
        if snap.is_null() || snap as isize == -1 {
            return out;
        }
        let mut pe: PROCESSENTRY32W = std::mem::zeroed();
        pe.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
        if Process32FirstW(snap, &mut pe) != 0 {
            loop {
                out.push((pe.th32ProcessID, from_wide(&pe.szExeFile)));
                if Process32NextW(snap, &mut pe) == 0 {
                    break;
                }
            }
        }
        CloseHandle(snap);
    }
    out
}

pub fn alive(pid: u32) -> bool {
    unsafe {
        let h = OpenProcess(0x1000, 0, pid);
        if h.is_null() {
            return false;
        }
        let mut code = 0;
        let ok = GetExitCodeProcess(h, &mut code) != 0 && code == 259; // STILL_ACTIVE
        CloseHandle(h);
        ok
    }
}

pub fn kill(pid: u32) {
    unsafe {
        let h = OpenProcess(0x0001 | 0x1000, 0, pid); // PROCESS_TERMINATE
        if !h.is_null() {
            TerminateProcess(h, 1);
            CloseHandle(h);
        }
    }
}

/// Windows lets a background program raise a window only right after a key press: Alt.
fn tap_alt() {
    let i = [INPUT::key(0x12, 0, 0), INPUT::key(0x12, 0, 2)];
    unsafe { SendInput(2, i.as_ptr(), std::mem::size_of::<INPUT>() as i32) };
}

fn send(inputs: &[INPUT]) -> bool {
    unsafe { SendInput(inputs.len() as u32, inputs.as_ptr(), std::mem::size_of::<INPUT>() as i32) as usize == inputs.len() }
}

/// Keys that need KEYEVENTF_EXTENDEDKEY: arrows, Home/End, PgUp/PgDn, Insert/Delete, Win.
fn extended(vk: u16) -> bool {
    matches!(vk, 0x21..=0x28 | 0x2D | 0x2E | 0x5B | 0x5C)
}

pub fn spawn(exe: &str, args: &[String]) -> Result<(), String> {
    use std::os::windows::process::CommandExt;
    std::process::Command::new(exe)
        .args(args)
        .creation_flags(CREATE_NO_WINDOW)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map(drop)
        .map_err(|e| e.to_string())
}

pub fn shell_open(target: &str) -> Result<(), String> {
    let (op, f) = (wide("open"), wide(target));
    let r = unsafe { ShellExecuteW(null_mut(), op.as_ptr(), f.as_ptr(), std::ptr::null(), std::ptr::null(), 1) } as isize;
    if r > 32 { Ok(()) } else { Err(format!("ShellExecute: {r}")) }
}

impl Sys for WinSys {
    fn shortcut_dirs(&self) -> (Vec<PathBuf>, Vec<PathBuf>) {
        let env = |k: &str| std::env::var(k).map(PathBuf::from).ok();
        let deep = [env("ProgramData"), env("APPDATA")].into_iter().flatten().map(|p| p.join("Microsoft\\Windows\\Start Menu\\Programs")).collect();
        let flat = [Some(desktop()), env("PUBLIC").map(|p| p.join("Desktop"))].into_iter().flatten().collect();
        (deep, flat)
    }

    fn reg_str(&self, hive: &str, key: &str, value: &str) -> Option<String> {
        reg_string(hkey(hive), key, value)
    }

    /// Store apps: shell:AppsFolder, read through the shell (as Get-StartApps), kept 5 minutes.
    fn start_apps(&self) -> Vec<Item> {
        let mut cache = self.start_apps.lock().unwrap();
        if let Some((t, list)) = cache.as_ref() {
            if t.elapsed() < Duration::from_secs(300) {
                return list.clone();
            }
        }
        let list = unsafe { apps_folder() };
        *cache = Some((Instant::now(), list.clone()));
        list
    }

    fn processes(&self) -> Vec<Proc> {
        let windows: BTreeSet<u32> = top_windows().into_iter().map(|(_, p)| p).collect();
        process_list()
            .into_iter()
            .filter(|(pid, _)| *pid > 4)
            .map(|(pid, exe)| {
                let name = exe.strip_suffix(".exe").or_else(|| exe.strip_suffix(".EXE")).unwrap_or(&exe).to_owned();
                Proc { pid, name, path: process_path(pid), has_window: windows.contains(&pid) }
            })
            .collect()
    }

    fn focus(&self, pids: &[u32]) -> bool {
        let Some((h, _)) = top_windows().into_iter().find(|(_, p)| pids.contains(p)) else { return false };
        if self.dry_run {
            return true;
        }
        unsafe {
            if IsIconic(h) != 0 {
                ShowWindow(h, 9); // SW_RESTORE
            }
            tap_alt();
            SetForegroundWindow(h) != 0
        }
    }

    fn close(&self, pids: &[u32]) {
        if self.dry_run {
            return;
        }
        for (h, _) in top_windows().into_iter().filter(|(_, p)| pids.contains(p)) {
            unsafe { PostMessageW(h, 0x0010, 0, 0) }; // WM_CLOSE: the app may ask to save
        }
        std::thread::sleep(Duration::from_secs(2));
        for pid in pids {
            if alive(*pid) {
                kill(*pid);
            }
        }
    }

    fn open(&self, target: &str) -> Result<(), String> {
        if self.dry_run {
            return Ok(());
        }
        // Paths, shortcuts and links (C:\…, steam://, shell:, tg://): through Explorer, exactly
        // like a double click — the new window comes to the front, programs that need
        // administrator rights ask for them on the screen. Bare names (notepad, calc): the shell.
        if target.contains('\\') || target.contains(':') {
            tap_alt();
            spawn("explorer.exe", &[target.to_owned()])
        } else {
            shell_open(target)
        }
    }

    fn run(&self, exe: &str, args: &[String]) -> Result<(), String> {
        if self.dry_run {
            return Ok(());
        }
        tap_alt();
        spawn(exe, args)
    }

    fn power(&self, what: Power) -> bool {
        if self.dry_run {
            return true;
        }
        match what {
            Power::Lock => unsafe { LockWorkStation() != 0 },
            Power::Sleep => {
                // A moment later: the answer reaches the speaker first.
                std::thread::spawn(|| {
                    std::thread::sleep(Duration::from_secs(2));
                    unsafe { SetSuspendState(0, 0, 0) };
                });
                true
            }
            Power::Shutdown => spawn("shutdown.exe", &["/s".into(), "/t".into(), "3".into()]).is_ok(),
            Power::Restart => spawn("shutdown.exe", &["/r".into(), "/t".into(), "3".into()]).is_ok(),
        }
    }

    fn click(&self, x: i32, y: i32, button: Button) -> bool {
        if self.dry_run {
            return true;
        }
        let (down, up) = if button == Button::Right { (0x0008, 0x0010) } else { (0x0002, 0x0004) };
        unsafe {
            if SetCursorPos(x, y) == 0 {
                return false;
            }
        }
        let times = if button == Button::Double { 2 } else { 1 };
        for i in 0..times {
            if i > 0 {
                std::thread::sleep(Duration::from_millis(60));
            }
            if !send(&[INPUT::mouse(down), INPUT::mouse(up)]) {
                return false;
            }
        }
        true
    }

    fn keys(&self, mods: &[u16], key: u16) -> bool {
        if self.dry_run {
            return true;
        }
        let ext = |vk: u16| if extended(vk) { 1 } else { 0 };
        let mut v: Vec<INPUT> = mods.iter().map(|m| INPUT::key(*m, 0, ext(*m))).collect();
        v.push(INPUT::key(key, 0, ext(key)));
        v.push(INPUT::key(key, 0, ext(key) | 2));
        v.extend(mods.iter().rev().map(|m| INPUT::key(*m, 0, ext(*m) | 2)));
        send(&v)
    }

    fn type_text(&self, text: &str) -> bool {
        if self.dry_run {
            return true;
        }
        // KEYEVENTF_UNICODE: the characters themselves, whatever the keyboard layout.
        let v: Vec<INPUT> = text.encode_utf16().flat_map(|c| [INPUT::key(0, c, 4), INPUT::key(0, c, 4 | 2)]).collect();
        send(&v)
    }

    fn cursor(&self) -> Option<(i32, i32)> {
        let mut p = POINT::default();
        unsafe { (GetCursorPos(&mut p) != 0).then_some((p.x, p.y)) }
    }

    fn metrics(&self) -> Value {
        let mut m = Map::new();
        m.insert("os".into(), json!("windows"));
        if let Some(cpu) = self.cpu() {
            m.insert("cpu".into(), json!(cpu));
        }
        unsafe {
            let mut ms: MEMORYSTATUSEX = std::mem::zeroed();
            ms.dwLength = std::mem::size_of::<MEMORYSTATUSEX>() as u32;
            if GlobalMemoryStatusEx(&mut ms) != 0 && ms.ullTotalPhys > 0 {
                m.insert("ram_used".into(), json!(ms.dwMemoryLoad));
                m.insert("ram_total_gb".into(), json!((ms.ullTotalPhys as f64 / 1_073_741_824.0 * 10.0).round() / 10.0));
            }
            m.insert("uptime_h".into(), json!(GetTickCount64() / 3_600_000));
        }
        if let Some(g) = self.gpu_load() {
            m.insert("gpu".into(), json!(g));
        }
        // Temperatures and nvidia-smi are slow to ask: kept 10 seconds (the Ditoo asks often).
        let mut slow = self.slow.lock().unwrap();
        let fresh = slow.as_ref().filter(|(t, _)| t.elapsed() < Duration::from_secs(10)).map(|(_, v)| v.clone());
        let extra = fresh.unwrap_or_else(|| {
            let v = slow_metrics();
            *slow = Some((Instant::now(), v.clone()));
            v
        });
        for (k, v) in extra {
            m.insert(k, v);
        }
        Value::Object(m)
    }

    fn folder(&self, which: &str) -> PathBuf {
        if which == "Desktop" { desktop() } else { documents() }
    }

    fn local_app_data(&self) -> PathBuf {
        std::env::var("LOCALAPPDATA").map(PathBuf::from).unwrap_or_else(|_| std::env::temp_dir())
    }

    fn hostname(&self) -> String {
        std::env::var("COMPUTERNAME").unwrap_or_else(|_| "Windows".into())
    }

    fn local_time(&self) -> (i32, u32, u32, u32, u32, u32) {
        let mut t = SYSTEMTIME::default();
        unsafe { GetLocalTime(&mut t) };
        (i32::from(t.wYear), u32::from(t.wMonth), u32::from(t.wDay), u32::from(t.wHour), u32::from(t.wMinute), u32::from(t.wSecond))
    }

    fn sleep_ms(&self, ms: u64) {
        std::thread::sleep(Duration::from_millis(ms));
    }

    fn mic_apps(&self) -> BTreeSet<String> {
        mic_in_use()
    }

    fn mac_for(&self, ip: &str) -> String {
        adapters().into_iter().find(|a| a.0.to_string() == ip).map(|a| a.1).unwrap_or_default()
    }

    fn wait_mic_change(&self, max: Duration) {
        let mut w = self.mic.lock().unwrap();
        if w.is_none() {
            *w = MicWatch::new();
        }
        let Some(watch) = w.as_ref() else {
            drop(w);
            std::thread::sleep(max.min(Duration::from_secs(5)));
            return;
        };
        let (event, key) = (watch.event, watch.key);
        drop(w);
        unsafe {
            WaitForSingleObject(event, max.as_millis().min(u32::MAX as u128 - 1) as u32);
            // Armed again before the list is read: no change is missed in between.
            RegNotifyChangeKeyValue(key, 1, 0x1 | 0x4, event, 1);
        }
    }
}

impl WinSys {
    /// Processor load from the time since the last call (or over 250 ms on the first).
    fn cpu(&self) -> Option<u32> {
        let sample = || unsafe {
            let (mut i, mut k, mut u) = (FILETIME::default(), FILETIME::default(), FILETIME::default());
            (GetSystemTimes(&mut i, &mut k, &mut u) != 0).then(|| (i.get(), k.get() + u.get()))
        };
        let mut last = self.cpu_last.lock().unwrap();
        let (idle0, total0) = match *last {
            Some((t, i, tot)) if t.elapsed() < Duration::from_secs(60) && t.elapsed() > Duration::from_millis(200) => (i, tot),
            _ => {
                let s = sample()?;
                std::thread::sleep(Duration::from_millis(250));
                s
            }
        };
        let (idle1, total1) = sample()?;
        *last = Some((Instant::now(), idle1, total1));
        let busy = total1.saturating_sub(total0);
        (busy > 0).then(|| (100.0 * (1.0 - idle1.saturating_sub(idle0) as f64 / busy as f64)).round().clamp(0.0, 100.0) as u32)
    }

    /// The 3D engines' load (what Task Manager shows as «GPU»), from the performance counters.
    fn gpu_load(&self) -> Option<u32> {
        let mut g = self.gpu.lock().unwrap();
        if g.is_none() {
            *g = Pdh::open("\\GPU Engine(*engtype_3D)\\Utilization Percentage");
            if let Some(p) = g.as_ref() {
                unsafe { PdhCollectQueryData(p.query) };
                std::thread::sleep(Duration::from_millis(200));
            }
        }
        let p = g.as_ref()?;
        p.sum().map(|s| s.round().clamp(0.0, 100.0) as u32)
    }
}

struct Pdh {
    query: HANDLE,
    counter: HANDLE,
}

impl Pdh {
    fn open(path: &str) -> Option<Pdh> {
        let mut q = null_mut();
        let mut c = null_mut();
        let p = wide(path);
        unsafe {
            if PdhOpenQueryW(std::ptr::null(), 0, &mut q) != 0 {
                return None;
            }
            if PdhAddEnglishCounterW(q, p.as_ptr(), 0, &mut c) != 0 {
                return None;
            }
        }
        Some(Pdh { query: q, counter: c })
    }

    fn sum(&self) -> Option<f64> {
        unsafe {
            if PdhCollectQueryData(self.query) != 0 {
                return None;
            }
            let (mut size, mut count) = (0u32, 0u32);
            PdhGetFormattedCounterArrayW(self.counter, 0x200, &mut size, &mut count, null_mut()); // PDH_FMT_DOUBLE
            if size == 0 {
                return Some(0.0);
            }
            let mut buf = vec![0u8; size as usize + 64];
            if PdhGetFormattedCounterArrayW(self.counter, 0x200, &mut size, &mut count, buf.as_mut_ptr()) != 0 {
                return None;
            }
            // PDH_FMT_COUNTERVALUE_ITEM_W: name pointer, status (padded to 8), double.
            let mut total = 0.0;
            for i in 0..count as usize {
                let base = buf.as_ptr().add(i * 24);
                let status = (base.add(8) as *const u32).read_unaligned();
                if status == 0 || status == 1 {
                    total += (base.add(16) as *const f64).read_unaligned();
                }
            }
            Some(total)
        }
    }
}

/// nvidia-smi (NVIDIA cards) and temperature sensors from WMI: the ACPI zone (not on every
/// board) and LibreHardwareMonitor / OpenHardwareMonitor when they run.
fn slow_metrics() -> Map<String, Value> {
    let mut m = Map::new();
    let smi = std::env::var("SystemRoot").map(|r| format!("{r}\\System32\\nvidia-smi.exe")).ok().filter(|p| std::path::Path::new(p).exists());
    if let Some(smi) = smi {
        use std::os::windows::process::CommandExt;
        if let Ok(o) = std::process::Command::new(smi).args(["--query-gpu=temperature.gpu,utilization.gpu", "--format=csv,noheader,nounits"]).creation_flags(CREATE_NO_WINDOW).output() {
            let text = String::from_utf8_lossy(&o.stdout);
            let p: Vec<&str> = text.lines().next().unwrap_or("").split(',').map(str::trim).collect();
            if let (Some(Ok(t)), Some(Ok(u))) = (p.first().map(|s| s.parse::<i64>()), p.get(1).map(|s| s.parse::<i64>())) {
                m.insert("gpu_temp".into(), json!(t));
                m.insert("gpu".into(), json!(u));
            }
        }
    }
    unsafe {
        let init = CoInitializeEx(null_mut(), 0x0); // multithreaded
        if let Some(rows) = wmi_query("ROOT\\WMI", "SELECT CurrentTemperature FROM MSAcpi_ThermalZoneTemperature", &["CurrentTemperature"]) {
            if let Some(t) = rows.first().and_then(|r| r[0].as_f64()).filter(|t| *t > 2732.0) {
                m.insert("cpu_temp".into(), json!((t / 10.0 - 273.15).round() as i64));
            }
        }
        for ns in ["ROOT\\LibreHardwareMonitor", "ROOT\\OpenHardwareMonitor"] {
            let Some(rows) = wmi_query(ns, "SELECT Name, Value FROM Sensor WHERE SensorType='Temperature'", &["Name", "Value"]) else { continue };
            let find = |pat: &[&str]| rows.iter().find(|r| r[0].as_str().is_some_and(|n| pat.iter().any(|p| n.contains(p)))).and_then(|r| r[1].as_f64());
            if let Some(c) = find(&["CPU Package", "Tctl", "CPU Total", "Core Average"]) {
                m.insert("cpu_temp".into(), json!(c.round() as i64));
            }
            if let Some(g) = find(&["GPU Core"]) {
                m.entry("gpu_temp").or_insert(json!(g.round() as i64));
            }
            break;
        }
        if init >= 0 {
            CoUninitialize();
        }
    }
    m
}

/// A WQL query: for each row, the asked properties (number or string, else null).
unsafe fn wmi_query(namespace: &str, query: &str, props: &[&str]) -> Option<Vec<Vec<Value>>> {
    let clsid = GUID::parse("{4590F811-1D3A-11D0-891F-00AA004B2E24}");
    let iid = GUID::parse("{DC12A687-737F-11CF-884D-00AA004B2E24}");
    let mut loc: *mut c_void = null_mut();
    if CoCreateInstance(&clsid, null_mut(), 1, &iid, &mut loc) < 0 || loc.is_null() {
        return None;
    }
    let ns = SysAllocString(wide(namespace).as_ptr());
    let mut svc: *mut c_void = null_mut();
    type Connect = unsafe extern "system" fn(*mut c_void, *mut u16, *mut u16, *mut u16, *mut u16, i32, *mut u16, *mut c_void, *mut *mut c_void) -> HRESULT;
    let connect: Connect = std::mem::transmute(vt(loc, 3));
    let hr = connect(loc, ns, null_mut(), null_mut(), null_mut(), 0, null_mut(), null_mut(), &mut svc);
    SysFreeString(ns);
    release(loc);
    if hr < 0 || svc.is_null() {
        return None;
    }
    CoSetProxyBlanket(svc, 10, 0, null_mut(), 3, 3, null_mut(), 0);
    let (lang, q) = (SysAllocString(wide("WQL").as_ptr()), SysAllocString(wide(query).as_ptr()));
    let mut en: *mut c_void = null_mut();
    type Exec = unsafe extern "system" fn(*mut c_void, *mut u16, *mut u16, i32, *mut c_void, *mut *mut c_void) -> HRESULT;
    let exec: Exec = std::mem::transmute(vt(svc, 20));
    let hr = exec(svc, lang, q, 0x20 | 0x10, null_mut(), &mut en);
    SysFreeString(lang);
    SysFreeString(q);
    release(svc);
    if hr < 0 || en.is_null() {
        return None;
    }
    type Next = unsafe extern "system" fn(*mut c_void, i32, u32, *mut *mut c_void, *mut u32) -> HRESULT;
    type Get = unsafe extern "system" fn(*mut c_void, *const u16, i32, *mut VARIANT, *mut i32, *mut i32) -> HRESULT;
    let next: Next = std::mem::transmute(vt(en, 4));
    let mut rows = Vec::new();
    for _ in 0..64 {
        let mut obj: *mut c_void = null_mut();
        let mut got = 0u32;
        // 3 seconds at most for a row.
        if next(en, 3000, 1, &mut obj, &mut got) != 0 || got == 0 || obj.is_null() {
            break;
        }
        let get: Get = std::mem::transmute(vt(obj, 4));
        let mut row = Vec::new();
        for p in props {
            let mut v: VARIANT = std::mem::zeroed();
            let name = wide(p);
            let val = if get(obj, name.as_ptr(), 0, &mut v, null_mut(), null_mut()) >= 0 { variant_value(&v) } else { Value::Null };
            VariantClear(&mut v);
            row.push(val);
        }
        release(obj);
        rows.push(row);
    }
    release(en);
    Some(rows)
}

unsafe fn variant_value(v: &VARIANT) -> Value {
    let p = v.data.as_ptr() as *const u8;
    match v.vt {
        2 => json!((p as *const i16).read_unaligned()),
        3 => json!((p as *const i32).read_unaligned()),
        4 => json!((p as *const f32).read_unaligned() as f64),
        5 => json!((p as *const f64).read_unaligned()),
        8 => json!(from_wide_ptr((p as *const *const u16).read_unaligned())),
        18 => json!((p as *const u16).read_unaligned()),
        19 => json!((p as *const u32).read_unaligned()),
        20 => json!((p as *const i64).read_unaligned()),
        21 => json!((p as *const u64).read_unaligned()),
        _ => Value::Null,
    }
}

/// Every app of the Start menu (shell:AppsFolder): name and «shell:AppsFolder\<id>».
unsafe fn apps_folder() -> Vec<Item> {
    let mut out = Vec::new();
    let init = CoInitializeEx(null_mut(), 0x2); // apartment-threaded
    let iid_item = GUID::parse("{43826D1E-E718-42EE-BC55-A1E261C37BFE}");
    let bhid_enum = GUID::parse("{94F60519-2850-4924-AA5A-D15E84868039}");
    let iid_enum = GUID::parse("{70629033-E363-4A28-A567-0DB78006E6D7}");
    let mut folder: *mut c_void = null_mut();
    if SHCreateItemFromParsingName(wide("shell:AppsFolder").as_ptr(), null_mut(), &iid_item, &mut folder) >= 0 && !folder.is_null() {
        type Bind = unsafe extern "system" fn(*mut c_void, *mut c_void, *const GUID, *const GUID, *mut *mut c_void) -> HRESULT;
        let bind: Bind = std::mem::transmute(vt(folder, 3));
        let mut en: *mut c_void = null_mut();
        if bind(folder, null_mut(), &bhid_enum, &iid_enum, &mut en) >= 0 && !en.is_null() {
            type Next = unsafe extern "system" fn(*mut c_void, u32, *mut *mut c_void, *mut u32) -> HRESULT;
            type Name = unsafe extern "system" fn(*mut c_void, u32, *mut *mut u16) -> HRESULT;
            let next: Next = std::mem::transmute(vt(en, 3));
            for _ in 0..2000 {
                let mut item: *mut c_void = null_mut();
                let mut got = 0;
                if next(en, 1, &mut item, &mut got) != 0 || got == 0 || item.is_null() {
                    break;
                }
                let name_of: Name = std::mem::transmute(vt(item, 5));
                let get = |sigdn: u32| {
                    let mut p: *mut u16 = null_mut();
                    let ok = name_of(item, sigdn, &mut p) >= 0;
                    let s = from_wide_ptr(p);
                    CoTaskMemFree(p as *mut c_void);
                    ok.then_some(s)
                };
                // SIGDN_NORMALDISPLAY; SIGDN_PARENTRELATIVEPARSING: the AppUserModelID here.
                if let (Some(name), Some(id)) = (get(0), get(0x8001_8001)) {
                    if !name.is_empty() && !id.is_empty() {
                        out.push(Item { name, path: format!("shell:AppsFolder\\{id}") });
                    }
                }
                release(item);
            }
            release(en);
        }
        release(folder);
    }
    if init >= 0 {
        CoUninitialize();
    }
    out
}

const MIC_KEY: &str = "Software\\Microsoft\\Windows\\CurrentVersion\\CapabilityAccessManager\\ConsentStore\\microphone";

/// Apps using the microphone now: LastUsedTimeStart set, LastUsedTimeStop 0 (the same data
/// as the microphone icon in the taskbar).
fn mic_in_use() -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for base in [format!("{MIC_KEY}\\NonPackaged"), MIC_KEY.to_owned()] {
        for sub in subkeys(HKEY_CURRENT_USER, &base) {
            if sub.eq_ignore_ascii_case("NonPackaged") {
                continue;
            }
            let path = format!("{base}\\{sub}");
            let start = reg_qword(HKEY_CURRENT_USER, &path, "LastUsedTimeStart").unwrap_or(0);
            let stop = reg_qword(HKEY_CURRENT_USER, &path, "LastUsedTimeStop");
            if start != 0 && stop == Some(0) {
                out.insert(mic_app_name(&sub));
            }
        }
    }
    out
}

fn subkeys(root: HKEY, key: &str) -> Vec<String> {
    let mut out = Vec::new();
    unsafe {
        let mut h: HKEY = null_mut();
        if RegOpenKeyExW(root, wide(key).as_ptr(), 0, 0x20019, &mut h) != 0 {
            return out;
        }
        for i in 0..4096 {
            let mut buf = [0u16; 512];
            let mut n = buf.len() as u32;
            if RegEnumKeyExW(h, i, buf.as_mut_ptr(), &mut n, null_mut(), null_mut(), null_mut(), null_mut()) != 0 {
                break;
            }
            out.push(String::from_utf16_lossy(&buf[..n as usize]));
        }
        RegCloseKey(h);
    }
    out
}

fn reg_qword(root: HKEY, key: &str, value: &str) -> Option<u64> {
    let mut v = 0u64;
    let mut size = 8u32;
    // RRF_RT_REG_QWORD
    let r = unsafe { RegGetValueW(root, wide(key).as_ptr(), wide(value).as_ptr(), 0x48, null_mut(), &mut v as *mut u64 as *mut c_void, &mut size) };
    (r == 0).then_some(v)
}

struct MicWatch {
    key: HKEY,
    event: HANDLE,
}

impl MicWatch {
    fn new() -> Option<MicWatch> {
        unsafe {
            let mut key: HKEY = null_mut();
            // KEY_NOTIFY | KEY_READ
            if RegOpenKeyExW(HKEY_CURRENT_USER, wide(MIC_KEY).as_ptr(), 0, 0x0010 | 0x20019, &mut key) != 0 {
                return None;
            }
            let event = CreateEventW(null_mut(), 0, 0, std::ptr::null());
            if event.is_null() {
                RegCloseKey(key);
                return None;
            }
            // Subtree; names and values (REG_NOTIFY_CHANGE_NAME | REG_NOTIFY_CHANGE_LAST_SET).
            if RegNotifyChangeKeyValue(key, 1, 0x1 | 0x4, event, 1) != 0 {
                CloseHandle(event);
                RegCloseKey(key);
                return None;
            }
            Some(MicWatch { key, event })
        }
    }
}

// ------------------------------------------------------------------ this PC on the network

/// This PC's IPv4 interfaces: (address, MAC "aa:bb:…", wired, up, point-to-point/tunnel).
pub fn adapters() -> Vec<(std::net::Ipv4Addr, String, bool, bool, bool)> {
    let mut out = Vec::new();
    let mut size: u32 = 32 * 1024;
    let mut buf = vec![0u8; size as usize];
    unsafe {
        // AF_INET; skip anycast, multicast, DNS server.
        let mut r = GetAdaptersAddresses(2, 0x2 | 0x4 | 0x8, null_mut(), buf.as_mut_ptr(), &mut size);
        if r == 111 {
            buf = vec![0u8; size as usize];
            r = GetAdaptersAddresses(2, 0x2 | 0x4 | 0x8, null_mut(), buf.as_mut_ptr(), &mut size);
        }
        if r != 0 {
            return out;
        }
        // IP_ADAPTER_ADDRESSES_LH (x64): Next @8, FirstUnicastAddress @24, PhysicalAddress @80,
        // PhysicalAddressLength @88, IfType @100, OperStatus @104.
        let mut a = buf.as_ptr();
        while !a.is_null() {
            let next = (a.add(8) as *const *const u8).read_unaligned();
            let len = (a.add(88) as *const u32).read_unaligned().min(8) as usize;
            let mac = (0..len).map(|i| format!("{:02X}", *a.add(80 + i))).collect::<Vec<_>>().join(":");
            let if_type = (a.add(100) as *const u32).read_unaligned();
            let up = (a.add(104) as *const u32).read_unaligned() == 1;
            // 6 ethernet, 71 wifi, 24 loopback, 131 tunnel, 23 ppp
            let wired = if_type == 6;
            let tunnel = matches!(if_type, 131 | 23 | 53);
            // IP_ADAPTER_UNICAST_ADDRESS: Next @8, Address.lpSockaddr @16.
            let mut u = (a.add(24) as *const *const u8).read_unaligned();
            while !u.is_null() {
                let sa = (u.add(16) as *const *const u8).read_unaligned();
                if !sa.is_null() && (sa as *const u16).read_unaligned() == 2 {
                    let ip = std::net::Ipv4Addr::new(*sa.add(4), *sa.add(5), *sa.add(6), *sa.add(7));
                    if if_type != 24 && !ip.is_loopback() {
                        out.push((ip, mac.clone(), wired, up, tunnel));
                    }
                }
                u = (u.add(8) as *const *const u8).read_unaligned();
            }
            a = next;
        }
    }
    out
}

/// The pid listening on a local TCP port (an older soundya-pc holding 8766, say).
pub fn tcp_listener_pid(port: u16) -> Option<u32> {
    let mut size: u32 = 64 * 1024;
    let mut buf = vec![0u8; size as usize];
    unsafe {
        // AF_INET, TCP_TABLE_OWNER_PID_LISTENER = 3
        let mut r = GetExtendedTcpTable(buf.as_mut_ptr(), &mut size, 0, 2, 3, 0);
        if r == 122 {
            buf = vec![0u8; size as usize + 1024];
            r = GetExtendedTcpTable(buf.as_mut_ptr(), &mut size, 0, 2, 3, 0);
        }
        if r != 0 {
            return None;
        }
        let n = (buf.as_ptr() as *const u32).read_unaligned() as usize;
        // MIB_TCPROW_OWNER_PID: state, local addr, local port (network order in the low 16 bits), remote addr, remote port, pid.
        for i in 0..n {
            let row = buf.as_ptr().add(4 + i * 24) as *const u32;
            let lport = u16::from_be(row.add(2).read_unaligned() as u16);
            if lport == port {
                return Some(row.add(5).read_unaligned());
            }
        }
    }
    None
}

/// A process's command line (Windows 8.1+), to tell soundya's old PowerShell scripts from others.
pub fn command_line(pid: u32) -> Option<String> {
    unsafe {
        let h = OpenProcess(0x1000, 0, pid);
        if h.is_null() {
            return None;
        }
        let mut buf = vec![0u8; 64 * 1024];
        let mut ret = 0u32;
        // ProcessCommandLineInformation = 60: a UNICODE_STRING followed by its text.
        let st = NtQueryInformationProcess(h, 60, buf.as_mut_ptr() as *mut c_void, buf.len() as u32, &mut ret);
        CloseHandle(h);
        if st < 0 {
            return None;
        }
        let len = (buf.as_ptr() as *const u16).read_unaligned() as usize / 2;
        let p = (buf.as_ptr().add(8) as *const *const u16).read_unaligned();
        if p.is_null() {
            return None;
        }
        Some(String::from_utf16_lossy(std::slice::from_raw_parts(p, len)))
    }
}
