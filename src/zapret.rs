//! The original zapret on the speaker: nfqws from zapret's official release
//! (packaging/nfqws), the same program and the same kind of strategies as
//! zapret for Windows.
//!
//! nfqws gets the first packets of HTTPS connections from the kernel (iptables
//! NFQUEUE — the speaker's kernel has it) and sends fake packets the provider's
//! DPI believes and the server ignores, or reorders and cuts the real ones. Only
//! SoundCloud's addresses are touched (`--hostlist-domains`); everything else
//! passes as it was. If soundya stops, nfqws stops with it and the rule lets
//! packets through (`--queue-bypass`), so nothing on the speaker breaks.

use std::{
    os::unix::process::CommandExt,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::Mutex,
    time::Duration,
};

const QUEUE: &str = "200";
const CHAIN: &str = "SOUNDYA_ZAPRET";
const DOMAINS: &str = "soundcloud.com,sndcdn.com,soundcloud.cloud";

/// Strategies tried in turn, the way zapret's blockcheck does (TLS, port 443).
pub const STRATEGIES: [&str; 6] = [
    "--dpi-desync=fake,multisplit --dpi-desync-split-pos=1,midsld --dpi-desync-fooling=md5sig --dpi-desync-repeats=6",
    "--dpi-desync=fake,multidisorder --dpi-desync-split-pos=1,midsld --dpi-desync-fooling=badseq --dpi-desync-repeats=6",
    "--dpi-desync=fake --dpi-desync-fooling=badseq --dpi-desync-repeats=11",
    "--dpi-desync=fake,fakedsplit --dpi-desync-split-pos=midsld --dpi-desync-fooling=ts --dpi-desync-repeats=6",
    "--dpi-desync=multidisorder --dpi-desync-split-pos=1,sniext+1,host+1,midsld-2,midsld,midsld+2,endhost-1",
    "--dpi-desync=fake,fakeddisorder --dpi-desync-split-pos=midsld --dpi-desync-fooling=md5sig,badseq --dpi-desync-repeats=4",
];

/// "nfq3" → strategy index 2.
pub fn index_of(id: &str) -> Option<usize> {
    id.strip_prefix("nfq")?.parse::<usize>().ok().filter(|n| (1..=STRATEGIES.len()).contains(n)).map(|n| n - 1)
}

pub fn id_of(index: usize) -> String {
    format!("nfq{}", index + 1)
}

static RUNNING: Mutex<Option<Child>> = Mutex::new(None);

extern "C" {
    fn prctl(option: i32, arg2: u64, arg3: u64, arg4: u64, arg5: u64) -> i32;
}

fn binary(dir: &Path) -> PathBuf {
    dir.join("nfqws")
}

pub fn installed(dir: &Path) -> bool {
    crate::fsx::exists(&binary(dir))
}

fn iptables(args: &[&str]) -> bool {
    let bin = if crate::fsx::exists(Path::new("/usr/sbin/iptables")) { "/usr/sbin/iptables" } else { "iptables" };
    Command::new(bin)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// Removes soundya's rules (also ones left by a crash).
fn remove_rules() {
    while iptables(&["-t", "mangle", "-D", "POSTROUTING", "-j", CHAIN]) {}
    iptables(&["-t", "mangle", "-F", CHAIN]);
    iptables(&["-t", "mangle", "-X", CHAIN]);
}

/// Sends the first packets of HTTPS connections to nfqws.
fn add_rules() -> Result<(), String> {
    remove_rules();
    if !iptables(&["-t", "mangle", "-N", CHAIN]) {
        return Err("iptables не создал цепочку".into());
    }
    // Only the first few packets of a connection (the TLS hello is there): almost
    // no extra work for the speaker. nfqws marks its own packets: they skip the queue.
    let first_packets = [
        "-t", "mangle", "-A", CHAIN, "-p", "tcp", "--dport", "443", "-m", "connbytes", "--connbytes-dir=original",
        "--connbytes-mode=packets", "--connbytes", "1:6", "-m", "mark", "!", "--mark", "0x40000000/0x40000000", "-j",
        "NFQUEUE", "--queue-num", QUEUE, "--queue-bypass",
    ];
    let all_packets = [
        "-t", "mangle", "-A", CHAIN, "-p", "tcp", "--dport", "443", "-m", "mark", "!", "--mark",
        "0x40000000/0x40000000", "-j", "NFQUEUE", "--queue-num", QUEUE, "--queue-bypass",
    ];
    if !iptables(&first_packets) && !iptables(&all_packets) {
        remove_rules();
        return Err("iptables не принял правило NFQUEUE".into());
    }
    if !iptables(&["-t", "mangle", "-I", "POSTROUTING", "-j", CHAIN]) {
        remove_rules();
        return Err("iptables не подключил цепочку".into());
    }
    Ok(())
}

/// Stops nfqws and removes the rules.
pub fn stop() {
    if let Some(mut c) = RUNNING.lock().unwrap().take() {
        let _ = c.kill();
        let _ = c.wait();
    }
    remove_rules();
}

/// Starts nfqws with strategy `index` (stopping a running one first).
pub fn start(dir: &Path, index: usize) -> Result<(), String> {
    stop();
    let bin = binary(dir);
    if !crate::fsx::exists(&bin) {
        return Err("на колонке нет nfqws: обновите soundya".into());
    }
    let strategy = STRATEGIES.get(index).ok_or("нет такой стратегии")?;
    let mut cmd = Command::new(&bin);
    cmd.arg(format!("--qnum={QUEUE}"))
        .arg(format!("--hostlist-domains={DOMAINS}"))
        .args(strategy.split_whitespace())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    // Dies with soundya.
    unsafe {
        cmd.pre_exec(|| {
            prctl(1 /* PR_SET_PDEATHSIG */, 9, 0, 0, 0);
            Ok(())
        });
    }
    let mut child = cmd.spawn().map_err(|e| format!("nfqws не запустился: {e}"))?;
    if let Some(err) = child.stderr.take() {
        std::thread::spawn(move || {
            use std::io::BufRead;
            for line in std::io::BufReader::new(err).lines().map_while(Result::ok) {
                eprintln!("nfqws: {line}");
            }
        });
    }
    std::thread::sleep(Duration::from_millis(400));
    if let Ok(Some(status)) = child.try_wait() {
        return Err(format!("nfqws сразу остановился ({status})"));
    }
    *RUNNING.lock().unwrap() = Some(child);
    if let Err(e) = add_rules() {
        stop();
        return Err(e);
    }
    eprintln!("zapret: nfqws running, strategy {}: {strategy}", index + 1);
    Ok(())
}

/// At soundya's start: the saved strategy, or a clean slate.
pub fn restore(dir: &Path, bypass: &str) {
    match index_of(bypass) {
        Some(i) => {
            if let Err(e) = start(dir, i) {
                eprintln!("zapret: {e}");
            }
        }
        None => remove_rules(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids() {
        assert_eq!(index_of("nfq1"), Some(0));
        assert_eq!(index_of(&id_of(5)), Some(5));
        assert_eq!(index_of("nfq0"), None);
        assert_eq!(index_of("nfq7"), None);
        assert_eq!(index_of("tlsrec"), None);
    }
}
