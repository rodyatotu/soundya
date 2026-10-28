//! Answers multicast DNS for `soundya.local`, so the page is reachable by name
//! even when the speaker gets a new IP address, and advertises services
//! (AirPlay, `_raop._tcp`) published with [`publish`].
//!
//! The port (5353) may already be used by the firmware's own discovery, so the
//! socket is opened with SO_REUSEADDR/SO_REUSEPORT and shares it.

use std::{
    ffi::{c_int, c_void},
    io,
    net::{Ipv4Addr, SocketAddr, SocketAddrV4, UdpSocket},
    os::fd::FromRawFd,
    sync::{
        atomic::{AtomicBool, Ordering},
        Mutex,
    },
    thread,
    time::{Duration, Instant},
};

const GROUP: Ipv4Addr = Ipv4Addr::new(224, 0, 0, 251);
const PORT: u16 = 5353;
const TTL: u32 = 120;

// Linux values (ARM and x86 alike).
const AF_INET: c_int = 2;
const SOCK_DGRAM: c_int = 2;
const SOL_SOCKET: c_int = 1;
const SO_REUSEADDR: c_int = 2;
const SO_REUSEPORT: c_int = 15;

#[repr(C)]
struct SockAddrIn {
    family: u16,
    port: u16,
    addr: u32,
    zero: [u8; 8],
}

extern "C" {
    fn socket(domain: c_int, kind: c_int, protocol: c_int) -> c_int;
    fn setsockopt(fd: c_int, level: c_int, name: c_int, value: *const c_void, len: u32) -> c_int;
    fn bind(fd: c_int, addr: *const SockAddrIn, len: u32) -> c_int;
    fn close(fd: c_int) -> c_int;
}

fn shared_socket() -> io::Result<UdpSocket> {
    unsafe {
        let fd = socket(AF_INET, SOCK_DGRAM, 0);
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        let one: c_int = 1;
        for opt in [SO_REUSEADDR, SO_REUSEPORT] {
            setsockopt(fd, SOL_SOCKET, opt, (&one as *const c_int).cast(), 4);
        }
        let addr = SockAddrIn { family: AF_INET as u16, port: PORT.to_be(), addr: 0, zero: [0; 8] };
        if bind(fd, &addr, std::mem::size_of::<SockAddrIn>() as u32) < 0 {
            let e = io::Error::last_os_error();
            close(fd);
            return Err(e);
        }
        Ok(UdpSocket::from_raw_fd(fd))
    }
}

/// The address other devices reach us on: the source of a route to the internet.
fn own_ip() -> Option<Ipv4Addr> {
    let probe = UdpSocket::bind("0.0.0.0:0").ok()?;
    probe.connect("8.8.8.8:53").ok()?; // no packet is sent
    match probe.local_addr().ok()? {
        SocketAddr::V4(a) if !a.ip().is_loopback() && !a.ip().is_unspecified() => Some(*a.ip()),
        _ => None,
    }
}

#[cfg(test)]
fn encode_name(name: &str, out: &mut Vec<u8>) {
    for label in name.trim_end_matches('.').split('.') {
        out.push(label.len() as u8);
        out.extend_from_slice(label.as_bytes());
    }
    out.push(0);
}

/// Reads a (possibly compressed) name starting at `i`; returns it and the offset after it.
fn read_name(buf: &[u8], mut i: usize) -> Option<(String, usize)> {
    let mut name = String::new();
    let mut end = None;
    for _ in 0..32 {
        let len = *buf.get(i)? as usize;
        if len & 0xC0 == 0xC0 {
            let ptr = ((len & 0x3F) << 8) | *buf.get(i + 1)? as usize;
            end.get_or_insert(i + 2);
            i = ptr;
            continue;
        }
        if len == 0 {
            return Some((name, end.unwrap_or(i + 1)));
        }
        let label = buf.get(i + 1..i + 1 + len)?;
        if !name.is_empty() {
            name.push('.');
        }
        name.push_str(&String::from_utf8_lossy(label).to_lowercase());
        i += 1 + len;
    }
    None
}

/// A DNS-SD service: `<instance>.<kind>.local` on `port` of this host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Service {
    /// "AABBCCDDEEFF@Станция" (any UTF-8, no dots needed).
    pub instance: String,
    /// "_raop._tcp".
    pub kind: String,
    pub port: u16,
    /// "key=value" entries.
    pub txt: Vec<String>,
}

static SERVICES: Mutex<Vec<Service>> = Mutex::new(Vec::new());
/// Services changed: announce (or say goodbye) right away.
static CHANGED: AtomicBool = AtomicBool::new(false);
static GOODBYE: Mutex<Vec<Service>> = Mutex::new(Vec::new());

