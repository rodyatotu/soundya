//! Sound levels for the light («Подсветка» → эквалайзер, пульс): the music is
//! split into bass, middle and treble with three cheap one-pole filters, and each
//! ~20 ms of sound becomes three levels 0..1. A level is shown when that sound
//! leaves the speaker, not when it is written: the card holds a fraction of a
//! second ahead.

use std::{
    collections::VecDeque,
    sync::{
        atomic::{AtomicBool, Ordering},
        Mutex,
    },
    time::{Duration, Instant},
};

/// Analysis runs only while the light wants it (it costs a little CPU).
pub static ON: AtomicBool = AtomicBool::new(false);

const BASS_HZ: f32 = 180.0;
const TREBLE_HZ: f32 = 2500.0;
/// Levels to come, each with the moment its sound is heard.
static QUEUE: Mutex<VecDeque<(Instant, [f32; 3])>> = Mutex::new(VecDeque::new());
static STATE: Mutex<Filters> = Mutex::new(Filters::new());

struct Filters {
    low: f32,
    mid: f32,
    /// Loudest recent level per band: levels are relative to it, so quiet and
    /// loud tracks both move the light.
    peak: [f32; 3],
}

impl Filters {
    const fn new() -> Self {
        Filters { low: 0.0, mid: 0.0, peak: [0.05; 3] }
    }

    /// Bass, middle, treble of one block (RMS, 0..1 of full scale).
    fn bands(&mut self, samples: &[i16], channels: usize, rate: u32) -> [f32; 3] {
        let ch = channels.max(1);
        let a = |hz: f32| 1.0 - (-2.0 * std::f32::consts::PI * hz / rate.max(1) as f32).exp();
        let (a_low, a_mid) = (a(BASS_HZ), a(TREBLE_HZ));
        let mut sum = [0f32; 3];
        let mut n = 0usize;
        for frame in samples.chunks_exact(ch) {
            let x = frame.iter().map(|&s| s as f32).sum::<f32>() / (ch as f32 * 32768.0);
            self.low += a_low * (x - self.low);
            self.mid += a_mid * (x - self.mid);
            let (b, m, t) = (self.low, self.mid - self.low, x - self.mid);
            sum[0] += b * b;
            sum[1] += m * m;
            sum[2] += t * t;
            n += 1;
        }
        if n == 0 {
            return [0.0; 3];
        }
        sum.map(|s| (s / n as f32).sqrt())
    }

    /// Relative to the band's recent loudest, with a floor so near-silence stays dark.
    fn levels(&mut self, rms: [f32; 3]) -> [f32; 3] {
        let mut out = [0f32; 3];
        for i in 0..3 {
            self.peak[i] = (self.peak[i] * 0.995).max(rms[i]).max(0.004);
            out[i] = (rms[i] / self.peak[i]).clamp(0.0, 1.0);
        }
        out
    }
}

/// After each write to the sound card: `queued` frames are still ahead of what
/// is heard.
pub fn feed(samples: &[i16], channels: usize, rate: u32, queued: Option<usize>) {
    if !ON.load(Ordering::Relaxed) || samples.is_empty() {
        return;
    }
    let levels = {
        let mut f = STATE.lock().unwrap();
        let rms = f.bands(samples, channels, rate);
        f.levels(rms)
    };
    let ahead = Duration::from_millis(queued.unwrap_or(0) as u64 * 1000 / rate.max(1) as u64);
    let mut q = QUEUE.lock().unwrap();
    q.push_back((Instant::now() + ahead, levels));
    while q.len() > 200 {
        q.pop_front();
    }
}

/// The levels being heard now; None in silence (nothing heard lately).
pub fn now() -> Option<[f32; 3]> {
    static LAST: Mutex<Option<(Instant, [f32; 3])>> = Mutex::new(None);
    let now = Instant::now();
    let mut q = QUEUE.lock().unwrap();
    let mut last = LAST.lock().unwrap();
    while q.front().is_some_and(|(at, _)| *at <= now) {
        *last = q.pop_front();
    }
    match *last {
        Some((at, l)) if now.duration_since(at) < Duration::from_millis(400) => Some(l),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone(hz: f32, rate: u32, ms: u32) -> Vec<i16> {
        let n = (rate * ms / 1000) as usize;
        (0..n)
            .flat_map(|i| {
                let v = ((i as f32 * hz * 2.0 * std::f32::consts::PI / rate as f32).sin() * 12000.0) as i16;
                [v, v]
            })
            .collect()
    }

    #[test]
    fn bass_and_treble_land_in_their_bands() {
        let mut f = Filters::new();
        let bass = f.bands(&tone(60.0, 44100, 200), 2, 44100);
        assert!(bass[0] > 3.0 * bass[2], "{bass:?}");
        let mut f = Filters::new();
        let high = f.bands(&tone(8000.0, 44100, 200), 2, 44100);
        assert!(high[2] > 3.0 * high[0], "{high:?}");
    }

    #[test]
    fn levels_follow_the_loudest_recent() {
        let mut f = Filters::new();
        let l = f.levels([0.2, 0.0, 0.1]);
        assert!(l[0] > 0.99 && l[1] == 0.0);
        let l = f.levels([0.1, 0.0, 0.1]);
        assert!((0.45..0.55).contains(&l[0]), "{l:?}");
    }
}
