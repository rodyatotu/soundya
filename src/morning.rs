//! Developer build only for now. The morning after the alarm (alarm.rs): once it is switched off by the right answer,
//! an intro track of the user's choice plays for a moment, then Alice greets («Доброе утро!»
//! unless changed), tells the weather, the day's plans and the headlines of a
//! news feed over the music, played quieter meanwhile.
//!
//! The feeds are plain RSS or open Telegram channels. Not the first five headlines in a row:
//! what was read out before is not read again, posts older than a day and a half, adverts and
//! the user's «не читать» words are left out, one story told by several channels is told once
//! (and counts for more), and what a channel's readers looked at more than usual goes first —
//! see `pick`. No AI in it: the views, the time and the words are all the speaker has.

use std::{
    sync::{mpsc::Sender, Arc},
    thread,
    time::Duration,
};

use crate::{settings::Settings, Cmd, Shared};

pub const DEFAULT_GREET: &str = "Доброе утро!";
/// Headlines read out.
const HEADLINES: usize = 5;
/// How long the intro track plays before the greeting.
const INTRO_FOR: Duration = Duration::from_secs(20);

/// The feeds to pick from on the panel: key, name, address.
pub const SOURCES: [(&str, &str, &str); 4] = [
    ("ria", "РИА Новости", "https://ria.ru/export/rss2/archive/index.xml"),
    ("rbc", "РБК", "https://rssexport.rbc.ru/rbcnews/news/30/full.rss"),
    ("lenta", "Лента.ру", "https://lenta.ru/rss/news"),
    ("habr", "Хабр", "https://habr.com/ru/rss/news/?fl=ru"),
];

/// Telegram channels to read: «tg:@meduzalive, rbc_news, t.me/breakingmash» → their names.
pub fn tg_channels(source: &str) -> Option<Vec<String>> {
    let list = source.trim().strip_prefix("tg:")?;
    let names: Vec<String> = list
        .split([',', ' ', ';', '\n'])
        .map(|n| n.trim().trim_start_matches("https://").trim_start_matches("t.me/s/").trim_start_matches("t.me/").trim_start_matches('@'))
        .map(|n| n.split(['/', '?']).next().unwrap_or("").to_owned())
        .filter(|n| !n.is_empty() && n.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'))
        .take(10)
        .collect();
    (!names.is_empty()).then_some(names)
}

/// One piece of news: a channel's post or a feed's item.
#[derive(Debug, Clone, PartialEq)]
pub struct Post {
    /// «channel/123», or the title itself where there is no number (RSS, an old news server).
    pub id: String,
    /// The channel or the feed.
    pub source: String,
    /// As it is read out: the headline and, if that is short, a sentence of detail.
    pub text: String,
    /// Everything the post says, lower case: for the adverts and the user's words.
    pub body: String,
    pub views: Option<u64>,
    /// Unix seconds.
    pub time: Option<u64>,
}

const TEXT_MARK: &str = "tgme_widget_message_text js-message_text";

/// What follows `key` in `chunk` up to a quote or a tag.
fn after<'a>(chunk: &'a str, key: &str) -> Option<&'a str> {
    let rest = &chunk[chunk.find(key)? + key.len()..];
    rest.split(['"', '<']).next()
}

/// «12.3K» → 12300, «1.2M», «845».
fn views_number(t: &str) -> Option<u64> {
    let t = t.trim();
    let (num, mul) = match t.chars().last()? {
        'K' | 'k' => (&t[..t.len() - 1], 1e3),
        'M' | 'm' => (&t[..t.len() - 1], 1e6),
        _ => (t, 1.0),
    };
    num.replace(',', ".").parse::<f64>().ok().filter(|v| *v >= 0.0).map(|v| (v * mul) as u64)
}

/// «2026-11-14T10:05:00+00:00» (Telegram's pages are in UTC) or plain unix seconds.
fn post_time(t: &str) -> Option<u64> {
    if let Ok(n) = t.parse::<u64>() {
        return Some(n);
    }
    let num = |a: usize, b: usize| t.get(a..b)?.parse::<i64>().ok();
    let (y, mo, d, h, mi) = (num(0, 4)?, num(5, 7)?, num(8, 10)?, num(11, 13)?, num(14, 16)?);
    let days = crate::planner::days_from_civil(y, mo as u32, d as u32);
    u64::try_from(days * 86_400 + h * 3600 + mi * 60).ok()
}

