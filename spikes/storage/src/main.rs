//! wl-spike: THROWAWAY storage spike for WartLog.
//! Proves the binary format, block index, index rebuild, index-free queries, and gives benchmark numbers.

mod bloom;
mod format;
mod import;
mod query;
mod store;
mod tix;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};

use format::*;
use query::{Mode, Pred};

#[derive(Parser)]
#[command(name = "wl-spike", about = "THROWAWAY WartLog storage spike")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Import `journalctl -o json` lines into a new store
    Import {
        #[arg(long)] data: PathBuf,
        /// input file, or - for stdin
        #[arg(long, default_value = "-")] input: String,
        #[arg(long, default_value = "user")] journal: String,
        /// import the input N times, each copy shifted later in time (needs a file input)
        #[arg(long, default_value_t = 1)] repeat: u32,
        #[arg(long, default_value_t = 256)] block_kib: usize,
        #[arg(long, default_value_t = 256)] segment_mib: u64,
        #[arg(long, default_value_t = 3)] zstd: i32,
        /// one open block per source (stream)
        #[arg(long)] per_source: bool,
        /// seal a block when its event-time span exceeds this many seconds
        #[arg(long, default_value_t = 600)] block_age_s: i64,
        /// build the .wtix term index for each segment
        #[arg(long)] term_index: bool,
    },
    /// Query with flags (KQL comes later)
    Query {
        #[arg(long)] data: PathBuf,
        #[arg(long, value_enum, default_value = "index")] mode: Mode,
        /// @epoch_seconds, epoch seconds, or relative like -2h
        #[arg(long)] since: Option<String>,
        #[arg(long)] until: Option<String>,
        /// exact field match, e.g. _SYSTEMD_USER_UNIT=foo.service (repeatable)
        #[arg(long = "field")] fields: Vec<String>,
        /// whole-term match in MESSAGE, case-insensitive (repeatable)
        #[arg(long)] has: Vec<String>,
        /// substring match in MESSAGE, case-insensitive
        #[arg(long)] contains: Option<String>,
        /// journald priority ceiling, like journalctl -p (0..7)
        #[arg(long)] priority: Option<u8>,
        /// print only the match count
        #[arg(long)] count: bool,
        /// print execution stats to stderr
        #[arg(long)] stats: bool,
    },
    /// Rebuild or verify .widx index files from the segments alone
    Index {
        #[command(subcommand)] op: IndexOp,
    },
    /// Print shell variables naming a test unit, a rare term, an example event and a time window
    Pick { #[arg(long)] data: PathBuf },
    /// Overwrite random bytes inside segments (for damage tests; use on a copy!)
    Corrupt {
        #[arg(long)] data: PathBuf,
        #[arg(long, default_value_t = 3)] hits: u32,
        #[arg(long, default_value_t = 1)] seed: u64,
    },
    /// Store statistics
    Info { #[arg(long)] data: PathBuf },
    /// Generate synthetic journalctl-style JSON lines (for testing without a real journal)
    Synth {
        #[arg(long)] records: u64,
        #[arg(long, default_value_t = 42)] seed: u64,
    },
}

#[derive(Subcommand)]
enum IndexOp {
    /// Delete-and-rebuild every index from segment data
    Rebuild { #[arg(long)] data: PathBuf },
    /// Rebuild each index in memory and check it is byte-identical to the one on disk
    Verify { #[arg(long)] data: PathBuf },
    /// Delete all index files
    Drop { #[arg(long)] data: PathBuf },
    /// Build (or rebuild) the .wtix term index for every segment
    Terms { #[arg(long)] data: PathBuf },
}

fn now_ns() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos() as i64
}

fn parse_time(s: &str) -> Result<i64> {
    let s = s.trim();
    if let Some(r) = s.strip_prefix('-') {
        let (n, unit) = r.split_at(r.len() - 1);
        let n: i64 = n.parse()?;
        let mul = match unit { "s" => 1, "m" => 60, "h" => 3600, "d" => 86400, _ => bail!("bad unit in {s}") };
        return Ok(now_ns() - n * mul * 1_000_000_000);
    }
    let secs: f64 = s.trim_start_matches('@').parse().with_context(|| format!("bad time {s}"))?;
    Ok((secs * 1e9) as i64)
}

fn open_input(input: &str) -> Result<Box<dyn BufRead>> {
    Ok(if input == "-" { Box::new(BufReader::with_capacity(1 << 20, std::io::stdin())) }
       else { Box::new(BufReader::with_capacity(1 << 20, std::fs::File::open(input)?)) })
}

fn dir_size(p: &Path) -> u64 {
    std::fs::read_dir(p).map(|rd| rd.filter_map(|e| e.ok()).map(|e| {
        let m = e.metadata().unwrap();
        if m.is_dir() { dir_size(&e.path()) } else { m.len() }
    }).sum()).unwrap_or(0)
}

fn main() -> Result<()> {
    match Cli::parse().cmd {
        Cmd::Import { data, input, journal, repeat, block_kib, segment_mib, zstd, per_source, block_age_s, term_index } => {
            if store::list_segments(&data)?.len() > 0 { bail!("{} already has segments; use an empty directory", data.display()); }
            if repeat > 1 && input == "-" { bail!("--repeat needs a file --input"); }
            // time span of the input, so repeated copies follow each other
            let mut span_ns = 0i64;
            if repeat > 1 {
                let (mut lo, mut hi) = (i64::MAX, i64::MIN);
                for line in open_input(&input)?.lines() {
                    if let Some(r) = import::parse_line(&line?, &journal, 0, 0, 0)? { lo = lo.min(r.time_ns); hi = hi.max(r.time_ns); }
                }
                span_ns = hi - lo + 1_000_000_000;
            }
            let mut w = store::Writer::create(&data, store::Options {
                block_target: block_kib * 1024, segment_target: segment_mib << 20, zstd_level: zstd,
                per_source, block_age_ns: block_age_s * 1_000_000_000, term_index,
            })?;
            let start = std::time::Instant::now();
            let (mut seq, mut in_bytes, mut bad) = (0u64, 0u64, 0u64);
            let observed = now_ns();
            for copy in 0..repeat {
                for line in open_input(&input)?.lines() {
                    let line = line?;
                    in_bytes += line.len() as u64 + 1;
                    match import::parse_line(&line, &journal, seq, copy as i64 * span_ns, observed) {
                        Ok(Some(r)) => { w.push(&r)?; seq += 1; }
                        _ => bad += 1,
                    }
                }
            }
            let (blocks, bytes) = w.finish()?;
            let secs = start.elapsed().as_secs_f64();
            println!("records={seq} skipped_lines={bad} blocks={blocks} input_bytes={in_bytes} store_bytes={} seconds={secs:.3} records_per_sec={:.0} input_mb_per_sec={:.1} json_to_store_ratio={:.2}",
                dir_size(&data), seq as f64 / secs, in_bytes as f64 / secs / 1e6, in_bytes as f64 / bytes.max(1) as f64);
        }
        Cmd::Query { data, mode, since, until, fields, has, contains, priority, count, stats } => {
            let mut pred = Pred {
                since_ns: since.as_deref().map(parse_time).transpose()?,
                until_ns: until.as_deref().map(parse_time).transpose()?,
                has: has.iter().map(|h| h.to_lowercase()).collect(),
                contains,
                min_severity: priority.map(import::severity_from_priority),
                ..Default::default()
            };
            for f in fields {
                let (k, v) = f.split_once('=').context("--field needs KEY=VALUE")?;
                pred.fields.push((k.to_string(), v.to_string()));
            }
            // journalctl -p N means "N and more severe"; severity numbers grow with severity,
            // but our mapping is coarse, so use the lowest severity number for that priority band.
            if let Some(p) = priority { pred.min_severity = Some(match p { 0 => 24, 1 => 23, 2 => 21, 3 => 17, 4 => 13, 5 => 10, 6 => 9, _ => 0 }); }
            let (res, secs) = query::timed(|| query::run(&data, mode, &pred, !count));
            let (rows, st) = res?;
            if count { println!("{}", st.matches); } else {
                let out = std::io::stdout();
                let mut o = std::io::BufWriter::with_capacity(1 << 20, out.lock());
                for r in rows { o.write_all(r.as_bytes())?; o.write_all(b"\n")?; }
            }
            if stats {
                eprintln!("mode={mode:?} seconds={secs:.4} segments={} term_index_segments={} index_fallbacks={} blocks={} pruned={} decoded={} decoded_without_match={} matches={} damaged={}",
                    st.segments, st.term_index_segments, st.index_fallbacks, st.blocks, st.pruned, st.decoded, st.false_positive_blocks, st.matches, st.damaged);
            }
        }
        Cmd::Index { op } => match op {
            IndexOp::Rebuild { data } => {
                let start = std::time::Instant::now();
                let mut blocks = 0;
                for seg in store::list_segments(&data)? {
                    let _ = std::fs::remove_file(store::idx_path(&seg));
                    let had_tix = tix::tix_path(&seg).exists();
                    let (n, damaged) = store::rebuild_index(&seg)?;
                    if had_tix { tix::write(&seg)?; }
                    blocks += n;
                    for (a, b) in damaged { eprintln!("damaged region in {}: bytes {a}..{b}", seg.display()); }
                }
                println!("rebuilt blocks={blocks} seconds={:.3}", start.elapsed().as_secs_f64());
            }
            IndexOp::Verify { data } => {
                let (mut same, mut diff) = (0, 0);
                for seg in store::list_segments(&data)? {
                    let (id, len, rep) = store::scan_segment(&seg, true)?;
                    let fresh = store::encode_index(&id, len, &rep.blocks);
                    let disk = std::fs::read(store::idx_path(&seg)).unwrap_or_default();
                    if fresh == disk { same += 1 } else { diff += 1; eprintln!("index differs: {}", seg.display()); }
                    if let Ok(disk) = std::fs::read(tix::tix_path(&seg)) {
                        if tix::build(&seg)? == disk { same += 1 } else { diff += 1; eprintln!("term index differs: {}", seg.display()); }
                    }
                }
                println!("indexes_identical={same} indexes_different={diff}");
                if diff > 0 { std::process::exit(1); }
            }
            IndexOp::Drop { data } => {
                let mut n = 0;
                for seg in store::list_segments(&data)? {
                    if std::fs::remove_file(store::idx_path(&seg)).is_ok() { n += 1; }
                    if std::fs::remove_file(tix::tix_path(&seg)).is_ok() { n += 1; }
                }
                println!("dropped={n}");
            }
            IndexOp::Terms { data } => {
                let start = std::time::Instant::now();
                let mut bytes = 0;
                for seg in store::list_segments(&data)? { bytes += tix::write(&seg)?; }
                println!("term_index_bytes={bytes} seconds={:.3}", start.elapsed().as_secs_f64());
            }
        },
        Cmd::Pick { data } => pick(&data)?,
        Cmd::Corrupt { data, hits, seed } => {
            let segs = store::list_segments(&data)?;
            let mut rng = seed.wrapping_mul(0x9E3779B97F4A7C15) | 1;
            let mut next = || { rng ^= rng << 13; rng ^= rng >> 7; rng ^= rng << 17; rng };
            for _ in 0..hits {
                let seg = &segs[(next() % segs.len() as u64) as usize];
                let mut b = std::fs::read(seg)?;
                let pos = SEG_HEADER_LEN + (next() as usize % (b.len() - SEG_HEADER_LEN));
                for i in 0..16.min(b.len() - pos) { b[pos + i] ^= 0xA5; }
                std::fs::write(seg, b)?;
                println!("corrupted 16 bytes at {} offset {pos}", seg.display());
            }
        }
        Cmd::Info { data } => {
            let (mut blocks, mut recs, mut unc, mut comp, mut bloom) = (0u64, 0u64, 0u64, 0u64, 0u64);
            let segs = store::list_segments(&data)?;
            for seg in &segs {
                let (_, _, rep) = store::scan_segment(seg, false)?;
                for b in rep.blocks {
                    blocks += 1; recs += b.record_count as u64; unc += b.uncompressed_len as u64; comp += b.compressed_len as u64;
                    bloom += Summary::decode(&b.summary)?.bloom.byte_len() as u64;
                }
            }
            println!("segments={} blocks={blocks} records={recs} store_bytes={} uncompressed_payload={unc} compressed_payload={comp} zstd_ratio={:.2} avg_records_per_block={:.0} avg_bloom_bytes={:.0} bytes_per_record={:.1}",
                segs.len(), dir_size(&data), unc as f64 / comp.max(1) as f64, recs as f64 / blocks.max(1) as f64,
                bloom as f64 / blocks.max(1) as f64, dir_size(&data) as f64 / recs.max(1) as f64);
        }
        Cmd::Synth { records, seed } => synth(records, seed)?,
    }
    Ok(())
}

fn shq(s: &str) -> String { format!("'{}'", s.replace('\'', "'\\''")) }

fn pick(data: &Path) -> Result<()> {
    let mut units: HashMap<String, u64> = HashMap::new();
    let mut df: HashMap<String, (u32, i64, String)> = HashMap::new();
    let mut times = Vec::new();
    for seg in store::list_segments(data)? {
        let f = std::fs::File::open(&seg)?;
        let (_, _, rep) = store::scan_segment(&seg, false)?;
        for b in rep.blocks {
            let p = store::read_payload(&f, &b)?;
            for r in decode_payload(&p, b.record_count)? {
                times.push(r.time_ns);
                if let Some((_, Value::Str(u))) = r.attrs.iter().find(|(k, _)| k == "_SYSTEMD_USER_UNIT") {
                    *units.entry(u.clone()).or_default() += 1;
                }
                let mut seen = std::collections::HashSet::new();
                for t in tokens(&r.body) {
                    if t.len() >= 6 && t.chars().all(|c| c.is_ascii_alphabetic()) && seen.insert(t.clone()) {
                        let e = df.entry(t).or_insert((0, r.time_ns, r.body.clone()));
                        e.0 += 1;
                    }
                }
            }
        }
    }
    if times.is_empty() { bail!("store is empty"); }
    times.sort_unstable();
    let total = times.len() as f64;
    let target = (total * 0.01).max(1.0);
    let unit = units.iter().min_by(|a, b| {
        ((*a.1 as f64 - target).abs()).partial_cmp(&(*b.1 as f64 - target).abs()).unwrap().then(a.0.cmp(b.0))
    }).map(|(u, n)| (u.clone(), *n));
    let term = df.iter().filter(|(_, v)| v.0 <= 3)
        .min_by_key(|(t, _)| (hash(t.as_bytes()), (*t).clone()))
        .map(|(t, v)| (t.clone(), v.clone()));
    // a window holding ~10% of entries, centred on the example event when there is one
    let centre = term.as_ref().map(|(_, (_, at, _))| times.partition_point(|t| t < at)).unwrap_or(times.len() / 2);
    let half = (total * 0.05) as usize;
    let lo = centre.saturating_sub(half);
    let hi = (centre + half).min(times.len() - 1);
    let since = times[lo] / 1_000_000_000;
    let until = times[hi] / 1_000_000_000 + 1;
    if let Some((u, n)) = unit { println!("UNIT={}\nUNIT_RECORDS={n}", shq(&u)); }
    if let Some((t, (n, at, body))) = term {
        println!("TERM={}\nTERM_RECORDS={n}\nEXAMPLE_TIME_S={}\nEXAMPLE_MESSAGE={}", shq(&t), at / 1_000_000_000, shq(&body.chars().take(200).collect::<String>()));
    }
    println!("SINCE=@{since}\nUNTIL=@{until}\nTOTAL_RECORDS={}", times.len());
    Ok(())
}

/// Synthetic journal-like data: a few dozen units, realistic message shapes, a long tail of rare words.
fn synth(n: u64, seed: u64) -> Result<()> {
    let mut rng = seed.wrapping_mul(0x9E3779B97F4A7C15) | 1;
    let mut next = move || { rng ^= rng << 13; rng ^= rng >> 7; rng ^= rng << 17; rng };
    let units: Vec<String> = (0..40).map(|i| match i % 5 {
        0 => format!("app-{i}.service"), 1 => format!("plasma-{i}.service"), 2 => format!("dbus-{i}.service"),
        3 => format!("pipewire-{i}.service"), _ => format!("xdg-{i}.service") }).collect();
    let verbs = ["started", "stopped", "failed", "connected", "disconnected", "loaded", "reloading", "timeout", "retrying", "listening"];
    let nouns = ["session", "socket", "device", "portal", "window", "surface", "stream", "monitor", "keyboard", "battery"];
    let syl = ["ka", "zu", "mor", "tel", "vin", "rash", "quo", "pex", "lum", "dra", "fen", "gor"];
    let out = std::io::stdout();
    let mut o = std::io::BufWriter::with_capacity(1 << 20, out.lock());
    let mut t_us: i64 = 1_764_000_000_000_000;
    for i in 0..n {
        t_us += (next() % 20_000) as i64;
        // skewed unit choice: a few chatty units, many quiet ones
        let u = &units[((next() % 1000) as f64 / 1000.0).powi(3).mul_add(units.len() as f64, 0.0) as usize % units.len()];
        let prio = match next() % 100 { 0..=1 => 3, 2..=6 => 4, 7..=14 => 5, 15..=89 => 6, _ => 7 };
        let mut msg = format!("{} {} {} id={} after {}ms", nouns[(next() % 10) as usize], verbs[(next() % 10) as usize],
            nouns[(next() % 10) as usize], next() % 100_000, next() % 5000);
        if next() % 50 == 0 {
            // rare made-up word
            let w: String = (0..4).map(|_| syl[(next() % syl.len() as u64) as usize]).collect();
            msg.push_str(&format!(" detail={w}"));
        }
        let line = serde_json::json!({
            "__REALTIME_TIMESTAMP": t_us.to_string(),
            "__MONOTONIC_TIMESTAMP": (t_us / 7).to_string(),
            "__CURSOR": format!("s=abc;i={i:x}"),
            "_BOOT_ID": "0f3c2b1a9d8e4f7a8b6c5d4e3f2a1b0c",
            "_MACHINE_ID": "5e4d3c2b1a0f9e8d7c6b5a4f3e2d1c0b",
            "_HOSTNAME": "laptop",
            "_TRANSPORT": "stdout",
            "PRIORITY": prio.to_string(),
            "SYSLOG_IDENTIFIER": u.trim_end_matches(".service"),
            "_PID": (1000 + (hash(u.as_bytes()) % 5000)).to_string(),
            "_UID": "1000", "_GID": "1000",
            "_COMM": u.trim_end_matches(".service"),
            "_SYSTEMD_USER_UNIT": u,
            "_SYSTEMD_UNIT": "user@1000.service",
            "_SYSTEMD_CGROUP": format!("/user.slice/user-1000.slice/user@1000.service/app.slice/{u}"),
            "MESSAGE": msg,
        });
        writeln!(o, "{line}")?;
    }
    let _ = std::io::stdin().read(&mut [0u8; 0]);
    Ok(())
}
