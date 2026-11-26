//! The library from a Rust caller's point of view: every writer mode, reads, errors.

use std::sync::Arc;
use std::time::Duration;
use wartlog::{ErrorKind, Record, Snapshot, Source, SourceKind, Value, WriteMode, Writer, WriterOptions};

fn src(name: &str) -> Arc<Source> {
    Arc::new(Source::new(SourceKind::Ndjson, vec![("source.name".into(), name.into())]))
}

fn rec(name: &str, i: i64) -> Record {
    let mut r = Record::log(src(name), 1_700_000_000_000_000_000 + i * 1_000_000, if i % 10 == 0 { 17 } else { 9 }, format!("event {i}"));
    r.attrs = vec![("n".into(), Value::I64(i))];
    r
}

#[test]
fn private_store_round_trip() {
    let d = tempfile::tempdir().unwrap();
    let root = d.path().join("app.wartlogx");
    let mut w = Writer::open(&root, WriterOptions::new(WriteMode::Private)).unwrap();
    for i in 0..100 {
        assert_eq!(w.push(rec("app", i)).unwrap(), i as u64 + 1);
    }
    let (a, b) = w.push_batch((100..110).map(|i| rec("app", i)).collect()).unwrap();
    assert_eq!((a, b), (101, 110));
    let s = w.push_json(r#"{"@t":"2026-11-26T10:00:00Z","@m":"from json","@l":"Error"}"#, "jsonsrc").unwrap();
    assert_eq!(s, 111);
    assert!(w.flush().unwrap() >= 111);

    // readable while the writer is open (records still in the WAL)
    let snap = Snapshot::open(&root).unwrap();
    assert_eq!(snap.record_count(), 111);
    w.close().unwrap();

    let snap = Snapshot::open(&root).unwrap();
    let rows = snap.query("logs | where Level == 'error' | count", &Default::default()).unwrap();
    assert_eq!(rows.columns(), ["Count"]);
    let r: Vec<_> = rows.collect();
    assert_eq!(r[0].values[0], Value::I64(12));
    let rows = snap.query("logs | where n > 104 and Source == 'app' | project n, Id", &Default::default()).unwrap();
    assert_eq!(rows.len(), 5);
    for row in rows {
        // a projected row is no longer one record, so `id` is None; the Id column still is
        let id = row.values[1].as_i64().unwrap() as u64;
        let rec = snap.get(id).unwrap().unwrap();
        assert_eq!(Some(&row.values[0]), rec.attr("n"));
    }
    let whole: Vec<_> = snap.query("logs | where n == 3", &Default::default()).unwrap().collect();
    assert_eq!(whole[0].id, Some(4));
    assert_eq!(snap.get(111).unwrap().unwrap().body, "from json");
    assert_eq!(wartlog::store_manifest(&root).unwrap().unwrap().origin, wartlog::Origin::External);
    let rep = wartlog::verify_store(&root, false, 10).unwrap();
    assert!(rep.ok(), "{:?}", rep.findings);
}

#[test]
fn errors_have_kinds() {
    let d = tempfile::tempdir().unwrap();
    let e = Snapshot::open(d.path().join("missing")).err().unwrap();
    assert_eq!(e.kind(), ErrorKind::NotFound);
    let e = Writer::open(d.path().join("missing"), WriterOptions::new(WriteMode::Exclusive)).err().unwrap();
    assert_eq!(e.kind(), ErrorKind::NotFound);

    let root = d.path().join("x.wartlogx");
    Writer::open(&root, WriterOptions::new(WriteMode::Private)).unwrap().close().unwrap();
    let snap = Snapshot::open(&root).unwrap();
    let e = snap.query("logs | where", &Default::default()).err().unwrap();
    assert_eq!(e.kind(), ErrorKind::Query);
    let e = snap.query("logs | frobnicate", &Default::default()).err().unwrap();
    assert!(e.span().is_some(), "{e}");

    let _held = Writer::open(&root, WriterOptions::new(WriteMode::Private)).unwrap();
    let e = Writer::open(&root, WriterOptions::new(WriteMode::Exclusive)).err().unwrap();
    assert_eq!(e.kind(), ErrorKind::Locked);
    // shared mode on an external store: no daemon can serve it
    let e = Writer::open(&root, WriterOptions::new(WriteMode::Shared)).err().unwrap();
    assert_eq!(e.kind(), ErrorKind::NoDaemon);
}

#[test]
fn dropping_a_private_writer_seals_it() {
    let d = tempfile::tempdir().unwrap();
    {
        let mut w = Writer::open(d.path(), WriterOptions::new(WriteMode::Private)).unwrap();
        w.push(rec("a", 1)).unwrap();
    }
    assert!(wartlog_store::wal::list(d.path()).is_empty(), "drop closes the writer and removes the WAL");
    assert_eq!(Snapshot::open(d.path()).unwrap().record_count(), 1);
}

#[test]
fn background_maintenance_syncs_without_flush() {
    let d = tempfile::tempdir().unwrap();
    let mut o = WriterOptions::new(WriteMode::Private);
    o.sync_every = Duration::from_millis(20);
    let mut w = Writer::open(d.path(), o).unwrap();
    w.push(rec("a", 1)).unwrap();
    std::thread::sleep(Duration::from_millis(200));
    // the WAL was fsynced by the maintenance thread: a reader sees the record
    assert_eq!(Snapshot::open(d.path()).unwrap().record_count(), 1);
    w.close().unwrap();
}

#[test]
fn shared_and_exclusive_on_a_daemon_store() {
    let d = tempfile::tempdir().unwrap();
    let store = d.path().join("store");
    let sock = d.path().join("api.sock");
    std::fs::write(d.path().join("wartlog.toml"), "[storage]\nmax_age = \"none\"\n").unwrap();
    let rt = tokio::runtime::Runtime::new().unwrap();
    let (stop, rx) = tokio::sync::watch::channel(false);
    let opts = wartlogd::RunOptions { config_path: d.path().join("wartlog.toml"), store: Some(store.clone()), api_socket: Some(sock.clone()), ..Default::default() };
    let daemon = rt.spawn(wartlogd::run(opts, rx));
    for _ in 0..300 {
        if sock.exists() {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }

    // private mode refuses the daemon's store; exclusive is locked out while it runs
    assert_eq!(Writer::open(&store, WriterOptions::new(WriteMode::Private)).err().unwrap().kind(), ErrorKind::DaemonStore);
    assert_eq!(Writer::open(&store, WriterOptions::new(WriteMode::Exclusive)).err().unwrap().kind(), ErrorKind::Locked);

    let mut o = WriterOptions::new(WriteMode::Shared);
    o.socket = Some(sock.clone());
    // the daemon on this socket serves `store`, not another directory
    assert_eq!(Writer::open(d.path(), o.clone()).err().unwrap().kind(), ErrorKind::NoDaemon);
    let mut w = Writer::open(&store, o).unwrap();
    assert_eq!(w.push(rec("shared", 1)).unwrap(), 1);
    assert_eq!(w.push_batch(vec![rec("shared", 2), rec("shared", 3)]).unwrap(), (2, 3));
    assert!(w.flush().unwrap() >= 3);
    w.close().unwrap();
    assert_eq!(Snapshot::open(&store).unwrap().record_count(), 3);

    stop.send(true).unwrap();
    rt.block_on(daemon).unwrap().unwrap();

    // daemon stopped: shared fails, exclusive works and continues the seqs
    let mut o = WriterOptions::new(WriteMode::Shared);
    o.socket = Some(sock);
    assert_eq!(Writer::open(&store, o).err().unwrap().kind(), ErrorKind::NoDaemon);
    let mut w = Writer::open(&store, WriterOptions::new(WriteMode::Exclusive)).unwrap();
    assert_eq!(w.push(rec("bulk", 4)).unwrap(), 4);
    w.close().unwrap();
    let snap = Snapshot::open(&store).unwrap();
    assert_eq!(snap.record_count(), 4);
    assert_eq!(wartlog::verify_store(&store, false, 10).unwrap().finding_count, 0);
}
