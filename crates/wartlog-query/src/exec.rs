//! Planning (pushdown and pruning) and execution against a store snapshot.

use rayon::prelude::*;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering as AtomicOrdering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Instant;
use wartlog_model::{time::now_ns, Record, Value};
use wartlog_store::{tix, BlockMeta, Snapshot};

use crate::ast::*;
use crate::eval::{self, builtin, compare, eval, truthy, Ctx, Fields, RecordLike, SourceFields};
use crate::QueryError;

#[derive(Clone, Debug, Default)]
pub struct QueryOptions {
    /// cap on returned rows when the query itself has no take/limit/summarize
    pub limit: Option<u64>,
    /// worker threads; None means every core
    pub threads: Option<usize>,
    /// injected time range when the query has none on Timestamp (inclusive ns)
    pub time_range: Option<(Option<i64>, Option<i64>)>,
    pub cancel: Option<Arc<AtomicBool>>,
    /// evaluate `now()` and `ago()` at this time (tests)
    pub now: Option<i64>,
}

#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct QueryStats {
    pub segments: usize,
    pub blocks: u64,
    pub blocks_pruned: u64,
    pub blocks_scanned: u64,
    pub blocks_damaged: u64,
    pub records_scanned: u64,
    pub wal_records: u64,
    pub rows: u64,
    pub elapsed_ms: f64,
    pub used_term_index: usize,
    pub truncated: bool,
}

#[derive(Clone, Debug, Default)]
pub struct QueryResult {
    pub columns: Vec<String>,
    pub rows: Vec<Vec<Value>>,
    /// record id (seq) of each row when the row is still one record
    pub ids: Vec<Option<u64>>,
    pub stats: QueryStats,
}

impl QueryResult {
    pub fn col(&self, name: &str) -> Option<usize> {
        self.columns.iter().position(|c| c == name)
    }
}

pub const LOG_COLUMNS: &[&str] = &[
    "Timestamp", "Level", "Source", "Body", "SeverityNumber", "SeverityText", "Template", "SourceKind", "TraceId", "SpanId", "ObservedTimestamp", "Attributes",
    "Resource", "Id",
];
pub const SPAN_COLUMNS: &[&str] = &[
    "Timestamp", "EndTimestamp", "Duration", "Name", "Kind", "Status", "StatusMessage", "Source", "TraceId", "SpanId", "ParentSpanId", "Attributes", "Resource",
    "Id",
];

/// A row moving through the pipeline. While `projected` is false the row is still the whole
/// record (`rec`) plus any extended columns.
#[derive(Clone, Debug)]
pub struct Row {
    pub rec: Option<Arc<Record>>,
    pub cols: Vec<(String, Value)>,
    pub projected: bool,
}

impl Row {
    fn get_named(&self, n: &str) -> Option<&Value> {
        self.cols.iter().rev().find(|(k, _)| k == n).map(|(_, v)| v)
    }
}

impl Fields for Row {
    fn col(&self, c: &Column) -> Value {
        let name = match c {
            Column::Named(n) => n.clone(),
            other => other.name(),
        };
        if let Some(v) = self.get_named(&name) {
            return v.clone();
        }
        match (&self.rec, self.projected) {
            (Some(r), false) => builtin(c, r.as_ref()),
            _ => Value::Null,
        }
    }
}

/// A record in a block during the scan, with the rendered body computed at most once.
struct RecFields<'a> {
    r: wartlog_store::Rec<'a>,
    body: std::cell::OnceCell<String>,
}

impl Fields for RecFields<'_> {
    fn col(&self, c: &Column) -> Value {
        match c {
            Column::Body => Value::Str(self.body.get_or_init(|| self.r.body()).clone()),
            other => builtin(other, &self.r),
        }
    }
}

/// `search "x"`: has-match on the body, attribute values and resource values.
fn search_match(r: &dyn RecordLike, body: &str, term: &str) -> bool {
    if eval::has(body, term, false) {
        return true;
    }
    if r.attrs().iter().any(|(k, v)| eval::has(&v.to_text(), term, false) || eval::has(k, term, false)) {
        return true;
    }
    r.source().attrs.iter().any(|(_, v)| eval::has(v, term, false))
}

// ---------- planning ----------

#[derive(Clone, Debug, PartialEq)]
enum Term {
    Exact(Vec<u8>),
    Prefix(Vec<u8>),
}

/// One index requirement: the block must contain at least one of these terms.
#[derive(Clone, Debug, PartialEq)]
struct Req {
    any: Vec<Term>,
    /// field terms only prune when the key is fully indexed in that segment
    key: Option<String>,
}

#[derive(Clone, Debug)]
pub struct Plan {
    pub table: Table,
    filters: Vec<Filter>,
    rest: Vec<Op>,
    time: (Option<i64>, Option<i64>),
    sev_mask: u32,
    sources: Option<HashSet<u64>>,
    reqs: Vec<Req>,
    limit: Option<u64>,
    default_order: bool,
    now: i64,
}

