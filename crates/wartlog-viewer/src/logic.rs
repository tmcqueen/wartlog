//! Viewer logic that does not touch Qt: query editing for click-to-filter, completion, and the
//! JSON the QML side renders. Unit tested without a display.

use serde_json::{json, Value as J};
use wartlog_api::proto::{Schema, SeenSource, SourceInfo, Table};
use wartlog_api::{kql_attr, kql_literal, kql_string};
use wartlog_model::{time, Value};

/// Columns the query bar may filter on directly (everything else is an attribute).
const BUILTIN_FILTER: &[&str] = &["Level", "Source", "SourceKind", "Service", "Unit", "Namespace", "Pod", "Container", "TraceId", "SpanId", "Template", "Body"];

fn column_ref(column: &str) -> String {
    if BUILTIN_FILTER.contains(&column) {
        column.to_string()
    } else {
        kql_attr(column)
    }
}

/// Adds `| where col == value` (or `!=`) to a query, merging into a trailing `where`.
pub fn add_filter(query: &str, column: &str, value: &Value, exclude: bool) -> String {
    let cond = format!("{} {} {}", column_ref(column), if exclude { "!=" } else { "==" }, kql_literal(value));
    append_where(query, &cond)
}

/// The query for one source from the sidebar, keeping the user's other filters.
pub fn only_source(query: &str, display: &str, kind: &str) -> String {
    let base = strip_source_filter(query);
    append_where(&base, &format!("Source == {} and SourceKind == {}", kql_string(display), kql_string(kind)))
}

fn strip_source_filter(query: &str) -> String {
    // remove a `| where Source == "..." and SourceKind == "..."` stage written by only_source
    let mut out = Vec::new();
    for stage in split_stages(query) {
        let s = stage.trim();
        if s.starts_with("where Source == ") && s.contains(" and SourceKind == ") && !s.contains(" or ") && s.matches(" and ").count() == 1 {
            continue;
        }
        out.push(stage.trim().to_string());
    }
    out.join(" | ")
}

/// Splits on top-level `|` (not inside strings or parentheses).
pub fn split_stages(q: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    let mut depth = 0i32;
    let mut esc = false;
    for c in q.chars() {
        if let Some(qc) = quote {
            cur.push(c);
            if esc {
                esc = false;
            } else if c == '\\' {
                esc = true;
            } else if c == qc {
                quote = None;
            }
            continue;
        }
        match c {
            '"' | '\'' => {
                quote = Some(c);
                cur.push(c);
            }
            '(' | '[' => {
                depth += 1;
                cur.push(c);
            }
            ')' | ']' => {
                depth -= 1;
                cur.push(c);
            }
            '|' if depth == 0 => out.push(std::mem::take(&mut cur)),
            _ => cur.push(c),
        }
    }
    out.push(cur);
    out
}

fn append_where(query: &str, cond: &str) -> String {
    let q = query.trim();
    let q = if q.is_empty() { "logs" } else { q };
    let stages = split_stages(q);
    let last = stages.last().map(|s| s.trim()).unwrap_or("");
    if stages.len() > 1 && (last.starts_with("where ") || last.starts_with("filter ")) && !last.contains(" or ") {
        let mut out: Vec<String> = stages[..stages.len() - 1].iter().map(|s| s.trim().to_string()).collect();
        out.push(format!("{last} and {cond}"));
        out.join(" | ")
    } else {
        format!("{q} | where {cond}")
    }
}

