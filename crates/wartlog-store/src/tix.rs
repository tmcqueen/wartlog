//! Per-segment term index (`.wtix`): memory-mapped, used without parsing, rebuildable from the segment.
//!
//! Layout (little-endian):
//!   header (64 bytes): magic "WARTTIX\0", u16 version, u16 pad, u32 block_count, [u8;16] segment id,
//!                      u64 segment length covered, u64 fst length, u64 postings length, u32 pad, u32 header crc
//!   block table: block_count x 88-byte entries (see `ENTRY`)
//!   fst: term -> postings offset. Terms:
//!        "t:<token>"         lowercase body token of 3+ chars (rendered body)
//!        "f:<key>\0<value>"  value of an attribute key with few distinct values in this segment
//!        "k:<key>"           marks a key whose values are all indexed under f: (empty postings)
//!        "x:<16 raw bytes>"  trace id
//!   postings: varint count, then varint deltas of block numbers
//!   u32 crc32c of everything before it (checked by `verify`, not on every open)
//!
//! The build makes two passes over the blocks so memory stays bounded: pass 1 collects body terms,
//! trace ids and per-key distinct-value counts (as hashes, abandoning a key once it passes the cap);
//! pass 2 collects field terms for the keys that stayed under the cap.

use anyhow::{bail, Context, Result};
use rayon::prelude::*;
use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::path::{Path, PathBuf};
use wartlog_model::{hash, tokens, Value, MIN_TERM_CHARS};

use crate::format::*;
use crate::segment::{BlockMeta, Segment};

pub const TIX_MAGIC: &[u8; 8] = b"WARTTIX\0";
pub const TIX_VERSION: u16 = 1;
const HDR: usize = 64;
const ENTRY: usize = 88;
/// longest attribute value text that gets an f: term
pub const MAX_FIELD_VALUE: usize = 64;

pub fn tix_path(seg: &Path) -> PathBuf {
    seg.with_extension("wtix")
}

fn put_varint(b: &mut Vec<u8>, mut v: u64) {
    while v >= 0x80 {
        b.push((v as u8) | 0x80);
        v >>= 7;
    }
    b.push(v as u8);
}

fn get_varint(b: &[u8], o: &mut usize) -> Option<u64> {
    let (mut v, mut s) = (0u64, 0);
    loop {
        let x = *b.get(*o)?;
        *o += 1;
        v |= ((x & 0x7f) as u64) << s;
        if x < 0x80 {
            return Some(v);
        }
        s += 7;
        if s > 63 {
            return None;
        }
    }
}

/// Text form of a value for f: terms (None when it should not be indexed).
pub fn field_text(v: &Value) -> Option<String> {
    let s = match v {
        Value::Null | Value::Bytes(_) | Value::Json(_) => return None,
        Value::Str(s) => s.to_lowercase(),
        other => other.to_text().to_lowercase(),
    };
    (s.len() <= MAX_FIELD_VALUE).then_some(s)
}

pub fn field_term(key: &str, value_text: &str) -> Vec<u8> {
    format!("f:{key}\0{value_text}").into_bytes()
}

pub fn body_term(token: &str) -> Vec<u8> {
    format!("t:{token}").into_bytes()
}

pub fn trace_term(id: &[u8; 16]) -> Vec<u8> {
    let mut t = b"x:".to_vec();
    t.extend_from_slice(id);
    t
}

fn encode_entry(out: &mut Vec<u8>, m: &BlockMeta) {
    put_u64(out, m.offset);
    put_u32(out, m.compressed_len);
    put_u32(out, m.uncompressed_len);
    put_u32(out, m.record_count);
    put_u32(out, m.payload_crc);
    put_i64(out, m.min_time);
    put_i64(out, m.max_time);
    put_u32(out, m.severities);
    put_u32(out, m.span_count);
    put_u64(out, m.first_seq);
    put_u64(out, m.last_seq);
    put_i64(out, m.max_observed);
    put_u64(out, if m.sources.len() == 1 { m.sources[0] } else { 0 });
    put_u64(out, m.block_seq);
}

type Card = HashMap<String, Option<HashSet<u64>>>;

