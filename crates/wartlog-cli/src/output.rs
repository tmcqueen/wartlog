//! Terminal and JSON output.

use anyhow::Result;
use std::io::Write;
use wartlog_api::proto::{SeenSource, SourceInfo, Status, Table, TemplateCount};
use wartlog_model::{time, Record, Value};

/// Local wall-clock offset in seconds for an instant (via the C library's time zone rules).
fn local_offset(ns: i64) -> i64 {
    let t: libc::time_t = ns.div_euclid(time::NS) as libc::time_t;
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    let r = unsafe { libc::localtime_r(&t, &mut tm) };
    if r.is_null() {
        0
    } else {
        tm.tm_gmtoff
    }
}

/// `11-26 07:03:25.123` in local time.
pub fn short_time(ns: i64) -> String {
    let l = ns + local_offset(ns) * time::NS;
    // 2026-11-26T07:03:25Z
    let s = time::format_rfc3339(l - l.rem_euclid(time::NS));
    let ms = l.rem_euclid(time::NS) / 1_000_000;
    format!("{} {}.{ms:03}", &s[5..10], &s[11..19])
}

/// `2026-11-26 07:03:25.123456789 -06:00` in local time.
pub fn long_time(ns: i64) -> String {
    let off = local_offset(ns);
    let l = ns + off * time::NS;
    let s = time::format_rfc3339(l - l.rem_euclid(time::NS));
    let sign = if off < 0 { '-' } else { '+' };
    format!("{} {}.{:09} {sign}{:02}:{:02}", &s[..10], &s[11..19], l.rem_euclid(time::NS), off.abs() / 3600, off.abs() / 60 % 60)
}

fn level_color(l: &str) -> &'static str {
    match l {
        "error" | "fatal" => "\x1b[31;1m",
        "warn" => "\x1b[33m",
        "debug" | "trace" => "\x1b[2m",
        _ => "",
    }
}

/// True when the query does not sort itself, so log rows read best oldest first (like journalctl).
pub fn display_oldest_first(kql: &str) -> bool {
    match wartlog_query::parse(kql) {
        Ok(q) => !q.ops.iter().any(|o| matches!(o, wartlog_query::ast::Op::Sort(_) | wartlog_query::ast::Op::Top(..))),
        Err(_) => true,
    }
}

fn is_log_shape(t: &Table) -> bool {
    ["Timestamp", "Level", "Source", "Body"].iter().all(|c| t.col(c).is_some()) && t.columns.len() >= 10
}

fn cell(v: &Value) -> String {
    match v {
        Value::Timestamp(t) => short_time(*t),
        Value::Null => String::new(),
        other => other.to_text().replace('\n', "⏎"),
    }
}

/// One log row as a line.
fn log_line(t: &Table, row: &[Value], id: Option<u64>, color: bool) -> String {
    let get = |c: &str| t.col(c).map(|i| &row[i]);
    let ts = match get("Timestamp") {
        Some(Value::Timestamp(x)) => short_time(*x),
        _ => String::new(),
    };
    let lvl = get("Level").map(|v| v.to_text()).unwrap_or_default();
    let src = get("Source").map(|v| v.to_text()).unwrap_or_default();
    let body = get("Body").map(|v| v.to_text()).unwrap_or_default();
    let id = id.map(|i| format!("  #{i}")).unwrap_or_default();
    let lv = format!("{:<5}", if lvl == "unspecified" { "-" } else { lvl.as_str() });
    if color {
        format!("\x1b[2m{ts}\x1b[0m {}{lv}\x1b[0m \x1b[36m{src}\x1b[0m  {body}\x1b[2m{id}\x1b[0m", level_color(&lvl))
    } else {
        format!("{ts} {lv} {src}  {body}{id}")
    }
}

fn json_row(t: &Table, row: &[Value], id: Option<u64>) -> serde_json::Value {
    let mut m = serde_json::Map::new();
    for (c, v) in t.columns.iter().zip(row) {
        if !matches!(v, Value::Null) {
            m.insert(c.clone(), v.to_json());
        }
    }
    if let Some(i) = id {
        m.insert("_id".into(), i.into());
    }
    serde_json::Value::Object(m)
}

/// Rows as they stream in (tail).
pub fn rows(out: &mut impl Write, t: &Table, json: bool, color: bool) -> Result<()> {
    for (row, id) in t.rows.iter().zip(&t.ids) {
        if json {
            writeln!(out, "{}", json_row(t, row, *id))?;
        } else if is_log_shape(t) {
            writeln!(out, "{}", log_line(t, row, *id, color))?;
        } else {
            writeln!(out, "{}", row.iter().map(cell).collect::<Vec<_>>().join("  "))?;
        }
    }
    Ok(())
}

