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
    /// blocks a count answered from block metadata without reading them
    #[serde(default)]
    pub blocks_from_metadata: u64,
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
    let mut p = Plan { table: q.table, filters: Vec::new(), rest, time: (None, None), sev_mask: u32::MAX, sources: None, reqs: Vec::new(), now };

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
//
// The operators after the leading filters split into three parts:
//   stream: leading where/search/project/project-away/extend, applied to each record during the scan
//   sink:   the first operator that needs more than one row (count, summarize, top, sort | take,
//           distinct, take), folded per scan thread and merged, so memory follows the size of the
//           result and not the number of records scanned
//   tail:   whatever follows, applied to the sink's (small) output
// Only a sort without a row cap still has to hold every matching row.

pub fn run(src: &str, snap: &Snapshot, opts: &QueryOptions) -> Result<QueryResult, QueryError> {
    let q = crate::parser::parse(src)?;
    run_query(&q, snap, opts)
}

enum SinkSpec {
    /// keep every row; when `newest` is set, newest first and capped at `limit`
    Collect { limit: Option<u64>, newest: bool },
    Count,
    Summarize { aggs: Vec<(String, Agg)>, by: Vec<(String, Expr)> },
    /// the first `n` rows by `keys`; `cap` marks a sort that is only bounded by the row cap
    Top { keys: Vec<SortKey>, n: u64, cap: bool },
    Distinct(Vec<(String, Expr)>),
}

struct Pipeline {
    stream: Vec<Op>,
    sink: SinkSpec,
    tail: Vec<Op>,
}

impl Pipeline {
    fn new(rest: &[Op], cap: Option<u64>) -> Pipeline {
        let k = rest.iter().take_while(|o| o.is_streaming()).count();
        let after = &rest[k..];
        let (sink, used) = match after {
            [] => (SinkSpec::Collect { limit: cap, newest: true }, 0),
            [Op::Take(n), more @ ..] => {
                let cap = if more.is_empty() { cap } else { None };
                (SinkSpec::Collect { limit: Some(cap.map_or(*n, |l| l.min(*n))), newest: true }, 1)
            }
            [Op::Count, ..] => (SinkSpec::Count, 1),
            [Op::Summarize { aggs, by }, ..] => (SinkSpec::Summarize { aggs: aggs.clone(), by: by.clone() }, 1),
            [Op::Top(n, key), ..] => (SinkSpec::Top { keys: vec![key.clone()], n: *n, cap: false }, 1),
            [Op::Sort(keys), Op::Take(n), ..] => (SinkSpec::Top { keys: keys.clone(), n: *n, cap: false }, 2),
            [Op::Sort(keys)] if cap.is_some() => (SinkSpec::Top { keys: keys.clone(), n: cap.unwrap_or(0), cap: true }, 1),
            [Op::Distinct(items), ..] => (SinkSpec::Distinct(items.clone()), 1),
            _ => (SinkSpec::Collect { limit: None, newest: false }, 0),
        };
        Pipeline { stream: rest[..k].to_vec(), sink, tail: after[used..].to_vec() }
    }

    /// True when the stream cannot drop rows, so a count can come from block metadata.
    fn stream_keeps_all(&self) -> bool {
        self.stream.iter().all(|o| !matches!(o, Op::Where(_) | Op::Search(_)))
    }
}

/// (time, seq) of a row's record: newest first is descending order of this.
fn age_key(r: &Row) -> (i64, u64) {
    r.rec.as_ref().map(|x| (x.time_ns, x.seq)).unwrap_or((i64::MIN, 0))
}

