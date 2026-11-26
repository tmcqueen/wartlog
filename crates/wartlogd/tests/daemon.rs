//! Runs the daemon in-process against temp dirs and drives it through its sockets.

use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use wartlog_api::proto::{Request, Response};
use wartlog_api::Client;

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

struct D {
    dir: tempfile::TempDir,
    stop: tokio::sync::watch::Sender<bool>,
    h: tokio::task::JoinHandle<anyhow::Result<()>>,
    api: PathBuf,
    ingest: PathBuf,
    http: String,
}

fn config(dir: &Path, http: &str) -> String {
    format!(
        "# test config\n[storage]\nmax_age = \"none\"\n\n[[receiver]]\ntype = \"ndjson-unix\"\npath = \"{}\"\nsource = \"sock\"\n\n[[receiver]]\ntype = \"http\"\nlisten = \"{http}\"\n",
        dir.join("ingest.sock").display()
    )
}

async fn start(dir: tempfile::TempDir) -> D {
    let http = format!("127.0.0.1:{}", free_port());
    let cfg = dir.path().join("wartlog.toml");
    if !cfg.exists() {
        std::fs::write(&cfg, config(dir.path(), &http)).unwrap();
    } else {
        // keep the file, but use a fresh port
        let t = std::fs::read_to_string(&cfg).unwrap();
        let re = t.lines().map(|l| if l.starts_with("listen") { format!("listen = \"{http}\"") } else { l.to_string() }).collect::<Vec<_>>().join("\n");
        std::fs::write(&cfg, re).unwrap();
    }
    let api = dir.path().join("api.sock");
    let opts = wartlogd::RunOptions { config_path: cfg, store: Some(dir.path().join("store")), api_socket: Some(api.clone()), reload_every: Duration::from_millis(100), ..Default::default() };
    let (stop, rx) = tokio::sync::watch::channel(false);
    let h = tokio::spawn(wartlogd::run(opts, rx));
    for _ in 0..300 {
        if Client::connect(&api, "test").await.is_ok() && dir.path().join("ingest.sock").exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let ingest = dir.path().join("ingest.sock");
    D { dir, stop, h, api, ingest, http }
}

async fn count(c: &mut Client, kql: &str) -> i64 {
    let r = c.call(&Request::Query { kql: format!("{kql} | count"), time_range: None, limit: None }).await.unwrap();
    match &r[0] {
        Response::Rows { table } => table.rows[0][0].as_i64().unwrap(),
        other => panic!("{other:?}"),
    }
}

async fn eventually(c: &mut Client, kql: &str, n: i64) {
    for _ in 0..300 {
        if count(c, kql).await >= n {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("`{kql}` never reached {n} (have {})", count(c, kql).await);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ingest_query_tail_sources_restart() {
    let d = start(tempfile::tempdir().unwrap()).await;
    let mut c = Client::connect(&d.api, "test").await.unwrap();

    // a tail opened before the data arrives
    let mut t = Client::connect(&d.api, "tail").await.unwrap();
    t.send(&Request::Tail { kql: "logs | where Level == \"error\"".into(), backfill: 10 }).await.unwrap();
    match t.recv().await.unwrap().unwrap() {
        Response::Rows { table } => assert!(table.rows.is_empty(), "nothing to backfill yet"),
        other => panic!("{other:?}"),
    }

    let mut s = tokio::net::UnixStream::connect(&d.ingest).await.unwrap();
    let mut lines = String::new();
    for i in 0..500 {
        lines.push_str(&format!("{{\"level\":\"{}\",\"msg\":\"job {i} finished\",\"job\":{i}}}\n", if i % 100 == 0 { "error" } else { "info" }));
    }
    s.write_all(lines.as_bytes()).await.unwrap();
    drop(s);
    eventually(&mut c, "logs", 500).await;
    assert_eq!(count(&mut c, "logs | where Source == \"sock\" and job >= 250").await, 250);

    // the tail saw the 5 errors
    let mut tailed = 0;
    while tailed < 5 {
        match tokio::time::timeout(Duration::from_secs(5), t.recv()).await.expect("tail rows").unwrap().unwrap() {
            Response::Rows { table } => {
                let b = table.col("Body").unwrap();
                for r in &table.rows {
                    assert!(r[b].to_text().ends_with("finished"));
                }
                tailed += table.rows.len();
            }
            other => panic!("{other:?}"),
        }
    }
    assert_eq!(tailed, 5);

    // HTTP NDJSON lands too
    let mut h = tokio::net::TcpStream::connect(&d.http).await.unwrap();
    let body = "{\"msg\":\"from curl\"}\n";
    h.write_all(format!("POST /ingest?source=curl HTTP/1.1\r\nHost: x\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
    eventually(&mut c, "logs | where Source == \"curl\"", 1).await;

    // record get and context by id
    let r = c.call(&Request::Query { kql: "logs | where job == 300".into(), time_range: None, limit: None }).await.unwrap();
    let Response::Rows { table } = &r[0] else { panic!() };
    let id = table.ids[0].unwrap();
    match c.call_one(&Request::RecordGet { id }).await.unwrap() {
        Response::Record { record: Some(rec) } => assert_eq!(rec.body, "job 300 finished"),
        other => panic!("{other:?}"),
    }
    match c.call_one(&Request::RecordContext { id, before: 2, after: 2, same: Default::default() }).await.unwrap() {
        Response::Context { table, center } => {
            assert_eq!(table.rows.len(), 5);
            assert_eq!(center, Some(2));
        }
        other => panic!("{other:?}"),
    }
    // query errors carry the span
    match c.call(&Request::Query { kql: "logs | where Body = 1".into(), time_range: None, limit: None }).await.unwrap().pop().unwrap() {
        Response::Error { message, start, .. } => {
            assert!(message.contains("=="), "{message}");
            assert!(start.is_some());
        }
        other => panic!("{other:?}"),
    }

    // status and sources
    let Response::Status { status } = c.call_one(&Request::Status).await.unwrap() else { panic!() };
    assert!(status.records >= 501);
    assert_eq!(status.sources.len(), 2);
    assert!(status.sources.iter().all(|s| s.state == "running"), "{:?}", status.sources);
    let unix = status.sources.iter().find(|s| s.kind == "ndjson-unix").unwrap();
    assert_eq!(unix.received, 500);

    // add a TCP receiver through the API; the config file keeps its comment
    let tcp = format!("127.0.0.1:{}", free_port());
    c.call_one(&Request::SourcesAdd { kind: "receiver".into(), config: format!("type = \"ndjson-tcp\"\nname = \"tcp\"\nlisten = \"{tcp}\"\nsource = \"netcat\"") }).await.unwrap();
    assert!(std::fs::read_to_string(d.dir.path().join("wartlog.toml")).unwrap().starts_with("# test config"));
    let mut ok = false;
    for _ in 0..200 {
        if let Ok(mut s) = tokio::net::TcpStream::connect(&tcp).await {
            s.write_all(b"plain WARN over tcp\n").await.unwrap();
            ok = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(ok, "tcp receiver never started");
    eventually(&mut c, "logs | where Source == \"netcat\" and Level == \"warn\"", 1).await;
    let Response::Sources { configured, seen } = c.call_one(&Request::SourcesList).await.unwrap() else { panic!() };
    assert_eq!(configured.len(), 3);
    assert!(seen.iter().any(|s| s.display == "netcat"));
    c.call_one(&Request::SourcesRemove { name: "tcp".into() }).await.unwrap();
    for _ in 0..200 {
        let Response::Sources { configured, .. } = c.call_one(&Request::SourcesList).await.unwrap() else { panic!() };
        if configured.len() == 2 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    match c.call_one(&Request::IndexRebuild { segment: None }).await.unwrap() {
        Response::Ok { message } => assert!(message.contains("rebuilt"), "{message}"),
        other => panic!("{other:?}"),
    }
    let n = count(&mut c, "logs").await;
    drop(c);
    drop(t);

    // a clean stop, then a restart sees everything
    d.stop.send(true).unwrap();
    d.h.await.unwrap().unwrap();
    assert!(!d.api.exists());
    let d2 = start(d.dir).await;
    let mut c = Client::connect(&d2.api, "test").await.unwrap();
    assert_eq!(count(&mut c, "logs").await, n);
    let Response::Status { status } = c.call_one(&Request::Status).await.unwrap() else { panic!() };
    assert_eq!(status.wal_records, 0, "a clean stop seals everything");
    assert_eq!(status.segments_unindexed, 0);
    d2.stop.send(true).unwrap();
    d2.h.await.unwrap().unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn second_daemon_is_refused() {
    let d = start(tempfile::tempdir().unwrap()).await;
    let opts = wartlogd::RunOptions {
        config_path: d.dir.path().join("wartlog.toml"),
        store: Some(d.dir.path().join("store")),
        api_socket: Some(d.dir.path().join("api2.sock")),
        ..Default::default()
    };
    let (_tx, rx) = tokio::sync::watch::channel(false);
    let e = wartlogd::run(opts, rx).await.unwrap_err().to_string();
    assert!(e.contains("another"), "{e}");
    d.stop.send(true).unwrap();
    d.h.await.unwrap().unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn external_store_is_refused_unless_adopted() {
    let dir = tempfile::tempdir().unwrap();
    let store = dir.path().join("store");
    let w = wartlog_store::Writer::open(&store, wartlog_store::WriterOptions { mode: wartlog_store::WriteMode::Private, ..Default::default() }).unwrap();
    w.close().unwrap();
    std::fs::write(dir.path().join("wartlog.toml"), config(dir.path(), &format!("127.0.0.1:{}", free_port()))).unwrap();
    let opts = |adopt| wartlogd::RunOptions {
        config_path: dir.path().join("wartlog.toml"),
        store: Some(store.clone()),
        api_socket: Some(dir.path().join("api.sock")),
        adopt,
        ..Default::default()
    };
    let (_tx, rx) = tokio::sync::watch::channel(false);
    let e = format!("{:#}", wartlogd::run(opts(false), rx).await.unwrap_err());
    assert!(e.contains("external store"), "{e}");
    let (tx, rx) = tokio::sync::watch::channel(false);
    let h = tokio::spawn(wartlogd::run(opts(true), rx));
    for _ in 0..300 {
        if Client::connect(&dir.path().join("api.sock"), "test").await.is_ok() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let mut c = Client::connect(&dir.path().join("api.sock"), "test").await.unwrap();
    match c.call_one(&Request::Status).await.unwrap() {
        Response::Status { status } => assert_eq!(status.store_origin, "daemon"),
        other => panic!("{other:?}"),
    }
    tx.send(true).unwrap();
    h.await.unwrap().unwrap();
}
