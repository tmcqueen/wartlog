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

/// Builds the .wtix bytes for a segment by scanning and decoding it.
pub fn build(seg: &Path) -> Result<Vec<u8>> {
    let (id, len, rep) = scan_segment(seg, true)?;
    let f = File::open(seg)?;
    let mut terms: BTreeMap<Vec<u8>, Vec<u32>> = BTreeMap::new();
    let mut distinct: HashMap<String, HashSet<String>> = HashMap::new();
    let mut high: HashSet<String> = HashSet::new();
    let mut table = Vec::with_capacity(rep.blocks.len() * ENTRY);
    let mut records = 0u64;
    let mut add = |terms: &mut BTreeMap<Vec<u8>, Vec<u32>>, t: Vec<u8>, bi: u32| {
        let v = terms.entry(t).or_default();
        if v.last() != Some(&bi) { v.push(bi); }
    };
    for (bi, b) in rep.blocks.iter().enumerate() {
        let bi = bi as u32;
        let p = read_payload(&f, b)?;
        let recs = decode_payload(&p, b.record_count)?;
        let s = Summary::decode(&b.summary)?;
        put_u64(&mut table, b.offset);
        put_u32(&mut table, b.compressed_len);
        put_u32(&mut table, b.uncompressed_len);
        put_u32(&mut table, b.record_count);
        put_u32(&mut table, b.payload_crc);
        put_i64(&mut table, s.min_time);
        put_i64(&mut table, s.max_time);
        put_u32(&mut table, s.severities);
        put_u32(&mut table, 0);
        put_u64(&mut table, s.first_seq);
        for r in &recs {
            records += 1;
            for t in tokens(&r.body) {
                if t.chars().count() >= 3 { add(&mut terms, [b"t:".as_slice(), t.as_bytes()].concat(), bi); }
            }
            for (k, v) in &r.attrs {
                if high.contains(k) { continue; }
                let v = v.as_display();
                if v.len() > 64 { continue; }
                let set = distinct.entry(k.clone()).or_default();
                set.insert(v.clone());
                add(&mut terms, format!("f:{k}\0{v}").into_bytes(), bi);
            }
        }
        // drop fields that turn out to be high-cardinality (ids, timestamps)
        let cap = (records / 10).max(1024) as usize;
        let over: Vec<String> = distinct.iter().filter(|(_, s)| s.len() > cap).map(|(k, _)| k.clone()).collect();
        for k in over {
            distinct.remove(&k);
            let prefix = format!("f:{k}\0").into_bytes();
            let keys: Vec<Vec<u8>> = terms.range(prefix.clone()..).take_while(|(t, _)| t.starts_with(&prefix)).map(|(t, _)| t.clone()).collect();
            for t in keys { terms.remove(&t); }
            high.insert(k);
        }
    }
    let final_cap = (records / 10).max(1024) as usize;
    for (k, s) in &distinct {
        if s.len() <= final_cap { terms.insert(format!("k:{k}").into_bytes(), vec![]); }
        else {
            let prefix = format!("f:{k}\0").into_bytes();
            let keys: Vec<Vec<u8>> = terms.range(prefix.clone()..).take_while(|(t, _)| t.starts_with(&prefix)).map(|(t, _)| t.clone()).collect();
            for t in keys { terms.remove(&t); }
        }
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
