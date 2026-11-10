//! Developer build only for now. The morning after the alarm (alarm.rs): once it is switched off by the right answer,
//! an intro track of the user's choice plays for a moment, then Alice greets («Доброе утро!»
//! unless changed), tells the weather, the day's plans and the headlines of a
//! news feed over the music, played quieter meanwhile.
//!
//! The feeds are plain RSS; the titles are read out, nothing else of the articles.

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

/// The posts of a channel's public page (t.me/s/<name>), newest first: each one's first line
/// (its headline), without emoji and links, cut to a sentence.
pub fn tg_posts(html: &str) -> Vec<String> {
    const MARK: &str = "tgme_widget_message_text js-message_text";
    let mut out = Vec::new();
    let mut rest = html;
    while let Some(at) = rest.find(MARK) {
        rest = &rest[at + MARK.len()..];
        let Some(gt) = rest.find('>') else { break };
        let body = &rest[gt + 1..];
        let end = body.find("</div>").unwrap_or(body.len());
        let raw = &body[..end];
        // The headline: the first line, or two when the first is only «⚡️Срочно».
        let lines: Vec<String> = raw.split("<br/>").map(clean).map(|l| speakable(&l)).filter(|l| !l.is_empty()).collect();
        let mut head = lines.first().cloned().unwrap_or_default();
        if head.chars().count() < 25 {
            if let Some(next) = lines.get(1) {
                head = format!("{head}. {next}");
            }
        }
        let head = cut(&head, 220);
        if head.chars().count() >= 12 {
            out.push(head);
        }
        rest = &body[end..];
    }
    out.reverse();
    out
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

/// Headlines from several channels, the newest of each in turn, `max` in all.
pub fn mix(per_channel: Vec<Vec<String>>, max: usize) -> Vec<String> {
    let mut out = Vec::new();
    let mut i = 0;
    while out.len() < max && per_channel.iter().any(|p| p.len() > i) {
        for p in &per_channel {
            if let Some(t) = p.get(i) {
                if out.len() < max && !out.contains(t) {
                    out.push(t.clone());
                }
            }
        }
        i += 1;
    }
    out
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
    s.split_whitespace().collect::<Vec<_>>().join(" ")
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
    let t = titles(&body, HEADLINES);
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

/// The news of the chosen source: (who, headlines). None = no news chosen.
fn news(source: &str, proxy: &str) -> Option<Result<(String, Vec<String>), String>> {
    if let Some(chans) = tg_channels(source) {
        let agent = match crate::sc::build_agent(proxy) {
            Ok(a) => a,
            Err(e) => return Some(Err(e.to_string())),
        };
        let per: Vec<Vec<String>> = chans
            .iter()
            .map(|c| {
                // Through the project's server abroad first (Telegram is blocked in Russia; it
                // keeps the posts 10 minutes), straight to t.me if it fails.
                let get = |url: String| agent.get(&url).timeout(Duration::from_secs(12)).call().map_err(|e| e.to_string()).and_then(|r| r.into_string().map_err(|e| e.to_string()));
                let page = get(format!("{}/tg/{c}", server_base())).or_else(|e| {
                    eprintln!("morning: news server for {c}: {e}");
                    if e.contains("status code 404") { Err(e) } else { get(format!("https://t.me/s/{c}")) }
                });
                match page {
                    Ok(html) => {
                        let posts = tg_posts(&html);
                        if posts.is_empty() {
                            eprintln!("morning: t.me/s/{c}: no posts (the channel may have its web page switched off)");
                        }
                        posts
                    }
                    Err(e) => {
                        eprintln!("morning: t.me/s/{c}: {e}");
                        Vec::new()
                    }
                }
            })
            .collect();
        let t = mix(per, HEADLINES);
        return Some(if t.is_empty() { Err("в каналах Telegram ничего не нашлось".into()) } else { Ok((String::from("из Телеграма"), t)) });
    }
    let url = feed_url(source)?;
    Some(fetch_titles(&url, proxy).map(|t| (source_name(source).to_owned(), t)).map_err(|e| format!("{url}: {e}")))
}

/// The whole morning, in its own thread. `test`: from the panel's «Проверить» (no wait).
pub fn run(s: Settings, shared: Arc<Shared>, tx: Sender<Cmd>, test: bool) {
    thread::spawn(move || {
        // The news is fetched meanwhile, so it is ready when its turn comes.
        let source = s.morning_news.clone();
        let proxy = shared.proxy.lock().unwrap().clone();
        let fetching = thread::spawn(move || news(&source, &proxy));
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
            Some(Ok((name, t))) => {
                for p in news_phrases(&name, &t) {
                    crate::alice::say_wait(&p);
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
        assert_eq!(tg_posts(html), ["Срочно. В Москве открыли новую станцию метро", "Старая новость про погоду"]);
        let m = mix(vec![vec!["a1".into(), "a2".into()], vec!["b1".into()]], 5);
        assert_eq!(m, ["a1", "b1", "a2"]);
        assert_eq!(news_phrases("из Телеграма", &["Раз".into()])[0], "Главные новости из Телеграма.");
    }
}
