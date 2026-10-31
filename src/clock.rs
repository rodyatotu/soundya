//! Keeps the speaker's clock right.
//!
//! A rooted speaker with updates switched off can be days behind (seen: 40 hours),
//! and then every HTTPS site whose certificate is newer than the speaker's date is
//! refused («certificate not valid yet») — GitHub updates first of all. The time
//! comes from the `Date` header of plain-HTTP answers of big sites (no TLS needed
//! to read it), and the clock is set when it is off by more than two minutes.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

const SOURCES: [&str; 4] = [
    "http://www.google.com/generate_204",
    "http://ya.ru/",
    "http://github.com/",
    "http://www.apple.com/",
];
const MAX_SKEW: i64 = 120;

#[repr(C)]
struct Timespec {
    sec: std::ffi::c_long,
    nsec: std::ffi::c_long,
}

extern "C" {
    fn clock_settime(clock: std::ffi::c_int, tp: *const Timespec) -> std::ffi::c_int;
}
const CLOCK_REALTIME: std::ffi::c_int = 0;

/// "Sat, 31 Oct 2026 12:52:58 GMT" → seconds since 1970.
pub fn parse_http_date(s: &str) -> Option<i64> {
    let parts: Vec<&str> = s.split_whitespace().collect();
    let [_, day, month, year, time, ..] = parts.as_slice() else { return None };
    let day: i64 = day.parse().ok()?;
    let year: i64 = year.parse().ok()?;
    let month = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"]
        .iter()
        .position(|m| m.eq_ignore_ascii_case(month))? as i64
        + 1;
    let mut hms = time.split(':').map(|x| x.parse::<i64>());
    let (h, m, sec) = (hms.next()?.ok()?, hms.next()?.ok()?, hms.next()?.ok()?);
    // Days from civil (Howard Hinnant's algorithm).
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some(days * 86_400 + h * 3600 + m * 60 + sec)
}

fn now() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

/// The time according to the web, from the first site that answers.
fn web_time(proxy: &str) -> Option<i64> {
    let direct = ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(6))
        .redirects(0)
        .resolver(crate::dns::Resolver::new())
        .build();
    let agents = std::iter::once(direct).chain(crate::sc::build_agent(proxy).ok().filter(|_| !proxy.is_empty()));
    for agent in agents {
        for url in SOURCES {
            let resp = match agent.get(url).call() {
                Ok(r) => r,
                Err(ureq::Error::Status(_, r)) => r, // a redirect or 404 still carries the date
                Err(_) => continue,
            };
            if let Some(t) = resp.header("Date").and_then(parse_http_date) {
                return Some(t);
            }
        }
    }
    None
}

/// Sets the clock if it is off. Returns the correction in seconds (0: was right).
pub fn fix(proxy: &str) -> Result<i64, String> {
    let web = web_time(proxy).ok_or("no site answered with the time")?;
    let skew = web - now();
    if skew.abs() <= MAX_SKEW {
        return Ok(0);
    }
    let ts = Timespec { sec: web as std::ffi::c_long, nsec: 0 };
    if unsafe { clock_settime(CLOCK_REALTIME, &ts) } != 0 {
        return Err(format!("clock is off by {skew} s but cannot be set: {}", std::io::Error::last_os_error()));
    }
    eprintln!("clock: was off by {skew} s; set from the web");
    Ok(skew)
}

/// «certificate not valid yet / expired»: the speaker's clock, not the site.
pub fn is_clock_error(e: &str) -> bool {
    let e = e.to_lowercase();
    e.contains("not valid yet") || e.contains("notvalidyet") || e.contains("expired")
}

/// Checks the clock soon after start and then twice a day.
pub fn run(shared: std::sync::Arc<crate::Shared>) {
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_secs(40));
        loop {
            let proxy = shared.proxy.lock().unwrap().clone();
            match fix(&proxy) {
                Ok(_) => std::thread::sleep(Duration::from_secs(12 * 3600)),
                Err(e) => {
                    eprintln!("clock: {e}");
                    std::thread::sleep(Duration::from_secs(600));
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_http_dates() {
        assert_eq!(parse_http_date("Sat, 31 Oct 2026 12:52:58 GMT"), Some(1_793_451_178));
        assert_eq!(parse_http_date("Thu, 01 Jan 1970 00:00:00 GMT"), Some(0));
        assert_eq!(parse_http_date("Tue, 29 Feb 2028 23:59:59 GMT"), Some(1_835_481_599));
        assert_eq!(parse_http_date("garbage"), None);
    }

    #[test]
    fn recognises_clock_errors() {
        assert!(is_clock_error(
            "tls connection init failed: invalid peer certificate: certificate not valid yet: verification time 1"
        ));
        assert!(!is_clock_error("Connection refused"));
    }
}
