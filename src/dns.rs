//! Name resolution that works even when the speaker's own resolver does not.
//!
//! Yandex firmware points /etc/resolv.conf at a local unbound on 127.0.0.1,
//! which is not always running. We try the system resolver first and fall
//! back to asking public DNS servers directly over UDP.

use std::{
    collections::HashMap,
    io,
    net::{IpAddr, Ipv4Addr, SocketAddr, ToSocketAddrs, UdpSocket},
    sync::Mutex,
    time::{Duration, Instant},
};

const SERVERS: [&str; 4] = ["77.88.8.8", "8.8.8.8", "1.1.1.1", "77.88.8.1"];
const TTL: Duration = Duration::from_secs(300);

pub struct Resolver {
    cache: Mutex<HashMap<String, (Vec<IpAddr>, Instant)>>,
}

impl Resolver {
    pub fn new() -> Self {
        Self { cache: Mutex::new(HashMap::new()) }
    }

    fn servers() -> Vec<String> {
        match std::env::var("SOUNDYA_DNS") {
            Ok(s) if !s.trim().is_empty() => s.split([',', ' ']).filter(|x| !x.is_empty()).map(str::to_owned).collect(),
            _ => SERVERS.iter().map(|s| (*s).to_owned()).collect(),
        }
    }

    fn lookup(&self, host: &str) -> io::Result<Vec<IpAddr>> {
        if let Ok(ip) = host.parse::<IpAddr>() {
            return Ok(vec![ip]);
        }
        if let Some((ips, at)) = self.cache.lock().unwrap().get(host) {
            if at.elapsed() < TTL {
                return Ok(ips.clone());
            }
        }
        let ips = lookup_any(host, &Self::servers())?;
        self.cache.lock().unwrap().insert(host.to_owned(), (ips.clone(), Instant::now()));
        Ok(ips)
    }
}

/// Yandex DNS hands out the provider's stub for blocked names (ER-Telecom: i1.sndcdn.com →
/// 188.186.154.79, seen on the owner's speaker); Google's and Cloudflare's answers are real.
fn trusted(server: &str) -> bool {
    !server.starts_with("77.88.8.")
}

static REAL: Mutex<Option<std::collections::HashSet<IpAddr>>> = Mutex::new(None);

fn is_real(ip: &IpAddr) -> bool {
    REAL.lock().unwrap().as_ref().is_some_and(|s| s.contains(ip))
}

/// Asks all `servers` at once; the first answer wins (Yandex's is usually first and
/// gives the nearest CloudFront — the music streams best from it), unless all of it is a
/// known stub of the provider: then the next answer. (One by one, two silent servers
/// before a working one cost 6 s — longer than a check waits.)
fn lookup_any(host: &str, servers: &[String]) -> io::Result<Vec<IpAddr>> {
    let (tx, rx) = std::sync::mpsc::channel();
    for server in servers {
        let (tx, host, server) = (tx.clone(), host.to_owned(), server.clone());
        std::thread::spawn(move || {
            let r = match query(&host, &server) {
                Ok(ips) if ips.is_empty() => Err(io::Error::other(format!("{host}: no A records from {server}"))),
                r => r,
            };
            let _ = tx.send((trusted(&server), r));
        });
    }
    drop(tx);
    let mut last = io::Error::other("no DNS servers");
    let mut stubs_only: Option<Vec<IpAddr>> = None;
    // Each query gives up by itself after 2 × 1.5 s.
    while let Ok((is_trusted, r)) = rx.recv() {
        match r {
            Ok(ips) => {
                if is_trusted {
                    REAL.lock().unwrap().get_or_insert_with(Default::default).extend(ips.iter().copied());
                }
                if ips.iter().all(is_stub) {
                    stubs_only.get_or_insert(ips);
                    continue;
                }
                return Ok(ips);
            }
            Err(e) => last = e,
        }
    }
    match stubs_only {
        Some(ips) => Ok(ips),
        None => Err(last),
    }
}

