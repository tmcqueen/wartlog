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

mod ownership {
    use super::*;
    use wartlog_store::{Manifest, Origin, StoreError, WriteMode, WriterKind};

    fn mode(m: WriteMode) -> WriterOptions {
        WriterOptions { mode: m, ..opts() }
    }

    fn err_of(r: anyhow::Result<Writer>) -> StoreError {
        let e = r.err().expect("open should fail");
        match StoreError::find(&e) {
            Some(StoreError::Locked(p)) => StoreError::Locked(p.clone()),
            Some(StoreError::DaemonStore(p)) => StoreError::DaemonStore(p.clone()),
            Some(StoreError::ExternalStore(p)) => StoreError::ExternalStore(p.clone()),
            Some(StoreError::NotFound(p)) => StoreError::NotFound(p.clone()),
            Some(StoreError::NewerFormat(v)) => StoreError::NewerFormat(v.clone()),
            None => panic!("not a StoreError: {e:#}"),
        }
    }

    #[test]
    fn daemon_creates_a_daemon_store_and_marks_segments() {
        let d = tempfile::tempdir().unwrap();
        let mut w = Writer::open(d.path(), opts()).unwrap();
        w.push(rec(1)).unwrap();
        w.close().unwrap();
        let m = Manifest::load(d.path()).unwrap().unwrap();
        assert_eq!(m.origin, Origin::Daemon);
        assert!(m.creator.starts_with("wartlog-store"));
        let s = Snapshot::open(d.path()).unwrap();
        assert_eq!(s.segments[0].header.writer, WriterKind::Daemon);
    }

    #[test]
    fn private_writer_owns_an_external_store() {
        let d = tempfile::tempdir().unwrap();
        let root = d.path().join("app.wartlogx");
        let mut w = Writer::open(&root, mode(WriteMode::Private)).unwrap();
        w.push(rec(1)).unwrap();
        w.close().unwrap();
        assert_eq!(Manifest::load(&root).unwrap().unwrap().origin, Origin::External);
        assert_eq!(Snapshot::open(&root).unwrap().segments[0].header.writer, WriterKind::Private);
        // reopening privately is fine; the daemon refuses it until adopted
        Writer::open(&root, mode(WriteMode::Private)).unwrap().close().unwrap();
        assert!(matches!(err_of(Writer::open(&root, opts())), StoreError::ExternalStore(_)));
        Writer::open(&root, WriterOptions { adopt: true, ..opts() }).unwrap().close().unwrap();
        assert_eq!(Manifest::load(&root).unwrap().unwrap().origin, Origin::Daemon);
        // now it is a daemon store: private is refused
        assert!(matches!(err_of(Writer::open(&root, mode(WriteMode::Private))), StoreError::DaemonStore(_)));
    }

    #[test]
    fn private_refuses_a_store_without_manifest() {
        // every store made before manifests existed is a daemon store
        let d = tempfile::tempdir().unwrap();
        let mut w = Writer::open(d.path(), opts()).unwrap();
        w.push(rec(1)).unwrap();
        w.close().unwrap();
        std::fs::remove_file(wartlog_store::manifest::manifest_path(d.path())).unwrap();
        assert!(matches!(err_of(Writer::open(d.path(), mode(WriteMode::Private))), StoreError::DaemonStore(_)));
        // the daemon opens it as before and does not need a manifest
        Writer::open(d.path(), opts()).unwrap().close().unwrap();
    }

    #[test]
    fn private_on_a_locked_daemon_store_says_daemon_store() {
        let d = tempfile::tempdir().unwrap();
        let _daemon = Writer::open(d.path(), opts()).unwrap();
        assert!(matches!(err_of(Writer::open(d.path(), mode(WriteMode::Private))), StoreError::DaemonStore(_)));
        assert!(matches!(err_of(Writer::open(d.path(), mode(WriteMode::Exclusive))), StoreError::Locked(_)));
    }

