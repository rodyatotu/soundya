//! Voice control of a computer: commands go to soundya-pc running on it (Windows:
//! the `pc/` folder of this repository; Mac: soundya.app's agent,
//! installer/src/agent.rs); switching it on is a wake-on-LAN packet.

use std::{net::UdpSocket, time::Duration};

use crate::{alice::PcAction, settings::Settings};

/// soundya-pc listens here unless the address names another port.
pub const PORT: u16 = 8766;

/// soundya-pc 0.5.00+ adds this header to a refusal: it checks signatures.
const AUTH_HEADER: &str = "X-Soundya-Auth";

/// Runs a command; the text (success or error) is what Alice says.
pub fn run(settings: &Settings, action: &PcAction) -> Result<String, String> {
    if *action == PcAction::Wake {
        return wake(&settings.pc_mac).map(|()| "Включаю компьютер".to_owned());
    }
    let host = settings.pc_host.trim();
    let key = settings.pc_token.trim();
    if host.is_empty() || key.is_empty() {
        return Err("Компьютер не настроен. Впишите его адрес и ключ в настройках soundya".into());
    }
    let (path, body) = match action {
        PcAction::Ping => ("ping", ""),
        PcAction::Sleep => ("sleep", ""),
        PcAction::Shutdown => ("shutdown", ""),
        PcAction::Restart => ("restart", ""),
        PcAction::Lock => ("lock", ""),
        PcAction::Open(name) => ("open", name.as_str()),
        PcAction::Close(name) => ("close", name.as_str()),
        PcAction::Command(text) => ("do", text.as_str()),
        PcAction::Metrics => ("metrics", ""),
        PcAction::Process(name) => ("process", name.as_str()),
        PcAction::Click(spec) => ("click", spec.as_str()),
        PcAction::Key(spec) => ("key", spec.as_str()),
        PcAction::Type(text) => ("type", text.as_str()),
        PcAction::Mouse => ("mouse", ""),
        PcAction::Wake => unreachable!(),
    };
    let address = if host.contains(':') { host.to_owned() } else { format!("{host}:{PORT}") };
    let url = format!("http://{address}/{path}");
    // Straight to the PC: never through the SoundCloud proxy.
    let agent = ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(3))
        .timeout(Duration::from_secs(30))
        .build();
    let contacts = (matches!(action, PcAction::Command(_)) && !settings.pc_contacts.is_empty()).then(|| {
        let map: serde_json::Map<String, serde_json::Value> = settings
            .pc_contacts
            .iter()
            .map(|c| (c.names.clone(), serde_json::Value::String(c.target.clone())))
            .collect();
        base64(serde_json::Value::Object(map).to_string().as_bytes())
    });
    // Signed (0.5.00): the key itself never goes over the network.
    let build = |signed: bool| {
        let mut r = agent.post(&url).set("Content-Type", "text/plain; charset=utf-8");
        if signed {
            let (ts, nonce, sign) = sign(key, &format!("/{path}"), body.as_bytes());
            r = r.set("X-Soundya-Time", &ts).set("X-Soundya-Nonce", &nonce).set("X-Soundya-Sign", &sign);
        } else {
            r = r.set("X-Soundya-Key", key);
        }
        if let Some(c) = &contacts {
            r = r.set("X-Soundya-Contacts", c);
        }
        r
    };
    let mut result = build(true).send_string(body);
    // soundya-pc before 0.5.00 knows only the key: once more the old way. A newer
    // soundya-pc marks its refusal (X-Soundya-Auth: hmac): the signature itself was
    // wrong (key, clock, replay), and the key is never sent in the clear to it.
    if matches!(&result, Err(ureq::Error::Status(403, r)) if r.header(AUTH_HEADER).is_none()) {
        eprintln!("pc: the PC does not take signed commands; soundya-pc is older than 0.5.00 — update it (pc/install.cmd)");
        LEGACY.store(true, std::sync::atomic::Ordering::Release);
        result = build(false).send_string(body);
    } else if result.is_ok() {
        LEGACY.store(false, std::sync::atomic::Ordering::Release);
    }
    match result {
        Ok(r) if *action == PcAction::Metrics => {
            let text = r.into_string().map_err(|e| format!("Компьютер ответил непонятно: {e}"))?;
            let v: serde_json::Value = serde_json::from_str(&text).map_err(|_| "Программа на компьютере старая: обновите soundya-pc".to_owned())?;
            Ok(metrics_speech(&v))
        }
        Ok(r) => r.into_string().map_err(|e| format!("Компьютер ответил непонятно: {e}")),
        Err(ureq::Error::Status(403, _)) => Err("Компьютер не принял ключ. Проверьте ключ в настройках soundya".into()),
        Err(ureq::Error::Status(_, r)) => Err(r.into_string().unwrap_or_else(|_| "Компьютер не понял команду".into())),
        Err(e) => {
            eprintln!("pc: {url}: {e}");
            if matches!(&e, ureq::Error::Transport(t) if t.kind() == ureq::ErrorKind::BadHeader) {
                return Err("Ключ компьютера вписан с ошибкой. Вставьте его заново в настройках soundya".into());
            }
            Err("Компьютер не отвечает. Он включён?".into())
        }
    }
}

