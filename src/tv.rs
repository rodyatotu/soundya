//! Android TV / projector by voice: switch on and off, open apps, search films
//! and YouTube, pause, volume. Talks to the TV over ADB (adb.rs); the TV needs
//! «Отладка по сети» (network ADB) on, and the first time a press on «Разрешить».

use std::{path::Path, time::Duration};

use crate::{adb, alice::TvAction};

/// Spoken app names → parts of their package names (first match wins).
const APPS: &[(&[&str], &[&str])] = &[
    (&["ютуб", "youtube", "ютьюб"], &["com.google.android.youtube.tv", "youtube.tv", "youtube"]),
    (&["ютуб музык", "ютуб мьюзик"], &["youtube.tv.unplugged", "youtube.music", "youtube.tvmusic"]),
    (&["кинопоиск"], &["kinopoisk"]),
    (&["иви", "ivi"], &["ivi"]),
    (&["окко", "okko"], &["okko"]),
    (&["винк", "wink"], &["wink", "ru.rt.video"]),
    (&["кион", "kion"], &["kion", "mts.mtstv", "ru.mts"]),
    (&["нетфликс", "netflix"], &["netflix"]),
    (&["твич", "twitch"], &["twitch"]),
    (&["вк видео", "вк", "вконтакте"], &["vkvideo", "vk.video", "com.vk"]),
    (&["рутуб", "rutube"], &["rutube"]),
    (&["премьер"], &["premier"]),
    (&["амедиатек"], &["amediateka"]),
    (&["смотрешк"], &["smotreshka"]),
    (&["плекс", "plex"], &["plex"]),
    (&["коди", "kodi"], &["kodi"]),
    (&["влц", "vlc"], &["videolan"]),
    (&["спотифай", "spotify"], &["spotify"]),
    (&["яндекс музык"], &["yandex.music"]),
    (&["браузер", "хром"], &["browser", "chrome"]),
    (&["настройки", "параметры"], &["com.android.tv.settings", "settings"]),
    (&["гугл плей", "плей маркет", "плеймаркет", "магазин"], &["com.android.vending"]),
    (&["лампа", "lampa"], &["lampa"]),
    (&["кинопаб", "kinopub"], &["kinopub"]),
    (&["видеобокс", "hd videobox"], &["videobox"]),
];

fn translit(s: &str) -> String {
    let mut out = String::new();
    for c in s.chars() {
        let t = match c {
            'а' => "a", 'б' => "b", 'в' => "v", 'г' => "g", 'д' => "d", 'е' | 'ё' | 'э' => "e", 'ж' => "zh",
            'з' => "z", 'и' | 'й' | 'ы' => "i", 'к' => "k", 'л' => "l", 'м' => "m", 'н' => "n", 'о' => "o",
            'п' => "p", 'р' => "r", 'с' => "s", 'т' => "t", 'у' => "u", 'ф' => "f", 'х' => "h", 'ц' => "c",
            'ч' => "ch", 'ш' => "sh", 'щ' => "sch", 'ю' => "yu", 'я' => "ya",
            c if c.is_ascii_alphanumeric() => {
                out.push(c.to_ascii_lowercase());
                continue;
            }
            _ => "",
        };
        out.push_str(t);
    }
    out
}

/// The installed package for a spoken app name.
pub fn find_package(spoken: &str, packages: &[String]) -> Option<String> {
    let spoken = spoken.trim().to_lowercase();
    // The most specific name wins: «ютуб музыку» is YouTube Music, not YouTube.
    let entry = APPS
        .iter()
        .filter_map(|(names, parts)| {
            let len = names.iter().filter(|n| spoken.starts_with(**n)).map(|n| n.len()).max()?;
            Some((len, *parts))
        })
        .max_by_key(|(len, _)| *len)
        .map(|(_, parts)| parts);
    if let Some(parts) = entry {
        // Parts of other apps: «…youtube.tvmusic» contains «…youtube.tv» but is YouTube Music.
        // A full package name («com.google.android.youtube.tv») only matches itself.
        let full = |p: &str| p.matches('.').count() >= 2;
        let others: Vec<&str> =
            APPS.iter().flat_map(|(_, p)| p.iter().copied()).filter(|p| !parts.contains(p) && !full(p)).collect();
        for part in parts {
            if let Some(p) = packages.iter().find(|p| p.as_str() == *part) {
                return Some(p.clone());
            }
            if full(part) {
                continue;
            }
            let found = packages.iter().find(|p| {
                // …and not a package that another app's name explains better.
                p.contains(part) && !others.iter().any(|o| p.contains(o) && !part.contains(o))
            });
            if let Some(p) = found {
                return Some(p.clone());
            }
        }
        // A known app that is not installed: not some other app with a similar name.
        return None;
    }
    // Anything else: the name, in Latin letters, inside the package name.
    // Within one part of the package name: «com.amazon.avod» joined up would
    // contain «zona».
    for t in spellings(&spoken) {
        if let Some(p) = packages.iter().find(|p| p.split(['.', '_', '-']).any(|part| part.contains(&t))) {
            return Some(p.clone());
        }
    }
    None
}

