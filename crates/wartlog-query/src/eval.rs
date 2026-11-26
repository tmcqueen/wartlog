//! Expression evaluation over records (raw block views or materialized rows).

use std::cmp::Ordering;
use std::sync::Arc;
use wartlog_model::{hex, level_name, time, tokens, Record, Source, SpanData, Value};

use crate::ast::*;

/// Anything that can answer built-in columns: a record in a block, or a materialized record.
pub trait RecordLike {
    fn time(&self) -> i64;
    fn observed(&self) -> i64;
    fn severity(&self) -> u8;
    fn severity_text(&self) -> Option<String>;
    fn body(&self) -> String;
    fn template(&self) -> Option<String>;
    fn source(&self) -> &Arc<Source>;
    fn trace_id(&self) -> Option<[u8; 16]>;
    fn span_id(&self) -> Option<[u8; 8]>;
    fn seq(&self) -> u64;
    fn span(&self) -> Option<SpanData>;
    fn attr(&self, k: &str) -> Option<Value>;
    fn attrs(&self) -> Vec<(String, Value)>;
    fn is_span(&self) -> bool;
}

impl RecordLike for Record {
    fn time(&self) -> i64 {
        self.time_ns
    }
    fn observed(&self) -> i64 {
        self.observed_ns
    }
    fn severity(&self) -> u8 {
        self.severity
    }
    fn severity_text(&self) -> Option<String> {
        self.severity_text.clone()
    }
    fn body(&self) -> String {
        self.rendered_body()
    }
    fn template(&self) -> Option<String> {
        self.template.clone()
    }
    fn source(&self) -> &Arc<Source> {
        &self.source
    }
    fn trace_id(&self) -> Option<[u8; 16]> {
        self.trace_id
    }
    fn span_id(&self) -> Option<[u8; 8]> {
        self.span_id
    }
    fn seq(&self) -> u64 {
        self.seq
    }
    fn span(&self) -> Option<SpanData> {
        self.span.clone()
    }
    fn attr(&self, k: &str) -> Option<Value> {
        Record::attr(self, k).cloned()
    }
    fn attrs(&self) -> Vec<(String, Value)> {
        self.attrs.clone()
    }
    fn is_span(&self) -> bool {
        self.span.is_some()
    }
}

impl RecordLike for wartlog_store::Rec<'_> {
    fn time(&self) -> i64 {
        wartlog_store::Rec::time(self)
    }
    fn observed(&self) -> i64 {
        wartlog_store::Rec::observed(self)
    }
    fn severity(&self) -> u8 {
        wartlog_store::Rec::severity(self)
    }
    fn severity_text(&self) -> Option<String> {
        wartlog_store::Rec::severity_text(self).map(str::to_string)
    }
    fn body(&self) -> String {
        wartlog_store::Rec::body(self)
    }
    fn template(&self) -> Option<String> {
        wartlog_store::Rec::template(self).map(str::to_string)
    }
    fn source(&self) -> &Arc<Source> {
        wartlog_store::Rec::source(self)
    }
    fn trace_id(&self) -> Option<[u8; 16]> {
        wartlog_store::Rec::trace_id(self)
    }
    fn span_id(&self) -> Option<[u8; 8]> {
        wartlog_store::Rec::span_id(self)
    }
    fn seq(&self) -> u64 {
        wartlog_store::Rec::seq(self)
    }
    fn span(&self) -> Option<SpanData> {
        wartlog_store::Rec::span(self)
    }
    fn attr(&self, k: &str) -> Option<Value> {
        wartlog_store::Rec::attr(self, k)
    }
    fn attrs(&self) -> Vec<(String, Value)> {
        wartlog_store::Rec::attrs(self)
    }
    fn is_span(&self) -> bool {
        wartlog_store::Rec::is_span(self)
    }
}

fn opt_str(s: Option<&str>) -> Value {
    Value::Str(s.unwrap_or("").to_string())
}

pub fn span_kind_name(k: u8) -> &'static str {
    match k {
        1 => "internal",
        2 => "server",
        3 => "client",
        4 => "producer",
        5 => "consumer",
        _ => "unspecified",
    }
}