/// Standard base64, for sending UTF-8 text in an HTTP header.
/// The PC's numbers as JSON (for the Smart Display), signed like any command.
pub fn metrics_raw(settings: &Settings) -> Result<serde_json::Value, String> {
    let host = settings.pc_host.trim();
    let key = settings.pc_token.trim();
    if host.is_empty() || key.is_empty() {
        return Err("компьютер не настроен".into());
    }
    let address = if host.contains(':') { host.to_owned() } else { format!("{host}:{PORT}") };
    let (ts, nonce, sign) = sign(key, "/metrics", b"");
    let r = ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(3))
        .timeout(Duration::from_secs(15))
        .build()
        .post(&format!("http://{address}/metrics"))
        .set("X-Soundya-Time", &ts)
        .set("X-Soundya-Nonce", &nonce)
        .set("X-Soundya-Sign", &sign)
        .send_string("")
        .map_err(|e| e.to_string())?;
    r.into_json().map_err(|e| e.to_string())
}

/// soundya-pc answered only the old way (older than 0.5.00): the page says so.
pub static LEGACY: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// (time, nonce, signature) for a command: HMAC-SHA256 of «time\nnonce\npath\nbody»
/// with the key, as soundya-pc checks it.
/// «500 300», «500,300 double», «500 300 right» → "500,300[,double|right]"; None if not a click.
pub fn click_spec(s: &str) -> Option<String> {
    let parts: Vec<&str> = s.split([' ', ',']).filter(|p| !p.is_empty()).collect();
    if !(2..=3).contains(&parts.len()) {
        return None;
    }
    let x: u32 = parts[0].parse().ok().filter(|v| *v <= 20_000)?;
    let y: u32 = parts[1].parse().ok().filter(|v| *v <= 20_000)?;
    match parts.get(2).map(|m| m.to_lowercase()) {
        None => Some(format!("{x},{y}")),
        Some(m) if m == "double" || m == "двойной" => Some(format!("{x},{y},double")),
        Some(m) if m == "right" || m == "правый" => Some(format!("{x},{y},right")),
        _ => None,
    }
}

const KEY_MODS: [&str; 6] = ["cmd", "ctrl", "alt", "shift", "win", "option"];
const KEY_NAMES: [&str; 15] = ["enter", "esc", "tab", "space", "backspace", "delete", "up", "down", "left", "right", "home", "end", "pageup", "pagedown", "insert"];

/// «Ctrl + Shift + Esc», «cmd+t», «enter», «F5» → "ctrl+shift+esc"; None if it is not a key.
pub fn key_spec(s: &str) -> Option<String> {
    let alias = |k: &str| -> String {
        match k {
            "command" | "⌘" => "cmd",
            "control" | "ctl" => "ctrl",
            "opt" | "⌥" => "option",
            "escape" => "esc",
            "return" | "ввод" | "энтер" => "enter",
            "пробел" => "space",
            "pgup" => "pageup",
            "pgdn" => "pagedown",
            "del" => "delete",
            other => other,
        }
        .to_owned()
    };
    let parts: Vec<String> = s.to_lowercase().split('+').map(|p| alias(p.trim())).collect();
    if parts.is_empty() || parts.len() > 4 || parts.iter().any(String::is_empty) {
        return None;
    }
    let (key, mods) = parts.split_last()?;
    if mods.iter().any(|m| !KEY_MODS.contains(&m.as_str())) {
        return None;
    }
    let f_key = key.strip_prefix('f').and_then(|n| n.parse::<u8>().ok()).is_some_and(|n| (1..=12).contains(&n));
    let one = key.chars().count() == 1 && key.chars().all(|c| c.is_ascii_alphanumeric() || "-=[];',./`".contains(c));
    if !(KEY_NAMES.contains(&key.as_str()) || f_key || one) {
        return None;
    }
    Some(parts.join("+"))
}