/// The text block of a post → (as read out, everything in lower case).
fn post_text(raw: &str) -> Option<(String, String)> {
    let lines: Vec<String> = raw.split("<br/>").map(clean).map(|l| speakable(&l)).filter(|l| !l.is_empty()).collect();
    // The headline: the first line, or two when the first is only «⚡️Срочно».
    let mut head = lines.first().cloned()?;
    let mut used = 1;
    if head.chars().count() < 25 {
        if let Some(next) = lines.get(1) {
            head = format!("{head}. {next}");
            used = 2;
        }
    }
    let mut head = cut(&head, 220);
    // A short headline gets a sentence of what happened: «…пройдёт с 26 по 30 ноября. Valve
    // предложила разработчикам запланировать скидки на эти даты».
    if head.chars().count() < 140 {
        let detail = lines[used.min(lines.len())..].iter().find(|l| {
            let low = l.to_lowercase();
            l.chars().count() >= 30 && !["подпис", "читать дал", "источник", "подробнее"].iter().any(|w| low.contains(w))
        });
        if let Some(d) = detail {
            let first = d.split_inclusive(['.', '!', '?']).next().unwrap_or(d);
            let first = if first.chars().count() >= 30 { first } else { d.as_str() };
            // The two together: a phrase, not a paragraph.
            let room = 260usize.saturating_sub(head.chars().count()).max(80);
            head = format!("{}. {}", head.trim_end_matches(['.', '!', '?']), cut(first, room).trim_end_matches(['.', ' ']));
        }
    }
    (head.chars().count() >= 12).then(|| (head, lines.join(" ").to_lowercase().replace('ё', "е")))
}

/// The posts of a channel's page, oldest first. Three shapes of it: the news server's
/// (`<div class="soundya_post" data-post=… data-views=… data-time=…>` before each text
/// block), Telegram's own t.me/s/ page, and an old news server's (text blocks only).
pub fn tg_parse(html: &str, channel: &str) -> Vec<Post> {
    let text_of = |chunk: &str| -> Option<(String, String)> {
        let rest = &chunk[chunk.find(TEXT_MARK)? + TEXT_MARK.len()..];
        let body = &rest[rest.find('>')? + 1..];
        post_text(&body[..body.find("</div>").unwrap_or(body.len())])
    };
    let mut out = Vec::new();
    if html.contains("data-post=\"") {
        for chunk in html.split("data-post=\"").skip(1) {
            let Some((text, body)) = text_of(chunk) else { continue };
            let id = chunk.split('"').next().unwrap_or("").to_owned();
            let views = after(chunk, "data-views=\"").or_else(|| after(chunk, "tgme_widget_message_views\">")).and_then(views_number);
            let time = after(chunk, "data-time=\"").or_else(|| after(chunk, "<time datetime=\"")).and_then(post_time);
            out.push(Post { id, source: channel.to_owned(), text, body, views, time });
        }
    } else {
        for chunk in html.split("<div class=\"tgme_widget_message_text").skip(1) {
            let chunk = format!("tgme_widget_message_text{chunk}");
            if let Some((text, body)) = text_of(&chunk) {
                out.push(Post { id: text.clone(), source: channel.to_owned(), text, body, views: None, time: None });
            }
        }
    }
    out
}

/// «игры, steam, нейросет» → the words, lower case.
pub fn word_list(list: &str) -> Vec<String> {
    list.split([',', ';', '\n']).map(|w| w.trim().to_lowercase().replace('ё', "е")).filter(|w| w.chars().count() >= 2).take(40).collect()
}

/// Adverts and draws: never news.
const ADS: [&str; 9] = ["erid", "реклама", "#реклама", "розыгрыш", "разыгрыва", "промокод", "партнерский материал", "giveaway", "на правах рекламы"];

/// The words a story is known by: four letters and more, cut to five (a crude stem).
fn stems(text: &str) -> std::collections::HashSet<String> {
    text.to_lowercase()
        .replace('ё', "е")
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| w.chars().count() >= 4)
        .map(|w| w.chars().take(5).collect())
        .collect()
}