pub fn span_status_name(s: u8) -> &'static str {
    match s {
        1 => "ok",
        2 => "error",
        _ => "unset",
    }
}

pub fn attrs_json(a: &[(String, Value)]) -> Value {
    let mut m = serde_json::Map::new();
    for (k, v) in a {
        m.insert(k.clone(), v.to_json());
    }
    Value::Json(serde_json::Value::Object(m).to_string())
}

pub fn source_json(s: &Source) -> Value {
    let mut m = serde_json::Map::new();
    for (k, v) in &s.attrs {
        m.insert(k.clone(), serde_json::Value::String(v.clone()));
    }
    Value::Json(serde_json::Value::Object(m).to_string())
}

/// Value of a built-in column for a record. `Named` resolves to an attribute, then a resource attribute.
pub fn builtin(c: &Column, r: &dyn RecordLike) -> Value {
    let src = r.source();
    match c {
        Column::Timestamp => Value::Timestamp(r.time()),
        Column::ObservedTimestamp => Value::Timestamp(r.observed()),
        Column::SeverityNumber => Value::I64(r.severity() as i64),
        Column::SeverityText => Value::Str(r.severity_text().unwrap_or_else(|| level_name(r.severity()).to_string())),
        Column::Level => Value::Str(level_name(r.severity()).to_string()),
        Column::Body => Value::Str(r.body()),
        Column::Template => Value::Str(r.template().unwrap_or_default()),
        Column::Source => Value::Str(src.display_name()),
        Column::SourceKind => Value::Str(src.kind.name().to_string()),
        Column::Service => opt_str(src.get("service.name")),
        Column::Unit => opt_str(src.get("systemd.unit").or_else(|| src.get("syslog.identifier"))),
        Column::Namespace => opt_str(src.get("k8s.namespace.name")),
        Column::Pod => match r.attr("k8s.pod.name") {
            Some(v) => v,
            None => opt_str(src.get("k8s.pod.name")),
        },
        Column::Container => opt_str(src.get("k8s.container.name")),
        Column::TraceId => Value::Str(r.trace_id().map(|t| hex(&t)).unwrap_or_default()),
        Column::SpanId => Value::Str(r.span_id().map(|t| hex(&t)).unwrap_or_default()),
        Column::Id => Value::I64(r.seq() as i64),
        Column::Name => Value::Str(r.span().map(|s| s.name).unwrap_or_default()),
        Column::ParentSpanId => Value::Str(r.span().and_then(|s| s.parent_span_id).map(|p| hex(&p)).unwrap_or_default()),
        Column::EndTimestamp => r.span().map(|s| Value::Timestamp(s.end_ns)).unwrap_or(Value::Null),
        Column::Duration => r.span().map(|s| Value::Duration(s.end_ns - r.time())).unwrap_or(Value::Null),
        Column::Kind => Value::Str(r.span().map(|s| span_kind_name(s.kind)).unwrap_or("").to_string()),
        Column::Status => Value::Str(r.span().map(|s| span_status_name(s.status)).unwrap_or("").to_string()),
        Column::StatusMessage => Value::Str(r.span().map(|s| s.status_message).unwrap_or_default()),
        Column::Attr(k) => r.attr(k).unwrap_or(Value::Null),
        Column::Resource(k) => src.get(k).map(|v| Value::Str(v.to_string())).unwrap_or(Value::Null),
        Column::Attributes => attrs_json(&r.attrs()),
        Column::ResourceMap => source_json(src),
        Column::Named(k) => r.attr(k).or_else(|| src.get(k).map(|v| Value::Str(v.to_string()))).unwrap_or(Value::Null),
    }
}

/// Column values available to expressions.
pub trait Fields {
    fn col(&self, c: &Column) -> Value;
}

/// A source descriptor alone (for source-level predicates during planning).
pub struct SourceFields<'a>(pub &'a Source);

