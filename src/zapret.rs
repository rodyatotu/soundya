//! The original zapret on the speaker: nfqws from zapret's official release
//! (packaging/nfqws), the same program and the same kind of strategies as
//! zapret for Windows, and nfqws2 from zapret2 (packaging/nfqws2, strategies
//! written in Lua: packaging/lua).
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

/// Strategies, named after zapret for Windows («zapret-discord-youtube»), whose
/// .bat files most people already know (the TLS, port 443, part of each), plus
/// the one from alice-zapret2 made for Yandex Stations.
/// `{FAKE}` is zapret's fake ClientHello of www.google.com (packaging/fake).
/// The ones with `--lua-desync` run in zapret2's nfqws2: there the same hello is
/// the blob `tls_google`. New ones go at the end: ids are positions.
pub const PRESETS: [(&str, &str); 21] = [
    // From github.com/lr1ne/alice-zapret2 (zapret2, checked on a Station Mini 2),
    // the same moves in nfqws v1 terms: a fake with TCP MD5 and seq −10000, then
    // the ClientHello in pieces, back to front.
    ("Станция (alice-zapret2)", "--dpi-desync=fake,multidisorder --dpi-desync-split-pos=1,midsld --dpi-desync-fooling=md5sig,badseq --dpi-desync-badseq-increment=-10000"),
    ("general", "--dpi-desync=multisplit --dpi-desync-split-seqovl=681 --dpi-desync-split-pos=1 --dpi-desync-split-seqovl-pattern={FAKE}"),
    ("general (ALT)", "--dpi-desync=fake,fakedsplit --dpi-desync-repeats=6 --dpi-desync-fooling=ts --dpi-desync-fakedsplit-pattern=0x00 --dpi-desync-fake-tls={FAKE}"),
    ("general (ALT2)", "--dpi-desync=multisplit --dpi-desync-split-seqovl=652 --dpi-desync-split-pos=2 --dpi-desync-split-seqovl-pattern={FAKE}"),
    ("general (ALT3)", "--dpi-desync=fake,hostfakesplit --dpi-desync-fake-tls-mod=rnd,dupsid,sni=www.google.com --dpi-desync-hostfakesplit-mod=host=www.google.com,altorder=1 --dpi-desync-fooling=ts"),
    ("general (ALT4)", "--dpi-desync=fake,multisplit --dpi-desync-repeats=6 --dpi-desync-fooling=badseq --dpi-desync-badseq-increment=1000 --dpi-desync-fake-tls={FAKE}"),
    ("general (ALT5)", "--dpi-desync=fake,fakeddisorder --dpi-desync-split-pos=midsld --dpi-desync-repeats=6 --dpi-desync-fooling=md5sig --dpi-desync-fake-tls={FAKE}"),
    ("general (FAKE TLS AUTO)", "--dpi-desync=fake,multidisorder --dpi-desync-split-pos=1,midsld --dpi-desync-repeats=11 --dpi-desync-fooling=badseq --dpi-desync-fake-tls=0x00000000 --dpi-desync-fake-tls=! --dpi-desync-fake-tls-mod=rnd,rndsni,dupsid"),
    ("general (SIMPLE FAKE)", "--dpi-desync=fake --dpi-desync-repeats=6 --dpi-desync-fooling=ts --dpi-desync-fake-tls={FAKE}"),
    ("fake + multisplit (md5sig)", "--dpi-desync=fake,multisplit --dpi-desync-split-pos=1,midsld --dpi-desync-fooling=md5sig --dpi-desync-repeats=6"),
    ("fake (badseq ×11)", "--dpi-desync=fake --dpi-desync-fooling=badseq --dpi-desync-repeats=11"),
    ("multidisorder", "--dpi-desync=multidisorder --dpi-desync-split-pos=1,sniext+1,host+1,midsld-2,midsld,midsld+2,endhost-1"),
    // zapret2 (github.com/bol-van/zapret2): its config.default for TLS (the one
    // alice-zapret2 runs), then strategies from its blockcheck2.d/standard.
    ("zapret2: по умолчанию", "--payload=tls_client_hello --lua-desync=fake:blob=fake_default_tls:tcp_md5:tcp_seq=-10000 --lua-desync=multidisorder:pos=1,midsld"),
    ("zapret2: fake + multisplit (md5)", "--payload=tls_client_hello --lua-desync=fake:blob=tls_google:tcp_md5:repeats=6 --lua-desync=multisplit:pos=1,midsld"),
    ("zapret2: multisplit seqovl", "--payload=tls_client_hello --lua-desync=multisplit:pos=1:seqovl=681:seqovl_pattern=tls_google"),
    ("zapret2: multidisorder seqovl", "--payload=tls_client_hello --lua-desync=multidisorder:pos=2,midsld:seqovl=1"),
    ("zapret2: tcpseg seqovl", "--payload=tls_client_hello --lua-desync=tcpseg:pos=0,-1:seqovl=#tls_google:seqovl_pattern=tls_google --lua-desync=drop"),
    ("zapret2: fake + fakedsplit (ts)", "--payload=tls_client_hello --lua-desync=fake:blob=tls_google:tcp_ts=-1000:repeats=6 --lua-desync=fakedsplit:pos=1:tcp_ts=-1000"),
    ("zapret2: hostfakesplit", "--payload=tls_client_hello --lua-desync=hostfakesplit:host=www.google.com:tcp_ts=-1000:repeats=2"),
    ("zapret2: fake tls_mod + multidisorder", "--payload=tls_client_hello --lua-desync=fake:blob=0x00000000:tcp_seq=-3000:repeats=6 --lua-desync=fake:blob=tls_google:tcp_seq=-3000:tls_mod=rnd,dupsid:repeats=6 --lua-desync=multidisorder:pos=1,midsld"),
    ("zapret2: fake badsum + multisplit", "--payload=tls_client_hello --lua-desync=fake:blob=tls_google:badsum:repeats=6 --lua-desync=multisplit:pos=1,sniext+1,host+1,midsld"),
];