#[derive(Clone, Debug)]
enum Filter {
    Expr(Expr),
    Search(String),
}

fn is_const(e: &Expr) -> bool {
    let mut c = true;
    e.columns(&mut |_| c = false);
    c
}

struct NoFields;
impl Fields for NoFields {
    fn col(&self, _: &Column) -> Value {
        Value::Null
    }
}

fn const_val(e: &Expr, cx: &Ctx) -> Option<Value> {
    is_const(e).then(|| eval(e, &NoFields, cx))
}

fn conjuncts(e: &Expr, out: &mut Vec<Expr>) {
    match e {
        Expr::Bin(BinOp::And, a, b) => {
            conjuncts(a, out);
            conjuncts(b, out);
        }
        other => out.push(other.clone()),
    }
}

fn sev_range_mask(lo: u8, hi: u8) -> u32 {
    let mut m = 0u32;
    for s in lo..=hi.min(31) {
        m |= 1 << s;
    }
    m
}

fn level_mask(name: &str) -> u32 {
    match wartlog_model::level_floor(name) {
        Some(lo) => sev_range_mask(lo, lo + 3),
        None if name.eq_ignore_ascii_case("unspecified") => 1,
        None => 0,
    }
}

fn validate(e: &Expr, allow_agg: bool) -> Result<(), QueryError> {
    match e {
        Expr::Call(f, args) => {
            if eval::is_aggregate(f) && !allow_agg && !eval::is_function(f) {
                return Err(QueryError::new(format!("{f}() can only be used in summarize")));
            }
            for a in args {
                validate(a, false)?;
            }
            Ok(())
        }
        Expr::Bin(_, a, b) => {
            validate(a, false)?;
            validate(b, false)
        }
        Expr::Not(a) | Expr::Neg(a) | Expr::Regex { lhs: a, .. } => validate(a, false),
        Expr::Str { lhs, rhs, .. } => {
            validate(lhs, false)?;
            validate(rhs, false)
        }
        Expr::In { lhs, list, .. } => {
            validate(lhs, false)?;
            list.iter().try_for_each(|x| validate(x, false))
        }
        Expr::Between { lhs, lo, hi, .. } => {
            validate(lhs, false)?;
            validate(lo, false)?;
            validate(hi, false)
        }
        _ => Ok(()),
    }
}

fn validate_query(q: &Query) -> Result<(), QueryError> {
    for op in &q.ops {
        match op {
            Op::Where(e) => validate(e, false)?,
            Op::Project(v) | Op::Extend(v) | Op::Distinct(v) => v.iter().try_for_each(|(_, e)| validate(e, false))?,
            Op::Sort(keys) => keys.iter().try_for_each(|k| validate(&k.expr, false))?,
            Op::Top(_, k) => validate(&k.expr, false)?,
            Op::Summarize { aggs, by } => {
                for (_, a) in aggs {
                    if let Some(x) = &a.arg {
                        validate(x, false)?;
                    }
                }
                by.iter().try_for_each(|(_, e)| validate(e, false))?;
            }
            _ => {}
        }
    }
    Ok(())
}

pub fn plan(q: &Query, snap: &Snapshot, opts: &QueryOptions) -> Result<Plan, QueryError> {
    validate_query(q)?;
    let now = opts.now.unwrap_or_else(now_ns);
    let cx = Ctx { now };
    let mut filters = Vec::new();
    let mut i = 0;
    while i < q.ops.len() {
        match &q.ops[i] {
            Op::Where(e) => filters.push(Filter::Expr(e.clone())),
            Op::Search(s) => filters.push(Filter::Search(s.clone())),
            _ => break,
        }
        i += 1;
    }
    let rest: Vec<Op> = q.ops[i..].to_vec();
    let mut p = Plan { table: q.table, filters: Vec::new(), rest, time: (None, None), sev_mask: u32::MAX, sources: None, reqs: Vec::new(), limit: None, default_order: false, now };

    let source_keys: HashSet<&str> = snap.catalog.sources.values().flat_map(|s| s.attrs.iter().map(|(k, _)| k.as_str())).collect();
    let mut has_time = false;
    for f in &filters {
        let Filter::Expr(e) = f else { continue };
        let mut cs = Vec::new();
        conjuncts(e, &mut cs);
        for c in cs {
            has_time |= push_time(&c, &cx, &mut p.time);
            push_severity(&c, &cx, &mut p.sev_mask);
            // source-level predicate: evaluate against each known source
            let mut cols = Vec::new();
            c.columns(&mut |col| cols.push(col.clone()));
            if !cols.is_empty() && cols.iter().all(|col| col.is_source_level()) {
                let allowed: HashSet<u64> = snap.catalog.sources.iter().filter(|(_, s)| truthy(&eval(&c, &SourceFields(s), &cx))).map(|(fp, _)| *fp).collect();
                p.sources = Some(match p.sources.take() {
                    Some(prev) => prev.intersection(&allowed).copied().collect(),
                    None => allowed,
                });
            }
            if let Some(r) = index_req(&c, &cx, &source_keys) {
                p.reqs.push(r);
            }
        }
    }
    for f in &filters {
        if let Filter::Search(s) = f {
            // every term of the search must appear somewhere; only the body is indexed, so no pruning
            let _ = s;
        }
    }
    if !has_time {
        if let Some((a, b)) = opts.time_range {
            if let Some(a) = a {
                p.time.0 = Some(p.time.0.map_or(a, |x: i64| x.max(a)));
            }
            if let Some(b) = b {
                p.time.1 = Some(p.time.1.map_or(b, |x: i64| x.min(b)));
            }
        }
    }
    if p.table == Table::Spans {
        // nothing extra: blocks without spans are skipped by span_count
    }
    p.filters = filters;
    // early-stop limit and default order
    let orders = p.rest.iter().any(|o| matches!(o, Op::Sort(_) | Op::Top(..) | Op::Summarize { .. } | Op::Distinct(_) | Op::Count));
    p.default_order = !orders;
    match p.rest.as_slice() {
        [] => p.limit = opts.limit,
        [Op::Take(n)] => p.limit = Some(opts.limit.map_or(*n, |l| l.min(*n))),
        _ => {}
    }
    Ok(p)
}

