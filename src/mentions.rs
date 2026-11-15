//! Mentions in the team's Telegram group: what is written to the owner («@ник …») or to the
//! bot («@бот …», a reply to it) is collected by the speaker itself.
//!
//! A message that asks for something («сделай», «нужно», «пришли», «до пятницы …») becomes a
//! task in the planner at once, with who asked; anything else is a message: in working hours
//! Alice says who wrote and where, outside them it waits and is counted when the working day
//! starts. The Ditoo launcher sees every mention through the Telegram counter it already
//! follows, and the list is in the status for the panel and /api/ditoo.
//!
//! The bot is the one of «Токен бота для ссылок». Telegram lets one program read a bot, so it
//! must not run anywhere else; and in a group a bot sees only messages that name it unless
//! its privacy mode is off (BotFather → /setprivacy → Disable, then add it to the group
//! again) — the panel shows which it is. There is no language model on the speaker: «task or
//! message» is decided by the words (`task_title`).

use std::{
    sync::{atomic::Ordering, Arc, Mutex},
    thread,
    time::Duration,
};

use serde_json::{json, Value};

use crate::Shared;

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Config {
    pub on: bool,
    pub token: String,
    /// The owner's Telegram name without «@», lower case.
    pub me: String,
}

impl Config {
    pub fn from_settings(s: &crate::settings::Settings) -> Config {
        // Not the bot that «Устройства → Telegram» already reads: two readers of one bot take
        // its messages from each other.
        let token = s.meet_bot_token.trim().to_owned();
        let taken = s.tg_on && s.tg_bots.iter().any(|b| b.token.trim() == token);
        Config { on: s.tg_mentions && !taken, token, me: s.tg_me.trim().trim_start_matches('@').to_lowercase() }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Mention {
    from: String,
    chat: String,
    text: String,
    at: i64,
    task: bool,
}

#[derive(Default)]
struct State {
    /// The bot's own name (getMe), lower case, and whether it sees every message of a group.
    bot: String,
    sees_all: Option<bool>,
    error: String,
    /// The last ones, newest last.
    items: Vec<Mention>,
    /// Came outside working hours: (messages, tasks).
    held: (usize, usize),
    /// What «/tasks» listed last, by its numbers: (source, id, Google list) for «/done 2».
    listed: Vec<(String, String, String)>,
}

static CONFIG: Mutex<Option<Config>> = Mutex::new(None);
static STATE: Mutex<Option<State>> = Mutex::new(None);
/// Bumped on every change: the old polling thread stops.
static GEN: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
const KEPT: usize = 20;

fn st<T>(f: impl FnOnce(&mut State) -> T) -> T {
    f(STATE.lock().unwrap().get_or_insert_with(State::default))
}

pub fn configure(c: Config, shared: &Arc<Shared>) {
    let changed = CONFIG.lock().unwrap().as_ref() != Some(&c);
    *CONFIG.lock().unwrap() = Some(c.clone());
    if !changed {
        return;
    }
    let gen = GEN.fetch_add(1, Ordering::AcqRel) + 1;
    st(|s| {
        s.error.clear();
        s.sees_all = None;
    });
    if c.on && !c.token.is_empty() {
        let shared = shared.clone();
        thread::spawn(move || poll(c, gen, shared));
    }
}

/// «@ник» in the text as a whole name: «@rod» is not in «@rodion».
fn names(text: &str, name: &str) -> bool {
    if name.is_empty() {
        return false;
    }
    let low = text.to_lowercase();
    let want = format!("@{name}");
    low.match_indices(&want).any(|(i, _)| !low[i + want.len()..].starts_with(|c: char| c.is_alphanumeric() || c == '_'))
}

/// Is the message for the owner or the bot: names one of them, or answers the bot.
pub fn addressed(m: &Value, me: &str, bot: &str) -> bool {
    let text = m["text"].as_str().or_else(|| m["caption"].as_str()).unwrap_or("");
    let reply_to = m["reply_to_message"]["from"]["username"].as_str().unwrap_or("").to_lowercase();
    names(text, me) || names(text, bot) || (!reply_to.is_empty() && (reply_to == bot || reply_to == me))
}

/// The text without «@names» and the space they leave.
fn without_names(text: &str) -> String {
    text.split_whitespace().filter(|w| !w.starts_with('@')).collect::<Vec<_>>().join(" ").trim_matches(|c: char| matches!(c, ',' | ':' | '-' | '—' | ' ')).to_owned()
}

/// A request to do something → its text as a task's title; None: just a message. By the
/// words: an order («сделай», «пришли», «проверь»), «нужно / надо», «задача:», a wish
/// («можешь глянуть»). A question without them («ты где?») is a message.
pub fn task_title(text: &str) -> Option<String> {
    const ORDERS: [&str; 44] = [
        "сдела", "нужно", "надо", "необходимо", "провер", "посмотри", "глян", "пришли", "скинь", "отправ", "подготов", "исправ", "поправ",
        "почини", "добав", "убери", "удали", "обнови", "залей", "выложи", "напиши", "позвони", "созвонись", "закажи", "оплати", "купи",
        "собери", "составь", "задача", "таск", "todo", "доделай", "допиши", "переделай", "согласуй", "уточни", "узнай", "забронируй",
        "запиши", "оформи", "настрой", "разберись", "дедлайн", "срочно",
    ];
    let title = without_names(text);
    let low = title.to_lowercase().replace('ё', "е");
    let asks = low.split(|c: char| !c.is_alphanumeric()).any(|w| ORDERS.iter().any(|o| w.starts_with(o)));
    (asks && title.chars().count() >= 3).then(|| title.chars().take(200).collect())
}

fn now() -> i64 {
    crate::planner::now()
}

fn working() -> bool {
    crate::planner::working_now()
}

fn poll(c: Config, gen: u64, shared: Arc<Shared>) {
    // Who the bot is, and whether it sees every message of a group or only those that name it.
    match crate::telegram::call_bot(&shared, &c.token, "getMe", json!({}), 20) {
        Ok(v) => st(|s| {
            s.bot = v["username"].as_str().unwrap_or("").to_lowercase();
            s.sees_all = v["can_read_all_group_messages"].as_bool();
        }),
        Err(e) => st(|s| s.error = e),
    }
    eprintln!("mentions: reading the group as @{}", st(|s| s.bot.clone()));
    // The commands in Telegram's own menu («/» in the chat).
    let menu: Vec<Value> = COMMANDS.iter().map(|(c, d)| json!({ "command": c, "description": d })).collect();
    if let Err(e) = crate::telegram::call_bot(&shared, &c.token, "setMyCommands", json!({ "commands": menu }), 20) {
        eprintln!("mentions: the command menu was not set: {e}");
    }
    let mut offset = 0i64;
    let mut first = true;
    while GEN.load(Ordering::Acquire) == gen {
        let res = crate::telegram::call_bot(&shared, &c.token, "getUpdates", json!({ "offset": offset, "timeout": 25, "allowed_updates": ["message"] }), 40);
        let updates = match res {
            Ok(v) => {
                st(|s| s.error.clear());
                v.as_array().cloned().unwrap_or_default()
            }
            Err(e) => {
                eprintln!("mentions: {e}");
                // «Conflict»: another program reads this bot — say it in words on the panel.
                st(|s| s.error = if e.contains("409") || e.to_lowercase().contains("conflict") { "этого бота читает ещё одна программа — колонка не получает его сообщения".into() } else { e });
                thread::sleep(Duration::from_secs(20));
                continue;
            }
        };
        for u in &updates {
            offset = offset.max(u["update_id"].as_i64().unwrap_or(0) + 1);
            // What came before soundya started is old news.
            if !first {
                if let Some(m) = u.get("message") {
                    on_message(m, &c, &shared);
                }
            }
        }
        first = false;
    }
}

/// The owner's Telegram id (his private chat with the bot), learned from his first message
/// and kept in <state dir>/tg_owner: «/срочно» writes there.
static OWNER: Mutex<Option<i64>> = Mutex::new(None);

fn owner_id() -> Option<i64> {
    let mut o = OWNER.lock().unwrap();
    if o.is_none() {
        *o = crate::fsx::read_string(&crate::state_dir().join("tg_owner")).and_then(|t| t.trim().parse().ok());
    }
    *o
}

fn remember_owner(id: i64) {
    if id != 0 && owner_id() != Some(id) {
        *OWNER.lock().unwrap() = Some(id);
        let _ = std::fs::write(crate::state_dir().join("tg_owner"), id.to_string());
    }
}

fn on_message(m: &Value, c: &Config, shared: &Shared) {
    let group = matches!(m["chat"]["type"].as_str(), Some("group" | "supergroup"));
    let own = !c.me.is_empty() && m["from"]["username"].as_str().is_some_and(|u| u.to_lowercase() == c.me);
    if own {
        remember_owner(m["from"]["id"].as_i64().unwrap_or(0));
    }
    let bot = st(|s| s.bot.clone());
    let text = m["text"].as_str().or_else(|| m["caption"].as_str()).unwrap_or("").to_owned();
    if m["from"]["is_bot"].as_bool() == Some(true) {
        return;
    }
    // A command: in the group from anyone, in a private chat only from the owner.
    if let Some((cmd, arg)) = command(&text, &bot) {
        if group || own {
            let answer = run_command(cmd, &arg, m, own, c, shared);
            let body = json!({ "chat_id": m["chat"]["id"], "text": answer, "reply_to_message_id": m["message_id"], "allow_sending_without_reply": true });
            if let Err(e) = crate::telegram::call_bot(shared, &c.token, "sendMessage", body, 15) {
                eprintln!("mentions: the answer to /{cmd} was not sent: {e}");
            }
        }
        return;
    }
    if !group {
        return;
    }
    // The owner's own messages are not news to him — unless he writes to the bot («@бот
    // задача: …»): that is a note to himself.
    let to_bot = names(&text, &bot);
    eprintln!("mentions: a message in the group (own: {own}, for the owner or the bot: {})", addressed(m, &c.me, &bot));
    if (own && !to_bot) || !addressed(m, &c.me, &bot) {
        return;
    }
    let from = [m["from"]["first_name"].as_str(), m["from"]["last_name"].as_str()].iter().flatten().copied().collect::<Vec<_>>().join(" ");
    let from = if from.trim().is_empty() { "Кто-то".to_owned() } else { from };
    let chat = m["chat"]["title"].as_str().unwrap_or("группа").to_owned();
    let task = task_title(&text);
    if let Some(title) = &task {
        // The day and the time written in it («до пятницы» is not understood, «до 15.11», «завтра в 18:00» are).
        let (t, off) = (now(), crate::planner::offset());
        let today = (t + off).div_euclid(86_400);
        let parsed = crate::planner::parse_document(&format!("- {title}"), today, off, t).into_iter().next();
        let (title, due, high) = parsed.map_or((title.clone(), None, false), |e| (e.title, e.due, e.high));
        crate::planner::add(&format!("{title} — {from}"), due, "", false, "Telegram", if high { "high" } else { "normal" });
    }
    let is_task = task.is_some();
    eprintln!("mentions: {} in a group", if is_task { "a task" } else { "a message" });
    st(|s| {
        s.items.push(Mention { from: from.clone(), chat: chat.clone(), text: without_names(&text).chars().take(300).collect(), at: now(), task: is_task });
        if s.items.len() > KEPT {
            s.items.remove(0);
        }
    });
    // The Ditoo launcher shows a Telegram pop-up when this counter grows.
    crate::telegram::count_said();
    if !working() {
        st(|s| if is_task { s.held.1 += 1 } else { s.held.0 += 1 });
        return;
    }
    let _ = crate::ducking::event(&crate::ducking::Request { source: "telegram".into(), kind: "notify".into(), state: "start".into(), ..Default::default() });
    crate::display::notify("tg");
    crate::alice::say(if is_task {
        format!("{from} в группе «{chat}» поставил задачу. Добавила её в планы")
    } else {
        format!("{from} написал вам в группе «{chat}». Проверьте Телеграм")
    });
}

/// The bot's commands: (name for Telegram's menu, what it does). Russian names work too.
const COMMANDS: [(&str, &str); 7] = [
    ("task", "Задача тимлиду: /task сверстать лендинг до 15.11"),
    ("call", "Созвон: /call завтра в 12:00 обсудить релиз"),
    ("urgent", "Срочно: сразу в личку и на колонку"),
    ("tasks", "Что нужно сделать"),
    ("done", "Отметить сделанным: /done 2"),
    ("say", "Сказать вслух на колонке: /say обед готов"),
    ("help", "Что умеет бот"),
];

/// «/task купить молоко», «/задача@бот …» → ("task", "купить молоко"). None: not a command,
/// or one for another bot of the group.
pub fn command(text: &str, bot: &str) -> Option<(&'static str, String)> {
    let rest = text.trim().strip_prefix('/')?;
    let (head, arg) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
    let (name, to) = head.split_once('@').unwrap_or((head, ""));
    if !to.is_empty() && to.to_lowercase() != bot {
        return None;
    }
    let cmd = match name.to_lowercase().as_str() {
        "task" | "задача" | "add" | "todo" => "task",
        "call" | "созвон" | "meet" | "встреча" => "call",
        "urgent" | "срочно" | "асап" | "asap" => "urgent",
        "tasks" | "задачи" | "list" | "дела" | "планы" => "tasks",
        "done" | "готово" | "сделано" => "done",
        "say" | "скажи" => "say",
        "help" | "помощь" | "start" => "help",
        _ => return None,
    };
    Some((cmd, arg.trim().to_owned()))
}

/// Does the command; the text is the bot's answer in the chat.
fn run_command(cmd: &str, arg: &str, m: &Value, own: bool, c: &Config, shared: &Shared) -> String {
    let from = [m["from"]["first_name"].as_str(), m["from"]["last_name"].as_str()].iter().flatten().copied().collect::<Vec<_>>().join(" ");
    let from = if from.trim().is_empty() { "Кто-то".to_owned() } else { from };
    let chat = m["chat"]["title"].as_str().unwrap_or("личный чат").to_owned();
    let (t, off) = (now(), crate::planner::offset());
    let today = (t + off).div_euclid(86_400);
    // Said on the speaker in working hours; outside them it is counted for the morning.
    let announce = |text: String, task: bool| {
        crate::telegram::count_said();
        if !working() {
            st(|s| if task { s.held.1 += 1 } else { s.held.0 += 1 });
            return;
        }
        let _ = crate::ducking::event(&crate::ducking::Request { source: "telegram".into(), kind: "notify".into(), state: "start".into(), ..Default::default() });
        crate::display::notify("tg");
        crate::alice::say(text);
    };
    let keep = |text: &str, task: bool| {
        st(|s| {
            s.items.push(Mention { from: from.clone(), chat: chat.clone(), text: text.chars().take(300).collect(), at: t, task });
            if s.items.len() > KEPT {
                s.items.remove(0);
            }
        });
    };
    match cmd {
        // «/созвон завтра в 12:00 обсудить релиз»: in the plans as a call, said aloud at its time.
        "call" => {
            let e = crate::planner::parse_document(&format!("- {arg}"), today, off, t).into_iter().next();
            let Some(due) = e.as_ref().and_then(|e| e.due) else {
                return "Напишите, когда созвон: /call завтра в 12:00 обсудить релиз".into();
            };
            let about = e.map(|e| e.title).filter(|x| x.chars().count() >= 2).unwrap_or_default();
            let who = if own { String::new() } else { format!(" — {from}") };
            let title = if about.is_empty() { format!("Созвон{who}") } else { format!("Созвон: {about}{who}") };
            crate::planner::add(&title, Some(due), "", due > t, "Созвон", "high");
            let when = crate::planner::say_time(due, t, off);
            keep(&format!("созвон {when}: {about}"), true);
            if !own {
                announce(format!("{from} назначил созвон {when}. {about}"), true);
            }
            format!("Созвон в планах: {when}{}", if about.is_empty() { String::new() } else { format!(" — {about}") })
        }
        // «/срочно сервер лежит»: to the owner's private chat at once, and aloud in working hours.
        "urgent" => {
            if arg.is_empty() {
                return "Напишите, что срочно: /urgent сервер лежит".into();
            }
            keep(&format!("срочно: {arg}"), false);
            let sent = match owner_id() {
                Some(id) => crate::telegram::call_bot(shared, &c.token, "sendMessage", json!({ "chat_id": id, "text": format!("🔥 Срочно от {from} («{chat}»):\n{arg}") }), 15).is_ok(),
                None => false,
            };
            crate::telegram::count_said();
            // Urgent is said in working hours even over the count of ordinary messages.
            if working() {
                let _ = crate::ducking::event(&crate::ducking::Request { source: "telegram".into(), kind: "notify".into(), state: "start".into(), ..Default::default() });
                crate::display::notify("tg");
                let text: String = arg.split_whitespace().collect::<Vec<_>>().join(" ").chars().take(250).collect();
                crate::alice::say(format!("Срочно от {from}: {text}"));
            } else {
                st(|s| s.held.0 += 1);
            }
            match (sent, working()) {
                (true, true) => "Передала: в личку и вслух на колонке".into(),
                (true, false) => "Передала в личку. На колонке скажу в рабочее время".into(),
                (false, true) => "Сказала на колонке. В личку не вышло: владелец должен один раз написать мне /start в личные сообщения".into(),
                (false, false) => "Записала. В личку не вышло: владелец должен один раз написать мне /start в личные сообщения".into(),
            }
        }
        "task" => {
            if arg.is_empty() {
                return "Напишите задачу после команды: /task купить молоко завтра в 18:00".into();
            }
            let e = crate::planner::parse_document(&format!("- {arg}"), today, off, t).into_iter().next();
            let Some(e) = e else { return "Не поняла задачу".into() };
            // The owner's own task is just his; someone else's says who asked.
            let title = if own { e.title.clone() } else { format!("{} — {from}", e.title) };
            crate::planner::add(&title, e.due, "", false, "Задача", if e.high { "high" } else { "normal" });
            keep(&e.title, true);
            if !own {
                announce(format!("{from} поставил задачу: {}", e.title), true);
            }
            match e.due {
                Some(d) => format!("Добавила: {} — {}", e.title, crate::planner::say_time(d, t, off)),
                None => format!("Добавила: {}", e.title),
            }
        }
        "tasks" => {
            let plan = crate::planner::json();
            let items: Vec<&Value> = plan["items"].as_array().map(|a| a.iter().filter(|i| i["kind"] != "event").take(20).collect()).unwrap_or_default();
            if items.is_empty() {
                st(|s| s.listed.clear());
                return "Задач нет".into();
            }
            st(|s| {
                s.listed = items.iter().map(|i| (i["source"].as_str().unwrap_or("local").to_owned(), i["id"].as_str().unwrap_or("").to_owned(), i["list"].as_str().unwrap_or("").to_owned())).collect();
            });
            let lines: Vec<String> = items
                .iter()
                .enumerate()
                .map(|(n, i)| {
                    let when = if i["overdue"] == true { " — просрочено".to_owned() } else { i["time"].as_str().map(|x| format!(" — {x}")).unwrap_or_default() };
                    format!("{}. {}{when}", n + 1, i["title"].as_str().unwrap_or(""))
                })
                .collect();
            format!("{}\n\nСделано — /done и номер", lines.join("\n"))
        }
        "done" if !own => "Отмечать сделанным может только владелец колонки".into(),
        "done" => {
            let Some((source, id, list)) = arg.trim().parse::<usize>().ok().and_then(|n| n.checked_sub(1)).and_then(|n| st(|s| s.listed.get(n).cloned())) else {
                return "Сначала /tasks, потом /done и номер из списка".into();
            };
            let ok = if source == "google" { crate::calendar::complete_task(&list, &id).is_ok() } else { id.parse::<u64>().is_ok_and(|id| crate::planner::set_done(id, true)) };
            if ok { "Готово, отметила".into() } else { "Не получилось отметить — обновите список: /tasks".into() }
        }
        "say" if !own => "Говорить через колонку может только её владелец".into(),
        "say" => {
            let text: String = arg.split_whitespace().collect::<Vec<_>>().join(" ").chars().take(300).collect();
            if text.is_empty() {
                return "Напишите, что сказать: /say обед готов".into();
            }
            crate::alice::say(text);
            "Говорю".into()
        }
        _ => COMMANDS.iter().map(|(c, d)| format!("/{c} — {d}")).collect::<Vec<_>>().join("\n"),
    }
}

/// When the working day starts: what came while it was not.
pub fn run_summary() {
    thread::spawn(|| loop {
        thread::sleep(Duration::from_secs(60));
        if !CONFIG.lock().unwrap().as_ref().is_some_and(|c| c.on) || !working() {
            continue;
        }
        let (messages, tasks) = st(|s| std::mem::take(&mut s.held));
        if messages + tasks > 0 {
            let mut parts = Vec::new();
            if messages > 0 {
                parts.push(format!("сообщений: {messages}"));
            }
            if tasks > 0 {
                parts.push(format!("задач: {tasks}, они уже в планах"));
            }
            crate::display::notify("tg");
            crate::alice::say(format!("Пока вы не работали, в группе Телеграма вам написали. {}", parts.join(", ")));
        }
    });
}

/// «Очистить» on the panel.
pub fn clear() {
    st(|s| {
        s.items.clear();
        s.held = (0, 0);
    });
}

/// For the panel and /api/ditoo: the bot, what it sees, the last mentions.
pub fn json() -> Value {
    let on = CONFIG.lock().unwrap().as_ref().is_some_and(|c| c.on && !c.token.is_empty());
    st(|s| {
        json!({
            "on": on, "bot": s.bot, "sees_all": s.sees_all, "error": s.error,
            "count": s.items.len(), "tasks": s.items.iter().filter(|m| m.task).count(),
            "waiting": s.held.0 + s.held.1,
            "items": s.items.iter().rev().map(|m| json!({ "from": m.from, "chat": m.chat, "text": m.text, "at": m.at, "task": m.task })).collect::<Vec<_>>(),
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn who_a_message_is_for() {
        let m = |text: &str| json!({ "text": text, "chat": { "type": "supergroup" } });
        assert!(addressed(&m("@rodya глянь макет"), "rodya", "soundya_bot"));
        assert!(addressed(&m("Привет, @Soundya_Bot, запиши"), "rodya", "soundya_bot"));
        assert!(!addressed(&m("@rodyaother привет"), "rodya", "soundya_bot"), "another name that starts the same");
        assert!(!addressed(&m("всем привет"), "rodya", "soundya_bot"));
        assert!(!addressed(&m("@rodya"), "", ""), "no names set up");
        let reply = json!({ "text": "ок, сделаю", "reply_to_message": { "from": { "username": "Soundya_Bot" } } });
        assert!(addressed(&reply, "rodya", "soundya_bot"));
    }

    #[test]
    fn commands_are_read() {
        assert_eq!(command("/task купить молоко завтра", "bot"), Some(("task", "купить молоко завтра".into())));
        assert_eq!(command("/Задача@Bot   оплатить счёт", "bot"), Some(("task", "оплатить счёт".into())));
        assert_eq!(command("/tasks", "bot"), Some(("tasks", String::new())));
        assert_eq!(command("/созвон завтра в 12:00 релиз", "bot"), Some(("call", "завтра в 12:00 релиз".into())));
        assert_eq!(command("/срочно сервер лежит", "bot"), Some(("urgent", "сервер лежит".into())));
        assert_eq!(command("/urgent@bot x", "bot"), Some(("urgent", "x".into())));
        assert_eq!(command("/done 2", "bot"), Some(("done", "2".into())));
        assert_eq!(command("/task@otherbot x", "bot"), None, "another bot's command");
        assert_eq!(command("/weather", "bot"), None);
        assert_eq!(command("привет /task", "bot"), None);
    }

    #[test]
    fn a_task_or_a_message() {
        assert_eq!(task_title("@rodya сделай презу до 15.11").as_deref(), Some("сделай презу до 15.11"));
        assert_eq!(task_title("@rodya, нужно созвониться с клиентом").as_deref(), Some("нужно созвониться с клиентом"));
        assert_eq!(task_title("@rodya можешь глянуть макет?").as_deref(), Some("можешь глянуть макет?"));
        assert_eq!(task_title("@soundya_bot задача: обновить сайт").as_deref(), Some("задача: обновить сайт"));
        for message in ["@rodya ты где?", "@rodya спасибо!", "@rodya привет, как дела", "@rodya"] {
            assert_eq!(task_title(message), None, "{message}");
        }
    }
}