/// How a spoken app name may appear in a package name, best first: as said
/// («zona»), back from the case Alice heard («зону» → zona), with
/// other spellings of the sounds (кс → x, ю → iu/u, й → y…), and the stem.
fn spellings(spoken: &str) -> Vec<String> {
    let words: String = spoken.split_whitespace().collect::<Vec<_>>().join("");
    let mut bases = vec![words.clone()];
    // «Открой зону / кинопоиск / тиви»: accusative and similar endings back to the name.
    for (end, base) in [("у", "а"), ("ю", "я"), ("ом", ""), ("е", "а")] {
        if let Some(stem) = words.strip_suffix(end) {
            bases.push(format!("{stem}{base}"));
        }
    }
    let mut out: Vec<String> = Vec::new();
    let mut add = |s: String| {
        if s.len() >= 3 && !out.contains(&s) {
            out.push(s);
        }
    };
    for b in &bases {
        let t = translit(b);
        add(t.clone());
        add(t.replace("ks", "x").replace("yu", "u").replace("ya", "ia").replace("ts", "z").replace("kh", "h"));
        add(t.replace("yu", "iu").replace("zh", "j").replace('c', "ts"));
        add(t.replace("i", "y"));
    }
    // The stem without its last vowel («zon» for «зона»), only when long enough
    // not to match everything.
    let t = translit(&words);
    let stem = t.trim_end_matches(['a', 'e', 'i', 'o', 'u', 'y']);
    if stem.len() >= 4 {
        add(stem.to_owned());
    }
    out
}

fn percent_encode(s: &str) -> String {
    s.bytes()
        .map(|b| if b.is_ascii_alphanumeric() { (b as char).to_string() } else if b == b' ' { "+".into() } else { format!("%{b:02X}") })
        .collect()
}

fn speech(e: &adb::Error) -> String {
    match e {
        adb::Error::Unreachable(_) => {
            "Телевизор не отвечает. Он включён, в той же сети и с включённой отладкой по сети?".into()
        }
        adb::Error::NeedsConfirm => {
            "На экране телевизора появился запрос на отладку. Нажмите «Разрешить» и повторите".into()
        }
        adb::Error::Other(e) => format!("Телевизор не выполнил команду: {e}"),
    }
}