/// Builds the .wtix bytes for a segment by scanning it.
pub fn build(seg: &Segment) -> Result<Vec<u8>> {
    let header = seg.header()?;
    let scan = seg.scan(true);
    let blocks = &scan.blocks;
    let records: u64 = blocks.iter().map(|b| b.record_count as u64).sum();
    let cap = (records / 10).max(1024) as usize;

    let merge = |a: &mut Card, k: &str, vals: &mut dyn Iterator<Item = u64>| {
        if !a.contains_key(k) {
            a.insert(k.to_string(), Some(HashSet::new()));
        }
        let Some(e) = a.get_mut(k) else { return };
        if let Some(set) = e {
            set.extend(vals);
            if set.len() > cap {
                *e = None;
            }
        }
    };

    // Blocks are read in parallel a chunk at a time and each chunk is merged into `post` before the
    // next one is read, so only one chunk's per-block sets are alive at once. Chunks go in block
    // order, so every postings list comes out sorted without a sort.
    let chunk = rayon::current_num_threads().max(1) * 4;
    // `post` is split into shards by term hash, so merging a chunk runs on every thread too
    const SHARDS: usize = 64;
    let shard_of = |t: &[u8]| (hash(t) % SHARDS as u64) as usize;
    let mut post: Vec<HashMap<Vec<u8>, Vec<u32>>> = (0..SHARDS).map(|_| HashMap::new()).collect();
    let split = |terms: HashSet<Vec<u8>>| -> Vec<Vec<Vec<u8>>> {
        let mut out: Vec<Vec<Vec<u8>>> = (0..SHARDS).map(|_| Vec::new()).collect();
        for t in terms {
            out[shard_of(&t)].push(t);
        }
        out
    };
    // adds one chunk's terms (per block, per shard), in block order
    let add_chunk = |post: &mut Vec<HashMap<Vec<u8>, Vec<u32>>>, first: usize, per_block: &mut Vec<Vec<Vec<Vec<u8>>>>| {
        let cols: Vec<Vec<(u32, Vec<Vec<u8>>)>> = {
            let mut cols: Vec<Vec<(u32, Vec<Vec<u8>>)>> = (0..SHARDS).map(|_| Vec::new()).collect();
            for (i, shards) in per_block.drain(..).enumerate() {
                for (s, ts) in shards.into_iter().enumerate() {
                    if !ts.is_empty() {
                        cols[s].push(((first + i) as u32, ts));
                    }
                }
            }
            cols
        };
        post.par_iter_mut().zip(cols).for_each(|(map, col)| {
            for (bi, ts) in col {
                for t in ts {
                    match map.get_mut(t.as_slice()) {
                        Some(v) => {
                            if v.last() != Some(&bi) {
                                v.push(bi)
                            }
                        }
                        None => {
                            map.insert(t, vec![bi]);
                        }
                    }
                }
            }
        });
    };

    // pass 1: body terms, trace ids, key cardinality
    let mut card = Card::new();
    for (ci, part) in blocks.chunks(chunk).enumerate() {
        let pass1: Vec<(Vec<Vec<Vec<u8>>>, HashMap<String, HashSet<u64>>)> = part
            .par_iter()
            .map(|m| -> Result<_> {
                let v = seg.read_block(m)?;
                let mut terms: HashSet<Vec<u8>> = HashSet::new();
                let mut seen_bodies: HashSet<u32> = HashSet::new();
                let mut by_key: HashMap<String, HashSet<u64>> = HashMap::new();
                for r in v.iter() {
                    let text = if r.template().is_some() {
                        Some(r.body())
                    } else if seen_bodies.insert(r.body_ref()) {
                        Some(r.raw_body().to_string())
                    } else {
                        None
                    };
                    if let Some(text) = text {
                        for t in tokens(&text) {
                            if t.chars().count() >= MIN_TERM_CHARS {
                                terms.insert(body_term(&t));
                            }
                        }
                    }
                    if let Some(tid) = r.trace_id() {
                        terms.insert(trace_term(&tid));
                    }
                    r.for_each_attr(|k, val| {
                        if let Some(t) = field_text(&val) {
                            match by_key.get_mut(k) {
                                Some(s) => {
                                    s.insert(hash(t.as_bytes()));
                                }
                                None => {
                                    by_key.insert(k.to_string(), HashSet::from([hash(t.as_bytes())]));
                                }
                            }
                        }
                    });
                }
                Ok((split(terms), by_key))
            })
            .collect::<Result<_>>()?;
        let mut per_block = Vec::with_capacity(pass1.len());
        for (terms, by_key) in pass1 {
            per_block.push(terms);
            for (k, set) in by_key {
                merge(&mut card, &k, &mut set.into_iter());
            }
        }
        add_chunk(&mut post, ci * chunk, &mut per_block);
    }
    let indexed: HashSet<String> = card.into_iter().filter(|(_, s)| s.is_some()).map(|(k, _)| k).collect();

    // pass 2: field terms of indexed keys
    if !indexed.is_empty() {
        for (ci, part) in blocks.chunks(chunk).enumerate() {
            let mut fields: Vec<Vec<Vec<Vec<u8>>>> = part
                .par_iter()
                .map(|m| -> Result<Vec<Vec<Vec<u8>>>> {
                    let v = seg.read_block(m)?;
                    let mut terms: HashSet<Vec<u8>> = HashSet::new();
                    for r in v.iter() {
                        r.for_each_attr(|k, val| {
                            if indexed.contains(k) {
                                if let Some(t) = field_text(&val) {
                                    terms.insert(field_term(k, &t));
                                }
                            }
                        });
                    }
                    Ok(split(terms))
                })
                .collect::<Result<_>>()?;
            add_chunk(&mut post, ci * chunk, &mut fields);
        }
    }
    for k in &indexed {
        let t = format!("k:{k}").into_bytes();
        post[shard_of(&t)].entry(t).or_default();
    }
    // the FST needs its keys in byte order
    let mut terms: Vec<(Vec<u8>, Vec<u32>)> = post.into_iter().flatten().collect();
    terms.par_sort_unstable_by(|a, b| a.0.cmp(&b.0));
    let mut table = Vec::with_capacity(blocks.len() * ENTRY);
    for m in blocks {
        encode_entry(&mut table, m);
    }
    let mut postings = Vec::new();
    let mut fb = fst::MapBuilder::memory();
    for (t, bl) in &terms {
        fb.insert(t, postings.len() as u64)?;
        put_varint(&mut postings, bl.len() as u64);
        let mut prev = 0u32;
        for &b in bl {
            put_varint(&mut postings, (b - prev) as u64);
            prev = b;
        }
    }
    let fst_bytes = fb.into_inner()?;
    let mut out = Vec::with_capacity(HDR + table.len() + fst_bytes.len() + postings.len() + 4);
    out.extend_from_slice(TIX_MAGIC);
    put_u16(&mut out, TIX_VERSION);
    put_u16(&mut out, 0);
    put_u32(&mut out, blocks.len() as u32);
    out.extend_from_slice(&header.id);
    put_u64(&mut out, scan.valid_len);
    put_u64(&mut out, fst_bytes.len() as u64);
    put_u64(&mut out, postings.len() as u64);
    put_u32(&mut out, 0);
    let hcrc = crc32c::crc32c(&out);
    put_u32(&mut out, hcrc);
    debug_assert_eq!(out.len(), HDR);
    out.extend_from_slice(&table);
    out.extend_from_slice(&fst_bytes);
    out.extend_from_slice(&postings);
    let crc = crc32c::crc32c(&out);
    put_u32(&mut out, crc);
    Ok(out)
}