fn push_time(c: &Expr, cx: &Ctx, t: &mut (Option<i64>, Option<i64>)) -> bool {
    let ts = |v: Option<Value>| match v {
        Some(Value::Timestamp(x)) => Some(x),
        Some(Value::Str(s)) => wartlog_model::time::parse_datetime(&s),
        _ => None,
    };
    let lo = |t: &mut (Option<i64>, Option<i64>), x: i64| t.0 = Some(t.0.map_or(x, |y| y.max(x)));
    let hi = |t: &mut (Option<i64>, Option<i64>), x: i64| t.1 = Some(t.1.map_or(x, |y| y.min(x)));
    match c {
        Expr::Bin(op, a, b) => {
            let (op, v) = match (a.as_ref(), b.as_ref()) {
                (Expr::Col(Column::Timestamp), r) => (*op, ts(const_val(r, cx))),
                (l, Expr::Col(Column::Timestamp)) => {
                    let flipped = match op {
                        BinOp::Lt => BinOp::Gt,
                        BinOp::Le => BinOp::Ge,
                        BinOp::Gt => BinOp::Lt,
                        BinOp::Ge => BinOp::Le,
                        o => *o,
                    };
                    (flipped, ts(const_val(l, cx)))
                }
                _ => return false,
            };
            let Some(v) = v else { return false };
            match op {
                BinOp::Gt => lo(t, v + 1),
                BinOp::Ge => lo(t, v),
                BinOp::Lt => hi(t, v - 1),
                BinOp::Le => hi(t, v),
                BinOp::Eq => {
                    lo(t, v);
                    hi(t, v)
                }
                _ => return false,
            }
            true
        }
        Expr::Between { lhs, lo: a, hi: b, neg: false } if matches!(lhs.as_ref(), Expr::Col(Column::Timestamp)) => {
            match (ts(const_val(a, cx)), ts(const_val(b, cx))) {
                (Some(x), Some(y)) => {
                    lo(t, x);
                    hi(t, y);
                    true
                }
                _ => false,
            }
        }
        _ => false,
    }
}

fn push_severity(c: &Expr, cx: &Ctx, mask: &mut u32) {
    let m = match c {
        Expr::Bin(op, a, b) => match (a.as_ref(), const_val(b, cx)) {
            (Expr::Col(Column::SeverityNumber), Some(v)) => match v.as_i64() {
                Some(n) => {
                    let n = n.clamp(0, 31) as u8;
                    match op {
                        BinOp::Ge => sev_range_mask(n, 31),
                        BinOp::Gt => sev_range_mask(n.saturating_add(1), 31),
                        BinOp::Le => sev_range_mask(0, n),
                        BinOp::Lt if n > 0 => sev_range_mask(0, n - 1),
                        BinOp::Eq => 1 << n,
                        _ => return,
                    }
                }
                None => return,
            },
            (Expr::Col(Column::Level), Some(Value::Str(s))) if matches!(op, BinOp::Eq | BinOp::EqCi) => level_mask(&s),
            _ => return,
        },
        Expr::In { lhs, list, neg: false, .. } if matches!(lhs.as_ref(), Expr::Col(Column::Level)) => {
            let mut m = 0;
            for x in list {
                match const_val(x, cx) {
                    Some(Value::Str(s)) => m |= level_mask(&s),
                    _ => return,
                }
            }
            m
        }
        _ => return,
    };
    *mask &= m;
}