/// Text a scenario types on the PC: one line, at most 300 characters, no control characters.
pub fn type_spec(s: &str) -> Option<String> {
    let t = s.trim();
    (!t.is_empty() && t.chars().count() <= 300 && !t.chars().any(char::is_control)).then(|| t.to_owned())
}

pub fn sign(key: &str, path: &str, body: &[u8]) -> (String, String, String) {
    let ts = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0).to_string();
    let mut n = [0u8; 8];
    if let Ok(mut f) = std::fs::File::open("/dev/urandom") {
        let _ = std::io::Read::read_exact(&mut f, &mut n);
    }
    let nonce = crate::sha256::hex(&n);
    (ts.clone(), nonce.clone(), sign_with(key, &ts, &nonce, path, body))
}

pub fn sign_with(key: &str, ts: &str, nonce: &str, path: &str, body: &[u8]) -> String {
    let mut msg = format!("{ts}\n{nonce}\n{path}\n").into_bytes();
    msg.extend_from_slice(body);
    crate::sha256::hex(&crate::sha256::hmac_sha256(key.as_bytes(), &msg))
}

/// The PC's numbers as a sentence for Alice.
pub fn metrics_speech(v: &serde_json::Value) -> String {
    let n = |k: &str| v.get(k).and_then(serde_json::Value::as_i64);
    let mut parts = Vec::new();
    match (n("cpu"), n("cpu_temp")) {
        (Some(l), Some(t)) => parts.push(format!("Процессор загружен на {l} процентов, {t} градусов")),
        (Some(l), None) => parts.push(format!("Процессор загружен на {l} процентов")),
        (None, Some(t)) => parts.push(format!("Процессор: {t} градусов")),
        _ => {}
    }
    match (n("gpu"), n("gpu_temp")) {
        (Some(l), Some(t)) => parts.push(format!("видеокарта — {l} процентов, {t} градусов")),
        (Some(l), None) => parts.push(format!("видеокарта — {l} процентов")),
        (None, Some(t)) => parts.push(format!("видеокарта — {t} градусов")),
        _ => {}
    }
    if let Some(r) = n("ram_used") {
        parts.push(format!("память занята на {r} процентов"));
    }
    if parts.is_empty() {
        return "Компьютер на связи, но не рассказал о себе".into();
    }
    let mut s = parts.join(", ") + ".";
    // A Mac gives temperatures only to root: nothing to advise there.
    if n("cpu_temp").is_none() && n("gpu_temp").is_none() && v["os"] != "macos" {
        s.push_str(" Температуру Windows не отдаёт: поставьте на компьютер LibreHardwareMonitor.");
    }
    s
}