/// Advertises a service (replacing one of the same kind).
pub fn publish(service: Service) {
    let mut list = SERVICES.lock().unwrap();
    if list.iter().any(|s| *s == service) {
        return;
    }
    let (gone, kept): (Vec<_>, Vec<_>) = list.drain(..).partition(|s| s.kind == service.kind);
    *list = kept;
    list.push(service);
    GOODBYE.lock().unwrap().extend(gone);
    CHANGED.store(true, Ordering::Release);
}

/// Stops advertising services of this kind.
pub fn withdraw(kind: &str) {
    let mut list = SERVICES.lock().unwrap();
    let (gone, kept): (Vec<_>, Vec<_>) = list.drain(..).partition(|s| s.kind == kind);
    *list = kept;
    if !gone.is_empty() {
        GOODBYE.lock().unwrap().extend(gone);
        CHANGED.store(true, Ordering::Release);
    }
}

/// The questions of a query: (lower-case name, type, unicast reply wanted).
fn questions(buf: &[u8]) -> Vec<(String, u16, bool)> {
    let mut out = Vec::new();
    if buf.len() < 12 || buf[2] & 0x80 != 0 {
        return out; // too short, or a response
    }
    let count = u16::from_be_bytes([buf[4], buf[5]]);
    let mut i = 12;
    for _ in 0..count {
        let Some((name, next)) = read_name(buf, i) else { break };
        let (Some(k), Some(c)) = (buf.get(next..next + 2), buf.get(next + 2..next + 4)) else { break };
        let kind = u16::from_be_bytes([k[0], k[1]]);
        let class = u16::from_be_bytes([c[0], c[1]]);
        out.push((name, kind, class & 0x8000 != 0));
        i = next + 4;
    }
    out
}

/// Does this query ask for `host` (type A or ANY)? Returns whether a unicast reply was asked for.
#[cfg(test)]
fn asks_for(buf: &[u8], host: &str) -> Option<bool> {
    questions(buf).into_iter().find(|(n, k, _)| n == host && (*k == T_A || *k == T_ANY)).map(|q| q.2)
}

const T_A: u16 = 1;
const T_PTR: u16 = 12;
const T_TXT: u16 = 16;
const T_SRV: u16 = 33;
const T_ANY: u16 = 255;
const SERVICES_LIST: &str = "_services._dns-sd._udp.local";

struct Record {
    name: Vec<String>,
    kind: u16,
    /// Unique records (A, SRV, TXT) set cache-flush; shared ones (PTR) do not.
    unique: bool,
    ttl: u32,
    data: Vec<u8>,
}

fn labels(name: &str) -> Vec<String> {
    name.trim_end_matches('.').split('.').map(str::to_owned).collect()
}

fn encode_labels(labels: &[String], out: &mut Vec<u8>) {
    for l in labels {
        let b = l.as_bytes();
        let b = &b[..b.len().min(63)];
        out.push(b.len() as u8);
        out.extend_from_slice(b);
    }
    out.push(0);
}

fn instance_name(s: &Service) -> Vec<String> {
    let mut n = vec![s.instance.clone()];
    n.extend(labels(&s.kind));
    n.push("local".into());
    n
}

fn kind_name(s: &Service) -> Vec<String> {
    let mut n = labels(&s.kind);
    n.push("local".into());
    n
}

fn a_record(host: &str, ip: Ipv4Addr, ttl: u32) -> Record {
    Record { name: labels(host), kind: T_A, unique: true, ttl, data: ip.octets().to_vec() }
}

fn ptr_record(s: &Service, ttl: u32) -> Record {
    let mut data = Vec::new();
    encode_labels(&instance_name(s), &mut data);
    Record { name: kind_name(s), kind: T_PTR, unique: false, ttl, data }
}

fn srv_record(s: &Service, host: &str, ttl: u32) -> Record {
    let mut data = vec![0, 0, 0, 0]; // priority, weight
    data.extend_from_slice(&s.port.to_be_bytes());
    encode_labels(&labels(host), &mut data);
    Record { name: instance_name(s), kind: T_SRV, unique: true, ttl, data }
}

fn txt_record(s: &Service, ttl: u32) -> Record {
    let mut data = Vec::new();
    for t in &s.txt {
        let b = &t.as_bytes()[..t.len().min(255)];
        data.push(b.len() as u8);
        data.extend_from_slice(b);
    }
    if data.is_empty() {
        data.push(0);
    }
    Record { name: instance_name(s), kind: T_TXT, unique: true, ttl, data }
}

