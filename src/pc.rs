//! Voice control of a Windows PC: commands go to soundya-pc (the `pc/` folder of
//! this repository) running on the PC; switching it on is a wake-on-LAN packet.

use std::{net::UdpSocket, time::Duration};

use crate::{alice::PcAction, settings::Settings};

/// soundya-pc listens here unless the address names another port.
pub const PORT: u16 = 8766;

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
    // soundya-pc before 0.5.00 knows only the key: once more the old way.
    if matches!(&result, Err(ureq::Error::Status(403, _))) {
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
    if n("cpu_temp").is_none() && n("gpu_temp").is_none() {
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
    use super::*;

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