fn index_req(c: &Expr, cx: &Ctx, source_keys: &HashSet<&str>) -> Option<Req> {
    match c {
        Expr::Str { op, neg: false, lhs, rhs, .. } if matches!(lhs.as_ref(), Expr::Col(Column::Body)) => {
            let Some(Value::Str(s)) = const_val(rhs, cx) else { return None };
            match op {
                StrOp::Has => {
                    // all terms of the phrase; use the longest indexable one
                    let best = wartlog_model::tokens(&s).filter(|t| t.chars().count() >= wartlog_model::MIN_TERM_CHARS).max_by_key(|t| t.len())?;
                    Some(Req { any: vec![Term::Exact(tix::body_term(&best))], key: None })
                }
                StrOp::HasPrefix => {
                    let toks: Vec<String> = wartlog_model::tokens(&s).collect();
                    if toks.len() != 1 || toks[0].chars().count() < wartlog_model::MIN_TERM_CHARS {
                        return None;
                    }
                    Some(Req { any: vec![Term::Prefix(tix::body_term(&toks[0]))], key: None })
                }
                _ => None,
            }
        }
        Expr::Bin(BinOp::Eq | BinOp::EqCi, a, b) => {
            let (col, v) = match (a.as_ref(), b.as_ref()) {
                (Expr::Col(c), r) => (c, const_val(r, cx)?),
                (l, Expr::Col(c)) => (c, const_val(l, cx)?),
                _ => return None,
            };
            field_req(col, &[v], source_keys)
        }
        Expr::In { lhs, list, neg: false, .. } => {
            let Expr::Col(col) = lhs.as_ref() else { return None };
            let vals: Option<Vec<Value>> = list.iter().map(|x| const_val(x, cx)).collect();
            field_req(col, &vals?, source_keys)
        }
        _ => None,
    }
}

fn field_req(col: &Column, vals: &[Value], source_keys: &HashSet<&str>) -> Option<Req> {
    match col {
        Column::TraceId => {
            let any: Option<Vec<Term>> = vals
                .iter()
                .map(|v| {
                    let b = wartlog_model::unhex(v.as_str()?.trim())?;
                    let id: [u8; 16] = b.try_into().ok()?;
                    Some(Term::Exact(tix::trace_term(&id)))
                })
                .collect();
            Some(Req { any: any?, key: None })
        }
        Column::Attr(k) => field_terms(k, vals),
        Column::Named(k) if !source_keys.contains(k.as_str()) => field_terms(k, vals),
        _ => None,
    }
}

fn field_terms(k: &str, vals: &[Value]) -> Option<Req> {
    let any: Option<Vec<Term>> = vals.iter().map(|v| tix::field_text(v).map(|t| Term::Exact(tix::field_term(k, &t)))).collect();
    Some(Req { any: any?, key: Some(k.to_string()) })
}

impl Plan {
    fn block_ok(&self, b: &BlockMeta) -> bool {
        if let Some(a) = self.time.0 {
            if b.max_time < a {
                return false;
            }
        }
        if let Some(z) = self.time.1 {
            if b.min_time > z {
                return false;
            }
        }
        if b.severities & self.sev_mask == 0 {
            return false;
        }
        match self.table {
            Table::Spans if b.span_count == 0 => return false,
            Table::Logs if !b.has_logs() => return false,
            _ => {}
        }
        if let Some(allowed) = &self.sources {
            if !b.sources.is_empty() && !b.sources.iter().any(|s| allowed.contains(s)) {
                return false;
            }
        }
        true
    }

    /// Candidate block indexes of one segment, using the term index when it can serve a requirement.
    fn candidates(&self, seg: &wartlog_store::SegmentHandle, used_index: &mut bool) -> Vec<usize> {
        let mut cand: Option<Vec<u32>> = None;
        if let Some(t) = &seg.tix {
            for r in &self.reqs {
                if let Some(k) = &r.key {
                    if !t.has_key(k) {
                        continue;
                    }
                }
                let mut list: Vec<u32> = Vec::new();
                let mut usable = true;
                for term in &r.any {
                    match term {
                        Term::Exact(b) => list.extend(t.postings(b)),
                        Term::Prefix(b) => match t.postings_prefix(b, 10_000) {
                            Some(v) => list.extend(v),
                            None => usable = false,
                        },
                    }
                }
                if !usable {
                    continue;
                }
                list.sort_unstable();
                list.dedup();
                *used_index = true;
                cand = Some(match cand.take() {
                    None => list,
                    Some(c) => {
                        let set: HashSet<u32> = list.into_iter().collect();
                        c.into_iter().filter(|b| set.contains(b)).collect()
                    }
                });
            }
        }
        let ids: Vec<usize> = match cand {
            Some(c) => c.into_iter().map(|x| x as usize).filter(|&i| i < seg.blocks.len()).collect(),
            None => (0..seg.blocks.len()).collect(),
        };
        ids.into_iter().filter(|&i| self.block_ok(&seg.blocks[i])).collect()
    }

