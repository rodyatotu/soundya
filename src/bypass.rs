//! «Обход блокировок без прокси»: the same tricks zapret's tpws plays, inside
//! soundya's own HTTPS connections.
//!
//! Russian DPI (ТСПУ) reads the site name (SNI) from the first packet of a TLS
//! connection, the ClientHello, and drops connections to blocked sites. Here the
//! ClientHello is cut so the name is not in one piece: across two TCP packets,
//! across two TLS records, or both. The server puts it back together; the DPI
//! box often does not. Nothing changes for the speaker's other programs.
//!
//! Helps only against blocking by name. When the addresses themselves are
//! blocked, only a proxy or VPN helps — [`pick`] then reports that no strategy works.

use std::{
    io::{self, Read, Write},
    net::TcpStream,
    sync::{Arc, Mutex},
    time::Duration,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Strategy {
    /// No trick: straight to SoundCloud, ignoring the proxy (it opens without one).
    Direct,
    /// First byte in its own TCP packet (tpws --split-pos=1).
    Split1,
    /// TCP packets cut in the middle of the site name (--split-pos=midsld).
    SplitSni,
    /// Two TLS records, cut inside the site name (--tlsrec=sni).
    TlsRec,
    /// Both: two TLS records, and the TCP packets cut inside the name too.
    TlsRecSplit,
}

pub const ALL: [Strategy; 4] = [Strategy::TlsRecSplit, Strategy::TlsRec, Strategy::SplitSni, Strategy::Split1];

impl Strategy {
    pub fn id(self) -> &'static str {
        match self {
            Strategy::Direct => "direct",
            Strategy::Split1 => "split1",
            Strategy::SplitSni => "splitsni",
            Strategy::TlsRec => "tlsrec",
            Strategy::TlsRecSplit => "tlsrec+split",
        }
    }

    pub fn from_id(id: &str) -> Option<Strategy> {
        std::iter::once(Strategy::Direct).chain(ALL).find(|s| s.id() == id)
    }

    /// The ClientHello as the pieces to send, each in its own TCP packet.
    pub fn pieces(self, hello: &[u8]) -> Vec<Vec<u8>> {
        let sni = sni_span(hello);
        match (self, sni) {
            (Strategy::Split1, _) if hello.len() > 1 => vec![hello[..1].to_vec(), hello[1..].to_vec()],
            (Strategy::SplitSni, Some((start, len))) => {
                let at = start + len / 2;
                vec![hello[..at].to_vec(), hello[at..].to_vec()]
            }
            (Strategy::TlsRec, Some((start, len))) => vec![split_record(hello, start + len / 2).unwrap_or_else(|| hello.to_vec())],
            (Strategy::TlsRecSplit, Some((start, len))) => match split_record(hello, start + len / 2) {
                // The extra record header (5 bytes) shifts the name by 5.
                Some(two) => {
                    let at = start + 5 + len / 2 + 2;
                    vec![two[..at.min(two.len())].to_vec(), two[at.min(two.len())..].to_vec()]
                }
                None => vec![hello.to_vec()],
            },
            _ => vec![hello.to_vec()],
        }
    }
}

/// Where the server name sits in a ClientHello record: (offset, length).
pub fn sni_span(rec: &[u8]) -> Option<(usize, usize)> {
    let be16 = |i: usize| -> Option<usize> { Some(usize::from(*rec.get(i)?) << 8 | usize::from(*rec.get(i + 1)?)) };
    // TLS record: 0x16 handshake; handshake type 1: ClientHello.
    if rec.first() != Some(&0x16) || rec.get(5) != Some(&0x01) {
        return None;
    }
    let mut i = 5 + 4 + 2 + 32; // handshake header, version, random
    i += 1 + usize::from(*rec.get(i)?); // session id
    i += 2 + be16(i)?; // cipher suites
    i += 1 + usize::from(*rec.get(i)?); // compression methods
    let ext_end = i + 2 + be16(i)?;
    i += 2;
    while i + 4 <= ext_end.min(rec.len()) {
        let (kind, len) = (be16(i)?, be16(i + 2)?);
        if kind == 0 {
            // server_name: list length (2), type (1), name length (2), name
            let name_len = be16(i + 4 + 3)?;
            let start = i + 4 + 5;
            return (start + name_len <= rec.len()).then_some((start, name_len));
        }
        i += 4 + len;
    }
    None
}

