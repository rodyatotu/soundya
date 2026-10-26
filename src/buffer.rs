//! Read-ahead buffer: a background thread keeps downloading the stream so that
//! slow HLS segment requests (each one a round trip through the proxy) never
//! leave the decoder waiting and the sound card running dry.

use std::{
    collections::VecDeque,
    io::{self, Read},
    sync::{Arc, Condvar, Mutex},
    thread,
};

/// Download speed and buffered amount, for the connection panel on the page.
pub struct NetStats {
    /// Smoothed download speed in bytes per second.
    pub speed: f64,
    pub updated: Option<std::time::Instant>,
    /// Bytes downloaded ahead of the playing position.
    pub ahead: usize,
}

pub static STATS: Mutex<NetStats> = Mutex::new(NetStats { speed: 0.0, updated: None, ahead: 0 });

fn record_speed(bytes: usize, secs: f64) {
    if secs <= 0.0 {
        return;
    }
    let rate = bytes as f64 / secs;
    let mut s = STATS.lock().unwrap();
    s.speed = if s.updated.is_some() { s.speed * 0.6 + rate * 0.4 } else { rate };
    s.updated = Some(std::time::Instant::now());
}

/// How far ahead to download: ~3 minutes of 160 kbit/s AAC.
const AHEAD: usize = 4 << 20;
const CHUNK: usize = 32 << 10;

struct State {
    data: VecDeque<u8>,
    done: bool,
    error: Option<io::Error>,
    closed: bool,
}

struct Inner {
    state: Mutex<State>,
    changed: Condvar,
}

pub struct Buffered {
    inner: Arc<Inner>,
    /// Not downloading yet: a prefetched next track must not compete with the
    /// current one for the proxy. The download starts on the first read.
    pending: Option<Box<dyn Read + Send + Sync>>,
}

impl Buffered {
    pub fn new(source: Box<dyn Read + Send + Sync>) -> Self {
        let inner = Arc::new(Inner {
            state: Mutex::new(State { data: VecDeque::new(), done: false, error: None, closed: false }),
            changed: Condvar::new(),
        });
        Self { inner, pending: Some(source) }
    }

    fn start(&mut self) {
        let Some(mut source) = self.pending.take() else { return };
        let filler = self.inner.clone();
        thread::spawn(move || {
            let mut chunk = vec![0u8; CHUNK];
            // Speed is measured over network time only (not while the buffer is full).
            let (mut win_bytes, mut win_secs) = (0usize, 0f64);
            loop {
                // Wait while the buffer is full; stop when the reader is gone.
                {
                    let mut st = filler.state.lock().unwrap();
                    while st.data.len() >= AHEAD && !st.closed {
                        st = filler.changed.wait(st).unwrap();
                    }
                    if st.closed {
                        return;
                    }
                }
                let t0 = std::time::Instant::now();
                let result = source.read(&mut chunk);
                if let Ok(n) = result {
                    win_bytes += n;
                    win_secs += t0.elapsed().as_secs_f64();
                    if win_secs >= 1.0 || win_bytes >= 512 << 10 {
                        record_speed(win_bytes, win_secs);
                        win_bytes = 0;
                        win_secs = 0.0;
                    }
                }
                let mut st = filler.state.lock().unwrap();
                match result {
                    Ok(0) => {
                        st.done = true;
                        filler.changed.notify_all();
                        return;
                    }
                    Ok(n) => st.data.extend(&chunk[..n]),
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                    Err(e) => {
                        st.error = Some(e);
                        st.done = true;
                        filler.changed.notify_all();
                        return;
                    }
                }
                filler.changed.notify_all();
            }
        });
    }
}

impl Read for Buffered {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        self.start();
        let mut st = self.inner.state.lock().unwrap();
        while st.data.is_empty() && !st.done {
            st = self.inner.changed.wait(st).unwrap();
        }
        if st.data.is_empty() {
            return match st.error.take() {
                Some(e) => Err(e),
                None => Ok(0),
            };
        }
        let n = buf.len().min(st.data.len());
        let (a, b) = st.data.as_slices();
        if n <= a.len() {
            buf[..n].copy_from_slice(&a[..n]);
        } else {
            buf[..a.len()].copy_from_slice(a);
            buf[a.len()..n].copy_from_slice(&b[..n - a.len()]);
        }
        st.data.drain(..n);
        let ahead = st.data.len();
        self.inner.changed.notify_all();
        drop(st);
        STATS.lock().unwrap().ahead = ahead;
        Ok(n)
    }
}

impl Drop for Buffered {
    fn drop(&mut self) {
        // Lets the download thread exit when the track is skipped or stopped.
        let mut st = self.inner.state.lock().unwrap();
        st.closed = true;
        st.data = VecDeque::new();
        self.inner.changed.notify_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A source that hands out data in small, uneven pieces.
    struct Slow(Vec<u8>, usize);
    impl Read for Slow {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            if self.1 >= self.0.len() {
                return Ok(0);
            }
            thread::sleep(std::time::Duration::from_millis(1));
            let n = buf.len().min(777).min(self.0.len() - self.1);
            buf[..n].copy_from_slice(&self.0[self.1..self.1 + n]);
            self.1 += n;
            Ok(n)
        }
    }

    #[test]
    fn delivers_everything_in_order() {
        let data: Vec<u8> = (0..200_000u32).map(|i| (i * 7 % 251) as u8).collect();
        let mut r = Buffered::new(Box::new(Slow(data.clone(), 0)));
        let mut out = Vec::new();
        let mut buf = [0u8; 4096];
        loop {
            let n = r.read(&mut buf).unwrap();
            if n == 0 {
                break;
            }
            out.extend_from_slice(&buf[..n]);
        }
        assert_eq!(out, data);
    }

    #[test]
    fn passes_errors_after_data() {
        struct Fail(bool);
        impl Read for Fail {
            fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
                if self.0 {
                    return Err(io::Error::other("boom"));
                }
                self.0 = true;
                buf[0] = 42;
                Ok(1)
            }
        }
        let mut r = Buffered::new(Box::new(Fail(false)));
        let mut buf = [0u8; 8];
        assert_eq!(r.read(&mut buf).unwrap(), 1);
        assert!(r.read(&mut buf).is_err());
    }
}