/// The same story in other words: half of the shorter one's words are in the other.
fn same_story(a: &std::collections::HashSet<String>, b: &std::collections::HashSet<String>) -> bool {
    let common = a.intersection(b).count();
    common >= 3 && common * 2 >= a.len().min(b.len())
}

/// A day and a half: yesterday evening's news is still news in the morning.
const FRESH_SECS: u64 = 36 * 3600;

/// What to read out, the most worth hearing first, `max` at most.
/// - Left out: told before (`told`), older than a day and a half, adverts, the user's `skip` words.
/// - A post's weight: its views against what is usual for its channel (views per hour of age,
///   over the channel's median) — a post read twice as much as usual weighs 2. No views
///   (RSS): 1, the feed's own order stays.
/// - The user's `like` words double it.
/// - One story from several sources is told once, in the weightiest wording, and weighs half
///   as much again for every other source that told it.
/// - Two at most from one channel when there are several.
pub fn pick(posts: Vec<Post>, now: u64, told: &std::collections::HashSet<String>, like: &[String], skip: &[String], max: usize) -> Vec<Post> {
    use std::collections::HashMap;
    let rate = |p: &Post| -> Option<f64> {
        let age_h = p.time.map_or(12.0, |t| now.saturating_sub(t) as f64 / 3600.0);
        p.views.map(|v| v as f64 / (age_h + 3.0))
    };
    // What is usual for each channel: over everything on its page, read before or not.
    let mut usual: HashMap<&str, Vec<f64>> = HashMap::new();
    for p in &posts {
        if let Some(r) = rate(p) {
            usual.entry(p.source.as_str()).or_default().push(r);
        }
    }
    let usual: HashMap<String, f64> = usual
        .into_iter()
        .map(|(k, mut v)| {
            v.sort_by(|a, b| a.total_cmp(b));
            (k.to_owned(), v[v.len() / 2].max(0.001))
        })
        .collect();
    let sources = posts.iter().map(|p| p.source.as_str()).collect::<std::collections::HashSet<_>>().len();
    let mut scored: Vec<(f64, Post)> = posts
        .into_iter()
        .rev() // newest first: equal weights keep that order
        .filter(|p| !told.contains(&p.id))
        .filter(|p| p.time.is_none_or(|t| now.saturating_sub(t) <= FRESH_SECS))
        .filter(|p| !ADS.iter().any(|w| p.body.contains(w)) && !skip.iter().any(|w| p.body.contains(w.as_str())))
        .map(|p| {
            let mut score = match (rate(&p), usual.get(&p.source)) {
                (Some(r), Some(u)) => (r / u).clamp(0.2, 5.0),
                _ => 1.0,
            };
            if like.iter().any(|w| p.body.contains(w.as_str())) {
                score *= 2.0;
            }
            (score, p)
        })
        .collect();
    scored.sort_by(|a, b| b.0.total_cmp(&a.0));
    // One story, once.
    let mut kept: Vec<(f64, Post, std::collections::HashSet<String>, Vec<String>)> = Vec::new();
    for (score, p) in scored {
        let words = stems(&p.text);
        match kept.iter_mut().find(|k| k.1.text == p.text || same_story(&k.2, &words)) {
            Some(k) => {
                if k.1.source != p.source && !k.3.contains(&p.source) {
                    k.0 *= 1.5;
                    k.3.push(p.source.clone());
                }
            }
            None => kept.push((score, p, words, Vec::new())),
        }
    }
    kept.sort_by(|a, b| b.0.total_cmp(&a.0));
    let per_source = if sources > 1 { 2 } else { max };
    let mut count: HashMap<String, usize> = HashMap::new();
    let mut out: Vec<Post> = Vec::new();
    let mut spare: Vec<Post> = Vec::new();
    for (_, p, _, _) in kept {
        let n = count.entry(p.source.clone()).or_default();
        if *n < per_source && out.len() < max {
            *n += 1;
            out.push(p);
        } else {
            spare.push(p);
        }
    }
    // A quiet day in the other channels: the rest of the busy one after all.
    out.extend(spare.into_iter().take(max.saturating_sub(out.len())));
    out
}

