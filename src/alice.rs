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
    /// Shuffle the queue on/off.
    Shuffle(bool),
    /// Load something and play it shuffled («включи мои лайки вперемешку»).
    Shuffled(Box<Intent>),
    /// Repeat the current track on/off.
    Repeat(bool),
    /// Play the current track from the start.
    Restart,
    /// Tracks similar to the current one.
    Similar,
    /// Like the current track on SoundCloud.
    Like,
    /// Say what is playing.
    WhatPlaying,
    /// «Включи саундклауд» with nothing else: continue, or play the likes.
    Default,
    /// Personal recommendations.
    Recommend,
    /// Remove the current track from likes.
    Unlike,
    /// Add the current track to one of the account's playlists.
    AddToPlaylist(String),
    /// Jump within the track by this many ms (negative: back).
    Seek(i64),
    /// A command for the Windows PC (see pc.rs).
    Pc(PcAction),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PcAction {
    Wake,
    Sleep,
    Shutdown,
    Restart,
    Lock,
    Open(String),
    Close(String),
    /// Connection check from the settings page.
    Ping,
}

/// «включи компьютер», «открой телеграм на компьютере», «усыпи комп», …
fn pc(text: &str) -> Option<PcAction> {
    const PLACES: [&str; 8] = [
        " на компьютере ",
        " на компе ",
        " на пк ",
        " в компьютере ",
        " на компьютер ",
        " на комп ",
        " на моем компьютере ",
        " на моем компе ",
    ];
    const NOUNS: [&str; 7] = ["компьютер", "компьютера", "компьютеру", "комп", "компа", "компу", "пк"];
    let mut rest = format!(" {text} ");
    let mut found = false;
    for place in PLACES {
        if rest.contains(place) {
            rest = rest.replacen(place, " ", 1);
            found = true;
            break;
        }
    }
    let mut words: Vec<&str> = rest.split_whitespace().collect();
    if !found {
        let before = words.len();
        words.retain(|w| !NOUNS.contains(w));
        found = words.len() < before;
    }
    if !found {
        // «открой стим», «закрой хром» without «на компьютере»: the PC too, as Alice
        // cannot open programs anyway. Her skills and sites stay hers.
        let plain = match words.as_slice() {
            [verb, first, ..] if matches!(*verb, "открой" | "закрой") => {
                !["навык", "сайт", "страниц", "приложение яндекс", "ссылку"].iter().any(|p| first.starts_with(p))
            }
            _ => false,
        };
        if !plain {
            return None;
        }
    }
    words.retain(|w| !matches!(*w, "мой" | "моего" | "мне" | "пожалуйста"));
    let (&verb, object) = words.split_first()?;
    let mut object = object.join(" ");
    for filler in ["программу ", "приложение ", "игру "] {
        if let Some(o) = object.strip_prefix(filler) {
            object = o.to_owned();
        }
    }
    let all = words.join(" ");
    if all.contains("спящ") || all.contains(" сон") || verb == "усыпи" {
        return Some(PcAction::Sleep);
    }
    let open = matches!(verb, "открой" | "запусти" | "включи" | "открыть" | "запустить" | "включить");
    let close = matches!(verb, "закрой" | "выключи" | "закрыть" | "выключить" | "отключи" | "выруби");
    if !object.is_empty() {
        return if open {
            Some(PcAction::Open(object))
        } else if close {
            Some(PcAction::Close(object))
        } else {
            None
        };
    }
    match verb {
        "включи" | "включить" | "запусти" | "разбуди" => Some(PcAction::Wake),
        "выключи" | "выключить" | "отключи" | "выруби" => Some(PcAction::Shutdown),
        "перезагрузи" | "перезапусти" | "перезагрузить" => Some(PcAction::Restart),
        "заблокируй" | "заблокировать" | "блокируй" => Some(PcAction::Lock),
        _ => None,
    }
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

/// Playback control phrased in any of the usual ways:
/// «продолжи воспроизведение», «поставь музыку на паузу», «включи следующую песню», …
fn transport(text: &str) -> Option<Intent> {
    let words: Vec<&str> = text.split_whitespace().collect();
    let first = *words.first()?;
    let has = |stems: &[&str]| words.iter().any(|w| stems.iter().any(|s| w.starts_with(s)));
    // Questions and requests for something new are not transport commands.
    if has(&["почему", "что", "какой", "какая", "сколько", "когда", "где", "кто"]) || words.len() > 6 {
        return None;
    }
    let music = has(&["музык", "воспроизвед", "трек", "песн", "плейлист", "композици", "проигрыв"]);
    if first.starts_with("продолж") || first == "возобнови" || (first == "сними" && has(&["пауз"])) {
        return Some(Intent::Resume);
    }
    if first == "пауза" || has(&["пауз"]) && !has(&["сними"]) {
        return Some(Intent::Pause);
    }
    if matches!(first, "стоп" | "хватит" | "останови" | "остановись" | "приостанови" | "замолчи")
        || (matches!(first, "выключи" | "останови" | "прекрати") && music)
    {
        return Some(Intent::Pause);
    }
    if has(&["следующ", "дальше", "пропусти", "переключи", "другую", "другой"])
        && (music || words.len() <= 2 || has(&["следующ", "дальше", "пропусти"]))
    {
        return Some(Intent::Next);
    }
    if has(&["предыдущ", "назад", "прошл"]) && (music || words.len() <= 3) {
        return Some(Intent::Previous);
    }
    None
}

/// Shuffle words: Some((text without them, on)) when present.
fn shuffle_words(text: &str) -> Option<(String, bool)> {
    const OFF: [&str; 10] = [
        "не перемешивай",
        "по порядку",
        "выключи перемешивание",
        "отключи перемешивание",
        "без перемешивания",
        "выключи шафл",
        "отключи шафл",
        "выключи случайный порядок",
        "отключи случайный порядок",
        "выключи рандом",
    ];
    if OFF.iter().any(|p| text.contains(p)) {
        return Some((String::new(), false));
    }
    const ON: [&str; 15] = [
        "в случайном порядке",
        "случайный порядок",
        "вперемешку",
        "перемешанно",
        "перемешай",
        "перемешать",
        "перемешивай",
        "перемешивание",
        "решаффл",
        "решафл",
        "шаффл",
        "шафл",
        "рандомно",
        "в рандоме",
        "рандом",
    ];
    let mut out = format!(" {text} ");
    let mut found = false;
    for w in ON {
        let needle = format!(" {w} ");
        while out.contains(&needle) {
            out = out.replacen(&needle, " ", 1);
            found = true;
        }
    }
    found.then(|| (out.split_whitespace().collect::<Vec<_>>().join(" "), true))
}

/// Commands about the current track: what is it, like it, repeat it, more like it.
fn extras(text: &str) -> Option<Intent> {
    let words: Vec<&str> = text.split_whitespace().collect();
    let first = *words.first()?;
    let has = |stems: &[&str]| words.iter().any(|w| stems.iter().any(|s| w.starts_with(s)));
    const WHAT: [&str; 13] = [
        "что играет",
        "что сейчас играет",
        "что это играет",
        "что за песня",
        "что за трек",
        "что это за песня",
        "что это за трек",
        "как называется песня",
        "как называется трек",
        "как называется эта песня",
        "как называется этот трек",
        "кто поет",
        "кто это поет",
    ];
    if WHAT.iter().any(|p| text == *p || text.starts_with(&format!("{p} "))) {
        return Some(Intent::WhatPlaying);
    }
    if matches!(first, "убери" | "сними" | "удали") && has(&["лайк"]) || text.starts_with("удали из лайков") || text.starts_with("убери из лайков") {
        return Some(Intent::Unlike);
    }
    for p in [
        "добавь в плейлист ",
        "добавь этот трек в плейлист ",
        "добавь эту песню в плейлист ",
        "добавь трек в плейлист ",
        "добавь песню в плейлист ",
        "сохрани в плейлист ",
        "сохрани трек в плейлист ",
        "сохрани песню в плейлист ",
        "закинь в плейлист ",
    ] {
        if let Some(name) = text.strip_prefix(p) {
            let name = name.trim().trim_matches('"').trim();
            if !name.is_empty() {
                return Some(Intent::AddToPlaylist(name.to_owned()));
            }
        }
    }
    if text.starts_with("мне не нравится") || first == "дизлайк" || text.starts_with("поставь дизлайк") {
        return Some(Intent::Next);
    }
    if first == "лайкни"
        || text == "лайк"
        || text.starts_with("поставь лайк")
        || text.starts_with("мне нравится")
        || (first == "добавь" && has(&["лайк", "избранн", "любим"]))
    {
        return Some(Intent::Like);
    }
    if (matches!(first, "выключи" | "отключи" | "убери") && has(&["повтор"]))
        || text.contains("без повтора")
        || text.starts_with("хватит повторять")
        || text.starts_with("не повторяй")
    {
        return Some(Intent::Repeat(false));
    }
    if (has(&["повтор"]) && (matches!(first, "поставь" | "включи") || text.contains("на повтор")))
        || matches!(first, "повторяй" | "зацикли")
    {
        return Some(Intent::Repeat(true));
    }
    if matches!(text, "повтори трек" | "повтори песню" | "заново" | "сначала" | "с начала" | "включи заново" | "включи сначала" | "включи трек заново" | "включи песню заново") {
        return Some(Intent::Restart);
    }
    if has(&["похож"]) && words.len() <= 5 && !has(&["почему", "на что"]) {
        return Some(Intent::Similar);
    }
    None
}

/// `keyword_only`: intercept music requests only when SoundCloud is named.
/// «перемотай на 30 секунд», «перемотай вперед на минуту», «отмотай назад на полминуты»,
/// «промотай на 2 минуты 30 секунд». Returns the jump in ms (negative: back).
fn seek_command(text: &str) -> Option<i64> {
    let words: Vec<&str> = text.split_whitespace().collect();
    let verb = *words.first()?;
    let back_verb = verb.starts_with("отмота") || verb.starts_with("отмотк");
    if !(back_verb
        || verb.starts_with("перемота")
        || verb.starts_with("перемотк")
        || verb.starts_with("промота")
        || verb.starts_with("прокрути")
        || verb.starts_with("мотни"))
    {
        return None;
    }
    let back = back_verb || words.iter().any(|w| *w == "назад");
    fn number(word: &str) -> Option<f64> {
        if let Ok(n) = word.replace(',', ".").parse::<f64>() {
            return Some(n);
        }
        Some(match word {
            "одну" | "одна" | "один" => 1.0,
            "полторы" | "полтора" => 1.5,
            "две" | "два" => 2.0,
            "три" => 3.0,
            "четыре" => 4.0,
            "пять" => 5.0,
            "десять" => 10.0,
            "пятнадцать" => 15.0,
            "двадцать" => 20.0,
            "тридцать" => 30.0,
            "сорок" => 40.0,
            "пятьдесят" => 50.0,
            _ => return None,
        })
    }
    let at = |i: Option<usize>| i.map(|j| words[j]).and_then(number);
    let mut ms = 0f64;
    let mut found = false;
    for (i, w) in words.iter().enumerate().skip(1) {
        let unit = if w.starts_with("полминут") {
            ms += 30_000.0;
            found = true;
            continue;
        } else if w.starts_with("секунд") || *w == "сек" {
            1_000.0
        } else if w.starts_with("минут") || *w == "мин" {
            60_000.0
        } else {
            continue;
        };
        // «минуту» alone is one minute; «2 минуты», «двадцать секунд» take the number before.
        let mut n = at(i.checked_sub(1)).unwrap_or(1.0);
        // «двадцать пять секунд»
        if let (Some(a), Some(b)) = (at(i.checked_sub(2)), at(i.checked_sub(1))) {
            if a >= 20.0 && b < 10.0 {
                n = a + b;
            }
        }
        ms += n * unit;
        found = true;
    }
    if !found {
        // «перемотай на 30»: seconds; «перемотай вперед»: 30 seconds.
        ms = words.iter().skip(1).find_map(|w| number(w)).unwrap_or(30.0) * 1_000.0;
    }
    let ms = ms.round() as i64;
    Some(if back { -ms } else { ms })
}

pub fn intent(text: &str, keyword_only: bool) -> Option<Intent> {
    if let Some(ms) = seek_command(&normalize(text)) {
        return Some(Intent::Seek(ms));
    }
    if let Some(action) = pc(&normalize(text)) {
        return Some(Intent::Pc(action));
    }
    let (norm, named) = strip_service(&normalize(text));
    if named
        && matches!(
            norm.as_str(),
            "" | "включи" | "запусти" | "открой" | "поставь" | "играй" | "включи музыку" | "запусти музыку"
                | "поставь музыку" | "включи что-нибудь" | "поставь что-нибудь" | "включи музыку из"
        )
    {
        return Some(Intent::Default);
    }
    const RECS: [&str; 16] = [
        "включи рекомендации",
        "поставь рекомендации",
        "включи мои рекомендации",
        "рекомендации",
        "порекомендуй музыку",
        "порекомендуй что-нибудь",
        "посоветуй музыку",
        "включи что-нибудь новое",
        "включи что нибудь новое",
        "включи новое",
        "включи новую музыку",
        "включи что-нибудь на мой вкус",
        "включи на мой вкус",
        "включи музыку на мой вкус",
        "включи мою волну",
        "включи волну",
    ];
    if RECS.contains(&norm.as_str()) {
        return (named || !keyword_only).then_some(Intent::Recommend);
    }
    if let Some(i) = extras(&norm) {
        return Some(i);
    }
    if let Some((core, on)) = shuffle_words(&norm) {
        if !on {
            return Some(Intent::Shuffle(false));
        }
        let generic = matches!(
            core.as_str(),
            "" | "включи" | "поставь" | "музыку" | "включи музыку" | "треки" | "песни" | "очередь" | "все" | "включи все" | "плейлист" | "этот плейлист"
        );
        if generic {
            return Some(Intent::Shuffle(true));
        }
        let cmd = if ["включи ", "поставь ", "запусти ", "сыграй "].iter().any(|p| core.starts_with(p)) {
            core
        } else {
            format!("включи {core}")
        };
        let cmd = if named { format!("{cmd} в саундклауде") } else { cmd };
        return match intent_core(&cmd, keyword_only) {
            Some(i @ (Intent::Play(_) | Intent::Playlist(_) | Intent::Likes | Intent::MyPlaylist(_))) => {
                Some(Intent::Shuffled(Box::new(i)))
            }
            _ => Some(Intent::Shuffle(true)),
        };
    }
    intent_core(text, keyword_only)
}

fn intent_core(text: &str, keyword_only: bool) -> Option<Intent> {
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
    if let Some(t) = transport(&text) {
        return Some(t);
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

/// Alice's state from an AliceState frame (2 = speaking).
fn alice_state(data: &[u8]) -> Option<u64> {
    if number(data, 253) != Some(3) {
        return None;
    }
    Some(number(bytes(data, 252)?, 1).unwrap_or(0))
}

/// Set while soundya itself is talking through Alice, so it is not cut off.
static OWN_SPEECH: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Until when (ms since the Unix epoch) Alice's own answers are cut off; set by `quiet_for`.
static QUIET_UNTIL: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Cuts off whatever Alice starts saying in the next `time` (e.g. her answer to
/// the ▶‖ button, which soundya handles itself).
pub fn quiet_for(time: Duration) {
    QUIET_UNTIL.store(now_ms() + time.as_millis() as u64, std::sync::atomic::Ordering::Release);
    cancel_alice();
}

fn quiet() -> bool {
    QUIET_UNTIL.load(std::sync::atomic::Ordering::Acquire) > now_ms()
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

/// Says `text` in Alice's voice (START_VOCALIZE, as in yandex-station-extended's alice-say.py).
/// Runs a moment later so it is not swallowed by the cancel of the user's own request.
pub fn say(text: String) {
    thread::spawn(move || {
        thread::sleep(Duration::from_millis(600));
        match speak(&text) {
            Ok(()) => eprintln!("say: {text:?}"),
            Err(e) => eprintln!("say: {text:?}: {e}"),
        }
    });
}

/// Subscribes to Alice's events and asks her to speak on the same connection,
/// keeping it open until the speech ends: closing early can cut the speech off.
fn speak(text: &str) -> io::Result<()> {
    let address: SocketAddr = ADDRESS.parse().expect("address");
    let mut socket = TcpStream::connect_timeout(&address, Duration::from_secs(1))?;
    socket.set_read_timeout(Some(Duration::from_secs(15)))?;
    let mut directive = Vec::new();
    put_bytes(&mut directive, 1, text.as_bytes());
    put_number(&mut directive, 2, 1);
    let mut method = Vec::new();
    put_number(&mut method, 1, 18); // START_VOCALIZE
    put_number(&mut method, 3, 1); // send events
    put_bytes(&mut method, 13, &directive);
    let mut subscribe = Vec::new();
    put_bytes(&mut subscribe, 1, b"AliceCapability");
    let mut frames = rpc("AliceCapability", 20, &subscribe);
    frames.extend(rpc("AliceCapability", 8, &method));
    OWN_SPEECH.store(true, std::sync::atomic::Ordering::Release);
    struct Done;
    impl Drop for Done {
        fn drop(&mut self) {
            OWN_SPEECH.store(false, std::sync::atomic::Ordering::Release);
        }
    }
    let _done = Done;
    socket.write_all(&frames)?;
    let started = std::time::Instant::now();
    let mut speaking = false;
    let mut data = Vec::new();
    while started.elapsed() < Duration::from_secs(30) {
        let mut len = [0u8; 4];
        socket.read_exact(&mut len)?;
        let size = u32::from_le_bytes(len) as usize;
        if size == 0 || size > MAX_FRAME {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "frame length"));
        }
        data.resize(size, 0);
        socket.read_exact(&mut data)?;
        if number(&data, 253) != Some(3) {
            continue;
        }
        let Some(event) = bytes(&data, 252).and_then(|r| bytes(r, 18)) else {
            continue;
        };
        match number(event, 1) {
            Some(6) => speaking = true,          // TTS_STARTED
            Some(1) if speaking => return Ok(()), // TTS_COMPLETED
            Some(4) => return Err(io::Error::other("Alice reported an error")),
            _ => {}
        }
    }
    Ok(())
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
    // After taking over a phrase, Alice's own late answer («нужна подписка…») is cut off.
    let mut suppress_until: Option<std::time::Instant> = None;
    let mut last_cancel = std::time::Instant::now();
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
        if alice_state(&data) == Some(2)
            && (suppress_until.is_some_and(|t| std::time::Instant::now() < t) || quiet())
            && !OWN_SPEECH.load(std::sync::atomic::Ordering::Acquire)
            && last_cancel.elapsed() > Duration::from_millis(400)
        {
            eprintln!("voice: cutting off Alice's own answer");
            cancel_alice();
            last_cancel = std::time::Instant::now();
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
                eprintln!("voice: {text:?} left to Alice");
                continue;
            };
            // Transport commands are ours only while SoundCloud owns the speaker.
            let ours = match intent {
                Intent::Play(_)
                | Intent::Playlist(_)
                | Intent::Likes
                | Intent::MyPlaylist(_)
                | Intent::Shuffled(_)
                | Intent::Default
                | Intent::Recommend => true,
                // «Продолжи» is ours even with an empty queue (e.g. after a restart):
                // Alice can only answer it with «нужна подписка».
                Intent::Resume => true,
                // Only with a PC set up in the settings; otherwise the phrase stays Alice's.
                Intent::Pc(_) => shared.pc_configured(),
                Intent::Shuffle(_) | Intent::Repeat(_) => shared.has_queue(),
                _ => shared.is_active(),
            };
            if !ours {
                eprintln!("voice: {text:?} -> {intent:?}, left to Alice (SoundCloud is not playing)");
                continue;
            }
            eprintln!("voice: {text:?} -> {intent:?}");
            cancel_alice();
            suppress_until = Some(std::time::Instant::now() + Duration::from_secs(6));
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
    fn seek_commands() {
        let s = |t: &str| intent(t, false);
        assert_eq!(s("Алиса, перемотай на 30 секунд"), Some(Intent::Seek(30_000)));
        assert_eq!(s("перемотай на минуту"), Some(Intent::Seek(60_000)));
        assert_eq!(s("перемотай вперед на 2 минуты"), Some(Intent::Seek(120_000)));
        assert_eq!(s("перемотай назад на 10 секунд"), Some(Intent::Seek(-10_000)));
        assert_eq!(s("отмотай на полминуты"), Some(Intent::Seek(-30_000)));
        assert_eq!(s("промотай на минуту 30 секунд"), Some(Intent::Seek(90_000)));
        assert_eq!(s("перемотай на двадцать пять секунд"), Some(Intent::Seek(25_000)));
        assert_eq!(s("перемотай на полторы минуты"), Some(Intent::Seek(90_000)));
        assert_eq!(s("перемотай вперед"), Some(Intent::Seek(30_000)));
        assert_eq!(s("перемотай на 45"), Some(Intent::Seek(45_000)));
    }

    #[test]
    fn pc_commands() {
        let pc = |t: &str| intent(t, false);
        assert_eq!(pc("Алиса, включи компьютер"), Some(Intent::Pc(PcAction::Wake)));
        assert_eq!(pc("включи мой комп"), Some(Intent::Pc(PcAction::Wake)));
        assert_eq!(pc("Выключи компьютер."), Some(Intent::Pc(PcAction::Shutdown)));
        assert_eq!(pc("усыпи компьютер"), Some(Intent::Pc(PcAction::Sleep)));
        assert_eq!(pc("переведи компьютер в спящий режим"), Some(Intent::Pc(PcAction::Sleep)));
        assert_eq!(pc("перезагрузи пк"), Some(Intent::Pc(PcAction::Restart)));
        assert_eq!(pc("заблокируй компьютер"), Some(Intent::Pc(PcAction::Lock)));
        assert_eq!(pc("открой телеграм на компьютере"), Some(Intent::Pc(PcAction::Open("телеграм".into()))));
        assert_eq!(pc("запусти на компе стим"), Some(Intent::Pc(PcAction::Open("стим".into()))));
        assert_eq!(pc("включи на компьютере игру дота"), Some(Intent::Pc(PcAction::Open("дота".into()))));
        assert_eq!(pc("закрой хром на компе"), Some(Intent::Pc(PcAction::Close("хром".into()))));
        assert_eq!(pc("выключи дискорд на компьютере"), Some(Intent::Pc(PcAction::Close("дискорд".into()))));
        // Not about the PC: music and Alice's own commands stay as before.
        assert_eq!(pc("включи компромат"), Some(Intent::Play("компромат".into())));
        assert_ne!(pc("включи свет"), Some(Intent::Pc(PcAction::Wake)));
        assert_eq!(super::pc("какая погода на компьютере"), None);
        // Without «на компьютере»: «открой»/«закрой» still go to the PC.
        assert_eq!(pc("Открой steam"), Some(Intent::Pc(PcAction::Open("steam".into()))));
        assert_eq!(pc("открой телеграм"), Some(Intent::Pc(PcAction::Open("телеграм".into()))));
        assert_eq!(pc("закрой хром"), Some(Intent::Pc(PcAction::Close("хром".into()))));
        assert_eq!(super::pc("открой навык угадай мелодию"), None);
        assert_eq!(super::pc("открой сайт кинопоиска"), None);
        assert_eq!(super::pc("открой"), None);
    }

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
        for (s, want) in [
            ("перемешай", Intent::Shuffle(true)),
            ("перемешай музыку", Intent::Shuffle(true)),
            ("включи случайный порядок", Intent::Shuffle(true)),
            ("алиса, решафл", Intent::Shuffle(true)),
            ("играй по порядку", Intent::Shuffle(false)),
            ("включи мои лайки вперемешку", Intent::Shuffled(Box::new(Intent::Likes))),
            ("перемешай мои лайки", Intent::Shuffled(Box::new(Intent::Likes))),
            ("поставь на повтор", Intent::Repeat(true)),
            ("включи повтор", Intent::Repeat(true)),
            ("выключи повтор", Intent::Repeat(false)),
            ("включи заново", Intent::Restart),
            ("включи похожее", Intent::Similar),
            ("включи что-то похожее", Intent::Similar),
            ("лайкни", Intent::Like),
            ("мне нравится этот трек", Intent::Like),
            ("мне не нравится", Intent::Next),
            ("что сейчас играет", Intent::WhatPlaying),
            ("убери лайк", Intent::Unlike),
            ("удали из лайков", Intent::Unlike),
            ("добавь в плейлист в дорогу", Intent::AddToPlaylist("в дорогу".into())),
            ("алиса, добавь эту песню в плейлист чилл осень", Intent::AddToPlaylist("чилл осень".into())),
            ("как называется эта песня", Intent::WhatPlaying),
        ] {
            assert_eq!(intent(s, true), Some(want), "{s}");
        }
        assert_eq!(intent("включи мои лайки", true), Some(Intent::Likes));
        assert_eq!(intent("включи рекомендации в саундклауде", true), Some(Intent::Recommend));
        assert_eq!(intent("включи мою волну", false), Some(Intent::Recommend));
        assert_eq!(intent("включи мою волну", true), None);
        for s in ["Алиса, включи саундклауд", "включи музыку в саундклауде", "запусти soundcloud", "саундклауд"] {
            assert_eq!(intent(s, true), Some(Intent::Default), "{s}");
            assert_eq!(intent(s, false), Some(Intent::Default), "{s}");
        }
        assert_eq!(
            intent("включи bonobo в случайном порядке", false),
            Some(Intent::Shuffled(Box::new(Intent::Play("bonobo".into()))))
        );
        assert_eq!(
            intent("включи bonobo вперемешку в саундклауде", true),
            Some(Intent::Shuffled(Box::new(Intent::Play("bonobo".into()))))
        );
        for (s, want) in [
            ("продолжи воспроизведение", Intent::Resume),
            ("Алиса, продолжай играть", Intent::Resume),
            ("сними с паузы", Intent::Resume),
            ("поставь музыку на паузу", Intent::Pause),
            ("выключи воспроизведение", Intent::Pause),
            ("включи следующую песню", Intent::Next),
            ("пропусти трек", Intent::Next),
            ("переключи", Intent::Next),
            ("включи предыдущий трек", Intent::Previous),
        ] {
            assert_eq!(intent(s, true), Some(want), "{s}");
        }
        for s in ["что сейчас играет", "почему музыка на паузе", "выключи свет", "переключи канал на телевизоре"] {
            assert_eq!(transport(&normalize(s)), None, "{s}");
        }
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
