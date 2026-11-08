//! Telegram on the speaker: the user's own bot(s). Messages to the bot — from
//! people the user allowed, or in a group where the bot is added — are read out
//! by Alice; «Алиса, ответь …» answers the last one after a spoken «да».
//!
//! What it cannot do, honestly: read the user's own chats. That needs a user
//! session (MTProto), too heavy for the speaker and a secret not worth keeping
//! on it; messages to contacts still go through Telegram Desktop on the PC.
//!
//! Rules: only allowed senders (others wait in the page's «Новые отправители»);
//! quiet hours (a count is said when they end); a length limit; the same text
//! from the same chat is not said twice in 10 minutes; optionally only who
//! wrote, not what. Bot tokens live in config.json on the speaker like the
//! SoundCloud token: masked on the page, removed from bug reports — not
//! encrypted, since the key would have to live on the same speaker.

use std::{
    collections::HashMap,
    sync::{atomic::Ordering, Arc, Mutex},
    thread,
    time::{Duration, Instant},
};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::Shared;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct Bot {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub token: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct Sender {
    /// Telegram user or chat id.
    #[serde(default)]
    pub id: i64,
    /// How Alice calls them: «Мама».
    #[serde(default)]
    pub name: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Config {
    pub on: bool,
    pub bots: Vec<Bot>,
    pub allowed: Vec<Sender>,
    pub read_text: bool,
    pub max_len: usize,
    /// Quiet hours, minutes from local midnight; None = never quiet.
    pub quiet: Option<(u16, u16)>,
    pub offset_min: i32,
}

impl Config {
    pub fn from_settings(s: &crate::settings::Settings) -> Config {
        Config {
            on: s.tg_on,
            bots: s.tg_bots.iter().filter(|b| !b.token.trim().is_empty()).cloned().collect(),
            allowed: s.tg_allowed.clone(),
            read_text: s.tg_read,
            max_len: usize::from(s.tg_max.clamp(20, 1000)),
            quiet: s.tg_quiet_on.then_some((s.tg_quiet_from.min(1439), s.tg_quiet_to.min(1439))),
            offset_min: s.tz_offset,
        }
    }
}

static CONFIG: Mutex<Option<Config>> = Mutex::new(None);
/// Bumped on every change: the polling threads of old bots stop.
static GEN: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

#[derive(Default)]
struct State {
    /// Unknown senders, for the page: id → (name as Telegram shows it, last text, bot).
    unknown: HashMap<i64, (String, String, usize)>,
    /// (chat, text) → when said: no repeats within 10 minutes.
    said: HashMap<(i64, String), Instant>,
    /// Messages that came during quiet hours.
    held: Vec<String>,
    /// Where «ответь» goes: (bot index, chat id, name).
    last: Option<(usize, i64, String)>,
    /// A reply waiting for «да»: (bot, chat, name, text, when asked).
    pending: Option<(usize, i64, String, String, Instant)>,
    /// Last error per bot, for the page.
    errors: HashMap<usize, String>,
    /// How many messages were said.
    count: u64,
    /// Groups the bots are in (seen in updates): chat id → (title, bot), for the page's team setup.
    groups: HashMap<i64, (String, usize)>,
}

/// For sending without a page request (the meeting link): the proxy lives in Shared.
static SHARED: Mutex<Option<Arc<Shared>>> = Mutex::new(None);

/// A group chat in an update (a message there, or the bot added to it): (id, title).
pub fn group_of(u: &Value) -> Option<(i64, String)> {
    let chat = u.get("message").or_else(|| u.get("my_chat_member")).map(|m| &m["chat"])?;
    if !matches!(chat["type"].as_str(), Some("group" | "supergroup")) {
        return None;
    }
    Some((chat["id"].as_i64()?, chat["title"].as_str().unwrap_or("группа").to_owned()))
}

/// Sends with a bot that soundya does not poll (the team's bot on a VPS): send-only.
pub fn send_with(token: &str, chat: i64, text: &str) -> Result<(), String> {
    let shared = SHARED.lock().unwrap().clone().ok_or("Telegram не настроен")?;
    call(&shared, token.trim(), "sendMessage", json!({ "chat_id": chat, "text": text }), 15).map(|_| ())
}

/// Sends a message to a chat (a team's group) by bot number.
pub fn send_to_chat(bot: usize, chat: i64, text: &str) -> Result<(), String> {
    let shared = SHARED.lock().unwrap().clone().ok_or("Telegram не настроен")?;
    let token = CONFIG.lock().unwrap().as_ref().and_then(|c| c.bots.get(bot).or(c.bots.first()).map(|b| b.token.clone()));
    let token = token.ok_or("нет бота Telegram: добавьте его в настройках")?;
    call(&shared, &token, "sendMessage", json!({ "chat_id": chat, "text": text }), 15).map(|_| ())
}

static STATE: Mutex<Option<State>> = Mutex::new(None);

fn st<T>(f: impl FnOnce(&mut State) -> T) -> T {
    let mut g = STATE.lock().unwrap();
    f(g.get_or_insert_with(State::default))
}

fn api_base() -> String {
    std::env::var("SOUNDYA_TG_API").unwrap_or_else(|_| "https://api.telegram.org".into()).trim_end_matches('/').to_owned()
}

/// Quiet now? `now_min`: local minutes from midnight.
pub fn is_quiet(quiet: Option<(u16, u16)>, now_min: u16) -> bool {
    match quiet {
        None => false,
        Some((from, to)) if from == to => false,
        Some((from, to)) if from < to => now_min >= from && now_min < to,
        Some((from, to)) => now_min >= from || now_min < to,
    }
}

fn local_minutes(offset_min: i32) -> u16 {
    let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0);
    ((t / 60 + i64::from(offset_min)).rem_euclid(1440)) as u16
}

/// What Alice says for a message, or None when it is not to be said.
pub fn announcement(c: &Config, from_name: &str, text: &str) -> String {
    if !c.read_text || text.trim().is_empty() {
        return format!("{from_name}: новое сообщение в Телеграме");
    }
    let mut t: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if t.chars().count() > c.max_len {
        t = t.chars().take(c.max_len).collect::<String>() + "… и дальше";
    }
    format!("{from_name} пишет в Телеграме: {t}")
}

pub fn configure(c: Config, shared: &Arc<Shared>) {
    *SHARED.lock().unwrap() = Some(shared.clone());
    let changed = CONFIG.lock().unwrap().as_ref().is_none_or(|old| old.bots != c.bots || old.on != c.on);
    *CONFIG.lock().unwrap() = Some(c.clone());
    if !changed {
        return;
    }
    let gen = GEN.fetch_add(1, Ordering::AcqRel) + 1;
    if !c.on {
        return;
    }
    for (i, bot) in c.bots.into_iter().enumerate() {
        let shared = shared.clone();
        thread::spawn(move || poll(i, bot, gen, shared));
    }
}

fn agent(shared: &Shared) -> ureq::Agent {
    let proxy = shared.proxy.lock().unwrap().clone();
    crate::sc::build_agent(&proxy).unwrap_or_else(|_| ureq::agent())
}

fn call(shared: &Shared, token: &str, method: &str, body: Value, timeout: u64) -> Result<Value, String> {
    let url = format!("{}/bot{token}/{method}", api_base());
    let r = agent(shared)
        .post(&url)
        .timeout(Duration::from_secs(timeout))
        .set("Content-Type", "application/json")
        .send_string(&body.to_string());
    let v: Value = match r {
        Ok(r) => r.into_json().map_err(|e| e.to_string())?,
        Err(ureq::Error::Status(code, r)) => {
            let v: Value = r.into_json().unwrap_or_default();
            return Err(format!("{code}: {}", v["description"].as_str().unwrap_or("ошибка")));
        }
        // Never the token in messages and logs.
        Err(e) => return Err(e.to_string().replace(token, "«токен»")),
    };
    if v["ok"].as_bool() == Some(true) {
        Ok(v["result"].clone())
    } else {
        Err(v["description"].as_str().unwrap_or("ошибка Telegram").to_owned())
    }
}

fn poll(index: usize, bot: Bot, gen: u64, shared: Arc<Shared>) {
    let mut offset = 0i64;
    let mut first = true;
    eprintln!("telegram: bot {} on", index + 1);
    while GEN.load(Ordering::Acquire) == gen {
        let res = call(&shared, &bot.token, "getUpdates", json!({ "offset": offset, "timeout": 25, "allowed_updates": ["message", "my_chat_member"] }), 40);
        let updates = match res {
            Ok(v) => {
                st(|s| s.errors.remove(&index));
                v.as_array().cloned().unwrap_or_default()
            }
            Err(e) => {
                eprintln!("telegram: bot {}: {e}", index + 1);
                st(|s| s.errors.insert(index, e));
                thread::sleep(Duration::from_secs(15));
                continue;
            }
        };
        for u in &updates {
            offset = offset.max(u["update_id"].as_i64().unwrap_or(0) + 1);
            if let Some((id, title)) = group_of(u) {
                st(|s| s.groups.insert(id, (title, index)));
            }
            // What came before soundya started is not read out.
            if first {
                continue;
            }
            if let Some(m) = u.get("message") {
                on_message(index, m, &shared);
            }
        }
        first = false;
    }
    eprintln!("telegram: bot {} off", index + 1);
}

fn on_message(bot: usize, m: &Value, shared: &Shared) {
    let Some(c) = CONFIG.lock().unwrap().clone() else { return };
    let chat = m["chat"]["id"].as_i64().unwrap_or(0);
    let from = m["from"]["id"].as_i64().unwrap_or(0);
    let text = m["text"].as_str().or_else(|| m["caption"].as_str()).unwrap_or("").to_owned();
    let tg_name = [m["from"]["first_name"].as_str(), m["from"]["last_name"].as_str()].iter().flatten().copied().collect::<Vec<_>>().join(" ");
    let allowed = c.allowed.iter().find(|a| a.id == from || a.id == chat).cloned();
    let Some(who) = allowed else {
        let group = matches!(m["chat"]["type"].as_str(), Some("group" | "supergroup"));
        let title = m["chat"]["title"].as_str().unwrap_or("группа").to_owned();
        st(|s| {
            if s.unknown.len() < 30 {
                s.unknown.insert(from, (tg_name.clone(), text.chars().take(80).collect(), bot));
            }
            // A group too: «Разрешить» it and everything said there is read out.
            if group && s.unknown.len() < 30 {
                s.unknown.insert(chat, (format!("Группа «{title}» (все сообщения)"), text.chars().take(80).collect(), bot));
            }
        });
        return;
    };
    let name = said_name(&who, from, chat, &tg_name, m["chat"]["title"].as_str().unwrap_or(""));
    let repeat = st(|s| {
        s.said.retain(|_, at| at.elapsed() < Duration::from_secs(600));
        let key = (chat, text.clone());
        let seen = s.said.contains_key(&key);
        s.said.insert(key, Instant::now());
        s.last = Some((bot, chat, name.clone()));
        seen
    });
    if repeat {
        return;
    }
    if is_quiet(c.quiet, local_minutes(c.offset_min)) {
        st(|s| s.held.push(name));
        return;
    }
    say(shared, announcement(&c, &name, &text));
}

/// Who Alice says it is from: the name given to the person; for a whole group allowed,
/// «Саша в «Работа»» (the sender, then the group's name given on the page or its title).
pub fn said_name(who: &Sender, from: i64, chat: i64, tg_name: &str, title: &str) -> String {
    let given = who.name.trim();
    if who.id == chat && chat != from {
        let place = if given.is_empty() { title.trim() } else { given };
        let person = if tg_name.trim().is_empty() { "Кто-то" } else { tg_name.trim() };
        return if place.is_empty() { person.to_owned() } else { format!("{person} в «{place}»") };
    }
    if given.is_empty() { tg_name.to_owned() } else { given.to_owned() }
}

fn say(_shared: &Shared, text: String) {
    st(|s| s.count += 1);
    eprintln!("telegram: announcing a message");
    // Music down a little while she reads it (Smart Ducking, «уведомление»).
    let _ = crate::ducking::event(&crate::ducking::Request { source: "telegram".into(), kind: "notify".into(), state: "start".into(), ..Default::default() });
    crate::display::notify("tg");
    crate::alice::say(text);
}

/// When quiet hours end: how many messages came.
pub fn run_summary(shared: Arc<Shared>) {
    thread::spawn(move || loop {
        thread::sleep(Duration::from_secs(60));
        let Some(c) = CONFIG.lock().unwrap().clone() else { continue };
        if !c.on || is_quiet(c.quiet, local_minutes(c.offset_min)) {
            continue;
        }
        let held = st(|s| std::mem::take(&mut s.held));
        if !held.is_empty() {
            let mut names = held.clone();
            names.sort();
            names.dedup();
            say(&shared, format!("Пока было тихо, в Телеграм пришло сообщений: {}. От: {}", held.len(), names.join(", ")));
        }
    });
}

// ---------------------------------------------------------------- replies

/// «Ответь …»: asks for confirmation first. The answer is what Alice says.
pub fn reply_draft(text: &str) -> String {
    let Some((bot, chat, name)) = st(|s| s.last.clone()) else {
        return "Пока некому отвечать: сообщений в Телеграме не было".into();
    };
    st(|s| s.pending = Some((bot, chat, name.clone(), text.to_owned(), Instant::now())));
    format!("Ответ для: {name}. {text}. Отправить? Скажите да или нет")
}

/// Is a reply waiting for «да» / «нет»?
pub fn awaiting() -> bool {
    st(|s| s.pending.as_ref().is_some_and(|p| p.4.elapsed() < Duration::from_secs(40)))
}

/// The reply to send after «да»: Err is what Alice says instead.
fn take_pending(yes: bool) -> Result<(usize, i64, String, String), String> {
    let Some((bot, chat, name, text, at)) = st(|s| s.pending.take()) else { return Err("Нечего отправлять".into()) };
    if at.elapsed() > Duration::from_secs(40) {
        return Err("Время на подтверждение вышло, скажите ещё раз".into());
    }
    if !yes {
        return Err("Не отправляю".into());
    }
    Ok((bot, chat, name, text))
}

/// «Да» (send) or «нет» (cancel).
pub fn confirm(yes: bool, shared: &Shared) -> String {
    let (bot, chat, name, text) = match take_pending(yes) {
        Ok(p) => p,
        Err(say) => return say,
    };
    let token = CONFIG.lock().unwrap().as_ref().and_then(|c| c.bots.get(bot).map(|b| b.token.clone()));
    let Some(token) = token else { return "Бот выключен".into() };
    match call(shared, &token, "sendMessage", json!({ "chat_id": chat, "text": text }), 15) {
        Ok(_) => format!("Отправила. Получатель: {name}"),
        Err(e) => {
            eprintln!("telegram: send: {e}");
            "Не получилось отправить".into()
        }
    }
}

pub fn json() -> Value {
    st(|s| {
        json!({
            "unknown": s.unknown.iter().map(|(id, (n, t, b))| json!({ "id": id, "name": n, "text": t, "bot": b })).collect::<Vec<_>>(),
            "errors": s.errors.iter().map(|(b, e)| json!({ "bot": b, "error": e })).collect::<Vec<_>>(),
            "held": s.held.len(),
            "said": s.count,
            "last": s.last.as_ref().map(|l| l.2.clone()),
            "groups": s.groups.iter().map(|(id, (t, b))| json!({ "id": id, "title": t, "bot": b })).collect::<Vec<_>>(),
        })
    })
}

/// Bots on and set up: «ответь …» and «да/нет» are soundya's.
pub fn active() -> bool {
    CONFIG.lock().unwrap().as_ref().is_some_and(|c| c.on && !c.bots.is_empty())
}

/// The page's «Разрешить»: removes them from the unknown list.
pub fn forget_unknown(id: i64) {
    st(|s| s.unknown.remove(&id));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_whole_group_says_who_wrote_and_where() {
        let group = Sender { id: -100500, name: "Работа".into() };
        assert_eq!(said_name(&group, 7, -100500, "Саша Петров", "Отдел продаж"), "Саша Петров в «Работа»");
        let untitled = Sender { id: -100500, name: String::new() };
        assert_eq!(said_name(&untitled, 7, -100500, "Саша", "Отдел продаж"), "Саша в «Отдел продаж»");
        let mom = Sender { id: 7, name: "Мама".into() };
        assert_eq!(said_name(&mom, 7, 7, "Ирина", ""), "Мама", "a person keeps the name given");
        assert_eq!(said_name(&mom, 7, -100500, "Ирина", "Семья"), "Мама", "allowed as a person: the same in any chat");
    }

    #[test]
    fn groups_are_noticed_for_the_team_setup() {
        let added = json!({"update_id": 1, "my_chat_member": {"chat": {"id": -100123, "type": "supergroup", "title": "Команда"}}});
        assert_eq!(group_of(&added), Some((-100123, "Команда".into())));
        let msg = json!({"update_id": 2, "message": {"chat": {"id": -42, "type": "group", "title": "Отдел"}, "text": "/start"}});
        assert_eq!(group_of(&msg), Some((-42, "Отдел".into())));
        let private = json!({"update_id": 3, "message": {"chat": {"id": 7, "type": "private"}, "text": "привет"}});
        assert_eq!(group_of(&private), None);
    }

    fn cfg() -> Config {
        Config { on: true, bots: vec![], allowed: vec![], read_text: true, max_len: 20, quiet: None, offset_min: 180 }
    }

    #[test]
    fn quiet_hours() {
        assert!(is_quiet(Some((23 * 60, 8 * 60)), 2 * 60), "night");
        assert!(!is_quiet(Some((23 * 60, 8 * 60)), 12 * 60));
        assert!(is_quiet(Some((13 * 60, 15 * 60)), 14 * 60), "a nap");
        assert!(!is_quiet(None, 3 * 60));
    }

    #[test]
    fn what_alice_says() {
        let c = cfg();
        assert_eq!(announcement(&c, "Мама", "Купи хлеба"), "Мама пишет в Телеграме: Купи хлеба");
        assert_eq!(
            announcement(&c, "Мама", "очень длинное сообщение про всё на свете"),
            "Мама пишет в Телеграме: очень длинное сообще… и дальше"
        );
        let silent = Config { read_text: false, ..cfg() };
        assert_eq!(announcement(&silent, "Мама", "секрет"), "Мама: новое сообщение в Телеграме");
    }

    #[test]
    fn reply_needs_a_yes() {
        *STATE.lock().unwrap() = None;
        assert!(reply_draft("скоро буду").contains("некому"));
        st(|s| s.last = Some((0, 42, "Мама".into())));
        assert_eq!(reply_draft("скоро буду"), "Ответ для: Мама. скоро буду. Отправить? Скажите да или нет");
        assert!(awaiting());
        assert_eq!(take_pending(false).unwrap_err(), "Не отправляю");
        assert!(!awaiting());
        reply_draft("уже еду");
        assert_eq!(take_pending(true).unwrap(), (0, 42, "Мама".into(), "уже еду".into()));
        *STATE.lock().unwrap() = None;
    }
}