/// What was read out before (the last 500), one id a line, in `dir`/news-told.
pub fn told(dir: &std::path::Path) -> std::collections::HashSet<String> {
    crate::fsx::read_string(&dir.join("news-told")).unwrap_or_default().lines().map(str::to_owned).collect()
}

pub fn remember_told(dir: &std::path::Path, posts: &[Post]) {
    let old = crate::fsx::read_string(&dir.join("news-told")).unwrap_or_default();
    let mut ids: Vec<&str> = old.lines().collect();
    ids.extend(posts.iter().map(|p| p.id.as_str()).filter(|id| !id.contains('\n')));
    let from = ids.len().saturating_sub(500);
    let _ = crate::settings::write_synced(&dir.join("news-told"), ids[from..].join("\n").as_bytes());
}

/// Letters, digits and plain punctuation only: emoji and symbols are read out as nonsense.
fn speakable(t: &str) -> String {
    let s: String = t
        .chars()
        .map(|c| if c.is_alphanumeric() || " .,:;!?—–-«»\"'()%№+".contains(c) { c } else { ' ' })
        .collect();
    s.split_whitespace().collect::<Vec<_>>().join(" ").trim_matches(|c: char| " .,:;—–-".contains(c)).to_owned()
}

/// At most `max` letters, ended at a sentence (or a word) if it had to be cut.
fn cut(t: &str, max: usize) -> String {
    if t.chars().count() <= max {
        return t.to_owned();
    }
    let head: String = t.chars().take(max).collect();
    match head.rfind(['.', '!', '?']) {
        Some(i) if i > max / 3 => head[..=i].to_owned(),
        _ => head.rsplit_once(' ').map_or(head.clone(), |(a, _)| a.to_owned()),
    }
}

/// The feed's address: one of SOURCES by its key, or the user's own link; None = no news.
pub fn feed_url(source: &str) -> Option<String> {
    let s = source.trim();
    if s.starts_with("https://") || s.starts_with("http://") {
        return Some(s.to_owned());
    }
    SOURCES.iter().find(|(k, _, _)| *k == s).map(|(_, _, u)| (*u).to_owned())
}

/// The first `max` item titles of an RSS (or Atom) feed, as plain text.
pub fn titles(xml: &str, max: usize) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = xml;
    // Items (RSS) or entries (Atom); the feed's own <title> before them is skipped.
    while out.len() < max {
        let Some(at) = ["<item", "<entry"].iter().filter_map(|t| rest.find(t)).min() else { break };
        rest = &rest[at + 5..];
        let Some(open) = rest.find("<title") else { break };
        let Some(gt) = rest[open..].find('>') else { break };
        let body = &rest[open + gt + 1..];
        let Some(close) = body.find("</title>") else { break };
        let t = clean(&body[..close]);
        if !t.is_empty() {
            out.push(t);
        }
        rest = &body[close..];
    }
    out
}

/// CDATA, tags and entities out; one line.
fn clean(raw: &str) -> String {
    let t = raw.trim().trim_start_matches("<![CDATA[").trim_end_matches("]]>");
    let mut s = String::with_capacity(t.len());
    let mut in_tag = false;
    for c in t.chars() {
        match c {
            '<' => in_tag = true,
            '>' if in_tag => in_tag = false,
            _ if !in_tag => s.push(c),
            _ => {}
        }
    }
    let s = s
        .replace("&quot;", "\"")
        .replace("&#34;", "\"")
        .replace("&apos;", "'")
        .replace("&#39;", "'")
        .replace("&laquo;", "«")
        .replace("&raquo;", "»")
        .replace("&nbsp;", " ")
        .replace("&#160;", " ")
        .replace("&mdash;", "—")
        .replace("&ndash;", "–")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&");
    numeric_entities(&s).split_whitespace().collect::<Vec<_>>().join(" ")
}