/// Builds and atomically writes the term index next to the segment.
pub fn write(seg_path: &Path) -> Result<usize> {
    let seg = Segment::open(seg_path)?;
    let b = build(&seg);
    // The build frees hundreds of MB spread over every rayon thread's malloc arena; glibc keeps
    // that in the process (wartlogd held 15 GB after a 117M-record load) unless asked to return it.
    release_free_memory();
    let b = b?;
    let tmp = seg_path.with_extension("wtix.tmp");
    crate::write_file_synced(&tmp, &b)?;
    std::fs::rename(&tmp, tix_path(seg_path))?;
    Ok(b.len())
}

/// Returns freed heap pages to the OS (glibc only; other allocators do this on their own).
pub fn release_free_memory() {
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    // SAFETY: malloc_trim only walks the allocator's own free lists.
    unsafe {
        libc::malloc_trim(0);
    }
}

/// An open term index.
pub struct Tix {
    map: memmap2::Mmap,
    pub blocks: usize,
    table: usize,
    fst: (usize, usize),
    post: usize,
}

impl Tix {
    /// Opens a term index if it exists and its header matches the segment (cheap; no full CRC).
    pub fn open(seg_path: &Path, seg_id: &[u8; 16], seg_len: u64) -> Result<Option<Tix>> {
        let f = match File::open(tix_path(seg_path)) {
            Ok(f) => f,
            Err(_) => return Ok(None),
        };
        // SAFETY: index files are written to a temporary name and renamed into place, never modified.
        let map = unsafe { memmap2::Mmap::map(&f)? };
        if map.len() < HDR + 4 || &map[0..8] != TIX_MAGIC || crc32c::crc32c(&map[..60]) != get_u32(&map, 60) {
            return Ok(None);
        }
        if get_u16(&map, 8) != TIX_VERSION || &map[16..32] != seg_id || get_u64(&map, 32) != seg_len {
            return Ok(None);
        }
        let blocks = get_u32(&map, 12) as usize;
        let fst_len = get_u64(&map, 40) as usize;
        let post_len = get_u64(&map, 48) as usize;
        let t0 = HDR;
        let f0 = t0 + blocks * ENTRY;
        let p0 = f0 + fst_len;
        if p0 + post_len + 4 != map.len() {
            return Ok(None);
        }
        if fst::Map::new(&map[f0..p0]).is_err() {
            return Ok(None);
        }
        Ok(Some(Tix { map, blocks, table: t0, fst: (f0, p0), post: p0 }))
    }