    #[test]
    fn exclusive_writes_any_existing_store_and_never_creates_one() {
        let d = tempfile::tempdir().unwrap();
        assert!(matches!(err_of(Writer::open(&d.path().join("nope"), mode(WriteMode::Exclusive))), StoreError::NotFound(_)));
        assert!(!d.path().join("nope").exists());
        assert!(matches!(err_of(Writer::open(d.path(), mode(WriteMode::Exclusive))), StoreError::NotFound(_)));

        let mut w = Writer::open(d.path(), opts()).unwrap();
        w.push(rec(1)).unwrap();
        w.close().unwrap();
        let mut w = Writer::open(d.path(), mode(WriteMode::Exclusive)).unwrap();
        assert_eq!(w.push(rec(2)).unwrap(), 2);
        w.close().unwrap();
        // the manifest still says daemon; the new segment says exclusive; the daemon reads both
        assert_eq!(Manifest::load(d.path()).unwrap().unwrap().origin, Origin::Daemon);
        let s = Snapshot::open(d.path()).unwrap();
        let kinds: Vec<_> = s.segments.iter().map(|h| h.header.writer).collect();
        assert_eq!(kinds, vec![WriterKind::Daemon, WriterKind::Exclusive]);
        let mut w = Writer::open(d.path(), opts()).unwrap();
        assert_eq!(w.push(rec(3)).unwrap(), 3);
        w.close().unwrap();
    }

    #[test]
    fn newer_store_version_is_refused_by_readers_and_writers() {
        let d = tempfile::tempdir().unwrap();
        Writer::open(d.path(), opts()).unwrap().close().unwrap();
        let mut m = Manifest::load(d.path()).unwrap().unwrap();
        m.store_version = wartlog_store::manifest::STORE_VERSION + 1;
        m.save(d.path()).unwrap();
        assert!(matches!(err_of(Writer::open(d.path(), opts())), StoreError::NewerFormat(_)));
        let e = Snapshot::open(d.path()).err().unwrap();
        assert!(matches!(StoreError::find(&e), Some(StoreError::NewerFormat(_))));
    }

    #[test]
    fn newer_segment_version_stops_the_writer() {
        let d = tempfile::tempdir().unwrap();
        let mut w = Writer::open(d.path(), opts()).unwrap();
        w.push(rec(1)).unwrap();
        w.close().unwrap();
        let p = wartlog_store::list_segments(d.path()).pop().unwrap();
        let mut b = std::fs::read(&p).unwrap();
        b[8] = 2;
        let crc = crc32c::crc32c(&b[0..60]);
        b[60..64].copy_from_slice(&crc.to_le_bytes());
        std::fs::write(&p, &b).unwrap();
        assert!(matches!(err_of(Writer::open(d.path(), opts())), StoreError::NewerFormat(_)));
        // a reader reports the segment and carries on
        let s = Snapshot::open(d.path()).unwrap();
        assert_eq!(s.errors.len(), 1);
        assert!(s.errors[0].1.contains("unsupported segment version 2"), "{}", s.errors[0].1);
    }

    #[test]
    fn adopt_command_turns_an_external_store_into_a_daemon_store() {
        let d = tempfile::tempdir().unwrap();
        Writer::open(d.path(), mode(WriteMode::Private)).unwrap().close().unwrap();
        {
            let _w = Writer::open(d.path(), mode(WriteMode::Private)).unwrap();
            assert!(wartlog_store::manifest::adopt(d.path()).is_err(), "adopt must wait for the lock");
        }
        assert_eq!(wartlog_store::manifest::adopt(d.path()).unwrap().origin, Origin::Daemon);
        Writer::open(d.path(), opts()).unwrap().close().unwrap();
    }
}

mod verify {
    use super::*;
    use wartlog_store::verify::{verify_store, Finding, Options};

    fn two_segments(root: &std::path::Path) -> Vec<std::path::PathBuf> {
        for chunk in 0..2u64 {
            let mut w = Writer::open(root, opts()).unwrap();
            for i in 0..200 {
                w.push(rec(chunk * 200 + i)).unwrap();
            }
            w.close().unwrap();
        }
        wartlog_store::list_segments(root)
    }