/// «&#036;20» → «$20», «&#8212;» → «—».
fn numeric_entities(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(at) = rest.find("&#") {
        out.push_str(&rest[..at]);
        let tail = &rest[at + 2..];
        let code = tail.find(';').filter(|e| *e <= 7).and_then(|e| {
            let n = &tail[..e];
            let v = n.strip_prefix(['x', 'X']).map_or_else(|| n.parse::<u32>().ok(), |h| u32::from_str_radix(h, 16).ok())?;
            Some((char::from_u32(v)?, e))
        });
        match code {
            Some((c, e)) => {
                out.push(c);
                rest = &tail[e + 1..];
            }
            None => {
                out.push_str("&#");
                rest = tail;
            }
        }
    }
    out.push_str(rest);
    out
}

/// «Сейчас +8, пасмурно. Днём до +9, ночью до +6.» from the panel's weather.
pub fn weather_phrase(w: &serde_json::Value) -> Option<String> {
    let deg = |v: &serde_json::Value| v.as_f64().map(|t| t.round() as i64).map(|t| if t > 0 { format!("+{t}") } else { t.to_string() });
    let now = deg(&w["temp"])?;
    let text = w["text"].as_str().unwrap_or("");
    let mut s = if text.is_empty() { format!("Сейчас {now}.") } else { format!("Сейчас {now}, {text}.") };
    if let (Some(max), Some(min)) = (deg(&w["max"]), deg(&w["min"])) {
        s.push_str(&format!(" Днём до {max}, ночью до {min}."));
    }
    Some(s)
}

/// The headlines as Alice says them, in pieces short enough for one phrase each.
pub fn news_phrases(name: &str, titles: &[String]) -> Vec<String> {
    if titles.is_empty() {
        return vec![];
    }
    let from = if name.is_empty() { String::new() } else if name.starts_with("из ") { format!(" {name}") } else { format!(" от {name}") };
    let mut out = vec![format!("Главные новости{from}.")];
    let mut cur = String::new();
    for t in titles {
        let t = t.trim_end_matches(['.', '!', '?', ' ']);
        if !cur.is_empty() && cur.chars().count() + t.chars().count() > 350 {
            out.push(std::mem::take(&mut cur));
        }
        cur.push_str(t);
        cur.push_str(". ");
    }
    if !cur.is_empty() {
        out.push(cur.trim_end().to_owned());
    }
    out
}

fn fetch_titles(url: &str, proxy: &str) -> Result<Vec<String>, String> {
    let agent = crate::sc::build_agent(proxy).map_err(|e| e.to_string())?;
    let body = agent
        .get(url)
        .timeout(Duration::from_secs(12))
        .call()
        .map_err(|e| e.to_string())?
        .into_string()
        .map_err(|e| e.to_string())?;
    let t = titles(&body, 25);
    if t.is_empty() {
        return Err("в ленте нет новостей".into());
    }
    Ok(t)
}

/// The project's server abroad (the one with the community bot): it reaches Telegram and
/// hands out open channels' posts (server/stats.py `GET /tg/<channel>`). Set at build time
/// with SOUNDYA_TG_SERVER to use another.
const TG_SERVER: &str = match option_env!("SOUNDYA_TG_SERVER") {
    Some(u) => u,
    None => "http://191.101.176.100:8787",
};

fn server_base() -> String {
    TG_SERVER.trim_end_matches('/').to_owned()
}

fn source_name(source: &str) -> &'static str {
    SOURCES.iter().find(|(k, _, _)| *k == source.trim()).map_or("", |(_, n, _)| n)
}

