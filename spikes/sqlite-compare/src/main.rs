//! THROWAWAY: SQLite side of the WartLog storage head-to-head.
//! usage: sqlite-compare load DB JSON
//!        sqlite-compare query DB TERM UNIT SINCE_S UNTIL_S
use anyhow::Result;
use rusqlite::{params, Connection};
use serde_json::Value as J;
use std::io::{BufRead, BufReader};
use std::time::Instant;

fn jstr(v: &J) -> Option<String> {
    match v { J::String(s) => Some(s.clone()), J::Null => None, o => Some(o.to_string()) }
}

fn sev(p: u8) -> i64 { match p { 0 => 24, 1 => 23, 2 => 21, 3 => 17, 4 => 13, 5 => 10, 6 => 9, _ => 5 } }

fn load(db: &str, json: &str) -> Result<()> {
    let _ = std::fs::remove_file(db);
    let c = Connection::open(db)?;
    c.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL; PRAGMA page_size=4096;
        CREATE TABLE logs(id INTEGER PRIMARY KEY, time_ns INTEGER NOT NULL, severity INTEGER NOT NULL,
                          unit TEXT, message TEXT, attrs TEXT);")?;
    let t = Instant::now();
    let tx = c.unchecked_transaction()?;
    {
        let mut ins = tx.prepare("INSERT INTO logs(time_ns,severity,unit,message,attrs) VALUES (?,?,?,?,?)")?;
        for line in BufReader::with_capacity(1 << 20, std::fs::File::open(json)?).lines() {
            let v: J = serde_json::from_str(&line?)?;
            let o = v.as_object().unwrap();
            let time: i64 = o.get("__REALTIME_TIMESTAMP").and_then(jstr).and_then(|s| s.parse().ok()).unwrap_or(0);
            let prio: u8 = o.get("PRIORITY").and_then(jstr).and_then(|s| s.parse().ok()).unwrap_or(6);
            let unit = o.get("_SYSTEMD_USER_UNIT").and_then(jstr);
            let msg = o.get("MESSAGE").and_then(jstr);
            let mut rest = serde_json::Map::new();
            for (k, v) in o { if !k.starts_with("__") && k != "MESSAGE" { rest.insert(k.clone(), v.clone()); } }
            ins.execute(params![time * 1000, sev(prio), unit, msg, J::Object(rest).to_string()])?;
        }
    }
    tx.commit()?;
    let rows = t.elapsed().as_secs_f64();
    let t = Instant::now();
    c.execute_batch("CREATE INDEX logs_time ON logs(time_ns); CREATE INDEX logs_unit_time ON logs(unit, time_ns);
                     CREATE INDEX logs_sev_time ON logs(severity, time_ns);")?;
    let idx = t.elapsed().as_secs_f64();
    c.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")?;
    let base = std::fs::metadata(db)?.len();
    let t = Instant::now();
    c.execute_batch("CREATE VIRTUAL TABLE fts USING fts5(message, content='logs', content_rowid='id');
                     INSERT INTO fts(rowid, message) SELECT id, message FROM logs;
                     INSERT INTO fts(fts) VALUES('optimize'); PRAGMA wal_checkpoint(TRUNCATE);")?;
    let fts = t.elapsed().as_secs_f64();
    let full = std::fs::metadata(db)?.len();
    println!("insert_seconds={rows:.2} btree_index_seconds={idx:.2} fts_seconds={fts:.2} db_bytes_without_fts={base} db_bytes_with_fts={full}");
    Ok(())
}

fn time_query(c: &Connection, label: &str, sql: &str, p: &[&dyn rusqlite::ToSql]) -> Result<()> {
    let mut times = Vec::new();
    let mut n = 0;
    for _ in 0..5 {
        let t = Instant::now();
        let mut st = c.prepare_cached(sql)?;
        let mut rows = st.query(p)?;
        n = 0;
        let mut sink = 0usize;
        while let Some(r) = rows.next()? {
            // materialize each row as a JSON line, like the other tools do
            let line = serde_json::json!({"t": r.get::<_, i64>(0)?, "s": r.get::<_, i64>(1)?,
                "u": r.get::<_, Option<String>>(2)?, "m": r.get::<_, Option<String>>(3)?,
                "a": r.get::<_, Option<String>>(4)?}).to_string();
            sink += line.len();
            n += 1;
        }
        std::hint::black_box(sink);
        times.push(t.elapsed().as_secs_f64());
    }
    times.sort_by(|a, b| a.partial_cmp(b).unwrap());
    println!("{label}\t{:.4}\t{n}", times[2]);
    Ok(())
}

fn query(db: &str, term: &str, unit: &str, since: i64, until: i64) -> Result<()> {
    let c = Connection::open(db)?;
    let (a, b) = (since * 1_000_000_000, until * 1_000_000_000);
    let cols = "l.time_ns, l.severity, l.unit, l.message, l.attrs";
    time_query(&c, "has (FTS5 MATCH)", &format!("SELECT {cols} FROM fts JOIN logs l ON l.id = fts.rowid WHERE fts MATCH ?1 ORDER BY l.time_ns"), &[&format!("\"{term}\"")])?;
    time_query(&c, "contains (LIKE scan)", &format!("SELECT {cols} FROM logs l WHERE l.message LIKE ?1 ORDER BY l.time_ns"), &[&format!("%{term}%")])?;
    time_query(&c, "has in time window", &format!("SELECT {cols} FROM fts JOIN logs l ON l.id = fts.rowid WHERE fts MATCH ?1 AND l.time_ns BETWEEN ?2 AND ?3 ORDER BY l.time_ns"), &[&format!("\"{term}\""), &a, &b])?;
    time_query(&c, "unit + time window", &format!("SELECT {cols} FROM logs l WHERE l.unit = ?1 AND l.time_ns BETWEEN ?2 AND ?3 ORDER BY l.time_ns"), &[&unit, &a, &b])?;
    time_query(&c, "unit, whole log", &format!("SELECT {cols} FROM logs l WHERE l.unit = ?1 ORDER BY l.time_ns"), &[&unit])?;
    time_query(&c, "errors and worse", &format!("SELECT {cols} FROM logs l WHERE l.severity >= 17 ORDER BY l.time_ns"), &[])?;
    time_query(&c, "everything in time window", &format!("SELECT {cols} FROM logs l WHERE l.time_ns BETWEEN ?1 AND ?2 ORDER BY l.time_ns"), &[&a, &b])?;
    Ok(())
}

fn main() -> Result<()> {
    let a: Vec<String> = std::env::args().collect();
    match a[1].as_str() {
        "load" => load(&a[2], &a[3]),
        "query" => query(&a[2], &a[3], &a[4], a[5].trim_start_matches('@').parse()?, a[6].trim_start_matches('@').parse()?),
        _ => anyhow::bail!("usage: load DB JSON | query DB TERM UNIT SINCE UNTIL"),
    }
}