/// A scan thread's partial result.
enum Acc<'p> {
    Rows(Vec<Row>),
    Count(u64),
    Groups(Groups<'p>),
    Top(TopN<'p>),
    Distinct(DistinctAcc<'p>),
}

impl<'p> Acc<'p> {
    fn new(s: &'p SinkSpec) -> Acc<'p> {
        match s {
            SinkSpec::Collect { .. } => Acc::Rows(Vec::new()),
            SinkSpec::Count => Acc::Count(0),
            SinkSpec::Summarize { aggs, by } => Acc::Groups(Groups::new(aggs, by)),
            SinkSpec::Top { keys, n, .. } => Acc::Top(TopN::new(keys, *n as usize)),
            SinkSpec::Distinct(items) => Acc::Distinct(DistinctAcc { items, seen: HashSet::new(), out: Vec::new() }),
        }
    }

    /// Adds one matching row. `make` builds the row only for sinks that keep it.
    fn push(&mut self, f: &dyn Fields, age: (i64, u64), make: impl FnOnce() -> Row, cx: &Ctx) {
        match self {
            Acc::Rows(v) => v.push(make()),
            Acc::Count(n) => *n += 1,
            Acc::Groups(g) => g.add(f, cx),
            Acc::Top(t) => t.push(f, age, make, cx),
            Acc::Distinct(d) => d.push(f, cx),
        }
    }

    fn push_row(&mut self, r: Row, cx: &Ctx) {
        let age = age_key(&r);
        self.push(&r, age, || r.clone(), cx);
    }

    /// Combines two partial results; `self` comes first in scan order.
    fn merge(self, other: Acc<'p>) -> Acc<'p> {
        match (self, other) {
            (Acc::Rows(mut a), Acc::Rows(b)) => {
                a.extend(b);
                Acc::Rows(a)
            }
            (Acc::Count(a), Acc::Count(b)) => Acc::Count(a + b),
            (Acc::Groups(mut a), Acc::Groups(b)) => {
                a.merge(b);
                Acc::Groups(a)
            }
            (Acc::Top(mut a), Acc::Top(b)) => {
                a.merge(b);
                Acc::Top(a)
            }
            (Acc::Distinct(mut a), Acc::Distinct(b)) => {
                a.merge(b);
                Acc::Distinct(a)
            }
            (a, _) => a,
        }
    }
}

/// Applies streaming operators to one row; None when a filter drops it.
fn stream_row(ops: &[Op], mut r: Row, cx: &Ctx, t: Table) -> Option<Row> {
    for op in ops {
        r = stream_op(op, r, cx, t)?;
    }
    Some(r)
}

fn stream_op(op: &Op, mut r: Row, cx: &Ctx, t: Table) -> Option<Row> {
    match op {
        Op::Where(e) => truthy(&eval(e, &r, cx)).then_some(r),
        Op::Search(s) => {
            let ok = match (&r.rec, r.projected) {
                (Some(rec), false) => search_match(rec.as_ref(), &rec.rendered_body(), s),
                _ => r.cols.iter().any(|(_, v)| eval::has(&v.to_text(), s, false)),
            };
            ok.then_some(r)
        }
        Op::Project(items) => {
            let cols = items.iter().map(|(n, e)| (n.clone(), eval(e, &r, cx))).collect();
            Some(Row { rec: r.rec, cols, projected: true })
        }
        Op::ProjectAway(names) => {
            let cols = materialize(&r, t).into_iter().filter(|(n, _)| !names.contains(n)).collect();
            Some(Row { rec: r.rec, cols, projected: true })
        }
        Op::Extend(items) => {
            for (n, e) in items {
                let v = eval(e, &r, cx);
                r.cols.retain(|(k, _)| k != n);
                r.cols.push((n.clone(), v));
            }
            Some(r)
        }
        _ => Some(r),
    }
}

impl Plan {
    /// How many of this block's records the query keeps, when the block metadata alone answers
    /// it: no record-level filter and the block wholly inside the time, severity and source limits.
    fn count_from_meta(&self, b: &BlockMeta) -> Option<u64> {
        if !self.filters.is_empty() {
            return None;
        }
        if self.time.0.is_some_and(|a| b.min_time < a) || self.time.1.is_some_and(|z| b.max_time > z) {
            return None;
        }
        if b.severities & !self.sev_mask != 0 {
            return None;
        }
        if let Some(allowed) = &self.sources {
            if b.sources.is_empty() || !b.sources.iter().all(|s| allowed.contains(s)) {
                return None;
            }
        }
        Some(match self.table {
            Table::Logs => b.record_count.saturating_sub(b.span_count) as u64,
            Table::Spans => b.span_count as u64,
        })
    }
}

pub fn run_query(q: &Query, snap: &Snapshot, opts: &QueryOptions) -> Result<QueryResult, QueryError> {
    let start = Instant::now();
    let p = plan(q, snap, opts)?;
    let pipe = Pipeline::new(&p.rest, opts.limit);
    let mut stats = QueryStats { segments: snap.segments.len(), ..Default::default() };
    let cancel = opts.cancel.clone().unwrap_or_default();
    let cx = Ctx { now: p.now };

    // candidate blocks, newest first
    let mut cands: Vec<(usize, usize, i64)> = Vec::new();
    for (si, seg) in snap.segments.iter().enumerate() {
        let mut used = false;
        let c = p.candidates(seg, &mut used);
        stats.used_term_index += used as usize;
        stats.blocks += seg.blocks.len() as u64;
        cands.extend(c.into_iter().map(|bi| (si, bi, seg.blocks[bi].max_time)));
    }
    cands.sort_by_key(|c| std::cmp::Reverse(c.2));
    stats.blocks_scanned = cands.len() as u64;
    stats.blocks_pruned = stats.blocks - stats.blocks_scanned;

    // WAL records
    let mut acc = Acc::new(&pipe.sink);
    for r in &snap.wal {
        let body = || r.rendered_body();
        let row = Row { rec: Some(Arc::new(r.clone())), cols: vec![], projected: false };
        if p.record_ok(r, &row, &body) {
            if let Some(row) = stream_row(&pipe.stream, row, &cx, p.table) {
                acc.push_row(row, &cx);
            }
        }
    }
    stats.wal_records = snap.wal.len() as u64;

    let scanned = AtomicU64::new(0);
    let damaged = AtomicU64::new(0);
    let from_meta = AtomicU64::new(0);
    let meta_count = matches!(pipe.sink, SinkSpec::Count) && pipe.stream_keeps_all();
    let pool = pool(opts.threads);
    let scan_block = scanner(|mut acc: Acc, &(si, bi, _): &(usize, usize, i64)| {
        if cancel.load(AtomicOrdering::Relaxed) {
            return acc;
        }
        let seg = &snap.segments[si];
        if meta_count {
            if let (Acc::Count(n), Some(c)) = (&mut acc, p.count_from_meta(&seg.blocks[bi])) {
                *n += c;
                from_meta.fetch_add(1, AtomicOrdering::Relaxed);
                return acc;
            }
        }
        let v = match seg.read_block(bi) {
            Ok(v) => v,
            Err(_) => {
                damaged.fetch_add(1, AtomicOrdering::Relaxed);
                return acc;
            }
        };
        scanned.fetch_add(v.len() as u64, AtomicOrdering::Relaxed);
        for r in v.iter() {
            let f = RecFields { r, body: Default::default() };
            let body = || f.body.get_or_init(|| f.r.body()).clone();
            if !p.record_ok(&r, &f, &body) {
                continue;
            }
            let make = || Row { rec: Some(Arc::new(r.to_record())), cols: vec![], projected: false };
            if pipe.stream.is_empty() {
                acc.push(&f, (r.time(), r.seq()), make, &cx);
            } else if let Some(row) = stream_row(&pipe.stream, make(), &cx, p.table) {
                acc.push_row(row, &cx);
            }
        }
        acc
    });
    let fold = |part: &[(usize, usize, i64)]| -> Acc<'_> { pool.install(|| part.par_iter().fold(|| Acc::new(&pipe.sink), scan_block).reduce(|| Acc::new(&pipe.sink), Acc::merge)) };

    match (&pipe.sink, acc) {
        (SinkSpec::Collect { limit: Some(limit), newest: true }, Acc::Rows(mut rows)) => {
            // newest-first early stop: scan chunks until `limit` rows are found and no remaining
            // block can hold anything newer than the limit-th row
            let limit = *limit;
            let chunk = pool.current_num_threads().max(1) * 2;
            let mut i = 0;
            while i < cands.len() {
                if rows.len() as u64 >= limit {
                    rows.sort_by(newest_first);
                    rows.truncate(limit as usize);
                    let cutoff = rows.last().and_then(|r| r.rec.as_ref()).map(|r| r.time_ns).unwrap_or(i64::MIN);
                    if cands[i].2 < cutoff {
                        stats.truncated = true;
                        break;
                    }
                }
                let end = (i + chunk).min(cands.len());
                if let Acc::Rows(part) = fold(&cands[i..end]) {
                    rows.extend(part);
                }
                i = end;
            }
            stats.blocks_scanned = stats.blocks_scanned.min(i as u64);
            acc = Acc::Rows(rows);
        }
        (_, a) => acc = a.merge(fold(&cands)),
    }
    if cancel.load(AtomicOrdering::Relaxed) {
        return Err(QueryError::new("query cancelled"));
    }
    stats.records_scanned = scanned.into_inner() + snap.wal.len() as u64;
    stats.blocks_damaged = damaged.into_inner();
    stats.blocks_from_metadata = from_meta.into_inner();

    let mut rows = match (acc, &pipe.sink) {
        (Acc::Rows(mut rows), SinkSpec::Collect { limit, newest: true }) => {
            rows.par_sort_by(newest_first);
            if let Some(l) = *limit {
                if rows.len() as u64 > l {
                    rows.truncate(l as usize);
                    stats.truncated = true;
                }
            }
            rows
        }
        (Acc::Rows(rows), _) => rows,
        (Acc::Count(n), _) => vec![count_row(n)],
        (Acc::Groups(g), _) => g.finish(),
        (Acc::Top(t), SinkSpec::Top { cap, .. }) => {
            let (rows, more) = t.finish();
            stats.truncated |= *cap && more;
            rows
        }
        (Acc::Top(t), _) => t.finish().0,
        (Acc::Distinct(d), _) => d.finish(),
    };
    for op in &pipe.tail {
        rows = apply(op, rows, &cx, p.table)?;
    }
    let mut res = finish(rows, p.table);
    stats.rows = res.rows.len() as u64;
    stats.elapsed_ms = start.elapsed().as_secs_f64() * 1000.0;
    res.stats = stats;
    Ok(res)
}

/// Ties the closure's argument and result to one lifetime.
fn scanner<'p, F: Fn(Acc<'p>, &(usize, usize, i64)) -> Acc<'p>>(f: F) -> F {
    f
}

fn count_row(n: u64) -> Row {
    Row { rec: None, cols: vec![("Count".into(), Value::I64(n as i64))], projected: true }
}

fn newest_first(a: &Row, b: &Row) -> std::cmp::Ordering {
    age_key(b).cmp(&age_key(a))
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
        Op::Where(_) | Op::Search(_) | Op::Project(_) | Op::ProjectAway(_) | Op::Extend(_) => rows.into_iter().filter_map(|r| stream_op(op, r, cx, t)).collect(),
        Op::Take(n) => rows.into_iter().take(*n as usize).collect(),
        Op::Sort(keys) => sort_rows(rows, keys, cx),
        Op::Top(n, k) => {
            let mut r = sort_rows(rows, std::slice::from_ref(k), cx);
            r.truncate(*n as usize);
            r
        }
        Op::Count => vec![count_row(rows.len() as u64)],
        Op::Distinct(items) => {
            let mut d = DistinctAcc { items, seen: HashSet::new(), out: Vec::new() };
            for r in &rows {
                d.push(r, cx);
            }
            d.finish()
        }
        Op::Summarize { aggs, by } => {
            let mut g = Groups::new(aggs, by);
            for r in &rows {
                g.add(r, cx);
            }
            g.finish()
        }
    })
}

