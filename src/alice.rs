//! Local Alice (Quasar) IPC on 127.0.0.1:9875.
//!
//! The wire format and message numbers are taken from spotiya
//! (https://github.com/klockky/spotiya, MIT): frames are a u32 little-endian
//! length followed by a protobuf-like QuasarMessage {252: payload, 253: type}.

use std::{
    collections::VecDeque,
    io::{self, Read, Write},
    net::{SocketAddr, TcpStream},
    sync::mpsc::Sender,
    thread,
    time::Duration,
};

use crate::{Cmd, Shared};

pub const ADDRESS: &str = "127.0.0.1:9875";
const MAX_FRAME: usize = 512 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Intent {
    Play(String),
    Playlist(String),
    Likes,
    MyPlaylist(String),
    Resume,
    Pause,
    Next,
    Previous,
}

fn normalize(text: &str) -> String {
    let text = text
        .trim()
        .trim_matches(['.', '!', '?'])
        .to_lowercase()
        .replace('ё', "е");
    text.strip_prefix("алиса, ")
        .or_else(|| text.strip_prefix("алиса "))
        .unwrap_or(&text)
        .trim()
        .to_owned()
}

/// Removes "в саундклауде", "на soundcloud" and similar; returns whether it was present.
pub fn strip_service(text: &str) -> (String, bool) {
    const NAMES: [&str; 8] = [
        "саундклауде",
        "саундклауда",
        "саундклауд",
        "саунд клауде",
        "саунд клауда",
        "саунд клауд",
        "soundcloud",
        "sound cloud",
    ];
    let mut out = format!(" {text} ");
    let mut found = false;
    for name in NAMES {
        for prep in [" в ", " на ", " из ", " с ", " "] {
            let needle = format!("{prep}{name} ");
            if out.contains(&needle) {
                out = out.replace(&needle, " ");
                found = true;
            }
        }
    }
    (out.split_whitespace().collect::<Vec<_>>().join(" "), found)
}

/// `keyword_only`: intercept music requests only when SoundCloud is named.
pub fn intent(text: &str, keyword_only: bool) -> Option<Intent> {
    let (text, named) = strip_service(&normalize(text));
    match text.as_str() {
        "стоп" | "пауза" | "останови музыку" | "выключи музыку" | "поставь на паузу"
        | "останови" | "хватит" => return Some(Intent::Pause),
        "дальше" | "следующий" | "следующий трек" | "следующая песня" | "переключи трек"
        | "переключи" => return Some(Intent::Next),
        "назад" | "предыдущий трек" | "предыдущая песня" => return Some(Intent::Previous),
        "продолжи" | "продолжи музыку" | "продолжай музыку" | "продолжай" => {
            return Some(Intent::Resume)
        }
        "включи музыку" if !named => return Some(Intent::Resume),
        _ => {}
    }
    let rest = ["включи ", "поставь ", "запусти ", "сыграй ", "играй "]
        .iter()
        .find_map(|p| text.strip_prefix(p))?
        .trim();
    // "Лайки" and "мой плейлист" only make sense for SoundCloud: taken even in keyword mode.
    if matches!(rest, "мои лайки" | "лайки" | "мои лайкнутые треки") {
        return Some(Intent::Likes);
    }
    for p in ["мой плейлист ", "мою подборку ", "мой альбом "] {
        if let Some(q) = rest.strip_prefix(p) {
            let q = q.trim().trim_matches('"').trim();
            return (!q.is_empty()).then(|| Intent::MyPlaylist(q.to_owned()));
        }
    }
    if keyword_only && !named {
        return None;
    }
    if matches!(rest, "мою музыку" | "мои треки" | "мое любимое" | "любимое" | "мои любимые треки") {
        return Some(Intent::Likes);
    }
    for p in ["плейлист ", "альбом ", "подборку "] {
        if let Some(q) = rest.strip_prefix(p) {
            let q = q.trim().trim_matches('"').trim();
            return (!q.is_empty()).then(|| Intent::Playlist(q.to_owned()));
        }
    }
    let explicit = ["песню ", "трек ", "музыку ", "группу ", "исполнителя ", "микс "]
        .iter()
        .find_map(|p| rest.strip_prefix(p));
    let query = explicit.unwrap_or(rest).trim().trim_matches('"').trim();
    if query.is_empty() || query.len() > 400 {
        return None;
    }
    if query == "музыку" {
        return Some(Intent::Resume);
    }
    const HOME: [&str; 30] = [
        "свет", "освещение", "ламп", "люстр", "торшер", "ночник", "розетк", "телевизор", "тв",
        "кондиционер", "вентилятор", "обогревател", "пылесос", "робот", "чайник", "будильник",
        "таймер", "блютуз", "bluetooth", "режим", "экран", "микрофон", "громк", "звук",
        "отоплен", "увлажнител", "очистител", "зарядк", "радио", "сказк",
    ];
    if explicit.is_none()
        && !named
        && query
            .split_whitespace()
            .any(|w| HOME.iter().any(|p| w.starts_with(p)))
    {
        return None;
    }
    Some(Intent::Play(query.to_owned()))
}