/// Runs the action; the answer is a short sentence for Alice to say.
pub fn run(host: &str, dir: &Path, action: &TvAction) -> Result<String, String> {
    let key = adb::Key::load_or_create(dir).map_err(|e| format!("нет ключа для телевизора: {e}"))?;
    // Waiting for «Разрешить» makes sense only for the check from the settings page.
    let wait = if matches!(action, TvAction::Ping) { Duration::from_secs(40) } else { Duration::from_secs(8) };
    let sh = |cmd: &str| adb::shell(host, &key, cmd, wait).map_err(|e| speech(&e));
    match action {
        TvAction::Ping => {
            let out = sh("getprop ro.product.manufacturer; getprop ro.product.model")?;
            let name = out.split_whitespace().collect::<Vec<_>>().join(" ");
            Ok(format!("Телевизор на связи: {}", if name.is_empty() { "Android" } else { &name }))
        }
        TvAction::Wake => sh("input keyevent KEYCODE_WAKEUP").map(|_| "Включаю".into()),
        TvAction::Sleep => sh("input keyevent KEYCODE_SLEEP").map(|_| "Выключаю".into()),
        TvAction::Key(code, n) => {
            let keys = vec![*code; usize::from(*n)].join(" ");
            sh(&format!("input keyevent {keys}")).map(|_| "Готово".into())
        }
        TvAction::Search(q) => {
            sh(&format!("am start -a android.search.action.GLOBAL_SEARCH --es query {}", adb::quote(q)))
                .map(|_| format!("Ищу {q}"))
        }
        TvAction::YouTube(q) => {
            let url = format!("https://www.youtube.com/results?search_query={}", percent_encode(q));
            let out = sh(&format!("am start -a android.intent.action.VIEW -d {}", adb::quote(&url)))?;
            if out.contains("Error") {
                // No app takes the link: the TV's own search, which includes YouTube.
                sh(&format!("am start -a android.search.action.GLOBAL_SEARCH --es query {}", adb::quote(q)))?;
            }
            Ok(format!("Ищу на ютубе {q}"))
        }
        TvAction::Apps => {
            // Apps with a TV launcher entry (what the TV's home screen shows);
            // older TVs without `query-activities`: third-party packages.
            let out = sh("cmd package query-activities --brief -a android.intent.action.MAIN -c android.intent.category.LEANBACK_LAUNCHER 2>/dev/null")?;
            let mut apps = packages_of(&out);
            if apps.is_empty() {
                let list = sh("pm list packages -3")?;
                apps = list.lines().filter_map(|l| l.trim().strip_prefix("package:")).map(str::to_owned).collect();
            }
            apps.sort();
            apps.dedup();
            Ok(serde_json::json!({ "apps": apps }).to_string())
        }
        TvAction::OpenPackage(pkg) => {
            if !valid_package(pkg) {
                return Err("неверное имя пакета".into());
            }
            let out = sh(&launch_script(pkg))?;
            if out.contains("Error") || out.contains("No activities") || out.contains("Exception") {
                return Err(format!("Телевизор не открыл {pkg}"));
            }
            Ok("Открываю".into())
        }
        TvAction::Close(name) => {
            let pkg = if valid_package(name) && name.contains('.') {
                name.clone()
            } else {
                let list = sh("pm list packages")?;
                let packages: Vec<String> =
                    list.lines().filter_map(|l| l.trim().strip_prefix("package:")).map(str::to_owned).collect();
                find_package(name, &packages).ok_or_else(|| format!("Не нашла на телевизоре приложение {name}"))?
            };
            sh(&format!("am force-stop {pkg}")).map(|_| format!("Закрыла {name}"))
        }
        TvAction::Open(name) => {
            let list = sh("pm list packages")?;
            let packages: Vec<String> =
                list.lines().filter_map(|l| l.trim().strip_prefix("package:")).map(str::to_owned).collect();
            let Some(pkg) = find_package(name, &packages) else {
                eprintln!("tv: no app for {name:?} among {} packages", packages.len());
                return Err(format!("Не нашла на телевизоре приложение {name}"));
            };
            let out = sh(&launch_script(&pkg))?;
            eprintln!("tv: open {name:?} -> {pkg}: {}", out.trim().replace('\n', " | "));
            if out.contains("Error") || out.contains("No activities") || out.contains("Exception") {
                return Err(format!("Телевизор не открыл {name}"));
            }
            Ok(format!("Открываю {name}"))
        }
    }
}

/// «com.google.android.youtube.tv/…» lines → packages.
fn packages_of(out: &str) -> Vec<String> {
    out.lines()
        .filter_map(|l| l.trim().split_once('/').map(|(p, _)| p.trim().to_owned()))
        .filter(|p| valid_package(p))
        .collect()
}

/// A package name safe to put into a shell command.
fn valid_package(p: &str) -> bool {
    !p.is_empty() && p.len() < 200 && p.chars().all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '_')
}