/// Order of two rows by sort keys (nulls last unless `nulls first`).
fn cmp_keys(keys: &[SortKey], a: &[Value], b: &[Value]) -> std::cmp::Ordering {
    use std::cmp::Ordering::*;
    for (i, k) in keys.iter().enumerate() {
        let (x, y) = (&a[i], &b[i]);
        let o = match (x.is_null(), y.is_null()) {
            (true, true) => Equal,
            (true, false) => {
                if k.nulls_first {
                    Less
                } else {
                    Greater
                }
            }
            (false, true) => {
                if k.nulls_first {
                    Greater
                } else {
                    Less
                }
            }
            _ => {
                let o = compare(x, y).unwrap_or(Equal);
                if k.desc {
                    o.reverse()
                } else {
                    o
                }
            }
        };
        if o != Equal {
            return o;
        }
    }
    Equal
}

/// Sorts by the keys; ties go newest record first, so results do not depend on scan order.
fn sort_rows(rows: Vec<Row>, keys: &[SortKey], cx: &Ctx) -> Vec<Row> {
    let mut keyed: Vec<(Vec<Value>, Row)> = rows.into_iter().map(|r| (keys.iter().map(|k| eval(&k.expr, &r, cx)).collect(), r)).collect();
    keyed.sort_by(|a, b| cmp_keys(keys, &a.0, &b.0).then_with(|| newest_first(&a.1, &b.1)));
    keyed.into_iter().map(|(_, r)| r).collect()
}