pub(crate) fn base64(data: &[u8]) -> String {
    const ABC: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(ABC[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// "AA:BB:CC:DD:EE:FF", "aa-bb-cc-dd-ee-ff" or "aabbccddeeff".
/// Standard base64 back to bytes (the page's screenshot); None if malformed.
pub(crate) fn unbase64(text: &str) -> Option<Vec<u8>> {
    let text = text.split_once("base64,").map_or(text, |(_, b)| b);
    let mut out = Vec::with_capacity(text.len() * 3 / 4);
    let mut acc = 0u32;
    let mut bits = 0;
    for c in text.bytes() {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' | b'-' => 62,
            b'/' | b'_' => 63,
            b'=' => break,
            b'\r' | b'\n' | b' ' => continue,
            _ => return None,
        };
        acc = (acc << 6) | u32::from(v);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
            acc &= (1 << bits) - 1;
        }
    }
    Some(out)
}

pub fn parse_mac(text: &str) -> Option<[u8; 6]> {
    let hex: String = text.chars().filter(|c| !matches!(c, ':' | '-' | '.' | ' ')).collect();
    if hex.len() != 12 || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let mut out = [0u8; 6];
    for (i, b) in out.iter_mut().enumerate() {
        *b = u8::from_str_radix(&hex[2 * i..2 * i + 2], 16).ok()?;
    }
    Some(out)
}

/// The wake-on-LAN "magic packet": six 0xFF bytes, then the MAC address 16 times.
fn magic_packet(mac: [u8; 6]) -> Vec<u8> {
    let mut packet = vec![0xFF; 6];
    for _ in 0..16 {
        packet.extend_from_slice(&mac);
    }
    packet
}

pub fn wake(mac: &str) -> Result<(), String> {
    let mac = parse_mac(mac).ok_or("Не задан MAC-адрес компьютера. Впишите его в настройках soundya")?;
    let packet = magic_packet(mac);
    let socket = UdpSocket::bind("0.0.0.0:0").map_err(|e| format!("Не получилось отправить сигнал: {e}"))?;
    socket.set_broadcast(true).map_err(|e| format!("Не получилось отправить сигнал: {e}"))?;
    // Ports 9 and 7 are both used for wake-on-LAN; a few copies in case one is lost.
    for _ in 0..3 {
        for port in [9u16, 7] {
            let _ = socket.send_to(&packet, ("255.255.255.255", port));
        }
    }
    eprintln!("pc: wake-on-LAN sent to {}", fmt_mac(mac));
    Ok(())
}

fn fmt_mac(mac: [u8; 6]) -> String {
    mac.iter().map(|b| format!("{b:02X}")).collect::<Vec<_>>().join(":")
}

#[cfg(test)]
mod tests {
    #[test]
    fn clicks_and_keys_for_scenarios() {
        use super::{click_spec, key_spec};
        assert_eq!(click_spec("500 300").as_deref(), Some("500,300"));
        assert_eq!(click_spec("500,300 double").as_deref(), Some("500,300,double"));
        assert_eq!(click_spec(" 10 20 правый ").as_deref(), Some("10,20,right"));
        for bad in ["", "500", "a b", "-1 5", "500 300 triple", "99999 1", "1 2 3 4"] {
            assert!(click_spec(bad).is_none(), "{bad}");
        }
        assert_eq!(key_spec("Ctrl + Shift + Esc").as_deref(), Some("ctrl+shift+esc"));
        assert_eq!(key_spec("command+t").as_deref(), Some("cmd+t"));
        assert_eq!(key_spec("F5").as_deref(), Some("f5"));
        assert_eq!(type_spec("  привет, мир ").as_deref(), Some("привет, мир"));
        assert_eq!(type_spec(""), None);
        assert_eq!(type_spec("две\nстроки"), None);
        assert_eq!(type_spec(&"а".repeat(301)), None);
        assert_eq!(key_spec("ввод").as_deref(), Some("enter"));
        for bad in ["", "cmd+", "hello", "cmd+ctrl", "f13", "rm -rf", "cmd+t+x", "a\"; do shell script \"x"] {
            assert!(key_spec(bad).is_none(), "{bad}");
        }
    }

    use super::*;
    use std::io::{BufRead, BufReader, Write};

    /// A soundya-pc stand-in on 127.0.0.1: answers each connection with the next
    /// of `answers` (status line and extra headers) and keeps every request's headers.
    fn fake_agent(answers: Vec<(&'static str, &'static str)>) -> (String, std::thread::JoinHandle<Vec<Vec<String>>>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let handle = std::thread::spawn(move || {
            listener.set_nonblocking(false).unwrap();
            let mut seen = Vec::new();
            for (status, extra) in answers {
                let (stream, _) = listener.accept().unwrap();
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut headers = Vec::new();
                let mut length = 0usize;
                loop {
                    let mut line = String::new();
                    reader.read_line(&mut line).unwrap();
                    let line = line.trim_end().to_owned();
                    if line.is_empty() {
                        break;
                    }
                    if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                        length = v.trim().parse().unwrap_or(0);
                    }
                    headers.push(line);
                }
                let mut body = vec![0u8; length];
                std::io::Read::read_exact(&mut reader, &mut body).unwrap();
                let text = "ответ";
                let mut out = stream;
                // The test's own «unblock» connection may be gone already: not an error.
                let _ = write!(out, "HTTP/1.1 {status}\r\n{extra}Content-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{text}", text.len());
                seen.push(headers);
            }
            seen
        });
        (addr, handle)
    }

    fn settings_for(addr: &str) -> Settings {
        Settings { pc_host: addr.to_owned(), pc_token: "secret-key-123".into(), ..Settings::default() }
    }

    fn has_plain_key(headers: &[String]) -> bool {
        headers.iter().any(|h| h.to_ascii_lowercase().starts_with("x-soundya-key:") || h.contains("secret-key-123"))
    }

    #[test]
    fn refused_signature_never_sends_the_key() {
        // soundya-pc 0.5.00+ refused the signature (wrong key, clock off, replay):
        // the key must not go over the network in the clear as a second try.
        let (addr, agent) = fake_agent(vec![("403 Forbidden", "X-Soundya-Auth: hmac\r\n"), ("200 OK", "")]);
        let result = run(&settings_for(&addr), &PcAction::Lock);
        // Unblock the fake if no second request came.
        let _ = std::net::TcpStream::connect(&addr).map(|mut s| s.write_all(b"GET / HTTP/1.1\r\n\r\n"));
        let seen = agent.join().unwrap();
        assert!(seen[0].iter().any(|h| h.to_ascii_lowercase().starts_with("x-soundya-sign:")));
        assert!(!has_plain_key(&seen[0]));
        assert!(!has_plain_key(&seen[1]), "the key was sent in the clear after a refused signature");
        assert!(result.is_err(), "{result:?}");
    }

    #[test]
    fn old_agent_still_gets_the_key() {
        // soundya-pc before 0.5.00 refuses any signature without saying why: it
        // knows only the key, so the old way still works for it.
        let (addr, agent) = fake_agent(vec![("403 Forbidden", ""), ("200 OK", "")]);
        let result = run(&settings_for(&addr), &PcAction::Lock);
        let seen = agent.join().unwrap();
        assert_eq!(result.as_deref(), Ok("ответ"));
        assert!(!has_plain_key(&seen[0]));
        assert!(has_plain_key(&seen[1]));
    }

    #[test]
    fn signature_matches_the_agent() {
        // The same as soundya-pc computes in PowerShell (HMACSHA256 over UTF-8),
        // checked independently: python3 -c "import hmac,hashlib;print(hmac.new(b'k3y',
        // b'1700000000\nabcd\n/open\n\xd1\x82\xd0\xb5\xd0\xbb\xd0\xb5\xd0\xb3\xd1\x80\xd0\xb0\xd0\xbc',hashlib.sha256).hexdigest())"
        assert_eq!(
            sign_with("k3y", "1700000000", "abcd", "/open", "телеграм".as_bytes()),
            "6b3843f5bdb061ace24a63cbc6497128a7bcba596b9d458fa15ee1c488345c48"
        );
        let (ts, nonce, s) = sign("k3y", "/ping", b"");
        assert_eq!(s, sign_with("k3y", &ts, &nonce, "/ping", b""));
        assert_eq!(nonce.len(), 16);
    }

    #[test]
    fn metrics_as_speech() {
        let v = serde_json::json!({"cpu": 23, "cpu_temp": 54, "gpu": 10, "gpu_temp": 48, "ram_used": 61});
        assert_eq!(metrics_speech(&v), "Процессор загружен на 23 процентов, 54 градусов, видеокарта — 10 процентов, 48 градусов, память занята на 61 процентов.");
        assert!(metrics_speech(&serde_json::json!({"cpu": 5})).contains("LibreHardwareMonitor"));
        // A Mac (soundya.app's agent) has no temperatures without root: no Windows advice.
        assert_eq!(
            metrics_speech(&serde_json::json!({"os": "macos", "cpu": 5, "ram_used": 40})),
            "Процессор загружен на 5 процентов, память занята на 40 процентов."
        );
    }

    #[test]
    fn base64_round_trip() {
        for data in [&b""[..], b"a", b"ab", b"abc", b"\x00\xff\x10 hello world"] {
            assert_eq!(unbase64(&base64(data)).unwrap(), data);
        }
        assert_eq!(unbase64("data:image/png;base64,aGk=").unwrap(), b"hi");
        assert!(unbase64("a$b").is_none());
    }

    #[test]
    fn mac_formats() {
        let want = [0xAA, 0xBB, 0xCC, 0x01, 0x02, 0x03];
        assert_eq!(parse_mac("AA:BB:CC:01:02:03"), Some(want));
        assert_eq!(parse_mac("aa-bb-cc-01-02-03"), Some(want));
        assert_eq!(parse_mac("aabbcc010203"), Some(want));
        assert_eq!(parse_mac("aa:bb:cc:01:02"), None);
        assert_eq!(parse_mac("zz:bb:cc:01:02:03"), None);
        assert_eq!(parse_mac(""), None);
        assert_eq!(fmt_mac(want), "AA:BB:CC:01:02:03");
    }

    #[test]
    fn base64_matches_the_standard() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64("мама".as_bytes()), "0LzQsNC80LA=");
    }

    #[test]
    fn magic_packet_layout() {
        let p = magic_packet([1, 2, 3, 4, 5, 6]);
        assert_eq!(p.len(), 102);
        assert_eq!(&p[..6], &[0xFF; 6]);
        assert_eq!(&p[96..], &[1, 2, 3, 4, 5, 6]);
    }
}

