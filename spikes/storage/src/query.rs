//! Flag-based queries (no KQL in the spike) with three execution modes.

use anyhow::Result;
use rayon::prelude::*;
use std::fs::File;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use crate::format::*;
use crate::store::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum Mode {
    /// use .widx index files (falls back to scanning a segment whose index is missing or invalid)
    Index,
    /// ignore index files; walk segment block headers and use the in-file block summaries
    Summaries,
    /// no pruning at all: decompress and check every block
    Full,
}

#[derive(Clone, Debug, Default)]
pub struct Pred {
    pub since_ns: Option<i64>,
    pub until_ns: Option<i64>,
    pub fields: Vec<(String, String)>,
    pub has: Vec<String>,
    pub contains: Option<String>,
    pub min_severity: Option<u8>,
}

impl Pred {
    fn block_may_match(&self, s: &Summary) -> bool {
        if let Some(a) = self.since_ns { if s.max_time < a { return false; } }
        if let Some(b) = self.until_ns { if s.min_time > b { return false; } }
        if let Some(m) = self.min_severity { if s.severities >> m == 0 { return false; } }
        for (k, v) in &self.fields {
            if s.keys.binary_search(&hash(k.as_bytes())).is_err() { return false; }
            if !s.bloom.may_contain(kv_hash(k, v)) { return false; }
        }
        for t in &self.has {
            if t.chars().count() >= 3 && !s.bloom.may_contain(hash(t.as_bytes())) { return false; }
        }
        true
    }
}

pub struct Stats {
    pub segments: usize,
    pub index_fallbacks: usize,
    pub blocks: u64,
    pub pruned: u64,
    pub decoded: u64,
    pub false_positive_blocks: u64,
    pub matches: u64,
    pub damaged: u64,
}

/// Zero-copy view over one decompressed block payload.
struct View<'a> {
    p: &'a [u8],
    strings: Vec<&'a str>,
    records_off: usize,
    count: u32,
}