/// Byte ranges of the hole values inside `body`, found by matching the template's literal text.
pub fn arg_spans(template: &str, body: &str) -> Vec<(usize, usize)> {
    let holes = wartlog_model::template::holes(template);
    if holes.is_empty() {
        return vec![];
    }
    let lit = |s: &str| s.replace("{{", "{").replace("}}", "}");
    let mut literals = Vec::new();
    let mut pos = 0;
    for h in &holes {
        literals.push(lit(&template[pos..h.span.0]));
        pos = h.span.1;
    }
    literals.push(lit(&template[pos..]));
    let mut spans = Vec::new();
    let Some(rest) = body.strip_prefix(literals[0].as_str()) else { return vec![] };
    let mut at = body.len() - rest.len();
    for (i, next) in literals.iter().enumerate().skip(1) {
        let end = if i == literals.len() - 1 {
            if next.is_empty() {
                body.len()
            } else if body.ends_with(next.as_str()) && body.len() - next.len() >= at {
                body.len() - next.len()
            } else {
                return spans;
            }
        } else if next.is_empty() {
            // adjacent holes: cannot split, give the rest to this hole
            at
        } else {
            match body[at..].find(next.as_str()) {
                Some(r) => at + r,
                None => return spans,
            }
        };
        if end > at {
            spans.push((at, end));
        }
        at = end + next.len();
    }
    spans
}