/// The first `n` rows by sort keys, holding at most about 2n rows at a time.
struct TopN<'p> {
    keys: &'p [SortKey],
    n: usize,
    buf: Vec<(Vec<Value>, (i64, u64), Row)>,
    /// the n-th row's keys once `buf` has been cut to n: anything not ahead of it is skipped
    cutoff: Option<(Vec<Value>, (i64, u64))>,
    seen: u64,
}

impl<'p> TopN<'p> {
    fn new(keys: &'p [SortKey], n: usize) -> TopN<'p> {
        TopN { keys, n, buf: Vec::new(), cutoff: None, seen: 0 }
    }

    fn ahead(&self, k: &[Value], age: (i64, u64), than: &(Vec<Value>, (i64, u64))) -> bool {
        cmp_keys(self.keys, k, &than.0).then_with(|| than.1.cmp(&age)) == std::cmp::Ordering::Less
    }

    fn push(&mut self, f: &dyn Fields, age: (i64, u64), make: impl FnOnce() -> Row, cx: &Ctx) {
        self.seen += 1;
        if self.n == 0 {
            return;
        }
        let k: Vec<Value> = self.keys.iter().map(|s| eval(&s.expr, f, cx)).collect();
        if let Some(c) = &self.cutoff {
            if !self.ahead(&k, age, c) {
                return;
            }
        }
        self.buf.push((k, age, make()));
        if self.buf.len() >= self.n + self.n.max(1024) {
            self.compact();
        }
    }

    fn compact(&mut self) {
        let keys = self.keys;
        self.buf.sort_by(|a, b| cmp_keys(keys, &a.0, &b.0).then_with(|| b.1.cmp(&a.1)));
        if self.buf.len() >= self.n {
            self.buf.truncate(self.n);
            let last = &self.buf[self.n - 1];
            self.cutoff = Some((last.0.clone(), last.1));
        }
    }

    fn merge(&mut self, other: TopN<'p>) {
        self.seen += other.seen;
        self.buf.extend(other.buf);
        self.compact();
    }

    /// The rows, and whether more rows matched than were kept.
    fn finish(mut self) -> (Vec<Row>, bool) {
        self.compact();
        (self.buf.into_iter().map(|(_, _, r)| r).collect(), self.seen > self.n as u64)
    }
}

struct DistinctAcc<'p> {
    items: &'p [(String, Expr)],
    seen: HashSet<Vec<String>>,
    out: Vec<(Vec<String>, Row)>,
}

