//! Wi-Fi from the web panel: which network, the ones around, switching to another.
//! The Station runs a plain wpa_supplicant (`wpa_cli -i wlan0`, update_config=1) and dhcpcd.
//! A switch cuts the panel's own connection, so it runs on its own thread and is safe: a
//! network that does not connect within CONNECT_WAIT is dropped and the speaker goes back to
//! the one it was on. Nothing is saved (save_config) until the new network works.
//! The password never goes to a log or a command line seen by `ps` longer than wpa_cli runs.

use serde_json::{json, Value};
use std::{
    process::{Command, Stdio},
    sync::Mutex,
    thread,
    time::{Duration, Instant},
};

const IFACE: &str = "wlan0";
const CONNECT_WAIT: Duration = Duration::from_secs(30);
const BACK_WAIT: Duration = Duration::from_secs(25);

/// The last switch: what the page shows after it reconnects.
#[derive(Clone, Default)]
struct Job {
    state: &'static str, // "", "connecting", "done", "failed"
    ssid: String,
    message: String,
    at: Option<Instant>,
}

static JOB: Mutex<Option<Job>> = Mutex::new(None);

fn wpa(args: &[&str]) -> Option<String> {
    let out = Command::new("wpa_cli")
        .arg("-i")
        .arg(IFACE)
        .args(args)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

fn ok(args: &[&str]) -> bool {
    wpa(args).is_some_and(|o| o.trim() == "OK")
}

/// wpa_cli prints SSIDs escaped (printf_encode): \xNN for every non-ASCII byte, \\ \" \e \n \r \t.
pub fn unescape(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'\\' && i + 1 < b.len() {
            match b[i + 1] {
                b'x' if i + 3 < b.len() => {
                    if let Ok(v) = u8::from_str_radix(&s[i + 2..i + 4], 16) {
                        out.push(v);
                        i += 4;
                        continue;
                    }
                    out.push(b'\\');
                    i += 1;
                    continue;
                }
                b'n' => out.push(b'\n'),
                b'r' => out.push(b'\r'),
                b't' => out.push(b'\t'),
                b'e' => out.push(0x1b),
                c => out.push(c),
            }
            i += 2;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex(s: &str) -> String {
    s.bytes().map(|b| format!("{b:02x}")).collect()
}

/// RSSI (dBm) as 0–4 bars, the way phones show it.
pub fn bars(rssi: i32) -> u8 {
    match rssi {
        r if r >= -55 => 4,
        r if r >= -67 => 3,
        r if r >= -75 => 2,
        r if r >= -85 => 1,
        _ => 0,
    }
}

pub fn parse_kv(text: &str) -> std::collections::HashMap<String, String> {
    text.lines().filter_map(|l| l.split_once('=')).map(|(k, v)| (k.trim().to_owned(), v.trim().to_owned())).collect()
}

#[derive(Debug, Clone, PartialEq)]
pub struct Saved {
    pub id: String,
    pub ssid: String,
    pub current: bool,
}

pub fn parse_saved(text: &str) -> Vec<Saved> {
    text.lines()
        .skip(1)
        .filter_map(|l| {
            let mut f = l.split('\t');
            let id = f.next()?.trim().to_owned();
            let ssid = unescape(f.next()?);
            let _bssid = f.next();
            let flags = f.next().unwrap_or("");
            (!id.is_empty() && id.chars().all(|c| c.is_ascii_digit())).then(|| Saved { id, ssid, current: flags.contains("CURRENT") })
        })
        .collect()
}

#[derive(Debug, Clone, PartialEq)]
pub struct Seen {
    pub ssid: String,
    pub rssi: i32,
    pub freq: u32,
    pub secure: bool,
}

/// scan_results → one row per network name, the strongest access point of it.
pub fn parse_scan(text: &str) -> Vec<Seen> {
    let mut by: Vec<Seen> = Vec::new();
    for l in text.lines().skip(1) {
        let f: Vec<&str> = l.split('\t').collect();
        if f.len() < 5 {
            continue;
        }
        let ssid = unescape(f[4]);
        if ssid.trim().is_empty() || ssid.contains('\0') {
            continue; // hidden networks
        }
        let rssi = f[2].trim().parse().unwrap_or(-100);
        let freq = f[1].trim().parse().unwrap_or(0);
        let secure = f[3].contains("WPA") || f[3].contains("WEP") || f[3].contains("SAE");
        match by.iter_mut().find(|s| s.ssid == ssid) {
            Some(s) if s.rssi >= rssi => {}
            Some(s) => *s = Seen { ssid, rssi, freq, secure },
            None => by.push(Seen { ssid, rssi, freq, secure }),
        }
    }
    by.sort_by(|a, b| b.rssi.cmp(&a.rssi));
    by
}

fn band(freq: u32) -> &'static str {
    if freq >= 4900 { "5 ГГц" } else if freq > 0 { "2,4 ГГц" } else { "" }
}

pub fn available() -> bool {
    wpa(&["ping"]).is_some_and(|o| o.contains("PONG"))
}

fn job_json() -> Value {
    let g = JOB.lock().unwrap();
    match g.as_ref() {
        Some(j) if j.at.is_some_and(|t| t.elapsed() < Duration::from_secs(600)) => {
            json!({ "state": j.state, "ssid": j.ssid, "message": j.message })
        }
        _ => Value::Null,
    }
}

/// Now: the network, the address, the signal, the saved networks and the last switch.
pub fn status() -> Value {
    if !available() {
        return json!({ "available": false });
    }
    let st = parse_kv(&wpa(&["status"]).unwrap_or_default());
    let rssi: Option<i32> = wpa(&["signal_poll"]).and_then(|t| parse_kv(&t).get("RSSI").and_then(|v| v.parse().ok()));
    let freq: u32 = st.get("freq").and_then(|v| v.parse().ok()).unwrap_or(0);
    let connected = st.get("wpa_state").is_some_and(|s| s == "COMPLETED");
    let saved = parse_saved(&wpa(&["list_networks"]).unwrap_or_default());
    json!({
        "available": true,
        "connected": connected,
        "ssid": st.get("ssid").map(|s| unescape(s)),
        "ip": st.get("ip_address"),
        "band": band(freq),
        "rssi": rssi,
        "bars": rssi.map(bars),
        "saved": saved.iter().map(|s| json!({ "id": s.id, "ssid": s.ssid, "current": s.current })).collect::<Vec<_>>(),
        "job": job_json(),
    })
}

/// The networks around (a scan takes 2–4 s).
pub fn scan() -> Value {
    if !available() {
        return json!({ "error": "Wi-Fi на этой колонке недоступен для soundya" });
    }
    let _ = wpa(&["scan"]);
    let t0 = Instant::now();
    let mut seen = Vec::new();
    while t0.elapsed() < Duration::from_secs(5) {
        thread::sleep(Duration::from_millis(700));
        seen = parse_scan(&wpa(&["scan_results"]).unwrap_or_default());
        if t0.elapsed() > Duration::from_millis(2800) && !seen.is_empty() {
            break;
        }
    }
    let saved = parse_saved(&wpa(&["list_networks"]).unwrap_or_default());
    let current = saved.iter().find(|s| s.current).map(|s| s.ssid.clone());
    json!({
        "networks": seen.iter().map(|s| json!({
            "ssid": s.ssid,
            "rssi": s.rssi,
            "bars": bars(s.rssi),
            "band": band(s.freq),
            "secure": s.secure,
            "saved": saved.iter().any(|x| x.ssid == s.ssid),
            "current": current.as_deref() == Some(s.ssid.as_str()),
        })).collect::<Vec<_>>()
    })
}

/// A password wpa_supplicant takes: 8–63 printable characters, or 64 hex digits.
pub fn valid_psk(p: &str) -> bool {
    let n = p.chars().count();
    (n == 64 && p.chars().all(|c| c.is_ascii_hexdigit())) || ((8..=63).contains(&n) && p.chars().all(|c| !c.is_control()))
}

fn set_job(state: &'static str, ssid: &str, message: String) {
    *JOB.lock().unwrap() = Some(Job { state, ssid: ssid.to_owned(), message, at: Some(Instant::now()) });
}

fn wait_connected(ssid: &str, limit: Duration) -> Option<String> {
    let t0 = Instant::now();
    while t0.elapsed() < limit {
        thread::sleep(Duration::from_millis(500));
        let st = parse_kv(&wpa(&["status"]).unwrap_or_default());
        if st.get("wpa_state").is_some_and(|s| s == "COMPLETED") && st.get("ssid").is_some_and(|s| unescape(s) == ssid) {
            if let Some(ip) = st.get("ip_address").filter(|ip| !ip.is_empty() && !ip.starts_with("169.254.")) {
                return Some(ip.clone());
            }
        }
    }
    None
}

/// Starts a switch to `ssid` (a saved one: `psk` None) and returns at once.
pub fn connect(ssid: &str, psk: Option<&str>) -> Result<String, String> {
    if !available() {
        return Err("Wi-Fi на этой колонке недоступен для soundya".into());
    }
    if ssid.is_empty() || ssid.len() > 32 {
        return Err("Имя сети — от 1 до 32 байт".into());
    }
    let psk = psk.map(str::to_owned).filter(|p| !p.is_empty());
    if let Some(p) = &psk {
        if !valid_psk(p) {
            return Err("Пароль Wi-Fi — от 8 до 63 символов".into());
        }
    }
    {
        let g = JOB.lock().unwrap();
        if g.as_ref().is_some_and(|j| j.state == "connecting" && j.at.is_some_and(|t| t.elapsed() < Duration::from_secs(90))) {
            return Err("Уже переключаюсь, подождите полминуты".into());
        }
    }
    let saved = parse_saved(&wpa(&["list_networks"]).unwrap_or_default());
    let prev = saved.iter().find(|s| s.current).cloned();
    if prev.as_ref().is_some_and(|p| p.ssid == ssid) && psk.is_none() {
        return Ok(format!("OK: колонка уже в сети «{ssid}»"));
    }
    let known = saved.iter().find(|s| s.ssid == ssid).cloned();
    set_job("connecting", ssid, String::new());
    let ssid = ssid.to_owned();
    thread::spawn(move || switch(&ssid, psk, known, prev));
    Ok("OK: переключаюсь".into())
}

fn switch(ssid: &str, psk: Option<String>, known: Option<Saved>, prev: Option<Saved>) {
    // A new entry for a new network or a new password; the saved one otherwise.
    let (id, added) = if psk.is_none() && known.is_some() {
        (known.clone().unwrap().id, false)
    } else {
        let Some(id) = wpa(&["add_network"]).map(|o| o.trim().to_owned()).filter(|o| o.chars().all(|c| c.is_ascii_digit()) && !o.is_empty()) else {
            set_job("failed", ssid, "wpa_supplicant не принял новую сеть".into());
            return;
        };
        let mut good = ok(&["set_network", &id, "ssid", &hex(ssid)]) && ok(&["set_network", &id, "scan_ssid", "1"]);
        good &= match &psk {
            Some(p) if p.len() == 64 && p.chars().all(|c| c.is_ascii_hexdigit()) => ok(&["set_network", &id, "psk", p]),
            Some(p) => ok(&["set_network", &id, "psk", &format!("\"{p}\"")]),
            None => ok(&["set_network", &id, "key_mgmt", "NONE"]),
        };
        if !good {
            let _ = wpa(&["remove_network", &id]);
            set_job("failed", ssid, "Не получилось записать сеть (проверьте пароль)".into());
            return;
        }
        (id, true)
    };
    eprintln!("wifi: switching to «{ssid}»");
    let _ = wpa(&["select_network", &id]);
    if let Some(ip) = wait_connected(ssid, CONNECT_WAIT) {
        let _ = wpa(&["enable_network", "all"]);
        // A new password for a saved network: the old entry goes.
        if added {
            if let Some(k) = known.filter(|k| k.id != id) {
                let _ = wpa(&["remove_network", &k.id]);
            }
        }
        let _ = wpa(&["save_config"]);
        eprintln!("wifi: on «{ssid}», {ip}");
        set_job("done", ssid, format!("Колонка в сети «{ssid}», адрес {ip}"));
        return;
    }
    // Did not connect: back to where it was, nothing saved.
    eprintln!("wifi: «{ssid}» did not connect; going back");
    if added {
        let _ = wpa(&["remove_network", &id]);
    }
    if let Some(p) = &prev {
        let _ = wpa(&["select_network", &p.id]);
    }
    let _ = wpa(&["enable_network", "all"]);
    let back = prev.as_ref().and_then(|p| wait_connected(&p.ssid, BACK_WAIT).map(|_| p.ssid.clone()));
    if back.is_none() {
        let _ = wpa(&["reassociate"]);
    }
    let why = if psk.is_some() { "неверный пароль или сеть слишком далеко" } else { "сеть не ответила" };
    set_job(
        "failed",
        ssid,
        match back {
            Some(b) => format!("Не подключилась к «{ssid}»: {why}. Колонка вернулась в «{b}»"),
            None => format!("Не подключилась к «{ssid}»: {why}"),
        },
    );
}

/// Removes a saved network (not the one in use).
pub fn forget(id: &str) -> Result<String, String> {
    if id.is_empty() || !id.chars().all(|c| c.is_ascii_digit()) {
        return Err("нет такой сети".into());
    }
    let saved = parse_saved(&wpa(&["list_networks"]).unwrap_or_default());
    let Some(s) = saved.iter().find(|s| s.id == id) else { return Err("нет такой сети".into()) };
    if s.current {
        return Err("Это текущая сеть: сначала переключитесь на другую".into());
    }
    if !(ok(&["remove_network", id]) && ok(&["save_config"])) {
        return Err("wpa_supplicant не удалил сеть".into());
    }
    Ok(format!("OK: сеть «{}» забыта", s.ssid))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_wpa_cli() {
        assert_eq!(unescape(r"NO ROYALTY"), "NO ROYALTY");
        assert_eq!(unescape(r"\xd0\x94\xd0\xbe\xd0\xbc"), "Дом");
        assert_eq!(unescape(r#"a\"b\\c"#), "a\"b\\c");
        let saved = parse_saved("network id / ssid / bssid / flags\n0\tNO ROYALTY\tany\t[CURRENT]\n1\t\\xd0\\x94\\xd0\\xbe\\xd0\\xbc\tany\t[DISABLED]\n");
        assert_eq!(saved, vec![
            Saved { id: "0".into(), ssid: "NO ROYALTY".into(), current: true },
            Saved { id: "1".into(), ssid: "Дом".into(), current: false },
        ]);
        let scan = parse_scan(
            "bssid / frequency / signal level / flags / ssid\n\
             aa:aa:aa:aa:aa:01\t2437\t-70\t[WPA2-PSK-CCMP][ESS]\tHome\n\
             aa:aa:aa:aa:aa:02\t5240\t-48\t[WPA2-PSK-CCMP][ESS]\tHome\n\
             aa:aa:aa:aa:aa:03\t2412\t-80\t[ESS]\tCafe\n\
             aa:aa:aa:aa:aa:04\t2412\t-60\t[WPA2-PSK-CCMP][ESS]\t\n",
        );
        assert_eq!(scan.len(), 2, "one row per name, hidden ones skipped");
        assert_eq!((scan[0].ssid.as_str(), scan[0].rssi, scan[0].freq), ("Home", -48, 5240));
        assert!(!scan[1].secure);
        assert_eq!(bars(-48), 4);
        assert_eq!(bars(-80), 1);
        assert_eq!(parse_kv("wpa_state=COMPLETED\nip_address=192.168.0.2\n")["ip_address"], "192.168.0.2");
        assert_eq!(hex("Дом"), "d094d0bed0bc");
        assert!(valid_psk("12345678") && !valid_psk("1234567") && !valid_psk("a\nbcdefgh"));
    }
}