// ---- minimal protobuf reader/writer ----

fn varint(data: &[u8], offset: &mut usize) -> Option<u64> {
    let mut value = 0u64;
    for shift in (0..70).step_by(7) {
        let byte = *data.get(*offset)?;
        *offset += 1;
        if shift == 63 && byte > 1 {
            return None;
        }
        value |= u64::from(byte & 127) << shift;
        if byte < 128 {
            return Some(value);
        }
    }
    None
}

#[derive(Clone, Copy)]
enum Value<'a> {
    Number(u64),
    Bytes(&'a [u8]),
}

fn field(data: &[u8], wanted: u64) -> Option<Value<'_>> {
    let mut offset = 0;
    while offset < data.len() {
        let key = varint(data, &mut offset)?;
        if key >> 3 == 0 {
            return None;
        }
        let value = match key & 7 {
            0 => Value::Number(varint(data, &mut offset)?),
            2 => {
                let len = usize::try_from(varint(data, &mut offset)?).ok()?;
                let end = offset.checked_add(len)?;
                let bytes = data.get(offset..end)?;
                offset = end;
                Value::Bytes(bytes)
            }
            1 | 5 => {
                offset = offset.checked_add(if key & 7 == 1 { 8 } else { 4 })?;
                if offset > data.len() {
                    return None;
                }
                continue;
            }
            _ => return None,
        };
        if key >> 3 == wanted {
            return Some(value);
        }
    }
    None
}

fn bytes(data: &[u8], n: u64) -> Option<&[u8]> {
    match field(data, n)? {
        Value::Bytes(v) => Some(v),
        Value::Number(_) => None,
    }
}

fn number(data: &[u8], n: u64) -> Option<u64> {
    match field(data, n)? {
        Value::Number(v) => Some(v),
        Value::Bytes(_) => None,
    }
}

fn alice_busy(data: &[u8]) -> Option<bool> {
    if number(data, 253) != Some(3) {
        return None;
    }
    let state = bytes(data, 252)?;
    match number(state, 1).unwrap_or(0) {
        0 | 6 => Some(false),
        1 | 2 | 3 | 5 => Some(true),
        _ => None,
    }
}

/// Final recognized phrase: (request id, text).
fn recognized(data: &[u8]) -> Option<(String, String)> {
    if number(data, 253) != Some(3) {
        return None;
    }
    let state = bytes(data, 252)?;
    if !matches!(number(state, 1), Some(1 | 3)) {
        return None;
    }
    let result = bytes(state, 15)?;
    if number(result, 2) != Some(1) || number(result, 4) != Some(1) {
        return None;
    }
    let text = std::str::from_utf8(bytes(result, 1)?).ok()?;
    if text.is_empty() || text.len() > 512 || Some(text.as_bytes()) != bytes(state, 2) {
        return None;
    }
    let request = std::str::from_utf8(bytes(state, 7)?).ok()?;
    if request.len() != 36 {
        return None;
    }
    Some((request.to_owned(), text.to_owned()))
}

