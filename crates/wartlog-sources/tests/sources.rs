//! End-to-end tests for receivers and collectors, with a fake writer on the other end of the sink.

use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{mpsc, watch};
use wartlog_model::{Record, Value};
use wartlog_sources::{Batch, Receiver, Sink, SourceStatus};

/// A fake writer: collects records and acks with a running count.
fn fake_writer(cap: usize) -> (Sink, Arc<Mutex<Vec<Record>>>) {
    let (tx, mut rx) = mpsc::channel::<Batch>(cap);
    let got = Arc::new(Mutex::new(Vec::new()));
    let g = got.clone();
    tokio::spawn(async move {
        let mut seq = 0u64;
        while let Some(b) = rx.recv().await {
            let mut g = g.lock().unwrap();
            for mut r in b.records {
                seq += 1;
                r.seq = seq;
                g.push(r);
            }
            if let Some(a) = b.ack {
                let _ = a.send(seq);
            }
        }
    });
    (Sink::new(tx, Arc::new(SourceStatus::default())), got)
}

async fn wait_for(got: &Arc<Mutex<Vec<Record>>>, n: usize) {
    for _ in 0..200 {
        if got.lock().unwrap().len() >= n {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("timed out waiting for {n} records, have {}", got.lock().unwrap().len());
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

async fn wait_listening(addr: &str) {
    for _ in 0..200 {
        if tokio::net::TcpStream::connect(addr).await.is_ok() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("{addr} never listened");
}

/// Minimal HTTP/1.1 POST; returns (status, body).
async fn post(addr: &str, path: &str, ctype: &str, body: &[u8]) -> (u16, String) {
    let mut s = tokio::net::TcpStream::connect(addr).await.unwrap();
    let head = format!("POST {path} HTTP/1.1\r\nHost: x\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len());
    s.write_all(head.as_bytes()).await.unwrap();
    s.write_all(body).await.unwrap();
    let mut out = Vec::new();
    s.read_to_end(&mut out).await.unwrap();
    let text = String::from_utf8_lossy(&out).to_string();
    let status = text.split_whitespace().nth(1).unwrap().parse().unwrap();
    let body = text.split("\r\n\r\n").nth(1).unwrap_or("").to_string();
    (status, body)
}

#[tokio::test]
async fn unix_socket_ndjson() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("run/ingest.sock");
    let (sink, got) = fake_writer(16);
    let (stop_tx, stop) = watch::channel(false);
    let r = Box::new(wartlog_sources::listeners::UnixReceiver { name: "unix".into(), path: path.clone(), source: "unix".into() });
    let h = tokio::spawn(r.run(sink, stop));
    for _ in 0..100 {
        if path.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let mut c = tokio::net::UnixStream::connect(&path).await.unwrap();
    c.write_all(b"{\"level\":\"error\",\"msg\":\"boom\",\"service\":\"svc-a\",\"n\":3}\nplain WARN text line\n\n").await.unwrap();
    // a partial last line without a newline still counts at EOF
    c.write_all(b"{\"@mt\":\"Hello {Name}\",\"Name\":\"Tim\"}").await.unwrap();
    drop(c);
    wait_for(&got, 3).await;
    {
        let g = got.lock().unwrap();
        assert_eq!(g[0].severity, 17);
        assert_eq!(g[0].source.display_name(), "svc-a");
        assert_eq!(g[0].attr("n"), Some(&Value::I64(3)));
        assert_eq!(g[1].severity, 13);
        assert_eq!(g[1].source.display_name(), "unix");
        assert_eq!(g[2].rendered_body(), "Hello Tim");
    }
    stop_tx.send(true).unwrap();
    h.await.unwrap().unwrap();
    assert!(!path.exists(), "socket removed on shutdown");
}

#[tokio::test]
async fn tcp_ndjson() {
    let addr = format!("127.0.0.1:{}", free_port());
    let (sink, got) = fake_writer(16);
    let (_stop_tx, stop) = watch::channel(false);
    let r = Box::new(wartlog_sources::listeners::TcpReceiver { name: "tcp".into(), listen: addr.clone(), source: "tcp".into() });
    tokio::spawn(r.run(sink, stop));
    wait_listening(&addr).await;
    let mut c = tokio::net::TcpStream::connect(&addr).await.unwrap();
    let lines: String = (0..2000).map(|i| format!("{{\"msg\":\"line {i}\"}}\n")).collect();
    c.write_all(lines.as_bytes()).await.unwrap();
    drop(c);
    wait_for(&got, 2000).await;
    let g = got.lock().unwrap();
    assert_eq!(g.len(), 2000);
    assert_eq!(g[1999].body, "line 1999");
}

#[cfg(feature = "otlp")]
#[tokio::test]
async fn http_ndjson_and_otlp_json() {
    let addr = format!("127.0.0.1:{}", free_port());
    let (sink, got) = fake_writer(16);
    let (_stop_tx, stop) = watch::channel(false);
    let r = Box::new(wartlog_sources::http::HttpReceiver {
        name: "http".into(),
        listen: addr.clone(),
        otlp: true,
        ndjson: true,
        default_source: "http".into(),
        forward: Default::default(),
    });
    tokio::spawn(r.run(sink, stop));
    wait_listening(&addr).await;

    let (st, body) = post(&addr, "/ingest?source=batchjob", "application/x-ndjson", b"{\"msg\":\"a\"}\n{\"msg\":\"b\"}\n").await;
    assert_eq!(st, 200, "{body}");
    assert!(body.contains("\"accepted\":2"), "{body}");
    wait_for(&got, 2).await;
    assert_eq!(got.lock().unwrap()[0].source.display_name(), "batchjob");

    // OTLP/HTTP JSON as the spec writes it (hex ids, camelCase, string int64)
    let logs = r#"{"resourceLogs":[{"resource":{"attributes":[{"key":"service.name","value":{"stringValue":"orders"}}]},
      "scopeLogs":[{"scope":{"name":"Orders.Api"},"logRecords":[{"timeUnixNano":"1764136800000000000","severityNumber":13,
      "body":{"stringValue":"Order 7 is late"},"attributes":[{"key":"{OriginalFormat}","value":{"stringValue":"Order {OrderId} is late"}},
      {"key":"OrderId","value":{"intValue":"7"}}],"traceId":"0af7651916cd43dd8448eb211c80319c","spanId":"b7ad6b7169203331"}]}]}]}"#;
    let (st, body) = post(&addr, "/v1/logs", "application/json", logs.as_bytes()).await;
    assert_eq!(st, 200, "{body}");
    wait_for(&got, 3).await;
    {
        let g = got.lock().unwrap();
        let r = &g[2];
        assert_eq!(r.source.display_name(), "orders");
        assert_eq!(r.template.as_deref(), Some("Order {OrderId} is late"));
        assert_eq!(r.rendered_body(), "Order 7 is late");
        assert_eq!(r.severity, 13);
        assert_eq!(r.time_ns, 1_764_136_800_000_000_000);
        assert_eq!(r.trace_id.unwrap()[0], 0x0a);
        assert_eq!(r.attr("otel.scope.name"), Some(&Value::Str("Orders.Api".into())));
    }

    let traces = r#"{"resourceSpans":[{"resource":{"attributes":[{"key":"service.name","value":{"stringValue":"orders"}}]},
      "scopeSpans":[{"spans":[{"traceId":"0af7651916cd43dd8448eb211c80319c","spanId":"b7ad6b7169203331","name":"GET /orders",
      "kind":2,"startTimeUnixNano":"1764136800000000000","endTimeUnixNano":"1764136800250000000","status":{"code":2,"message":"boom"},
      "events":[{"timeUnixNano":"1764136800100000000","name":"exception","attributes":[]}]}]}]}]}"#;
    let (st, body) = post(&addr, "/v1/traces", "application/json", traces.as_bytes()).await;
    assert_eq!(st, 200, "{body}");
    wait_for(&got, 5).await;
    {
        let g = got.lock().unwrap();
        let sp = g[3].span.as_ref().expect("span");
        assert_eq!(sp.end_ns - g[3].time_ns, 250_000_000);
        assert_eq!(sp.status, 2);
        assert_eq!(g[3].severity, 17);
        assert_eq!(g[4].body, "exception");
        assert_eq!(g[4].span_id, g[3].span_id);
    }

    let (st, _) = post(&addr, "/v1/logs", "application/json", b"{not json").await;
    assert_eq!(st, 400);
    let (st, _) = post(&addr, "/v1/metrics", "application/json", b"{}").await;
    assert_eq!(st, 200, "metrics are accepted and dropped");
}