impl Fields for SourceFields<'_> {
    fn col(&self, c: &Column) -> Value {
        let s = self.0;
        match c {
            Column::Source => Value::Str(s.display_name()),
            Column::SourceKind => Value::Str(s.kind.name().to_string()),
            Column::Service => opt_str(s.get("service.name")),
            Column::Unit => opt_str(s.get("systemd.unit").or_else(|| s.get("syslog.identifier"))),
            Column::Namespace => opt_str(s.get("k8s.namespace.name")),
            Column::Container => opt_str(s.get("k8s.container.name")),
            Column::Resource(k) => s.get(k).map(|v| Value::Str(v.to_string())).unwrap_or(Value::Null),
            Column::ResourceMap => source_json(s),
            _ => Value::Null,
        }
    }
}

pub struct Ctx {
    pub now: i64,
}

pub fn truthy(v: &Value) -> bool {
    matches!(v, Value::Bool(true))
}

fn num(v: &Value) -> Option<f64> {
    match v {
        Value::I64(_) | Value::F64(_) | Value::Bool(_) => v.as_f64(),
        Value::Str(s) => s.trim().parse().ok(),
        _ => None,
    }
}

/// KQL-ish comparison with light coercion (strings that hold numbers or datetimes compare as such).
pub fn compare(a: &Value, b: &Value) -> Option<Ordering> {
    use Value::*;
    match (a, b) {
        (Null, _) | (_, Null) => None,
        (I64(x), I64(y)) => Some(x.cmp(y)),
        (Timestamp(x), Timestamp(y)) | (Duration(x), Duration(y)) => Some(x.cmp(y)),
        (Timestamp(x), Str(s)) => time::parse_datetime(s).map(|y| x.cmp(&y)),
        (Str(s), Timestamp(y)) => time::parse_datetime(s).map(|x| x.cmp(y)),
        (Duration(x), Str(s)) => time::parse_duration(s).map(|y| x.cmp(&y)),
        (Str(x), Str(y)) => Some(x.cmp(y)),
        (Bool(x), Bool(y)) => Some(x.cmp(y)),
        (Bool(x), Str(s)) | (Str(s), Bool(x)) => {
            let y = s.eq_ignore_ascii_case("true");
            let o = x.cmp(&y);
            Some(if matches!(a, Str(_)) { o.reverse() } else { o })
        }
        _ => match (num(a), num(b)) {
            (Some(x), Some(y)) => x.partial_cmp(&y),
            _ => {
                let (x, y) = (a.to_text(), b.to_text());
                Some(x.cmp(&y))
            }
        },
    }
}

pub fn equals(a: &Value, b: &Value, ci: bool) -> bool {
    if ci {
        if let (Some(x), Some(y)) = (text_of(a), text_of(b)) {
            return x.to_lowercase() == y.to_lowercase();
        }
    }
    compare(a, b) == Some(Ordering::Equal)
}

fn text_of(v: &Value) -> Option<String> {
    match v {
        Value::Null => None,
        Value::Str(s) => Some(s.clone()),
        other => Some(other.to_text()),
    }
}

/// KQL `has`: `needle`'s terms appear as whole, consecutive terms in `hay`.
pub fn has(hay: &str, needle: &str, cs: bool) -> bool {
    let n: Vec<String> = if cs { split_terms(needle) } else { tokens(needle).collect() };
    if n.is_empty() {
        return true;
    }
    let h: Vec<String> = if cs { split_terms(hay) } else { tokens(hay).collect() };
    h.windows(n.len()).any(|w| w == n.as_slice())
}

fn split_terms(s: &str) -> Vec<String> {
    s.split(|c: char| !c.is_alphanumeric()).filter(|t| !t.is_empty()).map(str::to_string).collect()
}

pub fn has_prefix(hay: &str, prefix: &str, cs: bool) -> bool {
    if cs {
        split_terms(hay).iter().any(|t| t.starts_with(prefix))
    } else {
        let p = prefix.to_lowercase();
        tokens(hay).any(|t| t.starts_with(&p))
    }
}

