//! A small ADB client: runs shell commands on an Android TV / projector over the
//! network (port 5555), the same way `adb connect` + `adb shell` do.
//!
//! The speaker has no adb, so the protocol is spoken here: CNXN, AUTH with an RSA
//! key (made on the speaker on first use, 2048 bits), OPEN "shell:…", WRTE/OKAY/CLSE.
//! The first time the TV shows «Разрешить отладку?» for this key; after «Всегда
//! разрешать» it lets soundya in silently.
//!
//! RSA is done with a tiny Montgomery big-number implementation (no crates):
//! a key takes about a second to make once, a signature a fraction of a second.

use std::{
    io::{self, Read, Write},
    net::{SocketAddr, TcpStream, ToSocketAddrs},
    path::Path,
    time::{Duration, Instant},
};

// ---------------------------------------------------------------- big numbers

/// Unsigned big number, little-endian 32-bit limbs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Big(Vec<u32>);

impl Big {
    fn zero(limbs: usize) -> Self {
        Big(vec![0; limbs])
    }

    fn from_be(bytes: &[u8], limbs: usize) -> Self {
        let mut v = vec![0u32; limbs];
        for (i, b) in bytes.iter().rev().enumerate() {
            if i / 4 < limbs {
                v[i / 4] |= u32::from(*b) << (8 * (i % 4));
            }
        }
        Big(v)
    }

    fn to_be(&self, len: usize) -> Vec<u8> {
        let mut out = vec![0u8; len];
        for i in 0..len {
            let limb = self.0.get(i / 4).copied().unwrap_or(0);
            out[len - 1 - i] = (limb >> (8 * (i % 4))) as u8;
        }
        out
    }

    fn from_hex(s: &str, limbs: usize) -> Option<Self> {
        let s = s.trim();
        if s.is_empty() || !s.chars().all(|c| c.is_ascii_hexdigit()) {
            return None;
        }
        let bytes: Vec<u8> = (0..s.len())
            .rev()
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i.saturating_sub(1)..=i], 16).unwrap_or(0))
            .collect();
        let be: Vec<u8> = bytes.into_iter().rev().collect();
        Some(Big::from_be(&be, limbs))
    }

    fn to_hex(&self) -> String {
        let s: String = self.0.iter().rev().map(|l| format!("{l:08x}")).collect();
        let t = s.trim_start_matches('0');
        if t.is_empty() { "0".into() } else { t.into() }
    }

    fn bits(&self) -> usize {
        for i in (0..self.0.len()).rev() {
            if self.0[i] != 0 {
                return i * 32 + 32 - self.0[i].leading_zeros() as usize;
            }
        }
        0
    }

    fn bit(&self, i: usize) -> bool {
        self.0.get(i / 32).is_some_and(|l| l >> (i % 32) & 1 == 1)
    }

    fn cmp(&self, other: &Big) -> std::cmp::Ordering {
        let n = self.0.len().max(other.0.len());
        for i in (0..n).rev() {
            let (a, b) = (self.0.get(i).copied().unwrap_or(0), other.0.get(i).copied().unwrap_or(0));
            if a != b {
                return a.cmp(&b);
            }
        }
        std::cmp::Ordering::Equal
    }

    /// self -= other (self >= other).
    fn sub(&mut self, other: &Big) {
        let mut borrow = 0i64;
        for i in 0..self.0.len() {
            let d = i64::from(self.0[i]) - i64::from(other.0.get(i).copied().unwrap_or(0)) - borrow;
            self.0[i] = d as u32;
            borrow = i64::from(d < 0);
        }
    }

    fn sub_small(&mut self, x: u32) {
        self.sub(&Big(vec![x]));
    }

    /// self = 2·self mod m (self < m).
    fn double_mod(&mut self, m: &Big) {
        let mut carry = 0u32;
        for l in self.0.iter_mut() {
            let next = *l >> 31;
            *l = (*l << 1) | carry;
            carry = next;
        }
        if carry == 1 || self.cmp(m) != std::cmp::Ordering::Less {
            self.sub(m);
        }
    }

    fn mul(&self, other: &Big) -> Big {
        let mut out = vec![0u32; self.0.len() + other.0.len()];
        for (i, &a) in self.0.iter().enumerate() {
            let mut carry = 0u64;
            for (j, &b) in other.0.iter().enumerate() {
                let t = u64::from(a) * u64::from(b) + u64::from(out[i + j]) + carry;
                out[i + j] = t as u32;
                carry = t >> 32;
            }
            out[i + other.0.len()] = carry as u32;
        }
        Big(out)
    }

    fn mul_small(&self, x: u32) -> Big {
        self.mul(&Big(vec![x]))
    }

    fn add_small(&mut self, x: u32) {
        let mut carry = u64::from(x);
        for l in self.0.iter_mut() {
            let t = u64::from(*l) + carry;
            *l = t as u32;
            carry = t >> 32;
            if carry == 0 {
                break;
            }
        }
    }

    /// (quotient, remainder) of division by a small number.
    fn div_small(&self, x: u32) -> (Big, u32) {
        let mut q = vec![0u32; self.0.len()];
        let mut r = 0u64;
        for i in (0..self.0.len()).rev() {
            let cur = (r << 32) | u64::from(self.0[i]);
            q[i] = (cur / u64::from(x)) as u32;
            r = cur % u64::from(x);
        }
        (Big(q), r as u32)
    }

    fn resize(mut self, limbs: usize) -> Big {
        self.0.resize(limbs, 0);
        self
    }
}

