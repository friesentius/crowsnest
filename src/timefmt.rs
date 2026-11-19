//! Time helpers without a date-time dependency: ISO-8601 parsing to Unix
//! seconds, local wall-clock formatting through `localtime_r`, and compact
//! durations.

use std::time::{SystemTime, UNIX_EPOCH};

pub fn now_epoch() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Days since 1970-01-01 for a proleptic Gregorian date.
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Parse `YYYY-MM-DDTHH:MM[:SS[.frac]][Z|+HH:MM|-HH:MM]` to Unix seconds.
/// A timestamp with no zone is read as UTC.
pub fn parse_iso(s: &str) -> Option<i64> {
    let s = s.trim().trim_matches('"');
    let b = s.as_bytes();
    if b.len() < 16 || b[4] != b'-' || b[7] != b'-' || (b[10] != b'T' && b[10] != b' ') {
        return None;
    }
    let num = |r: std::ops::Range<usize>| s.get(r)?.parse::<i64>().ok();
    let (y, mo, d, h, mi) = (
        num(0..4)?,
        num(5..7)?,
        num(8..10)?,
        num(11..13)?,
        num(14..16)?,
    );
    let mut rest = &s[16..];
    let mut sec = 0;
    if let Some(r) = rest.strip_prefix(':') {
        sec = r.get(0..2)?.parse().ok()?;
        rest = &r[2..];
        if let Some(r) = rest.strip_prefix('.') {
            let digits = r.bytes().take_while(u8::is_ascii_digit).count();
            rest = &r[digits..];
        }
    }
    let offset = match rest {
        "" | "Z" | "z" => 0,
        zone if zone.len() == 6 && (zone.starts_with('+') || zone.starts_with('-')) => {
            let sign = if zone.starts_with('-') { -1 } else { 1 };
            let oh: i64 = zone.get(1..3)?.parse().ok()?;
            let om: i64 = zone.get(4..6)?.parse().ok()?;
            sign * (oh * 3600 + om * 60)
        }
        _ => return None,
    };
    if !(1..=12).contains(&mo) || !(1..=31).contains(&d) || h > 23 || mi > 59 || sec > 60 {
        return None;
    }
    Some(days_from_civil(y, mo, d) * 86_400 + h * 3600 + mi * 60 + sec - offset)
}

/// Find `until <ISO>` anywhere in a status line, as firstmate workers write it.
pub fn find_until(text: &str) -> Option<i64> {
    until_iso(text).and_then(parse_iso)
}

/// The `<ISO>` text of `until <ISO>` in a status line, if it parses.
pub fn until_iso(text: &str) -> Option<&str> {
    let idx = text.find("until ")?;
    let rest = &text[idx + 6..];
    let end = rest
        .find(|c: char| c.is_whitespace() || c == ';' || c == ',' || c == ')')
        .unwrap_or(rest.len());
    let token = rest[..end].trim_end_matches(':');
    parse_iso(token).map(|_| token)
}

pub struct LocalTime {
    pub hour: i32,
    pub min: i32,
    pub sec: i32,
    pub wday: i32,
    pub yday: i32,
    pub year: i32,
}

/// Local wall-clock fields for a Unix time, honouring `TZ`.
pub fn local(epoch: i64) -> LocalTime {
    // SAFETY: localtime_r only writes into the zeroed `tm` we pass it.
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    let t: libc::time_t = epoch as libc::time_t;
    unsafe {
        libc::localtime_r(&t, &mut tm);
    }
    LocalTime {
        hour: tm.tm_hour,
        min: tm.tm_min,
        sec: tm.tm_sec,
        wday: tm.tm_wday,
        yday: tm.tm_yday,
        year: tm.tm_year,
    }
}

const DAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];

/// `HH:MM:SS` in local time.
pub fn clock(epoch: i64) -> String {
    let t = local(epoch);
    format!("{:02}:{:02}:{:02}", t.hour, t.min, t.sec)
}