fn str_op(op: StrOp, cs: bool, hay: &str, needle: &str) -> bool {
    match op {
        StrOp::Has => has(hay, needle, cs),
        StrOp::HasPrefix => has_prefix(hay, needle, cs),
        StrOp::Contains | StrOp::StartsWith | StrOp::EndsWith => {
            let (h, n) = if cs { (hay.to_string(), needle.to_string()) } else { (hay.to_lowercase(), needle.to_lowercase()) };
            match op {
                StrOp::Contains => h.contains(&n),
                StrOp::StartsWith => h.starts_with(&n),
                _ => h.ends_with(&n),
            }
        }
    }
}

pub const FUNCTIONS: &[&str] = &[
    "ago", "now", "datetime", "todatetime", "bin", "floor", "tostring", "toint", "tolong", "todouble", "toreal", "tobool", "tolower", "toupper", "strlen",
    "iff", "iif", "isempty", "isnotempty", "isnull", "isnotnull", "strcat", "coalesce", "abs", "round", "substring", "trim",
];
pub const AGGREGATES: &[&str] = &["count", "countif", "dcount", "min", "max", "avg", "sum"];

pub fn is_function(f: &str) -> bool {
    FUNCTIONS.contains(&f)
}
pub fn is_aggregate(f: &str) -> bool {
    AGGREGATES.contains(&f)
}

pub fn eval(e: &Expr, f: &dyn Fields, cx: &Ctx) -> Value {
    use Value::*;
    match e {
        Expr::Lit(v) => v.clone(),
        Expr::Col(c) => f.col(c),
        Expr::Not(a) => match eval(a, f, cx) {
            Bool(b) => Bool(!b),
            Null => Null,
            _ => Bool(false),
        },
        Expr::Neg(a) => match eval(a, f, cx) {
            I64(n) => I64(-n),
            F64(n) => F64(-n),
            Duration(n) => Duration(-n),
            _ => Null,
        },
        Expr::Bin(BinOp::And, a, b) => {
            let l = eval(a, f, cx);
            if !truthy(&l) {
                return Bool(false);
            }
            Bool(truthy(&eval(b, f, cx)))
        }
        Expr::Bin(BinOp::Or, a, b) => {
            if truthy(&eval(a, f, cx)) {
                return Bool(true);
            }
            Bool(truthy(&eval(b, f, cx)))
        }
        Expr::Bin(op, a, b) => {
            let (l, r) = (eval(a, f, cx), eval(b, f, cx));
            binop(*op, &l, &r)
        }
        Expr::Str { op, cs, neg, lhs, rhs } => {
            let l = eval(lhs, f, cx);
            let r = eval(rhs, f, cx);
            let (Some(h), Some(n)) = (text_of(&l), text_of(&r)) else { return Bool(*neg) };
            Bool(str_op(*op, *cs, &h, &n) != *neg)
        }
        Expr::In { lhs, list, neg, ci } => {
            let l = eval(lhs, f, cx);
            let hit = list.iter().any(|x| equals(&l, &eval(x, f, cx), *ci));
            Bool(hit != *neg)
        }
        Expr::Between { lhs, lo, hi, neg } => {
            let v = eval(lhs, f, cx);
            let (a, b) = (eval(lo, f, cx), eval(hi, f, cx));
            let inside = matches!(compare(&v, &a), Some(Ordering::Greater | Ordering::Equal)) && matches!(compare(&v, &b), Some(Ordering::Less | Ordering::Equal));
            Bool(inside != *neg)
        }
        Expr::Regex { lhs, re } => match text_of(&eval(lhs, f, cx)) {
            Some(s) => Bool(re.0.is_match(&s)),
            None => Bool(false),
        },
        Expr::Call(name, args) => call(name, args, f, cx),
    }
}