// SoundCloud's names point at several servers, and a provider may let some of
// them through and not others (the bypass then «works, then does not» as the
// DNS answer rotates). A server that failed is put last for a while, so the
// next try goes to another one.
const BAD_FOR: Duration = Duration::from_secs(600);
static BAD: Mutex<Option<HashMap<IpAddr, Instant>>> = Mutex::new(None);
/// The address handed out first for each host (ureq connects to it first).
static FIRST: Mutex<Option<HashMap<String, IpAddr>>> = Mutex::new(None);

/// The server last used for `host` did not answer: try the others first.
pub fn failed(host: &str) {
    let host = host.trim_start_matches("https://").trim_start_matches("http://");
    let host = host.split(['/', ':']).next().unwrap_or(host);
    let Some(ip) = FIRST.lock().unwrap().as_ref().and_then(|m| m.get(host).copied()) else { return };
    BAD.lock().unwrap().get_or_insert_with(HashMap::new).insert(ip, Instant::now());
    eprintln!("dns: {host} at {ip} did not answer; trying its other servers first");
}

/// Addresses that answered with someone else's certificate: the provider's stub for blocked
/// names (ER-Telecom: «certificate only valid for *.ertelecom.ru»). Bad for every name and for
/// the whole run, not ten minutes: the stub never becomes SoundCloud.
static STUB: Mutex<Option<std::collections::HashSet<IpAddr>>> = Mutex::new(None);

/// `host` answered with a certificate for another name: its address is a stub.
pub fn stub(host: &str) {
    let host = host.trim_start_matches("https://").trim_start_matches("http://");
    let host = host.split(['/', ':', '?']).next().unwrap_or(host);
    let Some(ip) = FIRST.lock().unwrap().as_ref().and_then(|m| m.get(host).copied()) else { return };
    // A real address (Google's or Cloudflare's answer): the provider filters the name there;
    // writing the address off only took good servers away (CloudFront serves many names).
    if is_real(&ip) {
        static SAID: Mutex<Option<std::collections::HashSet<String>>> = Mutex::new(None);
        if SAID.lock().unwrap().get_or_insert_with(Default::default).insert(host.to_owned()) {
            eprintln!("dns: {host} at {ip}: wrong certificate on its real address — the provider filters the name; the address is kept");
        }
        return;
    }
    if STUB.lock().unwrap().get_or_insert_with(Default::default).insert(ip) {
        eprintln!("dns: {host} at {ip} is the provider's stub (wrong certificate); public DNS from now on");
    }
}

fn is_stub(ip: &IpAddr) -> bool {
    STUB.lock().unwrap().as_ref().is_some_and(|s| s.contains(ip))
}

/// A TLS error saying the server's certificate is for another name.
pub fn wrong_certificate(message: &str) -> bool {
    let m = message.to_ascii_lowercase();
    m.contains("not valid for name") || m.contains("notvalidforname") || m.contains("certificate is only valid for")
}

/// Good servers first, recently failed ones last; remembers which comes first.
fn order(host: &str, mut addrs: Vec<SocketAddr>) -> Vec<SocketAddr> {
    {
        let mut bad = BAD.lock().unwrap();
        let bad = bad.get_or_insert_with(HashMap::new);
        bad.retain(|_, at| at.elapsed() < BAD_FOR);
        addrs.sort_by_key(|a| bad.contains_key(&a.ip()));
    }
    if let Some(first) = addrs.first() {
        FIRST.lock().unwrap().get_or_insert_with(HashMap::new).insert(host.to_owned(), first.ip());
    }
    addrs
}

