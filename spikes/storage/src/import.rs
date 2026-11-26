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