// ---------------------------------------------------------------- computers that say hello

/// soundya on a computer (Mac agent, Windows app) tells the speaker it is there: POST
/// /api/pc/hello. The panel lists those not set up yet («Найдены компьютеры» → «Добавить»), so
/// nothing is typed and the key never passes through a browser. Kept in memory only.
#[derive(Debug, Clone, serde::Deserialize, PartialEq, Eq)]
pub struct Hello {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub os: String,
    #[serde(default)]
    pub port: u16,
    #[serde(default)]
    pub mac: String,
    #[serde(default)]
    pub key: String,
}

/// (hello, the address it came from, when: unix s).
static SEEN: std::sync::Mutex<Vec<(Hello, String, u64)>> = std::sync::Mutex::new(Vec::new());

fn unix_now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

fn valid_mac(m: &str) -> bool {
    m.is_empty() || (m.len() == 17 && m.split([':', '-']).count() == 6 && m.split([':', '-']).all(|p| p.len() == 2 && p.chars().all(|c| c.is_ascii_hexdigit())))
}

/// Takes a hello from `ip` (the connection's own address: a computer cannot announce another).
pub fn hello(h: Hello, ip: &str) -> Result<(), String> {
    if h.key.trim().is_empty() || h.key.len() > 128 || h.name.chars().count() > 80 || !valid_mac(h.mac.trim()) {
        return Err("неверные данные компьютера".into());
    }
    let h = Hello { name: h.name.trim().to_owned(), os: h.os.trim().chars().take(12).collect(), port: if h.port == 0 { PORT } else { h.port }, mac: h.mac.trim().to_owned(), key: h.key.trim().to_owned() };
    let mut seen = SEEN.lock().unwrap();
    seen.retain(|(_, a, _)| a != ip);
    seen.push((h, ip.to_owned(), unix_now()));
    // A day without a word: gone (switched off, another network).
    let old = unix_now().saturating_sub(24 * 3600);
    seen.retain(|(_, _, t)| *t >= old);
    if seen.len() > 16 {
        seen.remove(0);
    }
    Ok(())
}

