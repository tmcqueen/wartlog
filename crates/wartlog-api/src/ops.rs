//! Operations on a store snapshot, shared by in-process clients and the daemon.

use crate::proto::{ContextScope, KeyInfo, Schema, SeenSource, Table, TemplateCount, TimeRange};
use crate::{kql_attr, kql_literal, kql_string};
use std::collections::{BTreeMap, HashMap};
use wartlog_model::{hex, time::NS, unhex, Value};
use wartlog_query::{QueryError, QueryOptions, QueryStats};
use wartlog_store::Snapshot;

pub fn query(snap: &Snapshot, kql: &str, time_range: Option<TimeRange>, limit: Option<u64>) -> Result<(Table, QueryStats), QueryError> {
    let opts = QueryOptions { limit, time_range, ..Default::default() };
    Ok(Table::from_result(wartlog_query::run(kql, snap, &opts)?))
}

/// The scope filter for `record.context`, as KQL.
fn scope_filter(r: &wartlog_model::Record, same: ContextScope) -> Option<String> {
    let src = format!("Source == {} and SourceKind == {}", kql_string(&r.source.display_name()), kql_string(r.source.kind.name()));
    match same {
        ContextScope::Source => Some(src),
        ContextScope::Thread => {
            let t = r.attr("thread.id").or_else(|| r.attr("_PID")).or_else(|| r.attr("process.pid"))?;
            Some(format!("{src} and {} == {}", kql_attr("thread.id"), kql_literal(t)))
        }
        ContextScope::Trace => r.trace_id.map(|t| format!("TraceId == {}", kql_string(&hex(&t)))),
    }
}

/// Records around `id` from the same source, thread or trace, within an hour either side.
/// Returns the table (oldest first) and the row of `id` in it.
pub fn record_context(snap: &Snapshot, id: u64, before: u32, after: u32, same: ContextScope) -> anyhow::Result<(Table, Option<usize>)> {
    let Some(r) = snap.get(id)? else { anyhow::bail!("no record with id {id}") };
    let Some(filter) = scope_filter(&r, same) else {
        anyhow::bail!(match same {
            ContextScope::Trace => "this record has no trace id",
            _ => "this record has no thread or process id",
        })
    };
    let t = r.time_ns;
    let window = 3600 * NS;
    let run = |kql: String, range: TimeRange| -> anyhow::Result<Table> {
        let opts = QueryOptions { time_range: Some(range), ..Default::default() };
        Ok(Table::from_result(wartlog_query::run(&kql, snap, &opts).map_err(|e| anyhow::anyhow!("{e}"))?).0)
    };
    let mut older = run(format!("logs | where {filter} | sort by Timestamp desc | take {}", before as u64 + 64), (Some(t - window), Some(t)))?;
    let newer = run(format!("logs | where {filter} | sort by Timestamp asc | take {}", after as u64 + 64), (Some(t), Some(t + window)))?;
    // merge, dedupe by id, order by (time, id)
    let ts = older.col("Timestamp").unwrap_or(0);
    let mut rows: Vec<(i64, u64, Vec<Value>)> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for tab in [std::mem::take(&mut older), newer] {
        for (row, id) in tab.rows.into_iter().zip(tab.ids) {
            let Some(id) = id else { continue };
            if seen.insert(id) {
                let tns = match row.get(ts) {
                    Some(Value::Timestamp(x)) => *x,
                    _ => 0,
                };
                rows.push((tns, id, row));
            }
        }
    }
    rows.sort_by_key(|(t, id, _)| (*t, *id));
    let center = rows.iter().position(|(_, i, _)| *i == id);
    let (lo, hi) = match center {
        Some(c) => (c.saturating_sub(before as usize), (c + after as usize + 1).min(rows.len())),
        None => (0, rows.len()),
    };
    let columns = wartlog_query::LOG_COLUMNS.iter().map(|s| s.to_string()).collect();
    let mut table = Table { columns, rows: vec![], ids: vec![] };
    for (_, id, row) in rows.drain(lo..hi) {
        table.rows.push(row);
        table.ids.push(Some(id));
    }
    Ok((table, center.map(|c| c - lo)))
}

/// Spans and logs of one trace, oldest first.
pub fn trace_get(snap: &Snapshot, trace_id: &str) -> anyhow::Result<(Table, Table)> {
    let t = trace_id.trim();
    anyhow::ensure!(unhex(t).is_some_and(|b| b.len() == 16), "a trace id is 32 hex characters");
    let q = |kql: String| -> anyhow::Result<Table> { Ok(Table::from_result(wartlog_query::run(&kql, snap, &QueryOptions::default()).map_err(|e| anyhow::anyhow!("{e}"))?).0) };
    let lit = kql_string(&t.to_ascii_lowercase());
    let spans = q(format!("spans | where TraceId == {lit} | sort by Timestamp asc | take 10000"))?;
    let logs = q(format!("logs | where TraceId == {lit} | sort by Timestamp asc | take 10000"))?;
    Ok((spans, logs))
}