fn put_var(out: &mut Vec<u8>, mut v: u64) {
    while v >= 128 {
        out.push((v as u8 & 127) | 128);
        v >>= 7;
    }
    out.push(v as u8);
}
fn put_number(out: &mut Vec<u8>, field: u64, value: u64) {
    put_var(out, field << 3);
    put_var(out, value);
}
fn put_bytes(out: &mut Vec<u8>, field: u64, value: &[u8]) {
    put_var(out, (field << 3) | 2);
    put_var(out, value.len() as u64);
    out.extend_from_slice(value);
}

fn rpc(capability: &str, field: u64, method: &[u8]) -> Vec<u8> {
    let mut remoting = Vec::new();
    put_bytes(&mut remoting, 7, capability.as_bytes());
    put_bytes(&mut remoting, field, method);
    let mut message = Vec::new();
    put_bytes(&mut message, 252, &remoting);
    put_number(&mut message, 253, 210);
    let mut frame = (message.len() as u32).to_le_bytes().to_vec();
    frame.extend_from_slice(&message);
    frame
}

fn send_native(frame: &[u8]) -> io::Result<()> {
    let address: SocketAddr = ADDRESS.parse().expect("address");
    let mut socket = TcpStream::connect_timeout(&address, Duration::from_millis(500))?;
    socket.set_write_timeout(Some(Duration::from_millis(500)))?;
    socket.write_all(frame)?;
    socket.shutdown(std::net::Shutdown::Write)
}

/// Stops Alice's own answer to the phrase we took over.
pub fn cancel_alice() {
    let _ = send_native(&rpc("AliceCapability", 8, &[8, 3]));
}

/// Random UUID v4 string (no extra crate needed).
fn request_id() -> String {
    use std::hash::{BuildHasher, Hasher};
    let mut bytes = [0u8; 16];
    for chunk in bytes.chunks_mut(8) {
        let mut h = std::collections::hash_map::RandomState::new().build_hasher();
        h.write_u128(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0),
        );
        chunk.copy_from_slice(&h.finish().to_le_bytes());
    }
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    format!("{}-{}-{}-{}-{}", &hex[..8], &hex[8..12], &hex[12..16], &hex[16..20], &hex[20..])
}

/// Pauses the built-in music player so the tracks do not overlap.
pub fn pause_native() {
    let mut source = Vec::new();
    put_number(&mut source, 1, 14);
    put_number(&mut source, 2, 3);
    put_number(&mut source, 3, 1);
    put_bytes(&mut source, 4, request_id().as_bytes());
    put_bytes(&mut source, 6, b"SoundyaVoice");
    let mut method = vec![8, 1];
    put_bytes(&mut method, 5, &source);
    put_number(&mut method, 7, 0);
    let _ = send_native(&rpc("PlaybackControlCapability", 13, &method));
}

fn read_frames(
    socket: &mut TcpStream,
    tx: &Sender<Cmd>,
    shared: &Shared,
    seen: &mut VecDeque<String>,
) -> io::Result<()> {
    let mut data = Vec::with_capacity(4096);
    loop {
        let mut len = [0u8; 4];
        socket.read_exact(&mut len)?;
        let size = u32::from_le_bytes(len) as usize;
        if size == 0 || size > MAX_FRAME {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "frame length"));
        }
        data.resize(size, 0);
        socket.read_exact(&mut data)?;
        if let Some(busy) = alice_busy(&data) {
            shared.duck.set_busy(busy);
        }
        if let Some((request, text)) = recognized(&data) {
            if seen.contains(&request) {
                continue;
            }
            seen.push_back(request);
            if seen.len() > 64 {
                seen.pop_front();
            }
            let Some(intent) = intent(&text, shared.keyword_only()) else {
                continue;
            };
            // Transport commands are ours only while SoundCloud owns the speaker.
            let ours = match intent {
                Intent::Play(_) | Intent::Playlist(_) | Intent::Likes | Intent::MyPlaylist(_) => true,
                Intent::Resume => shared.has_queue(),
                _ => shared.is_active(),
            };
            if !ours {
                continue;
            }
            eprintln!("voice: {text:?} -> {intent:?}");
            cancel_alice();
            let _ = tx.send(Cmd::Voice(intent));
        }
        if data.capacity() > 64 * 1024 {
            data = Vec::with_capacity(4096);
        }
    }
}

