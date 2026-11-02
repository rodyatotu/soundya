//! Equalizer for what soundya plays itself (SoundCloud, AirPlay): ten bands of
//! peaking filters (RBJ biquads) on the samples before they go to the card.
//! Alice's own sound does not pass through soundya and is not touched.
//!
//! The settings live here (the page changes them at any moment); every stream
//! keeps its own filter state and picks new settings up on its next block.

use std::sync::{
    atomic::{AtomicU32, Ordering},
    Mutex,
};

pub const BANDS: usize = 10;
/// Band centres, Hz.
pub const FREQS: [f32; BANDS] = [31.0, 62.0, 125.0, 250.0, 500.0, 1000.0, 2000.0, 4000.0, 8000.0, 16000.0];
/// ±12 dB per band.
pub const MAX_DB: i32 = 12;
/// Octave-wide bands.
const Q: f32 = 1.41;

/// (on, gains in dB) and a counter bumped on every change.
static SETTINGS: Mutex<(bool, [i8; BANDS])> = Mutex::new((false, [0; BANDS]));
static VERSION: AtomicU32 = AtomicU32::new(1);

/// New settings from the page; out-of-range gains are clamped.
pub fn set(on: bool, gains: &[i8]) {
    let mut g = [0i8; BANDS];
    for (dst, src) in g.iter_mut().zip(gains) {
        *dst = (*src as i32).clamp(-MAX_DB, MAX_DB) as i8;
    }
    let mut s = SETTINGS.lock().unwrap();
    if *s != (on, g) {
        *s = (on, g);
        VERSION.fetch_add(1, Ordering::AcqRel);
    }
}

#[derive(Clone, Copy)]
struct Coeffs {
    b0: f32,
    b1: f32,
    b2: f32,
    a1: f32,
    a2: f32,
}

/// RBJ «Audio EQ Cookbook» peaking filter.
fn peaking(freq: f32, db: f32, rate: u32) -> Coeffs {
    let a = 10f32.powf(db / 40.0);
    let w = 2.0 * std::f32::consts::PI * freq / rate as f32;
    let alpha = w.sin() / (2.0 * Q);
    let cos = w.cos();
    let a0 = 1.0 + alpha / a;
    Coeffs {
        b0: (1.0 + alpha * a) / a0,
        b1: -2.0 * cos / a0,
        b2: (1.0 - alpha * a) / a0,
        a1: -2.0 * cos / a0,
        a2: (1.0 - alpha / a) / a0,
    }
}

/// One stream's equalizer.
#[derive(Default)]
pub struct Eq {
    version: u32,
    rate: u32,
    channels: usize,
    /// Only the bands that are not at 0 dB.
    filters: Vec<Coeffs>,
    /// Per channel, per filter: x1, x2, y1, y2.
    state: Vec<[f32; 4]>,
    /// Lowers everything by the largest boost, so boosted bands do not clip.
    pre: f32,
}

impl Eq {
    pub fn new() -> Self {
        Self::default()
    }

    fn refresh(&mut self, channels: usize, rate: u32) {
        let version = VERSION.load(Ordering::Acquire);
        if version == self.version && rate == self.rate && channels == self.channels {
            return;
        }
        let (on, gains) = *SETTINGS.lock().unwrap();
        let nyquist = rate as f32 / 2.0;
        let filters: Vec<Coeffs> = if on {
            FREQS
                .iter()
                .zip(gains)
                .filter(|&(&f, g)| g != 0 && f < nyquist * 0.9)
                .map(|(&f, g)| peaking(f, g as f32, rate))
                .collect()
        } else {
            Vec::new()
        };
        let boost = if on { gains.iter().copied().max().unwrap_or(0).max(0) } else { 0 };
        self.pre = 10f32.powf(-(boost as f32) / 20.0);
        // Keep the running state when only the gains changed: no click.
        if filters.len() != self.filters.len() || channels != self.channels || rate != self.rate {
            self.state = vec![[0.0; 4]; filters.len() * channels];
        }
        self.filters = filters;
        self.version = version;
        self.rate = rate;
        self.channels = channels;
    }