fn packet(answers: &[Record], additional: &[Record]) -> Vec<u8> {
    let mut p = vec![0, 0, 0x84, 0, 0, 0]; // authoritative response, no questions
    p.extend_from_slice(&(answers.len() as u16).to_be_bytes());
    p.extend_from_slice(&[0, 0]);
    p.extend_from_slice(&(additional.len() as u16).to_be_bytes());
    for r in answers.iter().chain(additional) {
        encode_labels(&r.name, &mut p);
        p.extend_from_slice(&r.kind.to_be_bytes());
        p.extend_from_slice(&(if r.unique { 0x8001u16 } else { 1 }).to_be_bytes());
        p.extend_from_slice(&r.ttl.to_be_bytes());
        p.extend_from_slice(&(r.data.len() as u16).to_be_bytes());
        p.extend_from_slice(&r.data);
    }
    p
}

#[cfg(test)]
fn answer(host: &str, ip: Ipv4Addr) -> Vec<u8> {
    packet(&[a_record(host, ip, TTL)], &[])
}

/// Everything about a service, as announced: PTR, SRV, TXT and our address.
fn service_records(s: &Service, host: &str, ip: Ipv4Addr, ttl: u32) -> (Vec<Record>, Vec<Record>) {
    (
        vec![ptr_record(s, ttl), srv_record(s, host, ttl), txt_record(s, ttl)],
        vec![a_record(host, ip, TTL)],
    )
}

/// The reply to a query, or None when it is not about us.
fn reply(query: &[u8], host: &str, ip: Ipv4Addr, services: &[Service]) -> Option<(Vec<u8>, bool)> {
    let mut answers = Vec::new();
    let mut additional = Vec::new();
    let mut unicast = false;
    let lower = |v: Vec<String>| v.join(".").to_lowercase();
    for (name, kind, uni) in questions(query) {
        let before = answers.len();
        if name == host && (kind == T_A || kind == T_ANY) {
            answers.push(a_record(host, ip, TTL));
        }
        for s in services {
            if name == lower(kind_name(s)) && (kind == T_PTR || kind == T_ANY) {
                answers.push(ptr_record(s, TTL));
                additional.push(srv_record(s, host, TTL));
                additional.push(txt_record(s, TTL));
                additional.push(a_record(host, ip, TTL));
            }
            if name == lower(instance_name(s)) {
                if kind == T_SRV || kind == T_ANY {
                    answers.push(srv_record(s, host, TTL));
                    additional.push(a_record(host, ip, TTL));
                }
                if kind == T_TXT || kind == T_ANY {
                    answers.push(txt_record(s, TTL));
                }
            }
            if name == SERVICES_LIST && (kind == T_PTR || kind == T_ANY) {
                let mut data = Vec::new();
                encode_labels(&kind_name(s), &mut data);
                answers.push(Record { name: labels(SERVICES_LIST), kind: T_PTR, unique: false, ttl: TTL, data });
            }
        }
        unicast |= uni && answers.len() > before;
    }
    if answers.is_empty() {
        return None;
    }
    // One A record in the additional section is enough.
    let mut seen_a = answers.iter().any(|r| r.kind == T_A);
    additional.retain(|r| {
        if r.kind != T_A {
            return true;
        }
        let keep = !seen_a;
        seen_a = true;
        keep
    });
    Some((packet(&answers, &additional), unicast))
}

