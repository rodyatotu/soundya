//! Decodes one track and writes it to ALSA, honouring pause, stop and ducking.

use std::{
    io::Read,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc,
    },
    thread,
    time::{Duration, Instant},
};

use symphonia::core::{
    audio::SampleBuffer,
    codecs::{DecoderOptions, CODEC_TYPE_NULL},
    errors::Error as SymError,
    formats::FormatOptions,
    io::{MediaSourceStream, MediaSourceStreamOptions, ReadOnlySource},
    meta::MetadataOptions,
    probe::Hint,
};

use crate::{alsa::Output, sc::Kind};

const DUCK_LEVEL: f32 = 0.2;
/// How far written sound may run ahead of the clock (a card's own buffer is well under this).
const MAX_AHEAD_MS: u64 = 3000;
const DUCK_HOLD_MS: u64 = 60_000;

/// Alice activity: music is quieter while she listens or speaks. Also the
/// level set by Smart Ducking (ducking.rs): calls, notifications, by voice.
pub struct Duck {
    origin: Instant,
    until: AtomicU64,
    /// Smart Ducking level in per mille of soundya's own sound (1000 = none).
    external: std::sync::atomic::AtomicU32,
    /// Someone listens in the browser («Слушать здесь»): SoundCloud on the
    /// speaker fades out (AirPlay is not affected: it is not in the browser).
    listening: AtomicBool,
}

impl Duck {
    pub fn new() -> Self {
        Self {
            origin: Instant::now(),
            until: AtomicU64::new(0),
            external: std::sync::atomic::AtomicU32::new(1000),
            listening: AtomicBool::new(false),
        }
    }
    /// 0.0..=1.0 of soundya's sound (not the speaker's volume, which stays the user's).
    pub fn set_external(&self, level: f32) {
        self.external.store((level.clamp(0.0, 1.0) * 1000.0).round() as u32, Ordering::Release);
    }
    pub fn set_listening(&self, on: bool) {
        self.listening.store(on, Ordering::Release);
    }
    pub fn listening(&self) -> bool {
        self.listening.load(Ordering::Acquire)
    }
    /// SoundCloud on the speaker: silent while it plays in a browser.
    pub fn player_level(&self) -> f32 {
        if self.listening() { 0.0 } else { self.level() }
    }
    /// What soundya's music should play at now: the lower of Alice's and Smart Ducking.
    pub fn level(&self) -> f32 {
        let ext = self.external.load(Ordering::Acquire) as f32 / 1000.0;
        if self.active() { ext.min(DUCK_LEVEL) } else { ext }
    }
    fn now(&self) -> u64 {
        self.origin.elapsed().as_millis() as u64
    }
    pub fn set_busy(&self, busy: bool) {
        let until = if busy { self.now() + DUCK_HOLD_MS } else { 0 };
        self.until.store(until, Ordering::Release);
    }
    pub fn active(&self) -> bool {
        self.until.load(Ordering::Acquire) > self.now()
    }
}

/// Per-track controls shared with the controller.
#[derive(Default)]
pub struct Flags {
    pub stop: AtomicBool,
    pub paused: AtomicBool,
    pub position_ms: AtomicU64,
    /// Requested jump: target position + 1 (0 = none). See `Flags::seek`.
    seek: AtomicU64,
    /// Another sound card for the rest of the track (Bluetooth on/off): the same download goes
    /// on, only the card is reopened — no new stream, no jump.
    move_to: std::sync::Mutex<Option<String>>,
}

impl Flags {
    /// Jumps to `ms`. Forward jumps continue the same download (the audio in
    /// between is decoded silently); a jump back ends the track with
    /// `Outcome::Seek`, so it is opened again from the start.
    pub fn seek(&self, ms: u64) {
        self.seek.store(ms + 1, Ordering::Release);
    }

    /// Plays on from here on `device` (an ALSA name, as `play` takes).
    pub fn move_to(&self, device: &str) {
        *self.move_to.lock().unwrap() = Some(device.to_owned());
    }
}

pub enum Outcome {
    Finished,
    Stopped,
    Failed(String),
    /// Jump back to this position: open the track again and skip to it.
    Seek(u64),
}

/// Volume ramp for ducking (Alice's voice, Smart Ducking).
pub struct Gain(f32);

impl Default for Gain {
    fn default() -> Self {
        Self(1.0)
    }
}

