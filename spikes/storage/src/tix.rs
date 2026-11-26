//! Per-segment term and field index (.wtix), memory-mapped and usable without parsing.
//!
//! Layout (little-endian):
//!   header (64 bytes): magic "WARTTIX\0", u16 version, u16 pad, u32 block_count, [u8;16] segment id,
//!                      u64 segment length, u64 fst length, u64 postings length, u32 pad, u32 header crc
//!   block table: block_count x 56 bytes (offset, lengths, crc, time range, severities, first seq)
//!   fst: term -> postings offset.  Terms: "t:<lowercase token>" for body tokens (3+ chars),
//!        "f:<key>\0<value>" for values of low-cardinality fields, "k:<key>" marks an indexed field.
//!   postings: varint count, then varint deltas of block numbers
//!   u32 crc32c of everything before it (checked by `index verify`, not on every open)
//! Rebuildable from the segment alone, deterministically.

use anyhow::{bail, Result};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs::File;
use std::path::{Path, PathBuf};

use crate::format::*;
use crate::store::*;

pub const TIX_MAGIC: &[u8; 8] = b"WARTTIX\0";
const HDR: usize = 64;
const ENTRY: usize = 56;

pub fn tix_path(seg: &Path) -> PathBuf { seg.with_extension("wtix") }

fn put_varint(b: &mut Vec<u8>, mut v: u64) {
    while v >= 0x80 { b.push((v as u8) | 0x80); v >>= 7; }
    b.push(v as u8);
}

fn get_varint(b: &[u8], o: &mut usize) -> u64 {
    let (mut v, mut s) = (0u64, 0);
    loop { let x = b[*o]; *o += 1; v |= ((x & 0x7f) as u64) << s; if x < 0x80 { return v; } s += 7; }
}

/// Terms of one block, extracted from its string table without materializing records.
/// Pass 1 (`indexed` = None) returns per-key hashes of distinct values, so cardinality can be judged in bounded memory.
/// Pass 2 returns the `f:` terms of the indexed keys only.
struct BlockTerms { entry: Vec<u8>, terms: Vec<Vec<u8>>, key_values: Vec<(String, Vec<u64>)> }

fn block_terms(f: &File, b: &BlockLoc, indexed: Option<&HashSet<String>>) -> Result<BlockTerms> {
    let p = read_payload(f, b)?;
    let s = Summary::decode(&b.summary)?;
    let mut entry = Vec::with_capacity(ENTRY);
    put_u64(&mut entry, b.offset);
    put_u32(&mut entry, b.compressed_len);
    put_u32(&mut entry, b.uncompressed_len);
    put_u32(&mut entry, b.record_count);
    put_u32(&mut entry, b.payload_crc);
    put_i64(&mut entry, s.min_time);
    put_i64(&mut entry, s.max_time);
    put_u32(&mut entry, s.severities);
    put_u32(&mut entry, 0);
    put_u64(&mut entry, s.first_seq);
    // string table
    let mut o = 0;
    let n = get_u32(&p, o) as usize; o += 4;
    let mut strings: Vec<&str> = Vec::with_capacity(n);
    for _ in 0..n { let l = get_u32(&p, o) as usize; o += 4; strings.push(std::str::from_utf8(&p[o..o + l])?); o += l; }
    let ns = get_u32(&p, o) as usize; o += 4;
    for _ in 0..ns { o += 9; let na = get_u16(&p, o) as usize; o += 2 + na * 8; }
    while o % 8 != 0 { o += 1; }
    // walk records: collect distinct body refs and distinct (key, value) pairs
    let mut bodies: HashSet<u32> = HashSet::new();
    let mut pairs: HashSet<(u32, u8, u64)> = HashSet::new();
    for _ in 0..b.record_count {
        let len = get_u32(&p, o) as usize;
        bodies.insert(get_u32(&p, o + 36));
        let na = get_u16(&p, o + 32) as usize;
        let mut q = o + RECORD_FIXED_LEN;
        for _ in 0..na {
            let k = get_u32(&p, q); let tag = p[q + 4]; q += 5;
            let v = match tag { 1 => { q += 1; p[q - 1] as u64 } 2 | 3 => { q += 8; get_u64(&p, q - 8) } 4 => { q += 4; get_u32(&p, q - 4) as u64 } _ => 0 };
            pairs.insert((k, tag, v));
        }
        o += len;
    }
    let show = |tag: u8, v: u64| -> Option<String> {
        Some(match tag { 1 => (v != 0).to_string(), 2 => (v as i64).to_string(), 3 => f64::from_bits(v).to_string(), 4 => strings[v as usize].to_string(), _ => return None })
    };
    let mut terms: Vec<Vec<u8>> = Vec::new();
    let mut key_values = Vec::new();
    match indexed {
        None => {
            for r in bodies {
                for t in tokens(strings[r as usize]) {
                    if t.chars().count() >= 3 { terms.push([b"t:".as_slice(), t.as_bytes()].concat()); }
                }
            }
            let mut by_key: HashMap<u32, Vec<u64>> = HashMap::new();
            for (k, tag, v) in pairs {
                let h = if tag == 4 { hash(strings[v as usize].as_bytes()) } else { hash(&[&[tag][..], &v.to_le_bytes()].concat()) };
                by_key.entry(k).or_default().push(h);
            }
            key_values = by_key.into_iter().map(|(k, v)| (strings[k as usize].to_string(), v)).collect();
        }
        Some(ix) => {
            for (k, tag, v) in pairs {
                let key = strings[k as usize];
                if !ix.contains(key) { continue; }
                if let Some(val) = show(tag, v) { if val.len() <= 64 { terms.push(format!("f:{key}\0{val}").into_bytes()); } }
            }
        }
    }
    terms.sort_unstable();
    terms.dedup();
    Ok(BlockTerms { entry, terms, key_values })
}