/// Cuts one TLS record into two at absolute offset `at` (inside its payload).
pub fn split_record(rec: &[u8], at: usize) -> Option<Vec<u8>> {
    if rec.len() < 6 || at <= 5 || at >= rec.len() {
        return None;
    }
    let len = usize::from(rec[3]) << 8 | usize::from(rec[4]);
    if 5 + len != rec.len() {
        return None; // more than one record in the write: leave it alone
    }
    let (first, second) = (&rec[5..at], &rec[at..]);
    let mut out = Vec::with_capacity(rec.len() + 5);
    for part in [first, second] {
        out.extend_from_slice(&rec[..3]); // type and version
        out.extend_from_slice(&(part.len() as u16).to_be_bytes());
        out.extend_from_slice(part);
    }
    Some(out)
}

/// The TCP connection as rustls sees it: the first write (the ClientHello)
/// goes out cut into pieces, everything else unchanged.
#[derive(Debug)]
struct Cut {
    io: Box<dyn ureq::ReadWrite>,
    strategy: Strategy,
    done: bool,
}

impl Read for Cut {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.io.read(buf)
    }
}

impl Write for Cut {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if self.done || buf.first() != Some(&0x16) {
            self.done = true;
            return self.io.write(buf);
        }
        self.done = true;
        let pieces = self.strategy.pieces(buf);
        if let Some(s) = self.io.socket() {
            let _ = s.set_nodelay(true);
        }
        for (n, p) in pieces.iter().enumerate() {
            self.io.write_all(p)?;
            self.io.flush()?;
            if n + 1 < pieces.len() {
                // A moment between pieces so they leave as separate packets.
                std::thread::sleep(Duration::from_millis(3));
            }
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.io.flush()
    }
}

impl ureq::ReadWrite for Cut {
    fn socket(&self) -> Option<&TcpStream> {
        self.io.socket()
    }
}

/// rustls with the ClientHello cut by `strategy`.
pub struct Connector {
    tls: Arc<ureq::rustls::ClientConfig>,
    strategy: Strategy,
}

fn tls_config() -> Arc<ureq::rustls::ClientConfig> {
    use ureq::rustls;
    let roots = rustls::RootCertStore { roots: webpki_roots::TLS_SERVER_ROOTS.to_vec() };
    Arc::new(
        rustls::ClientConfig::builder_with_provider(rustls::crypto::ring::default_provider().into())
            .with_protocol_versions(&[&rustls::version::TLS12, &rustls::version::TLS13])
            .expect("TLS versions")
            .with_root_certificates(roots)
            .with_no_client_auth(),
    )
}

impl Connector {
    pub fn new(strategy: Strategy) -> Self {
        Connector { tls: tls_config(), strategy }
    }
}

impl ureq::TlsConnector for Connector {
    fn connect(&self, dns_name: &str, io: Box<dyn ureq::ReadWrite>) -> Result<Box<dyn ureq::ReadWrite>, ureq::Error> {
        let cut = Cut { io, strategy: self.strategy, done: false };
        ureq::TlsConnector::connect(&self.tls, dns_name, Box::new(cut))
    }
}

/// The strategy in use (from the settings); None: off.
static ACTIVE: Mutex<Option<Strategy>> = Mutex::new(None);

pub fn set(strategy: Option<Strategy>) {
    *ACTIVE.lock().unwrap() = strategy;
}

pub fn active() -> Option<Strategy> {
    *ACTIVE.lock().unwrap()
}

/// What has to open for music to play: the API, the audio servers and the site
/// (soundya reads its client_id from there).
const CHECK: [&str; 3] = ["https://api-v2.soundcloud.com/", "https://cf-hls-media.sndcdn.com/", "https://soundcloud.com/"];
/// Below this the music stutters (a 128 kbit/s track plus headroom).
pub const ENOUGH_KBPS: f64 = 400.0;

fn agent_for(strategy: Strategy) -> ureq::Agent {
    let mut b = ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(6))
        .timeout(Duration::from_secs(12))
        .resolver(crate::dns::Resolver::new());
    if strategy != Strategy::Direct {
        b = b.tls_connector(Arc::new(Connector::new(strategy)));
    }
    b.build()
}