pub fn watch(tx: Sender<Cmd>, shared: std::sync::Arc<Shared>) {
    thread::spawn(move || {
        let mut seen = VecDeque::new();
        let address: SocketAddr = ADDRESS.parse().expect("address");
        loop {
            match TcpStream::connect_timeout(&address, Duration::from_secs(2)) {
                Ok(mut socket) => {
                    eprintln!("voice: connected to Alice");
                    let result = read_frames(&mut socket, &tx, &shared, &mut seen);
                    eprintln!("voice: connection lost ({result:?}); retrying");
                }
                Err(_) => {}
            }
            shared.duck.set_busy(false);
            thread::sleep(Duration::from_secs(2));
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn music_and_home_commands() {
        assert_eq!(
            intent("Включи radiohead", false),
            Some(Intent::Play("radiohead".into()))
        );
        assert_eq!(
            intent("Алиса, включи песню Всё идёт по плану", false),
            Some(Intent::Play("все идет по плану".into()))
        );
        assert_eq!(
            intent("включи bonobo в саундклауде", true),
            Some(Intent::Play("bonobo".into()))
        );
        assert_eq!(intent("включи bonobo", true), None);
        assert_eq!(
            intent("включи плейлист lofi beats", false),
            Some(Intent::Playlist("lofi beats".into()))
        );
        assert_eq!(intent("Алиса, включи мои лайки", false), Some(Intent::Likes));
        assert_eq!(intent("включи мой плейлист в дорогу", false), Some(Intent::MyPlaylist("в дорогу".into())));
        assert_eq!(intent("Останови музыку", false), Some(Intent::Pause));
        assert_eq!(intent("следующий трек", false), Some(Intent::Next));
        for s in ["включи свет", "поставь таймер на пять минут", "включи блютуз", "какая погода"] {
            assert_eq!(intent(s, false), None, "{s}");
        }
    }

    #[test]
    fn service_name_is_removed() {
        assert_eq!(strip_service("включи daft punk на soundcloud"), ("включи daft punk".into(), true));
        assert_eq!(strip_service("включи daft punk"), ("включи daft punk".into(), false));
    }

    #[test]
    fn parses_final_phrase() {
        let id = b"12345678-1234-1234-1234-123456789012";
        let text = "Включи radiohead".as_bytes();
        let mut asr = Vec::new();
        put_bytes(&mut asr, 1, text);
        put_number(&mut asr, 2, 1);
        put_number(&mut asr, 4, 1);
        let mut state = Vec::new();
        put_number(&mut state, 1, 3);
        put_bytes(&mut state, 2, text);
        put_bytes(&mut state, 7, id);
        put_bytes(&mut state, 15, &asr);
        let mut msg = Vec::new();
        put_bytes(&mut msg, 252, &state);
        put_number(&mut msg, 253, 3);
        let (req, got) = recognized(&msg).unwrap();
        assert_eq!(req.as_bytes(), id);
        assert_eq!(got, "Включи radiohead");
        assert_eq!(alice_busy(&msg), Some(true));
        for n in 0..msg.len() {
            assert_eq!(recognized(&msg[..n]), None);
        }
    }

    #[test]
    fn cancel_frame_matches_spotiya() {
        assert_eq!(
            &rpc("AliceCapability", 8, &[8, 3])[4..],
            &[
                0xe2, 0x0f, 0x15, 0x3a, 0x0f, b'A', b'l', b'i', b'c', b'e', b'C', b'a', b'p', b'a',
                b'b', b'i', b'l', b'i', b't', b'y', 0x42, 2, 8, 3, 0xe8, 0x0f, 0xd2, 1
            ]
        );
    }
}
