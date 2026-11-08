//! «Проверить возможности колонки»: what the speaker's own system has for
//! Bluetooth audio, AirPlay 2 and multiroom / Audio Hub. Only reads (files in
//! /sys and /proc, `command -v`, `shairport-sync -V`); changes nothing,
//! collects no MAC addresses, network names or user data. The result is shown
//! in the panel and goes with a bug report (diagnostics.json) when the user
//! sends it — so support is claimed only for what was seen on real speakers.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

static LAST: Mutex<Option<Value>> = Mutex::new(None);

/// The last result, for the bug report.
pub fn last() -> Option<Value> {
    LAST.lock().unwrap().clone()
}

/// Tests point this at a fake tree (SOUNDYA_PROBE_ROOT=/tmp/x → /tmp/x/proc/…).
fn root() -> PathBuf {
    PathBuf::from(std::env::var("SOUNDYA_PROBE_ROOT").unwrap_or_else(|_| "/".into()))
}

fn at(p: &str) -> PathBuf {
    root().join(p.trim_start_matches('/'))
}

fn read(p: &str) -> Option<String> {
    crate::fsx::read_string(&at(p)).map(|s| s.trim().to_owned())
}

fn names(dir: &str) -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir(at(dir))
        .map(|d| d.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect())
        .unwrap_or_default();
    v.sort();
    v
}

