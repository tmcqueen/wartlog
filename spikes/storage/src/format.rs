//! On-disk structures from the MVP spec (section 7), simplified for the spike.
//! Little-endian everywhere, explicit byte encoding, CRC32C checksums.

use anyhow::{bail, Result};
use std::collections::HashMap;

use crate::bloom::Bloom;

pub const SEG_MAGIC: &[u8; 8] = b"WARTSEG\0";
pub const SEG_HEADER_LEN: usize = 64;
pub const BLOCK_MAGIC: u32 = u32::from_le_bytes(*b"WBLK");
pub const TRAILER_MAGIC: u32 = u32::from_le_bytes(*b"KLBW");
pub const BLOCK_HEADER_LEN: usize = 48;
pub const BLOCK_TRAILER_LEN: usize = 16;
pub const RECORD_FIXED_LEN: usize = 72;
pub const NONE_REF: u32 = u32::MAX;

// ---------- small byte helpers ----------

pub fn put_u16(b: &mut Vec<u8>, v: u16) { b.extend_from_slice(&v.to_le_bytes()); }
pub fn put_u32(b: &mut Vec<u8>, v: u32) { b.extend_from_slice(&v.to_le_bytes()); }
pub fn put_u64(b: &mut Vec<u8>, v: u64) { b.extend_from_slice(&v.to_le_bytes()); }
pub fn put_i64(b: &mut Vec<u8>, v: i64) { b.extend_from_slice(&v.to_le_bytes()); }
pub fn get_u16(b: &[u8], o: usize) -> u16 { u16::from_le_bytes(b[o..o + 2].try_into().unwrap()) }
pub fn get_u32(b: &[u8], o: usize) -> u32 { u32::from_le_bytes(b[o..o + 4].try_into().unwrap()) }
pub fn get_u64(b: &[u8], o: usize) -> u64 { u64::from_le_bytes(b[o..o + 8].try_into().unwrap()) }
pub fn get_i64(b: &[u8], o: usize) -> i64 { i64::from_le_bytes(b[o..o + 8].try_into().unwrap()) }

pub fn hash(s: &[u8]) -> u64 { xxhash_rust::xxh3::xxh3_64(s) }

/// Hash used for exact field matches in the Bloom filter: key \0 lowercase(value).
pub fn kv_hash(k: &str, v: &str) -> u64 {
    let mut b = Vec::with_capacity(k.len() + v.len() + 1);
    b.extend_from_slice(k.as_bytes());
    b.push(0);
    b.extend_from_slice(v.to_lowercase().as_bytes());
    hash(&b)
}

/// KQL-ish `has` tokenization: lowercase alphanumeric runs.
pub fn tokens(s: &str) -> impl Iterator<Item = String> + '_ {
    s.split(|c: char| !c.is_alphanumeric()).filter(|t| !t.is_empty()).map(|t| t.to_lowercase())
}

// ---------- model ----------

#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    Null,
    Bool(bool),
    I64(i64),
    F64(f64),
    Str(String),
}