#[cfg(feature = "otlp")]
#[tokio::test]
async fn http_backpressure_is_503() {
    let addr = format!("127.0.0.1:{}", free_port());
    // a writer that never reads: the channel fills after one batch
    let (tx, _rx) = mpsc::channel::<Batch>(1);
    let sink = Sink::new(tx, Arc::new(SourceStatus::default()));
    let (_stop_tx, stop) = watch::channel(false);
    let r = Box::new(wartlog_sources::http::HttpReceiver {
        name: "http".into(),
        listen: addr.clone(),
        otlp: true,
        ndjson: true,
        default_source: "http".into(),
        forward: Default::default(),
    });
    tokio::spawn(r.run(sink, stop));
    wait_listening(&addr).await;
    let (st, _) = post(&addr, "/ingest", "application/x-ndjson", b"{\"msg\":\"a\"}\n").await;
    assert_eq!(st, 200);
    let (st, body) = post(&addr, "/ingest", "application/x-ndjson", b"{\"msg\":\"b\"}\n").await;
    assert_eq!(st, 503, "{body}");
}

#[cfg(feature = "otlp")]
#[tokio::test]
async fn otlp_grpc_and_forwarding() {
    use opentelemetry_proto::tonic::collector::logs::v1::{logs_service_client::LogsServiceClient, ExportLogsServiceRequest};
    use opentelemetry_proto::tonic::common::v1::{any_value, AnyValue, KeyValue};
    use opentelemetry_proto::tonic::logs::v1::{LogRecord, ResourceLogs, ScopeLogs};
    use opentelemetry_proto::tonic::resource::v1::Resource;

    // downstream collector (stands in for the Aspire dashboard)
    let down = format!("127.0.0.1:{}", free_port());
    let (dsink, dgot) = fake_writer(16);
    let (_s1, stop1) = watch::channel(false);
    tokio::spawn(Box::new(wartlog_sources::otlp::GrpcReceiver { name: "down".into(), listen: down.clone(), forward: Default::default() }).run(dsink, stop1));
    wait_listening(&down).await;

    let up = format!("127.0.0.1:{}", free_port());
    let (sink, got) = fake_writer(16);
    let (_s2, stop2) = watch::channel(false);
    let fwd = wartlog_sources::otlp::Forwarder::new(vec![(format!("http://{down}"), Default::default())]);
    tokio::spawn(Box::new(wartlog_sources::otlp::GrpcReceiver { name: "otlp".into(), listen: up.clone(), forward: fwd }).run(sink, stop2));
    wait_listening(&up).await;

    let s = |v: &str| Some(AnyValue { value: Some(any_value::Value::StringValue(v.into())) });
    let req = ExportLogsServiceRequest {
        resource_logs: vec![ResourceLogs {
            resource: Some(Resource { attributes: vec![KeyValue { key: "service.name".into(), value: s("billing") }], ..Default::default() }),
            scope_logs: vec![ScopeLogs {
                log_records: vec![LogRecord { time_unix_nano: 1_764_136_800_000_000_000, severity_number: 17, body: s("card declined"), ..Default::default() }],
                ..Default::default()
            }],
            ..Default::default()
        }],
    };
    let mut c = LogsServiceClient::connect(format!("http://{up}")).await.unwrap();
    c.export(req).await.unwrap();
    wait_for(&got, 1).await;
    wait_for(&dgot, 1).await;
    assert_eq!(got.lock().unwrap()[0].body, "card declined");
    assert_eq!(dgot.lock().unwrap()[0].source.display_name(), "billing");
}

