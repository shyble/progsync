//! Local time for people (T-46): `2026-11-18 12:21 +03:00`. Stored and JSON
//! times stay UTC; only text output is local.
//!
//! No crate. On Unix the zone is `TZ` if set (a zone name looked up in the
//! zoneinfo directories, a file path, or a POSIX rule like `EST5EDT,M3.2.0,
//! M11.1.0`), else `/etc/localtime`. Zone files (TZif, RFC 8536) give the
//! offset at each transition; past the last one, the rule in the file's
//! footer decides. On Windows the system converts
//! (`SystemTimeToTzSpecificLocalTime`). Anything unreadable falls back to UTC.

use crate::util::{civil_from_days, days_from_civil};
use std::sync::OnceLock;

/// A time zone: offsets east of UTC, in seconds.
#[derive(Clone, Debug, PartialEq)]
pub enum Zone {
    /// From a zone file: transitions (UTC seconds) and the offset from each
    /// on, the offset before the first, and the rule after the last.
    Table { times: Vec<i64>, offsets: Vec<i64>, before: i64, after: Option<Rule> },
    Rule(Rule),
    /// The system's (Windows).
    System,
}

/// A POSIX TZ rule: a standard offset, and maybe a daylight one with when it
/// starts and ends.
#[derive(Clone, Debug, PartialEq)]
pub struct Rule {
    pub std: i64,
    pub dst: Option<(i64, When, When)>,
}

/// A day of the year and a local time of day (seconds, may be negative or
/// past a day).
#[derive(Clone, Debug, PartialEq)]
pub struct When {
    day: Day,
    time: i64,
}

#[derive(Clone, Debug, PartialEq)]
enum Day {
    /// `Mm.w.d`: weekday `d` (0 = Sunday) of week `w` (5 = last) of month `m`.
    Month(u32, u32, u32),
    /// `Jn`: 1..=365, February 29 never counted.
    Julian(u32),
    /// `n`: 0..=365, February 29 counted.
    Ordinal(u32),
}

const UTC: Zone = Zone::Rule(Rule { std: 0, dst: None });

impl Zone {
    /// This machine's zone (read once).
    pub fn local() -> &'static Zone {
        static ZONE: OnceLock<Zone> = OnceLock::new();
        ZONE.get_or_init(|| {
            if cfg!(windows) {
                return Zone::System;
            }
            Zone::from_tz(std::env::var("TZ").ok().as_deref()).unwrap_or(UTC)
        })
    }

    /// The zone a `TZ` value names (`None`: `/etc/localtime`).
    pub fn from_tz(tz: Option<&str>) -> Option<Zone> {
        let Some(tz) = tz else { return Zone::from_file("/etc/localtime") };
        let tz = tz.strip_prefix(':').unwrap_or(tz);
        if tz.is_empty() {
            return Some(UTC);
        }
        if tz.starts_with('/') {
            return Zone::from_file(tz);
        }
        if !tz.contains("..") {
            for dir in ["/usr/share/zoneinfo", "/var/db/timezone/zoneinfo", "/usr/lib/zoneinfo", "/usr/share/lib/zoneinfo"] {
                if let Some(z) = Zone::from_file(&format!("{dir}/{tz}")) {
                    return Some(z);
                }
            }
        }
        Rule::parse(tz).map(Zone::Rule)
    }

    fn from_file(path: &str) -> Option<Zone> {
        Zone::from_tzif(&std::fs::read(path).ok()?)
    }

    /// Parse a TZif file (any version; the 64-bit part when there is one).
    pub fn from_tzif(b: &[u8]) -> Option<Zone> {
        let counts = |at: usize| -> Option<[usize; 6]> {
            if b.get(at..at + 4)? != b"TZif" {
                return None;
            }
            let mut c = [0usize; 6];
            for (i, v) in c.iter_mut().enumerate() {
                let o = at + 20 + i * 4;
                *v = u32::from_be_bytes(b.get(o..o + 4)?.try_into().ok()?) as usize;
            }
            Some(c)
        };
        // isutcnt, isstdcnt, leapcnt, timecnt, typecnt, charcnt
        let [isut, isstd, leap, timecnt, typecnt, chars] = counts(0)?;
        let v1_len = timecnt * 5 + typecnt * 6 + chars + leap * 8 + isstd + isut;
        let (at, size) = if *b.get(4)? >= b'2' { (44 + v1_len, 8) } else { (0, 4) };
        let [isut, isstd, leap, timecnt, typecnt, chars] = counts(at)?;
        if typecnt == 0 {
            return None;
        }
        let mut p = at + 44;
        let mut times = Vec::with_capacity(timecnt);
        for _ in 0..timecnt {
            let s = b.get(p..p + size)?;
            times.push(if size == 8 { i64::from_be_bytes(s.try_into().ok()?) } else { i32::from_be_bytes(s.try_into().ok()?) as i64 });
            p += size;
        }
        let idx = b.get(p..p + timecnt)?.to_vec();
        p += timecnt;
        let mut types = Vec::with_capacity(typecnt);
        for _ in 0..typecnt {
            let t = b.get(p..p + 6)?;
            types.push((i32::from_be_bytes(t[..4].try_into().ok()?) as i64, t[4] != 0));
            p += 6;
        }
        p += chars + leap * (size + 4) + isstd + isut;
        let offsets = idx.iter().map(|&i| types.get(i as usize).map(|t| t.0)).collect::<Option<Vec<_>>>()?;
        // Before the first transition: the first standard-time type.
        let before = types.iter().find(|t| !t.1).unwrap_or(&types[0]).0;
        let after = if size == 8 {
            let footer = b.get(p..)?;
            let s = std::str::from_utf8(footer).ok()?.trim_matches('\n');
            Rule::parse(s)
        } else {
            None
        };
        Some(Zone::Table { times, offsets, before, after })
    }

    /// Seconds east of UTC at `secs` (unix seconds).
    pub fn offset(&self, secs: i64) -> i64 {
        match self {
            Zone::Rule(r) => r.offset(secs),
            Zone::Table { times, offsets, before, after } => {
                let n = times.partition_point(|&t| t <= secs);
                match (n, after) {
                    (0, _) => *before,
                    (n, Some(r)) if n == times.len() => r.offset(secs),
                    (n, _) => offsets[n - 1],
                }
            }
            Zone::System => system_offset(secs),
        }
    }
}