/// Builds the .wtix bytes for a segment by scanning it; blocks are processed in parallel, in two passes
/// (body terms and field cardinality, then field terms) so memory stays bounded by the indexed terms.
pub fn build(seg: &Path) -> Result<Vec<u8>> {
    use rayon::prelude::*;
    let (id, len, rep) = scan_segment(seg, true)?;
    let f = File::open(seg)?;
    let records: u64 = rep.blocks.iter().map(|b| b.record_count as u64).sum();
    let cap = (records / 10).max(1024) as usize;
    // pass 1: body terms, block table, and distinct-value counts per key (a key is dropped once it passes cap)
    type Card = HashMap<String, Option<HashSet<u64>>>;
    let merge = |mut a: Card, k: String, vals: Vec<u64>| -> Card {
        let e = a.entry(k).or_insert_with(|| Some(HashSet::new()));
        if let Some(set) = e { set.extend(vals); if set.len() > cap { *e = None; } }
        a
    };
    let pass1: Vec<(Vec<u8>, Vec<Vec<u8>>, Card)> = rep.blocks.par_iter().map(|b| {
        let bt = block_terms(&f, b, None)?;
        let card = bt.key_values.into_iter().fold(Card::new(), |a, (k, v)| merge(a, k, v));
        Ok((bt.entry, bt.terms, card))
    }).collect::<Result<_>>()?;
    let mut card = Card::new();
    let mut table = Vec::with_capacity(pass1.len() * ENTRY);
    let mut pairs: Vec<(Vec<u8>, u32)> = Vec::new();
    for (bi, (entry, terms, c)) in pass1.into_iter().enumerate() {
        table.extend_from_slice(&entry);
        for t in terms { pairs.push((t, bi as u32)); }
        for (k, v) in c {
            match v { Some(set) => card = merge(card, k, set.into_iter().collect()), None => { card.insert(k, None); } }
        }
    }
    let indexed: HashSet<String> = card.into_iter().filter(|(_, s)| s.is_some()).map(|(k, _)| k).collect();
    // pass 2: field terms of the indexed keys
    let fields: Vec<Vec<Vec<u8>>> = rep.blocks.par_iter().map(|b| Ok(block_terms(&f, b, Some(&indexed))?.terms)).collect::<Result<_>>()?;
    for (bi, ts) in fields.into_iter().enumerate() { for t in ts { pairs.push((t, bi as u32)); } }
    for k in &indexed { pairs.push((format!("k:{k}").into_bytes(), u32::MAX)); }
    pairs.par_sort_unstable();
    pairs.dedup();
    let mut terms: BTreeMap<Vec<u8>, Vec<u32>> = BTreeMap::new();
    for (t, b) in pairs {
        let v = terms.entry(t).or_default();
        if b != u32::MAX { v.push(b); }
    }
    let mut postings = Vec::new();
    let mut fb = fst::MapBuilder::memory();
    for (t, blocks) in &terms {
        fb.insert(t, postings.len() as u64)?;
        put_varint(&mut postings, blocks.len() as u64);
        let mut prev = 0u32;
        for &b in blocks { put_varint(&mut postings, (b - prev) as u64); prev = b; }
    }
    let fst_bytes = fb.into_inner()?;
    let mut out = Vec::with_capacity(HDR + table.len() + fst_bytes.len() + postings.len() + 4);
    out.extend_from_slice(TIX_MAGIC);
    put_u16(&mut out, 1);
    put_u16(&mut out, 0);
    put_u32(&mut out, rep.blocks.len() as u32);
    out.extend_from_slice(&id);
    put_u64(&mut out, len);
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

pub fn write(seg: &Path) -> Result<usize> {
    let b = build(seg)?;
    let tmp = tix_path(seg).with_extension("wtix.tmp");
    std::fs::write(&tmp, &b)?;
    std::fs::rename(&tmp, tix_path(seg))?;
    Ok(b.len())
}

pub struct Tix {
    map: memmap2::Mmap,
    pub blocks: usize,
    table: (usize, usize),
    fst: (usize, usize),
    post: usize,
}

impl Tix {
    /// Opens and memory-maps a term index, checking its header against the segment (cheap; no full CRC).
    pub fn open(seg: &Path, seg_id: &[u8; 16], seg_len: u64) -> Result<Option<Tix>> {
        let f = match File::open(tix_path(seg)) { Ok(f) => f, Err(_) => return Ok(None) };
        let map = unsafe { memmap2::Mmap::map(&f)? };
        if map.len() < HDR + 4 || &map[0..8] != TIX_MAGIC || crc32c::crc32c(&map[..60]) != get_u32(&map, 60) { return Ok(None); }
        if &map[16..32] != seg_id || get_u64(&map, 32) != seg_len { return Ok(None); }
        let blocks = get_u32(&map, 12) as usize;
        let fst_len = get_u64(&map, 40) as usize;
        let post_len = get_u64(&map, 48) as usize;
        let t0 = HDR; let f0 = t0 + blocks * ENTRY; let p0 = f0 + fst_len;
        if p0 + post_len + 4 != map.len() { bail!("bad tix lengths"); }
        Ok(Some(Tix { map, blocks, table: (t0, f0), fst: (f0, p0), post: p0 }))
    }

    fn fst(&self) -> fst::Map<&[u8]> { fst::Map::new(&self.map[self.fst.0..self.fst.1]).expect("fst") }

    pub fn has_key(&self, key: &str) -> bool { self.fst().get(format!("k:{key}")).is_some() }

    /// Block numbers containing a term, or None when the term is absent.
    pub fn postings(&self, term: &[u8]) -> Vec<u32> {
        let off = match self.fst().get(term) { Some(o) => o as usize + self.post, None => return vec![] };
        let mut o = off;
        let n = get_varint(&self.map, &mut o);
        let mut v = Vec::with_capacity(n as usize);
        let mut prev = 0u64;
        for _ in 0..n { prev += get_varint(&self.map, &mut o); v.push(prev as u32); }
        v
    }

    /// Block table entry as a BlockLoc (no summary), plus time range and severities.
    pub fn block(&self, i: usize) -> (BlockLoc, i64, i64, u32) {
        let o = self.table.0 + i * ENTRY;
        let m = &self.map;
        (BlockLoc {
            offset: get_u64(m, o), block_seq: i as u64, total_len: 0,
            compressed_len: get_u32(m, o + 8), uncompressed_len: get_u32(m, o + 12),
            record_count: get_u32(m, o + 16), payload_crc: get_u32(m, o + 20), summary: vec![],
        }, get_i64(m, o + 24), get_i64(m, o + 32), get_u32(m, o + 40))
    }

    pub fn full_crc_ok(&self) -> bool {
        let n = self.map.len();
        crc32c::crc32c(&self.map[..n - 4]) == get_u32(&self.map, n - 4)
    }
}