/// A strategy for zapret2's nfqws2 (Lua), not nfqws.
fn is_lua(args: &[String]) -> bool {
    args.iter().any(|a| a.starts_with("--lua-desync"))
}

/// Which program a strategy id runs in: 2 for nfqws2, 1 for nfqws.
pub fn engine(id: &str, custom: &str) -> u8 {
    let lua = match index_of(id) {
        Some(i) => PRESETS[i].1.contains("--lua-desync"),
        None => custom.contains("--lua-desync"),
    };
    if lua { 2 } else { 1 }
}

/// The program for strategy `id` is on the speaker.
pub fn can_run(dir: &Path, id: &str, custom: &str) -> bool {
    crate::fsx::exists(&binary(dir, engine(id, custom)))
}

/// The id of the user's own arguments (pasted from a .bat).
pub const CUSTOM: &str = "nfqc";

/// "nfq3" → preset index 2.
pub fn index_of(id: &str) -> Option<usize> {
    id.strip_prefix("nfq")?.parse::<usize>().ok().filter(|n| (1..=PRESETS.len()).contains(n)).map(|n| n - 1)
}

pub fn id_of(index: usize) -> String {
    format!("nfq{}", index + 1)
}

/// A zapret strategy id: a preset or the user's own.
pub fn is_zapret(id: &str) -> bool {
    id == CUSTOM || index_of(id).is_some()
}

/// The name to show for an id.
pub fn name_of(id: &str) -> String {
    match index_of(id) {
        Some(i) => PRESETS[i].0.to_owned(),
        None if id == CUSTOM => "свои аргументы".into(),
        None => id.to_owned(),
    }
}

