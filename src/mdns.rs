//! Answers multicast DNS for `soundya.local`, so the page is reachable by name
//! even when the speaker gets a new IP address. Only A records are answered.
//!
//! The port (5353) may already be used by the firmware's own discovery, so the
//! socket is opened with SO_REUSEADDR/SO_REUSEPORT and shares it.

use std::{
    ffi::{c_int, c_void},
    io,
    net::{Ipv4Addr, SocketAddr, SocketAddrV4, UdpSocket},
    os::fd::FromRawFd,
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

/// Does this query ask for `host` (type A or ANY)? Returns whether a unicast reply was asked for.
fn asks_for(buf: &[u8], host: &str) -> Option<bool> {
    if buf.len() < 12 || buf[2] & 0x80 != 0 {
        return None; // too short, or a response
    }
    let questions = u16::from_be_bytes([buf[4], buf[5]]);
    let mut i = 12;
    for _ in 0..questions {
        let (name, next) = read_name(buf, i)?;
        let kind = u16::from_be_bytes([*buf.get(next)?, *buf.get(next + 1)?]);
        let class = u16::from_be_bytes([*buf.get(next + 2)?, *buf.get(next + 3)?]);
        i = next + 4;
        if name == host && (kind == 1 || kind == 255) {
            return Some(class & 0x8000 != 0);
        }
    }
    None
}

fn answer(host: &str, ip: Ipv4Addr) -> Vec<u8> {
    let mut p = vec![0, 0, 0x84, 0, 0, 0, 0, 1, 0, 0, 0, 0]; // authoritative response, 1 answer
    encode_name(host, &mut p);
    p.extend_from_slice(&[0, 1, 0x80, 1]); // type A, class IN with cache-flush
    p.extend_from_slice(&TTL.to_be_bytes());
    p.extend_from_slice(&[0, 4]);
    p.extend_from_slice(&ip.octets());
    p
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
    let _ = socket.set_read_timeout(Some(Duration::from_secs(30)));
    eprintln!("mdns: answering for {host}");
    thread::spawn(move || {
        let group = SocketAddr::V4(SocketAddrV4::new(GROUP, PORT));
        let mut ip = own_ip();
        let mut ip_checked = Instant::now();
        // Announce at start so caches pick up a changed address at once.
        if let Some(addr) = ip {
            for _ in 0..2 {
                let _ = socket.send_to(&answer(&host, addr), group);
                thread::sleep(Duration::from_secs(1));
            }
        }
        let mut buf = [0u8; 1500];
        loop {
            if ip_checked.elapsed() > Duration::from_secs(30) || ip.is_none() {
                ip = own_ip();
                ip_checked = Instant::now();
            }
            let Ok((n, from)) = socket.recv_from(&mut buf) else { continue };
            let Some(unicast) = asks_for(&buf[..n], &host) else { continue };
            let Some(addr) = ip else { continue };
            let reply = answer(&host, addr);
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

    #[test]
    fn answer_layout() {
        let a = answer("soundya.local", Ipv4Addr::new(192, 168, 0, 112));
        assert_eq!(&a[..12], &[0, 0, 0x84, 0, 0, 0, 0, 1, 0, 0, 0, 0]);
        assert_eq!(&a[a.len() - 4..], &[192, 168, 0, 112]);
        assert_eq!(read_name(&a, 12).unwrap().0, "soundya.local");
    }
}
