//! Connecting this Windows PC to the speaker, natively (it replaces pc/install.ps1):
//!   - soundya.exe goes to %LOCALAPPDATA%\soundya (a download may be deleted);
//!   - %LOCALAPPDATA%\soundya-pc\config.json: the key kept if there is one (the same file
//!     soundya-pc.ps1 used, so a PC connected before keeps its key);
//!   - started at logon from HKCU\…\Run, as the user, no administrator rights;
//!   - the firewall rule (home network only) and wake-on-LAN: the one question for
//!     administrator rights, only when the rule is not there yet;
//!   - soundya-pc.ps1's autostart goes and its copies stop;
//!   - the address, the key and the MAC go into the speaker's settings.

use std::{path::PathBuf, time::{Duration, Instant}};

use serde_json::{json, Value};

use crate::winapp::pc_dir;
use crate::winffi::*;
use crate::winsys;

const RUN_KEY: &str = "Software\\Microsoft\\Windows\\CurrentVersion\\Run";

pub fn random(n: usize) -> Vec<u8> {
    let mut b = vec![0u8; n];
    unsafe { SystemFunction036(b.as_mut_ptr(), n as u32) };
    b
}

/// A new key as install.ps1 made them: 18 random bytes, base64 with + → x and / → y.
fn new_key() -> String {
    crate::pc::base64(&random(18)).replace('+', "x").replace('/', "y")
}

/// The Startup folder (soundya-pc.ps1's .vbs files were there).
fn startup_dir() -> PathBuf {
    PathBuf::from(std::env::var("APPDATA").unwrap_or_default()).join("Microsoft\\Windows\\Start Menu\\Programs\\Startup")
}

fn legacy(cmdline: &str) -> bool {
    let c = cmdline.to_lowercase();
    c.contains("soundya-pc.ps1") || c.contains("soundya-pc.vbs") || c.contains("soundya-pc-tray")
}

/// soundya-pc.ps1 from before: its autostart goes and its copies (PowerShell, and the
/// wscript loop that restarted it every 5 seconds) stop.
pub fn stop_legacy() {
    for f in ["soundya-pc.vbs", "soundya-pc-tray.vbs"] {
        let _ = std::fs::remove_file(startup_dir().join(f));
    }
    for (pid, exe) in winsys::process_list() {
        let e = exe.to_lowercase();
        if matches!(e.as_str(), "wscript.exe" | "powershell.exe" | "pwsh.exe") && winsys::command_line(pid).is_some_and(|c| legacy(&c)) {
            winsys::kill(pid);
        }
    }
}

/// The agent's port held by an old soundya-pc.ps1: that copy stops.
pub fn free_port(port: u16) {
    if let Some(pid) = winsys::tcp_listener_pid(port) {
        if winsys::command_line(pid).is_some_and(|c| legacy(&c)) {
            winsys::kill(pid);
        }
    }
}

/// This PC's address towards the speaker and the MAC for wake-on-LAN (the wired card:
/// a switched-off PC wakes only over a cable).
pub fn address_for(host: &str) -> (String, String) {
    let ip = std::net::UdpSocket::bind("0.0.0.0:0")
        .and_then(|s| s.connect((host, 8765)).map(|()| s))
        .and_then(|s| s.local_addr())
        .map(|a| a.ip().to_string())
        .unwrap_or_default();
    let all = winsys::adapters();
    let wired = all.iter().filter(|a| a.2 && !a.4).max_by_key(|a| a.3).map(|a| a.1.clone());
    let own = all.iter().find(|a| a.0.to_string() == ip).map(|a| a.1.clone());
    (ip, wired.or(own).unwrap_or_default())
}