/// The system resolver's answer, unless every address in it failed lately: a
/// provider's DNS may answer a blocked name with its own stub (one address that
/// never answers), and reordering that answer alone never gets past it. Then
/// public DNS (`public`) is asked and its addresses go first; the system's stay last.
fn choose(
    name: &str,
    system: Vec<SocketAddr>,
    public: impl FnOnce() -> io::Result<Vec<IpAddr>>,
    port: u16,
) -> io::Result<Vec<SocketAddr>> {
    let all_failed = !system.is_empty() && {
        let mut bad = BAD.lock().unwrap();
        let bad = bad.get_or_insert_with(HashMap::new);
        bad.retain(|_, at| at.elapsed() < BAD_FOR);
        system.iter().all(|a| bad.contains_key(&a.ip()) || is_stub(&a.ip()))
    };
    if !system.is_empty() && !all_failed {
        return Ok(order(name, system));
    }
    match public() {
        Ok(ips) if !ips.is_empty() => {
            let mut v: Vec<SocketAddr> = ips.into_iter().map(|ip| SocketAddr::new(ip, port)).collect();
            if all_failed {
                eprintln!("dns: {name}: the local DNS answer does not work; using public DNS");
            }
            for a in system {
                if !v.contains(&a) {
                    v.push(a);
                }
            }
            // A known stub never goes first, and not at all while there is anything else.
            if v.iter().any(|a| !is_stub(&a.ip())) {
                v.retain(|a| !is_stub(&a.ip()));
            }
            Ok(order(name, v))
        }
        _ if !system.is_empty() => Ok(order(name, system)),
        Ok(_) => Err(io::Error::other(format!("{name}: no A records"))),
        Err(e) => Err(e),
    }
}

impl ureq::Resolver for Resolver {
    fn resolve(&self, netloc: &str) -> io::Result<Vec<SocketAddr>> {
        let (host, port) = netloc
            .rsplit_once(':')
            .ok_or_else(|| io::Error::other(format!("bad address {netloc}")))?;
        let port: u16 = port.parse().map_err(io::Error::other)?;
        let host = host.trim_start_matches('[').trim_end_matches(']');
        // System resolver first (fails fast when 127.0.0.1:53 refuses).
        let system: Vec<SocketAddr> = netloc.to_socket_addrs().map(Iterator::collect).unwrap_or_default();
        choose(host, system, || self.lookup(host), port)
    }
}

fn build_query(id: u16, host: &str) -> io::Result<Vec<u8>> {
    let mut q = Vec::with_capacity(32 + host.len());
    q.extend_from_slice(&id.to_be_bytes());
    q.extend_from_slice(&[0x01, 0x00, 0, 1, 0, 0, 0, 0, 0, 0]); // RD, 1 question
    for label in host.trim_end_matches('.').split('.') {
        if label.is_empty() || label.len() > 63 {
            return Err(io::Error::other(format!("bad host name {host}")));
        }
        q.push(label.len() as u8);
        q.extend_from_slice(label.as_bytes());
    }
    q.extend_from_slice(&[0, 0, 1, 0, 1]); // root, type A, class IN
    Ok(q)
}

fn skip_name(buf: &[u8], mut i: usize) -> Option<usize> {
    loop {
        let len = *buf.get(i)? as usize;
        if len & 0xC0 == 0xC0 {
            return Some(i + 2);
        }
        if len == 0 {
            return Some(i + 1);
        }
        i += 1 + len;
    }
}

fn parse_response(id: u16, buf: &[u8]) -> io::Result<Vec<IpAddr>> {
    let bad = || io::Error::other("malformed DNS response");
    if buf.len() < 12 || u16::from_be_bytes([buf[0], buf[1]]) != id {
        return Err(bad());
    }
    let rcode = buf[3] & 0x0F;
    if rcode != 0 {
        return Err(io::Error::other(format!("DNS error code {rcode}")));
    }
    let qd = u16::from_be_bytes([buf[4], buf[5]]) as usize;
    let an = u16::from_be_bytes([buf[6], buf[7]]) as usize;
    let mut i = 12;
    for _ in 0..qd {
        i = skip_name(buf, i).ok_or_else(bad)? + 4;
    }
    let mut ips = Vec::new();
    for _ in 0..an {
        i = skip_name(buf, i).ok_or_else(bad)?;
        let head = buf.get(i..i + 10).ok_or_else(bad)?;
        let kind = u16::from_be_bytes([head[0], head[1]]);
        let len = u16::from_be_bytes([head[8], head[9]]) as usize;
        i += 10;
        let data = buf.get(i..i + len).ok_or_else(bad)?;
        if kind == 1 && len == 4 {
            ips.push(IpAddr::V4(Ipv4Addr::new(data[0], data[1], data[2], data[3])));
        }
        i += len;
    }
    Ok(ips)
}

