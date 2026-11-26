//! Time helpers: ns since the Unix epoch, UTC civil dates, RFC 3339 and KQL timespan text.

/// Days since 1970-01-01 for a civil date (Howard Hinnant's algorithm).
pub fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

pub fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}

pub const NS: i64 = 1_000_000_000;

/// `2026-11-26T06:34:41.123Z` style, with as many fractional digits as needed (0, 3, 6 or 9).
pub fn format_rfc3339(ns: i64) -> String {
    let secs = ns.div_euclid(NS);
    let frac = ns.rem_euclid(NS);
    let (y, mo, d) = civil_from_days(secs.div_euclid(86400));
    let sod = secs.rem_euclid(86400);
    let base = format!("{y:04}-{mo:02}-{d:02}T{:02}:{:02}:{:02}", sod / 3600, sod / 60 % 60, sod % 60);
    if frac == 0 {
        format!("{base}Z")
    } else if frac % 1_000_000 == 0 {
        format!("{base}.{:03}Z", frac / 1_000_000)
    } else if frac % 1000 == 0 {
        format!("{base}.{:06}Z", frac / 1000)
    } else {
        format!("{base}.{frac:09}Z")
    }
}

/// Parses RFC 3339 (`T` or space, optional fraction, `Z` or `+hh:mm`, no zone means UTC), or a bare date.
pub fn parse_datetime(s: &str) -> Option<i64> {
    let s = s.trim();
    let b = s.as_bytes();
    let n = |r: std::ops::Range<usize>| -> Option<i64> {
        let t = s.get(r)?;
        if t.bytes().all(|c| c.is_ascii_digit()) { t.parse().ok() } else { None }
    };
    if b.len() < 10 || b[4] != b'-' || b[7] != b'-' {
        return None;
    }
    let (y, mo, d) = (n(0..4)?, n(5..7)?, n(8..10)?);
    if !(1..=12).contains(&mo) || !(1..=31).contains(&d) {
        return None;
    }
    let days = days_from_civil(y, mo, d);
    if b.len() == 10 {
        return Some(days * 86400 * NS);
    }
    if b[10] != b'T' && b[10] != b't' && b[10] != b' ' {
        return None;
    }
    let (hh, mm) = (n(11..13)?, n(14..16)?);
    if b.get(13) != Some(&b':') {
        return None;
    }
    let mut o = 16;
    let mut ss = 0;
    if b.get(16) == Some(&b':') {
        ss = n(17..19)?;
        o = 19;
    }
    let mut frac = 0i64;
    if b.get(o) == Some(&b'.') || b.get(o) == Some(&b',') {
        let start = o + 1;
        let mut end = start;
        while end < b.len() && b[end].is_ascii_digit() {
            end += 1;
        }
        let digits = &s[start..end];
        if digits.is_empty() {
            return None;
        }
        let digits = &digits[..digits.len().min(9)];
        frac = digits.parse::<i64>().ok()? * 10i64.pow(9 - digits.len() as u32);
        o = end;
    }
    let rest = s[o..].trim();
    let offset = match rest {
        "" | "Z" | "z" => 0,
        r if r.starts_with('+') || r.starts_with('-') => {
            let sign = if r.starts_with('-') { -1 } else { 1 };
            let r = r[1..].replace(':', "");
            if r.len() != 4 {
                return None;
            }
            let h: i64 = r[0..2].parse().ok()?;
            let m: i64 = r[2..4].parse().ok()?;
            sign * (h * 3600 + m * 60)
        }
        _ => return None,
    };
    Some((days * 86400 + hh * 3600 + mm * 60 + ss - offset) * NS + frac)
}