    fn record_ok(&self, r: &dyn RecordLike, fields: &dyn Fields, body: &dyn Fn() -> String) -> bool {
        self.record_ok_with(r, fields, body, true)
    }

    fn record_ok_with(&self, r: &dyn RecordLike, fields: &dyn Fields, body: &dyn Fn() -> String, check_sources: bool) -> bool {
        match self.table {
            Table::Logs if r.is_span() => return false,
            Table::Spans if !r.is_span() => return false,
            _ => {}
        }
        let t = r.time();
        if self.time.0.is_some_and(|a| t < a) || self.time.1.is_some_and(|z| t > z) {
            return false;
        }
        if self.sev_mask != u32::MAX && (self.sev_mask >> r.severity().min(31)) & 1 == 0 {
            return false;
        }
        if let Some(allowed) = self.sources.as_ref().filter(|_| check_sources) {
            if !allowed.contains(&r.source().fingerprint()) {
                return false;
            }
        }
        let cx = Ctx { now: self.now };
        for f in &self.filters {
            let ok = match f {
                Filter::Expr(e) => truthy(&eval(e, fields, &cx)),
                Filter::Search(s) => search_match(r, &body(), s),
            };
            if !ok {
                return false;
            }
        }
        true
    }

    /// True when the whole query can run on the live record stream.
    pub fn is_streaming(&self) -> bool {
        self.rest.iter().all(|o| o.is_streaming())
    }
}

// ---------- thread pools ----------

fn pool(threads: Option<usize>) -> Arc<rayon::ThreadPool> {
    static POOLS: OnceLock<Mutex<HashMap<usize, Arc<rayon::ThreadPool>>>> = OnceLock::new();
    let n = threads.unwrap_or(0).min(256);
    let mut m = POOLS.get_or_init(Default::default).lock().unwrap();
    m.entry(n)
        .or_insert_with(|| {
            let mut b = rayon::ThreadPoolBuilder::new().thread_name(|i| format!("wl-query-{i}"));
            if n > 0 {
                b = b.num_threads(n);
            }
            Arc::new(b.build().expect("thread pool"))
        })
        .clone()
}

// ---------- execution ----------

pub fn run(src: &str, snap: &Snapshot, opts: &QueryOptions) -> Result<QueryResult, QueryError> {
    let q = crate::parser::parse(src)?;
    run_query(&q, snap, opts)
}

