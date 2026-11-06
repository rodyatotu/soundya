//! Google Calendar and Google Tasks, both ways, with the user's own OAuth
//! client: soundya ships no Google keys (nothing secret in the sources).
//!
//! Setup, once, in the panel («Планы» → «Google»): the user makes an OAuth
//! client of type «Приложение для ПК» in their Google Cloud project (Calendar
//! API and Tasks API on), pastes its ID and secret, presses «Войти в Google»,
//! signs in, and pastes back the address the browser ended on
//! (http://127.0.0.1:…/?code=… — the page itself does not open, that is
//! expected: the speaker cannot be a Google redirect target, so the code comes
//! by hand). PKCE (RFC 7636) protects that code. The refresh token is a secret
//! like the SoundCloud token: config.json only, masked on the page, cut out of
//! reports.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use crate::planner::{civil, days_from_civil, local};

pub const REDIRECT: &str = "http://127.0.0.1:53682/soundya";
const SCOPES: &str =
    "https://www.googleapis.com/auth/calendar.readonly https://www.googleapis.com/auth/calendar.events https://www.googleapis.com/auth/tasks";

fn auth_base() -> String {
    std::env::var("SOUNDYA_GOOGLE_AUTH").unwrap_or_else(|_| "https://accounts.google.com".into())
}
fn token_url() -> String {
    std::env::var("SOUNDYA_GOOGLE_OAUTH").map(|b| format!("{b}/token")).unwrap_or_else(|_| "https://oauth2.googleapis.com/token".into())
}
fn api_base() -> String {
    std::env::var("SOUNDYA_GOOGLE_API").unwrap_or_else(|_| "https://www.googleapis.com".into())
}

#[derive(Clone, Default, PartialEq)]
pub struct Config {
    pub client_id: String,
    pub secret: String,
    pub refresh: String,
}

static CFG: Mutex<Option<Config>> = Mutex::new(None);
static TOKEN: Mutex<Option<(String, Instant)>> = Mutex::new(None);
/// (verifier, state, when) of the sign-in in progress.
static PENDING: Mutex<Option<(String, String, Instant)>> = Mutex::new(None);

pub fn configure(c: Config) {
    let mut g = CFG.lock().unwrap();
    if g.as_ref() != Some(&c) {
        *TOKEN.lock().unwrap() = None;
    }
    *g = Some(c);
}

fn cfg() -> Config {
    CFG.lock().unwrap().clone().unwrap_or_default()
}

/// Signed in (a refresh token is saved).
pub fn connected() -> bool {
    let c = cfg();
    !c.refresh.is_empty() && !c.client_id.is_empty()
}

// ---------------------------------------------------------------- encoding

pub fn b64url(data: &[u8]) -> String {
    crate::pc::base64(data).replace('+', "-").replace('/', "_").trim_end_matches('=').to_owned()
}

/// RFC 7636: BASE64URL(SHA256(verifier)).
pub fn challenge(verifier: &str) -> String {
    b64url(&crate::sha256::sha256(verifier.as_bytes()))
}

fn random(n: usize) -> Vec<u8> {
    let mut b = vec![0u8; n];
    if std::fs::File::open("/dev/urandom").and_then(|mut f| std::io::Read::read_exact(&mut f, &mut b)).is_err() {
        // No urandom (never on the speaker): hash the clock and the address of a local.
        let seed = format!("{:?}{:p}", std::time::SystemTime::now(), &b);
        let h = crate::sha256::sha256(seed.as_bytes());
        for (i, x) in b.iter_mut().enumerate() {
            *x = h[i % 32];
        }
    }
    b
}

