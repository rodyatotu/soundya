//! soundya's one zapret rule written straight into the kernel's iptables, for
//! speakers whose firmware has the kernel side (mangle table, NFQUEUE) but no
//! iptables program (Station Mini 1). The same calls iptables-legacy makes:
//! read the mangle table, add or remove the rule, give the table back.
//!
//! The rule is the one zapret.rs adds with iptables:
//! `-A POSTROUTING -p tcp --dport 443 [-m connbytes 1:20 packets original]
//!  -m mark ! --mark 0x40000000/0x40000000 -j NFQUEUE --queue-num N --queue-bypass`,
//! put first in POSTROUTING and found again by its NFQUEUE number.

use std::{ffi::c_void, mem::size_of};

const SOL_IP: i32 = 0;
const AF_INET: i32 = 2;
const SOCK_RAW: i32 = 3;
const IPPROTO_RAW: i32 = 255;
const SO_GET_INFO: i32 = 64;
const SO_GET_ENTRIES: i32 = 65;
const SO_SET_REPLACE: i32 = 64;
const POST_ROUTING: usize = 4;
const HOOKS: usize = 5;

extern "C" {
    fn socket(domain: i32, kind: i32, protocol: i32) -> i32;
    fn close(fd: i32) -> i32;
    fn getsockopt(fd: i32, level: i32, name: i32, val: *mut c_void, len: *mut u32) -> i32;
    // The same signature as in mdns.rs: one declaration per symbol.
    fn setsockopt(fd: i32, level: i32, name: i32, val: *const c_void, len: u32) -> i32;
}

/// struct ipt_getinfo
#[repr(C)]
#[derive(Clone, Copy)]
struct Info {
    name: [u8; 32],
    valid_hooks: u32,
    hook_entry: [u32; HOOKS],
    underflow: [u32; HOOKS],
    num_entries: u32,
    size: u32,
}

/// struct ipt_replace without the entries after it. `counters` is a pointer:
/// 4 bytes on the speaker, 8 here; repr(C) lays it out as the kernel expects
/// from a program built for that machine, the same as iptables.
#[repr(C)]
struct Replace {
    name: [u8; 32],
    valid_hooks: u32,
    num_entries: u32,
    size: u32,
    hook_entry: [u32; HOOKS],
    underflow: [u32; HOOKS],
    num_counters: u32,
    counters: *mut u8,
    _entries: [u64; 0],
}

/// struct ipt_get_entries without the entries.
#[repr(C)]
struct GetEntries {
    name: [u8; 32],
    size: u32,
    _entries: [u64; 0],
}

/// struct ipt_entry (with struct ipt_ip in front).
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Entry {
    src: u32,
    dst: u32,
    smsk: u32,
    dmsk: u32,
    iniface: [u8; 16],
    outiface: [u8; 16],
    iniface_mask: [u8; 16],
    outiface_mask: [u8; 16],
    proto: u16,
    flags: u8,
    invflags: u8,
    nfcache: u32,
    target_offset: u16,
    next_offset: u16,
    comefrom: u32,
    counters: [u64; 2],
}

/// struct xt_tcp
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Tcp {
    spts: [u16; 2],
    dpts: [u16; 2],
    option: u8,
    flg_mask: u8,
    flg_cmp: u8,
    invflags: u8,
}

/// struct xt_connbytes_info
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Connbytes {
    from: u64,
    to: u64,
    what: u8,
    direction: u8,
}

/// struct xt_mark_mtinfo1
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Mark {
    mark: u32,
    mask: u32,
    invert: u8,
}

/// struct xt_NFQ_info_v3 (v2 is the same without `flags`' meaning beyond bypass)
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Nfq {
    queuenum: u16,
    queues_total: u16,
    flags: u16,
}

/// XT_ALIGN: entries, matches and targets start on 8 bytes.
fn align(n: usize) -> usize {
    (n + 7) & !7
}

fn bytes<T: Copy>(v: &T) -> Vec<u8> {
    let p = v as *const T as *const u8;
    unsafe { std::slice::from_raw_parts(p, size_of::<T>()) }.to_vec()
}

fn read<T: Copy + Default>(b: &[u8], at: usize) -> T {
    let mut v = T::default();
    let n = size_of::<T>().min(b.len().saturating_sub(at));
    unsafe { std::ptr::copy_nonoverlapping(b[at..].as_ptr(), &mut v as *mut T as *mut u8, n) };
    v
}