/// Arguments pasted from a zapret .bat for Windows, made fit for nfqws on the
/// speaker: Windows-only and list options dropped (soundya filters by
/// SoundCloud's names itself), `"%BIN%file.bin"` turned into the speaker's copy,
/// and only the first profile kept (up to `--new`).
pub fn from_bat(text: &str, dir: &Path) -> Vec<String> {
    let mut out = Vec::new();
    for raw in text.split_whitespace() {
        let t = raw.replace(['"', '^'], "");
        let t = t.as_str();
        if t == "--new" {
            break;
        }
        if t.is_empty()
            || t.starts_with("--wf-")
            || t.starts_with("--filter-")
            || t.starts_with("--hostlist")
            || t.starts_with("--ipset")
            // zapret2 for Windows: its Lua files are loaded by soundya itself.
            || t.starts_with("--lua-init")
            || !t.starts_with("--")
        {
            continue;
        }
        let t = match t.split_once('=') {
            Some((k, v)) if v.ends_with(".bin") => {
                // zapret2: --blob=name:@file.bin
                let (k, v) = match v.split_once(":@") {
                    Some((name, file)) if k == "--blob" => (format!("{k}={name}:"), file),
                    _ => (format!("{k}="), v),
                };
                let file = v.trim_start_matches('@').replace('\\', "/");
                let name = file.rsplit('/').next().unwrap_or(&file).replace("%BIN%", "");
                // Packs for Windows ship their own fake files (4pda, max.ru…):
                // one the speaker does not have becomes zapret's Google hello,
                // otherwise nfqws stops at once.
                let path = dir.join("fake").join(&name);
                let path = if crate::fsx::exists(&path) {
                    path
                } else {
                    eprintln!("zapret: no fake/{name} on the speaker; using tls_clienthello_www_google_com.bin");
                    dir.join("fake").join("tls_clienthello_www_google_com.bin")
                };
                format!("{k}@{}", path.display())
            }
            // Other Windows variables (%GameFilter%…) mean nothing here.
            _ if t.contains('%') => continue,
            _ => t.to_owned(),
        };
        out.push(t);
    }
    out
}

fn preset_args(index: usize, dir: &Path) -> Vec<String> {
    let fake = format!("@{}", dir.join("fake").join("tls_clienthello_www_google_com.bin").display());
    PRESETS[index].1.split_whitespace().map(|a| a.replace("{FAKE}", &fake)).collect()
}

static RUNNING: Mutex<Option<Child>> = Mutex::new(None);

extern "C" {
    // unsigned long: 32 bits on the speaker. Declared as u64 the arguments
    // landed in the wrong registers and the death signal was never set.
    fn prctl(option: i32, arg2: std::os::raw::c_ulong, arg3: std::os::raw::c_ulong, arg4: std::os::raw::c_ulong, arg5: std::os::raw::c_ulong) -> i32;
}

fn binary(dir: &Path, engine: u8) -> PathBuf {
    dir.join(if engine == 2 { "nfqws2" } else { "nfqws" })
}

/// nfqws or nfqws2 is on the speaker.
pub fn installed(dir: &Path) -> bool {
    (crate::fsx::exists(&binary(dir, 1)) || crate::fsx::exists(&binary(dir, 2))) && has_iptables()
}

/// What nfqws2 needs before a strategy: zapret2's Lua library, Google's hello
/// as the blob `tls_google`, and only SoundCloud's HTTPS.
fn lua_base(dir: &Path) -> Vec<String> {
    let lua = |f: &str| format!("--lua-init=@{}", dir.join("lua").join(f).display());
    vec![
        // nfqws2 drops to uid 0x7FFFFFFF before reading Lua; stay root (it
        // still keeps only the network capabilities) so the files open from
        // any directory the speaker gives soundya.
        "--uid=0:0".into(),
        "--fwmark=0x40000000".into(),
        lua("zapret-lib.lua"),
        lua("zapret-antidpi.lua"),
        lua("zapret-auto.lua"),
        format!("--blob=tls_google:@{}", dir.join("fake").join("tls_clienthello_www_google_com.bin").display()),
        "--filter-tcp=443".into(),
    ]
}

