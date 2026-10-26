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
const DUCK_HOLD_MS: u64 = 60_000;

/// Alice activity: music is quieter while she listens or speaks.
pub struct Duck {
    origin: Instant,
    until: AtomicU64,
}

impl Duck {
    pub fn new() -> Self {
        Self { origin: Instant::now(), until: AtomicU64::new(0) }
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
}

pub enum Outcome {
    Finished,
    Stopped,
    Failed(String),
}

struct Gain(f32);

impl Gain {
    fn apply(&mut self, samples: &mut [i16], channels: usize, rate: u32, ducked: bool) {
        let target = if ducked { DUCK_LEVEL } else { 1.0 };
        if self.0 == 1.0 && target == 1.0 {
            return;
        }
        // 80 ms down, 300 ms back up.
        let span = if target < self.0 { 0.08 } else { 0.3 };
        let step = (1.0 - DUCK_LEVEL) / (rate as f32 * span);
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
    let mut gain = Gain(1.0);
    let mut frames_played: u64 = 0;
    let mut decode_errors = 0;
    let mut reopened = 0;

    loop {
        if flags.stop.load(Ordering::Acquire) {
            if let Some(o) = out.as_mut() {
                o.stop_now();
            }
            return Ok(Outcome::Stopped);
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
        if skip_ms > 0 && (frames_played * 1000 / u64::from(spec.rate.max(1))) < skip_ms {
            // Catching up to where the track broke off: decode without sound.
            frames_played += decoded.frames() as u64;
            flags
                .position_ms
                .store(frames_played * 1000 / spec.rate.max(1) as u64, Ordering::Release);
            continue;
        }
        if out.is_none() {
            out = Some(
                Output::open(device, spec.rate, channels).map_err(|e| format!("alsa: {e}"))?,
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
        gain.apply(samples, channels, spec.rate, duck.active());
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
        frames_played += (samples.len() / channels.max(1)) as u64;
        flags
            .position_ms
            .store(frames_played * 1000 / spec.rate.max(1) as u64, Ordering::Release);
    }
    if let Some(o) = out.as_mut() {
        o.finish();
    }
    Ok(Outcome::Finished)
}

pub fn new_flags() -> Arc<Flags> {
    Arc::new(Flags::default())
}