fn host_of(h: &Hello, ip: &str) -> String {
    if h.port == PORT { ip.to_owned() } else { format!("{ip}:{}", h.port) }
}

fn same_pc(host: &str, mac: &str, ip: &str, h: &Hello) -> bool {
    host.split(':').next() == Some(ip) || (!h.mac.is_empty() && mac.eq_ignore_ascii_case(&h.mac))
}

/// For the panel (no keys): every computer that said hello, and whether it is set up with its current key.
pub fn found_json(s: &Settings) -> serde_json::Value {
    let seen = SEEN.lock().unwrap();
    serde_json::Value::Array(
        seen.iter()
            .rev()
            .map(|(h, ip, at)| {
                let first = same_pc(&s.pc_host, &s.pc_mac, ip, h).then(|| (s.pc_host.clone(), s.pc_token.clone()));
                let set = first.or_else(|| s.pcs_more.iter().find(|d| same_pc(&d.host, &d.mac, ip, h)).map(|d| (d.host.clone(), d.token.clone())));
                let added = set.as_ref().is_some_and(|(host, token)| *host == host_of(h, ip) && *token == h.key);
                serde_json::json!({ "name": h.name, "os": h.os, "ip": ip, "mac": h.mac, "at": at, "added": added, "known": set.is_some() })
            })
            .collect(),
    )
}

