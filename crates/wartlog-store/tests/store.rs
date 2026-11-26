use std::sync::Arc;
use std::time::Duration;
use wartlog_model::{Record, Source, SourceKind, Value};
use wartlog_store::{tix, Snapshot, Writer, WriterOptions};

fn opts() -> WriterOptions {
    WriterOptions { background_index: false, block_target: 16 * 1024, retention_max_age: None, retention_max_bytes: None, ..Default::default() }
}

fn rec(i: u64) -> Record {
    let unit = format!("unit{}.service", i % 4);
    let src = Arc::new(Source::new(SourceKind::Journald, vec![("journal".into(), "user".into()), ("systemd.unit".into(), unit)]));
    let mut r = Record::log(src, 1_700_000_000_000_000_000 + i as i64 * 1_000_000, if i % 50 == 0 { 17 } else { 9 }, format!("message number {i} rare{}", i % 1000));
    r.observed_ns = r.time_ns;
    r.attrs = vec![("_PID".into(), Value::I64(1000 + (i % 3) as i64)), ("req".into(), Value::Str(format!("r{i}")))];
    r
}

fn all_seqs(s: &Snapshot) -> Vec<u64> {
    let mut v: Vec<u64> = s.wal.iter().map(|r| r.seq).collect();
    for h in &s.segments {
        for i in 0..h.blocks.len() {
            let b = h.read_block(i).unwrap();
            v.extend(b.iter().map(|r| r.seq()));
        }
    }
    v.sort_unstable();
    v
}

#[test]
fn write_close_read() {
    let d = tempfile::tempdir().unwrap();
    let mut w = Writer::open(d.path(), opts()).unwrap();
    for i in 0..5000 {
        assert_eq!(w.push(rec(i)).unwrap(), i + 1);
    }
    w.close().unwrap();
    let s = Snapshot::open(d.path()).unwrap();
    assert_eq!(s.segments.len(), 1);
    assert!(s.segments[0].tix.is_some());
    assert!(s.wal.is_empty());
    assert_eq!(all_seqs(&s), (1..=5000).collect::<Vec<_>>());
    // per-source blocks: each block has one source
    assert!(s.segments[0].blocks.iter().all(|b| b.sources.len() == 1));
    let r = s.get(1234).unwrap().unwrap();
    assert_eq!(r.body, "message number 1233 rare233");
    // term index finds the rare term in few blocks
    let t = s.segments[0].tix.as_ref().unwrap();
    let hits = t.postings(&tix::body_term("rare233"));
    assert!(!hits.is_empty() && hits.len() <= 8, "{hits:?}");
    assert!(t.has_key("_PID"));
    assert!(!t.has_key("req"), "req has a distinct value per record and must not be indexed");
    assert!(!t.postings(&tix::field_term("_PID", "1001")).is_empty());
    // reopen and continue: seqs keep increasing, new segment chains to the old one
    let mut w = Writer::open(d.path(), opts()).unwrap();
    assert_eq!(w.push(rec(1)).unwrap(), 5001);
    w.close().unwrap();
    let s = Snapshot::open(d.path()).unwrap();
    assert_eq!(s.segments.len(), 2);
    assert_eq!(s.segments[1].header.prev, s.segments[0].header.id);
}

#[test]
fn only_one_writer() {
    let d = tempfile::tempdir().unwrap();
    let _w = Writer::open(d.path(), opts()).unwrap();
    assert!(Writer::open(d.path(), opts()).is_err());
}

#[test]
fn crash_recovery_from_wal() {
    let d = tempfile::tempdir().unwrap();
    {
        let mut w = Writer::open(d.path(), opts()).unwrap();
        for i in 0..3000 {
            w.push(rec(i)).unwrap();
        }
        w.sync().unwrap();
        // simulate a crash: no close, so open blocks exist only in the WAL
        std::mem::forget(w);
    }
    // a reader sees everything (segments plus WAL) while the "crashed" writer still holds the lock
    let s = Snapshot::open(d.path()).unwrap();
    assert_eq!(all_seqs(&s), (1..=3000).collect::<Vec<_>>());
    drop(s);
    // forget kept the lock file handle open in this process; flock is per open file, so release it
    // by opening the writer from a fresh handle after the lock times out is not possible: instead
    // copy the store, which is what a restarted daemon would see after the old process died
    let d2 = tempfile::tempdir().unwrap();
    copy_dir(d.path(), d2.path());
    let w = Writer::open(d2.path(), opts()).unwrap();
    assert!(w.stats().recovered_from_wal > 0);
    w.close().unwrap();
    let s = Snapshot::open(d2.path()).unwrap();
    assert_eq!(all_seqs(&s), (1..=3000).collect::<Vec<_>>(), "no loss, no duplicates");
    assert!(s.wal.is_empty());
}

