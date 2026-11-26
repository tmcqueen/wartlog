//! NDJSON normalization (spec "NDJSON normalization"), including CLEF (Serilog compact format).

use serde_json::{Map, Value as J};
use std::sync::Arc;
use wartlog_model::{severity_from_text, template, time, unhex, Record, Source, SourceKind, Value};

/// Parses a timestamp value: RFC 3339 text, or epoch seconds/ms/us/ns by magnitude.
pub fn parse_time(v: &J) -> Option<i64> {
    match v {
        J::String(s) => time::parse_datetime(s).or_else(|| s.trim().parse::<f64>().ok().map(epoch_any)),
        J::Number(n) => n.as_f64().map(epoch_any),
        _ => None,
    }
}

fn epoch_any(x: f64) -> i64 {
    let a = x.abs();
    if a >= 1e17 {
        x as i64
    } else if a >= 1e14 {
        (x * 1e3) as i64
    } else if a >= 1e11 {
        (x * 1e6) as i64
    } else {
        (x * 1e9) as i64
    }
}

fn get_ci<'a>(m: &'a Map<String, J>, names: &[&str]) -> Option<(&'a String, &'a J)> {
    for n in names {
        if let Some(kv) = m.iter().find(|(k, _)| k.eq_ignore_ascii_case(n)) {
            return Some(kv);
        }
    }
    None
}

const TIME_KEYS: &[&str] = &["@t", "time", "timestamp", "ts"];
const LEVEL_KEYS: &[&str] = &["@l", "level", "severity", "lvl"];
const BODY_KEYS: &[&str] = &["@m", "msg", "message"];
const TRACE_KEYS: &[&str] = &["@tr", "trace_id", "traceid"];
const SPAN_KEYS: &[&str] = &["@sp", "span_id", "spanid"];
const SOURCE_KEYS: &[&str] = &["service", "service.name", "app"];