pub fn run_query(q: &Query, snap: &Snapshot, opts: &QueryOptions) -> Result<QueryResult, QueryError> {
    let start = Instant::now();
    let p = plan(q, snap, opts)?;
    let mut stats = QueryStats { segments: snap.segments.len(), ..Default::default() };
    let cancel = opts.cancel.clone().unwrap_or_default();

    // candidate blocks, newest first
    let mut cands: Vec<(usize, usize, i64)> = Vec::new();
    for (si, seg) in snap.segments.iter().enumerate() {
        let mut used = false;
        let c = p.candidates(seg, &mut used);
        stats.used_term_index += used as usize;
        stats.blocks += seg.blocks.len() as u64;
        cands.extend(c.into_iter().map(|bi| (si, bi, seg.blocks[bi].max_time)));
    }
    cands.sort_by(|a, b| b.2.cmp(&a.2));
    stats.blocks_scanned = cands.len() as u64;
    stats.blocks_pruned = stats.blocks - stats.blocks_scanned;

    // WAL records
    let mut rows: Vec<Row> = Vec::new();
    for r in &snap.wal {
        let body = || r.rendered_body();
        let row = Row { rec: Some(Arc::new(r.clone())), cols: vec![], projected: false };
        if p.record_ok(r, &row, &body) {
            rows.push(row);
        }
    }
    stats.wal_records = snap.wal.len() as u64;

    let scanned = AtomicU64::new(0);
    let damaged = AtomicU64::new(0);
    let pool = pool(opts.threads);
    let scan_block = |&(si, bi, _): &(usize, usize, i64)| -> Vec<Row> {
        if cancel.load(AtomicOrdering::Relaxed) {
            return vec![];
        }
        let seg = &snap.segments[si];
        let v = match seg.read_block(bi) {
            Ok(v) => v,
            Err(_) => {
                damaged.fetch_add(1, AtomicOrdering::Relaxed);
                return vec![];
            }
        };
        scanned.fetch_add(v.len() as u64, AtomicOrdering::Relaxed);
        let mut out = Vec::new();
        for r in v.iter() {
            let f = RecFields { r, body: Default::default() };
            let body = || f.body.get_or_init(|| f.r.body()).clone();
            if p.record_ok(&r, &f, &body) {
                out.push(Row { rec: Some(Arc::new(r.to_record())), cols: vec![], projected: false });
            }
        }
        out
    };

    match p.limit {
        Some(limit) if p.default_order => {
            // newest-first early stop: scan chunks until `limit` rows are found and no remaining
            // block can hold anything newer than the limit-th row
            let chunk = pool.current_num_threads().max(1) * 2;
            let mut i = 0;
            while i < cands.len() {
                if rows.len() as u64 >= limit {
                    rows.sort_by(|a, b| newest_first(a, b));
                    rows.truncate(limit as usize);
                    let cutoff = rows.last().and_then(|r| r.rec.as_ref()).map(|r| r.time_ns).unwrap_or(i64::MIN);
                    if cands[i].2 < cutoff {
                        stats.truncated = true;
                        break;
                    }
                }
                let end = (i + chunk).min(cands.len());
                let part: Vec<Row> = pool.install(|| cands[i..end].par_iter().flat_map_iter(scan_block).collect());
                rows.extend(part);
                i = end;
            }
            stats.blocks_scanned = stats.blocks_scanned.min(i as u64);
        }
        _ => {
            let part: Vec<Row> = pool.install(|| cands.par_iter().flat_map_iter(scan_block).collect());
            rows.extend(part);
        }
    }
    if cancel.load(AtomicOrdering::Relaxed) {
        return Err(QueryError::new("query cancelled"));
    }
    stats.records_scanned = scanned.into_inner() + snap.wal.len() as u64;
    stats.blocks_damaged = damaged.into_inner();

    if p.default_order {
        rows.par_sort_by(newest_first);
    }
    let cx = Ctx { now: p.now };
    for op in &p.rest {
        rows = apply(op, rows, &cx, p.table)?;
    }
    if p.default_order {
        if let Some(l) = p.limit {
            if rows.len() as u64 > l {
                rows.truncate(l as usize);
                stats.truncated = true;
            }
        }
    }
    let mut res = finish(rows, p.table);
    stats.rows = res.rows.len() as u64;
    stats.elapsed_ms = start.elapsed().as_secs_f64() * 1000.0;
    res.stats = stats;
    Ok(res)
}

fn newest_first(a: &Row, b: &Row) -> std::cmp::Ordering {
    let k = |r: &Row| r.rec.as_ref().map(|x| (x.time_ns, x.seq)).unwrap_or((i64::MIN, 0));
    k(b).cmp(&k(a))
}

fn default_columns(t: Table) -> &'static [&'static str] {
    match t {
        Table::Logs => LOG_COLUMNS,
        Table::Spans => SPAN_COLUMNS,
    }
}

/// Materializes a record row into explicit columns.
fn materialize(r: &Row, t: Table) -> Vec<(String, Value)> {
    let mut cols: Vec<(String, Value)> = match (&r.rec, r.projected) {
        (Some(rec), false) => default_columns(t).iter().map(|c| (c.to_string(), builtin(&Column::from_name(c), rec.as_ref()))).collect(),
        _ => vec![],
    };
    for (k, v) in &r.cols {
        match cols.iter_mut().find(|(n, _)| n == k) {
            Some(slot) => slot.1 = v.clone(),
            None => cols.push((k.clone(), v.clone())),
        }
    }
    cols
}

fn apply(op: &Op, rows: Vec<Row>, cx: &Ctx, t: Table) -> Result<Vec<Row>, QueryError> {
    Ok(match op {
        Op::Where(e) => rows.into_iter().filter(|r| truthy(&eval(e, r, cx))).collect(),
        Op::Search(s) => rows
            .into_iter()
            .filter(|r| match (&r.rec, r.projected) {
                (Some(rec), false) => search_match(rec.as_ref(), &rec.rendered_body(), s),
                _ => r.cols.iter().any(|(_, v)| eval::has(&v.to_text(), s, false)),
            })
            .collect(),
        Op::Project(items) => rows
            .into_iter()
            .map(|r| {
                let cols = items.iter().map(|(n, e)| (n.clone(), eval(e, &r, cx))).collect();
                Row { rec: r.rec, cols, projected: true }
            })
            .collect(),
        Op::ProjectAway(names) => rows
            .into_iter()
            .map(|r| {
                let cols = materialize(&r, t).into_iter().filter(|(n, _)| !names.contains(n)).collect();
                Row { rec: r.rec, cols, projected: true }
            })
            .collect(),
        Op::Extend(items) => rows
            .into_iter()
            .map(|mut r| {
                for (n, e) in items {
                    let v = eval(e, &r, cx);
                    r.cols.retain(|(k, _)| k != n);
                    r.cols.push((n.clone(), v));
                }
                r
            })
            .collect(),
        Op::Take(n) => rows.into_iter().take(*n as usize).collect(),
        Op::Sort(keys) => sort_rows(rows, keys, cx),
        Op::Top(n, k) => {
            let mut r = sort_rows(rows, std::slice::from_ref(k), cx);
            r.truncate(*n as usize);
            r
        }
        Op::Count => vec![Row { rec: None, cols: vec![("Count".into(), Value::I64(rows.len() as i64))], projected: true }],
        Op::Distinct(items) => {
            let mut seen = HashSet::new();
            let mut out = Vec::new();
            for r in rows {
                let cols: Vec<(String, Value)> = items.iter().map(|(n, e)| (n.clone(), eval(e, &r, cx))).collect();
                let key: Vec<String> = cols.iter().map(|(_, v)| group_key(v)).collect();
                if seen.insert(key) {
                    out.push(Row { rec: None, cols, projected: true });
                }
            }
            out
        }
        Op::Summarize { aggs, by } => summarize(rows, aggs, by, cx),
    })
}

