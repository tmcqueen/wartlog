//! The term index build must not hold every block's terms at once. The first version collected
//! per-block term and cardinality sets for the whole segment before merging them, which took
//! about 2 GB for a 4M-record segment and kept wartlogd at 15 GB after a 117M-record load.
//!
//! A counting global allocator measures the peak heap of one build. (One test in this binary, so
//! nothing else allocates while it measures.)

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use wartlog_model::{Record, Source, SourceKind, Value};
use wartlog_store::{tix, Writer, WriterOptions};

struct Counting;
static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        let p = unsafe { System.alloc(l) };
        if !p.is_null() {
            PEAK.fetch_max(LIVE.fetch_add(l.size(), Ordering::Relaxed) + l.size(), Ordering::Relaxed);
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
                PEAK.fetch_max(LIVE.fetch_add(new - l.size(), Ordering::Relaxed) + (new - l.size()), Ordering::Relaxed);
            } else {
                LIVE.fetch_sub(l.size() - new, Ordering::Relaxed);
            }
        }
        q
    }
}

#[global_allocator]
static A: Counting = Counting;

#[test]
fn index_build_memory_is_bounded() {
    let d = tempfile::tempdir().unwrap();
    let mut w = Writer::open(d.path(), WriterOptions { background_index: false, retention_max_age: None, retention_max_bytes: None, ..Default::default() }).unwrap();
    let src = Arc::new(Source::new(SourceKind::Ndjson, vec![("source.name".into(), "rides".into())]));
    let n = 300_000u64;
    for i in 0..n {
        // a dozen attributes, most of them with many distinct values (abandoned by the cardinality cap)
        let mut r = Record::log(src.clone(), 1_700_000_000_000_000_000 + i as i64 * 1_000_000, 9, format!("guest g{:07} rode ride-{} paid {}", i * 7919 % 1_000_003, i % 40, i % 997));
        r.attrs = (0..12).map(|k| (format!("a{k}"), Value::Str(format!("v{}", i * (k + 1) % (if k < 4 { 50 } else { 1_000_003 }))))).collect();
        w.push(r).unwrap();
    }
    w.seal_all().unwrap();
    let seg = wartlog_store::list_segments(d.path()).remove(0);
    // index the still-open segment's bytes as they are
    let base = LIVE.load(Ordering::Relaxed);
    PEAK.store(base, Ordering::Relaxed);
    let bytes = tix::build(&wartlog_store::segment::Segment::open(&seg).unwrap()).unwrap();
    let peak = PEAK.load(Ordering::Relaxed) - base;
    eprintln!("index {} KB, build peak {} MB", bytes.len() >> 10, peak >> 20);
    // the first version peaked at 65 MB here (and at 2 GB RSS on a 4M-record segment); this one at 33
    assert!(peak < 48 << 20, "term index build peaked at {} MB", peak >> 20);
    drop(w);
}