/// Montgomery arithmetic modulo an odd `n`.
struct Mont {
    n: Big,
    n0inv: u32, // −n⁻¹ mod 2³²
    r2: Big,    // R² mod n, R = 2^(32·limbs)
}

impl Mont {
    fn new(n: &Big) -> Self {
        let k = n.0.len();
        // Newton: inv = n⁻¹ mod 2³².
        let n0 = n.0[0];
        let mut inv: u32 = 1;
        for _ in 0..5 {
            inv = inv.wrapping_mul(2u32.wrapping_sub(n0.wrapping_mul(inv)));
        }
        let mut r2 = Big::zero(k);
        r2.0[0] = 1;
        // 1 → 2^(2·32k) mod n by doubling.
        for _ in 0..(64 * k) {
            r2.double_mod(n);
        }
        Mont { n: n.clone(), n0inv: inv.wrapping_neg(), r2 }
    }

    /// a·b·R⁻¹ mod n (CIOS).
    fn mul(&self, a: &Big, b: &Big) -> Big {
        let k = self.n.0.len();
        let mut t = vec![0u32; k + 2];
        for i in 0..k {
            let mut carry = 0u64;
            let ai = u64::from(a.0[i]);
            for j in 0..k {
                let s = u64::from(t[j]) + ai * u64::from(b.0[j]) + carry;
                t[j] = s as u32;
                carry = s >> 32;
            }
            let s = u64::from(t[k]) + carry;
            t[k] = s as u32;
            t[k + 1] = (s >> 32) as u32;
            let m = t[0].wrapping_mul(self.n0inv);
            let mut carry = (u64::from(t[0]) + u64::from(m) * u64::from(self.n.0[0])) >> 32;
            for j in 1..k {
                let s = u64::from(t[j]) + u64::from(m) * u64::from(self.n.0[j]) + carry;
                t[j - 1] = s as u32;
                carry = s >> 32;
            }
            let s = u64::from(t[k]) + carry;
            t[k - 1] = s as u32;
            t[k] = t[k + 1] + (s >> 32) as u32;
        }
        let mut out = Big(t[..k].to_vec());
        if t[k] != 0 || out.cmp(&self.n) != std::cmp::Ordering::Less {
            out.sub(&self.n);
        }
        out
    }

    fn pow(&self, base: &Big, exp: &Big) -> Big {
        let k = self.n.0.len();
        let b = self.mul(&base.clone().resize(k), &self.r2);
        let mut one = Big::zero(k);
        one.0[0] = 1;
        let mut x = self.mul(&one, &self.r2); // 1 in Montgomery form
        for i in (0..exp.bits()).rev() {
            x = self.mul(&x, &x);
            if exp.bit(i) {
                x = self.mul(&x, &b);
            }
        }
        self.mul(&x, &one)
    }
}

// ---------------------------------------------------------------- RSA

const E: u32 = 65_537;
const KEY_LIMBS: usize = 64; // 2048 bits

pub struct Key {
    n: Big,
    d: Big,
}

fn random_bytes(n: usize) -> io::Result<Vec<u8>> {
    let mut b = vec![0u8; n];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut b)?;
    Ok(b)
}

fn small_primes() -> Vec<u32> {
    let mut p = Vec::new();
    let mut n = 3u32;
    while p.len() < 1500 {
        if p.iter().take_while(|&&q| q * q <= n).all(|&q| n % q != 0) {
            p.push(n);
        }
        n += 2;
    }
    p
}