/// A whole result.
pub fn table(t: &Table, json: bool, color: bool, oldest_first: bool) -> Result<()> {
    let mut out = std::io::stdout().lock();
    let mut order: Vec<usize> = (0..t.rows.len()).collect();
    if is_log_shape(t) && oldest_first {
        order.reverse();
    }
    if json || is_log_shape(t) {
        for i in order {
            let (row, id) = (&t.rows[i], t.ids[i]);
            if json {
                writeln!(out, "{}", json_row(t, row, id))?;
            } else {
                writeln!(out, "{}", log_line(t, row, id, color))?;
            }
        }
        return Ok(());
    }
    // aligned columns, each capped at 80 characters
    let cells: Vec<Vec<String>> = t.rows.iter().map(|r| r.iter().map(cell).collect()).collect();
    let mut w: Vec<usize> = t.columns.iter().map(|c| c.chars().count()).collect();
    for r in &cells {
        for (i, c) in r.iter().enumerate() {
            w[i] = w[i].max(c.chars().count().min(80));
        }
    }
    let fit = |s: &str, n: usize| -> String {
        let len = s.chars().count();
        if len > n {
            format!("{}…", s.chars().take(n - 1).collect::<String>())
        } else {
            format!("{s}{}", " ".repeat(n - len))
        }
    };
    let head: Vec<String> = t.columns.iter().enumerate().map(|(i, c)| fit(c, w[i])).collect();
    if color {
        writeln!(out, "\x1b[1m{}\x1b[0m", head.join("  ").trim_end())?;
    } else {
        writeln!(out, "{}", head.join("  ").trim_end())?;
    }
    for r in cells {
        let line: Vec<String> = r.iter().enumerate().map(|(i, c)| fit(c, w[i])).collect();
        writeln!(out, "{}", line.join("  ").trim_end())?;
    }
    Ok(())
}

pub fn context(t: &Table, center: Option<usize>, color: bool) -> Result<()> {
    let mut out = std::io::stdout().lock();
    for (i, (row, id)) in t.rows.iter().zip(&t.ids).enumerate() {
        let mark = if Some(i) == center { "▶ " } else { "  " };
        writeln!(out, "{mark}{}", log_line(t, row, *id, color))?;
    }
    Ok(())
}

pub fn record(r: &Record) -> Result<()> {
    let mut out = std::io::stdout().lock();
    writeln!(out, "Id           {}", r.seq)?;
    writeln!(out, "Timestamp    {}", long_time(r.time_ns))?;
    writeln!(out, "Observed     {}", long_time(r.observed_ns))?;
    writeln!(out, "Level        {} ({}{})", wartlog_model::level_name(r.severity), r.severity, r.severity_text.as_ref().map(|t| format!(", \"{t}\"")).unwrap_or_default())?;
    writeln!(out, "Source       {} ({})", r.source.display_name(), r.source.kind.name())?;
    for (k, v) in &r.source.attrs {
        writeln!(out, "  {k} = {v}")?;
    }
    if let Some(t) = &r.template {
        writeln!(out, "Template     {t}")?;
    }
    writeln!(out, "Body         {}", r.rendered_body())?;
    if let Some(t) = r.trace_id {
        writeln!(out, "TraceId      {}", wartlog_model::hex(&t))?;
    }
    if let Some(s) = r.span_id {
        writeln!(out, "SpanId       {}", wartlog_model::hex(&s))?;
    }
    if let Some(sp) = &r.span {
        writeln!(out, "Span         {} ({}), {}", sp.name, time::format_duration(sp.end_ns - r.time_ns), ["unset", "ok", "error"].get(sp.status as usize).unwrap_or(&"?"))?;
    }
    if !r.attrs.is_empty() {
        writeln!(out, "Attributes")?;
        for (k, v) in &r.attrs {
            writeln!(out, "  {k} = {}", v.to_text())?;
        }
    }
    Ok(())
}

pub fn trace(spans: &Table, logs: &Table, color: bool) -> Result<()> {
    let mut out = std::io::stdout().lock();
    if spans.rows.is_empty() && logs.rows.is_empty() {
        anyhow::bail!("no spans or logs for this trace");
    }
    let g = |t: &Table, row: &[Value], c: &str| t.col(c).map(|i| row[i].clone()).unwrap_or(Value::Null);
    // indent spans by depth
    let ids: Vec<String> = spans.rows.iter().map(|r| g(spans, r, "SpanId").to_text()).collect();
    let parents: Vec<String> = spans.rows.iter().map(|r| g(spans, r, "ParentSpanId").to_text()).collect();
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
    writeln!(out, "Spans")?;
    for (i, r) in spans.rows.iter().enumerate() {
        let st = g(spans, r, "Status").to_text();
        let err = st == "error";
        let line = format!(
            "{}  {}{}  {}  {}  {}",
            cell(&g(spans, r, "Timestamp")),
            "  ".repeat(depth(i)),
            g(spans, r, "Name").to_text(),
            g(spans, r, "Source").to_text(),
            g(spans, r, "Duration").to_text(),
            st
        );
        if color && err {
            writeln!(out, "\x1b[31m{line}\x1b[0m")?;
        } else {
            writeln!(out, "{line}")?;
        }
    }
    writeln!(out, "Logs")?;
    for (row, id) in logs.rows.iter().zip(&logs.ids) {
        writeln!(out, "{}", log_line(logs, row, *id, color))?;
    }
    Ok(())
}