impl Rule {
    /// Parse a POSIX TZ string, e.g. `MSK-3`, `<+03>-3`,
    /// `EST5EDT,M3.2.0,M11.1.0`, `AEST-10AEDT,M10.1.0,M4.1.0/3`.
    pub fn parse(s: &str) -> Option<Rule> {
        let mut p = Parser(s.as_bytes());
        p.name()?;
        let std = -p.offset()?;
        if p.0.is_empty() {
            return Some(Rule { std, dst: None });
        }
        p.name()?;
        let dst_off = if p.0.first().is_some_and(|c| *c != b',') { -p.offset()? } else { std + 3600 };
        let (start, end) = if p.0.is_empty() {
            // No rule given: the US one.
            (When { day: Day::Month(3, 2, 0), time: 7200 }, When { day: Day::Month(11, 1, 0), time: 7200 })
        } else {
            p.eat(b',')?;
            let start = p.when()?;
            p.eat(b',')?;
            (start, p.when()?)
        };
        p.0.is_empty().then_some(Rule { std, dst: Some((dst_off, start, end)) })
    }

    fn offset(&self, secs: i64) -> i64 {
        let Some((dst, start, end)) = &self.dst else { return self.std };
        let (year, _, _) = civil_from_days((secs + self.std).div_euclid(86_400));
        // Start is given in standard time, end in daylight time.
        let on = start.utc(year) - self.std;
        let off = end.utc(year) - dst;
        let in_dst = if on < off { on <= secs && secs < off } else { !(off <= secs && secs < on) };
        if in_dst {
            *dst
        } else {
            self.std
        }
    }
}

impl When {
    /// Unix seconds of this moment in `year`, as if local time were UTC.
    fn utc(&self, year: i64) -> i64 {
        let leap = (year % 4 == 0 && year % 100 != 0) || year % 400 == 0;
        let jan1 = days_from_civil(year, 1, 1);
        let day = match self.day {
            Day::Julian(n) => jan1 + n as i64 - 1 + i64::from(leap && n >= 60),
            Day::Ordinal(n) => jan1 + n as i64,
            Day::Month(m, w, d) => {
                let first = days_from_civil(year, m, 1);
                let len = days_from_civil(year + i64::from(m == 12), m % 12 + 1, 1) - first;
                let weekday = (first + 4).rem_euclid(7); // 1970-01-01 was a Thursday
                let mut day = (d as i64 - weekday).rem_euclid(7) + 7 * (w as i64 - 1);
                while day >= len {
                    day -= 7;
                }
                first + day
            }
        };
        day * 86_400 + self.time
    }
}

struct Parser<'a>(&'a [u8]);