impl<'a> View<'a> {
    fn new(p: &'a [u8], count: u32) -> Result<View<'a>> {
        let mut o = 0;
        let n = get_u32(p, o) as usize; o += 4;
        let mut strings = Vec::with_capacity(n);
        for _ in 0..n {
            let l = get_u32(p, o) as usize; o += 4;
            strings.push(std::str::from_utf8(&p[o..o + l])?);
            o += l;
        }
        let ns = get_u32(p, o) as usize; o += 4;
        for _ in 0..ns {
            o += 9;
            let na = get_u16(p, o) as usize; o += 2 + na * 8;
        }
        while o % 8 != 0 { o += 1; }
        Ok(View { p, strings, records_off: o, count })
    }
}

/// Runs a query. `emit` gets matching records as journalctl-like JSON lines, already sorted by time.
pub fn run(root: &Path, mode: Mode, pred: &Pred, want_rows: bool) -> Result<(Vec<String>, Stats)> {
    let segs = list_segments(root)?;
    let mut work: Vec<(usize, BlockLoc)> = Vec::new();
    let mut fallbacks = 0;
    let mut damaged = 0u64;
    let mut total_blocks = 0u64;
    for (i, seg) in segs.iter().enumerate() {
        let blocks = match mode {
            Mode::Index => match load_index(seg)? {
                Some(b) => b,
                None => { fallbacks += 1; let (_, _, r) = scan_segment(seg, false)?; damaged += r.damaged_regions.len() as u64; r.blocks }
            },
            _ => { let (_, _, r) = scan_segment(seg, false)?; damaged += r.damaged_regions.len() as u64; r.blocks }
        };
        total_blocks += blocks.len() as u64;
        for b in blocks {
            let keep = mode == Mode::Full || Summary::decode(&b.summary).map(|s| pred.block_may_match(&s)).unwrap_or(true);
            if keep { work.push((i, b)); }
        }
    }
    let files: Vec<File> = segs.iter().map(File::open).collect::<std::io::Result<_>>()?;
    let decoded = work.len() as u64;
    let fp_blocks = AtomicU64::new(0);
    let corrupt = AtomicU64::new(0);

    let contains = pred.contains.as_ref().map(|c| c.to_lowercase());
    let mut rows: Vec<(i64, u64, String)> = work.par_iter().flat_map_iter(|(si, b)| {
        let mut out = Vec::new();
        let payload = match read_payload(&files[*si], b) { Ok(p) => p, Err(_) => { corrupt.fetch_add(1, Ordering::Relaxed); return out.into_iter(); } };
        let v = match View::new(&payload, b.record_count) { Ok(v) => v, Err(_) => { corrupt.fetch_add(1, Ordering::Relaxed); return out.into_iter(); } };
        // evaluate body predicates once per distinct string
        let body_ok: Option<Vec<bool>> = if pred.has.is_empty() && contains.is_none() { None } else {
            Some(v.strings.iter().map(|s| {
                let low = s.to_lowercase();
                pred.has.iter().all(|t| tokens(&low).any(|x| &x == t))
                    && contains.as_ref().map(|c| low.contains(c.as_str())).unwrap_or(true)
            }).collect())
        };
        // resolve field predicates to string refs
        let fields: Vec<(Option<u32>, Option<u32>, Option<i64>)> = pred.fields.iter().map(|(k, val)| {
            (v.strings.iter().position(|s| s == k).map(|x| x as u32),
             v.strings.iter().position(|s| s == val).map(|x| x as u32),
             val.parse::<i64>().ok())
        }).collect();
        if fields.iter().any(|(k, s, i)| k.is_none() || (s.is_none() && i.is_none())) {
            fp_blocks.fetch_add(1, Ordering::Relaxed);
            return out.into_iter();
        }
        let p = v.p;
        let mut o = v.records_off;
        for _ in 0..v.count {
            let len = get_u32(p, o) as usize;
            let rec = o;
            o += len;
            let t = get_i64(p, rec + 8);
            if pred.since_ns.map(|a| t < a).unwrap_or(false) || pred.until_ns.map(|b| t > b).unwrap_or(false) { continue; }
            if let Some(m) = pred.min_severity { if p[rec + 5] < m { continue; } }
            if let Some(ok) = &body_ok { if !ok[get_u32(p, rec + 36) as usize] { continue; } }
            if !fields.is_empty() {
                let n = get_u16(p, rec + 32) as usize;
                let mut q = rec + RECORD_FIXED_LEN;
                let mut hits = vec![false; fields.len()];
                for _ in 0..n {
                    let k = get_u32(p, q); let tag = p[q + 4]; q += 5;
                    let (sref, ival) = match tag {
                        1 => { q += 1; (None, None) }
                        2 => { q += 8; (None, Some(get_i64(p, q - 8))) }
                        3 => { q += 8; (None, None) }
                        4 => { q += 4; (Some(get_u32(p, q - 4)), None) }
                        _ => (None, None),
                    };
                    for (i, (fk, fs, fi)) in fields.iter().enumerate() {
                        if Some(k) == *fk && ((sref.is_some() && sref == *fs) || (ival.is_some() && ival == *fi)) { hits[i] = true; }
                    }
                }
                if !hits.iter().all(|h| *h) { continue; }
            }
            let line = if want_rows { to_json(&v, rec) } else { String::new() };
            out.push((t, get_u64(p, rec + 64), line));
        }
        if out.is_empty() { fp_blocks.fetch_add(1, Ordering::Relaxed); }
        out.into_iter()
    }).collect();
    rows.par_sort_unstable_by_key(|r| (r.0, r.1));
    let stats = Stats {
        segments: segs.len(), index_fallbacks: fallbacks, blocks: total_blocks,
        pruned: total_blocks - decoded, decoded, false_positive_blocks: fp_blocks.into_inner(),
        matches: rows.len() as u64, damaged: damaged + corrupt.into_inner(),
    };
    Ok((rows.into_iter().map(|r| r.2).collect(), stats))
}

fn to_json(v: &View, rec: usize) -> String {
    let p = v.p;
    let mut m = serde_json::Map::new();
    m.insert("__REALTIME_TIMESTAMP".into(), ((get_i64(p, rec + 8) / 1000).to_string()).into());
    m.insert("MESSAGE".into(), v.strings[get_u32(p, rec + 36) as usize].into());
    let n = get_u16(p, rec + 32) as usize;
    let mut q = rec + RECORD_FIXED_LEN;
    for _ in 0..n {
        let k = v.strings[get_u32(p, q) as usize]; let tag = p[q + 4]; q += 5;
        let val: serde_json::Value = match tag {
            1 => { q += 1; (p[q - 1] != 0).to_string().into() }
            2 => { q += 8; get_i64(p, q - 8).to_string().into() }
            3 => { q += 8; f64::from_le_bytes(p[q - 8..q].try_into().unwrap()).to_string().into() }
            4 => { q += 4; v.strings[get_u32(p, q - 4) as usize].into() }
            _ => serde_json::Value::Null,
        };
        m.insert(k.into(), val);
    }
    serde_json::Value::Object(m).to_string()
}

pub fn timed<T>(f: impl FnOnce() -> T) -> (T, f64) {
    let t = Instant::now();
    let r = f();
    (r, t.elapsed().as_secs_f64())
}