/// Where iptables is on this speaker: the program and what goes before the
/// arguments («xtables-multi iptables …» when there is no iptables link).
/// Firmwares put it in different places, and soundya started by Luna or an
/// init script may have a short PATH.
fn iptables_bin() -> Option<(String, Option<&'static str>)> {
    static FOUND: std::sync::OnceLock<Option<(String, Option<&'static str>)>> = std::sync::OnceLock::new();
    FOUND
        .get_or_init(|| {
            let mut dirs: Vec<String> = ["/usr/sbin", "/sbin", "/usr/bin", "/bin", "/system/bin", "/system/xbin", "/system/workdir/bin", "/usr/local/sbin", "/usr/local/bin"]
                .iter()
                .map(|d| d.to_string())
                .collect();
            if let Ok(path) = std::env::var("PATH") {
                dirs.extend(path.split(':').filter(|d| !d.is_empty()).map(str::to_owned));
            }
            for (name, pre) in [("iptables", None), ("iptables-legacy", None), ("xtables-legacy-multi", Some("iptables")), ("xtables-multi", Some("iptables"))] {
                for d in &dirs {
                    let p = format!("{d}/{name}");
                    if crate::fsx::exists(Path::new(&p)) {
                        eprintln!("zapret: iptables: {p}");
                        return Some((p, pre));
                    }
                }
            }
            None
        })
        .clone()
}

/// iptables is on the speaker (nfqws gets nothing without it).
pub fn has_iptables() -> bool {
    iptables_bin().is_some()
}

/// Runs iptables; Err carries what it said. `-w`: waits for the xtables lock
/// instead of failing when the firmware changes its rules at the same moment.
fn iptables_run(args: &[&str]) -> Result<(), String> {
    let Some((bin, pre)) = iptables_bin() else {
        return Err("на колонке не нашёлся iptables".into());
    };
    let bin = bin.as_str();
    let run = |wait: bool| {
        let mut c = Command::new(bin);
        c.args(pre);
        if wait {
            c.arg("-w");
        }
        c.args(args).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::piped()).output()
    };
    let out = run(true).map_err(|e| format!("{bin}: {e}"))?;
    if out.status.success() {
        return Ok(());
    }
    let said = String::from_utf8_lossy(&out.stderr).trim().to_owned();
    // An old iptables without -w.
    if said.contains("unrecognized option") || said.contains("invalid option") {
        let out = run(false).map_err(|e| format!("{bin}: {e}"))?;
        if out.status.success() {
            return Ok(());
        }
        return Err(String::from_utf8_lossy(&out.stderr).trim().to_owned());
    }
    Err(said)
}