/// Starts an app's main screen: the TV launcher entry (Android TV apps), else the
/// phone launcher entry, else whatever monkey finds.
fn launch_script(pkg: &str) -> String {
    let resolve = |cat: &str| {
        format!("cmd package resolve-activity --brief -a android.intent.action.MAIN -c android.intent.category.{cat} {pkg} 2>/dev/null | tail -n 1")
    };
    format!(
        "c=$({}); case \"$c\" in */*) ;; *) c=$({});; esac; \
         case \"$c\" in */*) am start -n \"$c\" 2>&1 ;; *) monkey -p {pkg} 1 2>&1 ;; esac",
        resolve("LEANBACK_LAUNCHER"),
        resolve("LAUNCHER")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_apps_by_spoken_name() {
        let pk: Vec<String> = [
            "com.google.android.youtube.tv",
            "ru.kinopoisk.tv",
            "ru.ivi.client",
            "com.netflix.ninja",
            "org.videolan.vlc",
            "com.android.tv.settings",
            "tv.okko.androidtv",
        ]
        .iter()
        .map(|s| (*s).to_owned())
        .collect();
        assert_eq!(find_package("ютуб", &pk).as_deref(), Some("com.google.android.youtube.tv"));
        assert_eq!(find_package("кинопоиск", &pk).as_deref(), Some("ru.kinopoisk.tv"));
        assert_eq!(find_package("иви", &pk).as_deref(), Some("ru.ivi.client"));
        assert_eq!(find_package("нетфликс", &pk).as_deref(), Some("com.netflix.ninja"));
        assert_eq!(find_package("влц", &pk).as_deref(), Some("org.videolan.vlc"));
        assert_eq!(find_package("настройки", &pk).as_deref(), Some("com.android.tv.settings"));
        assert_eq!(find_package("окко", &pk).as_deref(), Some("tv.okko.androidtv"));
        assert_eq!(find_package("твич", &pk), None);
        // Xiaomi L1 Pro lists YouTube Music before YouTube.
        let pk: Vec<String> =
            ["com.google.android.youtube.tvmusic", "com.google.android.youtube.tv"].iter().map(|s| (*s).to_owned()).collect();
        assert_eq!(find_package("youtube", &pk).as_deref(), Some("com.google.android.youtube.tv"));
        assert_eq!(find_package("ютуб", &pk).as_deref(), Some("com.google.android.youtube.tv"));
        assert_eq!(find_package("ютуб музыку", &pk).as_deref(), Some("com.google.android.youtube.tvmusic"));
        let only_music: Vec<String> = vec!["com.google.android.youtube.tvmusic".into()];
        assert_eq!(find_package("ютуб", &only_music), None);
    }

    #[test]
    fn launch_script_is_valid_shell() {
        let script = launch_script("com.google.android.youtube.tv");
        assert!(script.contains("LEANBACK_LAUNCHER com.google.android.youtube.tv"));
        let ok = std::process::Command::new("sh").arg("-n").arg("-c").arg(&script).status().unwrap().success();
        assert!(ok, "{script}");
    }

    #[test]
    fn latin_names_said_in_russian() {
        let pk = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let installed = pk(&["ru.zona.tv", "com.mxtech.videoplayer.ad", "tv.twitch.android.app", "com.amazon.avod"]);
        assert_eq!(find_package("зону", &installed).as_deref(), Some("ru.zona.tv"));
        assert_eq!(find_package("zona", &installed).as_deref(), Some("ru.zona.tv"));
        assert_eq!(find_package("зона", &installed).as_deref(), Some("ru.zona.tv"));
        assert_eq!(find_package("зону", &pk(&["com.amazon.avod"])), None, "«zon» must not match amazon");
    }

    #[test]
    fn reads_launcher_apps_safely() {
        let out = "priority=0 preferredOrder=0\ncom.google.android.youtube.tv/com.google.android.apps.youtube.tv.activity.ShellActivity\nru.kinopoisk.tv/.MainActivity\nbad;rm -rf/x\n";
        assert_eq!(packages_of(out), vec!["com.google.android.youtube.tv", "ru.kinopoisk.tv"]);
        assert!(!valid_package("a;b") && !valid_package("") && valid_package("tv.okko.androidtv"));
    }

    #[test]
    fn transliterates_and_encodes() {
        assert_eq!(translit("лампа"), "lampa");
        assert_eq!(percent_encode("котики 2"), "%D0%BA%D0%BE%D1%82%D0%B8%D0%BA%D0%B8+2");
    }
}
