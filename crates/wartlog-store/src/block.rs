//! Block payloads: building (with per-block interning) and zero-copy reading.
//!
//! Payload layout (uncompressed):
//!   StringTable  u32 count, then per entry: u32 len, bytes
//!   SourceTable  u32 count, then per entry: u64 fingerprint, u8 kind, u16 attr_count, attr_count x (u32 key_ref, u32 value_ref)
//!   pad to 8
//!   Records      each 8-byte aligned: 72-byte fixed part, then the variable part

use anyhow::{bail, Context, Result};
use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;
use wartlog_model::{hash, Record, Source, SourceKind, SpanData, Value};

use crate::format::*;

/// Accumulates records for one block.
pub struct BlockBuilder {
    strings: Vec<Vec<u8>>,
    string_ix: HashMap<Vec<u8>, u32>,
    sources: Vec<Arc<Source>>,
    source_ix: HashMap<u64, u32>,
    records: Vec<u8>,
    summary: Summary,
    keys: BTreeSet<u64>,
    srcs: BTreeSet<u64>,
    approx_len: usize,
}

impl Default for BlockBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl BlockBuilder {
    pub fn new() -> Self {
        BlockBuilder {
            strings: Vec::new(),
            string_ix: HashMap::new(),
            sources: Vec::new(),
            source_ix: HashMap::new(),
            records: Vec::new(),
            summary: Summary { min_time: i64::MAX, max_time: i64::MIN, max_observed: i64::MIN, ..Default::default() },
            keys: BTreeSet::new(),
            srcs: BTreeSet::new(),
            approx_len: 16,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.summary.record_count() == 0
    }
    pub fn len(&self) -> u32 {
        self.summary.record_count()
    }
    /// Approximate uncompressed payload size so far.
    pub fn approx_len(&self) -> usize {
        self.approx_len
    }
    pub fn first_seq(&self) -> Option<u64> {
        (!self.is_empty()).then_some(self.summary.first_seq)
    }

    fn intern(&mut self, s: &[u8]) -> u32 {
        if let Some(&i) = self.string_ix.get(s) {
            return i;
        }
        let i = self.strings.len() as u32;
        self.approx_len += s.len() + 4;
        self.strings.push(s.to_vec());
        self.string_ix.insert(s.to_vec(), i);
        i
    }

    fn put_value(&mut self, out: &mut Vec<u8>, v: &Value) {
        match v {
            Value::Null => out.push(T_NULL),
            Value::Bool(x) => {
                out.push(T_BOOL);
                out.push(*x as u8);
            }
            Value::I64(x) => {
                out.push(T_I64);
                put_i64(out, *x);
            }
            Value::F64(x) => {
                out.push(T_F64);
                out.extend_from_slice(&x.to_le_bytes());
            }
            Value::Timestamp(x) => {
                out.push(T_TIMESTAMP);
                put_i64(out, *x);
            }
            Value::Duration(x) => {
                out.push(T_DURATION);
                put_i64(out, *x);
            }
            Value::Str(s) => {
                let r = self.intern(s.as_bytes());
                out.push(T_STR);
                put_u32(out, r);
            }
            Value::Json(s) => {
                let r = self.intern(s.as_bytes());
                out.push(T_JSON);
                put_u32(out, r);
            }
            Value::Bytes(b) => {
                let r = self.intern(b);
                out.push(T_BYTES);
                put_u32(out, r);
            }
        }
    }

    /// Appends a record. `r.seq` must already be assigned and increasing.
    pub fn push(&mut self, r: &Record) {
        let fp = r.source.fingerprint();
        let src_ref = match self.source_ix.get(&fp) {
            Some(&i) => i,
            None => {
                for (k, v) in &r.source.attrs {
                    self.intern(k.as_bytes());
                    self.intern(v.as_bytes());
                }
                let i = self.sources.len() as u32;
                self.sources.push(r.source.clone());
                self.source_ix.insert(fp, i);
                i
            }
        };
        self.srcs.insert(fp);
        let body_ref = self.intern(r.body.as_bytes());
        let tmpl_ref = match &r.template {
            Some(t) => self.intern(t.as_bytes()),
            None => NONE_REF,
        };
        let mut flags = F_BODY_STRING;
        if r.trace_id.is_some() {
            flags |= F_TRACE;
        }
        if r.span_id.is_some() {
            flags |= F_SPAN;
        }
        if r.template.is_some() {
            flags |= F_TEMPLATE;
        }
        if r.severity_text.is_some() {
            flags |= F_SEVERITY_TEXT;
        }

        let mut var = Vec::with_capacity(16 + r.attrs.len() * 9);
        if let Some(sp) = &r.span {
            var.extend_from_slice(&sp.parent_span_id.unwrap_or_default());
            put_i64(&mut var, sp.end_ns);
            let n = self.intern(sp.name.as_bytes());
            put_u32(&mut var, n);
            var.push(sp.kind);
            var.push(sp.status);
            put_u16(&mut var, if sp.parent_span_id.is_some() { SF_PARENT } else { 0 });
            let m = self.intern(sp.status_message.as_bytes());
            put_u32(&mut var, m);
        }
        if let Some(st) = &r.severity_text {
            let s = self.intern(st.as_bytes());
            put_u32(&mut var, s);
        }
        for a in &r.args {
            self.put_value(&mut var, a);
        }
        for (k, v) in &r.attrs {
            let kr = self.intern(k.as_bytes());
            put_u32(&mut var, kr);
            self.keys.insert(hash(k.as_bytes()));
            self.put_value(&mut var, v);
        }

        let start = self.records.len();
        let b = &mut self.records;
        put_u32(b, 0); // patched below
        b.push(if r.span.is_some() { 2 } else { 1 });
        b.push(r.severity);
        put_u16(b, flags);
        put_i64(b, r.time_ns);
        put_i64(b, r.observed_ns);
        put_u32(b, src_ref);
        put_u32(b, tmpl_ref);
        put_u16(b, r.attrs.len() as u16);
        put_u16(b, r.args.len() as u16);
        put_u32(b, body_ref);
        b.extend_from_slice(&r.trace_id.unwrap_or_default());
        b.extend_from_slice(&r.span_id.unwrap_or_default());
        put_u64(b, r.seq);
        debug_assert_eq!(b.len() - start, RECORD_FIXED_LEN);
        b.extend_from_slice(&var);
        while b.len() % 8 != 0 {
            b.push(0);
        }
        let len = (b.len() - start) as u32;
        b[start..start + 4].copy_from_slice(&len.to_le_bytes());
        self.approx_len += len as usize;

        let s = &mut self.summary;
        if s.record_count() == 0 {
            s.first_seq = r.seq;
        }
        s.last_seq = r.seq;
        if r.span.is_some() {
            s.span_count += 1;
        } else {
            s.log_count += 1;
        }
        s.min_time = s.min_time.min(r.time_ns);
        s.max_time = s.max_time.max(r.time_ns);
        s.max_observed = s.max_observed.max(r.observed_ns);
        s.severities |= 1 << r.severity.min(31);
    }

    /// Returns (uncompressed payload, summary).
    pub fn finish(mut self) -> (Vec<u8>, Summary) {
        let mut p = Vec::with_capacity(self.approx_len + 64);
        put_u32(&mut p, self.strings.len() as u32);
        for s in &self.strings {
            put_u32(&mut p, s.len() as u32);
            p.extend_from_slice(s);
        }
        put_u32(&mut p, self.sources.len() as u32);
        for s in &self.sources {
            put_u64(&mut p, s.fingerprint());
            p.push(s.kind.code());
            put_u16(&mut p, s.attrs.len() as u16);
            for (k, v) in &s.attrs {
                put_u32(&mut p, self.string_ix[k.as_bytes()]);
                put_u32(&mut p, self.string_ix[v.as_bytes()]);
            }
        }
        while p.len() % 8 != 0 {
            p.push(0);
        }
        p.extend_from_slice(&self.records);
        self.summary.sources = self.srcs.into_iter().collect();
        self.summary.keys = self.keys.into_iter().collect();
        (p, self.summary)
    }
}

/// Encodes a full block: header, compressed payload, summary, trailer.
pub fn encode_block(block_seq: u64, payload: &[u8], summary: &Summary, zstd_level: i32) -> Vec<u8> {
    let compressed = zstd::bulk::compress(payload, zstd_level).expect("zstd compression cannot fail on a buffer");
    let sum = summary.encode();
    let h = BlockHeader {
        codec: CODEC_ZSTD,
        block_seq,
        record_count: summary.record_count(),
        uncompressed_len: payload.len() as u32,
        compressed_len: compressed.len() as u32,
        summary_len: sum.len() as u32,
        payload_crc: crc32c::crc32c(&compressed),
        summary_crc: crc32c::crc32c(&sum),
    };
    let mut b = h.encode();
    b.reserve(compressed.len() + sum.len() + BLOCK_TRAILER_LEN);
    b.extend_from_slice(&compressed);
    b.extend_from_slice(&sum);
    let total = (b.len() + BLOCK_TRAILER_LEN) as u32;
    b.extend_from_slice(&encode_trailer(total, block_seq));
    b
}

/// Verifies and decompresses a block payload.
pub fn decompress(h: &BlockHeader, compressed: &[u8]) -> Result<Vec<u8>> {
    if crc32c::crc32c(compressed) != h.payload_crc {
        bail!("block {} payload CRC mismatch", h.block_seq);
    }
    let p = match h.codec {
        CODEC_ZSTD => zstd::bulk::decompress(compressed, h.uncompressed_len as usize).context("zstd")?,
        CODEC_NONE => compressed.to_vec(),
        c => bail!("unknown codec {c}"),
    };
    if p.len() != h.uncompressed_len as usize {
        bail!("block {} decompressed to the wrong length", h.block_seq);
    }
    Ok(p)
}

/// A decoded block, read without materializing records.
pub struct BlockView {
    p: Vec<u8>,
    strings: Vec<(u32, u32)>,
    pub sources: Vec<Arc<Source>>,
    offsets: Vec<u32>,
}

impl BlockView {
    pub fn parse(p: Vec<u8>, count: u32) -> Result<BlockView> {
        let mut c = Cursor::new(&p);
        let n = c.u32()? as usize;
        let mut strings = Vec::with_capacity(n);
        for _ in 0..n {
            let l = c.u32()? as usize;
            let o = c.o;
            c.take(l)?;
            strings.push((o as u32, l as u32));
        }
        let str_at = |i: u32| -> Result<String> {
            let &(o, l) = strings.get(i as usize).context("bad string ref")?;
            Ok(String::from_utf8_lossy(&p[o as usize..(o + l) as usize]).into_owned())
        };
        let n = c.u32()? as usize;
        let mut sources = Vec::with_capacity(n);
        for _ in 0..n {
            c.u64()?;
            let kind = SourceKind::from_u8(c.u8()?).context("bad source kind")?;
            let na = c.u16()? as usize;
            let mut attrs = Vec::with_capacity(na);
            for _ in 0..na {
                let k = c.u32()?;
                let v = c.u32()?;
                attrs.push((str_at(k)?, str_at(v)?));
            }
            sources.push(Arc::new(Source { kind, attrs }));
        }
        c.align8();
        let mut offsets = Vec::with_capacity(count as usize);
        for _ in 0..count {
            let o = c.o;
            if o + RECORD_FIXED_LEN > p.len() {
                bail!("truncated record");
            }
            let len = get_u32(&p, o) as usize;
            if len < RECORD_FIXED_LEN || o + len > p.len() || get_u32(&p, o + 24) as usize >= sources.len() {
                bail!("bad record at {o}");
            }
            offsets.push(o as u32);
            c.o = o + len;
        }
        Ok(BlockView { p, strings, sources, offsets })
    }