pub fn run(name: &str) {
    let host = format!("{}.local", name.trim().to_lowercase());
    let socket = match shared_socket() {
        Ok(s) => s,
        Err(e) => {
            eprintln!("mdns: cannot open port {PORT}: {e}; {host} will not resolve");
            return;
        }
    };
    if let Err(e) = socket.join_multicast_v4(&GROUP, &Ipv4Addr::UNSPECIFIED) {
        eprintln!("mdns: cannot join multicast group: {e}");
        return;
    }
    let _ = socket.set_multicast_ttl_v4(255);
    let _ = socket.set_read_timeout(Some(Duration::from_secs(1)));
    eprintln!("mdns: answering for {host}");
    thread::spawn(move || {
        let group = SocketAddr::V4(SocketAddrV4::new(GROUP, PORT));
        let mut ip = own_ip();
        let mut ip_checked = Instant::now();
        // Announce at start so caches pick up a changed address at once.
        if let Some(addr) = ip {
            for _ in 0..2 {
                let _ = socket.send_to(&packet(&[a_record(&host, addr, TTL)], &[]), group);
                thread::sleep(Duration::from_secs(1));
            }
        }
        // Announcements still due: (when, how many left).
        let mut announce: Option<(Instant, u32)> = None;
        let mut buf = [0u8; 1500];
        loop {
            if ip_checked.elapsed() > Duration::from_secs(30) || ip.is_none() {
                let new = own_ip();
                if new != ip && new.is_some() {
                    announce = Some((Instant::now(), 3));
                }
                ip = new;
                ip_checked = Instant::now();
            }
            if CHANGED.swap(false, Ordering::AcqRel) {
                if let Some(addr) = ip {
                    for s in GOODBYE.lock().unwrap().drain(..) {
                        let (a, _) = service_records(&s, &host, addr, 0);
                        let _ = socket.send_to(&packet(&a, &[]), group);
                    }
                }
                announce = Some((Instant::now(), 3));
            }
            // Three announcements, 1 s and 2 s apart (RFC 6762, 8.3).
            if let (Some((when, left)), Some(addr)) = (announce, ip) {
                if Instant::now() >= when {
                    for s in SERVICES.lock().unwrap().iter() {
                        let (a, b) = service_records(s, &host, addr, TTL);
                        let _ = socket.send_to(&packet(&a, &b), group);
                    }
                    announce = (left > 1).then(|| (when + Duration::from_secs(1 << (3 - left)), left - 1));
                }
            }
            let Ok((n, from)) = socket.recv_from(&mut buf) else { continue };
            let Some(addr) = ip else { continue };
            let services = SERVICES.lock().unwrap().clone();
            let Some((reply, unicast)) = reply(&buf[..n], &host, addr, &services) else { continue };
            let _ = socket.send_to(&reply, group);
            if unicast || from.port() != PORT {
                let _ = socket.send_to(&reply, from);
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn query(name: &str, kind: u16, class: u16) -> Vec<u8> {
        let mut q = vec![0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0];
        encode_name(name, &mut q);
        q.extend_from_slice(&kind.to_be_bytes());
        q.extend_from_slice(&class.to_be_bytes());
        q
    }

    #[test]
    fn answers_only_our_name() {
        assert_eq!(asks_for(&query("soundya.local", 1, 1), "soundya.local"), Some(false));
        assert_eq!(asks_for(&query("SoundYa.local", 255, 0x8001), "soundya.local"), Some(true));
        assert_eq!(asks_for(&query("other.local", 1, 1), "soundya.local"), None);
        assert_eq!(asks_for(&query("soundya.local", 28, 1), "soundya.local"), None); // AAAA
        let mut response = query("soundya.local", 1, 1);
        response[2] = 0x84;
        assert_eq!(asks_for(&response, "soundya.local"), None);
        for n in 0..20 {
            let _ = asks_for(&query("soundya.local", 1, 1)[..n], "soundya.local");
        }
    }

    fn raop() -> Service {
        Service {
            instance: "AABBCCDDEEFF@Станция".into(),
            kind: "_raop._tcp".into(),
            port: 5000,
            txt: vec!["txtvers=1".into(), "ch=2".into()],
        }
    }

    #[test]
    fn answers_service_queries() {
        let ip = Ipv4Addr::new(192, 168, 0, 112);
        let services = [raop()];
        let (p, _) = reply(&query("_raop._tcp.local", 12, 1), "soundya.local", ip, &services).unwrap();
        // 1 answer (PTR), 3 additional (SRV, TXT, A).
        assert_eq!(&p[4..12], &[0, 0, 0, 1, 0, 0, 0, 3]);
        let (target, _) = read_name(&p, 12 + "_raop._tcp.local".len() + 2 + 10).unwrap();
        assert_eq!(target, "aabbccddeeff@станция._raop._tcp.local");
        let (p, uni) = reply(&query("AABBCCDDEEFF@Станция._raop._tcp.local", 33, 0x8001), "soundya.local", ip, &services).unwrap();
        assert!(uni);
        assert_eq!(&p[6..8], &[0, 1]);
        assert!(reply(&query("_googlecast._tcp.local", 12, 1), "soundya.local", ip, &services).is_none());
        assert!(reply(&query("_raop._tcp.local", 12, 1), "soundya.local", ip, &[]).is_none());
        let (p, _) = reply(&query("_services._dns-sd._udp.local", 12, 1), "soundya.local", ip, &services).unwrap();
        assert_eq!(&p[6..8], &[0, 1]);
    }

    #[test]
    fn publish_replaces_same_kind() {
        publish(raop());
        let mut other = raop();
        other.port = 5001;
        publish(other.clone());
        assert_eq!(SERVICES.lock().unwrap().clone(), vec![other]);
        withdraw("_raop._tcp");
        assert!(SERVICES.lock().unwrap().is_empty());
    }

    #[test]
    fn answer_layout() {
        let a = answer("soundya.local", Ipv4Addr::new(192, 168, 0, 112));
        assert_eq!(&a[..12], &[0, 0, 0x84, 0, 0, 0, 0, 1, 0, 0, 0, 0]);
        assert_eq!(&a[a.len() - 4..], &[192, 168, 0, 112]);
        assert_eq!(read_name(&a, 12).unwrap().0, "soundya.local");
    }
}