#[cfg(feature = "journald")]
#[test]
fn journald_directory_import_and_resume() {
    use wartlog_sources::journald::{JournalKind, JournaldCollector};
    let tool = ["/lib/systemd/systemd-journal-remote", "/usr/lib/systemd/systemd-journal-remote"].into_iter().find(|p| std::path::Path::new(p).exists());
    let Some(tool) = tool else {
        eprintln!("skipping: systemd-journal-remote not installed");
        return;
    };
    if wartlog_sources::journald::Journal::open(&JournalKind::Directory(std::env::temp_dir())).is_err() {
        eprintln!("skipping: libsystemd not available");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let jdir = dir.path().join("journal");
    std::fs::create_dir_all(&jdir).unwrap();
    let now_us = wartlog_model::time::now_ns() / 1000;
    let export = |from: i64, n: i64, file: &str| {
        let mut s = String::new();
        for i in from..from + n {
            s.push_str(&format!(
                "__REALTIME_TIMESTAMP={}\n__MONOTONIC_TIMESTAMP={i}\n_BOOT_ID=0123456789abcdef0123456789abcdef\n_HOSTNAME=testhost\nPRIORITY={}\n_SYSTEMD_USER_UNIT={}\n_PID=77\nMESSAGE=hello number {i}\n\n",
                now_us - 1000 + i,
                if i % 2 == 0 { 3 } else { 6 },
                if i % 3 == 0 { "noisy.service" } else { "demo.service" }
            ));
        }
        let p = dir.path().join("e.export");
        std::fs::write(&p, s).unwrap();
        let st = std::process::Command::new(tool).arg(format!("--output={}", jdir.join(file).display())).arg(&p).output().unwrap();
        assert!(st.status.success(), "{}", String::from_utf8_lossy(&st.stderr));
    };
    export(1, 30, "a.journal");

    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap();
    let cursors = wartlog_sources::CursorStore::open(dir.path());
    let col = JournaldCollector {
        name: "imp".into(),
        kind: JournalKind::Directory(jdir.clone()),
        backfill: Duration::from_secs(3600),
        include: vec![],
        exclude: vec!["noisy.service".into()],
        follow: false,
    };
    let (sink, got) = rt.block_on(async { fake_writer(16) });
    let (_stop_tx, stop) = watch::channel(false);
    let n = col.run_blocking(&sink, &cursors, &stop, rt.handle()).unwrap();
    assert_eq!(n, 20, "30 entries minus 10 from the excluded unit");
    {
        let g = got.lock().unwrap();
        assert_eq!(g.len(), 20);
        assert_eq!(g[0].body, "hello number 1");
        assert_eq!(g[0].source.display_name(), "demo.service");
        assert_eq!(g[1].severity, 17, "PRIORITY=3 is error");
        assert!(g.windows(2).all(|w| w[0].time_ns <= w[1].time_ns));
    }

    // a second run resumes after the committed cursor: nothing new, then only new entries
    // (the third run reopens the cursor file, proving it was persisted)
    assert_eq!(col.run_blocking(&sink, &cursors, &stop, rt.handle()).unwrap(), 0);
    export(31, 6, "b.journal");
    let n = col.run_blocking(&sink, &wartlog_sources::CursorStore::open(dir.path()), &stop, rt.handle()).unwrap();
    assert_eq!(n, 4, "6 new entries minus 2 excluded");
    assert_eq!(got.lock().unwrap().last().unwrap().body, "hello number 35");
}

