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
        let mut last = io::Error::other("no DNS servers");
        for server in Self::servers() {
            match query(host, &server) {
                Ok(ips) if !ips.is_empty() => {
                    self.cache.lock().unwrap().insert(host.to_owned(), (ips.clone(), Instant::now()));
                    return Ok(ips);
                }
                Ok(_) => last = io::Error::other(format!("{host}: no A records from {server}")),
                Err(e) => last = e,
            }
        }
        Err(last)
    }
}

impl ureq::Resolver for Resolver {
    fn resolve(&self, netloc: &str) -> io::Result<Vec<SocketAddr>> {
        // System resolver first (fails fast when 127.0.0.1:53 refuses).
        if let Ok(addrs) = netloc.to_socket_addrs() {
            let v: Vec<SocketAddr> = addrs.collect();
            if !v.is_empty() {
                return Ok(v);
            }
        }
        let (host, port) = netloc
            .rsplit_once(':')
            .ok_or_else(|| io::Error::other(format!("bad address {netloc}")))?;
        let port: u16 = port.parse().map_err(io::Error::other)?;
        let host = host.trim_start_matches('[').trim_end_matches(']');
        Ok(self.lookup(host)?.into_iter().map(|ip| SocketAddr::new(ip, port)).collect())
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
    let addr: SocketAddr = format!("{server}:53").parse().map_err(io::Error::other)?;
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
}