/// «Добавить» on the panel: that computer goes into the settings — over its own old entry
/// (same address or MAC), else first if there is none yet, else into «Ещё компьютеры».
pub fn adopt(s: &mut Settings, ip: &str) -> Result<String, String> {
    let (h, ip) = SEEN.lock().unwrap().iter().find(|(_, a, _)| a == ip).map(|(h, a, _)| (h.clone(), a.clone())).ok_or("этот компьютер давно не выходил на связь — откройте на нём soundya")?;
    let host = host_of(&h, &ip);
    let label = if h.os == "mac" { "мак" } else { "компьютер" };
    if s.pc_host.trim().is_empty() || same_pc(&s.pc_host, &s.pc_mac, &ip, &h) {
        s.pc_host = host.clone();
        s.pc_token = h.key.clone();
        if !h.mac.is_empty() {
            s.pc_mac = h.mac.clone();
        }
        if s.pc_name.trim().is_empty() {
            s.pc_name = label.into();
        }
        return Ok(format!("OK: {} ({host}) — основной компьютер", if h.name.is_empty() { label } else { &h.name }));
    }
    if let Some(d) = s.pcs_more.iter_mut().find(|d| same_pc(&d.host, &d.mac, &ip, &h)) {
        d.host = host.clone();
        d.token = h.key.clone();
        if !h.mac.is_empty() {
            d.mac = h.mac.clone();
        }
        return Ok(format!("OK: {} ({host}) обновлён", d.name));
    }
    let taken: Vec<String> = std::iter::once(s.pc_name.clone()).chain(s.pcs_more.iter().map(|d| d.name.clone())).map(|n| n.trim().to_lowercase()).collect();
    let name = if taken.iter().any(|n| n == label) { h.name.to_lowercase() } else { label.to_owned() };
    s.pcs_more.push(crate::settings::PcDevice { name: name.clone(), host: host.clone(), token: h.key.clone(), mac: h.mac.clone() });
    Ok(format!("OK: {name} ({host}) добавлен"))
}

#[cfg(test)]
mod hello_tests {
    use super::*;

    #[test]
    fn a_computer_says_hello_and_is_added() {
        let h = |name: &str, os: &str, mac: &str| Hello { name: name.into(), os: os.into(), port: 0, mac: mac.into(), key: format!("k-{name}") };
        assert!(hello(Hello { key: String::new(), ..h("x", "mac", "") }, "10.9.9.9").is_err());
        assert!(hello(h("x", "mac", "zz"), "10.9.9.9").is_err(), "bad MAC");
        hello(h("MacBook", "mac", "46:a5:2f:8b:ba:c7"), "10.9.9.82").unwrap();
        hello(h("DESKTOP", "windows", "D8:43:AE:19:42:AB"), "10.9.9.100").unwrap();
        let mut s = Settings::default();
        assert!(adopt(&mut s, "10.9.9.82").unwrap().contains("основной"));
        assert_eq!((s.pc_host.as_str(), s.pc_token.as_str(), s.pc_name.as_str()), ("10.9.9.82", "k-MacBook", "мак"));
        // The Windows PC was there before under another address: updated by its MAC, not added twice.
        s.pcs_more.push(crate::settings::PcDevice { name: "компьютер".into(), host: "10.9.9.50".into(), token: "old".into(), mac: "d8:43:ae:19:42:ab".into() });
        adopt(&mut s, "10.9.9.100").unwrap();
        assert_eq!(s.pcs_more.len(), 1);
        assert_eq!((s.pcs_more[0].host.as_str(), s.pcs_more[0].token.as_str()), ("10.9.9.100", "k-DESKTOP"));
        let found = found_json(&s);
        assert!(found.as_array().unwrap().iter().filter(|f| f["ip"].as_str().unwrap().starts_with("10.9.9.")).all(|f| f["added"] == true));
        assert!(!found.to_string().contains("k-MacBook"), "no keys to the panel");
        assert!(adopt(&mut s, "10.9.9.7").is_err());
    }
}
