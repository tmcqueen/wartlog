//! Drives the `wl` binary against an in-process daemon.

use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

struct Env {
    dir: PathBuf,
}

impl Env {
    fn wl(&self, args: &[&str]) -> (bool, String, String) {
        let o = Command::new(env!("CARGO_BIN_EXE_wl"))
            .args(["--store", self.dir.join("store").to_str().unwrap(), "--socket", self.dir.join("api.sock").to_str().unwrap()])
            .args(args)
            .env("NO_COLOR", "1")
            .env("TZ", "UTC")
            .output()
            .unwrap();
        (o.status.success(), String::from_utf8_lossy(&o.stdout).to_string(), String::from_utf8_lossy(&o.stderr).to_string())
    }

    fn ok(&self, args: &[&str]) -> String {
        let (ok, out, err) = self.wl(args);
        assert!(ok, "wl {args:?} failed: {err}");
        out
    }

    fn send(&self, source: &str, input: &str) {
        let mut c = Command::new(env!("CARGO_BIN_EXE_wl"))
            .args(["send", "--source", source, "--ingest-socket", self.dir.join("ingest.sock").to_str().unwrap()])
            .stdin(Stdio::piped())
            .spawn()
            .unwrap();
        c.stdin.take().unwrap().write_all(input.as_bytes()).unwrap();
        assert!(c.wait().unwrap().success());
    }

    fn wait_count(&self, kql: &str, n: usize) {
        for _ in 0..200 {
            let out = self.ok(&["query", &format!("{kql} | count")]);
            if out.lines().nth(1).and_then(|l| l.trim().parse::<usize>().ok()).unwrap_or(0) >= n {
                return;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        panic!("{kql} never reached {n}");
    }
}

fn config(dir: &Path) -> String {
    format!("[storage]\nmax_age = \"none\"\n\n[[receiver]]\ntype = \"ndjson-unix\"\npath = \"{}\"\n", dir.join("ingest.sock").display())
}

#[test]
fn end_to_end() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().to_path_buf();
    std::fs::write(dir.join("wartlog.toml"), config(&dir)).unwrap();
    let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().unwrap();
    let (stop, rx) = tokio::sync::watch::channel(false);
    let opts = wartlogd::RunOptions { config_path: dir.join("wartlog.toml"), store: Some(dir.join("store")), api_socket: Some(dir.join("api.sock")), ..Default::default() };
    let daemon = rt.spawn(wartlogd::run(opts, rx));
    for _ in 0..200 {
        if dir.join("ingest.sock").exists() && dir.join("api.sock").exists() {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let e = Env { dir: dir.clone() };

    // tail first, so it sees the sends
    let mut tail = Command::new(env!("CARGO_BIN_EXE_wl"))
        .args(["--socket", dir.join("api.sock").to_str().unwrap(), "tail", "logs | where Level == \"error\"", "-n", "0"])
        .env("NO_COLOR", "1")
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    std::thread::sleep(Duration::from_millis(300));

    let mut input = String::new();
    for i in 0..30 {
        input.push_str(&format!("{{\"level\":\"{}\",\"@mt\":\"Order {{OrderId}} shipped\",\"OrderId\":{i}}}\n", if i == 7 { "error" } else { "info" }));
    }
    input.push_str("plain ERROR line from a script\n");
    e.send("shop", &input);
    e.wait_count("logs", 31);

    let out = e.ok(&["query", "logs | where OrderId == 7"]);
    assert!(out.contains("error shop  Order 7 shipped"), "{out}");
    let id: u64 = out.trim().rsplit('#').next().unwrap().parse().unwrap();

    let out = e.ok(&["query", "logs | summarize n = count() by Level | sort by Level asc"]);
    assert!(out.starts_with("Level"), "{out}");
    assert!(out.contains("info   29"), "{out}");

    let out = e.ok(&["query", "logs | take 2", "--json"]);
    let v: serde_json::Value = serde_json::from_str(out.lines().next().unwrap()).unwrap();
    assert_eq!(v["Source"], "shop");
    assert!(v["_id"].is_u64());

    let (ok, _, err) = e.wl(&["query", "logs | where Body = 1"]);
    assert!(!ok);
    assert!(err.contains('^') && err.contains("=="), "{err}");

    let out = e.ok(&["show", &id.to_string()]);
    assert!(out.contains("Template     Order {OrderId} shipped") && out.contains("OrderId = 7"), "{out}");
    let out = e.ok(&["context", &id.to_string(), "-B", "2", "-A", "2"]);
    assert_eq!(out.lines().count(), 5, "{out}");
    assert!(out.lines().nth(2).unwrap().starts_with("▶"), "{out}");

    let out = e.ok(&["templates"]);
    assert!(out.lines().next().unwrap().contains("30") && out.contains("Order {OrderId} shipped"), "{out}");

    let out = e.ok(&["status"]);
    assert!(out.contains("records    31"), "{out}");
    let out = e.ok(&["sources"]);
    assert!(out.contains("ndjson-unix") && out.contains("shop"), "{out}");

    // the tail printed both errors
    let mut rd = std::io::BufReader::new(tail.stdout.take().unwrap());
    let mut got = Vec::new();
    for _ in 0..2 {
        let mut l = String::new();
        rd.read_line(&mut l).unwrap();
        got.push(l);
    }
    tail.kill().unwrap();
    let _ = tail.wait();
    assert!(got[0].contains("Order 7 shipped"), "{got:?}");
    assert!(got[1].contains("plain ERROR line"), "{got:?}");

    e.ok(&["index", "rebuild"]);
    e.ok(&["index", "verify"]);

    stop.send(true).unwrap();
    rt.block_on(daemon).unwrap().unwrap();
    // without the daemon, queries still work and sources fall back to the data
    let out = e.ok(&["query", "logs | count"]);
    assert!(out.contains("31"), "{out}");
    let (ok, out, err) = e.wl(&["sources"]);
    assert!(ok && out.contains("shop"), "{out}{err}");
    let (ok, _, err) = e.wl(&["status"]);
    assert!(!ok && err.contains("is it running"), "{err}");
}