impl Parser<'_> {
    fn eat(&mut self, c: u8) -> Option<()> {
        (self.0.first() == Some(&c)).then(|| self.0 = &self.0[1..])
    }

    /// `ABC` or `<+03>`.
    fn name(&mut self) -> Option<()> {
        if self.eat(b'<').is_some() {
            let end = self.0.iter().position(|&c| c == b'>')?;
            self.0 = &self.0[end + 1..];
            return Some(());
        }
        let n = self.0.iter().take_while(|c| c.is_ascii_alphabetic()).count();
        self.0 = &self.0[n..];
        (n >= 3).then_some(())
    }

    fn number(&mut self) -> Option<i64> {
        let n = self.0.iter().take_while(|c| c.is_ascii_digit()).count();
        let v = std::str::from_utf8(&self.0[..n]).ok()?.parse().ok()?;
        self.0 = &self.0[n..];
        Some(v)
    }

    /// `[+-]hh[:mm[:ss]]` in seconds (POSIX sign: positive is west).
    fn offset(&mut self) -> Option<i64> {
        let sign = if self.eat(b'-').is_some() {
            -1
        } else {
            let _ = self.eat(b'+');
            1
        };
        let mut v = self.number()? * 3600;
        if self.eat(b':').is_some() {
            v += self.number()? * 60;
            if self.eat(b':').is_some() {
                v += self.number()?;
            }
        }
        Some(sign * v)
    }

    fn when(&mut self) -> Option<When> {
        let day = if self.eat(b'M').is_some() {
            let m = self.number()?;
            self.eat(b'.')?;
            let w = self.number()?;
            self.eat(b'.')?;
            let d = self.number()?;
            ((1..=12).contains(&m) && (1..=5).contains(&w) && d <= 6).then_some(Day::Month(m as u32, w as u32, d as u32))?
        } else if self.eat(b'J').is_some() {
            Day::Julian(self.number().filter(|n| (1..=365).contains(n))? as u32)
        } else {
            Day::Ordinal(self.number().filter(|n| *n <= 365)? as u32)
        };
        let time = if self.eat(b'/').is_some() { self.offset()? } else { 7200 };
        Some(When { day, time })
    }
}

/// `ms` (unix milliseconds) in `zone`: `2026-11-18 12:21 +03:00`, with
/// `:ss` when `seconds`.
pub fn format_in(zone: &Zone, ms: i64, seconds: bool) -> String {
    let secs = ms.div_euclid(1000);
    let off = zone.offset(secs);
    let local = secs + off;
    let (y, m, d) = civil_from_days(local.div_euclid(86_400));
    let sod = local.rem_euclid(86_400);
    let ss = if seconds { format!(":{:02}", sod % 60) } else { String::new() };
    let sign = if off < 0 { '-' } else { '+' };
    let a = off.abs();
    format!("{y:04}-{m:02}-{d:02} {:02}:{:02}{ss} {sign}{:02}:{:02}", sod / 3600, sod % 3600 / 60, a / 3600, a % 3600 / 60)
}

/// `ms` in this machine's zone (see [`format_in`]).
pub fn format(ms: i64, seconds: bool) -> String {
    format_in(Zone::local(), ms, seconds)
}

#[cfg(windows)]
fn system_offset(secs: i64) -> i64 {
    #[repr(C)]
    #[derive(Default)]
    struct SystemTime {
        year: u16,
        month: u16,
        weekday: u16,
        day: u16,
        hour: u16,
        minute: u16,
        second: u16,
        millis: u16,
    }
    #[link(name = "kernel32")]
    extern "system" {
        fn SystemTimeToTzSpecificLocalTime(zone: *const std::ffi::c_void, utc: *const SystemTime, local: *mut SystemTime) -> i32;
    }
    let (y, m, d) = civil_from_days(secs.div_euclid(86_400));
    let sod = secs.rem_euclid(86_400);
    let utc = SystemTime {
        year: y as u16,
        month: m as u16,
        day: d as u16,
        hour: (sod / 3600) as u16,
        minute: (sod % 3600 / 60) as u16,
        second: (sod % 60) as u16,
        ..Default::default()
    };
    let mut local = SystemTime::default();
    // SAFETY: both pointers are to valid SYSTEMTIME structs; a null zone
    // means the current one.
    if unsafe { SystemTimeToTzSpecificLocalTime(std::ptr::null(), &utc, &mut local) } == 0 {
        return 0;
    }
    let local_secs = days_from_civil(local.year as i64, local.month as u32, local.day as u32) * 86_400
        + local.hour as i64 * 3600
        + local.minute as i64 * 60
        + local.second as i64;
    local_secs - secs
}