pub fn templates(t: &[TemplateCount]) {
    const BARS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
    for x in t {
        let max = x.sparkline.iter().copied().max().unwrap_or(0).max(1);
        let spark: String = x.sparkline.iter().map(|&n| if n == 0 { ' ' } else { BARS[((n * 7) / max) as usize] }).collect();
        println!("{:>8}  {spark}  {}", x.count, x.template.replace('\n', "⏎"));
    }
}

pub fn sources(configured: &[SourceInfo], seen: &[SeenSource], json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::json!({ "configured": configured.iter().map(|s| serde_json::json!({
            "name": s.name, "role": s.role, "kind": s.kind, "enabled": s.enabled, "state": s.state,
            "received": s.received, "dropped": s.dropped, "errors": s.errors, "last_error": s.last_error, "hint": s.hint,
        })).collect::<Vec<_>>(), "seen": seen.iter().map(|s| serde_json::json!({ "kind": s.kind, "name": s.display })).collect::<Vec<_>>() }));
        return Ok(());
    }
    if !configured.is_empty() {
        println!("Configured");
        for s in configured {
            println!("  {:<22} {:<12} {:<10} {:>10} received  {}", s.name, s.kind, s.state.split(':').next().unwrap_or(""), s.received, s.hint.lines().next().unwrap_or(""));
            if let Some(e) = &s.last_error {
                println!("  {:<22} last error: {e}", "");
            }
        }
    }
    if seen.is_empty() && !configured.is_empty() {
        return Ok(());
    }
    println!("Seen in the data");
    let mut v: Vec<&SeenSource> = seen.iter().collect();
    v.sort_by(|a, b| (&a.kind, &a.display).cmp(&(&b.kind, &b.display)));
    for s in v {
        println!("  {:<10} {}", s.kind, s.display);
    }
    Ok(())
}

fn bytes(n: u64) -> String {
    let f = n as f64;
    if f >= 1073741824.0 {
        format!("{:.1} GiB", f / 1073741824.0)
    } else if f >= 1048576.0 {
        format!("{:.1} MiB", f / 1048576.0)
    } else {
        format!("{:.1} KiB", f / 1024.0)
    }
}

pub fn status(s: &Status, json: bool) -> Result<()> {
    if json {
        println!(
            "{}",
            serde_json::json!({
                "version": s.version, "pid": s.pid, "uptime_s": s.uptime_s, "store": s.store_root, "disk_bytes": s.disk_bytes,
                "records": s.records, "segments": s.segments, "blocks": s.blocks, "wal_records": s.wal_records,
                "ingest_rate": s.ingest_rate, "segments_unindexed": s.segments_unindexed, "config": s.config_path,
                "config_error": s.config_error, "warnings": s.warnings,
                "store_origin": s.store_origin, "segment_writers": s.segment_writers,
            })
        );
        return Ok(());
    }
    println!("wartlogd {} (pid {}), up {}", s.version, s.pid, time::format_duration(s.uptime_s as i64 * time::NS));
    println!("store      {}  {} on disk", s.store_root, bytes(s.disk_bytes));
    println!("records    {}  ({} not yet sealed), {} segments, {} blocks", s.records, s.wal_records, s.segments, s.blocks);
    if !s.segment_writers.is_empty() {
        let w: Vec<String> = s.segment_writers.iter().map(|(k, n)| format!("{n} {k}")).collect();
        println!("writers    {} store; segments written by: {}", s.store_origin, w.join(", "));
    }
    println!("ingest     {:.1} records/s (last 10 s)", s.ingest_rate);
    // the newest segment is the open one while the daemon runs; it is indexed when it closes
    let closed = s.segments.saturating_sub(1).max(s.segments_indexed);
    println!("indexes    {} of {} closed segments indexed (the open segment is indexed when it closes)", s.segments_indexed, closed);
    println!("config     {}", s.config_path);
    if let Some(e) = &s.config_error {
        println!("           NOT RELOADED: {e}");
    }
    for w in &s.warnings {
        println!("warning    {w}");
    }
    sources(&s.sources, &[], false).map(|_| ())
}