    pub fn len(&self) -> usize {
        self.offsets.len()
    }
    pub fn is_empty(&self) -> bool {
        self.offsets.is_empty()
    }

    pub fn str_bytes(&self, i: u32) -> &[u8] {
        match self.strings.get(i as usize) {
            Some(&(o, l)) => &self.p[o as usize..(o + l) as usize],
            None => &[],
        }
    }

    pub fn str(&self, i: u32) -> &str {
        std::str::from_utf8(self.str_bytes(i)).unwrap_or("\u{FFFD}")
    }

    pub fn string_count(&self) -> usize {
        self.strings.len()
    }

    /// Index of a string in this block's table, if present.
    pub fn find_string(&self, s: &str) -> Option<u32> {
        (0..self.strings.len() as u32).find(|&i| self.str_bytes(i) == s.as_bytes())
    }

    pub fn rec(&self, i: usize) -> Rec<'_> {
        Rec { v: self, o: self.offsets[i] as usize }
    }

    pub fn iter(&self) -> impl Iterator<Item = Rec<'_>> {
        (0..self.len()).map(move |i| self.rec(i))
    }
}

/// One record inside a `BlockView`. Fixed fields are read at known offsets.
#[derive(Clone, Copy)]
pub struct Rec<'a> {
    pub v: &'a BlockView,
    o: usize,
}