/// soundya.exe in %LOCALAPPDATA%\soundya: copied there (the running copies of an older one
/// stop first); the path to start at logon.
fn install_exe() -> Result<PathBuf, String> {
    let me = std::env::current_exe().map_err(|e| e.to_string())?;
    let target = crate::winwv2::app_dir().join("soundya.exe");
    let same = |a: &std::path::Path, b: &std::path::Path| a.to_string_lossy().eq_ignore_ascii_case(&b.to_string_lossy());
    if same(&me, &target) {
        return Ok(target);
    }
    let own = std::process::id();
    let t = target.to_string_lossy().to_lowercase();
    for (pid, _) in winsys::process_list() {
        if pid != own && winsys::process_path(pid).is_some_and(|p| p.to_lowercase() == t) {
            winsys::kill(pid);
        }
    }
    let mut last = String::new();
    for _ in 0..20 {
        match std::fs::copy(&me, &target) {
            Ok(_) => return Ok(target),
            Err(e) => last = e.to_string(),
        }
        std::thread::sleep(Duration::from_millis(300));
    }
    Err(format!("soundya.exe не скопировался в {}: {last}", target.display()))
}

fn set_run(name: &str, value: Option<&str>) {
    unsafe {
        let mut k: HKEY = std::ptr::null_mut();
        if RegCreateKeyExW(HKEY_CURRENT_USER, wide(RUN_KEY).as_ptr(), 0, std::ptr::null(), 0, 0x20006, std::ptr::null_mut(), &mut k, std::ptr::null_mut()) != 0 {
            return;
        }
        match value {
            Some(v) => {
                let w = wide(v);
                RegSetValueExW(k, wide(name).as_ptr(), 0, 1, w.as_ptr() as *const u8, (w.len() * 2) as u32); // REG_SZ
            }
            None => {
                RegDeleteValueW(k, wide(name).as_ptr());
            }
        }
        RegCloseKey(k);
    }
}

/// config.json: the key kept (or made), the port, the speaker. Returns (key, port).
fn write_config(speaker: &str) -> Result<(String, u16), String> {
    let dir = pc_dir();
    std::fs::create_dir_all(&dir).map_err(|e| format!("не создалась папка {}: {e}", dir.display()))?;
    let path = dir.join("config.json");
    let mut v: Value = std::fs::read_to_string(&path).ok().and_then(|t| serde_json::from_str(t.trim_start_matches('\u{feff}')).ok()).unwrap_or_else(|| json!({}));
    if !v.is_object() {
        v = json!({});
    }
    let key = match v["key"].as_str() {
        Some(k) if k.len() >= 16 => k.to_owned(),
        _ => new_key(),
    };
    v["key"] = json!(key);
    if v["port"].as_u64().is_none() {
        v["port"] = json!(crate::winagent::PORT);
    }
    for k in ["apps", "sites"] {
        if !v[k].is_object() {
            v[k] = json!({});
        }
    }
    v["speaker"] = json!(speaker);
    let port = v["port"].as_u64().and_then(|p| u16::try_from(p).ok()).unwrap_or(crate::winagent::PORT);
    std::fs::write(&path, serde_json::to_string_pretty(&v).unwrap_or_default()).map_err(|e| format!("не записался {}: {e}", path.display()))?;
    Ok((key, port))
}