fn sort_rows(rows: Vec<Row>, keys: &[SortKey], cx: &Ctx) -> Vec<Row> {
    let mut keyed: Vec<(Vec<Value>, Row)> = rows.into_iter().map(|r| (keys.iter().map(|k| eval(&k.expr, &r, cx)).collect(), r)).collect();
    keyed.sort_by(|a, b| {
        for (i, k) in keys.iter().enumerate() {
            let (x, y) = (&a.0[i], &b.0[i]);
            let o = match (x.is_null(), y.is_null()) {
                (true, true) => std::cmp::Ordering::Equal,
                (true, false) => {
                    if k.nulls_first {
                        std::cmp::Ordering::Less
                    } else {
                        std::cmp::Ordering::Greater
                    }
                }
                (false, true) => {
                    if k.nulls_first {
                        std::cmp::Ordering::Greater
                    } else {
                        std::cmp::Ordering::Less
                    }
                }
                _ => {
                    let o = compare(x, y).unwrap_or(std::cmp::Ordering::Equal);
                    if k.desc {
                        o.reverse()
                    } else {
                        o
                    }
                }
            };
            if o != std::cmp::Ordering::Equal {
                return o;
            }
        }
        std::cmp::Ordering::Equal
    });
    keyed.into_iter().map(|(_, r)| r).collect()
}

fn group_key(v: &Value) -> String {
    format!("{}\u{0}{}", v.type_name(), v.to_text())
}

enum AggState {
    Count(i64),
    DCount(HashSet<String>),
    Min(Option<Value>),
    Max(Option<Value>),
    Avg(f64, i64),
    Sum(Option<Value>),
}