/// Templates (or bodies, for records without one) of the query's results, most frequent first.
pub fn templates_top(snap: &Snapshot, kql: &str, time_range: Option<TimeRange>, limit: u32) -> Result<Vec<TemplateCount>, QueryError> {
    const BUCKETS: usize = 24;
    let base = kql.trim();
    let base = if base.is_empty() { "logs" } else { base };
    // one minute bins are enough to draw 24 buckets for anything longer than half an hour, and
    // keep the group count bounded for long ranges
    let full = format!("{base} | summarize __n = count(), __lo = min(Timestamp), __hi = max(Timestamp) by __t = iff(isempty(Template), Body, Template), __b = bin(Timestamp, 1m)");
    let opts = QueryOptions { time_range, ..Default::default() };
    let r = wartlog_query::run(&full, snap, &opts).map_err(|e| {
        // point errors at the user's part of the text
        if e.end <= base.len() {
            e
        } else {
            QueryError::new(e.message)
        }
    })?;
    let (ti, ni, li, hi, bi) = (r.col("__t"), r.col("__n"), r.col("__lo"), r.col("__hi"), r.col("__b"));
    let (Some(ti), Some(ni), Some(li), Some(hi), Some(bi)) = (ti, ni, li, hi, bi) else { return Ok(vec![]) };
    let ts = |v: &Value| match v {
        Value::Timestamp(t) => *t,
        _ => 0,
    };
    struct Acc {
        count: u64,
        lo: i64,
        hi: i64,
        bins: Vec<(i64, u64)>,
    }
    let mut by: HashMap<String, Acc> = HashMap::new();
    let (mut glo, mut ghi) = (i64::MAX, i64::MIN);
    for row in &r.rows {
        let t = row[ti].to_text();
        let n = row[ni].as_i64().unwrap_or(0) as u64;
        let (lo, hi, b) = (ts(&row[li]), ts(&row[hi]), ts(&row[bi]));
        glo = glo.min(lo);
        ghi = ghi.max(hi);
        let a = by.entry(t).or_insert(Acc { count: 0, lo: i64::MAX, hi: i64::MIN, bins: vec![] });
        a.count += n;
        a.lo = a.lo.min(lo);
        a.hi = a.hi.max(hi);
        a.bins.push((b, n));
    }
    let (rlo, rhi) = match time_range {
        Some((lo, hi)) => (lo.unwrap_or(glo), hi.unwrap_or(ghi)),
        None => (glo, ghi),
    };
    let span = (rhi - rlo).max(1) as f64;
    let mut out: Vec<TemplateCount> = by
        .into_iter()
        .map(|(template, a)| {
            let mut sparkline = vec![0u64; BUCKETS];
            for (b, n) in a.bins {
                let i = (((b - rlo) as f64 / span) * BUCKETS as f64).floor().clamp(0.0, (BUCKETS - 1) as f64) as usize;
                sparkline[i] += n;
            }
            TemplateCount { template, count: a.count, first_ns: a.lo, last_ns: a.hi, sparkline }
        })
        .collect();
    out.sort_by(|a, b| b.count.cmp(&a.count).then_with(|| a.template.cmp(&b.template)));
    out.truncate(limit.max(1) as usize);
    Ok(out)
}

/// Columns, operators, functions, sources seen in the data, and attribute keys with recent values,
/// for completion. Keys and values come from the term indexes of the newest segments, plus a scan of
/// the WAL and of unindexed segments' newest blocks.
pub fn schema(snap: &Snapshot, time_range: Option<TimeRange>) -> Schema {
    const MAX_VALUES: usize = 30;
    let in_range = |lo: i64, hi: i64| match time_range {
        Some((a, b)) => a.is_none_or(|a| hi >= a) && b.is_none_or(|b| lo <= b),
        None => true,
    };
    let mut keys: BTreeMap<String, BTreeMap<String, usize>> = BTreeMap::new();
    let mut note = |k: &str, v: Option<String>, weight: usize| {
        let e = keys.entry(k.to_string()).or_default();
        if let Some(v) = v {
            if e.len() < 4 * MAX_VALUES || e.contains_key(&v) {
                *e.entry(v).or_default() += weight;
            }
        }
    };
    let mut scanned = 0usize;
    for seg in snap.segments.iter().rev().take(16) {
        if !in_range(seg.min_time(), seg.max_time()) {
            continue;
        }
        match &seg.tix {
            Some(tix) => {
                for (term, _) in tix.terms_with_prefix(b"k:", 4096) {
                    let key = String::from_utf8_lossy(&term[2..]).to_string();
                    note(&key, None, 0);
                    let mut p = b"f:".to_vec();
                    p.extend_from_slice(key.as_bytes());
                    p.push(0);
                    for (t, n) in tix.terms_with_prefix(&p, MAX_VALUES) {
                        note(&key, Some(String::from_utf8_lossy(&t[p.len()..]).to_string()), n);
                    }
                }
            }
            None if scanned < 8 => {
                for i in (0..seg.blocks.len()).rev().take(8 - scanned) {
                    scanned += 1;
                    if let Ok(v) = seg.read_block(i) {
                        for r in v.iter().take(2000) {
                            r.for_each_attr(|k, v| note(k, wartlog_store::tix::field_text(&v), 1));
                        }
                    }
                }
            }
            None => {}
        }
    }
    for r in snap.wal.iter().rev().take(5000) {
        for (k, v) in &r.attrs {
            note(k, wartlog_store::tix::field_text(v), 1);
        }
    }
    let keys = keys
        .into_iter()
        .map(|(key, vals)| {
            let mut v: Vec<(String, usize)> = vals.into_iter().collect();
            v.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
            KeyInfo { key, values: v.into_iter().take(MAX_VALUES).map(|x| x.0).collect() }
        })
        .collect();
    let sources = snap
        .catalog
        .sources
        .iter()
        .map(|(fp, s)| SeenSource { fingerprint: *fp, kind: s.kind.name().to_string(), display: s.display_name(), attrs: s.attrs.clone() })
        .collect();
    let s = |x: &[&str]| x.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    Schema {
        tables: s(&["logs", "spans"]),
        log_columns: s(wartlog_query::LOG_COLUMNS),
        span_columns: s(wartlog_query::SPAN_COLUMNS),
        operators: s(wartlog_query::parser::OPERATORS),
        functions: s(wartlog_query::eval::FUNCTIONS),
        aggregates: s(wartlog_query::eval::AGGREGATES),
        sources,
        keys,
    }
}
