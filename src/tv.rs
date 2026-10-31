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
    for (names, parts) in APPS {
        if names.iter().any(|n| spoken.starts_with(n)) {
            for part in *parts {
                if let Some(p) = packages.iter().find(|p| p.contains(part)) {
                    return Some(p.clone());
                }
            }
        }
    }
    // Anything else: the transliterated name inside the package name.
    let t = translit(&spoken);
    (t.len() >= 3).then(|| packages.iter().find(|p| p.replace(['.', '_'], "").contains(&t)).cloned())?
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
        TvAction::Open(name) => {
            let list = sh("pm list packages")?;
            let packages: Vec<String> =
                list.lines().filter_map(|l| l.trim().strip_prefix("package:")).map(str::to_owned).collect();
            let Some(pkg) = find_package(name, &packages) else {
                return Err(format!("Не нашла на телевизоре приложение {name}"));
            };
            // TV apps start from the TV launcher category; phone-style apps from the usual one.
            sh(&format!(
                "monkey -p {pkg} -c android.intent.category.LEANBACK_LAUNCHER 1 2>&1 | grep -q 'No activities' && \
                 monkey -p {pkg} -c android.intent.category.LAUNCHER 1; true"
            ))?;
            Ok(format!("Открываю {name}"))
        }
    }
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
    }

    #[test]
    fn transliterates_and_encodes() {
        assert_eq!(translit("лампа"), "lampa");
        assert_eq!(percent_encode("котики 2"), "%D0%BA%D0%BE%D1%82%D0%B8%D0%BA%D0%B8+2");
    }
}