fn esc(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

/// Body as Qt rich text with template arguments highlighted.
pub fn body_html(template: Option<&str>, body: &str, accent: &str) -> String {
    let one_line = |s: &str| esc(&s.replace('\n', " ⏎ "));
    let Some(t) = template.filter(|t| !t.is_empty()) else { return one_line(body) };
    let spans = arg_spans(t, body);
    if spans.is_empty() {
        return one_line(body);
    }
    let mut out = String::new();
    let mut at = 0;
    for (a, b) in spans {
        out.push_str(&one_line(&body[at..a]));
        out.push_str(&format!("<font color=\"{accent}\">{}</font>", one_line(&body[a..b])));
        at = b;
    }
    out.push_str(&one_line(&body[at..]));
    out
}

/// Local time zone offset (seconds) via the C library.
pub fn local_offset(ns: i64) -> i64 {
    let t: libc::time_t = ns.div_euclid(time::NS) as libc::time_t;
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    if unsafe { libc::localtime_r(&t, &mut tm) }.is_null() {
        return 0;
    }
    tm.tm_gmtoff
}

pub fn time_label(ns: i64) -> String {
    let l = ns + local_offset(ns) * time::NS;
    let s = time::format_rfc3339(l - l.rem_euclid(time::NS));
    format!("{} {}.{:03}", &s[5..10], &s[11..19], l.rem_euclid(time::NS) / 1_000_000)
}

pub fn full_time(ns: i64) -> String {
    let off = local_offset(ns);
    let l = ns + off * time::NS;
    let s = time::format_rfc3339(l - l.rem_euclid(time::NS));
    let sign = if off < 0 { '-' } else { '+' };
    format!("{} {}.{:09} {sign}{:02}:{:02}", &s[..10], &s[11..19], l.rem_euclid(time::NS), off.abs() / 3600, off.abs() / 60 % 60)
}

fn cell_text(v: &Value) -> String {
    match v {
        Value::Timestamp(t) => time_label(*t),
        Value::Null => String::new(),
        other => other.to_text(),
    }
}

/// True when every built-in log column is present (the row can be drawn as a log line).
pub fn is_log_shape(t: &Table) -> bool {
    ["Timestamp", "Level", "Source", "Body"].iter().all(|c| t.col(c).is_some())
}

/// Rows for the list. Log-shaped rows carry named fields; other results carry `cells`.
pub fn rows_json(t: &Table, accent: &str) -> J {
    let log = is_log_shape(t);
    let g = |row: &[Value], c: &str| t.col(c).map(|i| row[i].clone()).unwrap_or(Value::Null);
    let rows: Vec<J> = t
        .rows
        .iter()
        .zip(&t.ids)
        .map(|(row, id)| {
            if log {
                let tmpl = g(row, "Template").to_text();
                let body = g(row, "Body").to_text();
                let ts = match g(row, "Timestamp") {
                    Value::Timestamp(x) => x,
                    _ => 0,
                };
                json!({
                    "rid": id.map(|i| i as f64).unwrap_or(-1.0),
                    "ts": time_label(ts),
                    "level": g(row, "Level").to_text(),
                    "source": g(row, "Source").to_text(),
                    "kind": g(row, "SourceKind").to_text(),
                    "html": body_html(Some(&tmpl), &body, accent),
                    "trace": g(row, "TraceId").to_text(),
                })
            } else {
                {
                let cells: Vec<String> = row.iter().map(cell_text).collect();
                json!({ "rid": id.map(|i| i as f64).unwrap_or(-1.0), "cellsJson": serde_json::to_string(&cells).unwrap_or_default(), "text": cells.join(" ") })
            }
            }
        })
        .collect();
    json!({ "log": log, "columns": t.columns, "rows": rows })
}

/// `180 ms`, `2.5 s`, `850 µs`, `3m 20s`.
pub fn human_duration(ns: i64) -> String {
    let a = ns.unsigned_abs() as f64;
    if a < 1e3 {
        format!("{ns} ns")
    } else if a < 1e6 {
        format!("{:.0} µs", ns as f64 / 1e3)
    } else if a < 1e9 {
        let ms = ns as f64 / 1e6;
        if ms < 10.0 { format!("{ms:.1} ms") } else { format!("{ms:.0} ms") }
    } else if a < 60e9 {
        format!("{:.2} s", ns as f64 / 1e9)
    } else {
        let s = ns / time::NS;
        if s < 3600 { format!("{}m {}s", s / 60, s % 60) } else { format!("{}h {}m", s / 3600, s / 60 % 60) }
    }
}

/// Spans for the trace panel: depth from parent links, start offset and duration for a waterfall.
pub fn spans_json(t: &Table) -> J {
    let g = |row: &[Value], c: &str| t.col(c).map(|i| row[i].clone()).unwrap_or(Value::Null);
    let ts = |v: Value| match v {
        Value::Timestamp(x) => x,
        _ => 0,
    };
    let ids: Vec<String> = t.rows.iter().map(|r| g(r, "SpanId").to_text()).collect();
    let parents: Vec<String> = t.rows.iter().map(|r| g(r, "ParentSpanId").to_text()).collect();
    let starts: Vec<i64> = t.rows.iter().map(|r| ts(g(r, "Timestamp"))).collect();
    let ends: Vec<i64> = t.rows.iter().map(|r| ts(g(r, "EndTimestamp"))).collect();
    let t0 = starts.iter().copied().min().unwrap_or(0);
    let t1 = ends.iter().copied().max().unwrap_or(t0).max(t0 + 1);
    let depth = |mut i: usize| {
        let mut d = 0;
        while let Some(p) = ids.iter().position(|x| !parents[i].is_empty() && *x == parents[i]) {
            d += 1;
            i = p;
            if d > 32 {
                break;
            }
        }
        d
    };
    let spans: Vec<J> = t
        .rows
        .iter()
        .enumerate()
        .map(|(i, r)| {
            json!({
                "name": g(r, "Name").to_text(),
                "source": g(r, "Source").to_text(),
                "status": g(r, "Status").to_text(),
                "kind": g(r, "Kind").to_text(),
                "start": time_label(starts[i]),
                "duration": human_duration(ends[i] - starts[i]),
                "offset": (starts[i] - t0) as f64 / (t1 - t0) as f64,
                "width": ((ends[i] - starts[i]) as f64 / (t1 - t0) as f64).max(0.002),
                "depth": depth(i),
                "rid": t.ids[i].map(|x| x as f64).unwrap_or(-1.0),
            })
        })
        .collect();
    json!({ "total": human_duration(t1 - t0), "items": spans })
}

/// The record details pane.
pub fn details_json(r: &wartlog_model::Record) -> J {
    let mut fields: Vec<J> = vec![
        json!({ "key": "Level", "value": wartlog_model::level_name(r.severity), "filter": "Level" }),
        json!({ "key": "Source", "value": r.source.display_name(), "filter": "Source" }),
        json!({ "key": "SourceKind", "value": r.source.kind.name(), "filter": "SourceKind" }),
    ];
    if let Some(t) = &r.template {
        fields.push(json!({ "key": "Template", "value": t, "filter": "Template" }));
    }
    if let Some(t) = r.trace_id {
        fields.push(json!({ "key": "TraceId", "value": wartlog_model::hex(&t), "filter": "TraceId" }));
    }
    if let Some(s) = r.span_id {
        fields.push(json!({ "key": "SpanId", "value": wartlog_model::hex(&s), "filter": "SpanId" }));
    }
    for (k, v) in &r.source.attrs {
        fields.push(json!({ "key": k, "value": v, "filter": "", "resource": true }));
    }
    for (k, v) in &r.attrs {
        let typed = match v {
            Value::I64(i) => json!(i),
            Value::F64(f) => json!(f),
            Value::Bool(b) => json!(b),
            other => json!(other.to_text()),
        };
        fields.push(json!({ "key": k, "value": v.to_text(), "typed": typed, "filter": k }));
    }
    // journald's catalog text (`-x`), when the entry has one
    let catalog = r.attr("catalog").or_else(|| r.attr("MESSAGE_ID_TEXT")).map(|v| v.to_text()).unwrap_or_default();
    json!({
        "id": r.seq as f64,
        "time": full_time(r.time_ns),
        "observed": full_time(r.observed_ns),
        "body": r.rendered_body(),
        "trace": r.trace_id.map(|t| wartlog_model::hex(&t)).unwrap_or_default(),
        "hasThread": r.attr("thread.id").or_else(|| r.attr("_PID")).is_some(),
        "catalog": catalog,
        "fields": fields,
    })
}

/// Sidebar tree: kinds, then sources, with configured-source states.
pub fn sources_json(seen: &[SeenSource], configured: &[SourceInfo]) -> J {
    let label = |k: &str| match k {
        "journald" => "journald",
        "k8s" => "Kubernetes",
        "otlp" => "OpenTelemetry",
        "ndjson" => "NDJSON",
        other => other,
    }
    .to_string();
    let mut groups: std::collections::BTreeMap<String, Vec<J>> = Default::default();
    for s in seen {
        groups.entry(s.kind.clone()).or_default().push(json!({ "display": s.display, "kind": s.kind }));
    }
    let order = ["journald", "k8s", "otlp", "ndjson"];
    let mut out = Vec::new();
    let mut keys: Vec<&String> = groups.keys().collect();
    keys.sort_by_key(|k| order.iter().position(|o| o == k).unwrap_or(9));
    for k in keys {
        let mut items = groups[k].clone();
        items.sort_by(|a, b| a["display"].as_str().cmp(&b["display"].as_str()));
        out.push(json!({ "kind": k, "label": label(k), "items": items }));
    }
    let cfg: Vec<J> = configured
        .iter()
        .map(|c| json!({ "name": c.name, "kind": c.kind, "role": c.role, "state": c.state, "received": c.received as f64, "error": c.last_error, "hint": c.hint, "config": c.config, "enabled": c.enabled }))
        .collect();
    json!({ "groups": out, "configured": cfg })
}

/// Completion candidates for the word at `cursor` (a char index) in `text`.
pub fn complete(schema: &Schema, text: &str, cursor: usize) -> J {
    let chars: Vec<char> = text.chars().collect();
    let cursor = cursor.min(chars.len());
    let mut start = cursor;
    while start > 0 && (chars[start - 1].is_alphanumeric() || matches!(chars[start - 1], '_' | '.' | '-' | '!' | '"')) {
        start -= 1;
    }
    let word: String = chars[start..cursor].iter().collect();
    let before: String = chars[..start].iter().collect();
    let before_t = before.trim_end();
    let prev_stage = split_stages(&before).last().cloned().unwrap_or_default();
    let mut out: Vec<(String, &str)> = Vec::new();
    let want = word.trim_start_matches('"').to_lowercase();
    let push = |out: &mut Vec<(String, &'static str)>, s: &str, kind: &'static str| {
        if s.to_lowercase().starts_with(&want) && s != word {
            out.push((s.to_string(), kind));
        }
    };
    if before_t.is_empty() {
        for t in &schema.tables {
            push(&mut out, t, "table");
        }
    } else if before_t.ends_with('|') {
        for o in &schema.operators {
            push(&mut out, o, "operator");
        }
    } else if before_t.ends_with("==") || before_t.ends_with("!=") || before_t.ends_with(" in (") {
        // values for the column just before the operator
        let lhs: String = before_t.trim_end_matches(['=', '!', '(']).trim_end().trim_end_matches(" in").rsplit([' ', '(']).next().unwrap_or("").to_string();
        let key = lhs.strip_prefix("Attributes[\"").and_then(|s| s.strip_suffix("\"]")).unwrap_or(&lhs).to_string();
        if key == "Level" {
            for l in ["trace", "debug", "info", "warn", "error", "fatal"] {
                push(&mut out, &kql_string(l), "value");
            }
        } else if key == "Source" {
            for s in &schema.sources {
                push(&mut out, &kql_string(&s.display), "value");
            }
        } else if key == "SourceKind" {
            for s in ["journald", "k8s", "otlp", "ndjson"] {
                push(&mut out, &kql_string(s), "value");
            }
        } else if let Some(k) = schema.keys.iter().find(|k| k.key == key) {
            for v in &k.values {
                let lit = if v.parse::<f64>().is_ok() { v.clone() } else { kql_string(v) };
                push(&mut out, &lit, "value");
            }
        }
    } else {
        let in_where = prev_stage.trim_start().starts_with("where") || prev_stage.trim_start().starts_with("filter");
        for c in &schema.log_columns {
            push(&mut out, c, "column");
        }
        for k in &schema.keys {
            push(&mut out, &kql_attr(&k.key), "attribute");
        }
        if in_where {
            for w in ["and", "or", "not", "has", "!has", "contains", "!contains", "startswith", "endswith", "hasprefix", "in", "!in", "between", "matches regex"] {
                push(&mut out, w, "keyword");
            }
        }
        for f in &schema.functions {
            push(&mut out, &format!("{f}("), "function");
        }
        if prev_stage.contains("summarize") {
            for f in &schema.aggregates {
                push(&mut out, &format!("{f}("), "aggregate");
            }
            push(&mut out, "by", "keyword");
        }
    }
    out.dedup();
    out.truncate(40);
    json!({
        "start": start,
        "end": cursor,
        "items": out.into_iter().map(|(t, k)| json!({ "text": t, "kind": k })).collect::<Vec<_>>(),
    })
}

/// Saved views: a named query plus time range, stored as JSON next to the config.
pub fn views_path() -> std::path::PathBuf {
    let base = std::env::var_os("XDG_CONFIG_HOME").filter(|d| !d.is_empty()).map(std::path::PathBuf::from).unwrap_or_else(|| {
        std::path::PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".config")
    });
    base.join("wartlog").join("views.json")
}

pub fn load_views() -> J {
    std::fs::read_to_string(views_path()).ok().and_then(|t| serde_json::from_str(&t).ok()).filter(|v: &J| v.is_array()).unwrap_or_else(|| json!([]))
}

pub fn save_view(name: &str, query: &str, range: &str) -> anyhow::Result<J> {
    let mut v = load_views();
    let arr = v.as_array_mut().unwrap();
    arr.retain(|x| x["name"] != name);
    arr.push(json!({ "name": name, "query": query, "range": range }));
    write_views(&v)?;
    Ok(v)
}

pub fn delete_view(name: &str) -> anyhow::Result<J> {
    let mut v = load_views();
    v.as_array_mut().unwrap().retain(|x| x["name"] != name);
    write_views(&v)?;
    Ok(v)
}

fn write_views(v: &J) -> anyhow::Result<()> {
    let p = views_path();
    if let Some(d) = p.parent() {
        std::fs::create_dir_all(d)?;
    }
    std::fs::write(&p, serde_json::to_string_pretty(v)?)?;
    Ok(())
}

/// `15m` → a time range ending now; `all` → none.
pub fn range_ns(range: &str) -> Option<(Option<i64>, Option<i64>)> {
    if range == "all" || range.is_empty() {
        return None;
    }
    time::parse_duration(range).map(|d| (Some(time::now_ns() - d), None))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filters() {
        assert_eq!(add_filter("logs", "Level", &Value::Str("error".into()), false), r#"logs | where Level == "error""#);
        assert_eq!(add_filter(r#"logs | where Level == "error""#, "code", &Value::I64(5), true), r#"logs | where Level == "error" and code != 5"#);
        assert_eq!(add_filter("logs | take 5", "k8s.pod.name", &Value::Str("a".into()), false), r#"logs | take 5 | where Attributes["k8s.pod.name"] == "a""#);
        assert_eq!(add_filter(r#"logs | where Body has "a|b""#, "Unit", &Value::Str("x".into()), false), r#"logs | where Body has "a|b" and Unit == "x""#);
        let q = only_source(r#"logs | where Level == "error""#, "foo.service", "journald");
        assert_eq!(q, r#"logs | where Level == "error" and Source == "foo.service" and SourceKind == "journald""#);
        let q = only_source(r#"logs | where Source == "a" and SourceKind == "journald" | where Level == "error""#, "b", "otlp");
        assert_eq!(q, r#"logs | where Level == "error" and Source == "b" and SourceKind == "otlp""#);
        for q in [q, add_filter("", "x y", &Value::Str("\"q\"".into()), false)] {
            wartlog_query::parse(&q).unwrap_or_else(|e| panic!("{q}: {e}"));
        }
    }

    #[test]
    fn highlighting() {
        let b = "GET /orders/7 returned 500 in 3.5 ms";
        let sp = arg_spans("GET {Path} returned {Status} in {Elapsed} ms", b);
        let words: Vec<&str> = sp.iter().map(|(a, z)| &b[*a..*z]).collect();
        assert_eq!(words, vec!["/orders/7", "500", "3.5"]);
        assert_eq!(arg_spans("{A} then {B}", "x then y").len(), 2);
        assert!(arg_spans("Hello {Name}!", "Goodbye").is_empty());
        let h = body_html(Some("User {Id} <admin>"), "User 42 <admin>", "#f00");
        assert_eq!(h, "User <font color=\"#f00\">42</font> &lt;admin&gt;");
    }

    #[test]
    fn completion() {
        let mut s = Schema::default();
        s.tables = vec!["logs".into(), "spans".into()];
        s.operators = vec!["where".into(), "take".into()];
        s.log_columns = vec!["Level".into(), "Body".into()];
        s.keys = vec![wartlog_api::proto::KeyInfo { key: "code".into(), values: vec!["E_TIMEOUT".into(), "200".into()] }];
        let items = |j: J| j["items"].as_array().unwrap().iter().map(|i| i["text"].as_str().unwrap().to_string()).collect::<Vec<_>>();
        assert_eq!(items(complete(&s, "lo", 2)), vec!["logs"]);
        assert_eq!(items(complete(&s, "logs | w", 8)), vec!["where"]);
        assert!(items(complete(&s, "logs | where Le", 15)).contains(&"Level".to_string()));
        assert_eq!(items(complete(&s, "logs | where Level == ", 22))[0], "\"trace\"");
        assert_eq!(items(complete(&s, "logs | where code == ", 21)), vec!["\"E_TIMEOUT\"", "200"]);
        let j = complete(&s, "logs | where co", 15);
        assert_eq!(j["start"], 13);
    }

    #[test]
    fn durations() {
        assert_eq!(human_duration(180_000_000), "180 ms");
        assert_eq!(human_duration(2_500_000), "2.5 ms");
        assert_eq!(human_duration(850_000), "850 µs");
        assert_eq!(human_duration(2_500_000_000), "2.50 s");
        assert_eq!(human_duration(200 * time::NS), "3m 20s");
    }

    #[test]
    fn times() {
        let ns = time::parse_datetime("2026-11-26T06:00:00.250Z").unwrap();
        let t = time_label(ns);
        assert!(t.ends_with(".250") && t.len() == 18, "{t}");
        assert!(full_time(ns).contains('.'));
    }
}