#[test]
fn torn_segment_tail_and_damage() {
    let d = tempfile::tempdir().unwrap();
    let mut w = Writer::open(d.path(), opts()).unwrap();
    for i in 0..4000 {
        w.push(rec(i)).unwrap();
    }
    w.seal_all().unwrap();
    std::mem::forget(w);
    let d2 = tempfile::tempdir().unwrap();
    copy_dir(d.path(), d2.path());
    // tear the open segment's tail
    let seg = wartlog_store::list_segments(d2.path()).pop().unwrap();
    let len = std::fs::metadata(&seg).unwrap().len();
    let f = std::fs::OpenOptions::new().write(true).open(&seg).unwrap();
    f.set_len(len - 100).unwrap();
    drop(f);
    let w = Writer::open(d2.path(), opts()).unwrap();
    w.close().unwrap();
    let s = Snapshot::open(d2.path()).unwrap();
    // the torn block's records come back from the WAL
    assert_eq!(all_seqs(&s), (1..=4000).collect::<Vec<_>>());

    // flip bytes in the middle of the first segment: only damaged blocks are lost
    let seg = wartlog_store::list_segments(d2.path()).remove(0);
    let mut b = std::fs::read(&seg).unwrap();
    let mid = b.len() / 2;
    for x in &mut b[mid..mid + 16] {
        *x ^= 0xa5;
    }
    std::fs::write(&seg, &b).unwrap();
    std::fs::remove_file(tix::tix_path(&seg)).unwrap();
    let s = Snapshot::open(d2.path()).unwrap();
    let got = all_seqs_lossy(&s);
    assert!(got.len() > 3000 && got.len() < 4000, "{}", got.len());
}

fn all_seqs_lossy(s: &Snapshot) -> Vec<u64> {
    let mut v = Vec::new();
    for h in &s.segments {
        for i in 0..h.blocks.len() {
            if let Ok(b) = h.read_block(i) {
                v.extend(b.iter().map(|r| r.seq()));
            }
        }
    }
    v
}

#[test]
fn index_rebuild_is_byte_identical() {
    let d = tempfile::tempdir().unwrap();
    let mut w = Writer::open(d.path(), opts()).unwrap();
    for i in 0..3000 {
        w.push(rec(i)).unwrap();
    }
    w.close().unwrap();
    let seg = wartlog_store::list_segments(d.path()).remove(0);
    let before = std::fs::read(tix::tix_path(&seg)).unwrap();
    assert!(tix::verify(&seg).unwrap());
    std::fs::remove_file(tix::tix_path(&seg)).unwrap();
    // no index: the reader scans the segment and sees the same blocks
    let s = Snapshot::open(d.path()).unwrap();
    assert!(s.segments[0].tix.is_none());
    assert_eq!(all_seqs(&s).len(), 3000);
    drop(s);
    wartlog_store::writer::rebuild_indexes(d.path()).unwrap();
    assert_eq!(std::fs::read(tix::tix_path(&seg)).unwrap(), before);
}

#[test]
fn aging_and_retention() {
    let d = tempfile::tempdir().unwrap();
    let mut o = opts();
    o.block_age = Duration::from_millis(0);
    o.segment_target = 8 * 1024;
    o.retention_max_bytes = Some(40 * 1024);
    let mut w = Writer::open(d.path(), o).unwrap();
    for i in 0..20000 {
        w.push(rec(i)).unwrap();
        if i % 100 == 0 {
            w.tick().unwrap();
        }
    }
    w.close().unwrap();
    let s = Snapshot::open(d.path()).unwrap();
    assert!(s.disk_bytes() < 120 * 1024, "{}", s.disk_bytes());
    assert!(w_deleted(&s));
}

fn w_deleted(s: &Snapshot) -> bool {
    // oldest records are gone, newest remain
    let seqs = all_seqs(s);
    seqs.first().copied().unwrap_or(0) > 1 && seqs.last().copied() == Some(20000)
}

#[test]
fn reader_while_writing() {
    let d = tempfile::tempdir().unwrap();
    let root = d.path().to_path_buf();
    let mut o = opts();
    o.segment_target = 64 * 1024;
    o.background_index = true;
    let writer = std::thread::spawn(move || {
        let mut w = Writer::open(&root, o).unwrap();
        for i in 0..30000 {
            w.push(rec(i)).unwrap();
            if i % 500 == 0 {
                w.sync().unwrap();
                w.tick().unwrap();
            }
        }
        w.close().unwrap();
    });
    let mut last = 0;
    while !writer.is_finished() {
        let s = Snapshot::open(d.path()).unwrap();
        let seqs = all_seqs_lossy(&s);
        // no partial records, no duplicates, monotonic visibility
        let mut sorted = seqs.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), seqs.len(), "duplicate records in segments");
        let n = sorted.len() + s.wal.len();
        assert!(n + 600 >= last, "visible count went backwards: {n} < {last}");
        last = n;
    }
    writer.join().unwrap();
    let s = Snapshot::open(d.path()).unwrap();
    assert_eq!(all_seqs(&s), (1..=30000).collect::<Vec<_>>());
}

fn copy_dir(from: &std::path::Path, to: &std::path::Path) {
    for e in std::fs::read_dir(from).unwrap().flatten() {
        let p = e.path();
        let t = to.join(e.file_name());
        if p.is_dir() {
            std::fs::create_dir_all(&t).unwrap();
            copy_dir(&p, &t);
        } else if e.file_name() != "LOCK" {
            std::fs::copy(&p, &t).unwrap();
        }
    }
}