/// Is there a firewall rule of that name? (netsh answers without administrator rights.)
fn rule_exists(name: &str) -> bool {
    use std::os::windows::process::CommandExt;
    std::process::Command::new("netsh")
        .args(["advfirewall", "firewall", "show", "rule", &format!("name={name}")])
        .creation_flags(0x0800_0000)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// What runs with administrator rights, once: the firewall (port and program, home network
/// only; a «block» Windows may have made for soundya.exe goes) and wake-on-LAN (as install.ps1).
pub fn admin_script(exe: &str, port: u16) -> String {
    let exe = exe.replace('\'', "''");
    format!(
        r#"$ErrorActionPreference = 'SilentlyContinue'
Get-NetFirewallRule -DisplayName 'soundya-pc' | Remove-NetFirewallRule
Get-NetFirewallRule -DisplayName 'soundya' | Remove-NetFirewallRule
New-NetFirewallRule -DisplayName 'soundya-pc' -Direction Inbound -Protocol TCP -LocalPort {port} -Action Allow -Profile Any -RemoteAddress LocalSubnet | Out-Null
New-NetFirewallRule -DisplayName 'soundya' -Direction Inbound -Program '{exe}' -Protocol TCP -LocalPort {port} -Action Allow -Profile Any -RemoteAddress LocalSubnet | Out-Null
Get-NetFirewallApplicationFilter -Program '{exe}' | Get-NetFirewallRule | Where-Object {{ $_.Action -eq 'Block' }} | Remove-NetFirewallRule
$wired = @(Get-NetAdapter -Physical | Where-Object {{ $_.MediaType -eq '802.3' }})
foreach ($a in $wired) {{
    Set-NetAdapterPowerManagement -Name $a.Name -WakeOnMagicPacket Enabled
    foreach ($p in 'Wake on Magic Packet', '*WakeOnMagicPacket', 'Wake on magic packet') {{
        Set-NetAdapterAdvancedProperty -Name $a.Name -DisplayName $p -DisplayValue 'Enabled'
        Set-NetAdapterAdvancedProperty -Name $a.Name -RegistryKeyword $p -RegistryValue 1
    }}
}}
if ($wired.Count) {{ Set-ItemProperty -Path 'HKLM:\SYSTEM\CurrentControlSet\Control\Session Manager\Power' -Name HiberbootEnabled -Value 0 }}
"#
    )
}

/// PowerShell's -EncodedCommand: base64 of UTF-16LE.
pub fn encoded(script: &str) -> String {
    let bytes: Vec<u8> = script.encode_utf16().flat_map(u16::to_le_bytes).collect();
    crate::pc::base64(&bytes)
}

/// Runs the admin script with the UAC question; false if it was declined or failed to start.
fn run_elevated(script: &str) -> bool {
    let args = format!("-NoProfile -ExecutionPolicy Bypass -WindowStyle Hidden -EncodedCommand {}", encoded(script));
    let (verb, file, params) = (wide("runas"), wide("powershell.exe"), wide(&args));
    unsafe {
        let mut info: SHELLEXECUTEINFOW = std::mem::zeroed();
        info.cbSize = std::mem::size_of::<SHELLEXECUTEINFOW>() as u32;
        info.fMask = 0x40 | 0x100; // SEE_MASK_NOCLOSEPROCESS | SEE_MASK_NOASYNC
        info.lpVerb = verb.as_ptr();
        info.lpFile = file.as_ptr();
        info.lpParameters = params.as_ptr();
        info.nShow = 0;
        if ShellExecuteExW(&mut info) == 0 {
            return false;
        }
        if !info.hProcess.is_null() {
            WaitForSingleObject(info.hProcess, 120_000);
            CloseHandle(info.hProcess);
        }
    }
    true
}

/// A signed ping to the agent on this PC: its answer, or why not.
fn ping(key: &str, port: u16) -> Result<String, String> {
    use std::io::{Read, Write};
    let mut s = std::net::TcpStream::connect_timeout(&std::net::SocketAddr::from(([127, 0, 0, 1], port)), Duration::from_secs(2)).map_err(|e| e.to_string())?;
    s.set_read_timeout(Some(Duration::from_secs(5))).ok();
    let mut req = "POST /ping HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Length: 0\r\nConnection: close\r\n".to_owned();
    for (k, v) in crate::winagent::sign(key, "/ping", b"", &random(8)) {
        req.push_str(&format!("{k}: {v}\r\n"));
    }
    req.push_str("\r\n");
    s.write_all(req.as_bytes()).map_err(|e| e.to_string())?;
    let mut out = String::new();
    s.read_to_string(&mut out).map_err(|e| e.to_string())?;
    Ok(out.split_once("\r\n\r\n").map_or("", |(_, b)| b).to_owned())
}

/// Stops soundya's agent (and, unless it is this process, its tray icon).
fn stop_running(target: &std::path::Path, tray_too: bool) {
    let own = std::process::id();
    let t = target.to_string_lossy().to_lowercase();
    for (pid, _) in winsys::process_list() {
        if pid == own || !winsys::process_path(pid).is_some_and(|p| p.to_lowercase() == t) {
            continue;
        }
        let cmd = winsys::command_line(pid).unwrap_or_default();
        if cmd.trim_end().ends_with(" agent") || (tray_too && cmd.trim_end().ends_with(" tray")) {
            winsys::kill(pid);
        }
    }
}

/// «Подключить этот компьютер»: everything above; the text for the page.
pub fn pc_setup(ip: &str) -> Result<String, String> {
    let target = install_exe()?;
    stop_legacy();
    // The scheduled task of soundya-pc before the Startup folder (it started PowerShell at logon).
    {
        use std::os::windows::process::CommandExt;
        let _ = std::process::Command::new("schtasks").args(["/Delete", "/TN", "soundya-pc", "/F"]).creation_flags(0x0800_0000)
            .stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).status();
    }
    let (key, port) = write_config(ip)?;
    let exe = target.to_string_lossy().into_owned();
    set_run("soundya-pc", Some(&format!("\"{exe}\" agent")));
    set_run("soundya", Some(&format!("\"{exe}\" tray")));
    // The firewall before the agent listens: else Windows asks itself, and «Отмена» there blocks it.
    let mut firewall_note = "";
    if !rule_exists("soundya") && !run_elevated(&admin_script(&exe, port)) {
        firewall_note = " Брандмауэр Windows не настроен (запрос администратора отклонён): если колонка не достучится, разрешите soundya в брандмауэре.";
    }
    stop_running(&target, false);
    std::thread::sleep(Duration::from_millis(300));
    // Started as this user, from the installed copy.
    let _ = winsys::spawn(&exe, &["agent".into()]);
    let t0 = Instant::now();
    loop {
        match ping(&key, port) {
            Ok(t) if t.starts_with("Компьютер") => break,
            _ if t0.elapsed() > Duration::from_secs(15) => {
                return Err(format!("программа для ПК не отвечает; журнал: {}", pc_dir().join("log.txt").display()));
            }
            _ => std::thread::sleep(Duration::from_millis(400)),
        }
    }
    // The tray icon: soundya on this PC without the browser (at logon too).
    if !crate::winapp::tray_running() {
        let _ = winsys::spawn(&exe, &["tray".into()]);
    }
    let (me, mac) = address_for(ip);
    // Who is there already (a Mac stays first; this PC joins or is updated).
    let current = match crate::speaker_http(ip, "GET", "/api/status", "") {
        Ok((200, body)) => serde_json::from_str::<Value>(&body).map(|v| v["settings"].clone()).unwrap_or_default(),
        _ => Value::Null,
    };
    let patch = crate::winagent::pc_patch(&current, &me, &key, &mac);
    match crate::speaker_http(ip, "POST", "/api/settings", &patch.to_string()) {
        Ok((200, body)) if body.starts_with("OK") => Ok(format!("Компьютер {me} подключён к колонке; soundya — значок возле часов.{firewall_note}")),
        Ok((401, _)) => Err(format!("на панели колонки стоит PIN: впишите в «Устройства → Компьютер» адрес {me}, MAC {mac} и ключ (меню значка soundya → «Данные для колонки»)")),
        Ok((_, body)) => Err(format!("колонка не приняла настройки: {body}")),
        Err(e) => Err(e),
    }
}