pub fn binop(op: BinOp, l: &Value, r: &Value) -> Value {
    use Value::*;
    match op {
        BinOp::Eq => {
            if l.is_null() || r.is_null() {
                return Bool(false);
            }
            Bool(equals(l, r, false))
        }
        BinOp::Ne => {
            if l.is_null() && r.is_null() {
                return Bool(false);
            }
            Bool(!equals(l, r, false))
        }
        BinOp::EqCi => Bool(equals(l, r, true)),
        BinOp::NeCi => Bool(!equals(l, r, true)),
        BinOp::Lt => Bool(compare(l, r) == Some(Ordering::Less)),
        BinOp::Le => Bool(matches!(compare(l, r), Some(Ordering::Less | Ordering::Equal))),
        BinOp::Gt => Bool(compare(l, r) == Some(Ordering::Greater)),
        BinOp::Ge => Bool(matches!(compare(l, r), Some(Ordering::Greater | Ordering::Equal))),
        BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::Div | BinOp::Mod => arith(op, l, r),
        BinOp::And | BinOp::Or => unreachable!("handled with short-circuit"),
    }
}

fn arith(op: BinOp, l: &Value, r: &Value) -> Value {
    use Value::*;
    match (op, l, r) {
        (BinOp::Sub, Timestamp(a), Timestamp(b)) => Duration(a - b),
        (BinOp::Add, Timestamp(a), Duration(b)) | (BinOp::Add, Duration(b), Timestamp(a)) => Timestamp(a + b),
        (BinOp::Sub, Timestamp(a), Duration(b)) => Timestamp(a - b),
        (BinOp::Add, Duration(a), Duration(b)) => Duration(a + b),
        (BinOp::Sub, Duration(a), Duration(b)) => Duration(a - b),
        (BinOp::Div, Duration(a), Duration(b)) if *b != 0 => F64(*a as f64 / *b as f64),
        (BinOp::Mul, Duration(a), x) | (BinOp::Mul, x, Duration(a)) => match num(x) {
            Some(n) => Duration((*a as f64 * n) as i64),
            None => Null,
        },
        (BinOp::Div, Duration(a), x) => match num(x) {
            Some(n) if n != 0.0 => Duration((*a as f64 / n) as i64),
            _ => Null,
        },
        (_, I64(a), I64(b)) => match op {
            BinOp::Add => I64(a.wrapping_add(*b)),
            BinOp::Sub => I64(a.wrapping_sub(*b)),
            BinOp::Mul => I64(a.wrapping_mul(*b)),
            BinOp::Div if *b != 0 => I64(a / b),
            BinOp::Mod if *b != 0 => I64(a % b),
            _ => Null,
        },
        _ => match (num(l), num(r)) {
            (Some(a), Some(b)) => match op {
                BinOp::Add => F64(a + b),
                BinOp::Sub => F64(a - b),
                BinOp::Mul => F64(a * b),
                BinOp::Div if b != 0.0 => F64(a / b),
                BinOp::Mod if b != 0.0 => F64(a % b),
                _ => Null,
            },
            _ => match (op, l, r) {
                (BinOp::Add, Str(a), Str(b)) => Str(format!("{a}{b}")),
                _ => Null,
            },
        },
    }
}

/// `bin(v, size)`: rounds down to a multiple of size.
pub fn bin(v: &Value, size: &Value) -> Value {
    use Value::*;
    match (v, size) {
        (Timestamp(t), Duration(d)) if *d > 0 => Timestamp(t.div_euclid(*d) * d),
        (Duration(t), Duration(d)) if *d > 0 => Duration(t.div_euclid(*d) * d),
        (I64(x), I64(s)) if *s > 0 => I64(x.div_euclid(*s) * s),
        (x, s) => match (num(x), num(s)) {
            (Some(x), Some(s)) if s > 0.0 => F64((x / s).floor() * s),
            _ => Null,
        },
    }
}

