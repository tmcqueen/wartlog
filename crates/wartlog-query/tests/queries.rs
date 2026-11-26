use std::sync::Arc;
use wartlog_model::{Record, Source, SourceKind, SpanData, Value};
use wartlog_query::{run, QueryOptions, TailFilter};
use wartlog_store::{Snapshot, Writer, WriterOptions};

const T0: i64 = 1_764_000_000_000_000_000; // 2025-11-24T16:00:00Z
const MIN: i64 = 60_000_000_000;

fn store(n: u64) -> (tempfile::TempDir, Snapshot) {
    let d = tempfile::tempdir().unwrap();
    let mut w = Writer::open(d.path(), WriterOptions { background_index: false, block_target: 8 * 1024, retention_max_age: None, retention_max_bytes: None, ..Default::default() }).unwrap();
    let units = ["foo.service", "bar.service", "kwin_wayland.service"];
    for i in 0..n {
        let src = Arc::new(Source::new(SourceKind::Journald, vec![("journal".into(), "user".into()), ("systemd.unit".into(), units[(i % 3) as usize].into())]));
        let sev = match i % 20 {
            0 => 17,
            1..=3 => 13,
            _ => 9,
        };
        let mut r = Record::log(src, T0 + i as i64 * MIN / 10, sev, format!("request {i} handled in {}ms", i % 97));
        r.attrs = vec![("_PID".into(), Value::I64(100 + (i % 3) as i64)), ("code".into(), Value::Str(if i % 20 == 0 { "E_TIMEOUT".into() } else { "OK".into() }))];
        if i == 4242 {
            r.body = "connection reset by zanzibar peer".into();
        }
        w.push(r).unwrap();
    }
    // a templated OTLP service with spans
    let api = Arc::new(Source::new(SourceKind::Otlp, vec![("service.name".into(), "orders-api".into())]));
    for i in 0..200u64 {
        let mut r = Record::log(api.clone(), T0 + i as i64 * MIN, 9, "");
        r.template = Some("GET {Path} returned {Status} in {Elapsed} ms".into());
        r.args = vec![Value::Str(format!("/orders/{i}")), Value::I64(if i % 10 == 0 { 500 } else { 200 }), Value::F64(i as f64 / 2.0)];
        r.trace_id = Some([i as u8; 16]);
        w.push(r).unwrap();
        let mut s = Record::log(api.clone(), T0 + i as i64 * MIN, 9, "");
        s.trace_id = Some([i as u8; 16]);
        s.span_id = Some([1; 8]);
        s.span = Some(SpanData { name: "GET /orders".into(), end_ns: T0 + i as i64 * MIN + (i as i64 + 1) * 1_000_000, kind: 2, status: if i % 10 == 0 { 2 } else { 1 }, ..Default::default() });
        w.push(s).unwrap();
    }
    w.close().unwrap();
    let s = Snapshot::open(d.path()).unwrap();
    (d, s)
}

fn q(s: &Snapshot, kql: &str) -> wartlog_query::QueryResult {
    run(kql, s, &QueryOptions { now: Some(T0 + 1000 * MIN), ..Default::default() }).unwrap_or_else(|e| panic!("{kql}: {e}"))
}

fn count(s: &Snapshot, kql: &str) -> i64 {
    let r = q(s, &format!("{kql} | count"));
    r.rows[0][0].as_i64().unwrap()
}

