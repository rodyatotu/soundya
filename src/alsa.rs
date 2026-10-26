//! Tiny libasound binding loaded with dlopen, so the binary builds without ALSA
//! headers and uses the speaker's own libasound and plugins.

use std::{
    ffi::{c_char, c_int, c_long, c_uint, c_ulong, c_void, CStr, CString},
    io,
    sync::OnceLock,
};

type Pcm = *mut c_void;

struct Lib {
    open: unsafe extern "C" fn(*mut Pcm, *const c_char, c_int, c_int) -> c_int,
    set_params: unsafe extern "C" fn(Pcm, c_int, c_int, c_uint, c_uint, c_int, c_uint) -> c_int,
    writei: unsafe extern "C" fn(Pcm, *const c_void, c_ulong) -> c_long,
    recover: unsafe extern "C" fn(Pcm, c_int, c_int) -> c_int,
    drop: unsafe extern "C" fn(Pcm) -> c_int,
    wait: unsafe extern "C" fn(Pcm, c_int) -> c_int,
    state: unsafe extern "C" fn(Pcm) -> c_int,
    start: unsafe extern "C" fn(Pcm) -> c_int,
    avail_update: unsafe extern "C" fn(Pcm) -> c_long,
    get_params: unsafe extern "C" fn(Pcm, *mut c_ulong, *mut c_ulong) -> c_int,
    close: unsafe extern "C" fn(Pcm) -> c_int,
    strerror: unsafe extern "C" fn(c_int) -> *const c_char,
}

unsafe impl Send for Lib {}
unsafe impl Sync for Lib {}

// glibc / Linux values (ARM and x86 alike).
const RTLD_NOW: c_int = 2;
const RTLD_GLOBAL: c_int = 0x100;
const EAGAIN: c_int = 11;
const EBUSY: c_int = 16;

extern "C" {
    fn dlopen(filename: *const c_char, flag: c_int) -> *mut c_void;
    fn dlsym(handle: *mut c_void, symbol: *const c_char) -> *mut c_void;
}

const SND_PCM_STREAM_PLAYBACK: c_int = 0;
const SND_PCM_STATE_PREPARED: c_int = 2;
/// Never block inside libasound: a stuck sound card must not hang the player.
const SND_PCM_NONBLOCK: c_int = 1;
/// No sound accepted for this long means the card is stuck: reopen it.
const STALL: std::time::Duration = std::time::Duration::from_secs(3);
const SND_PCM_FORMAT_S16_LE: c_int = 2;
const SND_PCM_ACCESS_RW_INTERLEAVED: c_int = 3;

fn lib() -> io::Result<&'static Lib> {
    static LIB: OnceLock<Result<Lib, String>> = OnceLock::new();
    LIB.get_or_init(|| unsafe {
        let mut handle = std::ptr::null_mut();
        for name in [c"libasound.so.2", c"libasound.so"] {
            handle = dlopen(name.as_ptr(), RTLD_NOW | RTLD_GLOBAL);
            if !handle.is_null() {
                break;
            }
        }
        if handle.is_null() {
            return Err("cannot dlopen libasound.so.2".to_owned());
        }
        macro_rules! sym {
            ($name:literal) => {{
                let p = dlsym(handle, concat!($name, "\0").as_ptr().cast());
                if p.is_null() {
                    return Err(format!("libasound has no {}", $name));
                }
                std::mem::transmute::<*mut c_void, _>(p)
            }};
        }
        Ok(Lib {
            open: sym!("snd_pcm_open"),
            set_params: sym!("snd_pcm_set_params"),
            writei: sym!("snd_pcm_writei"),
            recover: sym!("snd_pcm_recover"),
            drop: sym!("snd_pcm_drop"),
            wait: sym!("snd_pcm_wait"),
            state: sym!("snd_pcm_state"),
            start: sym!("snd_pcm_start"),
            avail_update: sym!("snd_pcm_avail_update"),
            get_params: sym!("snd_pcm_get_params"),
            close: sym!("snd_pcm_close"),
            strerror: sym!("snd_strerror"),
        })
    })
    .as_ref()
    .map_err(|e| io::Error::other(e.clone()))
}