/// A match or target: its 32-byte head (size, name, revision) and data.
fn part<T: Copy>(name: &str, revision: u8, data: &T) -> Vec<u8> {
    let size = align(32 + size_of::<T>());
    let mut out = vec![0u8; size];
    out[0..2].copy_from_slice(&(size as u16).to_ne_bytes());
    out[2..2 + name.len()].copy_from_slice(name.as_bytes());
    out[31] = revision;
    out[32..32 + size_of::<T>()].copy_from_slice(&bytes(data));
    out
}

fn rule(queue: u16, mark: u32, first_packets: bool, nfq_rev: u8) -> Vec<u8> {
    let mut matches = part("tcp", 0, &Tcp { spts: [0, 0xffff], dpts: [443, 443], ..Default::default() });
    if first_packets {
        // packets 1..20, original direction
        matches.extend(part("connbytes", 0, &Connbytes { from: 1, to: 20, what: 0, direction: 0 }));
    }
    matches.extend(part("mark", 1, &Mark { mark, mask: mark, invert: 1 }));
    let target = part("NFQUEUE", nfq_rev, &Nfq { queuenum: queue, queues_total: 1, flags: 1 /* bypass */ });
    let head = size_of::<Entry>();
    let entry = Entry {
        proto: 6,
        target_offset: (head + matches.len()) as u16,
        next_offset: (head + matches.len() + target.len()) as u16,
        ..Default::default()
    };
    let mut out = bytes(&entry);
    out.extend(matches);
    out.extend(target);
    out
}

struct Table {
    fd: i32,
    info: Info,
    entries: Vec<u8>,
}

impl Drop for Table {
    fn drop(&mut self) {
        unsafe { close(self.fd) };
    }
}

fn os_err(what: &str) -> String {
    format!("{what}: {}", std::io::Error::last_os_error())
}

fn open() -> Result<Table, String> {
    let fd = unsafe { socket(AF_INET, SOCK_RAW, IPPROTO_RAW) };
    if fd < 0 {
        return Err(os_err("socket"));
    }
    let mut name = [0u8; 32];
    name[..6].copy_from_slice(b"mangle");
    let mut info = Info { name, valid_hooks: 0, hook_entry: [0; HOOKS], underflow: [0; HOOKS], num_entries: 0, size: 0 };
    let mut len = size_of::<Info>() as u32;
    if unsafe { getsockopt(fd, SOL_IP, SO_GET_INFO, &mut info as *mut Info as *mut c_void, &mut len) } < 0 {
        let e = os_err("таблица mangle");
        unsafe { close(fd) };
        return Err(e);
    }
    let head = size_of::<GetEntries>();
    let mut buf = vec![0u8; head + info.size as usize];
    buf[..32].copy_from_slice(&name);
    buf[32..36].copy_from_slice(&info.size.to_ne_bytes());
    let mut len = buf.len() as u32;
    if unsafe { getsockopt(fd, SOL_IP, SO_GET_ENTRIES, buf.as_mut_ptr() as *mut c_void, &mut len) } < 0 {
        let e = os_err("правила mangle");
        unsafe { close(fd) };
        return Err(e);
    }
    Ok(Table { fd, info, entries: buf[head..].to_vec() })
}

impl Table {
    /// Offsets of the entries, in order.
    fn offsets(&self) -> Vec<usize> {
        let mut out = Vec::new();
        let mut at = 0;
        while at + size_of::<Entry>() <= self.entries.len() {
            out.push(at);
            let e: Entry = read(&self.entries, at);
            if e.next_offset == 0 {
                break;
            }
            at += e.next_offset as usize;
        }
        out
    }

    /// The target name and data offset of the entry at `at`.
    fn target(&self, at: usize) -> (String, usize) {
        let e: Entry = read(&self.entries, at);
        let t = at + e.target_offset as usize;
        let name = &self.entries[t + 2..t + 31];
        let name = String::from_utf8_lossy(&name[..name.iter().position(|&c| c == 0).unwrap_or(name.len())]).into_owned();
        (name, t + 32)
    }

    /// Entries of this table that are soundya's rule for queue `queue`.
    fn ours(&self, queue: u16) -> Vec<usize> {
        self.offsets()
            .into_iter()
            .filter(|&at| {
                let (name, data) = self.target(at);
                name == "NFQUEUE" && u16::from_ne_bytes([self.entries[data], self.entries[data + 1]]) == queue
            })
            .collect()
    }