/// Everything the chosen source has now: (who, posts). None = no news chosen.
fn news(source: &str, proxy: &str) -> Option<Result<(String, Vec<Post>), String>> {
    if let Some(chans) = tg_channels(source) {
        let agent = match crate::sc::build_agent(proxy) {
            Ok(a) => a,
            Err(e) => return Some(Err(e.to_string())),
        };
        let mut all = Vec::new();
        for c in &chans {
            // Through the project's server abroad first (Telegram is blocked in Russia; it
            // keeps the posts 10 minutes), straight to t.me if it fails.
            let get = |url: String| agent.get(&url).timeout(Duration::from_secs(12)).call().map_err(|e| e.to_string()).and_then(|r| r.into_string().map_err(|e| e.to_string()));
            let page = get(format!("{}/tg/{c}", server_base())).or_else(|e| {
                eprintln!("morning: news server for {c}: {e}");
                if e.contains("status code 404") { Err(e) } else { get(format!("https://t.me/s/{c}")) }
            });
            match page {
                Ok(html) => {
                    let posts = tg_parse(&html, c);
                    if posts.is_empty() {
                        eprintln!("morning: t.me/s/{c}: no posts (the channel may have its web page switched off)");
                    }
                    all.extend(posts);
                }
                Err(e) => eprintln!("morning: t.me/s/{c}: {e}"),
            }
        }
        return Some(if all.is_empty() { Err("в каналах Telegram ничего не нашлось".into()) } else { Ok((String::from("из Телеграма"), all)) });
    }
    let url = feed_url(source)?;
    let name = source_name(source);
    Some(
        fetch_titles(&url, proxy)
            .map(|t| {
                // A feed is newest first; `pick` wants the oldest first.
                let posts = t.into_iter().rev().map(|t| Post { id: t.clone(), source: name.to_owned(), body: t.to_lowercase().replace('ё', "е"), text: t, views: None, time: None }).collect();
                (name.to_owned(), posts)
            })
            .map_err(|e| format!("{url}: {e}")),
    )
}

/// The news to read out now, chosen (`pick`); with `dir`, without what was read before.
pub fn chosen(s: &Settings, proxy: &str, dir: Option<&std::path::Path>) -> Option<Result<(String, Vec<Post>), String>> {
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
    let before = dir.map(told).unwrap_or_default();
    Some(news(&s.morning_news, proxy)?.map(|(name, posts)| {
        let found = posts.len();
        let picked = pick(posts, now, &before, &word_list(&s.morning_like), &word_list(&s.morning_skip), HEADLINES);
        eprintln!("morning: news: {} of {found} chosen", picked.len());
        (name, picked)
    }))
}

