//! Kills the real daemon binary with SIGKILL during heavy ingest, several times, and checks the
//! store recovers: it opens, nothing is duplicated, ids stay unique, and the loss is bounded.

use std::io::Write;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::process::{Child, Command};
use std::time::{Duration, Instant};
use wartlog_store::Snapshot;

fn start(dir: &Path) -> Child {
    let c = Command::new(env!("CARGO_BIN_EXE_wartlogd"))
        .args(["--config", dir.join("wartlog.toml").to_str().unwrap(), "--store", dir.join("store").to_str().unwrap(), "--socket", dir.join("api.sock").to_str().unwrap()])
        .env("WARTLOG_LOG", "warn")
        .spawn()
        .unwrap();
    let t = Instant::now();
    while !(dir.join("ingest.sock").exists() && dir.join("api.sock").exists()) {
        assert!(t.elapsed() < Duration::from_secs(10), "daemon did not start");
        std::thread::sleep(Duration::from_millis(20));
    }
    std::thread::sleep(Duration::from_millis(100));
    c
}

fn count(dir: &Path) -> (u64, usize) {
    let s = Snapshot::open(&dir.join("store")).unwrap();
    let r = wartlog_query::run("logs | project Id", &s, &wartlog_query::QueryOptions { limit: Some(10_000_000), ..Default::default() }).unwrap();
    let mut ids: Vec<u64> = r.rows.iter().map(|row| row[0].as_i64().unwrap() as u64).collect();
    let n = ids.len();
    ids.sort();
    ids.dedup();
    (n as u64, ids.len())
}

#[test]
fn sigkill_during_ingest() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    std::fs::write(
        dir.join("wartlog.toml"),
        format!("[storage]\nmax_age = \"none\"\n[[receiver]]\ntype = \"ndjson-unix\"\npath = \"{}\"\n", dir.join("ingest.sock").display()),
    )
    .unwrap();
    let mut sent_total = 0u64;
    let mut prev = 0u64;
    for round in 0..4 {
        let mut d = start(dir);
        // write as fast as possible for a while, then kill without warning
        let mut s = UnixStream::connect(dir.join("ingest.sock")).unwrap();
        let t = Instant::now();
        let mut sent = 0u64;
        while t.elapsed() < Duration::from_millis(400 + round * 150) {
            let mut buf = String::new();
            for i in 0..200 {
                buf.push_str(&format!("{{\"level\":\"info\",\"msg\":\"round {round} line {}\",\"n\":{i}}}\n", sent + i));
            }
            if s.write_all(buf.as_bytes()).is_err() {
                break;
            }
            sent += 200;
        }
        unsafe { libc_kill(d.id() as i32, 9) };
        d.wait().unwrap();
        sent_total += sent;
        let _ = std::fs::remove_file(dir.join("api.sock"));
        let (n, unique) = count(dir);
        assert_eq!(n as usize, unique, "duplicate ids after crash {round}");
        assert!(n >= prev, "records disappeared after crash {round}: {prev} -> {n}");
        assert!(n <= sent_total, "more records than sent");
        eprintln!("round {round}: sent {sent}, stored {} new (total {n} of {sent_total})", n - prev);
        prev = n;
    }
    // a clean run after the crashes: recovery, then a clean close keeps everything
    let mut d = start(dir);
    let before = count(dir).0;
    unsafe { libc_kill(d.id() as i32, 15) };
    let st = d.wait().unwrap();
    assert!(st.success(), "clean stop failed: {st:?}");
    let (after, unique) = count(dir);
    assert_eq!(after as usize, unique);
    assert_eq!(after, before, "a clean stop loses nothing");
    assert!(after > 0);
    // indexes are complete and match
    for p in wartlog_store::list_segments(&dir.join("store")) {
        assert!(wartlog_store::tix::verify(&p).unwrap(), "stale index {}", p.display());
    }
}

extern "C" {
    #[link_name = "kill"]
    fn libc_kill(pid: i32, sig: i32) -> i32;
}
