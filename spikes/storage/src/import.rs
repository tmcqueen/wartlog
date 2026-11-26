//! journalctl -o json  ->  WartLog records.

use anyhow::Result;
use serde_json::Value as J;

use crate::format::{Record, Source, Value};

const NUMERIC: &[&str] = &["_PID", "_UID", "_GID", "TID", "CODE_LINE", "ERRNO", "_AUDIT_SESSION", "_AUDIT_LOGINUID", "SYSLOG_PID"];

fn jstr(v: &J) -> Option<String> {
    match v {
        J::String(s) => Some(s.clone()),
        J::Array(a) => {
            // binary field: array of byte values, or multiple values for the same field
            if a.iter().all(|x| x.is_u64()) {
                let bytes: Vec<u8> = a.iter().map(|x| x.as_u64().unwrap() as u8).collect();
                Some(String::from_utf8_lossy(&bytes).into_owned())
            } else {
                Some(a.iter().filter_map(jstr).collect::<Vec<_>>().join("\n"))
            }
        }
        J::Null => None,
        other => Some(other.to_string()),
    }
}

/// journald PRIORITY (0 emerg .. 7 debug) to OTel severity number.
pub fn severity_from_priority(p: u8) -> u8 {
    match p { 0 => 24, 1 => 23, 2 => 21, 3 => 17, 4 => 13, 5 => 10, 6 => 9, _ => 5 }
}

pub fn parse_line(line: &str, journal: &str, seq: u64, shift_ns: i64, observed_ns: i64) -> Result<Option<Record>> {
    let v: J = serde_json::from_str(line)?;
    let o = match v.as_object() { Some(o) => o, None => return Ok(None) };
    let time_us: i64 = o.get("__REALTIME_TIMESTAMP").and_then(jstr).and_then(|s| s.parse().ok()).unwrap_or(0);
    let body = o.get("MESSAGE").and_then(jstr).unwrap_or_default();
    let prio: u8 = o.get("PRIORITY").and_then(jstr).and_then(|s| s.parse().ok()).unwrap_or(6);
    let get = |k: &str| o.get(k).and_then(jstr);
    let unit = get("_SYSTEMD_USER_UNIT").or_else(|| get("_SYSTEMD_UNIT"))
        .or_else(|| get("SYSLOG_IDENTIFIER")).or_else(|| get("_COMM")).unwrap_or_else(|| "unknown".into());
    let mut src = vec![("journal".to_string(), journal.to_string()), ("systemd.unit".to_string(), unit)];
    if let Some(h) = get("_HOSTNAME") { src.push(("host.name".into(), h)); }
    let mut attrs = Vec::with_capacity(o.len());
    for (k, val) in o {
        if k.starts_with("__") || k == "MESSAGE" { continue; }
        let s = match jstr(val) { Some(s) => s, None => continue };
        let tv = if NUMERIC.contains(&k.as_str()) {
            s.parse::<i64>().map(Value::I64).unwrap_or(Value::Str(s))
        } else { Value::Str(s) };
        attrs.push((k.clone(), tv));
    }
    Ok(Some(Record {
        seq,
        time_ns: time_us * 1000 + shift_ns,
        observed_ns,
        severity: severity_from_priority(prio),
        source: Source { kind: 2, attrs: src },
        body,
        attrs,
    }))
}

/// Generic NDJSON (for example DuckDB `COPY ... (FORMAT json)`) -> WartLog records.
pub struct NdjsonOpts {
    pub table: String,
    /// first present of these fields gives the event time
    pub time_fields: Vec<String>,
    /// these fields (with the table name) identify the source; they are kept as attributes too
    pub source_fields: Vec<String>,
    pub body_field: Option<String>,
}

/// "YYYY-MM-DD[ T]HH:MM:SS[.ffffff]" (UTC, as DuckDB writes TIMESTAMP) to epoch ns.
pub fn parse_timestamp(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    if b.len() < 19 || b[4] != b'-' || b[7] != b'-' || (b[10] != b' ' && b[10] != b'T') || b[13] != b':' || b[16] != b':' { return None; }
    let n = |r: std::ops::Range<usize>| s.get(r)?.parse::<i64>().ok();
    let (y, m, d, hh, mm, ss) = (n(0..4)?, n(5..7)?, n(8..10)?, n(11..13)?, n(14..16)?, n(17..19)?);
    let mut frac_ns = 0i64;
    if b.len() > 19 {
        if b[19] != b'.' { return None; }
        let digits = &s[20..];
        if digits.is_empty() || digits.len() > 9 || !digits.bytes().all(|c| c.is_ascii_digit()) { return None; }
        frac_ns = digits.parse::<i64>().ok()? * 10i64.pow(9 - digits.len() as u32);
    }
    // days from civil (Howard Hinnant)
    let y2 = if m <= 2 { y - 1 } else { y };
    let era = if y2 >= 0 { y2 } else { y2 - 399 } / 400;
    let yoe = y2 - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146097 + doe - 719468;
    Some(((days * 86400 + hh * 3600 + mm * 60 + ss) * 1_000_000_000) + frac_ns)
}

fn typed(v: &J) -> Option<Value> {
    Some(match v {
        J::Null => return None,
        J::Bool(b) => Value::Bool(*b),
        J::Number(n) => match n.as_i64() { Some(i) => Value::I64(i), None => Value::F64(n.as_f64()?) },
        J::String(s) => match parse_timestamp(s) { Some(t) => Value::I64(t), None => Value::Str(s.clone()) },
        other => Value::Str(other.to_string()),
    })
}

pub fn parse_ndjson(line: &str, o: &NdjsonOpts, seq: u64, shift_ns: i64, observed_ns: i64) -> Result<Option<Record>> {
    let v: J = serde_json::from_str(line)?;
    let m = match v.as_object() { Some(m) => m, None => return Ok(None) };
    let time = o.time_fields.iter().find_map(|k| m.get(k).and_then(|x| x.as_str()).and_then(parse_timestamp));
    let time = match time { Some(t) => t, None => return Ok(None) };
    let body = o.body_field.as_ref().and_then(|k| m.get(k)).and_then(|x| x.as_str()).unwrap_or("").to_string();
    let mut src = vec![("table".to_string(), o.table.clone())];
    for k in &o.source_fields {
        let val = match m.get(k) { Some(J::String(s)) => s.clone(), Some(J::Null) | None => "null".into(), Some(x) => x.to_string() };
        src.push((k.clone(), val));
    }
    let mut attrs = Vec::with_capacity(m.len());
    for (k, val) in m {
        if Some(k) == o.body_field.as_ref() { continue; }
        if let Some(tv) = typed(val) { attrs.push((k.clone(), tv)); }
    }
    Ok(Some(Record { seq, time_ns: time + shift_ns, observed_ns, severity: 9, source: Source { kind: 3, attrs: src }, body, attrs }))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn timestamps() {
        assert_eq!(parse_timestamp("1970-01-01 00:00:00"), Some(0));
        assert_eq!(parse_timestamp("2000-03-01 00:00:01.5"), Some(951868801_500_000_000));
        assert_eq!(parse_timestamp("2023-07-04T12:34:56.000001"), Some(1688474096_000_001_000));
        assert_eq!(parse_timestamp("2023-07-04"), None);
    }
}