    /// Equalizes interleaved 16-bit samples in place.
    pub fn apply(&mut self, samples: &mut [i16], channels: usize, rate: u32) {
        let ch = channels.max(1);
        self.refresh(ch, rate);
        if self.filters.is_empty() {
            return;
        }
        let n = self.filters.len();
        for frame in samples.chunks_exact_mut(ch) {
            for (c, s) in frame.iter_mut().enumerate() {
                let mut x = *s as f32 * self.pre;
                for (f, st) in self.filters.iter().zip(&mut self.state[c * n..(c + 1) * n]) {
                    let y = f.b0 * x + f.b1 * st[0] + f.b2 * st[1] - f.a1 * st[2] - f.a2 * st[3];
                    st[1] = st[0];
                    st[0] = x;
                    st[3] = st[2];
                    st[2] = y;
                    x = y;
                }
                *s = x.round().clamp(-32768.0, 32767.0) as i16;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sine(freq: f32, rate: u32, n: usize) -> Vec<i16> {
        (0..n)
            .flat_map(|i| {
                let v = ((2.0 * std::f32::consts::PI * freq * i as f32 / rate as f32).sin() * 8000.0) as i16;
                [v, v]
            })
            .collect()
    }

    fn rms(s: &[i16]) -> f32 {
        let tail = &s[s.len() / 2..];
        (tail.iter().map(|&v| (v as f32).powi(2)).sum::<f32>() / tail.len() as f32).sqrt()
    }

    // One test: the settings are global.
    #[test]
    fn bands_boost_and_cut() {
        let rate = 44100;
        let orig = rms(&sine(1000.0, rate, 44100));

        set(false, &[0, 0, 0, 0, 0, 12, 0, 0, 0, 0]);
        let mut s = sine(1000.0, rate, 44100);
        Eq::new().apply(&mut s, 2, rate);
        assert_eq!(rms(&s), orig, "off: untouched");

        // +12 dB at 1 kHz with the pre-gain: 1 kHz stays about the same, 62 Hz drops 12 dB.
        set(true, &[0, 0, 0, 0, 0, 12, 0, 0, 0, 0]);
        let mut s = sine(1000.0, rate, 44100);
        Eq::new().apply(&mut s, 2, rate);
        let db = 20.0 * (rms(&s) / orig).log10();
        assert!(db.abs() < 1.0, "1 kHz: {db} dB");
        let low = rms(&sine(62.0, rate, 44100));
        let mut s = sine(62.0, rate, 44100);
        Eq::new().apply(&mut s, 2, rate);
        let db = 20.0 * (rms(&s) / low).log10();
        assert!((db + 12.0).abs() < 1.0, "62 Hz: {db} dB");

        // −12 dB at 62 Hz: no pre-gain, 62 Hz down 12 dB, 4 kHz untouched.
        set(true, &[0, -12, 0, 0, 0, 0, 0, 0, 0, 0]);
        let mut s = sine(62.0, rate, 44100);
        Eq::new().apply(&mut s, 2, rate);
        let db = 20.0 * (rms(&s) / low).log10();
        assert!((db + 12.0).abs() < 1.0, "62 Hz cut: {db} dB");
        let hi = rms(&sine(4000.0, rate, 44100));
        let mut s = sine(4000.0, rate, 44100);
        Eq::new().apply(&mut s, 2, rate);
        let db = 20.0 * (rms(&s) / hi).log10();
        assert!(db.abs() < 0.5, "4 kHz: {db} dB");

        // 16 kHz at 22050 Hz is above Nyquist: skipped, no blow-up.
        set(true, &[0, 0, 0, 0, 0, 0, 0, 0, 0, 12]);
        let mut s = sine(1000.0, 22050, 22050);
        Eq::new().apply(&mut s, 2, 22050);
        assert!(rms(&s) > 1000.0);
        set(false, &[0; BANDS]);
    }
}