/// Does SoundCloud answer with this strategy (or with no trick at all)?
fn works(agent: &ureq::Agent, strategy: Strategy) -> bool {
    CHECK.iter().all(|url| match agent.get(url).call() {
        Ok(_) | Err(ureq::Error::Status(..)) => true, // any HTTP answer: the connection went through
        Err(e) => {
            eprintln!("bypass: {} on {url}: {e}", strategy.id());
            false
        }
    })
}

/// A big file on SoundCloud's CDN (the site's script), to measure speed with:
/// providers slow SoundCloud's servers down to a trickle rather than block them.
fn cdn_file(agent: &ureq::Agent) -> Option<String> {
    let html = agent.get("https://soundcloud.com/").call().ok()?.into_string().ok()?;
    crate::sc::script_urls(&html).into_iter().last()
}

/// Download speed in kbit/s: up to 768 KB or 6 seconds of the file.
fn speed(agent: &ureq::Agent, url: &str) -> f64 {
    let started = std::time::Instant::now();
    let Ok(resp) = agent.get(url).call() else { return 0.0 };
    let mut body = resp.into_reader();
    let mut buf = [0u8; 16 * 1024];
    let mut got = 0usize;
    while got < 768 * 1024 && started.elapsed() < Duration::from_secs(6) {
        match body.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => got += n,
        }
    }
    got as f64 * 8.0 / 1000.0 / started.elapsed().as_secs_f64().max(0.001)
}

/// The fastest way that gets to SoundCloud: straight, or one of the strategies.
/// Returns it with its speed (kbit/s); None when nothing gets through.
pub fn pick() -> Option<(Strategy, f64)> {
    let mut best: Option<(Strategy, f64)> = None;
    let mut file: Option<String> = None;
    for s in std::iter::once(Strategy::Direct).chain(ALL) {
        let agent = agent_for(s);
        if !works(&agent, s) {
            continue;
        }
        if file.is_none() {
            file = cdn_file(&agent);
        }
        let kbps = file.as_deref().map_or(0.0, |f| speed(&agent, f));
        eprintln!("bypass: {} gets through at {kbps:.0} kbit/s", s.id());
        if best.map_or(true, |(_, b)| kbps > b) {
            best = Some((s, kbps));
        }
        // Fast enough with no trick at all: nothing better to look for.
        if s == Strategy::Direct && kbps >= 4.0 * ENOUGH_KBPS {
            break;
        }
    }
    best
}

#[cfg(test)]
mod tests {
    use super::*;

    const HELLO: &[u8] = include_bytes!("../testdata/clienthello.bin");
    const NAME: &[u8] = b"api-v2.soundcloud.com";

    fn records(data: &[u8]) -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        let mut i = 0;
        while i + 5 <= data.len() {
            let len = usize::from(data[i + 3]) << 8 | usize::from(data[i + 4]);
            out.push(data[i + 5..i + 5 + len].to_vec());
            i += 5 + len;
        }
        assert_eq!(i, data.len(), "records fill the data exactly");
        out
    }

    #[test]
    fn finds_the_site_name() {
        let (start, len) = sni_span(HELLO).unwrap();
        assert_eq!(&HELLO[start..start + len], NAME);
        assert_eq!(sni_span(b"\x17\x03\x03\x00\x01x"), None);
    }

    #[test]
    fn every_strategy_keeps_the_hello_intact() {
        for s in ALL {
            let pieces = s.pieces(HELLO);
            let joined: Vec<u8> = pieces.concat();
            // The handshake bytes, records unwrapped, are exactly the original's.
            let payload: Vec<u8> = records(&joined).concat();
            assert_eq!(payload, records(HELLO).concat(), "{s:?}");
            // And the whole name never sits in one piece.
            if s != Strategy::Split1 {
                assert!(pieces.iter().all(|p| !p.windows(NAME.len()).any(|w| w == NAME)), "{s:?}");
            }
        }
        assert_eq!(Strategy::TlsRec.pieces(HELLO).len(), 1);
        assert_eq!(records(&Strategy::TlsRec.pieces(HELLO)[0]).len(), 2);
        assert_eq!(Strategy::TlsRecSplit.pieces(HELLO).len(), 2);
        assert_eq!(Strategy::Split1.pieces(HELLO)[0], vec![0x16]);
    }

    #[test]
    fn ids_round_trip() {
        for s in ALL {
            assert_eq!(Strategy::from_id(s.id()), Some(s));
        }
    }
}