impl<'a> Rec<'a> {
    fn p(&self) -> &'a [u8] {
        &self.v.p
    }
    pub fn kind(&self) -> u8 {
        self.p()[self.o + 4]
    }
    pub fn is_span(&self) -> bool {
        self.kind() == 2
    }
    pub fn severity(&self) -> u8 {
        self.p()[self.o + 5]
    }
    pub fn flags(&self) -> u16 {
        get_u16(self.p(), self.o + 6)
    }
    pub fn time(&self) -> i64 {
        get_i64(self.p(), self.o + 8)
    }
    pub fn observed(&self) -> i64 {
        get_i64(self.p(), self.o + 16)
    }
    pub fn source_ref(&self) -> u32 {
        get_u32(self.p(), self.o + 24)
    }
    pub fn source(&self) -> &'a Arc<Source> {
        &self.v.sources[self.source_ref() as usize]
    }
    pub fn template_ref(&self) -> u32 {
        get_u32(self.p(), self.o + 28)
    }
    pub fn attr_count(&self) -> usize {
        get_u16(self.p(), self.o + 32) as usize
    }
    pub fn arg_count(&self) -> usize {
        get_u16(self.p(), self.o + 34) as usize
    }
    pub fn body_ref(&self) -> u32 {
        get_u32(self.p(), self.o + 36)
    }
    pub fn trace_id(&self) -> Option<[u8; 16]> {
        (self.flags() & F_TRACE != 0).then(|| self.p()[self.o + 40..self.o + 56].try_into().unwrap())
    }
    pub fn span_id(&self) -> Option<[u8; 8]> {
        (self.flags() & F_SPAN != 0).then(|| self.p()[self.o + 56..self.o + 64].try_into().unwrap())
    }
    pub fn seq(&self) -> u64 {
        get_u64(self.p(), self.o + 64)
    }
    pub fn template(&self) -> Option<&'a str> {
        (self.flags() & F_TEMPLATE != 0).then(|| self.v.str(self.template_ref()))
    }
    /// Raw body string (the template text is separate).
    pub fn raw_body(&self) -> &'a str {
        self.v.str(self.body_ref())
    }

    fn var_start(&self) -> usize {
        self.o + RECORD_FIXED_LEN
    }

    pub fn span(&self) -> Option<SpanData> {
        if !self.is_span() {
            return None;
        }
        let p = self.p();
        let o = self.var_start();
        let parent: [u8; 8] = p[o..o + 8].try_into().ok()?;
        let sf = get_u16(p, o + 22);
        Some(SpanData {
            parent_span_id: (sf & SF_PARENT != 0).then_some(parent),
            end_ns: get_i64(p, o + 8),
            name: self.v.str(get_u32(p, o + 16)).to_string(),
            kind: p[o + 20],
            status: p[o + 21],
            status_message: self.v.str(get_u32(p, o + 24)).to_string(),
        })
    }

    /// Span name without decoding the rest of the span extension.
    pub fn span_name(&self) -> Option<&'a str> {
        self.is_span().then(|| self.v.str(get_u32(self.p(), self.var_start() + 16)))
    }

    fn after_ext(&self) -> usize {
        let mut o = self.var_start();
        if self.is_span() {
            o += SPAN_EXT_LEN;
        }
        o
    }

    pub fn severity_text(&self) -> Option<&'a str> {
        (self.flags() & F_SEVERITY_TEXT != 0).then(|| self.v.str(get_u32(self.p(), self.after_ext())))
    }

    fn values_start(&self) -> usize {
        let mut o = self.after_ext();
        if self.flags() & F_SEVERITY_TEXT != 0 {
            o += 4;
        }
        o
    }

    fn read_value(&self, o: &mut usize) -> Value {
        let p = self.p();
        let tag = p[*o];
        *o += 1;
        let v = match tag {
            T_BOOL => {
                *o += 1;
                Value::Bool(p[*o - 1] != 0)
            }
            T_I64 | T_TIMESTAMP | T_DURATION | T_F64 => {
                *o += 8;
                let raw = get_i64(p, *o - 8);
                match tag {
                    T_I64 => Value::I64(raw),
                    T_TIMESTAMP => Value::Timestamp(raw),
                    T_DURATION => Value::Duration(raw),
                    _ => Value::F64(f64::from_bits(raw as u64)),
                }
            }
            T_STR | T_JSON | T_BYTES => {
                *o += 4;
                let r = get_u32(p, *o - 4);
                match tag {
                    T_STR => Value::Str(self.v.str(r).to_string()),
                    T_JSON => Value::Json(self.v.str(r).to_string()),
                    _ => Value::Bytes(self.v.str_bytes(r).to_vec()),
                }
            }
            T_NULL => Value::Null,
            // compatibility policy: a tag this build does not know is a u32 string ref; show its raw bytes
            _ => {
                *o += 4;
                let r = get_u32(p, *o - 4);
                Value::Bytes(self.v.str_bytes(r).to_vec())
            }
        };
        v
    }

    fn skip_value(p: &[u8], o: &mut usize) {
        let tag = p[*o];
        *o += 1 + match tag {
            T_BOOL => 1,
            T_I64 | T_TIMESTAMP | T_DURATION | T_F64 => 8,
            T_NULL => 0,
            // T_STR, T_JSON, T_BYTES, and any tag this build does not know (compatibility policy)
            _ => 4,
        };
    }

    pub fn args(&self) -> Vec<Value> {
        let mut o = self.values_start();
        (0..self.arg_count()).map(|_| self.read_value(&mut o)).collect()
    }

    fn attrs_start(&self) -> usize {
        let p = self.p();
        let mut o = self.values_start();
        for _ in 0..self.arg_count() {
            Self::skip_value(p, &mut o);
        }
        o
    }

    pub fn attrs(&self) -> Vec<(String, Value)> {
        let p = self.p();
        let mut o = self.attrs_start();
        (0..self.attr_count())
            .map(|_| {
                let k = self.v.str(get_u32(p, o)).to_string();
                o += 4;
                (k, self.read_value(&mut o))
            })
            .collect()
    }

    /// Visits attributes as (key, value) without allocating the key.
    pub fn for_each_attr(&self, mut f: impl FnMut(&'a str, Value)) {
        let p = self.p();
        let mut o = self.attrs_start();
        for _ in 0..self.attr_count() {
            let k = self.v.str(get_u32(p, o));
            o += 4;
            let v = self.read_value(&mut o);
            f(k, v);
        }
    }

    /// One attribute by key.
    pub fn attr(&self, key: &str) -> Option<Value> {
        let p = self.p();
        let mut o = self.attrs_start();
        for _ in 0..self.attr_count() {
            let k = get_u32(p, o);
            o += 4;
            if self.v.str_bytes(k) == key.as_bytes() {
                return Some(self.read_value(&mut o));
            }
            Self::skip_value(p, &mut o);
        }
        None
    }

    /// Body text: the template rendered with its args when there is one.
    pub fn body(&self) -> String {
        match self.template() {
            Some(t) => wartlog_model::render(t, &self.args()),
            None => self.raw_body().to_string(),
        }
    }

    pub fn to_record(&self) -> Record {
        Record {
            seq: self.seq(),
            time_ns: self.time(),
            observed_ns: self.observed(),
            severity: self.severity(),
            severity_text: self.severity_text().map(str::to_string),
            body: self.raw_body().to_string(),
            template: self.template().map(str::to_string),
            args: self.args(),
            attrs: self.attrs(),
            source: self.source().clone(),
            trace_id: self.trace_id(),
            span_id: self.span_id(),
            span: self.span(),
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use wartlog_model::SourceKind;

    pub fn sample(seq: u64) -> Record {
        let src = Arc::new(Source::new(SourceKind::Otlp, vec![("service.name".into(), format!("svc{}", seq % 3))]));
        let mut r = Record::log(src, 1_000 + seq as i64, 9 + (seq % 9) as u8, "");
        r.seq = seq;
        r.observed_ns = 5_000 + seq as i64;
        if seq % 2 == 0 {
            r.template = Some("GET {Path} took {Ms:F1} ms".into());
            r.args = vec![Value::Str(format!("/p/{seq}")), Value::F64(seq as f64 / 3.0)];
        } else {
            r.body = format!("plain message {seq}");
        }
        r.attrs = vec![
            ("http.status".into(), Value::I64(200 + (seq % 5) as i64)),
            ("ok".into(), Value::Bool(seq % 3 == 0)),
            ("user".into(), Value::Str(format!("u{}", seq % 7))),
            ("blob".into(), Value::Bytes(vec![0, 255, seq as u8])),
            ("when".into(), Value::Timestamp(seq as i64)),
            ("took".into(), Value::Duration(-(seq as i64))),
            ("j".into(), Value::Json("[1,2]".into())),
            ("nothing".into(), Value::Null),
        ];
        if seq % 4 == 0 {
            r.trace_id = Some([seq as u8; 16]);
            r.span_id = Some([1; 8]);
            r.severity_text = Some("Information".into());
        }
        if seq % 5 == 0 {
            r.span = Some(SpanData { parent_span_id: (seq % 10 == 0).then_some([2; 8]), end_ns: 99, name: "op".into(), kind: 2, status: 1, status_message: "fine".into() });
        }
        r
    }

    #[test]
    fn unknown_source_kind_and_value_tag_are_kept() {
        // compatibility policy: a newer writer's source kind and value tag still read
        let src = Arc::new(Source::new(SourceKind::Other(42), vec![("x".into(), "y".into())]));
        let mut r = Record::log(src, 1, 9, "hi");
        r.seq = 1;
        r.attrs = vec![("a".into(), Value::Str("raw".into())), ("b".into(), Value::I64(7))];
        let mut b = BlockBuilder::new();
        b.push(&r);
        let (mut p, _) = b.finish();
        let v = BlockView::parse(p.clone(), 1).unwrap();
        assert_eq!(v.rec(0).source().kind, SourceKind::Other(42));
        assert_eq!(v.rec(0).source().display_name(), "kind:42");
        // rewrite attribute a's tag (T_STR) to a tag this build does not know
        let tag_at = v.rec(0).attrs_start() + 4;
        assert_eq!(p[tag_at], T_STR);
        p[tag_at] = 200;
        let v = BlockView::parse(p, 1).unwrap();
        let attrs = v.rec(0).attrs();
        assert_eq!(attrs[0], ("a".to_string(), Value::Bytes(b"raw".to_vec())));
        // the following value still decodes: the unknown tag was skipped by its 4-byte width
        assert_eq!(attrs[1], ("b".to_string(), Value::I64(7)));
    }

    #[test]
    fn records_round_trip() {
        let mut b = BlockBuilder::new();
        let recs: Vec<Record> = (1..=50).map(sample).collect();
        for r in &recs {
            b.push(r);
        }
        let (p, s) = b.finish();
        assert_eq!(s.record_count(), 50);
        assert_eq!((s.first_seq, s.last_seq), (1, 50));
        assert_eq!(s.span_count, 10);
        let v = BlockView::parse(p, 50).unwrap();
        for (i, r) in recs.iter().enumerate() {
            let got = v.rec(i).to_record();
            assert_eq!(&got, r, "record {i}");
            assert_eq!(v.rec(i).attr("user"), r.attr("user").cloned());
            assert_eq!(v.rec(i).body(), r.rendered_body());
        }
        assert_eq!(v.rec(1).body(), "GET /p/2 took 0.7 ms");
    }

    #[test]
    fn block_encode_and_damage() {
        let mut b = BlockBuilder::new();
        for i in 1..=10 {
            b.push(&sample(i));
        }
        let (p, s) = b.finish();
        let bytes = encode_block(3, &p, &s, 3);
        let h = BlockHeader::decode(&bytes).unwrap();
        assert_eq!(h.total_len(), bytes.len());
        let payload = &bytes[BLOCK_HEADER_LEN..BLOCK_HEADER_LEN + h.compressed_len as usize];
        assert_eq!(decompress(&h, payload).unwrap(), p);
        let mut bad = payload.to_vec();
        bad[3] ^= 0xff;
        assert!(decompress(&h, &bad).is_err());
        assert!(BlockView::parse(p[..p.len() - 9].to_vec(), 10).is_err());
    }
}