/// A command's output, killed after `secs` (a hung tool must not hang the panel).
fn output(cmd: &str, args: &[&str], secs: u64) -> Option<String> {
    let mut child = Command::new(cmd).args(args).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().ok()?;
    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if start.elapsed() < Duration::from_secs(secs) => std::thread::sleep(Duration::from_millis(50)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
    let out = child.wait_with_output().ok()?;
    let mut s = String::from_utf8_lossy(&out.stdout).into_owned();
    s.push_str(&String::from_utf8_lossy(&out.stderr));
    Some(s.trim().chars().take(400).collect())
}

/// Where a program is: PATH, the firmware's own dirs, soundya's dir.
fn find(prog: &str, dir: &Path) -> Option<String> {
    let mut dirs: Vec<PathBuf> = std::env::var("PATH").unwrap_or_default().split(':').filter(|d| !d.is_empty()).map(PathBuf::from).collect();
    for d in ["/system/bin", "/system/xbin", "/system/vendor/bin", "/vendor/bin", "/usr/bin", "/usr/sbin", "/bin", "/sbin", "/usr/libexec/bluetooth"] {
        dirs.push(PathBuf::from(d));
    }
    dirs.push(dir.to_owned());
    dirs.iter().map(|d| if root() == Path::new("/") { d.join(prog) } else { at(&d.join(prog).to_string_lossy()) }).find(|p| crate::fsx::is_file(p)).map(|p| p.to_string_lossy().into_owned())
}

/// Running programs whose name has one of `words` (names only, no arguments).
fn processes(words: &[&str]) -> Vec<String> {
    let mut out: Vec<String> = names("/proc")
        .iter()
        .filter(|n| n.chars().all(|c| c.is_ascii_digit()))
        .filter_map(|pid| read(&format!("/proc/{pid}/comm")))
        .filter(|c| {
            let l = c.to_lowercase();
            words.iter().any(|w| l.contains(w))
        })
        .collect();
    out.sort();
    out.dedup();
    out
}

/// Ports something listens on (TCP) or holds (UDP), from /proc/net.
fn ports(file: &str, listen_only: bool) -> Vec<u16> {
    let mut v: Vec<u16> = read(file)
        .unwrap_or_default()
        .lines()
        .skip(1)
        .filter_map(|l| {
            let f: Vec<&str> = l.split_whitespace().collect();
            let port = u16::from_str_radix(f.get(1)?.rsplit(':').next()?, 16).ok()?;
            (!listen_only || f.get(3) == Some(&"0A")).then_some(port)
        })
        .collect();
    v.sort_unstable();
    v.dedup();
    v
}

pub fn run(dir: &Path) -> Value {
    // ---- Bluetooth
    let hci: Vec<String> = names("/sys/class/bluetooth").into_iter().filter(|n| n.starts_with("hci")).collect();
    let rfkill: Vec<Value> = names("/sys/class/rfkill")
        .iter()
        .map(|r| {
            json!({
                "type": read(&format!("/sys/class/rfkill/{r}/type")),
                "name": read(&format!("/sys/class/rfkill/{r}/name")),
                "on": read(&format!("/sys/class/rfkill/{r}/state")).map(|s| s == "1"),
            })
        })
        .collect();
    let modules: Vec<String> = read("/proc/modules")
        .unwrap_or_default()
        .lines()
        .filter_map(|l| l.split_whitespace().next())
        .filter(|m| ["bt", "bluetooth", "hci", "rfcomm", "bnep", "hidp", "rtk_btusb", "snd_aloop", "aml_bt"].iter().any(|w| m.contains(w)))
        .map(str::to_owned)
        .collect();
    let bt_tools: Vec<Value> = ["bluetoothd", "bluetoothctl", "hciconfig", "hcitool", "btmgmt", "bluealsa", "bluealsa-aplay", "bt_vendor_test"]
        .iter()
        .filter_map(|p| find(p, dir).map(|path| json!({ "name": p, "path": path })))
        .collect();
    let bt_proc = processes(&["blue", "bt_", "btd", "hci", "a2dp"]);
    let quasar_bt: Vec<String> = names("/system/vendor/quasar").into_iter().filter(|n| n.to_lowercase().contains("blue") || n.to_lowercase().contains("bt")).collect();

    // ---- AirPlay 2
    let shairport = ["shairport-sync", "shairport-sync-ap2"].iter().find_map(|p| find(p, dir));
    let shairport_v = shairport.as_deref().and_then(|p| output(p, &["-V"], 4));
    let nqptp = find("nqptp", dir);
    let udp = ports("/proc/net/udp", false);
    let cpuinfo = read("/proc/cpuinfo").unwrap_or_default();
    let cores = cpuinfo.lines().filter(|l| l.starts_with("processor")).count();
    let hardware = cpuinfo.lines().find(|l| l.starts_with("Hardware") || l.starts_with("model name")).and_then(|l| l.split_once(':')).map(|x| x.1.trim().to_owned());
    let mem = |k: &str| {
        read("/proc/meminfo").unwrap_or_default().lines().find(|l| l.starts_with(k)).and_then(|l| l.split_whitespace().nth(1)?.parse::<u64>().ok()).map(|kb| kb / 1024)
    };

    // ---- Audio Hub / multiroom
    let cards = read("/proc/asound/cards").unwrap_or_default();
    let pcm = read("/proc/asound/pcm").unwrap_or_default();
    let capture: Vec<String> = pcm.lines().filter(|l| l.contains("capture")).map(|l| l.chars().take(80).collect()).collect();
    let multiroom = processes(&["multiroom", "glagol", "mrforwarder", "audiosender", "audioclient", "snapclient", "snapserver"]);
    let tcp = ports("/proc/net/tcp", true);

    let ap2 = shairport_v.as_deref().is_some_and(|v| v.to_lowercase().contains("airplay2"));
    let mut summary: Vec<String> = Vec::new();
    summary.push(if hci.is_empty() {
        "Bluetooth: контроллер в системе не виден (/sys/class/bluetooth пуст) — значит, им управляет сама прошивка Алисы или его нет.".into()
    } else {
        format!("Bluetooth: контроллер есть ({}).", hci.join(", "))
    });
    summary.push(match (bt_tools.iter().any(|t| t["name"] == "bluetoothd"), bt_tools.iter().any(|t| t["name"].as_str().is_some_and(|n| n.starts_with("bluealsa")))) {
        (true, true) => "BlueZ и BlueALSA есть — A2DP можно пробовать.".into(),
        (true, false) => "BlueZ есть, BlueALSA нет — для A2DP её нужно собрать и проверить.".into(),
        _ => "BlueZ (bluetoothd) нет — Bluetooth-звук soundya сделать не может без него.".into(),
    });
    let bt_plugin = crate::bt::plugin(&root());
    summary.push(match &bt_plugin {
        Some(p) => format!("ALSA-модуль BlueALSA есть ({p}) — музыку можно отправить в наушники."),
        None => "ALSA-модуля BlueALSA нет — отправить музыку в наушники soundya не сможет.".into(),
    });
    summary.push(if ap2 {
        format!("AirPlay 2: shairport-sync собран с AirPlay 2; nqptp {}.", if nqptp.is_some() { "есть" } else { "нет — без него AirPlay 2 не работает" })
    } else if shairport.is_some() {
        "AirPlay: работает AirPlay 1 (shairport-sync без AirPlay 2).".into()
    } else {
        "AirPlay: shairport-sync не установлен.".into()
    });
    summary.push(if udp.contains(&319) || udp.contains(&320) {
        "Порты PTP 319/320 заняты — кто-то уже синхронизирует время (нужно для AirPlay 2).".into()
    } else {
        "Порты PTP 319/320 свободны.".into()
    });
    summary.push(if multiroom.is_empty() {
        "Мультирум Алисы: процессов не видно.".into()
    } else {
        format!("Мультирум Алисы: {}.", multiroom.join(", "))
    });
    summary.push(if cards.contains("Loopback") { "ALSA Loopback есть." } else { "ALSA Loopback нет (для Audio Hub нужен модуль snd-aloop)." }.into());

    let v = json!({
        "checked_at": crate::planner::now(),
        "summary": summary,
        "bluetooth": {
            "controllers": hci,
            "rfkill": rfkill,
            "kernel_modules": modules,
            "tools": bt_tools,
            "processes": bt_proc,
            "quasar_files": quasar_bt,
            "alsa_plugin": bt_plugin,
        },
        "airplay2": {
            "shairport": shairport,
            "shairport_version": shairport_v,
            "airplay2_build": ap2,
            "nqptp": nqptp,
            "udp_319_320_busy": udp.contains(&319) || udp.contains(&320),
            "cpu": hardware,
            "cores": cores,
            "mem_total_mb": mem("MemTotal:"),
            "mem_available_mb": mem("MemAvailable:"),
            "kernel": read("/proc/sys/kernel/osrelease"),
        },
        "audio_hub": {
            "cards": cards,
            "capture_pcms": capture,
            "loopback": cards.contains("Loopback"),
            "multiroom_processes": multiroom,
            "tcp_listen": tcp,
        },
    });
    *LAST.lock().unwrap() = Some(v.clone());
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_a_fake_speaker() {
        let base = std::env::temp_dir().join(format!("soundya-probe-{}", std::process::id()));
        let w = |p: &str, t: &str| {
            let f = base.join(p);
            std::fs::create_dir_all(f.parent().unwrap()).unwrap();
            std::fs::write(f, t).unwrap();
        };
        w("sys/class/bluetooth/hci0/uevent", "DEVTYPE=host");
        w("sys/class/rfkill/rfkill0/type", "bluetooth\n");
        w("sys/class/rfkill/rfkill0/name", "bt_dev\n");
        w("sys/class/rfkill/rfkill0/state", "1\n");
        w("proc/modules", "snd_aloop 20480 0 - Live 0x0\nrtk_btusb 61440 0 - Live 0x0\nwifi 1 0 - Live 0x0\n");
        w("proc/1234/comm", "bluetoothd\n");
        w("proc/1300/comm", "multiroomd\n");
        w("proc/1/comm", "init\n");
        w("proc/cpuinfo", "processor\t: 0\nprocessor\t: 1\nprocessor\t: 2\nprocessor\t: 3\nHardware\t: Amlogic\n");
        w("proc/meminfo", "MemTotal:  506000 kB\nMemAvailable: 200000 kB\n");
        w("proc/asound/cards", " 0 [AMLAUGESOUND   ]: AML-AUGESOUND\n 1 [Loopback       ]: Loopback - Loopback\n");
        w("proc/asound/pcm", "00-00: TDM-B : TDM-B : playback 1 : capture 1\n");
        w(
            "proc/net/udp",
            "  sl  local_address rem_address   st\n   0: 00000000:013F 00000000:0000 07\n",
        );
        w(
            "proc/net/tcp",
            "  sl  local_address rem_address   st\n   0: 00000000:223D 00000000:0000 0A\n   1: 0100007F:2693 0100007F:1234 01\n",
        );
        w("usr/sbin/bluetoothd", "");
        std::env::set_var("SOUNDYA_PROBE_ROOT", &base);
        let v = run(&base.join("opt"));
        std::env::remove_var("SOUNDYA_PROBE_ROOT");
        let _ = std::fs::remove_dir_all(&base);
        assert_eq!(v["bluetooth"]["controllers"], json!(["hci0"]));
        assert_eq!(v["bluetooth"]["rfkill"][0]["on"], json!(true));
        assert_eq!(v["bluetooth"]["kernel_modules"], json!(["snd_aloop", "rtk_btusb"]));
        assert_eq!(v["bluetooth"]["processes"], json!(["bluetoothd"]));
        assert_eq!(v["bluetooth"]["tools"][0]["name"], "bluetoothd");
        assert_eq!(v["airplay2"]["cores"], 4);
        assert_eq!(v["airplay2"]["mem_total_mb"], 494);
        assert_eq!(v["airplay2"]["udp_319_320_busy"], true);
        assert_eq!(v["audio_hub"]["loopback"], true);
        assert_eq!(v["audio_hub"]["multiroom_processes"], json!(["multiroomd"]));
        assert_eq!(v["audio_hub"]["tcp_listen"], json!([8765]), "only listening sockets");
        let s = v["summary"].to_string();
        assert!(s.contains("контроллер есть (hci0)") && s.contains("BlueALSA нет") && s.contains("заняты"), "{s}");
        assert!(last().is_some());
    }
}