/// Does soundya answer at this address (its panel)? 401: yes, with a PIN.
fn answers(host: &str) -> bool {
    host.parse::<std::net::Ipv4Addr>().is_ok() && matches!(crate::speaker_http(host, "GET", "/api/status", ""), Ok((200 | 401, _)))
}

/// The speaker for this PC: the one it knows (config.json, the last caller in status.json,
/// the speaker the installer remembers), else the first soundya found in the home network.
pub fn find_speaker() -> Option<String> {
    let mut known = Vec::new();
    if let Some(c) = crate::winagent::Config::load(&pc_dir()) {
        known.push(c.speaker);
    }
    known.push(crate::winagent::Status::load(&pc_dir()).speaker);
    if let Some(v) = crate::remembered_speaker(&crate::data_dir()) {
        known.push(v["ip"].as_str().unwrap_or("").to_owned());
    }
    if let Some(h) = known.iter().find(|h| answers(h)) {
        return Some(h.clone());
    }
    let found = crate::scan();
    found["devices"].as_array()?.iter().find(|d| !d["soundya"].is_null()).and_then(|d| d["ip"].as_str()).map(str::to_owned)
}

/// This PC is connected to the speaker at `ip`: its agent answers, the firewall rule is there and
/// the speaker has its address. (A speaker that does not answer or has a PIN: left alone.)
fn connected(ip: &str) -> bool {
    let Some(cfg) = crate::winagent::Config::load(&pc_dir()).filter(|c| c.key.len() >= 16) else { return false };
    if cfg.speaker != ip || !rule_exists("soundya") || !crate::winapp::installed_copy() {
        return false;
    }
    crate::winapp::start_agent();
    let t0 = Instant::now();
    while !ping(&cfg.key, cfg.port).is_ok_and(|t| t.starts_with("Компьютер")) {
        if t0.elapsed() > Duration::from_secs(5) {
            return false;
        }
        std::thread::sleep(Duration::from_millis(400));
    }
    match crate::speaker_http(ip, "GET", "/api/status", "") {
        Ok((200, body)) => serde_json::from_str::<Value>(&body).is_ok_and(|v| crate::winagent::pc_listed(&v["settings"], &address_for(ip).0)),
        _ => true,
    }
}

