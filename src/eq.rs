//! Equalizer for what soundya plays itself (SoundCloud, AirPlay): ten bands of
//! peaking filters (RBJ biquads) on the samples before they go to the card.
//! Alice's own sound does not pass through soundya and is not touched.
//!
//! The settings live here (the page changes them at any moment); every stream
//! keeps its own filter state and picks new settings up on its next block.
//!
//! Loudness (0.5.00). Before, everything was turned down by the largest band
//! boost (+6 dB on the bass made the whole track 6 dB quieter, the bass merely
//! stayed where it was): «the EQ lowers the volume». Clipping does need
//! headroom, but only where the boosted signal actually reaches full scale.
//! Now: the real peak of the combined response is computed (neighbouring
//! boosted bands add up, which the old rule missed), only half of it is taken
//! as fixed headroom, and a peak limiter takes the rest only on the samples
//! that would clip — instead of hard clipping or a constant loss of level.

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
    /// Fixed headroom: half of the response's peak boost.
    pre: f32,
    /// Peak limiter: current gain and its release step per frame.
    lim: f32,
    release: f32,
}

/// Limiter ceiling: −0.2 dBFS.
const CEILING: f32 = 32_000.0;
/// Limiter release time, s.
const RELEASE_S: f32 = 0.15;

/// Magnitude of one biquad at `freq`, as a power ratio.
fn power(c: &Coeffs, freq: f32, rate: u32) -> f32 {
    let w = 2.0 * std::f32::consts::PI * freq / rate as f32;
    let (c1, c2) = (w.cos(), (2.0 * w).cos());
    let (s1, s2) = (w.sin(), (2.0 * w).sin());
    let nr = c.b0 + c.b1 * c1 + c.b2 * c2;
    let ni = -(c.b1 * s1 + c.b2 * s2);
    let dr = 1.0 + c.a1 * c1 + c.a2 * c2;
    let di = -(c.a1 * s1 + c.a2 * s2);
    (nr * nr + ni * ni) / (dr * dr + di * di)
}

/// Highest gain of the whole chain between 20 Hz and 20 kHz (or Nyquist), dB.
fn peak_db(filters: &[Coeffs], rate: u32) -> f32 {
    if filters.is_empty() {
        return 0.0;
    }
    let top = (rate as f32 * 0.45).min(20_000.0);
    let steps = 160;
    (0..=steps)
        .map(|i| 20.0 * (top / 20.0).powf(i as f32 / steps as f32))
        .map(|f| 10.0 * filters.iter().map(|c| power(c, f, rate)).product::<f32>().log10())
        .fold(f32::MIN, f32::max)
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
        let peak = peak_db(&filters, rate).max(0.0);
        self.pre = 10f32.powf(-(peak / 2.0) / 20.0);
        self.release = 1.0 - (-1.0 / (RELEASE_S * rate.max(1) as f32)).exp();
        if self.lim == 0.0 {
            self.lim = 1.0;
        }
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
        let mut out = [0f32; 8];
        for frame in samples.chunks_exact_mut(ch) {
            let mut peak = 0f32;
            for (c, s) in frame.iter().enumerate() {
                let mut x = *s as f32 * self.pre;
                for (f, st) in self.filters.iter().zip(&mut self.state[c * n..(c + 1) * n]) {
                    let y = f.b0 * x + f.b1 * st[0] + f.b2 * st[1] - f.a1 * st[2] - f.a2 * st[3];
                    st[1] = st[0];
                    st[0] = x;
                    st[3] = st[2];
                    st[2] = y;
                    x = y;
                }
                if c < out.len() {
                    out[c] = x;
                }
                peak = peak.max(x.abs());
            }
            // Peak limiter: down at once on a sample that would clip, back up slowly.
            if peak * self.lim > CEILING {
                self.lim = CEILING / peak;
            } else {
                self.lim += (1.0 - self.lim) * self.release;
            }
            for (c, s) in frame.iter_mut().enumerate() {
                let x = if c < out.len() { out[c] } else { 0.0 };
                *s = (x * self.lim).round().clamp(-32768.0, 32767.0) as i16;
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

    fn sine_amp(freq: f32, rate: u32, n: usize, amp: f32) -> Vec<i16> {
        (0..n)
            .flat_map(|i| {
                let v = ((2.0 * std::f32::consts::PI * freq * i as f32 / rate as f32).sin() * amp) as i16;
                [v, v]
            })
            .collect()
    }

    fn eq_db(freq: f32, amp: f32) -> f32 {
        let rate = 44100;
        let orig = rms(&sine_amp(freq, rate, 44100, amp));
        let mut s = sine_amp(freq, rate, 44100, amp);
        Eq::new().apply(&mut s, 2, rate);
        20.0 * (rms(&s) / orig).log10()
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

        // The reported bug: +12 dB at 1 kHz made everything else 12 dB quieter.
        // Now half of it is headroom: other bands lose 6 dB, 1 kHz gains 6 dB.
        set(true, &[0, 0, 0, 0, 0, 12, 0, 0, 0, 0]);
        let db = eq_db(1000.0, 8000.0);
        assert!((db - 6.0).abs() < 1.0, "1 kHz: {db} dB");
        let db = eq_db(62.0, 8000.0);
        assert!((db + 6.0).abs() < 1.0, "62 Hz: {db} dB (was −12)");

        // A full-scale tone boosted by 12 dB is limited, not hard-clipped.
        let mut s = sine_amp(1000.0, rate, 44100, 32000.0);
        Eq::new().apply(&mut s, 2, rate);
        let tail = &s[s.len() / 2..];
        let peak = tail.iter().map(|v| (*v as i32).abs()).max().unwrap();
        assert!(peak <= CEILING as i32 + 1, "peak {peak}");
        assert!(!tail.iter().any(|v| *v == i16::MAX || *v == i16::MIN), "hard clipping");

        // Neighbouring boosts add up: the headroom follows the real response.
        set(true, &[0, 0, 0, 0, 12, 12, 0, 0, 0, 0]);
        let filters: Vec<Coeffs> = [500.0, 1000.0].iter().map(|&f| peaking(f, 12.0, rate)).collect();
        let peak = peak_db(&filters, rate);
        assert!(peak > 13.0, "combined peak {peak} dB");

        // −12 dB at 62 Hz: no headroom taken, 62 Hz down 12 dB, 4 kHz untouched.
        set(true, &[0, -12, 0, 0, 0, 0, 0, 0, 0, 0]);
        let db = eq_db(62.0, 8000.0);
        assert!((db + 12.0).abs() < 1.0, "62 Hz cut: {db} dB");
        let db = eq_db(4000.0, 8000.0);
        assert!(db.abs() < 0.5, "4 kHz: {db} dB");

        // 16 kHz at 22050 Hz is above Nyquist: skipped, no blow-up.
        set(true, &[0, 0, 0, 0, 0, 0, 0, 0, 0, 12]);
        let mut s = sine(1000.0, 22050, 22050);
        Eq::new().apply(&mut s, 2, 22050);
        assert!(rms(&s) > 1000.0);
        set(false, &[0; BANDS]);
    }
}
