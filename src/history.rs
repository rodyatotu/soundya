//! «Недавно играло» on the page: the last tracks and the last things asked for
//! (search, playlist, album, likes…), each with the command that plays it again.
//!
//! Kept in history.json in the state dir. Flash is written at most once a
//! minute, not on every track.

use std::{
    collections::VecDeque,
    path::PathBuf,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};

const TRACKS: usize = 100;
const SETS: usize = 30;
const SAVE_EVERY: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Entry {
    pub name: String,
    /// Plays it again: "replay https://soundcloud.com/…", "playlist …", "likes".
    pub cmd: String,
    /// Unix time, seconds.
    pub at: u64,
    /// The track's cover on SoundCloud's CDN, "" if none (older entries too).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub art: String,
    /// SoundCloud's track id (0: a set, or an entry from before ids).
    #[serde(default, skip_serializing_if = "is_zero")]
    pub id: u64,
}

fn is_zero(n: &u64) -> bool {
    *n == 0
}

#[derive(Default, Serialize, Deserialize)]
struct Saved {
    #[serde(default)]
    tracks: VecDeque<Entry>,
    #[serde(default)]
    sets: VecDeque<Entry>,
}

pub struct History {
    path: PathBuf,
    data: Saved,
    dirty: bool,
    saved: Instant,
}

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// Newest first; the same thing again moves to the top instead of repeating.
fn push(list: &mut VecDeque<Entry>, entry: Entry, cap: usize) {
    list.retain(|e| e.cmd != entry.cmd);
    list.push_front(entry);
    list.truncate(cap);
}

impl History {
    pub fn load(dir: &std::path::Path) -> History {
        let path = dir.join("history.json");
        let data = crate::fsx::read_string(&path)
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default();
        History { path, data, dirty: false, saved: Instant::now() }
    }

    pub fn track(&mut self, name: String, link: Option<&str>, art: Option<String>, id: u64) {
        let Some(link) = link.filter(|l| l.starts_with("https://")) else { return };
        let art = art.unwrap_or_default();
        push(&mut self.data.tracks, Entry { name, cmd: format!("replay {link}"), at: now(), art, id }, TRACKS);
        self.dirty = true;
    }

    pub fn set(&mut self, name: String, cmd: String) {
        push(&mut self.data.sets, Entry { name, cmd, at: now(), art: String::new(), id: 0 }, SETS);
        self.dirty = true;
    }

    /// «Недавно играло» to play again: the ids of the recent tracks, newest first.
    pub fn recent_ids(&self, max: usize) -> Vec<u64> {
        self.data.tracks.iter().map(|e| e.id).filter(|id| *id != 0).take(max).collect()
    }

    pub fn clear(&mut self) {
        self.data = Saved::default();
        self.dirty = true;
        self.flush(true);
    }

    /// Writes the file if something changed and the last write was a minute ago
    /// (`now`: at once).
    pub fn flush(&mut self, now: bool) {
        if !self.dirty || (!now && self.saved.elapsed() < SAVE_EVERY) {
            return;
        }
        if let Ok(text) = serde_json::to_string(&self.data) {
            let tmp = self.path.with_extension("json.tmp");
            if std::fs::write(&tmp, text).is_ok() && std::fs::rename(&tmp, &self.path).is_ok() {
                self.dirty = false;
                self.saved = Instant::now();
            }
        }
    }

    pub fn json(&self) -> serde_json::Value {
        serde_json::json!({ "tracks": self.data.tracks, "sets": self.data.sets })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn newest_first_without_repeats() {
        let mut list = VecDeque::new();
        let e = |n: &str| Entry { name: n.into(), cmd: format!("play {n}"), at: 0, art: String::new(), id: 0 };
        push(&mut list, e("a"), 3);
        push(&mut list, e("b"), 3);
        push(&mut list, e("a"), 3);
        push(&mut list, e("c"), 3);
        push(&mut list, e("d"), 3);
        let names: Vec<_> = list.iter().map(|x| x.name.as_str()).collect();
        assert_eq!(names, ["d", "c", "a"]);
    }

    #[test]
    fn survives_a_restart() {
        let dir = std::env::temp_dir().join(format!("soundya-history-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut h = History::load(&dir);
        h.track("Bonobo — Kerala".into(), Some("https://soundcloud.com/bonobo/kerala"), Some("https://i1.sndcdn.com/artworks-x-t300x300.jpg".into()), 42);
        h.track("no link".into(), None, None, 7);
        h.set("Плейлист: chill".into(), "playlist chill".into());
        h.flush(true);
        let h = History::load(&dir);
        assert_eq!(h.data.tracks.len(), 1);
        assert_eq!(h.data.tracks[0].cmd, "replay https://soundcloud.com/bonobo/kerala");
        assert_eq!(h.data.tracks[0].art, "https://i1.sndcdn.com/artworks-x-t300x300.jpg");
        // A file from before covers: entries load with none.
        let old: Saved = serde_json::from_str(r#"{"tracks":[{"name":"a","cmd":"replay https://x","at":1}]}"#).unwrap();
        assert_eq!((old.tracks[0].art.as_str(), old.tracks[0].id), ("", 0));
        assert_eq!(h.recent_ids(10), [42]);
        assert_eq!(h.data.sets[0].cmd, "playlist chill");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