/// Miller–Rabin with `rounds` random bases.
fn probably_prime(n: &Big, rounds: usize) -> io::Result<bool> {
    let mont = Mont::new(n);
    let mut n1 = n.clone();
    n1.sub_small(1);
    let s = (0..n1.bits()).take_while(|&i| !n1.bit(i)).count();
    // d = (n−1) / 2^s
    let mut d = n1.clone();
    for _ in 0..s {
        d = d.div_small(2).0;
    }
    let mut one = Big::zero(n.0.len());
    one.0[0] = 1;
    'rounds: for _ in 0..rounds {
        let mut a = Big::from_be(&random_bytes(n.0.len() * 4 - 1)?, n.0.len());
        a.add_small(2);
        let mut x = mont.pow(&a, &d);
        if x == one || x == n1 {
            continue;
        }
        for _ in 1..s {
            x = mont.pow(&x, &Big(vec![2]));
            if x == n1 {
                continue 'rounds;
            }
        }
        return Ok(false);
    }
    Ok(true)
}

fn random_prime(bits: usize, primes: &[u32]) -> io::Result<Big> {
    let limbs = bits / 32;
    loop {
        let mut p = Big::from_be(&random_bytes(bits / 8)?, limbs);
        p.0[limbs - 1] |= 0xC000_0000; // top two bits: p·q has exactly 2·bits bits
        p.0[0] |= 1;
        // p − 1 must not be divisible by e (so d exists).
        if primes.iter().any(|&q| p.div_small(q).1 == 0) {
            continue;
        }
        let mut p1 = p.clone();
        p1.sub_small(1);
        if p1.div_small(E).1 == 0 {
            continue;
        }
        if probably_prime(&p, 24)? {
            return Ok(p);
        }
    }
}

impl Key {
    pub fn generate() -> io::Result<Key> {
        let primes = small_primes();
        let p = random_prime(1024, &primes)?;
        let q = loop {
            let q = random_prime(1024, &primes)?;
            if q != p {
                break q;
            }
        };
        let n = p.mul(&q).resize(KEY_LIMBS);
        let (mut p1, mut q1) = (p, q);
        p1.sub_small(1);
        q1.sub_small(1);
        let phi = p1.mul(&q1).resize(KEY_LIMBS + 1);
        // d = (1 + k·φ) / e for the k that makes it whole.
        let phi_mod_e = phi.div_small(E).1;
        let k = (1..E).find(|&k| (1 + u64::from(k) * u64::from(phi_mod_e)) % u64::from(E) == 0).unwrap_or(1);
        let mut t = phi.mul_small(k).resize(KEY_LIMBS + 2);
        t.add_small(1);
        let d = t.div_small(E).0.resize(KEY_LIMBS);
        Ok(Key { n, d })
    }

    pub fn load(path: &Path) -> Option<Key> {
        let text = crate::fsx::read_string(path)?;
        let get = |k: &str| text.lines().find_map(|l| l.strip_prefix(k)).and_then(|h| Big::from_hex(h, KEY_LIMBS));
        let key = Key { n: get("n=")?, d: get("d=")? };
        (key.n.bits() == 2048).then_some(key)
    }

    pub fn save(&self, path: &Path) -> io::Result<()> {
        std::fs::write(path, format!("n={}\nd={}\n", self.n.to_hex(), self.d.to_hex()))
    }

    /// The key in a file next to soundya's settings, made on first use.
    pub fn load_or_create(dir: &Path) -> io::Result<Key> {
        let path = dir.join("adbkey");
        if let Some(k) = Key::load(&path) {
            return Ok(k);
        }
        let started = Instant::now();
        let k = Key::generate()?;
        eprintln!("adb: made a new key in {:.1} s", started.elapsed().as_secs_f32());
        k.save(&path)?;
        Ok(k)
    }

    /// What adbd's RSA_verify(SHA1, token) accepts: PKCS#1 v1.5 over the token
    /// as if it were a SHA-1 digest.
    pub fn sign(&self, token: &[u8]) -> Vec<u8> {
        const SHA1_PREFIX: [u8; 15] = [0x30, 0x21, 0x30, 0x09, 0x06, 0x05, 0x2b, 0x0e, 0x03, 0x02, 0x1a, 0x05, 0x00, 0x04, 0x14];
        let mut em = vec![0xffu8; 256];
        em[0] = 0;
        em[1] = 1;
        let tail = SHA1_PREFIX.len() + token.len();
        em[256 - tail - 1] = 0;
        em[256 - tail..256 - token.len()].copy_from_slice(&SHA1_PREFIX);
        em[256 - token.len()..].copy_from_slice(token);
        let m = Big::from_be(&em, KEY_LIMBS);
        Mont::new(&self.n).pow(&m, &self.d).to_be(256)
    }