/// soundya.exe opened: the speaker found, the windows sent to it, and this PC connected if it is
/// not yet — quietly, in the background (only the firewall's one administrator question shows).
pub fn connect_in_background(tray: usize) {
    std::thread::spawn(move || {
        let log = |t: &str| crate::winagent::log_to(&pc_dir(), crate::winagent::Sys::local_time(&winsys::WinSys::new(true)), t);
        let _ = std::fs::create_dir_all(pc_dir());
        let Some(ip) = find_speaker() else {
            log("connect: no soundya speaker found in the home network");
            return;
        };
        *crate::winapp::NEW_SPEAKER.lock().unwrap() = Some(ip.clone());
        unsafe { PostMessageW(tray as HWND, crate::winapp::WM_APP_SPEAKER, 0, 0) };
        if connected(&ip) {
            return;
        }
        match pc_setup(&ip) {
            Ok(t) => log(&format!("connect: {t}")),
            Err(e) => log(&format!("connect: {e}")),
        }
    });
}

/// soundya.exe opened from a download: put in place (%LOCALAPPDATA%\soundya, the copy there
/// replaced — an update) and started from there. None: it could not be, run from here.
pub fn install_and_start() -> Option<()> {
    let target = install_exe().ok()?;
    winsys::spawn(&target.to_string_lossy(), &["tray".into(), "open".into()]).ok()
}

/// Removes soundya's PC part from this PC; the speaker stops calling it.
pub fn pc_remove(ip: &str) -> Result<String, String> {
    set_run("soundya-pc", None);
    set_run("soundya", None);
    let target = crate::winwv2::app_dir().join("soundya.exe");
    stop_running(&target, true);
    stop_legacy();
    std::thread::sleep(Duration::from_millis(300));
    let _ = std::fs::remove_dir_all(pc_dir());
    if !ip.is_empty() {
        let _ = crate::speaker_http(ip, "POST", "/api/settings", &json!({ "pc_on": false }).to_string());
    }
    Ok("Программа для ПК удалена (правило брандмауэра «soundya» осталось — его можно удалить в «Брандмауэр Защитника Windows»)".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_admin_script_is_encoded_for_powershell() {
        let s = admin_script("C:\\Users\\O'Neil\\AppData\\Local\\soundya\\soundya.exe", 8766);
        assert!(s.contains("-Program 'C:\\Users\\O''Neil\\AppData\\Local\\soundya\\soundya.exe'"));
        assert!(s.contains("-LocalPort 8766") && s.contains("RemoteAddress LocalSubnet"));
        assert_eq!(encoded("ab"), "YQBiAA==");
    }
}