impl Value {
    pub fn as_display(&self) -> String {
        match self {
            Value::Null => "null".into(),
            Value::Bool(b) => b.to_string(),
            Value::I64(i) => i.to_string(),
            Value::F64(f) => f.to_string(),
            Value::Str(s) => s.clone(),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Source {
    pub kind: u8, // 2 = journald
    pub attrs: Vec<(String, String)>,
}

impl Source {
    pub fn fingerprint(&self) -> u64 {
        let mut b = vec![self.kind];
        for (k, v) in &self.attrs {
            b.extend_from_slice(k.as_bytes());
            b.push(0);
            b.extend_from_slice(v.as_bytes());
            b.push(0);
        }
        hash(&b)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Record {
    pub seq: u64,
    pub time_ns: i64,
    pub observed_ns: i64,
    pub severity: u8,
    pub source: Source,
    pub body: String,
    pub attrs: Vec<(String, Value)>,
}

// ---------- segment header ----------

pub fn encode_segment_header(segment_id: [u8; 16], prev: [u8; 16], created_ns: i64) -> Vec<u8> {
    let mut b = Vec::with_capacity(SEG_HEADER_LEN);
    b.extend_from_slice(SEG_MAGIC);
    put_u16(&mut b, 1); // version
    put_u16(&mut b, 0); // flags
    put_u32(&mut b, SEG_HEADER_LEN as u32);
    b.extend_from_slice(&segment_id);
    b.extend_from_slice(&prev);
    put_i64(&mut b, created_ns);
    b.extend_from_slice(&[0u8; 4]);
    let crc = crc32c::crc32c(&b);
    put_u32(&mut b, crc);
    assert_eq!(b.len(), SEG_HEADER_LEN);
    b
}

pub fn decode_segment_header(b: &[u8]) -> Result<[u8; 16]> {
    if b.len() < SEG_HEADER_LEN || &b[0..8] != SEG_MAGIC {
        bail!("not a WartLog segment");
    }
    if crc32c::crc32c(&b[0..60]) != get_u32(b, 60) {
        bail!("segment header CRC mismatch");
    }
    Ok(b[16..32].try_into().unwrap())
}

// ---------- block summary ----------

#[derive(Clone, Debug, Default)]
pub struct Summary {
    pub min_time: i64,
    pub max_time: i64,
    pub first_seq: u64,
    pub last_seq: u64,
    pub count: u32,
    pub severities: u32,
    pub sources: Vec<u64>,
    pub keys: Vec<u64>,
    pub bloom: Bloom,
}

impl Summary {
    pub fn encode(&self) -> Vec<u8> {
        let mut b = Vec::new();
        put_i64(&mut b, self.min_time);
        put_i64(&mut b, self.max_time);
        put_u64(&mut b, self.first_seq);
        put_u64(&mut b, self.last_seq);
        put_u32(&mut b, self.count);
        put_u32(&mut b, 0); // span count, unused in spike
        put_u32(&mut b, self.severities);
        put_u32(&mut b, self.sources.len() as u32);
        for s in &self.sources { put_u64(&mut b, *s); }
        put_u32(&mut b, self.keys.len() as u32);
        for k in &self.keys { put_u64(&mut b, *k); }
        self.bloom.encode(&mut b);
        put_u32(&mut b, 0); // trace bloom length, unused in spike
        b
    }

    pub fn decode(b: &[u8]) -> Result<Summary> {
        let mut o = 0;
        let min_time = get_i64(b, o); o += 8;
        let max_time = get_i64(b, o); o += 8;
        let first_seq = get_u64(b, o); o += 8;
        let last_seq = get_u64(b, o); o += 8;
        let count = get_u32(b, o); o += 4;
        o += 4;
        let severities = get_u32(b, o); o += 4;
        let n = get_u32(b, o) as usize; o += 4;
        let sources = (0..n).map(|i| get_u64(b, o + i * 8)).collect(); o += n * 8;
        let n = get_u32(b, o) as usize; o += 4;
        let keys = (0..n).map(|i| get_u64(b, o + i * 8)).collect(); o += n * 8;
        let (bloom, _used) = Bloom::decode(&b[o..])?;
        Ok(Summary { min_time, max_time, first_seq, last_seq, count, severities, sources, keys, bloom })
    }
}

// ---------- block payload ----------

/// Accumulates records for one block, interning strings and sources per block.
pub struct BlockBuilder {
    strings: Vec<String>,
    string_ix: HashMap<String, u32>,
    sources: Vec<Source>,
    source_ix: HashMap<u64, u32>,
    records: Vec<u8>,
    pub summary: Summary,
    terms: Vec<u64>,
    keys: std::collections::BTreeSet<u64>,
    srcs: std::collections::BTreeSet<u64>,
    approx_len: usize,
}

impl BlockBuilder {
    pub fn new() -> Self {
        BlockBuilder {
            strings: Vec::new(),
            string_ix: HashMap::new(),
            sources: Vec::new(),
            source_ix: HashMap::new(),
            records: Vec::new(),
            summary: Summary { min_time: i64::MAX, max_time: i64::MIN, ..Default::default() },
            terms: Vec::new(),
            keys: Default::default(),
            srcs: Default::default(),
            approx_len: 0,
        }
    }

    pub fn is_empty(&self) -> bool { self.summary.count == 0 }
    pub fn approx_len(&self) -> usize { self.approx_len }

    fn intern(&mut self, s: &str) -> u32 {
        if let Some(&i) = self.string_ix.get(s) { return i; }
        let i = self.strings.len() as u32;
        self.approx_len += s.len() + 4;
        self.strings.push(s.to_string());
        self.string_ix.insert(s.to_string(), i);
        i
    }

    fn add_term(&mut self, t: &str) {
        // only terms of 3+ chars go in the filter; the planner never prunes on shorter ones
        if t.chars().count() >= 3 { self.terms.push(hash(t.as_bytes())); }
    }

    pub fn push(&mut self, r: &Record) {
        let fp = r.source.fingerprint();
        let src_ref = match self.source_ix.get(&fp) {
            Some(&i) => i,
            None => {
                let i = self.sources.len() as u32;
                for (k, v) in r.source.attrs.clone() { self.intern(&k); self.intern(&v); }
                self.sources.push(r.source.clone());
                self.source_ix.insert(fp, i);
                i
            }
        };
        self.srcs.insert(fp);
        let body_ref = self.intern(&r.body);
        for t in tokens(&r.body).collect::<Vec<_>>() { self.add_term(&t); }

        let start = self.records.len();
        let b = &mut self.records;
        put_u32(b, 0); // patched below
        b.push(1); // kind: log
        b.push(r.severity);
        put_u16(b, 0x0008); // flags: body_is_string
        put_i64(b, r.time_ns);
        put_i64(b, r.observed_ns);
        put_u32(b, src_ref);
        put_u32(b, NONE_REF); // template
        put_u16(b, r.attrs.len() as u16);
        put_u16(b, 0); // args
        put_u32(b, body_ref);
        b.extend_from_slice(&[0u8; 16]); // trace id
        b.extend_from_slice(&[0u8; 8]); // span id
        put_u64(b, r.seq);
        debug_assert_eq!(self.records.len() - start, RECORD_FIXED_LEN);

        let mut var = Vec::new();
        for (k, v) in &r.attrs {
            let kr = self.intern(k);
            put_u32(&mut var, kr);
            self.keys.insert(hash(k.as_bytes()));
            match v {
                Value::Null => var.push(0),
                Value::Bool(x) => { var.push(1); var.push(*x as u8); }
                Value::I64(x) => { var.push(2); put_i64(&mut var, *x); }
                Value::F64(x) => { var.push(3); var.extend_from_slice(&x.to_le_bytes()); }
                Value::Str(s) => {
                    let vr = self.intern(s);
                    var.push(4);
                    put_u32(&mut var, vr);
                    for t in tokens(s).collect::<Vec<_>>() { self.add_term(&t); }
                }
            }
            self.terms.push(kv_hash(k, &v.as_display()));
        }
        self.records.extend_from_slice(&var);
        while self.records.len() % 8 != 0 { self.records.push(0); }
        let len = (self.records.len() - start) as u32;
        self.records[start..start + 4].copy_from_slice(&len.to_le_bytes());
        self.approx_len += len as usize;

        let s = &mut self.summary;
        if s.count == 0 { s.first_seq = r.seq; }
        s.last_seq = r.seq;
        s.count += 1;
        s.min_time = s.min_time.min(r.time_ns);
        s.max_time = s.max_time.max(r.time_ns);
        s.severities |= 1 << (r.severity.min(31));
    }

    /// Returns (uncompressed payload, summary).
    pub fn finish(mut self, with_bloom: bool) -> (Vec<u8>, Summary) {
        let mut p = Vec::with_capacity(self.approx_len + 64);
        put_u32(&mut p, self.strings.len() as u32);
        for s in &self.strings { put_u32(&mut p, s.len() as u32); p.extend_from_slice(s.as_bytes()); }
        put_u32(&mut p, self.sources.len() as u32);
        for s in &self.sources {
            put_u64(&mut p, s.fingerprint());
            p.push(s.kind);
            put_u16(&mut p, s.attrs.len() as u16);
            for (k, v) in &s.attrs {
                put_u32(&mut p, self.string_ix[k]);
                put_u32(&mut p, self.string_ix[v]);
            }
        }
        while p.len() % 8 != 0 { p.push(0); }
        p.extend_from_slice(&self.records);

        self.terms.sort_unstable();
        self.terms.dedup();
        let mut bloom = if with_bloom { Bloom::with_capacity(self.terms.len()) } else { Bloom::default() };
        if with_bloom { for t in &self.terms { bloom.insert(*t); } }
        self.summary.bloom = bloom;
        self.summary.sources = self.srcs.into_iter().collect();
        self.summary.keys = self.keys.into_iter().collect();
        (p, self.summary)
    }
}

/// Decodes a whole uncompressed payload back into records.
pub fn decode_payload(p: &[u8], count: u32) -> Result<Vec<Record>> {
    let mut o = 0;
    let n = get_u32(p, o) as usize; o += 4;
    let mut strings = Vec::with_capacity(n);
    for _ in 0..n {
        let l = get_u32(p, o) as usize; o += 4;
        strings.push(std::str::from_utf8(&p[o..o + l])?.to_string());
        o += l;
    }
    let n = get_u32(p, o) as usize; o += 4;
    let mut sources = Vec::with_capacity(n);
    for _ in 0..n {
        o += 8; // fingerprint
        let kind = p[o]; o += 1;
        let na = get_u16(p, o) as usize; o += 2;
        let mut attrs = Vec::with_capacity(na);
        for _ in 0..na {
            let k = get_u32(p, o) as usize; let v = get_u32(p, o + 4) as usize; o += 8;
            attrs.push((strings[k].clone(), strings[v].clone()));
        }
        sources.push(Source { kind, attrs });
    }
    while o % 8 != 0 { o += 1; }
    let mut out = Vec::with_capacity(count as usize);
    for _ in 0..count {
        let len = get_u32(p, o) as usize;
        let severity = p[o + 5];
        let time_ns = get_i64(p, o + 8);
        let observed_ns = get_i64(p, o + 16);
        let src = get_u32(p, o + 24) as usize;
        let attr_count = get_u16(p, o + 32) as usize;
        let body = get_u32(p, o + 36) as usize;
        let seq = get_u64(p, o + 64);
        let mut v = o + RECORD_FIXED_LEN;
        let mut attrs = Vec::with_capacity(attr_count);
        for _ in 0..attr_count {
            let k = get_u32(p, v) as usize; v += 4;
            let tag = p[v]; v += 1;
            let val = match tag {
                0 => Value::Null,
                1 => { v += 1; Value::Bool(p[v - 1] != 0) }
                2 => { v += 8; Value::I64(get_i64(p, v - 8)) }
                3 => { v += 8; Value::F64(f64::from_le_bytes(p[v - 8..v].try_into().unwrap())) }
                4 => { v += 4; Value::Str(strings[get_u32(p, v - 4) as usize].clone()) }
                t => bail!("bad value tag {t}"),
            };
            attrs.push((strings[k].clone(), val));
        }
        out.push(Record {
            seq, time_ns, observed_ns, severity,
            source: sources[src].clone(),
            body: strings[body].clone(),
            attrs,
        });
        o += len;
    }
    Ok(out)
}

// ---------- block framing ----------

pub struct BlockHeader {
    pub block_seq: u64,
    pub record_count: u32,
    pub uncompressed_len: u32,
    pub compressed_len: u32,
    pub summary_len: u32,
    pub payload_crc: u32,
    pub summary_crc: u32,
}

impl BlockHeader {
    pub fn total_len(&self) -> usize {
        BLOCK_HEADER_LEN + self.compressed_len as usize + self.summary_len as usize + BLOCK_TRAILER_LEN
    }

    pub fn decode(b: &[u8]) -> Option<BlockHeader> {
        if b.len() < BLOCK_HEADER_LEN || get_u32(b, 0) != BLOCK_MAGIC { return None; }
        if crc32c::crc32c(&b[0..44]) != get_u32(b, 44) { return None; }
        Some(BlockHeader {
            block_seq: get_u64(b, 8),
            record_count: get_u32(b, 16),
            uncompressed_len: get_u32(b, 20),
            compressed_len: get_u32(b, 24),
            summary_len: get_u32(b, 28),
            payload_crc: get_u32(b, 32),
            summary_crc: get_u32(b, 36),
        })
    }
}

/// Encodes a full block: header, compressed payload, summary, trailer.
pub fn encode_block(block_seq: u64, count: u32, payload: &[u8], summary: &Summary, level: i32) -> Vec<u8> {
    let compressed = zstd::bulk::compress(payload, level).expect("zstd");
    let sum = summary.encode();
    let mut b = Vec::with_capacity(BLOCK_HEADER_LEN + compressed.len() + sum.len() + BLOCK_TRAILER_LEN);
    put_u32(&mut b, BLOCK_MAGIC);
    put_u16(&mut b, 1); // version
    b.push(1); // codec zstd
    b.push(0); // flags
    put_u64(&mut b, block_seq);
    put_u32(&mut b, count);
    put_u32(&mut b, payload.len() as u32);
    put_u32(&mut b, compressed.len() as u32);
    put_u32(&mut b, sum.len() as u32);
    put_u32(&mut b, crc32c::crc32c(&compressed));
    put_u32(&mut b, crc32c::crc32c(&sum));
    put_u32(&mut b, 0);
    let crc = crc32c::crc32c(&b);
    put_u32(&mut b, crc);
    debug_assert_eq!(b.len(), BLOCK_HEADER_LEN);
    b.extend_from_slice(&compressed);
    b.extend_from_slice(&sum);
    let total = (b.len() + BLOCK_TRAILER_LEN) as u32;
    let mut t = Vec::with_capacity(16);
    put_u32(&mut t, total);
    put_u32(&mut t, block_seq as u32);
    let tcrc = crc32c::crc32c(&t);
    put_u32(&mut t, tcrc);
    put_u32(&mut t, TRAILER_MAGIC);
    b.extend_from_slice(&t);
    b
}
