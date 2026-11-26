use std::sync::Arc;
use wartlog_api::ops;
use wartlog_api::proto::ContextScope;
use wartlog_model::{hex, Record, Source, SourceKind, SpanData, Value};
use wartlog_store::{Snapshot, Writer, WriterOptions};

const T0: i64 = 1_764_000_000_000_000_000;
const SEC: i64 = 1_000_000_000;

/// 3 journald units logging every second (pids 100..102), plus a traced OTLP service; half the data
/// is left in the WAL so both paths are exercised.
fn store() -> (tempfile::TempDir, Snapshot) {
    let d = tempfile::tempdir().unwrap();
    let opts = WriterOptions { background_index: false, block_target: 4 * 1024, retention_max_age: None, retention_max_bytes: None, ..Default::default() };
    let mut w = Writer::open(d.path(), opts.clone()).unwrap();
    let units = ["foo.service", "bar.service", "baz.service"];
    for i in 0..3000i64 {
        let src = Arc::new(Source::new(SourceKind::Journald, vec![("systemd.unit".into(), units[(i % 3) as usize].into())]));
        let mut r = Record::log(src, T0 + i * SEC, if i % 50 == 0 { 17 } else { 9 }, format!("tick {i}"));
        r.attrs = vec![("thread.id".into(), Value::I64(100 + (i % 2))), ("code".into(), Value::Str(format!("C{}", i % 4)))];
        w.push(r).unwrap();
        if i == 1500 {
            w.close().unwrap();
            w = Writer::open(d.path(), opts.clone()).unwrap();
        }
    }
    let api = Arc::new(Source::new(SourceKind::Otlp, vec![("service.name".into(), "orders".into())]));
    for i in 0..5i64 {
        let mut s = Record::log(api.clone(), T0 + i * SEC, 9, "");
        s.trace_id = Some([7; 16]);
        s.span_id = Some([i as u8 + 1; 8]);
        s.span = Some(SpanData { name: format!("step {i}"), end_ns: T0 + i * SEC + 1000, kind: 1, status: 1, ..Default::default() });
        w.push(s).unwrap();
        let mut l = Record::log(api.clone(), T0 + i * SEC + 10, 9, "");
        l.template = Some("Step {N} done".into());
        l.args = vec![Value::I64(i)];
        l.trace_id = Some([7; 16]);
        w.push(l).unwrap();
    }
    w.sync().unwrap();
    // leave the second half in the WAL: drop without close
    std::mem::forget(w);
    let s = Snapshot::open(d.path()).unwrap();
    (d, s)
}

#[test]
fn context_trace_templates_schema() {
    let (_d, s) = store();
    // id of `tick 2000` (in the WAL half): seqs are 1-based in push order
    let (t, _) = ops::query(&s, r#"logs | where Body == "tick 2000""#, None, None).unwrap();
    let id = t.ids[0].unwrap();
    let (ctx, center) = ops::record_context(&s, id, 3, 2, ContextScope::Source).unwrap();
    let b = ctx.col("Body").unwrap();
    let bodies: Vec<String> = ctx.rows.iter().map(|r| r[b].to_text()).collect();
    // foo.service is every third tick
    assert_eq!(bodies, vec!["tick 1991", "tick 1994", "tick 1997", "tick 2000", "tick 2003", "tick 2006"]);
    assert_eq!(center, Some(3));
    let (ctx, _) = ops::record_context(&s, id, 2, 1, ContextScope::Thread).unwrap();
    let bodies: Vec<String> = ctx.rows.iter().map(|r| r[b].to_text()).collect();
    // same unit and same thread (even ticks): every sixth
    assert_eq!(bodies, vec!["tick 1988", "tick 1994", "tick 2000", "tick 2006"]);
    // context across the segment / WAL boundary
    let (t, _) = ops::query(&s, r#"logs | where Body == "tick 1500""#, None, None).unwrap();
    let (ctx, _) = ops::record_context(&s, t.ids[0].unwrap(), 1, 1, ContextScope::Source).unwrap();
    assert_eq!(ctx.rows.iter().map(|r| r[b].to_text()).collect::<Vec<_>>(), vec!["tick 1497", "tick 1500", "tick 1503"]);
    assert!(ops::record_context(&s, id, 1, 1, ContextScope::Trace).is_err(), "no trace id");

    let (spans, logs) = ops::trace_get(&s, &hex(&[7; 16])).unwrap();
    assert_eq!(spans.rows.len(), 5);
    assert_eq!(logs.rows.len(), 5);
    assert!(ops::trace_get(&s, "nope").is_err());

    let top = ops::templates_top(&s, "logs", None, 3).unwrap();
    assert_eq!(top.len(), 3);
    assert_eq!(top[0].count, 5, "the template collapses its 5 records; each tick body is unique");
    assert_eq!(top[0].template, "Step {N} done");
    assert_eq!(top[0].sparkline.iter().sum::<u64>(), 5);
    let top = ops::templates_top(&s, "logs | where Level == \"error\"", None, 100).unwrap();
    assert_eq!(top.len(), 60);
    assert!(ops::templates_top(&s, "logs | where (", None, 3).is_err());

    let sc = ops::schema(&s, None);
    assert!(sc.sources.iter().any(|x| x.display == "foo.service"));
    let code = sc.keys.iter().find(|k| k.key == "code").expect("code key");
    assert_eq!(code.values.len(), 4);
    assert!(sc.keys.iter().any(|k| k.key == "thread.id"));
    assert!(sc.operators.contains(&"where".to_string()));
}
