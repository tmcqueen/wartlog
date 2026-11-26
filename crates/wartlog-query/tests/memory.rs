//! Queries over a store far larger than their result must not hold every record in memory.
//!
//! A counting global allocator records the peak of live heap bytes while each query runs. The
//! store holds about 200 MB of records once decoded; every query here must stay under a fixed
//! bound that does not grow with the store. (One test function, so no other test allocates on
//! another thread while a peak is measured.)

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use wartlog_model::{Record, Source, SourceKind, Value};
use wartlog_query::{run, QueryOptions};
use wartlog_store::{Snapshot, Writer, WriterOptions};

struct Counting;

static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        let p = unsafe { System.alloc(l) };
        if !p.is_null() {
            let now = LIVE.fetch_add(l.size(), Ordering::Relaxed) + l.size();
            PEAK.fetch_max(now, Ordering::Relaxed);
        }
        p
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        unsafe { System.dealloc(p, l) };
        LIVE.fetch_sub(l.size(), Ordering::Relaxed);
    }
    unsafe fn realloc(&self, p: *mut u8, l: Layout, new: usize) -> *mut u8 {
        let q = unsafe { System.realloc(p, l, new) };
        if !q.is_null() {
            if new >= l.size() {
                let now = LIVE.fetch_add(new - l.size(), Ordering::Relaxed) + (new - l.size());
                PEAK.fetch_max(now, Ordering::Relaxed);
            } else {
                LIVE.fetch_sub(l.size() - new, Ordering::Relaxed);
            }
        }
        q
    }
}

#[global_allocator]
static A: Counting = Counting;

const T0: i64 = 1_764_000_000_000_000_000;
const RECORDS: u64 = 120_000;
/// far below the ~200 MB the matching records take once materialized
const BOUND: usize = 48 << 20;

/// Ride-like events: a templated body and a dozen attributes each.
fn store() -> (tempfile::TempDir, Snapshot) {
    let d = tempfile::tempdir().unwrap();
    let mut w = Writer::open(d.path(), WriterOptions { background_index: false, retention_max_age: None, retention_max_bytes: None, ..Default::default() }).unwrap();
    let parks = ["north", "south", "east", "west"];
    let srcs: Vec<Arc<Source>> = parks.iter().map(|p| Arc::new(Source::new(SourceKind::Otlp, vec![("service.name".into(), format!("rides-{p}"))]))).collect();
    for i in 0..RECORDS {
        let sev = if i % 50 == 0 { 17 } else { 9 };
        let mut r = Record::log(srcs[(i % 4) as usize].clone(), T0 + i as i64 * 1_000_000_000, sev, "");
        r.template = Some("guest {Guest} rode {Ride} for {Seconds} s and paid {Fare}".into());
        r.args = vec![Value::Str(format!("guest-{:08}", i * 7919 % 1_000_003)), Value::Str(format!("ride-{}", i % 40)), Value::I64((i % 600) as i64), Value::F64((i % 997) as f64 / 10.0)];
        r.attrs = (0..12).map(|k| (format!("attr_{k}"), Value::Str(format!("value-{k}-{}-{}", i % (k + 3), "x".repeat(24))))).collect();
        r.attrs.push(("fare".into(), Value::F64((i % 997) as f64 / 10.0)));
        r.attrs.push(("guest".into(), Value::Str(format!("guest-{:08}", i * 7919 % 1_000_003))));
        w.push(r).unwrap();
    }
    w.close().unwrap();
    let s = Snapshot::open(d.path()).unwrap();
    (d, s)
}

/// Runs a query and returns (result, peak heap bytes above the level before it).
fn measured(s: &Snapshot, kql: &str, limit: Option<u64>) -> (wartlog_query::QueryResult, usize) {
    let base = LIVE.load(Ordering::Relaxed);
    PEAK.store(base, Ordering::Relaxed);
    let r = run(kql, s, &QueryOptions { threads: Some(4), limit, ..Default::default() }).unwrap_or_else(|e| panic!("{kql}: {e}"));
    let peak = PEAK.load(Ordering::Relaxed) - base;
    (r, peak)
}

#[test]
fn queries_do_not_hold_every_record() {
    let (_d, s) = store();
    assert_eq!(s.record_count(), RECORDS);

    // what collecting every record costs: the bound must be well below it, or this test proves nothing
    let (all, collect_peak) = measured(&s, "logs | take 100000000", None);
    assert_eq!(all.rows.len() as u64, RECORDS);
    drop(all);
    eprintln!("collect all: {} MB", collect_peak >> 20);
    assert!(collect_peak > 3 * BOUND, "materializing every record only took {} MB; make the store bigger", collect_peak >> 20);

    let cases: &[(&str, Option<u64>)] = &[
        ("logs | count", None),
        ("logs | where attr_0 == 'value-0-1-xxxxxxxxxxxxxxxxxxxxxxxx' | count", None),
        ("logs | where Body has 'ride-7' | count", None),
        ("logs | extend f2 = fare * 2 | where f2 > 10 | count", None),
        ("logs | summarize n = count(), f = sum(fare), a = avg(fare), lo = min(fare), hi = max(fare) by Level", None),
        ("logs | summarize count() by bin(Timestamp, 1h), Source", None),
        ("logs | summarize dcount(attr_3) by attr_11", None),
        ("logs | top 10 by fare desc", None),
        ("logs | sort by fare desc | take 10", None),
        ("logs | sort by guest asc", Some(1000)),
        ("logs | distinct Source, attr_2", None),
        ("logs | where fare > 50 | project guest, fare", Some(1000)),
        ("logs | project-away attr_0", Some(1000)),
    ];
    for (kql, limit) in cases {
        let (r, peak) = measured(&s, kql, *limit);
        eprintln!("{:>6} KB  {kql}", peak >> 10);
        assert!(!r.rows.is_empty(), "{kql}: no rows");
        assert!(peak < BOUND, "{kql}: peak {} MB (collecting everything takes {} MB)", peak >> 20, collect_peak >> 20);
    }

    let (r, _) = measured(&s, "logs | count", None);
    assert_eq!(r.rows[0][0].as_i64(), Some(RECORDS as i64));
    assert!(r.stats.blocks_from_metadata > 0, "an unfiltered count should come from block metadata");
}