    /// Puts `len` bytes in at `at` (`add`) or takes the entry at `at` out, and
    /// moves every offset after it: hook starts, chain ends and jumps.
    fn edit(&mut self, at: usize, add: Option<Vec<u8>>, hook_start: bool) {
        let len = match &add {
            Some(b) => b.len() as i64,
            None => -(read::<Entry>(&self.entries, at).next_offset as i64),
        };
        let moved = |off: u32| -> u32 { if off as usize > at || (off as usize == at && !hook_start) { (off as i64 + len) as u32 } else { off } };
        // Jumps (standard targets with an offset) in the old table.
        let offsets = self.offsets();
        for &e in &offsets {
            if add.is_none() && e == at {
                continue;
            }
            let (name, data) = self.target(e);
            if name.is_empty() {
                let v = i32::from_ne_bytes(self.entries[data..data + 4].try_into().unwrap());
                if v >= 0 {
                    let nv = moved(v as u32) as i32;
                    self.entries[data..data + 4].copy_from_slice(&nv.to_ne_bytes());
                }
            }
        }
        for h in 0..HOOKS {
            if self.info.valid_hooks & (1 << h) == 0 {
                continue;
            }
            // A rule put first in a chain becomes the chain's start.
            if !(hook_start && self.info.hook_entry[h] as usize == at) {
                self.info.hook_entry[h] = moved(self.info.hook_entry[h]);
            }
            self.info.underflow[h] = moved(self.info.underflow[h]);
        }
        match add {
            Some(b) => {
                self.entries.splice(at..at, b);
                self.info.num_entries += 1;
            }
            None => {
                self.entries.drain(at..(at as i64 - len) as usize);
                self.info.num_entries -= 1;
            }
        }
        self.info.size = self.entries.len() as u32;
    }

    fn commit(&self, old_entries: u32) -> Result<(), String> {
        let mut counters = vec![0u8; old_entries as usize * 16];
        let head = Replace {
            name: self.info.name,
            valid_hooks: self.info.valid_hooks,
            num_entries: self.info.num_entries,
            size: self.info.size,
            hook_entry: self.info.hook_entry,
            underflow: self.info.underflow,
            num_counters: old_entries,
            counters: counters.as_mut_ptr(),
            _entries: [],
        };
        let mut buf = vec![0u8; size_of::<Replace>()];
        unsafe { std::ptr::copy_nonoverlapping(&head as *const Replace as *const u8, buf.as_mut_ptr(), size_of::<Replace>()) };
        buf.extend_from_slice(&self.entries);
        if unsafe { setsockopt(self.fd, SOL_IP, SO_SET_REPLACE, buf.as_ptr() as *const c_void, buf.len() as u32) } < 0 {
            return Err(os_err("ядро не приняло правило"));
        }
        Ok(())
    }
}

/// The kernel has the mangle table (and soundya may change it).
pub fn available() -> bool {
    open().is_ok()
}

/// Removes soundya's rules for `queue`.
pub fn remove(queue: u16) -> Result<(), String> {
    loop {
        let mut t = open()?;
        let Some(&at) = t.ours(queue).first() else { return Ok(()) };
        let old = t.info.num_entries;
        t.edit(at, None, true);
        t.commit(old)?;
    }
}

/// Puts soundya's rule first in POSTROUTING. Tries the smaller variants an
/// older kernel may lack: without connbytes, NFQUEUE revision 2 for 3.
pub fn add(queue: u16, mark: u32) -> Result<(), String> {
    remove(queue)?;
    let mut last = String::new();
    for (first_packets, rev) in [(true, 3), (true, 2), (false, 3), (false, 2)] {
        let mut t = open()?;
        let old = t.info.num_entries;
        let at = t.info.hook_entry[POST_ROUTING] as usize;
        t.edit(at, Some(rule(queue, mark, first_packets, rev)), true);
        match t.commit(old) {
            Ok(()) => {
                eprintln!("zapret: rule in the kernel (connbytes {first_packets}, NFQUEUE v{rev})");
                return Ok(());
            }
            Err(e) => last = e,
        }
    }
    Err(last)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_match_the_kernel() {
        assert_eq!(size_of::<Info>(), 84);
        assert_eq!(size_of::<Entry>(), 112);
        assert_eq!(size_of::<GetEntries>(), 40);
        assert_eq!(size_of::<Replace>(), if size_of::<usize>() == 8 { 96 } else { 96 });
        assert_eq!(part("tcp", 0, &Tcp::default()).len(), 48);
        assert_eq!(part("connbytes", 0, &Connbytes::default()).len(), 56);
        assert_eq!(part("mark", 1, &Mark::default()).len(), 48);
        assert_eq!(part("NFQUEUE", 3, &Nfq::default()).len(), 40);
    }
}