fn summarize(rows: Vec<Row>, aggs: &[(String, Agg)], by: &[(String, Expr)], cx: &Ctx) -> Vec<Row> {
    let new_states = || -> Vec<AggState> {
        aggs.iter()
            .map(|(_, a)| match a.func {
                AggFunc::Count | AggFunc::CountIf => AggState::Count(0),
                AggFunc::DCount => AggState::DCount(HashSet::new()),
                AggFunc::Min => AggState::Min(None),
                AggFunc::Max => AggState::Max(None),
                AggFunc::Avg => AggState::Avg(0.0, 0),
                AggFunc::Sum => AggState::Sum(None),
            })
            .collect()
    };
    let mut groups: BTreeMap<Vec<String>, (Vec<Value>, Vec<AggState>)> = BTreeMap::new();
    let mut order: Vec<Vec<String>> = Vec::new();
    for r in &rows {
        let kv: Vec<Value> = by.iter().map(|(_, e)| eval(e, r, cx)).collect();
        let key: Vec<String> = kv.iter().map(group_key).collect();
        let entry = groups.entry(key.clone()).or_insert_with(|| {
            order.push(key);
            (kv, new_states())
        });
        for (i, (_, a)) in aggs.iter().enumerate() {
            let v = a.arg.as_ref().map(|e| eval(e, r, cx));
            match (&mut entry.1[i], a.func) {
                (AggState::Count(n), AggFunc::Count) => *n += 1,
                (AggState::Count(n), AggFunc::CountIf) => {
                    if v.as_ref().is_some_and(truthy) {
                        *n += 1
                    }
                }
                (AggState::DCount(s), _) => {
                    if let Some(v) = v.filter(|v| !v.is_null()) {
                        s.insert(group_key(&v));
                    }
                }
                (AggState::Min(m), _) => {
                    if let Some(v) = v.filter(|v| !v.is_null()) {
                        if m.as_ref().is_none_or(|cur| compare(&v, cur) == Some(std::cmp::Ordering::Less)) {
                            *m = Some(v);
                        }
                    }
                }
                (AggState::Max(m), _) => {
                    if let Some(v) = v.filter(|v| !v.is_null()) {
                        if m.as_ref().is_none_or(|cur| compare(&v, cur) == Some(std::cmp::Ordering::Greater)) {
                            *m = Some(v);
                        }
                    }
                }
                (AggState::Avg(s, n), _) => {
                    if let Some(x) = v.as_ref().and_then(|v| match v {
                        Value::Duration(d) => Some(*d as f64),
                        other => other.as_f64(),
                    }) {
                        *s += x;
                        *n += 1;
                    }
                }
                (AggState::Sum(acc), _) => {
                    if let Some(v) = v.filter(|v| !v.is_null()) {
                        *acc = Some(match acc.take() {
                            None => v,
                            Some(cur) => eval::binop(BinOp::Add, &cur, &v),
                        });
                    }
                }
                _ => {}
            }
        }
    }
    // KQL summarize with no rows and no `by` still returns one row
    if rows.is_empty() && by.is_empty() {
        let key = vec![];
        order.push(key.clone());
        groups.insert(key, (vec![], new_states()));
    }
    let is_duration = |i: usize| aggs[i].1.arg.as_ref().is_some_and(|e| matches!(e, Expr::Col(Column::Duration)));
    let mut out: Vec<Row> = Vec::with_capacity(groups.len());
    for key in order {
        let (kv, states) = groups.remove(&key).unwrap();
        let mut cols: Vec<(String, Value)> = by.iter().map(|(n, _)| n.clone()).zip(kv).collect();
        for (i, st) in states.into_iter().enumerate() {
            let v = match st {
                AggState::Count(n) => Value::I64(n),
                AggState::DCount(s) => Value::I64(s.len() as i64),
                AggState::Min(m) | AggState::Max(m) | AggState::Sum(m) => m.unwrap_or(Value::Null),
                AggState::Avg(s, n) if n > 0 => {
                    if is_duration(i) {
                        Value::Duration((s / n as f64) as i64)
                    } else {
                        Value::F64(s / n as f64)
                    }
                }
                AggState::Avg(..) => Value::Null,
            };
            cols.push((aggs[i].0.clone(), v));
        }
        out.push(Row { rec: None, cols, projected: true });
    }
    // deterministic: sort groups by their keys
    if !by.is_empty() {
        let nk = by.len();
        out.sort_by(|a, b| {
            for i in 0..nk {
                let o = compare(&a.cols[i].1, &b.cols[i].1).unwrap_or(std::cmp::Ordering::Equal);
                if o != std::cmp::Ordering::Equal {
                    return o;
                }
            }
            std::cmp::Ordering::Equal
        });
    }
    out
}

fn finish(rows: Vec<Row>, t: Table) -> QueryResult {
    let mut columns: Vec<String> = Vec::new();
    let mut mats: Vec<(Vec<(String, Value)>, Option<u64>)> = Vec::with_capacity(rows.len());
    for r in &rows {
        let cols = materialize(r, t);
        for (k, _) in &cols {
            if !columns.contains(k) {
                columns.push(k.clone());
            }
        }
        let id = if r.projected { None } else { r.rec.as_ref().map(|x| x.seq) };
        mats.push((cols, id));
    }
    if rows.is_empty() {
        columns = default_columns(t).iter().map(|s| s.to_string()).collect();
    }
    let mut res = QueryResult { columns: columns.clone(), ..Default::default() };
    for (cols, id) in mats {
        let mut m: HashMap<String, Value> = cols.into_iter().collect();
        res.rows.push(columns.iter().map(|c| m.remove(c).unwrap_or(Value::Null)).collect());
        res.ids.push(id);
    }
    res
}

/// A compiled filter for live tail: streaming operators only.
pub struct TailFilter {
    plan: Plan,
}

impl TailFilter {
    pub fn compile(src: &str, snap: &Snapshot) -> Result<TailFilter, QueryError> {
        let q = crate::parser::parse(src)?;
        let plan = plan(&q, snap, &QueryOptions::default())?;
        if !plan.is_streaming() {
            return Err(QueryError::new("live tail supports only where, search, project, project-away and extend"));
        }
        Ok(TailFilter { plan })
    }

    /// Row for a new record, or None when it does not match.
    pub fn apply(&self, r: &Arc<Record>) -> Option<(Vec<String>, Vec<Value>)> {
        let row = Row { rec: Some(r.clone()), cols: vec![], projected: false };
        let body = || r.rendered_body();
        let p = &self.plan;
        // the source set is only a shortcut (the filters are always evaluated), and a source seen for
        // the first time after compiling is not in it, so tail never prunes by source
        if !p.record_ok_with(r.as_ref(), &row, &body, false) {
            return None;
        }
        let cx = Ctx { now: now_ns() };
        let mut rows = vec![row];
        for op in &p.rest {
            rows = apply(op, rows, &cx, p.table).ok()?;
        }
        let r = rows.pop()?;
        let cols = materialize(&r, p.table);
        Some(cols.into_iter().unzip())
    }
}
