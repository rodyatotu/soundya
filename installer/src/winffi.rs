//! The bits of the Windows API soundya uses, declared by hand (no crates): kernel32,
//! user32, shell32, advapi32, ole32/oleaut32, iphlpapi, powrprof, pdh.
#![allow(non_snake_case, non_camel_case_types, clippy::upper_case_acronyms, dead_code)]

use std::ffi::c_void;

pub type HANDLE = *mut c_void;
pub type HWND = *mut c_void;
pub type HKEY = *mut c_void;
pub type HICON = *mut c_void;
pub type HMENU = *mut c_void;
pub type HINSTANCE = *mut c_void;
pub type HBRUSH = *mut c_void;
pub type BOOL = i32;
pub type HRESULT = i32;
pub type WPARAM = usize;
pub type LPARAM = isize;
pub type LRESULT = isize;

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GUID {
    pub d1: u32,
    pub d2: u16,
    pub d3: u16,
    pub d4: [u8; 8],
}

impl GUID {
    /// "{XXXXXXXX-XXXX-XXXX-XXXX-XXXXXXXXXXXX}" (the braces are optional).
    pub const fn parse(s: &str) -> GUID {
        let b = s.as_bytes();
        let mut v = [0u8; 16];
        let (mut i, mut n) = (0, 0);
        while i < b.len() {
            let c = b[i];
            let h = match c {
                b'0'..=b'9' => Some(c - b'0'),
                b'a'..=b'f' => Some(c - b'a' + 10),
                b'A'..=b'F' => Some(c - b'A' + 10),
                _ => None,
            };
            if let Some(h) = h {
                v[n / 2] |= if n % 2 == 0 { h << 4 } else { h };
                n += 1;
            }
            i += 1;
        }
        GUID {
            d1: u32::from_be_bytes([v[0], v[1], v[2], v[3]]),
            d2: u16::from_be_bytes([v[4], v[5]]),
            d3: u16::from_be_bytes([v[6], v[7]]),
            d4: [v[8], v[9], v[10], v[11], v[12], v[13], v[14], v[15]],
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy, Default, Debug)]
pub struct RECT {
    pub left: i32,
    pub top: i32,
    pub right: i32,
    pub bottom: i32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct POINT {
    pub x: i32,
    pub y: i32,
}

#[repr(C)]
pub struct MSG {
    pub hwnd: HWND,
    pub message: u32,
    pub wParam: WPARAM,
    pub lParam: LPARAM,
    pub time: u32,
    pub pt: POINT,
    pub private: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct FILETIME {
    pub low: u32,
    pub high: u32,
}

impl FILETIME {
    pub fn get(self) -> u64 {
        (u64::from(self.high) << 32) | u64::from(self.low)
    }
}

#[repr(C)]
#[derive(Default)]
pub struct SYSTEMTIME {
    pub wYear: u16,
    pub wMonth: u16,
    pub wDayOfWeek: u16,
    pub wDay: u16,
    pub wHour: u16,
    pub wMinute: u16,
    pub wSecond: u16,
    pub wMilliseconds: u16,
}

#[repr(C)]
pub struct MEMORYSTATUSEX {
    pub dwLength: u32,
    pub dwMemoryLoad: u32,
    pub ullTotalPhys: u64,
    pub ullAvailPhys: u64,
    pub ullTotalPageFile: u64,
    pub ullAvailPageFile: u64,
    pub ullTotalVirtual: u64,
    pub ullAvailVirtual: u64,
    pub ullAvailExtendedVirtual: u64,
}

#[repr(C)]
pub struct PROCESSENTRY32W {
    pub dwSize: u32,
    pub cntUsage: u32,
    pub th32ProcessID: u32,
    pub th32DefaultHeapID: usize,
    pub th32ModuleID: u32,
    pub cntThreads: u32,
    pub th32ParentProcessID: u32,
    pub pcPriClassBase: i32,
    pub dwFlags: u32,
    pub szExeFile: [u16; 260],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct MOUSEINPUT {
    pub dx: i32,
    pub dy: i32,
    pub mouseData: u32,
    pub dwFlags: u32,
    pub time: u32,
    pub dwExtraInfo: usize,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct KEYBDINPUT {
    pub wVk: u16,
    pub wScan: u16,
    pub dwFlags: u32,
    pub time: u32,
    pub dwExtraInfo: usize,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub union INPUT_U {
    pub mi: MOUSEINPUT,
    pub ki: KEYBDINPUT,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct INPUT {
    pub kind: u32,
    pub u: INPUT_U,
}

impl INPUT {
    pub fn key(vk: u16, scan: u16, flags: u32) -> INPUT {
        INPUT { kind: 1, u: INPUT_U { ki: KEYBDINPUT { wVk: vk, wScan: scan, dwFlags: flags, time: 0, dwExtraInfo: 0 } } }
    }
    pub fn mouse(flags: u32) -> INPUT {
        INPUT { kind: 0, u: INPUT_U { mi: MOUSEINPUT { dx: 0, dy: 0, mouseData: 0, dwFlags: flags, time: 0, dwExtraInfo: 0 } } }
    }
}

#[repr(C)]
pub struct WNDCLASSEXW {
    pub cbSize: u32,
    pub style: u32,
    pub lpfnWndProc: Option<unsafe extern "system" fn(HWND, u32, WPARAM, LPARAM) -> LRESULT>,
    pub cbClsExtra: i32,
    pub cbWndExtra: i32,
    pub hInstance: HINSTANCE,
    pub hIcon: HICON,
    pub hCursor: HANDLE,
    pub hbrBackground: HBRUSH,
    pub lpszMenuName: *const u16,
    pub lpszClassName: *const u16,
    pub hIconSm: HICON,
}

#[repr(C)]
pub struct NOTIFYICONDATAW {
    pub cbSize: u32,
    pub hWnd: HWND,
    pub uID: u32,
    pub uFlags: u32,
    pub uCallbackMessage: u32,
    pub hIcon: HICON,
    pub szTip: [u16; 128],
    pub dwState: u32,
    pub dwStateMask: u32,
    pub szInfo: [u16; 256],
    pub uVersion: u32,
    pub szInfoTitle: [u16; 64],
    pub dwInfoFlags: u32,
    pub guidItem: GUID,
    pub hBalloonIcon: HICON,
}

#[repr(C)]
pub struct ICONINFO {
    pub fIcon: BOOL,
    pub xHotspot: u32,
    pub yHotspot: u32,
    pub hbmMask: HANDLE,
    pub hbmColor: HANDLE,
}

#[repr(C)]
pub struct BITMAPINFOHEADER {
    pub biSize: u32,
    pub biWidth: i32,
    pub biHeight: i32,
    pub biPlanes: u16,
    pub biBitCount: u16,
    pub biCompression: u32,
    pub biSizeImage: u32,
    pub biXPelsPerMeter: i32,
    pub biYPelsPerMeter: i32,
    pub biClrUsed: u32,
    pub biClrImportant: u32,
}

#[repr(C)]
pub struct WINDOWPLACEMENT {
    pub length: u32,
    pub flags: u32,
    pub showCmd: u32,
    pub ptMinPosition: POINT,
    pub ptMaxPosition: POINT,
    pub rcNormalPosition: RECT,
}

#[repr(C)]
pub struct SHELLEXECUTEINFOW {
    pub cbSize: u32,
    pub fMask: u32,
    pub hwnd: HWND,
    pub lpVerb: *const u16,
    pub lpFile: *const u16,
    pub lpParameters: *const u16,
    pub lpDirectory: *const u16,
    pub nShow: i32,
    pub hInstApp: HINSTANCE,
    pub lpIDList: *mut c_void,
    pub lpClass: *const u16,
    pub hkeyClass: HKEY,
    pub dwHotKey: u32,
    pub hIconOrMonitor: HANDLE,
    pub hProcess: HANDLE,
}

#[repr(C)]
pub struct VARIANT {
    pub vt: u16,
    pub r1: u16,
    pub r2: u16,
    pub r3: u16,
    pub data: [u64; 2],
}

#[link(name = "kernel32")]
extern "system" {
    pub fn CloseHandle(h: HANDLE) -> BOOL;
    pub fn CreateToolhelp32Snapshot(flags: u32, pid: u32) -> HANDLE;
    pub fn Process32FirstW(snap: HANDLE, pe: *mut PROCESSENTRY32W) -> BOOL;
    pub fn Process32NextW(snap: HANDLE, pe: *mut PROCESSENTRY32W) -> BOOL;
    pub fn OpenProcess(access: u32, inherit: BOOL, pid: u32) -> HANDLE;
    pub fn QueryFullProcessImageNameW(h: HANDLE, flags: u32, name: *mut u16, size: *mut u32) -> BOOL;
    pub fn TerminateProcess(h: HANDLE, code: u32) -> BOOL;
    pub fn GetExitCodeProcess(h: HANDLE, code: *mut u32) -> BOOL;
    pub fn GetSystemTimes(idle: *mut FILETIME, kernel: *mut FILETIME, user: *mut FILETIME) -> BOOL;
    pub fn GlobalMemoryStatusEx(m: *mut MEMORYSTATUSEX) -> BOOL;
    pub fn GetTickCount64() -> u64;
    pub fn GetLocalTime(t: *mut SYSTEMTIME);
    pub fn CreateEventW(attrs: *mut c_void, manual: BOOL, initial: BOOL, name: *const u16) -> HANDLE;
    pub fn SetEvent(h: HANDLE) -> BOOL;
    pub fn WaitForSingleObject(h: HANDLE, ms: u32) -> u32;
    pub fn CreateMutexW(attrs: *mut c_void, owner: BOOL, name: *const u16) -> HANDLE;
    pub fn OpenMutexW(access: u32, inherit: BOOL, name: *const u16) -> HANDLE;
    pub fn GetLastError() -> u32;
    pub fn LoadLibraryW(name: *const u16) -> HINSTANCE;
    pub fn GetProcAddress(m: HINSTANCE, name: *const u8) -> *mut c_void;
    pub fn GetModuleHandleW(name: *const u16) -> HINSTANCE;
    pub fn SetEnvironmentVariableW(name: *const u16, value: *const u16) -> BOOL;
    pub fn GlobalAlloc(flags: u32, bytes: usize) -> HANDLE;
    pub fn GlobalLock(h: HANDLE) -> *mut c_void;
    pub fn GlobalUnlock(h: HANDLE) -> BOOL;
    pub fn SetProcessWorkingSetSize(h: HANDLE, min: usize, max: usize) -> BOOL;
    pub fn GetCurrentProcess() -> HANDLE;
    pub fn SetPriorityClass(h: HANDLE, class: u32) -> BOOL;
}

#[link(name = "user32")]
extern "system" {
    pub fn EnumWindows(f: unsafe extern "system" fn(HWND, LPARAM) -> BOOL, l: LPARAM) -> BOOL;
    pub fn GetWindowThreadProcessId(h: HWND, pid: *mut u32) -> u32;
    pub fn IsWindowVisible(h: HWND) -> BOOL;
    pub fn IsWindow(h: HWND) -> BOOL;
    pub fn GetWindow(h: HWND, cmd: u32) -> HWND;
    pub fn PostMessageW(h: HWND, msg: u32, w: WPARAM, l: LPARAM) -> BOOL;
    pub fn SendMessageW(h: HWND, msg: u32, w: WPARAM, l: LPARAM) -> LRESULT;
    pub fn SetForegroundWindow(h: HWND) -> BOOL;
    pub fn GetForegroundWindow() -> HWND;
    pub fn ShowWindow(h: HWND, cmd: i32) -> BOOL;
    pub fn IsIconic(h: HWND) -> BOOL;
    pub fn SendInput(n: u32, inputs: *const INPUT, size: i32) -> u32;
    pub fn SetCursorPos(x: i32, y: i32) -> BOOL;
    pub fn GetCursorPos(p: *mut POINT) -> BOOL;
    pub fn LockWorkStation() -> BOOL;
    pub fn SetProcessDPIAware() -> BOOL;
    pub fn RegisterClassExW(c: *const WNDCLASSEXW) -> u16;
    pub fn CreateWindowExW(ex: u32, class: *const u16, title: *const u16, style: u32, x: i32, y: i32, w: i32, h: i32, parent: HWND, menu: HMENU, inst: HINSTANCE, param: *mut c_void) -> HWND;
    pub fn DestroyWindow(h: HWND) -> BOOL;
    pub fn DefWindowProcW(h: HWND, msg: u32, w: WPARAM, l: LPARAM) -> LRESULT;
    pub fn GetMessageW(m: *mut MSG, h: HWND, min: u32, max: u32) -> BOOL;
    pub fn TranslateMessage(m: *const MSG) -> BOOL;
    pub fn DispatchMessageW(m: *const MSG) -> LRESULT;
    pub fn PostQuitMessage(code: i32);
    pub fn GetClientRect(h: HWND, r: *mut RECT) -> BOOL;
    pub fn GetWindowRect(h: HWND, r: *mut RECT) -> BOOL;
    pub fn SetWindowPos(h: HWND, after: HWND, x: i32, y: i32, w: i32, cy: i32, flags: u32) -> BOOL;
    pub fn MoveWindow(h: HWND, x: i32, y: i32, w: i32, cy: i32, repaint: BOOL) -> BOOL;
    pub fn GetWindowPlacement(h: HWND, p: *mut WINDOWPLACEMENT) -> BOOL;
    pub fn SetWindowPlacement(h: HWND, p: *const WINDOWPLACEMENT) -> BOOL;
    pub fn CreatePopupMenu() -> HMENU;
    pub fn DestroyMenu(m: HMENU) -> BOOL;
    pub fn AppendMenuW(m: HMENU, flags: u32, id: usize, text: *const u16) -> BOOL;
    pub fn TrackPopupMenu(m: HMENU, flags: u32, x: i32, y: i32, r: i32, h: HWND, rect: *const RECT) -> BOOL;
    pub fn LoadCursorW(inst: HINSTANCE, name: *const u16) -> HANDLE;
    pub fn CreateIconIndirect(i: *const ICONINFO) -> HICON;
    pub fn DestroyIcon(i: HICON) -> BOOL;
    pub fn MessageBoxW(h: HWND, text: *const u16, title: *const u16, kind: u32) -> i32;
    pub fn RegisterWindowMessageW(name: *const u16) -> u32;
    pub fn FindWindowW(class: *const u16, title: *const u16) -> HWND;
    pub fn SetTimer(h: HWND, id: usize, ms: u32, f: *mut c_void) -> usize;
    pub fn KillTimer(h: HWND, id: usize) -> BOOL;
    pub fn ReleaseCapture() -> BOOL;
    pub fn GetSystemMetrics(i: i32) -> i32;
    pub fn SystemParametersInfoW(action: u32, p: u32, v: *mut c_void, win: u32) -> BOOL;
    pub fn OpenClipboard(h: HWND) -> BOOL;
    pub fn EmptyClipboard() -> BOOL;
    pub fn SetClipboardData(fmt: u32, h: HANDLE) -> HANDLE;
    pub fn CloseClipboard() -> BOOL;
    pub fn SetWindowLongPtrW(h: HWND, i: i32, v: isize) -> isize;
    pub fn GetWindowLongPtrW(h: HWND, i: i32) -> isize;
    pub fn GetDpiForWindow(h: HWND) -> u32;
    pub fn GetDpiForSystem() -> u32;
    pub fn SetWindowTextW(h: HWND, t: *const u16) -> BOOL;
    pub fn InvalidateRect(h: HWND, r: *const RECT, erase: BOOL) -> BOOL;
}

#[link(name = "gdi32")]
extern "system" {
    pub fn CreateDIBSection(dc: HANDLE, bi: *const BITMAPINFOHEADER, usage: u32, bits: *mut *mut c_void, section: HANDLE, offset: u32) -> HANDLE;
    pub fn CreateBitmap(w: i32, h: i32, planes: u32, bits: u32, data: *const c_void) -> HANDLE;
    pub fn DeleteObject(o: HANDLE) -> BOOL;
    pub fn CreateSolidBrush(color: u32) -> HBRUSH;
}

#[link(name = "shell32")]
extern "system" {
    pub fn ShellExecuteW(h: HWND, op: *const u16, file: *const u16, params: *const u16, dir: *const u16, show: i32) -> HINSTANCE;
    pub fn ShellExecuteExW(info: *mut SHELLEXECUTEINFOW) -> BOOL;
    pub fn SHGetKnownFolderPath(id: *const GUID, flags: u32, token: HANDLE, path: *mut *mut u16) -> HRESULT;
    pub fn Shell_NotifyIconW(msg: u32, data: *mut NOTIFYICONDATAW) -> BOOL;
    pub fn SHCreateItemFromParsingName(name: *const u16, bc: *mut c_void, riid: *const GUID, out: *mut *mut c_void) -> HRESULT;
}

#[link(name = "advapi32")]
extern "system" {
    pub fn RegGetValueW(key: HKEY, sub: *const u16, value: *const u16, flags: u32, kind: *mut u32, data: *mut c_void, size: *mut u32) -> i32;
    pub fn RegOpenKeyExW(key: HKEY, sub: *const u16, opts: u32, access: u32, out: *mut HKEY) -> i32;
    pub fn RegCreateKeyExW(key: HKEY, sub: *const u16, res: u32, class: *const u16, opts: u32, access: u32, sec: *mut c_void, out: *mut HKEY, disp: *mut u32) -> i32;
    pub fn RegCloseKey(key: HKEY) -> i32;
    pub fn RegEnumKeyExW(key: HKEY, i: u32, name: *mut u16, len: *mut u32, res: *mut u32, class: *mut u16, clen: *mut u32, time: *mut FILETIME) -> i32;
    pub fn RegQueryValueExW(key: HKEY, name: *const u16, res: *mut u32, kind: *mut u32, data: *mut u8, size: *mut u32) -> i32;
    pub fn RegSetValueExW(key: HKEY, name: *const u16, res: u32, kind: u32, data: *const u8, size: u32) -> i32;
    pub fn RegDeleteValueW(key: HKEY, name: *const u16) -> i32;
    pub fn RegNotifyChangeKeyValue(key: HKEY, subtree: BOOL, filter: u32, event: HANDLE, asynchronous: BOOL) -> i32;
}

#[link(name = "ole32")]
extern "system" {
    pub fn CoInitializeEx(r: *mut c_void, flags: u32) -> HRESULT;
    pub fn CoUninitialize();
    pub fn CoTaskMemFree(p: *mut c_void);
    pub fn CoCreateInstance(clsid: *const GUID, outer: *mut c_void, ctx: u32, iid: *const GUID, out: *mut *mut c_void) -> HRESULT;
    pub fn CoSetProxyBlanket(p: *mut c_void, authn: u32, authz: u32, name: *mut u16, level: u32, imp: u32, auth: *mut c_void, caps: u32) -> HRESULT;
}

#[link(name = "oleaut32")]
extern "system" {
    pub fn SysAllocString(s: *const u16) -> *mut u16;
    pub fn SysFreeString(s: *mut u16);
    pub fn VariantClear(v: *mut VARIANT) -> HRESULT;
}

#[link(name = "powrprof")]
extern "system" {
    pub fn SetSuspendState(hibernate: u8, force: u8, wake_off: u8) -> u8;
}

#[link(name = "pdh")]
extern "system" {
    pub fn PdhOpenQueryW(src: *const u16, data: usize, q: *mut HANDLE) -> u32;
    pub fn PdhAddEnglishCounterW(q: HANDLE, path: *const u16, data: usize, c: *mut HANDLE) -> u32;
    pub fn PdhCollectQueryData(q: HANDLE) -> u32;
    pub fn PdhGetFormattedCounterArrayW(c: HANDLE, fmt: u32, size: *mut u32, count: *mut u32, items: *mut u8) -> u32;
}

#[link(name = "iphlpapi")]
extern "system" {
    pub fn GetAdaptersAddresses(family: u32, flags: u32, res: *mut c_void, out: *mut u8, size: *mut u32) -> u32;
    pub fn GetExtendedTcpTable(table: *mut u8, size: *mut u32, order: BOOL, af: u32, class: u32, res: u32) -> u32;
}

#[link(name = "advapi32")]
extern "system" {
    /// RtlGenRandom.
    pub fn SystemFunction036(buf: *mut u8, len: u32) -> u8;
}

#[link(name = "dwmapi")]
extern "system" {
    pub fn DwmSetWindowAttribute(h: HWND, attr: u32, value: *const c_void, size: u32) -> HRESULT;
}

#[link(name = "ntdll")]
extern "system" {
    pub fn NtQueryInformationProcess(h: HANDLE, class: u32, info: *mut c_void, len: u32, ret: *mut u32) -> i32;
}

pub const HKEY_CURRENT_USER: HKEY = 0x8000_0001u32 as i32 as isize as HKEY;
pub const HKEY_LOCAL_MACHINE: HKEY = 0x8000_0002u32 as i32 as isize as HKEY;

/// UTF-16, zero-terminated.
pub fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// A zero-terminated UTF-16 string at `p`.
///
/// # Safety
/// `p` must be null or point to a zero-terminated UTF-16 string.
pub unsafe fn from_wide_ptr(p: *const u16) -> String {
    if p.is_null() {
        return String::new();
    }
    let mut n = 0;
    while *p.add(n) != 0 {
        n += 1;
    }
    String::from_utf16_lossy(std::slice::from_raw_parts(p, n))
}

pub fn from_wide(buf: &[u16]) -> String {
    let n = buf.iter().position(|c| *c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..n])
}

/// A COM method by its index in the vtable.
///
/// # Safety
/// `this` must be a live COM object whose vtable has at least `i + 1` entries.
pub unsafe fn vt(this: *mut c_void, i: usize) -> *const c_void {
    let table = *(this as *const *const *const c_void);
    *table.add(i)
}

/// IUnknown::Release.
///
/// # Safety
/// `this` must be null or a live COM object.
pub unsafe fn release(this: *mut c_void) {
    if !this.is_null() {
        let f: unsafe extern "system" fn(*mut c_void) -> u32 = std::mem::transmute(vt(this, 2));
        f(this);
    }
}

pub fn message_box(text: &str, title: &str) {
    let (t, c) = (wide(text), wide(title));
    // MB_OK | MB_ICONINFORMATION | MB_SETFOREGROUND
    unsafe { MessageBoxW(std::ptr::null_mut(), t.as_ptr(), c.as_ptr(), 0x40 | 0x10000) };
}

/// Copies text to the clipboard.
pub fn set_clipboard(text: &str) -> bool {
    let w = wide(text);
    unsafe {
        if OpenClipboard(std::ptr::null_mut()) == 0 {
            return false;
        }
        EmptyClipboard();
        let h = GlobalAlloc(0x0002, w.len() * 2); // GMEM_MOVEABLE
        let ok = !h.is_null() && {
            let p = GlobalLock(h) as *mut u16;
            std::ptr::copy_nonoverlapping(w.as_ptr(), p, w.len());
            GlobalUnlock(h);
            !SetClipboardData(13, h).is_null() // CF_UNICODETEXT
        };
        CloseClipboard();
        ok
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn guids_parse() {
        let g = super::GUID::parse("{C979903E-D4CA-4228-92EB-47EE3FA96EAB}");
        assert_eq!((g.d1, g.d2, g.d3, g.d4), (0xC979903E, 0xD4CA, 0x4228, [0x92, 0xEB, 0x47, 0xEE, 0x3F, 0xA9, 0x6E, 0xAB]));
    }
}