    /// The public key in Android's format, base64, as `adb` sends it.
    pub fn android_public(&self, name: &str) -> Vec<u8> {
        let mont = Mont::new(&self.n);
        let mut raw = Vec::with_capacity(524);
        raw.extend_from_slice(&(KEY_LIMBS as u32).to_le_bytes());
        raw.extend_from_slice(&mont.n0inv.to_le_bytes());
        for l in &self.n.0 {
            raw.extend_from_slice(&l.to_le_bytes());
        }
        for l in &mont.r2.0 {
            raw.extend_from_slice(&l.to_le_bytes());
        }
        raw.extend_from_slice(&E.to_le_bytes());
        let mut out = crate::pc::base64(&raw).into_bytes();
        out.extend_from_slice(format!(" {name}\0").as_bytes());
        out
    }
}

// ---------------------------------------------------------------- protocol

const CNXN: u32 = 0x4e58_4e43;
const AUTH: u32 = 0x4854_5541;
const OPEN: u32 = 0x4e45_504f;
const OKAY: u32 = 0x5941_4b4f;
const CLSE: u32 = 0x4553_4c43;
const WRTE: u32 = 0x4554_5257;
const STLS: u32 = 0x534c_5453;
const VERSION: u32 = 0x0100_0001;
const MAX_DATA: u32 = 256 * 1024;
const AUTH_TOKEN: u32 = 1;
const AUTH_SIGNATURE: u32 = 2;
const AUTH_RSAPUBLICKEY: u32 = 3;
pub const PORT: u16 = 5555;

#[derive(Debug, PartialEq, Eq)]
pub enum Error {
    /// No answer on the network (TV off, wrong address, network debugging off).
    Unreachable(String),
    /// The TV shows «Разрешить отладку?»: someone has to press «Разрешить».
    NeedsConfirm,
    Other(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Unreachable(e) => write!(f, "не отвечает: {e}"),
            Error::NeedsConfirm => write!(f, "нужно разрешить отладку на экране"),
            Error::Other(e) => write!(f, "{e}"),
        }
    }
}

impl From<io::Error> for Error {
    fn from(e: io::Error) -> Self {
        Error::Other(e.to_string())
    }
}

fn send(s: &mut TcpStream, cmd: u32, a0: u32, a1: u32, data: &[u8]) -> io::Result<()> {
    let sum: u32 = data.iter().map(|&b| u32::from(b)).fold(0, u32::wrapping_add);
    let mut m = Vec::with_capacity(24 + data.len());
    for v in [cmd, a0, a1, data.len() as u32, sum, cmd ^ 0xffff_ffff] {
        m.extend_from_slice(&v.to_le_bytes());
    }
    m.extend_from_slice(data);
    s.write_all(&m)
}

struct Msg {
    cmd: u32,
    a0: u32,
    data: Vec<u8>,
}

fn recv(s: &mut TcpStream) -> io::Result<Msg> {
    let mut h = [0u8; 24];
    s.read_exact(&mut h)?;
    let w = |i: usize| u32::from_le_bytes([h[i], h[i + 1], h[i + 2], h[i + 3]]);
    let (cmd, a0, len, magic) = (w(0), w(4), w(12), w(20));
    if magic != cmd ^ 0xffff_ffff || len > MAX_DATA {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "not an ADB answer"));
    }
    let mut data = vec![0u8; len as usize];
    s.read_exact(&mut data)?;
    Ok(Msg { cmd, a0, data })
}

fn address(host: &str) -> Result<SocketAddr, Error> {
    let target = if host.contains(':') { host.to_owned() } else { format!("{host}:{PORT}") };
    target
        .to_socket_addrs()
        .map_err(|e| Error::Unreachable(e.to_string()))?
        .next()
        .ok_or_else(|| Error::Unreachable(format!("нет адреса {host}")))
}

