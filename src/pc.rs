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
        PcAction::Wake => unreachable!(),
    };
    let address = if host.contains(':') { host.to_owned() } else { format!("{host}:{PORT}") };
    let url = format!("http://{address}/{path}");
    // Straight to the PC: never through the SoundCloud proxy.
    let agent = ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(3))
        .timeout(Duration::from_secs(20))
        .build();
    let result = agent
        .post(&url)
        .set("X-Soundya-Key", key)
        .set("Content-Type", "text/plain; charset=utf-8")
        .send_string(body);
    match result {
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

/// "AA:BB:CC:DD:EE:FF", "aa-bb-cc-dd-ee-ff" or "aabbccddeeff".
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
    fn magic_packet_layout() {
        let p = magic_packet([1, 2, 3, 4, 5, 6]);
        assert_eq!(p.len(), 102);
        assert_eq!(&p[..6], &[0xFF; 6]);
        assert_eq!(&p[96..], &[1, 2, 3, 4, 5, 6]);
    }
}
