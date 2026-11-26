//! WartLog's canonical data model (spec section 6). Follows the OpenTelemetry log and trace models.
//! No I/O lives here.

use serde::{Deserialize, Serialize};
use std::fmt;
use std::sync::Arc;

pub mod template;
pub mod time;

pub use template::render;

/// 64-bit hash used for fingerprints and index keys (xxh3).
pub fn hash(b: &[u8]) -> u64 {
    xxhash_rust::xxh3::xxh3_64(b)
}

/// KQL `has` tokenization: lowercase alphanumeric runs.
pub fn tokens(s: &str) -> impl Iterator<Item = String> + '_ {
    s.split(|c: char| !c.is_alphanumeric()).filter(|t| !t.is_empty()).map(|t| t.to_lowercase())
}

/// Shortest term the index stores. Shorter `has` terms are matched by scanning.
pub const MIN_TERM_CHARS: usize = 3;

/// A typed value (spec "Typed values").
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "t", content = "v", rename_all = "lowercase")]
pub enum Value {
    Null,
    Bool(bool),
    I64(i64),
    F64(f64),
    Str(String),
    Bytes(Vec<u8>),
    /// ns since the Unix epoch
    Timestamp(i64),
    /// ns
    Duration(i64),
    /// a string holding JSON (OTLP arrays and maps)
    Json(String),
}

impl Value {
    pub fn is_null(&self) -> bool {
        matches!(self, Value::Null)
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::Str(s) | Value::Json(s) => Some(s),
            _ => None,
        }
    }

    pub fn as_i64(&self) -> Option<i64> {
        match self {
            Value::I64(i) | Value::Timestamp(i) | Value::Duration(i) => Some(*i),
            Value::F64(f) => Some(*f as i64),
            Value::Bool(b) => Some(*b as i64),
            Value::Str(s) => s.trim().parse().ok(),
            _ => None,
        }
    }

    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Value::F64(f) => Some(*f),
            Value::I64(i) | Value::Timestamp(i) | Value::Duration(i) => Some(*i as f64),
            Value::Bool(b) => Some(*b as i64 as f64),
            Value::Str(s) => s.trim().parse().ok(),
            _ => None,
        }
    }

    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Value::Bool(b) => Some(*b),
            Value::I64(i) => Some(*i != 0),
            Value::Str(s) => match s.to_ascii_lowercase().as_str() {
                "true" => Some(true),
                "false" => Some(false),
                _ => None,
            },
            _ => None,
        }
    }

    /// Text form used for rendering templates, `has`/`contains` on attributes and index terms.
    pub fn to_text(&self) -> String {
        match self {
            Value::Null => String::new(),
            Value::Bool(b) => b.to_string(),
            Value::I64(i) => i.to_string(),
            Value::F64(f) => fmt_f64(*f),
            Value::Str(s) | Value::Json(s) => s.clone(),
            Value::Bytes(b) => hex(b),
            Value::Timestamp(t) => time::format_rfc3339(*t),
            Value::Duration(d) => time::format_duration(*d),
        }
    }

    /// JSON form for output.
    pub fn to_json(&self) -> serde_json::Value {
        use serde_json::Value as J;
        match self {
            Value::Null => J::Null,
            Value::Bool(b) => J::Bool(*b),
            Value::I64(i) => J::from(*i),
            Value::F64(f) => serde_json::Number::from_f64(*f).map(J::Number).unwrap_or(J::Null),
            Value::Str(s) => J::String(s.clone()),
            Value::Json(s) => serde_json::from_str(s).unwrap_or_else(|_| J::String(s.clone())),
            Value::Bytes(b) => J::String(hex(b)),
            Value::Timestamp(t) => J::String(time::format_rfc3339(*t)),
            Value::Duration(d) => J::String(time::format_duration(*d)),
        }
    }

    pub fn from_json(j: &serde_json::Value) -> Value {
        use serde_json::Value as J;
        match j {
            J::Null => Value::Null,
            J::Bool(b) => Value::Bool(*b),
            J::Number(n) => match n.as_i64() {
                Some(i) => Value::I64(i),
                None => Value::F64(n.as_f64().unwrap_or(f64::NAN)),
            },
            J::String(s) => Value::Str(s.clone()),
            other => Value::Json(other.to_string()),
        }
    }

    /// KQL type name.
    pub fn type_name(&self) -> &'static str {
        match self {
            Value::Null => "null",
            Value::Bool(_) => "bool",
            Value::I64(_) => "long",
            Value::F64(_) => "real",
            Value::Str(_) => "string",
            Value::Bytes(_) => "bytes",
            Value::Timestamp(_) => "datetime",
            Value::Duration(_) => "timespan",
            Value::Json(_) => "dynamic",
        }
    }
}

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_text())
    }
}