impl<'p> DistinctAcc<'p> {
    fn push(&mut self, f: &dyn Fields, cx: &Ctx) {
        let cols: Vec<(String, Value)> = self.items.iter().map(|(n, e)| (n.clone(), eval(e, f, cx))).collect();
        let key: Vec<String> = cols.iter().map(|(_, v)| group_key(v)).collect();
        if !self.seen.contains(&key) {
            self.seen.insert(key.clone());
            self.out.push((key, Row { rec: None, cols, projected: true }));
        }
    }

    fn merge(&mut self, other: DistinctAcc<'p>) {
        for (k, r) in other.out {
            if !self.seen.contains(&k) {
                self.seen.insert(k.clone());
                self.out.push((k, r));
            }
        }
    }

    fn finish(self) -> Vec<Row> {
        self.out.into_iter().map(|(_, r)| r).collect()
    }
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

impl AggState {
    fn new(a: &Agg) -> AggState {
        match a.func {
            AggFunc::Count | AggFunc::CountIf => AggState::Count(0),
            AggFunc::DCount => AggState::DCount(HashSet::new()),
            AggFunc::Min => AggState::Min(None),
            AggFunc::Max => AggState::Max(None),
            AggFunc::Avg => AggState::Avg(0.0, 0),
            AggFunc::Sum => AggState::Sum(None),
        }
    }

    fn add(&mut self, a: &Agg, f: &dyn Fields, cx: &Ctx) {
        let v = a.arg.as_ref().map(|e| eval(e, f, cx));
        match (self, a.func) {
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
            (AggState::Min(m), _) => keep_if(m, v, std::cmp::Ordering::Less),
            (AggState::Max(m), _) => keep_if(m, v, std::cmp::Ordering::Greater),
            (AggState::Avg(s, n), _) => {
                if let Some(x) = v.as_ref().and_then(|v| match v {
                    Value::Duration(d) => Some(*d as f64),
                    other => other.as_f64(),
                }) {
                    *s += x;
                    *n += 1;
                }
            }
            (AggState::Sum(acc), _) => add_sum(acc, v),
            _ => {}
        }
    }