/// KQL-style timespan text: `[d.]hh:mm:ss[.fffffff]`.
pub fn format_duration(ns: i64) -> String {
    let neg = ns < 0;
    let a = ns.unsigned_abs() as i128;
    let total_s = a / NS as i128;
    let frac = a % NS as i128;
    let (d, h, m, s) = (total_s / 86400, total_s / 3600 % 24, total_s / 60 % 60, total_s % 60);
    let mut out = String::new();
    if neg {
        out.push('-');
    }
    if d > 0 {
        out.push_str(&format!("{d}."));
    }
    out.push_str(&format!("{h:02}:{m:02}:{s:02}"));
    if frac > 0 {
        out.push_str(&format!(".{:07}", frac / 100));
    }
    out
}

/// Timespan literal: `5m`, `1.5h`, `2d`, `100ms`, `10s`, `3us`, `7ns`, `1tick`, or `[d.]hh:mm[:ss[.f]]`.
pub fn parse_duration(s: &str) -> Option<i64> {
    let s = s.trim();
    if s.contains(':') {
        let (neg, s) = s.strip_prefix('-').map(|r| (true, r)).unwrap_or((false, s));
        let (days, rest) = match s.split_once('.') {
            Some((d, r)) if r.contains(':') && !d.contains(':') => (d.parse::<i64>().ok()?, r),
            _ => (0, s),
        };
        let parts: Vec<&str> = rest.split(':').collect();
        if parts.len() < 2 || parts.len() > 3 {
            return None;
        }
        let h: i64 = parts[0].parse().ok()?;
        let m: i64 = parts[1].parse().ok()?;
        let secs: f64 = parts.get(2).map(|x| x.parse().ok()).unwrap_or(Some(0.0))?;
        let v = (days * 86400 + h * 3600 + m * 60) * NS + (secs * 1e9).round() as i64;
        return Some(if neg { -v } else { v });
    }
    let split = s.find(|c: char| c.is_ascii_alphabetic())?;
    let (num, unit) = s.split_at(split);
    let x: f64 = num.trim().parse().ok()?;
    let mul = match unit.to_ascii_lowercase().as_str() {
        "d" | "day" | "days" => 86400.0 * 1e9,
        "h" | "hr" | "hrs" | "hour" | "hours" => 3600.0 * 1e9,
        "m" | "min" | "minute" | "minutes" => 60.0 * 1e9,
        "s" | "sec" | "second" | "seconds" => 1e9,
        "ms" | "milli" | "millis" | "millisecond" | "milliseconds" => 1e6,
        "us" | "microsecond" | "microseconds" => 1e3,
        "ns" | "nanosecond" | "nanoseconds" => 1.0,
        "tick" | "ticks" => 100.0,
        _ => return None,
    };
    Some((x * mul).round() as i64)
}

pub fn now_ns() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos() as i64).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn datetime_round_trip() {
        for s in ["1970-01-01T00:00:00Z", "2026-11-26T06:34:41.123Z", "2000-02-29T23:59:59.000001Z", "1969-12-31T23:59:59.999999999Z"] {
            assert_eq!(format_rfc3339(parse_datetime(s).unwrap()), s);
        }
        assert_eq!(parse_datetime("2026-11-26 06:34:41"), parse_datetime("2026-11-26T06:34:41Z"));
        assert_eq!(parse_datetime("2026-11-26T00:34:41-06:00"), parse_datetime("2026-11-26T06:34:41Z"));
        assert_eq!(parse_datetime("2026-11-26"), Some(days_from_civil(2026, 11, 26) * 86400 * NS));
        assert!(parse_datetime("nope").is_none());
    }

    #[test]
    fn durations() {
        assert_eq!(parse_duration("5m"), Some(300 * NS));
        assert_eq!(parse_duration("1.5h"), Some(5400 * NS));
        assert_eq!(parse_duration("100ms"), Some(100_000_000));
        assert_eq!(parse_duration("1.02:00:00"), Some((86400 + 7200) * NS));
        assert_eq!(parse_duration("00:00:01.5"), Some(1_500_000_000));
        assert_eq!(format_duration(90 * NS + 500_000_000), "00:01:30.5000000");
        assert_eq!(format_duration(86400 * NS * 2 + 3600 * NS), "2.01:00:00");
    }
}