fn call(name: &str, args: &[Expr], f: &dyn Fields, cx: &Ctx) -> Value {
    use Value::*;
    let a = |i: usize| args.get(i).map(|e| eval(e, f, cx)).unwrap_or(Null);
    match name {
        "now" => match a(0) {
            Duration(d) => Timestamp(cx.now + d),
            _ => Timestamp(cx.now),
        },
        "ago" => match a(0) {
            Duration(d) => Timestamp(cx.now - d),
            v => match v.to_text().as_str() {
                s if !s.is_empty() => time::parse_duration(s).map(|d| Timestamp(cx.now - d)).unwrap_or(Null),
                _ => Null,
            },
        },
        "datetime" | "todatetime" => match a(0) {
            Timestamp(t) => Timestamp(t),
            Str(s) => time::parse_datetime(&s).map(Timestamp).unwrap_or(Null),
            I64(n) => Timestamp(n),
            _ => Null,
        },
        "bin" | "floor" => bin(&a(0), &a(1)),
        "tostring" => match a(0) {
            Null => Str(String::new()),
            v => Str(v.to_text()),
        },
        "toint" | "tolong" => match a(0) {
            F64(x) => I64(x as i64),
            v => v.as_i64().map(I64).or_else(|| num(&v).map(|x| I64(x as i64))).unwrap_or(Null),
        },
        "todouble" | "toreal" => num(&a(0)).or_else(|| a(0).as_f64()).map(F64).unwrap_or(Null),
        "tobool" => a(0).as_bool().map(Bool).unwrap_or(Null),
        "tolower" => Str(a(0).to_text().to_lowercase()),
        "toupper" => Str(a(0).to_text().to_uppercase()),
        "trim" => Str(a(0).to_text().trim().to_string()),
        "strlen" => match a(0) {
            Null => Null,
            v => I64(v.to_text().chars().count() as i64),
        },
        "substring" => {
            let s: Vec<char> = a(0).to_text().chars().collect();
            let start = a(1).as_i64().unwrap_or(0).max(0) as usize;
            let len = a(2).as_i64().map(|l| l.max(0) as usize).unwrap_or(usize::MAX);
            Str(s.iter().skip(start).take(len).collect())
        }
        "iff" | "iif" => {
            if truthy(&a(0)) {
                a(1)
            } else {
                a(2)
            }
        }
        "isempty" => Bool(matches!(a(0), Null) || a(0).to_text().is_empty()),
        "isnotempty" => Bool(!(matches!(a(0), Null) || a(0).to_text().is_empty())),
        "isnull" => Bool(a(0).is_null()),
        "isnotnull" => Bool(!a(0).is_null()),
        "strcat" => Str((0..args.len()).map(|i| a(i).to_text()).collect()),
        "coalesce" => (0..args.len()).map(a).find(|v| !v.is_null() && !matches!(v, Str(s) if s.is_empty())).unwrap_or(Null),
        "abs" => match a(0) {
            I64(n) => I64(n.abs()),
            Duration(n) => Duration(n.abs()),
            v => num(&v).map(|x| F64(x.abs())).unwrap_or(Null),
        },
        "round" => {
            let p = a(1).as_i64().unwrap_or(0) as i32;
            num(&a(0)).map(|x| F64((x * 10f64.powi(p)).round() / 10f64.powi(p))).unwrap_or(Null)
        }
        // aggregates outside summarize and unknown names never reach here (the parser and
        // validator reject them), but stay total
        _ => Null,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn has_semantics() {
        assert!(has("Connection to db-01 failed", "db", false));
        assert!(has("Connection to db-01 failed", "DB-01", false));
        assert!(!has("Connection to db01 failed", "db", false));
        assert!(has("a b c", "b c", false));
        assert!(!has("abc", "ab", false));
        assert!(!has("Timeout", "timeout", true));
        assert!(has_prefix("GET /api/users", "us", false));
    }

    #[test]
    fn comparisons_and_arith() {
        use Value::*;
        assert_eq!(compare(&I64(5), &Str("5".into())), Some(Ordering::Equal));
        assert_eq!(compare(&I64(5), &F64(5.5)), Some(Ordering::Less));
        assert!(equals(&Str("API".into()), &Str("api".into()), true));
        assert_eq!(arith(BinOp::Sub, &Timestamp(10), &Timestamp(4)), Duration(6));
        assert_eq!(arith(BinOp::Div, &Duration(3_000_000), &Duration(1_000_000)), F64(3.0));
        assert_eq!(bin(&Timestamp(3_700_000_000_000), &Duration(3_600_000_000_000)), Timestamp(3_600_000_000_000));
        assert_eq!(binop(BinOp::Eq, &Null, &Null), Bool(false));
    }
}