#[test]
fn filters_and_pruning() {
    let (_d, s) = store(10_000);
    assert_eq!(count(&s, r#"where Unit == "foo.service""#), 3334);
    assert_eq!(count(&s, r#"where Unit == "foo.service" and Level == "error""#), 167);
    assert_eq!(count(&s, r#"where Level in ("warn", "error")"#), 2000);
    assert_eq!(count(&s, r#"where SeverityNumber >= 13"#), 2000);
    assert_eq!(count(&s, r#"where code == "E_TIMEOUT""#), 500);
    assert_eq!(count(&s, r#"where _PID == 101"#), 3333);
    assert_eq!(count(&s, r#"where Attributes["_PID"] in (100, 102)"#), 6667);

    // rare term: the term index prunes nearly every block
    let r = q(&s, r#"where Body has "zanzibar""#);
    assert_eq!(r.rows.len(), 1);
    assert_eq!(r.stats.used_term_index, 1);
    assert!(r.stats.blocks_scanned <= 2, "{:?}", r.stats);
    let body = r.col("Body").unwrap();
    assert_eq!(r.rows[0][body], Value::Str("connection reset by zanzibar peer".into()));
    assert_eq!(r.ids[0], Some(4243));

    // time window
    let a = wartlog_model::time::format_rfc3339(T0 + 100 * MIN);
    let b = wartlog_model::time::format_rfc3339(T0 + 110 * MIN);
    let n = count(&s, &format!("where Timestamp between (datetime({a}) .. datetime({b})) and SourceKind == 'journald'"));
    assert_eq!(n, 101);
    let r = q(&s, &format!("where Timestamp >= datetime({a}) and Timestamp <= datetime({b}) and SourceKind == 'journald'"));
    assert!(r.stats.blocks_pruned > r.stats.blocks_scanned, "{:?}", r.stats);

    // source pruning
    let r = q(&s, r#"where Unit == "kwin_wayland.service" | take 5"#);
    assert_eq!(r.rows.len(), 5);
    assert!(r.stats.blocks_scanned < r.stats.blocks / 2, "{:?}", r.stats);
    // newest first by default
    let ts = r.col("Timestamp").unwrap();
    assert!(wartlog_query::eval::compare(&r.rows[0][ts], &r.rows[4][ts]) == Some(std::cmp::Ordering::Greater));

    // string operators
    assert_eq!(count(&s, r#"where Body contains "dled in 9ms""#), count(&s, r#"where Body endswith "in 9ms""#));
    assert_eq!(count(&s, r#"where Body startswith "request 999 ""#), 1);
    assert_eq!(count(&s, r#"where Body matches regex "^request 12[0-9] ""#), 10);
    assert_eq!(count(&s, r#"where Body !has "request" and SourceKind == "journald""#), 1);
    assert_eq!(count(&s, r#"search "zanzibar""#), 1);
    assert_eq!(count(&s, r#"search "E_TIMEOUT""#), 500);
}

#[test]
fn templates_and_spans() {
    let (_d, s) = store(30);
    let r = q(&s, r#"where Service == "orders-api" and Body has "returned 500" | project Body, Template | take 3"#);
    assert_eq!(r.columns, vec!["Body", "Template"]);
    assert_eq!(r.rows.len(), 3);
    assert_eq!(r.rows[0][0], Value::Str("GET /orders/190 returned 500 in 95.0 ms".into()));
    assert_eq!(r.rows[0][1], Value::Str("GET {Path} returned {Status} in {Elapsed} ms".into()));

    assert_eq!(count(&s, "spans"), 200);
    assert_eq!(count(&s, r#"spans | where Status == "error""#), 20);
    assert_eq!(count(&s, "spans | where Duration > 100ms"), 100);
    let r = q(&s, "spans | summarize n = count(), slowest = max(Duration), avg_d = avg(Duration) by Status | sort by Status asc");
    assert_eq!(r.columns, vec!["Status", "n", "slowest", "avg_d"]);
    assert_eq!(r.rows[0], vec![Value::Str("error".into()), Value::I64(20), Value::Duration(191_000_000), Value::Duration(96_000_000)]);

    let tid = wartlog_model::hex(&[7u8; 16]);
    let r = q(&s, &format!(r#"where TraceId == "{tid}""#));
    assert_eq!(r.rows.len(), 1);
    assert_eq!(r.stats.used_term_index, 1);
    let r = q(&s, &format!(r#"spans | where TraceId == "{tid}" | project Name, Kind, Duration"#));
    assert_eq!(r.rows[0], vec![Value::Str("GET /orders".into()), Value::Str("server".into()), Value::Duration(8_000_000)]);
}

#[test]
fn tabular_operators() {
    let (_d, s) = store(1000);
    let r = q(&s, r#"where SourceKind == "journald" | summarize count() by Unit | sort by count_ desc, Unit asc"#);
    assert_eq!(r.columns, vec!["Unit", "count_"]);
    assert_eq!(r.rows.len(), 3);
    assert_eq!(r.rows[0], vec![Value::Str("foo.service".into()), Value::I64(334)]);

    let r = q(&s, r#"where SourceKind == "journald" | summarize c = count(), e = countif(Level == "error"), p = dcount(_PID) by bin(Timestamp, 10m)"#);
    assert_eq!(r.columns, vec!["bin_Timestamp", "c", "e", "p"]);
    assert_eq!(r.rows.len(), 10);
    assert_eq!(r.rows[0][1], Value::I64(100));
    assert_eq!(r.rows[0][2], Value::I64(5));
    assert_eq!(r.rows[0][3], Value::I64(3));

    let r = q(&s, r#"where SourceKind == "journald" | top 3 by Timestamp asc | project Id, Unit, x = strlen(Body) * 2"#);
    assert_eq!(r.columns, vec!["Id", "Unit", "x"]);
    assert_eq!(r.rows[0][0], Value::I64(1));
    let r = q(&s, r#"where SourceKind == "journald" | distinct Unit, _PID | count"#);
    assert_eq!(r.rows[0][0], Value::I64(3));
    let r = q(&s, r#"where Id == 5 | extend twice = Id * 2, lvl = toupper(Level) | project-away Attributes, Resource, Template"#);
    assert!(r.columns.contains(&"twice".to_string()) && !r.columns.contains(&"Attributes".to_string()));
    let tw = r.col("twice").unwrap();
    assert_eq!(r.rows[0][tw], Value::I64(10));
    let r = q(&s, r#"where Id < 0 | summarize count()"#);
    assert_eq!(r.rows, vec![vec![Value::I64(0)]]);
    let r = q(&s, r#"where Timestamp > ago(10000d) | take 7"#);
    assert_eq!(r.rows.len(), 7);
    // limit option caps unbounded queries
    let r = run("logs", &s, &QueryOptions { limit: Some(10), ..Default::default() }).unwrap();
    assert_eq!(r.rows.len(), 10);
    assert!(r.stats.truncated);
}

#[test]
fn errors_are_clear() {
    let (_d, s) = store(10);
    let e = run("logs | summarize avg()", &s, &QueryOptions::default()).unwrap_err();
    assert!(e.message.contains("argument"), "{}", e.message);
    let e = run("logs | where count() > 1", &s, &QueryOptions::default()).unwrap_err();
    assert!(e.message.contains("summarize"), "{}", e.message);
    assert!(TailFilter::compile("logs | summarize count()", &s).is_err());
}

#[test]
fn tail_filter() {
    let (_d, s) = store(10);
    let f = TailFilter::compile(r#"where Body has "boom" | project Body, n = 1"#, &s).unwrap();
    let src = Arc::new(Source::new(SourceKind::Ndjson, vec![("source.name".into(), "new-one".into())]));
    let r = Arc::new(Record::log(src.clone(), T0, 9, "a boom happened"));
    let (cols, vals) = f.apply(&r).unwrap();
    assert_eq!(cols, vec!["Body", "n"]);
    assert_eq!(vals[1], Value::I64(1));
    assert!(f.apply(&Arc::new(Record::log(src, T0, 9, "quiet"))).is_none());
}

#[test]
fn unindexed_store_gives_same_answers() {
    let (d, s) = store(3000);
    let queries = [r#"where Body has "zanzibar""#, r#"where code == "E_TIMEOUT""#, r#"where Unit == "bar.service" and Level == "warn""#, "spans | where Status == 'error'"];
    let with: Vec<_> = queries.iter().map(|k| count(&s, k)).collect();
    drop(s);
    for p in wartlog_store::list_segments(d.path()) {
        std::fs::remove_file(wartlog_store::tix::tix_path(&p)).unwrap();
    }
    let s = Snapshot::open(d.path()).unwrap();
    let without: Vec<_> = queries.iter().map(|k| count(&s, k)).collect();
    assert_eq!(with, without);
}

/// The streaming sinks (count, summarize, top, sort | take, distinct) give the same answers as
/// collecting every row first and applying the operator afterwards.
#[test]
fn streaming_sinks_match_collect_then_apply() {
    let (_d, s) = store(10_000);
    let all = "take 1000000000";
    let rows = |kql: &str| {
        let r = q(&s, kql);
        (r.columns, r.rows.into_iter().map(|row| row.iter().map(|v| v.to_text()).collect::<Vec<_>>()).collect::<Vec<_>>(), r.ids)
    };
    for tail in [
        "count",
        "summarize n = count(), e = countif(Level == 'error'), p = dcount(_PID), lo = min(Timestamp), hi = max(Timestamp), s = sum(SeverityNumber) by Unit",
        "summarize count() by bin(Timestamp, 10m), Level",
        "summarize count()",
        "top 7 by Unit asc",
        "sort by Unit asc, SeverityNumber desc | take 9",
        "sort by Unit desc | take 3 | project Id, Unit",
    ] {
        for filter in ["", "where SourceKind == 'journald' | ", "where code == 'E_TIMEOUT' | extend u = toupper(Unit) | "] {
            let fast = rows(&format!("logs | {filter}{tail}"));
            let slow = rows(&format!("logs | {filter}{all} | {tail}"));
            assert_eq!(fast, slow, "logs | {filter}{tail}");
        }
    }
    // distinct keeps first-seen order, which differs between the two paths; compare the sets
    let mut a = rows("logs | distinct Unit, _PID").1;
    let mut b = rows(&format!("logs | {all} | distinct Unit, _PID")).1;
    a.sort();
    b.sort();
    assert_eq!(a, b);
    // an unfiltered count comes from block metadata and agrees with a filtered scan
    let meta = q(&s, "logs | count");
    assert!(meta.stats.blocks_from_metadata > 0);
    assert_eq!(meta.rows[0][0], q(&s, "logs | where Id > 0 | count").rows[0][0]);
    assert_eq!(count(&s, "where Timestamp >= datetime(2025-11-24T16:10:00Z)"), count(&s, "where Timestamp >= datetime(2025-11-24T16:10:00Z) and Id > 0"));
    // the row cap bounds a plain sort and says so
    let r = run("logs | sort by Unit asc", &s, &QueryOptions { limit: Some(50), now: Some(T0), ..Default::default() }).unwrap();
    assert_eq!(r.rows.len(), 50);
    assert!(r.stats.truncated);
    // and it now applies to project as well
    let r = run("logs | project Id, Unit", &s, &QueryOptions { limit: Some(50), now: Some(T0), ..Default::default() }).unwrap();
    assert_eq!(r.rows.len(), 50);
    assert!(r.stats.truncated);
}