fn query(host: &str, server: &str) -> io::Result<Vec<IpAddr>> {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    let id = (nanos as u16) ^ ((nanos >> 16) as u16) ^ (std::process::id() as u16);
    let packet = build_query(id, host)?;
    let socket = UdpSocket::bind("0.0.0.0:0")?;
    socket.set_read_timeout(Some(Duration::from_millis(1500)))?;
    // "1.1.1.1" (port 53) or "host:port" (tests).
    let addr: SocketAddr = if server.contains(':') { server.parse() } else { format!("{server}:53").parse() }.map_err(io::Error::other)?;
    let mut buf = [0u8; 1500];
    for _ in 0..2 {
        socket.send_to(&packet, addr)?;
        match socket.recv_from(&mut buf) {
            Ok((n, from)) if from == addr => return parse_response(id, &buf[..n]),
            Ok(_) => continue,
            Err(e) if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut) => continue,
            Err(e) => return Err(e),
        }
    }
    Err(io::Error::new(io::ErrorKind::TimedOut, format!("DNS {server} did not answer")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_failed_server_goes_last() {
        let a: SocketAddr = "1.1.1.1:443".parse().unwrap();
        let b: SocketAddr = "2.2.2.2:443".parse().unwrap();
        assert_eq!(order("x.test", vec![a, b])[0], a);
        failed("https://x.test/path");
        assert_eq!(order("x.test", vec![a, b]), vec![b, a]);
        // Now b is first: if it fails too, both are bad and the order stays stable.
        failed("x.test");
        assert_eq!(order("x.test", vec![a, b]).len(), 2);
    }

    #[test]
    fn query_and_response_round_trip() {
        let q = build_query(0x1234, "soundcloud.com").unwrap();
        assert_eq!(&q[12..], b"\x0asoundcloud\x03com\x00\x00\x01\x00\x01");
        // Response: same question, one CNAME (pointer name) and one A record.
        let mut r = q.clone();
        r[2] = 0x81;
        r[3] = 0x80;
        r[7] = 2; // ANCOUNT = 2
        r.extend_from_slice(&[0xC0, 12, 0, 5, 0, 1, 0, 0, 0, 60, 0, 2, 0xC0, 12]);
        r.extend_from_slice(&[0xC0, 12, 0, 1, 0, 1, 0, 0, 0, 60, 0, 4, 52, 84, 150, 57]);
        assert_eq!(parse_response(0x1234, &r).unwrap(), vec![IpAddr::V4(Ipv4Addr::new(52, 84, 150, 57))]);
        assert!(parse_response(0x9999, &r).is_err());
        for n in 0..r.len() {
            let _ = parse_response(0x1234, &r[..n]); // never panics
        }
    }

    #[test]
    fn a_stub_from_the_provider_is_replaced_by_public_dns() {
        // The provider's DNS answers soundcloud.com with its own stub (one address that
        // never answers). Once it failed, public DNS must be asked; before, the stub
        // stayed the only address and soundcloud.com never opened.
        let stub: SocketAddr = "188.186.146.207:443".parse().unwrap();
        let real: IpAddr = "52.84.150.39".parse().unwrap();
        assert_eq!(choose("stub.test", vec![stub], || panic!("asked public DNS while the system answer works"), 443).unwrap(), vec![stub]);
        FIRST.lock().unwrap().get_or_insert_with(HashMap::new).insert("stub.test".into(), stub.ip());
        failed("https://stub.test/");
        let got = choose("stub.test", vec![stub], || Ok(vec![real]), 443).unwrap();
        assert_eq!(got[0], SocketAddr::new(real, 443));
        assert_eq!(got.last(), Some(&stub), "the system answer stays, last");
        // Public DNS unreachable too: the system answer is all there is.
        assert_eq!(choose("stub.test", vec![stub], || Err(io::Error::other("timeout")), 443).unwrap(), vec![stub]);
    }

    #[test]
    fn a_stub_certificate_sends_every_name_on_it_to_public_dns() {
        // ER-Telecom answers soundcloud.com and i1.sndcdn.com with the same stub address;
        // its certificate is for *.ertelecom.ru. Covers and the client_id never loaded.
        let stub_addr: SocketAddr = "188.186.146.208:443".parse().unwrap();
        let real: IpAddr = "52.84.150.40".parse().unwrap();
        assert!(wrong_certificate("tls connection init failed: invalid peer certificate: certificate not valid for name \"i1.sndcdn.com\"; certificate is only valid for DnsName(\"*.ertelecom.ru\")"));
        assert!(!wrong_certificate("Connection Failed: Resource temporarily unavailable (os error 11)"));
        FIRST.lock().unwrap().get_or_insert_with(HashMap::new).insert("site.stub-test".into(), stub_addr.ip());
        stub("https://site.stub-test/");
        // Another name on the same stub goes to public DNS at once, without failing first.
        let got = choose("cdn.stub-test", vec![stub_addr], || Ok(vec![real]), 443).unwrap();
        assert_eq!(got[0], SocketAddr::new(real, 443));
    }

    #[test]
    fn a_real_address_is_not_written_off_and_a_stub_never_comes_back() {
        // The owner's speaker: Yandex DNS gave i1.sndcdn.com the stub 188.186.154.79 again
        // after it was marked, and a real CloudFront address (from 1.1.1.1) was marked a stub
        // for the provider's name filtering. Covers hung for 15 s.
        let real: IpAddr = "18.165.122.92".parse().unwrap();
        REAL.lock().unwrap().get_or_insert_with(Default::default).insert(real);
        FIRST.lock().unwrap().get_or_insert_with(HashMap::new).insert("cdn.real-test".into(), real);
        stub("https://cdn.real-test/a.jpg");
        assert!(!is_stub(&real), "a real address stays usable");
        let stub_ip: IpAddr = "188.186.154.79".parse().unwrap();
        FIRST.lock().unwrap().get_or_insert_with(HashMap::new).insert("cdn.stub2-test".into(), stub_ip);
        stub("https://cdn.stub2-test/");
        assert!(is_stub(&stub_ip));
        // Public DNS (Yandex) hands the stub out again: it is dropped, the real one is used.
        let got = choose("cdn.stub2-test", vec![SocketAddr::new(stub_ip, 443)], || Ok(vec![stub_ip, real]), 443).unwrap();
        assert_eq!(got, vec![SocketAddr::new(real, 443)]);
        assert!(trusted("1.1.1.1") && trusted("8.8.8.8") && !trusted("77.88.8.8") && !trusted("77.88.8.1"));
    }

    #[test]
    fn a_silent_dns_server_does_not_hold_up_the_answer() {
        // Where 77.88.8.8 and 8.8.8.8 do not answer (they did not at ER-Telecom) and
        // 1.1.1.1 does, asking one by one took 6 s, past the bypass check's 8 s with
        // the connection: public servers are asked at once, the first answer wins.
        let silent = UdpSocket::bind("127.0.0.1:0").unwrap();
        let answering = UdpSocket::bind("127.0.0.1:0").unwrap();
        let servers = vec![silent.local_addr().unwrap().to_string(), answering.local_addr().unwrap().to_string()];
        std::thread::spawn(move || {
            let mut buf = [0u8; 512];
            let (n, from) = answering.recv_from(&mut buf).unwrap();
            let mut r = buf[..n].to_vec();
            r[2] = 0x81;
            r[3] = 0x80;
            r[7] = 1; // one answer
            r.extend_from_slice(&[0xC0, 0x0C, 0, 1, 0, 1, 0, 0, 0, 60, 0, 4, 1, 2, 3, 4]);
            answering.send_to(&r, from).unwrap();
        });
        let t = Instant::now();
        let ips = lookup_any("x.test", &servers).unwrap();
        assert_eq!(ips, vec![IpAddr::V4(Ipv4Addr::new(1, 2, 3, 4))]);
        assert!(t.elapsed() < Duration::from_secs(1), "took {:?}", t.elapsed());
        drop(silent);
    }
}