/// The whole morning, in its own thread. `test`: from the panel's «Проверить» (no wait).
pub fn run(s: Settings, shared: Arc<Shared>, tx: Sender<Cmd>, test: bool) {
    thread::spawn(move || {
        // The news is fetched meanwhile, so it is ready when its turn comes.
        let proxy = shared.proxy.lock().unwrap().clone();
        let dir = crate::planner::dir();
        let (s2, dir2) = (s.clone(), dir.clone());
        let fetching = thread::spawn(move || chosen(&s2, &proxy, dir2.as_deref()));
        // «Правильно! Будильник выключен» first.
        if !test {
            thread::sleep(Duration::from_secs(5));
        }
        let intro = s.morning_intro.trim().to_owned();
        if !intro.is_empty() {
            let (reply, _) = std::sync::mpsc::channel();
            let _ = tx.send(Cmd::Control(format!("play {intro}"), reply));
            thread::sleep(INTRO_FOR);
            // The music goes on under Alice, quieter (it was paused: «музыку на фоне вырубило»).
            let (reply, _) = std::sync::mpsc::channel();
            let _ = tx.send(Cmd::Control("duck down".into(), reply));
            thread::sleep(Duration::from_millis(800));
        }
        let greet = if s.morning_greet.trim().is_empty() { DEFAULT_GREET.to_owned() } else { s.morning_greet.trim().to_owned() };
        crate::alice::say_wait(&greet);
        if s.morning_weather {
            if let (Some(lat), Some(lon)) = (s.display_lat, s.display_lon) {
                match crate::display::home_weather(&shared, lat, lon) {
                    Ok(w) => {
                        if let Some(p) = weather_phrase(&w) {
                            crate::alice::say_wait(&p);
                        }
                    }
                    Err(e) => eprintln!("morning: weather: {e}"),
                }
            }
        }
        if s.morning_plans {
            crate::alice::say_wait(&crate::planner::agenda(Some(0)));
            if !test {
                crate::planner::brief_told_today();
            }
        }
        match fetching.join().ok().flatten() {
            Some(Ok((_, posts))) if posts.is_empty() => crate::alice::say_wait("Свежих новостей с прошлого раза нет"),
            Some(Ok((name, posts))) => {
                let t: Vec<String> = posts.iter().map(|p| p.text.clone()).collect();
                for p in news_phrases(&name, &t) {
                    crate::alice::say_wait(&p);
                }
                // «Проверить» reads the same again; the real morning does not.
                if let (false, Some(dir)) = (test, dir.as_deref()) {
                    remember_told(dir, &posts);
                }
            }
            Some(Err(e)) => {
                eprintln!("morning: news: {e}");
                crate::alice::say_wait("Новости сейчас не загрузились");
            }
            None => {}
        }
        if !intro.is_empty() {
            let (reply, _) = std::sync::mpsc::channel();
            let _ = tx.send(Cmd::Control("duck restore".into(), reply));
        }
        eprintln!("morning: done");
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rss_titles() {
        let xml = r#"<?xml version="1.0"?><rss><channel><title>РИА Новости</title>
            <item><title><![CDATA[Первая &laquo;новость&raquo;]]></title><link>x</link></item>
            <item><title>Вторая &amp; третья</title></item>
            <item><title>  </title></item>
            <item><title>Четвёртая</title></item></channel></rss>"#;
        assert_eq!(titles(xml, 5), ["Первая «новость»", "Вторая & третья", "Четвёртая"]);
        assert_eq!(titles(xml, 1), ["Первая «новость»"]);
        let atom = r#"<feed><title>Хабр</title><entry><title type="html">Atom &lt;b&gt;раз&lt;/b&gt;</title></entry></feed>"#;
        assert_eq!(titles(atom, 5), ["Atom <b>раз</b>"]);
    }

    #[test]
    fn sources_and_phrases() {
        assert_eq!(feed_url("lenta").as_deref(), Some("https://lenta.ru/rss/news"));
        assert_eq!(feed_url("https://example.com/rss").as_deref(), Some("https://example.com/rss"));
        assert_eq!(feed_url(""), None);
        let p = news_phrases("РИА Новости", &["Раз.".into(), "Два".into()]);
        assert_eq!(p, ["Главные новости от РИА Новости.", "Раз. Два."]);
        let w = serde_json::json!({"temp": 7.6, "text": "пасмурно", "max": 9, "min": -1});
        assert_eq!(weather_phrase(&w).unwrap(), "Сейчас +8, пасмурно. Днём до +9, ночью до -1.");
    }

    #[test]
    fn telegram_channels() {
        assert_eq!(tg_channels("tg:@meduzalive, https://t.me/s/rbc_news t.me/breakingmash?x=1").unwrap(), ["meduzalive", "rbc_news", "breakingmash"]);
        assert_eq!(tg_channels("ria"), None);
        let html = r#"<div class="tgme_widget_message_text js-message_text" dir="auto"><b>Старая новость про погоду</b><br/>текст</div>
            <div class="tgme_widget_message_text js-message_text" dir="auto">⚡️Срочно<br/><br/><b>В Москве открыли новую станцию метро</b><br/>подробности <a href="x">тут</a></div>"#;
        let old: Vec<String> = tg_parse(html, "c").into_iter().map(|p| p.text).collect();
        assert_eq!(old, ["Старая новость про погоду", "Срочно. В Москве открыли новую станцию метро"], "an old news server: text only, oldest first");
        assert_eq!(news_phrases("из Телеграма", &["Раз".into()])[0], "Главные новости из Телеграма.");
    }

    fn post(source: &str, n: u32, text: &str, views: u64, hours_ago: u64) -> Post {
        Post { id: format!("{source}/{n}"), source: source.into(), text: text.into(), body: text.to_lowercase(), views: Some(views), time: Some(1_000_000 - hours_ago * 3600) }
    }

    #[test]
    fn a_post_is_read_with_its_numbers() {
        // The news server's shape, and Telegram's own page.
        let server = r#"<div class="soundya_post" data-post="pekagame/101" data-views="12300" data-time="1794650000"></div>
            <div class="tgme_widget_message_text js-message_text" dir="auto">‼️«<b>Чёрная пятница» в Steam пройдёт с 26 по 30 ноября</b><br/><br/>Valve предложила разрабам запланировать скидки на эти даты. По случаю события страница получит оформление.<br/><br/>Готовимся</div>"#;
        let p = &tg_parse(server, "pekagame")[0];
        assert_eq!((p.id.as_str(), p.views, p.time), ("pekagame/101", Some(12300), Some(1_794_650_000)));
        assert_eq!(p.text, "«Чёрная пятница» в Steam пройдёт с 26 по 30 ноября. Valve предложила разрабам запланировать скидки на эти даты");
        let own = r#"<div class="tgme_widget_message js-widget_message" data-post="chan/7" data-view="x"><div class="tgme_widget_message_text js-message_text" dir="auto">Вышло большое обновление игры, в нём новая карта</div>
            <span class="tgme_widget_message_views">1.5K</span><time datetime="1970-01-02T00:00:00+00:00" class="time">00:00</time></div>"#;
        let p = &tg_parse(own, "chan")[0];
        assert_eq!((p.id.as_str(), p.views, p.time), ("chan/7", Some(1500), Some(86_400)));
        assert_eq!(views_number("2.4M"), Some(2_400_000));
        assert_eq!(clean("всего за &#036;20 &#x2014; и &#bad; &amp;"), "всего за $20 — и &#bad; &");
    }

    #[test]
    fn the_worth_hearing_is_picked() {
        let now = 1_000_000;
        let none = std::collections::HashSet::new();
        let posts = vec![
            post("a", 1, "Обычная новость про обновление лаунчера", 1000, 20),
            post("a", 2, "Мем дня без особого смысла", 900, 18),
            post("a", 3, "Вышла Half-Life 3: Valve выпустила игру без анонса", 9000, 6),
            post("a", 4, "Разыгрываем консоль среди подписчиков", 20000, 5),
            post("a", 5, "Патч исправил ошибки сетевой игры", 800, 4),
            post("a", 6, "Совсем старая история про выставку", 50000, 60),
            post("b", 1, "Valve без анонса выпустила Half-Life 3, игра уже доступна", 3000, 5),
            post("b", 2, "Новый трейлер стратегии показали на выставке", 1100, 8),
            post("b", 3, "Скучная заметка про курс биткоина", 1000, 7),
        ];
        let texts = |v: Vec<Post>| v.into_iter().map(|p| p.text).collect::<Vec<_>>();
        let got = texts(pick(posts.clone(), now, &none, &[], &[], 3));
        assert!(got[0].contains("Half-Life 3"), "read far more than usual and told by two channels: first; {got:?}");
        assert_eq!(got.iter().filter(|t| t.contains("Half-Life")).count(), 1, "one story, once");
        assert!(!got.iter().any(|t| t.contains("Разыгрываем") || t.contains("старая")), "no adverts, nothing stale: {got:?}");
        // Read before: not again. The user's words: «не читать» and «интересно».
        let told: std::collections::HashSet<String> = ["a/3".to_owned(), "b/1".to_owned()].into();
        let got = texts(pick(posts.clone(), now, &told, &word_list("биткоин"), &word_list("Мем, трейлер"), 5));
        assert!(!got.iter().any(|t| t.contains("Half-Life") || t.contains("Мем") || t.contains("трейлер")), "{got:?}");
        assert!(got[0].contains("биткоина"), "a liked word goes first: {got:?}");
        // Two from one channel at most while others have something; the rest fills what is left.
        let got = pick(posts, now, &none, &[], &[], 5);
        assert!(got.iter().take(4).filter(|p| p.source == "a").count() <= 2, "{got:?}");
        // RSS: no numbers — the feed's order stays (it comes oldest first).
        let rss: Vec<Post> = ["третья", "вторая", "первая"].iter().map(|t| Post { id: t.to_string(), source: "РБК".into(), text: t.to_string(), body: t.to_string(), views: None, time: None }).collect();
        assert_eq!(texts(pick(rss, now, &none, &[], &[], 2)), ["первая", "вторая"]);
    }

    #[test]
    fn what_was_read_is_remembered() {
        let dir = std::env::temp_dir().join(format!("soundya-news-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        assert!(told(&dir).is_empty());
        remember_told(&dir, &[post("a", 1, "x", 1, 1), post("b", 2, "y", 1, 1)]);
        remember_told(&dir, &[post("a", 3, "z", 1, 1)]);
        let t = told(&dir);
        assert!(t.contains("a/1") && t.contains("b/2") && t.contains("a/3") && t.len() == 3);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