/// Path segments and form values.
pub fn enc(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

fn dec(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(if b[i] == b'+' { b' ' } else { b[i] });
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// RFC 3339 → unix time: «2026-11-06T15:00:00+03:00», «…Z», «…​.000Z».
pub fn parse_rfc3339(s: &str) -> Option<i64> {
    let s = s.trim();
    let n = |a: usize, b: usize| s.get(a..b).and_then(|x| x.parse::<i64>().ok());
    let (y, mo, d) = (n(0, 4)?, n(5, 7)?, n(8, 10)?);
    if s.as_bytes().get(10).map(u8::to_ascii_uppercase) != Some(b'T') {
        return None;
    }
    let (h, mi, se) = (n(11, 13)?, n(14, 16)?, n(17, 19)?);
    let rest = &s[19..];
    let rest = rest.strip_prefix('.').map_or(rest, |r| r.trim_start_matches(|c: char| c.is_ascii_digit()));
    let off = match rest.as_bytes().first() {
        Some(b'Z' | b'z') | None => 0,
        Some(b'+' | b'-') => {
            let sign = if rest.starts_with('-') { -1 } else { 1 };
            let oh = rest.get(1..3)?.parse::<i64>().ok()?;
            let om = rest.get(4..6).and_then(|x| x.parse::<i64>().ok()).unwrap_or(0);
            sign * (oh * 3600 + om * 60)
        }
        _ => return None,
    };
    Some(days_from_civil(y, mo as u32, d as u32) * 86_400 + h * 3600 + mi * 60 + se - off)
}

/// Unix time → RFC 3339 in the speaker's offset: «2026-11-06T15:00:00+03:00».
pub fn rfc3339(t: i64, off: i64) -> String {
    let (day, secs) = local(t, off);
    let (y, m, d) = civil(day);
    let sign = if off < 0 { '-' } else { '+' };
    let a = off.abs();
    format!("{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}{sign}{:02}:{:02}", secs / 3600, secs / 60 % 60, secs % 60, a / 3600, a / 60 % 60)
}

/// A local day number → «2026-11-06».
pub fn date(day: i64) -> String {
    let (y, m, d) = civil(day);
    format!("{y:04}-{m:02}-{d:02}")
}

fn parse_date(s: &str) -> Option<i64> {
    let n = |a: usize, b: usize| s.get(a..b).and_then(|x| x.parse::<i64>().ok());
    Some(days_from_civil(n(0, 4)?, n(5, 7)? as u32, n(8, 10)? as u32))
}

// ---------------------------------------------------------------- sign-in

/// The address to open to sign in; remembers the PKCE verifier.
pub fn auth_url(client_id: &str) -> String {
    let verifier = b64url(&random(48));
    let state = b64url(&random(12));
    let url = format!(
        "{}/o/oauth2/v2/auth?client_id={}&redirect_uri={}&response_type=code&scope={}&access_type=offline&prompt=consent&code_challenge={}&code_challenge_method=S256&state={}",
        auth_base(),
        enc(client_id.trim()),
        enc(REDIRECT),
        enc(SCOPES),
        challenge(&verifier),
        state
    );
    *PENDING.lock().unwrap() = Some((verifier, state, Instant::now()));
    url
}

/// What the user pasted: the whole address (…?code=…&state=…&scope=…), or the code alone.
pub fn code_from(pasted: &str) -> Option<(String, Option<String>)> {
    let p = pasted.trim();
    if p.is_empty() {
        return None;
    }
    if let Some(q) = p.split_once('?').map(|x| x.1).or_else(|| p.contains("code=").then_some(p)) {
        let mut code = None;
        let mut state = None;
        for kv in q.split(['&', '#']) {
            match kv.split_once('=') {
                Some(("code", v)) => code = Some(dec(v)),
                Some(("state", v)) => state = Some(dec(v)),
                Some(("error", v)) => return Some((format!("error:{}", dec(v)), None)),
                _ => {}
            }
        }
        return code.filter(|c| !c.is_empty()).map(|c| (c, state));
    }
    (!p.contains(char::is_whitespace) && !p.contains("://") && p.len() > 10).then(|| (p.to_owned(), None))
}

fn google_error(e: ureq::Error) -> String {
    match e {
        ureq::Error::Status(code, resp) => {
            let v: Value = resp.into_json().unwrap_or(Value::Null);
            let err = v["error"].as_str().map(str::to_owned).or_else(|| v["error"]["status"].as_str().map(str::to_owned)).unwrap_or_default();
            let msg = v["error_description"].as_str().or_else(|| v["error"]["message"].as_str()).unwrap_or("").chars().take(200).collect::<String>();
            match err.as_str() {
                "invalid_grant" => "вход в Google устарел или отозван — войдите заново (в режиме «Тестирование» Google отзывает вход через 7 дней)".into(),
                "invalid_client" | "unauthorized_client" => "Google не принял ID или секрет клиента — проверьте, что клиент типа «Приложение для ПК»".into(),
                "PERMISSION_DENIED" if msg.contains("has not been used") || msg.contains("disabled") => {
                    "в проекте Google Cloud не включён Calendar API или Tasks API".into()
                }
                _ => format!("Google ответил {code}{}{}", if err.is_empty() { "" } else { ": " }, if msg.is_empty() { err } else { msg }),
            }
        }
        ureq::Error::Transport(t) => format!("нет связи с Google ({})", t.kind()),
    }
}

/// Exchanges the pasted code; returns the refresh token to save.
pub fn exchange(agent: &ureq::Agent, client_id: &str, secret: &str, pasted: &str) -> Result<String, String> {
    let (code, state) = code_from(pasted).ok_or("вставьте адрес из строки браузера целиком (он начинается с http://127.0.0.1)")?;
    if let Some(e) = code.strip_prefix("error:") {
        return Err(if e == "access_denied" { "вход отменён в Google".into() } else { format!("Google: {e}") });
    }
    let (verifier, expect) = match PENDING.lock().unwrap().clone() {
        Some((v, s, at)) if at.elapsed() < Duration::from_secs(1800) => (v, s),
        _ => return Err("сначала нажмите «Войти в Google» (ссылка действует 30 минут)".into()),
    };
    if state.as_deref().is_some_and(|s| s != expect) {
        return Err("это адрес от другой попытки входа — нажмите «Войти в Google» ещё раз".into());
    }
    let v: Value = agent
        .post(&token_url())
        .timeout(Duration::from_secs(20))
        .send_form(&[
            ("code", code.as_str()),
            ("client_id", client_id.trim()),
            ("client_secret", secret.trim()),
            ("redirect_uri", REDIRECT),
            ("grant_type", "authorization_code"),
            ("code_verifier", verifier.as_str()),
        ])
        .map_err(google_error)?
        .into_json()
        .map_err(|e| e.to_string())?;
    let refresh = v["refresh_token"].as_str().ok_or("Google не выдал постоянный вход — удалите доступ soundya в myaccount.google.com → Безопасность и войдите снова")?;
    if let Some(a) = v["access_token"].as_str() {
        let ttl = v["expires_in"].as_u64().unwrap_or(3600).saturating_sub(120);
        *TOKEN.lock().unwrap() = Some((a.to_owned(), Instant::now() + Duration::from_secs(ttl)));
    }
    *PENDING.lock().unwrap() = None;
    Ok(refresh.to_owned())
}

fn access(agent: &ureq::Agent) -> Result<String, String> {
    if let Some((t, until)) = TOKEN.lock().unwrap().clone() {
        if Instant::now() < until {
            return Ok(t);
        }
    }
    let c = cfg();
    if c.refresh.is_empty() {
        return Err("не выполнен вход в Google".into());
    }
    let v: Value = agent
        .post(&token_url())
        .timeout(Duration::from_secs(20))
        .send_form(&[
            ("client_id", c.client_id.trim()),
            ("client_secret", c.secret.trim()),
            ("refresh_token", c.refresh.as_str()),
            ("grant_type", "refresh_token"),
        ])
        .map_err(google_error)?
        .into_json()
        .map_err(|e| e.to_string())?;
    let t = v["access_token"].as_str().ok_or("Google не выдал ключ доступа")?.to_owned();
    let ttl = v["expires_in"].as_u64().unwrap_or(3600).saturating_sub(120);
    *TOKEN.lock().unwrap() = Some((t.clone(), Instant::now() + Duration::from_secs(ttl)));
    Ok(t)
}

/// One API call; an expired key is renewed once.
fn call(agent: &ureq::Agent, method: &str, path: &str, query: &[(&str, String)], body: Option<&Value>) -> Result<Value, String> {
    for attempt in 0..2 {
        let token = access(agent)?;
        let mut req = agent.request(method, &format!("{}{path}", api_base())).timeout(Duration::from_secs(25)).set("Authorization", &format!("Bearer {token}"));
        for (k, v) in query {
            req = req.query(k, v);
        }
        let r = match body {
            Some(b) => req.send_json(b.clone()),
            None => req.call(),
        };
        match r {
            Ok(resp) => {
                let text = resp.into_string().map_err(|e| e.to_string())?;
                return Ok(if text.trim().is_empty() { Value::Null } else { serde_json::from_str(&text).map_err(|e| e.to_string())? });
            }
            Err(ureq::Error::Status(401, _)) if attempt == 0 => *TOKEN.lock().unwrap() = None,
            Err(e) => return Err(google_error(e)),
        }
    }
    Err("Google не принял вход".into())
}

// ---------------------------------------------------------------- calendars

#[derive(Debug, Clone, PartialEq)]
pub struct Cal {
    pub id: String,
    pub name: String,
    pub primary: bool,
    pub writable: bool,
    /// Shown in Google Calendar's own list.
    pub selected: bool,
}

pub fn parse_calendars(v: &Value) -> Vec<Cal> {
    v["items"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|c| {
                    Some(Cal {
                        id: c["id"].as_str()?.to_owned(),
                        name: c["summaryOverride"].as_str().or_else(|| c["summary"].as_str()).unwrap_or("Календарь").to_owned(),
                        primary: c["primary"].as_bool().unwrap_or(false),
                        writable: matches!(c["accessRole"].as_str(), Some("owner" | "writer")),
                        selected: c["selected"].as_bool().unwrap_or(false) || c["primary"].as_bool().unwrap_or(false),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

pub fn calendars(agent: &ureq::Agent) -> Result<Vec<Cal>, String> {
    call(agent, "GET", "/calendar/v3/users/me/calendarList", &[("maxResults", "100".into())], None).map(|v| parse_calendars(&v))
}

/// (id, title, start, end, all-day).
pub type GEvent = (String, String, i64, i64, bool);

/// Each event; cancelled ones dropped.
pub fn parse_events(v: &Value, off: i64) -> Vec<GEvent> {
    v["items"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter(|e| e["status"].as_str() != Some("cancelled"))
                .filter_map(|e| {
                    let title = e["summary"].as_str().unwrap_or("Событие").trim().to_owned();
                    let id = e["id"].as_str()?.to_owned();
                    if let Some(s) = e["start"]["dateTime"].as_str() {
                        let start = parse_rfc3339(s)?;
                        let end = e["end"]["dateTime"].as_str().and_then(parse_rfc3339).unwrap_or(start + 3600);
                        Some((id, title, start, end, false))
                    } else {
                        let d = parse_date(e["start"]["date"].as_str()?)?;
                        let end = e["end"]["date"].as_str().and_then(parse_date).unwrap_or(d + 1);
                        Some((id, title, d * 86_400 - off, end * 86_400 - off, true))
                    }
                })
                .collect()
        })
        .unwrap_or_default()
}

pub fn events(agent: &ureq::Agent, cal: &str, from: i64, to: i64, off: i64) -> Result<Vec<GEvent>, String> {
    let v = call(
        agent,
        "GET",
        &format!("/calendar/v3/calendars/{}/events", enc(cal)),
        &[
            ("timeMin", rfc3339(from, 0).replace("+00:00", "Z")),
            ("timeMax", rfc3339(to, 0).replace("+00:00", "Z")),
            ("singleEvents", "true".into()),
            ("orderBy", "startTime".into()),
            ("maxResults", "250".into()),
        ],
        None,
    )?;
    Ok(parse_events(&v, off))
}

/// A new event; `start..end` or a whole day. Returns its id.
pub fn add_event(agent: &ureq::Agent, cal: &str, title: &str, start: i64, end: i64, all_day: bool, off: i64) -> Result<String, String> {
    let body = if all_day {
        let (d, _) = local(start, off);
        json!({ "summary": title, "start": { "date": date(d) }, "end": { "date": date(d + 1) } })
    } else {
        json!({ "summary": title, "start": { "dateTime": rfc3339(start, off) }, "end": { "dateTime": rfc3339(end, off) } })
    };
    let v = call(agent, "POST", &format!("/calendar/v3/calendars/{}/events", enc(cal)), &[], Some(&body))?;
    Ok(v["id"].as_str().unwrap_or("").to_owned())
}

// ---------------------------------------------------------------- tasks

#[derive(Debug, Clone, PartialEq)]
pub struct GTask {
    pub id: String,
    pub list: String,
    pub title: String,
    pub notes: String,
    /// The due date as a local day number (Google keeps no time for tasks).
    pub due_day: Option<i64>,
}

pub fn parse_lists(v: &Value) -> Vec<(String, String)> {
    v["items"]
        .as_array()
        .map(|a| a.iter().filter_map(|l| Some((l["id"].as_str()?.to_owned(), l["title"].as_str().unwrap_or("Задачи").to_owned()))).collect())
        .unwrap_or_default()
}

pub fn parse_tasks(v: &Value, list: &str) -> Vec<GTask> {
    v["items"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter(|t| t["status"].as_str() != Some("completed") && !t["deleted"].as_bool().unwrap_or(false))
                .filter_map(|t| {
                    let title = t["title"].as_str()?.trim().to_owned();
                    (!title.is_empty()).then(|| GTask {
                        id: t["id"].as_str().unwrap_or("").to_owned(),
                        list: list.to_owned(),
                        title,
                        notes: t["notes"].as_str().unwrap_or("").chars().take(300).collect(),
                        // «2026-11-06T00:00:00.000Z» means the date 6 November.
                        due_day: t["due"].as_str().and_then(parse_date),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

pub fn task_lists(agent: &ureq::Agent) -> Result<Vec<(String, String)>, String> {
    call(agent, "GET", "/tasks/v1/users/@me/lists", &[("maxResults", "100".into())], None).map(|v| parse_lists(&v))
}

pub fn tasks(agent: &ureq::Agent, list: &str) -> Result<Vec<GTask>, String> {
    let v = call(
        agent,
        "GET",
        &format!("/tasks/v1/lists/{}/tasks", enc(list)),
        &[("showCompleted", "false".into()), ("showHidden", "false".into()), ("maxResults", "100".into())],
        None,
    )?;
    Ok(parse_tasks(&v, list))
}

pub fn add_task(agent: &ureq::Agent, list: &str, title: &str, due_day: Option<i64>, notes: &str) -> Result<String, String> {
    let mut body = json!({ "title": title });
    if let Some(d) = due_day {
        body["due"] = format!("{}T00:00:00.000Z", date(d)).into();
    }
    if !notes.is_empty() {
        body["notes"] = notes.into();
    }
    let v = call(agent, "POST", &format!("/tasks/v1/lists/{}/tasks", enc(list)), &[], Some(&body))?;
    Ok(v["id"].as_str().unwrap_or("").to_owned())
}

pub fn complete_task(agent: &ureq::Agent, list: &str, id: &str) -> Result<(), String> {
    call(agent, "PATCH", &format!("/tasks/v1/lists/{}/tasks/{}", enc(list), enc(id)), &[], Some(&json!({ "status": "completed" }))).map(|_| ())
}

pub fn delete_task(agent: &ureq::Agent, list: &str, id: &str) -> Result<(), String> {
    call(agent, "DELETE", &format!("/tasks/v1/lists/{}/tasks/{}", enc(list), enc(id)), &[], None).map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    const MSK: i64 = 3 * 3600;

    #[test]
    fn pkce_and_encoding() {
        // RFC 7636, appendix B.
        assert_eq!(challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"), "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM");
        assert_eq!(enc("me@gmail.com"), "me%40gmail.com");
        assert_eq!(enc("ru.russian#holiday@group.v.calendar.google.com"), "ru.russian%23holiday%40group.v.calendar.google.com");
        let url = auth_url("123-abc.apps.googleusercontent.com");
        assert!(url.contains("code_challenge_method=S256") && url.contains("access_type=offline") && url.contains("redirect_uri=http%3A%2F%2F127.0.0.1"));
        assert!(url.contains("calendar.events") && url.contains("%2Fauth%2Ftasks"));
    }

    #[test]
    fn reads_the_pasted_address() {
        let (c, s) = code_from("http://127.0.0.1:53682/soundya?state=xyz&code=4%2F0AQlEd8x-abc_DEF&scope=https://www.googleapis.com/auth/tasks").unwrap();
        assert_eq!(c, "4/0AQlEd8x-abc_DEF");
        assert_eq!(s.as_deref(), Some("xyz"));
        assert_eq!(code_from("  4/0AQlEd8x-abc_DEF  ").unwrap().0, "4/0AQlEd8x-abc_DEF");
        assert_eq!(code_from("http://127.0.0.1:53682/soundya?error=access_denied&state=x").unwrap().0, "error:access_denied");
        assert!(code_from("").is_none());
        assert!(code_from("http://127.0.0.1:53682/soundya").is_none());
    }

    #[test]
    fn times() {
        // 2026-11-06 12:00 UTC.
        const NOW: i64 = 1_793_966_400;
        assert_eq!(parse_rfc3339("2026-11-06T15:00:00+03:00"), Some(NOW));
        assert_eq!(parse_rfc3339("2026-11-06T12:00:00Z"), Some(NOW));
        assert_eq!(parse_rfc3339("2026-11-06T12:00:00.000Z"), Some(NOW));
        assert_eq!(parse_rfc3339("2026-11-06T07:00:00-05:00"), Some(NOW));
        assert_eq!(parse_rfc3339("2026-11-06"), None);
        assert_eq!(rfc3339(NOW, MSK), "2026-11-06T15:00:00+03:00");
        assert_eq!(rfc3339(NOW, -5 * 3600 - 1800), "2026-11-06T06:30:00-05:30");
        assert_eq!(parse_rfc3339(&rfc3339(NOW + 59, MSK)), Some(NOW + 59));
    }

    #[test]
    fn reads_api_answers() {
        let cals = parse_calendars(&json!({ "items": [
            { "id": "me@gmail.com", "summary": "me@gmail.com", "summaryOverride": "Личный", "primary": true, "accessRole": "owner" },
            { "id": "ru.russian#holiday@group.v.calendar.google.com", "summary": "Праздники", "accessRole": "reader", "selected": true },
            { "id": "work", "summary": "Работа", "accessRole": "writer" },
        ]}));
        assert_eq!(cals.len(), 3);
        assert!(cals[0].primary && cals[0].writable && cals[0].selected && cals[0].name == "Личный");
        assert!(!cals[1].writable && cals[1].selected);
        assert!(cals[2].writable && !cals[2].selected);
        let ev = parse_events(
            &json!({ "items": [
                { "id": "a", "summary": "Планёрка", "start": { "dateTime": "2026-11-06T10:00:00+03:00" }, "end": { "dateTime": "2026-11-06T10:30:00+03:00" } },
                { "id": "b", "summary": "Отпуск", "start": { "date": "2026-11-07" }, "end": { "date": "2026-11-09" } },
                { "id": "c", "status": "cancelled" },
                { "id": "d", "start": { "dateTime": "2026-11-06T18:00:00Z" } },
            ]}),
            MSK,
        );
        assert_eq!(ev.len(), 3);
        assert_eq!(ev[0].1, "Планёрка");
        assert_eq!(ev[0].3 - ev[0].2, 1800);
        assert!(ev[1].4 && ev[1].3 - ev[1].2 == 2 * 86_400);
        assert_eq!((ev[2].1.as_str(), ev[2].3 - ev[2].2), ("Событие", 3600), "no title, no end");
        let t = parse_tasks(
            &json!({ "items": [
                { "id": "t1", "title": "Сдать отчёт", "due": "2026-11-06T00:00:00.000Z", "notes": "до 18:00", "status": "needsAction" },
                { "id": "t2", "title": "Готово", "status": "completed" },
                { "id": "t3", "title": "Без срока", "status": "needsAction" },
                { "id": "t4", "title": "", "status": "needsAction" },
            ]}),
            "L",
        );
        assert_eq!(t.len(), 2);
        assert_eq!(t[0].due_day, Some(days_from_civil(2026, 11, 6)));
        assert_eq!(t[0].notes, "до 18:00");
        assert_eq!(t[1].due_day, None);
        assert_eq!(parse_lists(&json!({ "items": [{ "id": "MDE", "title": "Мои задачи" }] })), vec![("MDE".to_owned(), "Мои задачи".to_owned())]);
    }
}