impl Gain {
    /// `target`: 0..=1 of full level (Duck::level()).
    pub fn apply(&mut self, samples: &mut [i16], channels: usize, rate: u32, target: f32) {
        let target = target.clamp(0.0, 1.0);
        if self.0 == 1.0 && target == 1.0 {
            return;
        }
        // 80 ms down, 300 ms back up (for the whole range); fading out
        // completely (music moved to the browser) takes a second, both ways.
        let span = if target < 0.01 || (self.0 < 0.01 && target > 0.5) { 1.0 } else if target < self.0 { 0.08 } else { 0.3 };
        let step = 1.0 / (rate as f32 * span);
        for frame in samples.chunks_exact_mut(channels.max(1)) {
            self.0 = if target < self.0 { (self.0 - step).max(target) } else { (self.0 + step).min(target) };
            for s in frame {
                *s = (*s as f32 * self.0) as i16;
            }
        }
    }
}

/// `skip_ms`: resume a track that broke off, without sound up to that point.
/// `duration_ms`: the track's length, to tell a broken download from the real end.
pub fn play(
    source: Box<dyn Read + Send + Sync>,
    kind: Kind,
    device: &str,
    flags: &Flags,
    duck: &Duck,
    skip_ms: u64,
    duration_ms: u64,
) -> Outcome {
    match run(source, kind, device, flags, duck, skip_ms, duration_ms) {
        Ok(o) => o,
        Err(e) => Outcome::Failed(e),
    }
}