/// Connects and authenticates. `confirm_wait`: how long to wait for someone to
/// press «Разрешить» on the TV when it does not know the key yet.
fn connect(host: &str, key: &Key, confirm_wait: Duration) -> Result<TcpStream, Error> {
    let addr = address(host)?;
    let mut s = TcpStream::connect_timeout(&addr, Duration::from_secs(3)).map_err(|e| Error::Unreachable(e.to_string()))?;
    s.set_read_timeout(Some(Duration::from_secs(5)))?;
    s.set_nodelay(true)?;
    send(&mut s, CNXN, VERSION, MAX_DATA, b"host::soundya\0")?;
    let mut signed = false;
    let mut offered_key = false;
    loop {
        let m = match recv(&mut s) {
            Ok(m) => m,
            Err(e) if offered_key && matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut) => {
                return Err(Error::NeedsConfirm)
            }
            Err(e) => return Err(Error::Other(format!("связь оборвалась: {e}"))),
        };
        match m.cmd {
            CNXN => {
                s.set_read_timeout(Some(Duration::from_secs(10)))?;
                return Ok(s);
            }
            STLS => return Err(Error::Other("устройство просит беспроводную отладку с кодом; включите обычную «Отладку по сети»".into())),
            AUTH if m.a0 == AUTH_TOKEN && !signed => {
                signed = true;
                send(&mut s, AUTH, AUTH_SIGNATURE, 0, &key.sign(&m.data))?;
            }
            AUTH if m.a0 == AUTH_TOKEN && !offered_key => {
                offered_key = true;
                send(&mut s, AUTH, AUTH_RSAPUBLICKEY, 0, &key.android_public("soundya@yandex-station"))?;
                // The TV now asks the user; the answer (CNXN) comes when they press «Разрешить».
                s.set_read_timeout(Some(confirm_wait.max(Duration::from_secs(1))))?;
            }
            AUTH => return Err(Error::NeedsConfirm),
            _ => {}
        }
    }
}

/// Runs `command` in the TV's shell and returns what it printed.
pub fn shell(host: &str, key: &Key, command: &str, confirm_wait: Duration) -> Result<String, Error> {
    let mut s = connect(host, key, confirm_wait)?;
    let local = 1u32;
    let mut service = format!("shell:{command}").into_bytes();
    service.push(0);
    send(&mut s, OPEN, local, 0, &service)?;
    let mut remote = 0u32;
    let mut out = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(30);
    while Instant::now() < deadline {
        let m = match recv(&mut s) {
            Ok(m) => m,
            Err(e) if matches!(e.kind(), io::ErrorKind::UnexpectedEof) => break,
            Err(e) => return Err(Error::Other(format!("связь оборвалась: {e}"))),
        };
        match m.cmd {
            OKAY => remote = m.a0,
            WRTE => {
                remote = m.a0;
                if out.len() < 256 * 1024 {
                    out.extend_from_slice(&m.data);
                }
                send(&mut s, OKAY, local, remote, &[])?;
            }
            CLSE => {
                let _ = send(&mut s, CLSE, local, m.a0.max(remote), &[]);
                break;
            }
            _ => {}
        }
    }
    Ok(String::from_utf8_lossy(&out).replace("\r\n", "\n"))
}

/// Quotes a string for the TV's shell.
pub fn quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn montgomery_matches_plain_arithmetic() {
        // 2^10 mod 1009 = 15; 7^560 mod 561 = 1 (Carmichael); small odd moduli.
        let n = Big(vec![1009]);
        assert_eq!(Mont::new(&n).pow(&Big(vec![2]), &Big(vec![10])), Big(vec![15]));
        let n = Big(vec![561]);
        assert_eq!(Mont::new(&n).pow(&Big(vec![7]), &Big(vec![560])), Big(vec![1]));
        // Two limbs: 3^65537 mod (2^61 − 1).
        let m: u128 = (1 << 61) - 1;
        let n = Big(vec![m as u32, (m >> 32) as u32]);
        let mut want: u128 = 1;
        for _ in 0..65_537 {
            want = want * 3 % m;
        }
        let got = Mont::new(&n).pow(&Big(vec![3, 0]), &Big(vec![65_537]));
        assert_eq!(u128::from(got.0[0]) | u128::from(got.0[1]) << 32, want);
    }

    #[test]
    fn hex_round_trip() {
        let b = Big::from_hex("1234567890abcdef0fedcba987654321", 8).unwrap();
        assert_eq!(b.to_hex(), "1234567890abcdef0fedcba987654321");
        assert_eq!(Big::from_hex("abc", 2).unwrap().to_hex(), "abc");
    }

    #[test]
    fn quotes_for_the_shell() {
        assert_eq!(quote("дюна"), "'дюна'");
        assert_eq!(quote("it's"), "'it'\\''s'");
    }
}