fn iptables(args: &[&str]) -> bool {
    iptables_run(args).is_ok()
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
    if let Err(e) = iptables_run(&["-t", "mangle", "-N", CHAIN]) {
        // Left over and still in use: empty it and go on.
        if !(e.contains("exists") && iptables(&["-t", "mangle", "-F", CHAIN])) {
            eprintln!("zapret: iptables -N {CHAIN}: {e}");
            return Err(format!("iptables не создал цепочку: {}", if e.is_empty() { "без объяснений" } else { e.as_str() }));
        }
    }
    // Only the first few packets of a connection (the TLS hello is there): almost
    // no extra work for the speaker. nfqws marks its own packets: they skip the queue.
    let first_packets = [
        "-t", "mangle", "-A", CHAIN, "-p", "tcp", "--dport", "443", "-m", "connbytes", "--connbytes-dir=original",
        "--connbytes-mode=packets", "--connbytes", "1:20", "-m", "mark", "!", "--mark", "0x40000000/0x40000000", "-j",
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

/// Starts nfqws with strategy `id` ("nfq3", or [`CUSTOM`] with `custom` args),
/// stopping a running one first.
pub fn start(dir: &Path, id: &str, custom: &str) -> Result<(), String> {
    stop();
    let args = match index_of(id) {
        Some(i) => preset_args(i, dir),
        None if id == CUSTOM => from_bat(custom, dir),
        None => return Err("нет такой стратегии".into()),
    };
    let lua = is_lua(&args);
    let (bin, prog) = if lua { (binary(dir, 2), "nfqws2") } else { (binary(dir, 1), "nfqws") };
    if !crate::fsx::exists(&bin) {
        return Err(format!("на колонке нет {prog}: обновите soundya"));
    }
    if !has_iptables() {
        return Err("на колонке не нашёлся iptables: без него zapret не работает, остаются встроенные стратегии".into());
    }
    if !lua && !args.iter().any(|a| a.starts_with("--dpi-desync")) {
        return Err("в аргументах нет --dpi-desync или --lua-desync: вставьте строку из .bat целиком".into());
    }
    let mut cmd = Command::new(&bin);
    cmd.arg(format!("--qnum={QUEUE}"));
    if lua {
        cmd.args(lua_base(dir));
    }
    cmd.arg(format!("--hostlist-domains={DOMAINS}"))
        .args(&args)
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
    let mut child = cmd.spawn().map_err(|e| format!("{prog} не запустился: {e}"))?;
    // Its last words, for the page when it stops at once.
    let said = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
    if let Some(err) = child.stderr.take() {
        let said = said.clone();
        std::thread::spawn(move || {
            use std::io::BufRead;
            for line in std::io::BufReader::new(err).lines().map_while(Result::ok) {
                eprintln!("{prog}: {line}");
                *said.lock().unwrap() = line;
            }
        });
    }
    std::thread::sleep(Duration::from_millis(400));
    if let Ok(Some(status)) = child.try_wait() {
        std::thread::sleep(Duration::from_millis(100));
        let last = said.lock().unwrap().clone();
        return Err(if last.is_empty() {
            format!("{prog} сразу остановился ({status}): проверьте аргументы")
        } else {
            format!("{prog} не принял аргументы: {last}")
        });
    }
    *RUNNING.lock().unwrap() = Some(child);
    if let Err(e) = add_rules() {
        stop();
        return Err(e);
    }
    eprintln!("zapret: {prog} running, {}: {}", name_of(id), args.join(" "));
    Ok(())
}

/// At soundya's start: the saved strategy, or a clean slate.
pub fn restore(dir: &Path, bypass: &str, custom: &str) {
    if is_zapret(bypass) {
        if let Err(e) = start(dir, bypass, custom) {
            eprintln!("zapret: {e}");
        }
    } else {
        stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_a_windows_bat_line() {
        let bat = r#"--wf-tcp=80,443 --wf-udp=443,50000-50100 ^
--filter-tcp=443 --hostlist="%LISTS%list-general.txt" --dpi-desync=fake,fakedsplit --dpi-desync-repeats=6 --dpi-desync-fooling=ts --dpi-desync-fakedsplit-pattern=0x00 --dpi-desync-fake-tls="%BIN%tls_clienthello_www_google_com.bin" --new ^
--filter-udp=443 --dpi-desync=fake"#;
        let args = from_bat(bat, Path::new("/data/opt/soundya"));
        assert_eq!(
            args,
            vec![
                "--dpi-desync=fake,fakedsplit",
                "--dpi-desync-repeats=6",
                "--dpi-desync-fooling=ts",
                "--dpi-desync-fakedsplit-pattern=0x00",
                "--dpi-desync-fake-tls=@/data/opt/soundya/fake/tls_clienthello_www_google_com.bin",
            ]
        );
    }

    #[test]
    fn reads_a_zapret2_bat_line() {
        let bat = r#"--wf-tcp=443 --lua-init=@"%LUA%zapret-lib.lua" --filter-tcp=443 --hostlist="%LISTS%list.txt" --blob=g:@"%BIN%tls_clienthello_www_google_com.bin" --payload=tls_client_hello --lua-desync=multisplit:pos=1:seqovl=681:seqovl_pattern=g --new"#;
        let args = from_bat(bat, Path::new("/d"));
        assert_eq!(
            args,
            vec![
                "--blob=g:@/d/fake/tls_clienthello_www_google_com.bin",
                "--payload=tls_client_hello",
                "--lua-desync=multisplit:pos=1:seqovl=681:seqovl_pattern=g",
            ]
        );
        assert!(is_lua(&args));
    }

    #[test]
    fn presets_use_the_fake_file() {
        let a = preset_args(2, Path::new("/d"));
        assert!(a.contains(&"--dpi-desync-fake-tls=@/d/fake/tls_clienthello_www_google_com.bin".to_owned()));
        assert!(PRESETS.iter().all(|(_, a)| a.contains("--dpi-desync=") || a.contains("--lua-desync=")));
    }

    #[test]
    fn ids() {
        assert_eq!(index_of("nfq1"), Some(0));
        assert_eq!(index_of(&id_of(5)), Some(5));
        assert_eq!(index_of("nfq0"), None);
        assert_eq!(index_of("nfq22"), None);
        assert_eq!(engine("nfq1", ""), 1);
        assert_eq!(engine("nfq13", ""), 2);
        assert_eq!(engine(CUSTOM, "--lua-desync=multisplit"), 2);
        assert!(is_zapret(CUSTOM) && is_zapret("nfq2") && !is_zapret("tlsrec"));
        assert_eq!(name_of("nfq3"), "general (ALT)");
        assert_eq!(index_of("tlsrec"), None);
    }
}