fn fmt_f64(f: f64) -> String {
    if f.fract() == 0.0 && f.abs() < 1e15 {
        format!("{f:.1}")
    } else {
        f.to_string()
    }
}

pub fn hex(b: &[u8]) -> String {
    let mut s = String::with_capacity(b.len() * 2);
    for x in b {
        s.push_str(&format!("{x:02x}"));
    }
    s
}

pub fn unhex(s: &str) -> Option<Vec<u8>> {
    if s.len() % 2 != 0 {
        return None;
    }
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(s.get(i..i + 2)?, 16).ok()).collect()
}

/// Kind of source (spec "Source descriptor").
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
#[repr(u8)]
pub enum SourceKind {
    Otlp = 1,
    Journald = 2,
    K8s = 3,
    Ndjson = 4,
}

impl SourceKind {
    pub fn from_u8(v: u8) -> Option<SourceKind> {
        Some(match v {
            1 => SourceKind::Otlp,
            2 => SourceKind::Journald,
            3 => SourceKind::K8s,
            4 => SourceKind::Ndjson,
            _ => return None,
        })
    }

    pub fn name(self) -> &'static str {
        match self {
            SourceKind::Otlp => "otlp",
            SourceKind::Journald => "journald",
            SourceKind::K8s => "k8s",
            SourceKind::Ndjson => "ndjson",
        }
    }
}

/// "Who sent this". `attrs` holds only the identifying attributes, so the fingerprint is stable
/// across runs; per-run values (instance ids, pids, pod names) go on each record.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Source {
    pub kind: SourceKind,
    /// identifying attributes, sorted by key
    pub attrs: Vec<(String, String)>,
}

impl Source {
    pub fn new(kind: SourceKind, mut attrs: Vec<(String, String)>) -> Source {
        attrs.retain(|(_, v)| !v.is_empty());
        attrs.sort();
        attrs.dedup_by(|a, b| a.0 == b.0);
        Source { kind, attrs }
    }

    pub fn get(&self, k: &str) -> Option<&str> {
        self.attrs.iter().find(|(a, _)| a == k).map(|(_, v)| v.as_str())
    }

    pub fn fingerprint(&self) -> u64 {
        let mut b = vec![self.kind as u8];
        for (k, v) in &self.attrs {
            b.extend_from_slice(k.as_bytes());
            b.push(0);
            b.extend_from_slice(v.as_bytes());
            b.push(0);
        }
        hash(&b)
    }

    /// Display name per kind (spec table).
    pub fn display_name(&self) -> String {
        match self.kind {
            SourceKind::Otlp => self.get("service.name").unwrap_or("unknown_service").to_string(),
            SourceKind::Journald => self
                .get("systemd.unit")
                .or_else(|| self.get("syslog.identifier"))
                .unwrap_or("journal")
                .to_string(),
            SourceKind::K8s => format!(
                "{}/{}/{}",
                self.get("k8s.namespace.name").unwrap_or("?"),
                self.get("k8s.workload.name").unwrap_or("?"),
                self.get("k8s.container.name").unwrap_or("?")
            ),
            SourceKind::Ndjson => self.get("source.name").unwrap_or("ndjson").to_string(),
        }
    }
}

/// Record kind.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[repr(u8)]
pub enum Kind {
    Log = 1,
    Span = 2,
}

/// Span-only fields.
#[derive(Clone, Debug, PartialEq, Default, Serialize, Deserialize)]
pub struct SpanData {
    pub parent_span_id: Option<[u8; 8]>,
    pub end_ns: i64,
    pub name: String,
    /// 0 unspecified, 1 internal, 2 server, 3 client, 4 producer, 5 consumer
    pub kind: u8,
    /// 0 unset, 1 ok, 2 error
    pub status: u8,
    pub status_message: String,
}