fn check(lib: &Lib, what: &str, code: c_int) -> io::Result<()> {
    if code < 0 {
        let msg = unsafe { CStr::from_ptr((lib.strerror)(code)) }.to_string_lossy();
        return Err(io::Error::other(format!("{what}: {msg}")));
    }
    Ok(())
}

pub struct Output {
    lib: &'static Lib,
    pcm: Pcm,
    channels: usize,
}

unsafe impl Send for Output {}

impl Output {
    pub fn open(device: &str, rate: u32, channels: usize) -> io::Result<Self> {
        let lib = lib()?;
        let name = CString::new(device).map_err(io::Error::other)?;
        let mut pcm: Pcm = std::ptr::null_mut();
        // The previous track may still be closing its handle: retry briefly on EBUSY.
        let mut attempt = 0;
        loop {
            let code = unsafe { (lib.open)(&mut pcm, name.as_ptr(), SND_PCM_STREAM_PLAYBACK, SND_PCM_NONBLOCK) };
            if code == -EBUSY && attempt < 30 {
                attempt += 1;
                std::thread::sleep(std::time::Duration::from_millis(100));
                continue;
            }
            check(lib, "snd_pcm_open", code)?;
            break;
        }
        let out = Self { lib, pcm, channels };
        check(lib, "snd_pcm_set_params", unsafe {
            (lib.set_params)(
                pcm,
                SND_PCM_FORMAT_S16_LE,
                SND_PCM_ACCESS_RW_INTERLEAVED,
                channels as c_uint,
                rate,
                1,       // allow software resampling
                400_000, // 400 ms of sound queued in the card
            )
        })?;
        Ok(out)
    }

    /// Writes interleaved S16 samples, recovering from underruns. Fails with
    /// `ErrorKind::TimedOut` if the card accepts nothing for a few seconds.
    pub fn write(&mut self, mut samples: &[i16]) -> io::Result<()> {
        let mut progress = std::time::Instant::now();
        while samples.len() >= self.channels {
            let frames = samples.len() / self.channels;
            let n = unsafe { (self.lib.writei)(self.pcm, samples.as_ptr().cast(), frames as c_ulong) };
            if n < 0 {
                let err = n as c_int;
                if err == -EAGAIN {
                    // Buffer full but the stream never started on its own (seen with the
                    // speaker's softvol → plug → dmix chain): start it explicitly.
                    if unsafe { (self.lib.state)(self.pcm) } == SND_PCM_STATE_PREPARED {
                        let code = unsafe { (self.lib.start)(self.pcm) };
                        eprintln!("alsa: buffer full but not playing; started explicitly ({code})");
                    }
                    // Buffer full: wait for room, but not forever.
                    if progress.elapsed() > STALL {
                        return Err(io::Error::new(io::ErrorKind::TimedOut, "sound card stopped accepting audio"));
                    }
                    unsafe { (self.lib.wait)(self.pcm, 200) };
                    continue;
                }
                // -32 (EPIPE) is an underrun: audio arrived too late. Logged to diagnose stutter.
                eprintln!("alsa: recovering from error {err}");
                check(self.lib, "snd_pcm_recover", unsafe { (self.lib.recover)(self.pcm, err, 1) })?;
                continue;
            }
            samples = &samples[n as usize * self.channels..];
            progress = std::time::Instant::now();
        }
        Ok(())
    }

    /// Drops queued audio (pause or skip).
    pub fn stop_now(&mut self) {
        unsafe {
            (self.lib.drop)(self.pcm);
        }
    }

    /// Lets the queued audio play out (at most a second and a half), so the end
    /// of a track is not cut off.
    pub fn finish(&mut self) {
        let (mut buffer, mut period): (c_ulong, c_ulong) = (0, 0);
        if unsafe { (self.lib.get_params)(self.pcm, &mut buffer, &mut period) } < 0 || buffer == 0 {
            return;
        }
        let start = std::time::Instant::now();
        while start.elapsed() < std::time::Duration::from_millis(1500) {
            let avail = unsafe { (self.lib.avail_update)(self.pcm) };
            if avail < 0 || avail as c_ulong >= buffer {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }
}

impl Drop for Output {
    fn drop(&mut self) {
        unsafe {
            (self.lib.close)(self.pcm);
        }
    }
}