    fn fst(&self) -> fst::Map<&[u8]> {
        fst::Map::new(&self.map[self.fst.0..self.fst.1]).expect("checked at open")
    }

    /// True when every value of `key` in this segment is indexed under f: terms.
    pub fn has_key(&self, key: &str) -> bool {
        self.fst().get(format!("k:{key}")).is_some()
    }

    /// Block numbers containing a term (empty when absent).
    pub fn postings(&self, term: &[u8]) -> Vec<u32> {
        let off = match self.fst().get(term) {
            Some(o) => o as usize + self.post,
            None => return vec![],
        };
        let mut o = off;
        let n = get_varint(&self.map, &mut o).unwrap_or(0);
        let mut v = Vec::with_capacity(n as usize);
        let mut prev = 0u64;
        for _ in 0..n {
            match get_varint(&self.map, &mut o) {
                Some(d) => prev += d,
                None => break,
            }
            v.push(prev as u32);
        }
        v
    }

    /// Union of postings of every term starting with `prefix` (for prefix searches).
    pub fn postings_prefix(&self, prefix: &[u8], limit_terms: usize) -> Option<Vec<u32>> {
        use fst::{IntoStreamer, Streamer};
        let m = self.fst();
        let mut s = m.range().ge(prefix).into_stream();
        let mut out: Vec<u32> = Vec::new();
        let mut n = 0;
        while let Some((k, _)) = s.next() {
            if !k.starts_with(prefix) {
                break;
            }
            n += 1;
            if n > limit_terms {
                return None;
            }
            out.extend(self.postings(k));
        }
        out.sort_unstable();
        out.dedup();
        Some(out)
    }

    /// Every term with the given prefix (for completion), up to `limit`.
    pub fn terms_with_prefix(&self, prefix: &[u8], limit: usize) -> Vec<(Vec<u8>, usize)> {
        use fst::{IntoStreamer, Streamer};
        let m = self.fst();
        let mut s = m.range().ge(prefix).into_stream();
        let mut out = Vec::new();
        while let Some((k, _)) = s.next() {
            if !k.starts_with(prefix) || out.len() >= limit {
                break;
            }
            out.push((k.to_vec(), self.postings(k).len()));
        }
        out
    }

    pub fn block(&self, i: usize) -> BlockMeta {
        let o = self.table + i * ENTRY;
        let m = &self.map;
        let src = get_u64(m, o + 72);
        BlockMeta {
            offset: get_u64(m, o),
            compressed_len: get_u32(m, o + 8),
            uncompressed_len: get_u32(m, o + 12),
            record_count: get_u32(m, o + 16),
            payload_crc: get_u32(m, o + 20),
            min_time: get_i64(m, o + 24),
            max_time: get_i64(m, o + 32),
            severities: get_u32(m, o + 40),
            span_count: get_u32(m, o + 44),
            first_seq: get_u64(m, o + 48),
            last_seq: get_u64(m, o + 56),
            max_observed: get_i64(m, o + 64),
            sources: if src == 0 { vec![] } else { vec![src] },
            block_seq: get_u64(m, o + 80),
            codec: CODEC_ZSTD,
        }
    }

    pub fn full_crc_ok(&self) -> bool {
        let n = self.map.len();
        crc32c::crc32c(&self.map[..n - 4]) == get_u32(&self.map, n - 4)
    }
}

/// Rebuilds in memory and compares with the file on disk.
pub fn verify(seg_path: &Path) -> Result<bool> {
    let seg = Segment::open(seg_path)?;
    let fresh = build(&seg)?;
    let disk = std::fs::read(tix_path(seg_path)).context("read term index")?;
    if disk.len() < 4 {
        bail!("term index too short");
    }
    Ok(fresh == disk)
}