/// `HH:MM` when the time falls today, otherwise `Ddd HH:MM`.
pub fn short_when(epoch: i64, now: i64) -> String {
    let t = local(epoch);
    let n = local(now);
    if t.yday == n.yday && t.year == n.year {
        format!("{:02}:{:02}", t.hour, t.min)
    } else {
        format!(
            "{} {:02}:{:02}",
            DAYS[t.wday.clamp(0, 6) as usize],
            t.hour,
            t.min
        )
    }
}

/// Compact age: `45s`, `13m`, `2h05m`, `3d`.
pub fn dur(secs: i64) -> String {
    let s = secs.max(0);
    if s < 60 {
        format!("{s}s")
    } else if s < 3600 {
        format!("{}m", s / 60)
    } else if s < 86_400 {
        let (h, m) = (s / 3600, (s % 3600) / 60);
        if m == 0 {
            format!("{h}h")
        } else {
            format!("{h}h{m:02}m")
        }
    } else {
        format!("{}d", s / 86_400)
    }
}

/// Step duration from milliseconds: `25ms`, `1.5s`, `35s`, `4m52s`.
pub fn dur_ms(ms: u64) -> String {
    if ms < 1000 {
        format!("{ms}ms")
    } else if ms < 10_000 {
        format!("{:.1}s", ms as f64 / 1000.0)
    } else if ms < 60_000 {
        format!("{}s", ms / 1000)
    } else if ms < 3_600_000 {
        format!("{}m{:02}s", ms / 60_000, (ms % 60_000) / 1000)
    } else {
        dur((ms / 1000) as i64)
    }
}

/// Parse a Go-style duration such as `7m19s`, `35m1s`, `1h2m3s` or `6s`.
pub fn parse_go_dur(s: &str) -> Option<i64> {
    let mut total = 0i64;
    let mut num = String::new();
    let mut any = false;
    let mut chars = s.trim().chars().peekable();
    while let Some(c) = chars.next() {
        if c.is_ascii_digit() || c == '.' {
            num.push(c);
            continue;
        }
        let unit = if c == 'm' && chars.peek() == Some(&'s') {
            chars.next();
            0.001
        } else {
            match c {
                'h' => 3600.0,
                'm' => 60.0,
                's' => 1.0,
                _ => return None,
            }
        };
        let v: f64 = num.parse().ok()?;
        total += (v * unit) as i64;
        num.clear();
        any = true;
    }
    if !num.is_empty() || !any {
        return None;
    }
    Some(total)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iso_variants() {
        assert_eq!(parse_iso("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_iso("2026-11-19T13:58:12Z"), Some(1_795_096_692));
        assert_eq!(parse_iso("2026-11-19T14:15Z"), Some(1_795_097_700));
        assert_eq!(
            parse_iso("2026-11-19T15:20:00.071922+00:00"),
            Some(1_795_101_600)
        );
        assert_eq!(parse_iso("2026-11-19T10:20:00-05:00"), Some(1_795_101_600));
        assert_eq!(parse_iso("not a time"), None);
    }

    #[test]
    fn until_in_status_lines() {
        assert_eq!(
            find_until("paused [at=1] until 2026-11-19T14:50Z: fix round"),
            parse_iso("2026-11-19T14:50Z")
        );
        assert_eq!(
            find_until("driving review->CI; resume on next gate until 2026-11-19T14:15Z"),
            parse_iso("2026-11-19T14:15Z")
        );
        assert_eq!(find_until("no wait here"), None);
    }

    #[test]
    fn durations() {
        assert_eq!(dur(45), "45s");
        assert_eq!(dur(13 * 60 + 5), "13m");
        assert_eq!(dur(2 * 3600 + 5 * 60), "2h05m");
        assert_eq!(dur(3 * 86_400 + 5), "3d");
        assert_eq!(dur_ms(25), "25ms");
        assert_eq!(dur_ms(1544), "1.5s");
        assert_eq!(dur_ms(35_279), "35s");
        assert_eq!(dur_ms(291_885), "4m51s");
        assert_eq!(parse_go_dur("7m19s"), Some(439));
        assert_eq!(parse_go_dur("1h2m3s"), Some(3723));
        assert_eq!(parse_go_dur("250ms"), Some(0));
        assert_eq!(parse_go_dur("starting"), None);
    }
}