/// A log record or span (spans carry `span`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Record {
    /// assigned by the writer; 0 until then
    pub seq: u64,
    pub time_ns: i64,
    pub observed_ns: i64,
    pub severity: u8,
    pub severity_text: Option<String>,
    /// for templated records this is the template with no rendering; use `rendered_body`
    pub body: String,
    pub template: Option<String>,
    pub args: Vec<Value>,
    pub attrs: Vec<(String, Value)>,
    pub source: Arc<Source>,
    pub trace_id: Option<[u8; 16]>,
    pub span_id: Option<[u8; 8]>,
    pub span: Option<SpanData>,
}

impl Record {
    pub fn log(source: Arc<Source>, time_ns: i64, severity: u8, body: impl Into<String>) -> Record {
        Record {
            seq: 0,
            time_ns,
            observed_ns: time_ns,
            severity,
            severity_text: None,
            body: body.into(),
            template: None,
            args: Vec::new(),
            attrs: Vec::new(),
            source,
            trace_id: None,
            span_id: None,
            span: None,
        }
    }

    pub fn kind(&self) -> Kind {
        if self.span.is_some() {
            Kind::Span
        } else {
            Kind::Log
        }
    }

    /// Body text: the template rendered with its args when there is a template.
    pub fn rendered_body(&self) -> String {
        match &self.template {
            Some(t) => render(t, &self.args),
            None => self.body.clone(),
        }
    }

    pub fn attr(&self, k: &str) -> Option<&Value> {
        self.attrs.iter().find(|(a, _)| a == k).map(|(_, v)| v)
    }
}

/// OTel severity number to a level name.
pub fn level_name(sev: u8) -> &'static str {
    match sev {
        1..=4 => "trace",
        5..=8 => "debug",
        9..=12 => "info",
        13..=16 => "warn",
        17..=20 => "error",
        21..=24 => "fatal",
        _ => "unspecified",
    }
}

/// Lowest severity number of a level name (accepts common spellings).
pub fn level_floor(name: &str) -> Option<u8> {
    Some(match name.trim().to_ascii_lowercase().as_str() {
        "trace" | "trc" | "verbose" | "vrb" => 1,
        "debug" | "dbg" | "debg" => 5,
        "info" | "inf" | "information" | "notice" | "informational" => 9,
        "warn" | "wrn" | "warning" => 13,
        "error" | "err" | "eror" | "fail" => 17,
        "fatal" | "ftl" | "critical" | "crit" | "crt" | "emerg" | "emergency" | "alert" | "panic" => 21,
        _ => return None,
    })
}

/// Severity number for a severity text from a sender (NDJSON level, CLEF @l, ...).
pub fn severity_from_text(s: &str) -> u8 {
    if let Ok(n) = s.trim().parse::<u8>() {
        if n <= 24 {
            return n;
        }
    }
    match s.trim().to_ascii_lowercase().as_str() {
        "notice" => 10,
        "alert" => 22,
        "emerg" | "emergency" => 24,
        other => level_floor(other).unwrap_or(0),
    }
}

/// journald PRIORITY (0 emerg .. 7 debug) to OTel severity number.
pub fn severity_from_syslog(p: u8) -> u8 {
    match p {
        0 => 24,
        1 => 22,
        2 => 21,
        3 => 17,
        4 => 13,
        5 => 10,
        6 => 9,
        _ => 5,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fingerprint_ignores_attr_order_and_empty_values() {
        let a = Source::new(SourceKind::Otlp, vec![("service.name".into(), "a".into()), ("host.name".into(), "h".into())]);
        let b = Source::new(
            SourceKind::Otlp,
            vec![("host.name".into(), "h".into()), ("service.name".into(), "a".into()), ("x".into(), "".into())],
        );
        assert_eq!(a.fingerprint(), b.fingerprint());
        assert_eq!(a.display_name(), "a");
    }

    #[test]
    fn severities() {
        assert_eq!(severity_from_text("Warning"), 13);
        assert_eq!(severity_from_text("err"), 17);
        assert_eq!(severity_from_text("17"), 17);
        assert_eq!(level_name(severity_from_syslog(3)), "error");
        assert_eq!(level_name(severity_from_syslog(6)), "info");
    }

    #[test]
    fn tokens_are_lowercase_alnum_runs() {
        let t: Vec<_> = tokens("GET /api/v1 took 3ms; ÉTÉ ok").collect();
        assert_eq!(t, ["get", "api", "v1", "took", "3ms", "été", "ok"]);
    }
}