    fn merge(&mut self, other: AggState) {
        match (self, other) {
            (AggState::Count(a), AggState::Count(b)) => *a += b,
            (AggState::DCount(a), AggState::DCount(b)) => a.extend(b),
            (AggState::Min(a), AggState::Min(b)) => keep_if(a, b, std::cmp::Ordering::Less),
            (AggState::Max(a), AggState::Max(b)) => keep_if(a, b, std::cmp::Ordering::Greater),
            (AggState::Avg(s, n), AggState::Avg(s2, n2)) => {
                *s += s2;
                *n += n2;
            }
            (AggState::Sum(a), AggState::Sum(b)) => add_sum(a, b),
            _ => {}
        }
    }
}

/// min/max: replaces `m` when `v` orders `want` against it.
fn keep_if(m: &mut Option<Value>, v: Option<Value>, want: std::cmp::Ordering) {
    if let Some(v) = v.filter(|v| !v.is_null()) {
        if m.as_ref().is_none_or(|cur| compare(&v, cur) == Some(want)) {
            *m = Some(v);
        }
    }
}

fn add_sum(acc: &mut Option<Value>, v: Option<Value>) {
    if let Some(v) = v.filter(|v| !v.is_null()) {
        *acc = Some(match acc.take() {
            None => v,
            Some(cur) => eval::binop(BinOp::Add, &cur, &v),
        });
    }
}

/// summarize state: one entry per group.
struct Groups<'p> {
    aggs: &'p [(String, Agg)],
    by: &'p [(String, Expr)],
    map: HashMap<Vec<String>, (Vec<Value>, Vec<AggState>)>,
}

impl<'p> Groups<'p> {
    fn new(aggs: &'p [(String, Agg)], by: &'p [(String, Expr)]) -> Groups<'p> {
        Groups { aggs, by, map: HashMap::new() }
    }

    fn add(&mut self, f: &dyn Fields, cx: &Ctx) {
        let kv: Vec<Value> = self.by.iter().map(|(_, e)| eval(e, f, cx)).collect();
        let key: Vec<String> = kv.iter().map(group_key).collect();
        let aggs = self.aggs;
        let entry = self.map.entry(key).or_insert_with(|| (kv, aggs.iter().map(|(_, a)| AggState::new(a)).collect()));
        for (st, (_, a)) in entry.1.iter_mut().zip(aggs) {
            st.add(a, f, cx);
        }
    }

    fn merge(&mut self, other: Groups<'p>) {
        for (k, (kv, states)) in other.map {
            match self.map.entry(k) {
                std::collections::hash_map::Entry::Occupied(mut e) => {
                    for (a, b) in e.get_mut().1.iter_mut().zip(states) {
                        a.merge(b);
                    }
                }
                std::collections::hash_map::Entry::Vacant(e) => {
                    e.insert((kv, states));
                }
            }
        }
    }

    fn finish(mut self) -> Vec<Row> {
        let aggs = self.aggs;
        // KQL summarize with no rows and no `by` still returns one row
        if self.map.is_empty() && self.by.is_empty() {
            self.map.insert(vec![], (vec![], aggs.iter().map(|(_, a)| AggState::new(a)).collect()));
        }
        let is_duration = |i: usize| aggs[i].1.arg.as_ref().is_some_and(|e| matches!(e, Expr::Col(Column::Duration)));
        // group keys first, so ties in the value sort below are deterministic
        let groups: BTreeMap<Vec<String>, (Vec<Value>, Vec<AggState>)> = self.map.into_iter().collect();
        let mut out: Vec<Row> = Vec::with_capacity(groups.len());
        for (_, (kv, states)) in groups {
            let mut cols: Vec<(String, Value)> = self.by.iter().map(|(n, _)| n.clone()).zip(kv).collect();
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
        if !self.by.is_empty() {
            let nk = self.by.len();
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
        let r = stream_row(&p.rest, row, &cx, p.table)?;
        let cols = materialize(&r, p.table);
        Some(cols.into_iter().unzip())
    }
}