#[cfg(not(windows))]
fn system_offset(_secs: i64) -> i64 {
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(y: i64, m: u32, d: u32, h: i64, min: i64) -> i64 {
        days_from_civil(y, m, d) * 86_400 + h * 3600 + min * 60
    }

    #[test]
    fn posix_rules() {
        let ny = Zone::Rule(Rule::parse("EST5EDT,M3.2.0,M11.1.0").unwrap());
        assert_eq!(ny.offset(at(2026, 1, 15, 12, 0)), -5 * 3600);
        assert_eq!(ny.offset(at(2026, 7, 1, 12, 0)), -4 * 3600);
        // 2026-03-08 02:00 EST is 07:00 UTC; 2026-11-01 02:00 EDT is 06:00 UTC.
        assert_eq!(ny.offset(at(2026, 3, 8, 6, 59)), -5 * 3600);
        assert_eq!(ny.offset(at(2026, 3, 8, 7, 0)), -4 * 3600);
        assert_eq!(ny.offset(at(2026, 11, 1, 5, 59)), -4 * 3600);
        assert_eq!(ny.offset(at(2026, 11, 1, 6, 0)), -5 * 3600);
        assert_eq!(Rule::parse("EST5EDT").unwrap(), Rule::parse("EST5EDT,M3.2.0,M11.1.0").unwrap(), "US rule by default");

        let syd = Zone::Rule(Rule::parse("AEST-10AEDT,M10.1.0,M4.1.0/3").unwrap());
        assert_eq!(syd.offset(at(2026, 1, 15, 0, 0)), 11 * 3600, "southern summer");
        assert_eq!(syd.offset(at(2026, 7, 1, 0, 0)), 10 * 3600);

        assert_eq!(Zone::Rule(Rule::parse("<+0330>-3:30").unwrap()).offset(0), 3 * 3600 + 1800);
        assert_eq!(Rule::parse("MSK-3").unwrap(), Rule { std: 3 * 3600, dst: None });
        for bad in ["", "M", "EST", "EST5EDT,M13.1.0,M11.1.0", "EST5x"] {
            assert!(Rule::parse(bad).is_none(), "{bad}");
        }
    }

    #[test]
    fn formats_with_the_offset() {
        let ms = at(2026, 11, 18, 9, 21) * 1000 + 16_240;
        assert_eq!(format_in(&Zone::Rule(Rule::parse("MSK-3").unwrap()), ms, false), "2026-11-18 12:21 +03:00");
        assert_eq!(format_in(&Zone::Rule(Rule::parse("NST3:30").unwrap()), ms, true), "2026-11-18 05:51:16 -03:30");
        assert_eq!(format_in(&UTC, ms, false), "2026-11-18 09:21 +00:00");
        // Across midnight backwards.
        assert_eq!(format_in(&Zone::Rule(Rule::parse("PST8").unwrap()), at(2026, 1, 1, 3, 0) * 1000, false), "2025-12-31 19:00 -08:00");
    }

    #[test]
    fn tzif_files() {
        // A v2 file: one transition at 1000 to +03:00 (from +02:00), footer MSK-3.
        let mut f = Vec::new();
        let header = |f: &mut Vec<u8>, timecnt: u32, typecnt: u32, chars: u32| {
            f.extend_from_slice(b"TZif2");
            f.extend_from_slice(&[0; 15]);
            for c in [0, 0, 0, timecnt, typecnt, chars] {
                f.extend_from_slice(&c.to_be_bytes());
            }
        };
        header(&mut f, 0, 1, 4); // v1 part: just a type, ignored
        f.extend_from_slice(&[0, 0, 0, 0, 0, 0]);
        f.extend_from_slice(b"UTC\0");
        header(&mut f, 1, 2, 8);
        f.extend_from_slice(&1000i64.to_be_bytes());
        f.push(1);
        f.extend_from_slice(&7200i32.to_be_bytes());
        f.extend_from_slice(&[0, 0]);
        f.extend_from_slice(&10800i32.to_be_bytes());
        f.extend_from_slice(&[0, 4]);
        f.extend_from_slice(b"EET\0MSK\0");
        f.extend_from_slice(b"\nMSK-3\n");
        let z = Zone::from_tzif(&f).unwrap();
        assert_eq!((z.offset(999), z.offset(1000), z.offset(2_000_000_000)), (7200, 10800, 10800));
        assert!(Zone::from_tzif(b"nope").is_none());

        // The system's files, where installed.
        if let Some(ny) = Zone::from_tz(Some("America/New_York")) {
            assert_eq!(ny.offset(at(2026, 7, 1, 12, 0)), -4 * 3600);
            assert_eq!(ny.offset(at(2026, 1, 15, 12, 0)), -5 * 3600);
            assert_eq!(ny.offset(at(2066, 7, 1, 12, 0)), -4 * 3600, "past the table: the footer rule");
            assert_eq!(ny.offset(at(1950, 1, 15, 12, 0)), -5 * 3600);
        }
        assert_eq!(Zone::from_tz(Some("")), Some(UTC));
        assert_eq!(Zone::from_tz(Some("UTC0")), Some(UTC));
    }
}