fn text(v: &J) -> String {
    match v {
        J::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// Best-effort level from the start of a plain text line (`ERROR ...`, `[warn] ...`, `level=info`).
pub fn sniff_level(line: &str) -> u8 {
    let head: String = line.chars().take(48).collect();
    for tok in wartlog_model::tokens(&head) {
        let sev = match tok.as_str() {
            "trace" | "trc" => 1,
            "debug" | "dbg" => 5,
            "info" | "inf" | "information" => 9,
            "notice" => 10,
            "warn" | "wrn" | "warning" => 13,
            "error" | "err" | "eror" => 17,
            "fatal" | "ftl" | "crit" | "critical" | "panic" => 21,
            _ => continue,
        };
        return sev;
    }
    0
}

/// Normalizes one line. `default_source` names the source when the record does not.
/// A line that is not a JSON object becomes a record with the line as its body.
pub fn normalize(line: &str, default_source: &str, now_ns: i64) -> Record {
    let line = line.trim_end_matches(['\r', '\n']);
    let parsed: Option<Map<String, J>> = if line.trim_start().starts_with('{') { serde_json::from_str(line).ok() } else { None };
    let Some(mut m) = parsed else {
        let src = Arc::new(Source::new(SourceKind::Ndjson, vec![("source.name".into(), default_source.into())]));
        let mut r = Record::log(src, now_ns, sniff_level(line), line);
        r.observed_ns = now_ns;
        return r;
    };
    normalize_object(&mut m, default_source, now_ns)
}

pub fn normalize_object(m: &mut Map<String, J>, default_source: &str, now_ns: i64) -> Record {
    let mut take = |names: &[&str]| -> Option<J> {
        let k = get_ci(m, names)?.0.clone();
        m.remove(&k)
    };
    let time_ns = take(TIME_KEYS).and_then(|v| parse_time(&v)).unwrap_or(now_ns);
    let level = take(LEVEL_KEYS);
    let body = take(BODY_KEYS).map(|v| text(&v));
    let tmpl = take(&["@mt"]).map(|v| text(&v));
    let trace = take(TRACE_KEYS).and_then(|v| unhex(text(&v).trim())).and_then(|b| <[u8; 16]>::try_from(b).ok());
    let span = take(SPAN_KEYS).and_then(|v| unhex(text(&v).trim())).and_then(|b| <[u8; 8]>::try_from(b).ok());
    let source_name = get_ci(m, SOURCE_KEYS).map(|(_, v)| text(v)).unwrap_or_else(|| default_source.to_string());
    let (severity, severity_text) = match &level {
        Some(J::Number(n)) => (n.as_u64().map(|x| x.min(24) as u8).unwrap_or(0), None),
        Some(v) => {
            let t = text(v);
            (severity_from_text(&t), Some(t))
        }
        // CLEF: a missing @l means Information
        None if tmpl.is_some() => (9, None),
        None => (0, None),
    };
    // CLEF renames: @x exception, @i event id, @r renderings (dropped)
    if let Some(x) = m.remove("@x") {
        m.insert("exception".into(), x);
    }
    if let Some(i) = m.remove("@i") {
        m.insert("event.id".into(), i);
    }
    m.remove("@r");
    let mut args = Vec::new();
    if let Some(t) = &tmpl {
        for h in template::holes(t) {
            let v = if h.name.is_empty() { None } else { m.get(&h.name) };
            args.push(v.map(Value::from_json).unwrap_or(Value::Null));
        }
    }
    let attrs: Vec<(String, Value)> = m.iter().map(|(k, v)| (k.clone(), Value::from_json(v))).collect();
    let src = Arc::new(Source::new(SourceKind::Ndjson, vec![("source.name".into(), source_name)]));
    // with a template, the rendered message is dropped (it is rebuilt from template plus args)
    let body_text = if tmpl.is_some() { String::new() } else { body.unwrap_or_default() };
    Record {
        seq: 0,
        time_ns,
        observed_ns: now_ns,
        severity,
        severity_text,
        body: body_text,
        template: tmpl,
        args,
        attrs,
        source: src,
        trace_id: trace,
        span_id: span,
        span: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_json() {
        let r = normalize(r#"{"time":"2026-11-26T06:00:00Z","level":"warn","msg":"disk low","free_mb":120,"service":"backup"}"#, "tcp", 5);
        assert_eq!(r.severity, 13);
        assert_eq!(r.severity_text.as_deref(), Some("warn"));
        assert_eq!(r.body, "disk low");
        assert_eq!(r.source.display_name(), "backup");
        assert_eq!(r.attr("free_mb"), Some(&Value::I64(120)));
        assert_eq!(r.time_ns, time::parse_datetime("2026-11-26T06:00:00Z").unwrap());
    }

    #[test]
    fn clef() {
        let r = normalize(
            r#"{"@t":"2026-11-26T06:00:01.5Z","@mt":"User {UserId} logged in from {Ip}","UserId":42,"Ip":"10.0.0.2","@tr":"0af7651916cd43dd8448eb211c80319c","@sp":"b7ad6b7169203331"}"#,
            "unix",
            5,
        );
        assert_eq!(r.severity, 9);
        assert_eq!(r.rendered_body(), "User 42 logged in from 10.0.0.2");
        assert_eq!(r.args, vec![Value::I64(42), Value::Str("10.0.0.2".into())]);
        assert!(r.attr("UserId").is_some(), "named holes stay attributes");
        assert_eq!(r.trace_id.unwrap()[0], 0x0a);
        assert_eq!(r.source.display_name(), "unix");
    }

    #[test]
    fn plain_text_and_epochs() {
        let r = normalize("2026-11-26 ERROR could not open /x", "my-app", 7);
        assert_eq!(r.body, "2026-11-26 ERROR could not open /x");
        assert_eq!(r.severity, 17);
        assert_eq!(r.time_ns, 7);
        assert_eq!(normalize(r#"{"ts":1764000000.5,"msg":"x"}"#, "a", 0).time_ns, 1_764_000_000_500_000_000);
        assert_eq!(normalize(r#"{"ts":1764000000500,"msg":"x"}"#, "a", 0).time_ns, 1_764_000_000_500_000_000);
        assert_eq!(normalize(r#"{"Level":3,"Message":"x"}"#, "a", 0).severity, 3);
        assert_eq!(normalize("{not json", "a", 0).body, "{not json");
    }
}