fn run(
    source: Box<dyn Read + Send + Sync>,
    kind: Kind,
    device: &str,
    flags: &Flags,
    duck: &Duck,
    skip_ms: u64,
    duration_ms: u64,
) -> Result<Outcome, String> {
    let mss = MediaSourceStream::new(
        Box::new(ReadOnlySource::new(source)),
        MediaSourceStreamOptions { buffer_len: 256 * 1024 },
    );
    let mut hint = Hint::new();
    hint.with_extension(kind.extension());
    let probed = symphonia::default::get_probe()
        .format(&hint, mss, &FormatOptions::default(), &MetadataOptions::default())
        .map_err(|e| format!("probe: {e}"))?;
    eprintln!("player: stream recognised");
    let mut format = probed.format;
    let track = format
        .tracks()
        .iter()
        .find(|t| t.codec_params.codec != CODEC_TYPE_NULL)
        .ok_or("no audio track")?;
    let track_id = track.id;
    let mut decoder = symphonia::default::get_codecs()
        .make(&track.codec_params, &DecoderOptions::default())
        .map_err(|e| format!("decoder: {e}"))?;

    let mut out: Option<Output> = None;
    let mut buf: Option<SampleBuffer<i16>> = None;
    let mut buf_frames = 0usize;
    let mut pcm: Vec<i16> = Vec::new();
    // From the level it should be at now, not full: with the music in the browser a new
    // track started loud and faded out over a second (half a second of it from the speaker).
    let mut gain = Gain(duck.player_level());
    let mut eq = crate::eq::Eq::new();
    let mut frames_played: u64 = 0;
    // Since the card was opened: (when, track position then). A card takes sound only as fast
    // as it plays, but BlueALSA with a stalling headset takes it at once: the track then «played»
    // minutes ahead, and moving back to the speaker jumped there. Sound never runs more than a
    // few seconds ahead of the clock.
    let mut clock: Option<(std::time::Instant, u64)> = None;
    let mut device = device.to_owned();
    let mut decode_errors = 0;
    let mut reopened = 0;
    // Decoded silently up to here: resuming after a broken download, or a jump forward.
    let mut skip_until = skip_ms;
    let mut rate: u64 = 0;

    loop {
        if flags.stop.load(Ordering::Acquire) {
            if let Some(o) = out.as_mut() {
                o.stop_now();
            }
            return Ok(Outcome::Stopped);
        }
        if let Some(d) = flags.move_to.lock().unwrap().take() {
            if d != device {
                eprintln!("player: moving to {d}");
                device = d;
                // What waits in the old card is dropped; the next packet opens the new one.
                if let Some(mut o) = out.take() {
                    o.stop_now();
                }
            }
        }
        let seek = flags.seek.swap(0, Ordering::AcqRel);
        if seek > 0 {
            let target = seek - 1;
            let now = if rate > 0 { frames_played * 1000 / rate } else { 0 };
            if target < now {
                if let Some(o) = out.as_mut() {
                    o.stop_now();
                }
                return Ok(Outcome::Seek(target));
            }
            eprintln!("player: jump to {}:{:02}", target / 60_000, target / 1000 % 60);
            skip_until = target;
            // Drop what is queued in the card, so the jump is heard at once.
            if let Some(mut o) = out.take() {
                o.stop_now();
            }
        }
        if flags.paused.load(Ordering::Acquire) {
            // Release the sound card while paused, so Alice can use the speaker
            // freely; it is opened again with the next packet.
            if let Some(mut o) = out.take() {
                o.stop_now();
            }
            eprintln!("player: paused");
            while flags.paused.load(Ordering::Acquire) && !flags.stop.load(Ordering::Acquire) {
                thread::sleep(Duration::from_millis(50));
            }
            eprintln!("player: resumed");
            continue;
        }

        let position = flags.position_ms.load(Ordering::Acquire);
        // Stopping more than 5 s before the known length is a broken download, not the end.
        let cut_short = duration_ms > 0 && position + 5_000 < duration_ms;
        let packet = match format.next_packet() {
            Ok(p) => p,
            Err(SymError::IoError(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof && !cut_short => break,
            Err(SymError::ResetRequired) => break,
            Err(e) => {
                if !cut_short && frames_played > 0 {
                    break;
                }
                return Err(format!("stream interrupted at {}:{:02}: {e}", position / 60_000, position / 1000 % 60));
            }
        };
        if packet.track_id() != track_id {
            continue;
        }
        let decoded = match decoder.decode(&packet) {
            Ok(d) => d,
            Err(SymError::DecodeError(e)) => {
                decode_errors += 1;
                if decode_errors > 50 {
                    return Err(format!("decode: {e}"));
                }
                continue;
            }
            Err(e) => return Err(format!("decode: {e}")),
        };
        let spec = *decoded.spec();
        let channels = spec.channels.count();
        rate = u64::from(spec.rate.max(1));
        if skip_until > 0 && (frames_played * 1000 / rate) < skip_until {
            // Catching up to where the track broke off: decode without sound.
            frames_played += decoded.frames() as u64;
            flags
                .position_ms
                .store(frames_played * 1000 / spec.rate.max(1) as u64, Ordering::Release);
            continue;
        }
        if out.is_none() {
            clock = None;
            out = Some(
                Output::open(&device, spec.rate, channels).map_err(|e| format!("alsa: {e}"))?,
            );
            eprintln!("player: sound card open, {} Hz, {} ch", spec.rate, channels);
        }
        let need = decoded.capacity();
        if buf_frames < need {
            buf = Some(SampleBuffer::new(need as u64, spec));
            buf_frames = need;
        }
        let sb = buf.as_mut().unwrap();
        sb.copy_interleaved_ref(decoded);
        pcm.clear();
        pcm.extend_from_slice(sb.samples());
        let samples = pcm.as_mut_slice();
        eq.apply(samples, channels, spec.rate);
        gain.apply(samples, channels, spec.rate, duck.player_level());
        if let Err(e) = out.as_mut().unwrap().write(samples) {
            if e.kind() == std::io::ErrorKind::TimedOut && reopened < 2 {
                // Close and reopen the card; the track goes on from the next packet.
                reopened += 1;
                eprintln!("alsa: {e}; reopening the sound card");
                out = None;
                continue;
            }
            return Err(format!("alsa write: {e}"));
        }
        if crate::vis::ON.load(Ordering::Relaxed) {
            crate::vis::feed(samples, channels, spec.rate, out.as_ref().and_then(|o| o.queued()));
        }
        let written_from = frames_played * 1000 / rate;
        frames_played += (samples.len() / channels.max(1)) as u64;
        let written = frames_played * 1000 / rate;
        let (since, from) = *clock.get_or_insert((std::time::Instant::now(), written_from));
        let ahead = (written - from).saturating_sub(since.elapsed().as_millis() as u64);
        if ahead > MAX_AHEAD_MS {
            thread::sleep(Duration::from_millis(ahead - MAX_AHEAD_MS + 500));
        }
        // Where the listener is: what was written less what still waits in the card.
        let queued = out.as_ref().and_then(|o| o.queued()).map_or(0, |f| f as u64 * 1000 / rate);
        flags.position_ms.store(written.saturating_sub(queued), Ordering::Release);
    }
    if let Some(o) = out.as_mut() {
        o.finish();
    }
    Ok(Outcome::Finished)
}

pub fn new_flags() -> Arc<Flags> {
    Arc::new(Flags::default())
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_new_track_is_silent_while_the_browser_listens() {
        let duck = super::Duck::new();
        duck.set_listening(true);
        let mut gain = super::Gain(duck.player_level());
        let mut samples = vec![12_000i16; 4410 * 2];
        gain.apply(&mut samples, 2, 44_100, duck.player_level());
        assert!(samples.iter().all(|s| *s == 0), "the first 100 ms reached the speaker");
        // Back on the speaker: it fades in from silence, not a jump.
        duck.set_listening(false);
        let mut samples = vec![12_000i16; 441 * 2];
        gain.apply(&mut samples, 2, 44_100, duck.player_level());
        assert!(samples[0].abs() < 100 && samples[samples.len() - 1] < 12_000);
    }
}