    #[test]
    fn clean_store_passes() {
        let d = tempfile::tempdir().unwrap();
        two_segments(d.path());
        // plus unsealed WAL records from a writer that has not closed
        let mut w = Writer::open(d.path(), opts()).unwrap();
        w.push(rec(1)).unwrap();
        w.sync().unwrap();
        let r = verify_store(d.path(), &Options::default());
        assert!(r.ok(), "{:?}", r.findings);
        assert_eq!((r.segments, r.records, r.wal_records), (2, 400, 1));
        assert_eq!(r.writers, vec![(wartlog_store::WriterKind::Daemon, 2)]);
        let q = verify_store(d.path(), &Options { full: false, ..Default::default() });
        assert!(q.ok() && q.records == 0);
    }

    #[test]
    fn copied_segment_is_duplicate_and_backwards() {
        let d = tempfile::tempdir().unwrap();
        let segs = two_segments(d.path());
        // a copy of the first segment under a later name: same seqs again, after higher ones
        let copy = segs[1].with_file_name("ffffffffffffffffffffffffffffffff.wseg");
        std::fs::copy(&segs[0], &copy).unwrap();
        let r = verify_store(d.path(), &Options::default());
        assert!(r.findings.iter().any(|f| matches!(f, Finding::DuplicateSeq { .. })), "{:?}", r.findings);
        assert!(r.findings.iter().any(|f| matches!(f, Finding::SeqBackwards { .. })));
        assert!(r.findings.iter().any(|f| matches!(f, Finding::SealedAboveNewest { .. })));
        // metadata-only mode still sees the order problems, not the duplicates
        let q = verify_store(d.path(), &Options { full: false, ..Default::default() });
        assert!(q.findings.iter().all(|f| !matches!(f, Finding::DuplicateSeq { .. })));
        assert!(!q.ok());
        assert!(r.findings.iter().all(|f| !f.describe().is_empty()));
    }

    #[test]
    fn swapped_segments_seal_above_newest() {
        let d = tempfile::tempdir().unwrap();
        let segs = two_segments(d.path());
        // swap contents: the older name now holds the higher seqs
        let a = std::fs::read(&segs[0]).unwrap();
        let b = std::fs::read(&segs[1]).unwrap();
        for s in &segs {
            let _ = std::fs::remove_file(tix::tix_path(s));
        }
        std::fs::write(&segs[0], b).unwrap();
        std::fs::write(&segs[1], a).unwrap();
        let r = verify_store(d.path(), &Options::default());
        let f = r.findings.iter().find(|f| matches!(f, Finding::SealedAboveNewest { .. })).expect("finding");
        if let Finding::SealedAboveNewest { max_seq, newest_max_seq, .. } = f {
            assert_eq!((*max_seq, *newest_max_seq), (400, 200));
        }
    }

    #[test]
    fn hidden_wal_record_is_reported() {
        let d = tempfile::tempdir().unwrap();
        two_segments(d.path());
        // a WAL record whose seq counts as sealed (below the older segment's max) but is in no segment
        let mut wal = wartlog_store::wal::WalFile::create(d.path(), 1).unwrap();
        let mut r = rec(7);
        r.seq = 1_000_000;
        wal.append(&r).unwrap();
        let mut r = rec(8);
        r.seq = 5; // present in a segment: fine
        wal.append(&r).unwrap();
        wal.sync().unwrap();
        let rep = verify_store(d.path(), &Options::default());
        assert!(rep.ok(), "seq above everything is simply unsealed: {:?}", rep.findings);
        let mut r = rec(9);
        r.seq = 0;
        wal.append(&r).unwrap();
        wal.sync().unwrap();
        let rep = verify_store(d.path(), &Options::default());
        assert_eq!(rep.findings, vec![Finding::HiddenWalRecord { seq: 0 }]);
    }
}
