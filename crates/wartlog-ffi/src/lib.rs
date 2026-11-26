//! The WartLog C ABI (`libwartlog.so`, header `include/wartlog.h`).
//!
//! Rules (docs/superpowers/specs/2026-11-26-wartlog-ffi-design.md, section 5):
//! - every handle is opaque; records go in through a builder and rows come out through a cursor
//! - every function returns a status (0 ok, negative `WL_E_*`), detail in `wl_last_error()`;
//!   panics are caught at the boundary and become `WL_E_PANIC`
//! - strings are UTF-8; returned strings stay owned by the library and are valid until the
//!   owning handle is freed or the next call on it, unless the function says to free them
//! - a snapshot may be shared by threads for reading; a writer and a cursor belong to one thread
//!   at a time
//! - new versions only add functions and enum values (`wl_abi_version`)
//!
//! Every function below that takes a pointer checks it for NULL. Beyond that, the caller must
//! pass handles this library returned and has not freed, and buffers of the stated length.

#![allow(non_camel_case_types)]
#![allow(clippy::missing_safety_doc)]

use std::cell::RefCell;
use std::ffi::{c_char, c_int, CStr, CString};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use wl::{Error, ErrorKind, Record, Source, SourceKind, Value};

/// Bumped when functions or enum values are added (never when they would change).
pub const WL_ABI_VERSION: u32 = 1;

pub const WL_OK: c_int = 0;
/// a NULL pointer, bad UTF-8, a closed handle, or a value out of range
pub const WL_E_INVALID: c_int = -1;
/// another process holds the store lock
pub const WL_E_LOCKED: c_int = -2;
/// private mode on a store wartlogd owns
pub const WL_E_DAEMON_STORE: c_int = -3;
/// daemon-only operation on a store the library created
pub const WL_E_EXTERNAL_STORE: c_int = -4;
/// shared mode, but no running wartlogd serves the store
pub const WL_E_NO_DAEMON: c_int = -5;
/// no store at the path
pub const WL_E_NOT_FOUND: c_int = -6;
/// the store or a segment is newer than this library reads
pub const WL_E_NEWER_FORMAT: c_int = -7;
/// the KQL text is wrong; `wl_last_error_span` points into it
pub const WL_E_QUERY: c_int = -8;
/// a Rust panic (a bug in WartLog); the handle involved should be freed
pub const WL_E_PANIC: c_int = -9;
/// a column holds a different type (or null)
pub const WL_E_TYPE: c_int = -10;
/// I/O, damage, or the daemon refusing a request
pub const WL_E_OTHER: c_int = -11;

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum wl_writer_mode {
    /// through the running wartlogd
    WL_WRITER_SHARED = 1,
    /// a store the library owns (`<name>.wartlogx`), created if absent
    WL_WRITER_PRIVATE = 2,
    /// a daemon store with wartlogd stopped (takes the lock)
    WL_WRITER_EXCLUSIVE = 3,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum wl_value_type {
    WL_NULL = 0,
    WL_BOOL = 1,
    WL_I64 = 2,
    WL_F64 = 3,
    WL_STR = 4,
    WL_BYTES = 5,
    /// i64 ns since the epoch
    WL_TIMESTAMP = 6,
    /// i64 ns
    WL_DURATION = 7,
    /// JSON text
    WL_JSON = 8,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum wl_source_kind {
    WL_SOURCE_OTLP = 1,
    WL_SOURCE_JOURNALD = 2,
    WL_SOURCE_K8S = 3,
    WL_SOURCE_NDJSON = 4,
}

/// Query options. Zero-initialize, set `size = sizeof(wl_query_opts)`, then set what you need.
/// Passing NULL uses the defaults.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct wl_query_opts {
    /// sizeof(wl_query_opts), so later versions can add fields
    pub size: u32,
    /// query threads; 0 = default (2: the query runs in your process)
    pub threads: u32,
    /// row cap when the query has no take/top/aggregation; 0 = default (10000), UINT64_MAX = none
    pub limit: u64,
    /// inclusive time range in ns since the epoch, used when the query has none on Timestamp;
    /// INT64_MIN and INT64_MAX (or 0 and 0) mean open
    pub from_ns: i64,
    pub to_ns: i64,
}

/// Writer options. Zero-initialize and set `size = sizeof(wl_writer_opts)`; NULL uses defaults.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct wl_writer_opts {
    pub size: u32,
    /// private/exclusive: WAL fsync interval of the maintenance thread; 0 = 250 ms
    pub sync_every_ms: u32,
    /// shared: the daemon's API socket; NULL = $XDG_RUNTIME_DIR/wartlog/api.sock
    pub socket: *const c_char,
    /// recorded in the manifest of a store this writer creates; NULL = "wartlog-ffi <version>"
    pub creator: *const c_char,
    /// source name for `wl_writer_push_json` lines that do not name one; NULL = "app"
    pub default_source: *const c_char,
    /// private/exclusive retention: 0 = none
    pub retention_max_bytes: u64,
    pub retention_max_age_s: u64,
}

pub struct wl_snapshot {
    snap: wl::Snapshot,
}

pub struct wl_cursor {
    rows: wl::Rows,
    columns: Vec<String>,
    row: Option<wl_row>,
}

pub struct wl_row {
    row: wl::Row,
    columns: Arc<[String]>,
    /// text rendering of the last column read with wl_row_get_str (kept alive for the caller)
    text: String,
    json: Option<CString>,
}

pub struct wl_record {
    json: CString,
}

pub struct wl_writer {
    w: wl::Writer,
    default_source: String,
}

pub struct wl_source {
    kind: SourceKind,
    attrs: Vec<(String, String)>,
}

pub struct wl_record_builder {
    r: Record,
}

// ---------- errors ----------

struct LastError {
    msg: CString,
    span: Option<(usize, usize)>,
}

thread_local! {
    static LAST: RefCell<LastError> = RefCell::new(LastError { msg: CString::default(), span: None });
}

fn set_error(msg: &str, span: Option<(usize, usize)>) {
    let msg = CString::new(msg.replace('\0', " ")).unwrap_or_default();
    LAST.with(|l| *l.borrow_mut() = LastError { msg, span });
}

fn code_of(e: &Error) -> c_int {
    match e.kind() {
        ErrorKind::Locked => WL_E_LOCKED,
        ErrorKind::DaemonStore => WL_E_DAEMON_STORE,
        ErrorKind::ExternalStore => WL_E_EXTERNAL_STORE,
        ErrorKind::NoDaemon => WL_E_NO_DAEMON,
        ErrorKind::NotFound => WL_E_NOT_FOUND,
        ErrorKind::NewerFormat => WL_E_NEWER_FORMAT,
        ErrorKind::Query => WL_E_QUERY,
        ErrorKind::InvalidArgument => WL_E_INVALID,
        _ => WL_E_OTHER,
    }
}

/// An error before it becomes a status code.
struct Fail(c_int, String, Option<(usize, usize)>);

impl From<Error> for Fail {
    fn from(e: Error) -> Fail {
        Fail(code_of(&e), e.message().to_string(), e.span())
    }
}

fn invalid(msg: impl Into<String>) -> Fail {
    Fail(WL_E_INVALID, msg.into(), None)
}

/// Runs `f`, turning errors and panics into status codes. `f` returns the success code (0, or
/// 1/0 for the functions that answer yes/no).
fn guard(f: impl FnOnce() -> Result<c_int, Fail>) -> c_int {
    match catch_unwind(AssertUnwindSafe(f)) {
        Ok(Ok(c)) => c,
        Ok(Err(Fail(code, msg, span))) => {
            set_error(&msg, span);
            code
        }
        Err(p) => {
            let m = p.downcast_ref::<&str>().map(|s| s.to_string()).or_else(|| p.downcast_ref::<String>().cloned()).unwrap_or_default();
            set_error(&format!("internal error (panic): {m}"), None);
            WL_E_PANIC
        }
    }
}

unsafe fn cstr<'a>(p: *const c_char, what: &str) -> Result<&'a str, Fail> {
    if p.is_null() {
        return Err(invalid(format!("{what} is NULL")));
    }
    CStr::from_ptr(p).to_str().map_err(|_| invalid(format!("{what} is not UTF-8")))
}

unsafe fn opt_cstr<'a>(p: *const c_char, what: &str) -> Result<Option<&'a str>, Fail> {
    if p.is_null() {
        Ok(None)
    } else {
        cstr(p, what).map(Some)
    }
}

unsafe fn bytes<'a>(p: *const c_char, len: usize, what: &str) -> Result<&'a [u8], Fail> {
    if len == 0 {
        return Ok(&[]);
    }
    if p.is_null() {
        return Err(invalid(format!("{what} is NULL")));
    }
    Ok(std::slice::from_raw_parts(p as *const u8, len))
}

unsafe fn text<'a>(p: *const c_char, len: usize, what: &str) -> Result<&'a str, Fail> {
    std::str::from_utf8(bytes(p, len, what)?).map_err(|_| invalid(format!("{what} is not UTF-8")))
}

unsafe fn handle<'a, T>(p: *mut T, what: &str) -> Result<&'a mut T, Fail> {
    p.as_mut().ok_or_else(|| invalid(format!("{what} is NULL")))
}

unsafe fn out<T>(p: *mut T, v: T) {
    if !p.is_null() {
        *p = v;
    }
}

// ---------- general ----------

/// The ABI version this library implements (`WL_ABI_VERSION`). Wrappers refuse a library older
/// than the one they were built for.
#[no_mangle]
pub extern "C" fn wl_abi_version() -> u32 {
    WL_ABI_VERSION
}

/// The library version, e.g. "0.1.0". Static.
#[no_mangle]
pub extern "C" fn wl_version() -> *const c_char {
    static V: &str = concat!(env!("CARGO_PKG_VERSION"), "\0");
    V.as_ptr() as *const c_char
}

/// Message of the last error on this thread ("" if none). Valid until the next call on this thread.
#[no_mangle]
pub extern "C" fn wl_last_error() -> *const c_char {
    LAST.with(|l| l.borrow().msg.as_ptr())
}

/// For `WL_E_QUERY`: the byte range of the KQL text the last error is about. Returns 1 when
/// there is one, 0 otherwise.
#[no_mangle]
pub unsafe extern "C" fn wl_last_error_span(start: *mut usize, end: *mut usize) -> c_int {
    LAST.with(|l| match l.borrow().span {
        Some((a, b)) => {
            out(start, a);
            out(end, b);
            1
        }
        None => 0,
    })
}

/// Frees a string the library allocated for the caller (only where a function says so).
#[no_mangle]
pub unsafe extern "C" fn wl_string_free(s: *mut c_char) {
    if !s.is_null() {
        drop(CString::from_raw(s));
    }
}

/// The daemon's default store path. Free with `wl_string_free`.
#[no_mangle]
pub extern "C" fn wl_default_store() -> *mut c_char {
    CString::new(wl::default_store().to_string_lossy().into_owned()).unwrap_or_default().into_raw()
}

// ---------- reading ----------

/// Opens a read-only snapshot of the store at `root` (works with or without wartlogd).
#[no_mangle]
pub unsafe extern "C" fn wl_snapshot_open(root: *const c_char, out_snap: *mut *mut wl_snapshot) -> c_int {
    guard(|| {
        if out_snap.is_null() {
            return Err(invalid("out is NULL"));
        }
        let snap = wl::Snapshot::open(cstr(root, "root")?)?;
        *out_snap = Box::into_raw(Box::new(wl_snapshot { snap }));
        Ok(WL_OK)
    })
}

#[no_mangle]
pub unsafe extern "C" fn wl_snapshot_free(s: *mut wl_snapshot) {
    if !s.is_null() {
        drop(Box::from_raw(s));
    }
}

/// Highest seq in the snapshot, and its record count.
#[no_mangle]
pub unsafe extern "C" fn wl_snapshot_stats(s: *mut wl_snapshot, max_seq: *mut u64, records: *mut u64) -> c_int {
    guard(|| {
        let s = handle(s, "snapshot")?;
        out(max_seq, s.snap.max_seq());
        out(records, s.snap.record_count());
        Ok(WL_OK)
    })
}

/// Runs a KQL query. The cursor must be freed with `wl_cursor_free` (before the snapshot).
#[no_mangle]
pub unsafe extern "C" fn wl_query(s: *mut wl_snapshot, kql: *const c_char, opts: *const wl_query_opts, out_cur: *mut *mut wl_cursor) -> c_int {
    guard(|| {
        let s = handle(s, "snapshot")?;
        if out_cur.is_null() {
            return Err(invalid("out is NULL"));
        }
        let kql = cstr(kql, "kql")?;
        let mut qo = wl::QueryOptions::default();
        if let Some(o) = opts.as_ref() {
            if (o.size as usize) < std::mem::size_of::<wl_query_opts>() {
                return Err(invalid("wl_query_opts.size is too small (set it to sizeof(wl_query_opts))"));
            }
            if o.threads > 0 {
                qo.threads = o.threads as usize;
            }
            qo.limit = match o.limit {
                0 => qo.limit,
                u64::MAX => None,
                n => Some(n),
            };
            let open = |v: i64| v == i64::MIN || v == i64::MAX;
            let (a, b) = (o.from_ns, o.to_ns);
            if !(a == 0 && b == 0) && !(open(a) && open(b)) {
                qo.time_range = Some(((!open(a)).then_some(a), (!open(b)).then_some(b)));
            }
        }
        let rows = s.snap.query(kql, &qo)?;
        let columns = rows.columns().to_vec();
        *out_cur = Box::into_raw(Box::new(wl_cursor { rows, columns, row: None }));
        Ok(WL_OK)
    })
}

#[no_mangle]
pub unsafe extern "C" fn wl_cursor_free(c: *mut wl_cursor) {
    if !c.is_null() {
        drop(Box::from_raw(c));
    }
}

#[no_mangle]
pub unsafe extern "C" fn wl_cursor_columns(c: *mut wl_cursor, count: *mut u32) -> c_int {
    guard(|| {
        let c = handle(c, "cursor")?;
        out(count, c.columns.len() as u32);
        Ok(WL_OK)
    })
}

/// Column name (not NUL-terminated: use the length). Valid until the cursor is freed.
#[no_mangle]
pub unsafe extern "C" fn wl_cursor_column_name(c: *mut wl_cursor, col: u32, p: *mut *const c_char, len: *mut usize) -> c_int {
    guard(|| {
        let c = handle(c, "cursor")?;
        let n = c.columns.get(col as usize).ok_or_else(|| invalid(format!("column {col} out of range")))?;
        out(p, n.as_ptr() as *const c_char);
        out(len, n.len());
        Ok(WL_OK)
    })
}

/// Advances to the next row: returns 1 and sets `*row` (valid until the next call on the cursor),
/// 0 when there are no more rows, or a negative error.
#[no_mangle]
pub unsafe extern "C" fn wl_cursor_next(c: *mut wl_cursor, row: *mut *mut wl_row) -> c_int {
    guard(|| {
        let c = handle(c, "cursor")?;
        match c.rows.next() {
            Some(r) => {
                let columns: Arc<[String]> = match &c.row {
                    Some(prev) => prev.columns.clone(),
                    None => c.columns.clone().into(),
                };
                c.row = Some(wl_row { row: r, columns, text: String::new(), json: None });
                out(row, c.row.as_mut().unwrap() as *mut wl_row);
                Ok(1)
            }
            None => {
                c.row = None;
                out(row, std::ptr::null_mut());
                Ok(0)
            }
        }
    })
}

/// Rows the cursor has not returned yet.
#[no_mangle]
pub unsafe extern "C" fn wl_cursor_remaining(c: *mut wl_cursor, n: *mut u64) -> c_int {
    guard(|| {
        let c = handle(c, "cursor")?;
        out(n, c.rows.remaining() as u64);
        Ok(WL_OK)
    })
}

/// Query statistics as JSON. Free with `wl_string_free`.
#[no_mangle]
pub unsafe extern "C" fn wl_cursor_stats_json(c: *mut wl_cursor, json: *mut *mut c_char) -> c_int {
    guard(|| {
        let c = handle(c, "cursor")?;
        let s = serde_json::to_string(c.rows.stats()).unwrap_or_default();
        out(json, CString::new(s).unwrap_or_default().into_raw());
        Ok(WL_OK)
    })
}

fn value_of(r: &wl_row, col: u32) -> Result<&Value, Fail> {
    r.row.values.get(col as usize).ok_or_else(|| invalid(format!("column {col} out of range")))
}

fn type_of(v: &Value) -> wl_value_type {
    match v {
        Value::Null => wl_value_type::WL_NULL,
        Value::Bool(_) => wl_value_type::WL_BOOL,
        Value::I64(_) => wl_value_type::WL_I64,
        Value::F64(_) => wl_value_type::WL_F64,
        Value::Str(_) => wl_value_type::WL_STR,
        Value::Bytes(_) => wl_value_type::WL_BYTES,
        Value::Timestamp(_) => wl_value_type::WL_TIMESTAMP,
        Value::Duration(_) => wl_value_type::WL_DURATION,
        Value::Json(_) => wl_value_type::WL_JSON,
    }
}

#[no_mangle]
pub unsafe extern "C" fn wl_row_type(r: *mut wl_row, col: u32, t: *mut wl_value_type) -> c_int {
    guard(|| {
        let r = handle(r, "row")?;
        out(t, type_of(value_of(r, col)?));
        Ok(WL_OK)
    })
}

/// The column as text: strings and JSON as stored, bytes raw, other types rendered (timestamps
/// as RFC 3339). `WL_E_TYPE` for null. Not NUL-terminated; valid until the next call on the row
/// or cursor.
#[no_mangle]
pub unsafe extern "C" fn wl_row_get_str(r: *mut wl_row, col: u32, p: *mut *const c_char, len: *mut usize) -> c_int {
    guard(|| {
        let r = handle(r, "row")?;
        let (ptr, n) = match value_of(r, col)? {
            Value::Null => return Err(Fail(WL_E_TYPE, format!("column {col} is null"), None)),
            Value::Str(s) | Value::Json(s) => (s.as_ptr(), s.len()),
            Value::Bytes(b) => (b.as_ptr(), b.len()),
            v => {
                r.text = v.to_text();
                (r.text.as_ptr(), r.text.len())
            }
        };
        out(p, ptr as *const c_char);
        out(len, n);
        Ok(WL_OK)
    })
}

/// Integers, timestamps (ns), durations (ns) and booleans (0/1). `WL_E_TYPE` otherwise.
#[no_mangle]
pub unsafe extern "C" fn wl_row_get_i64(r: *mut wl_row, col: u32, v: *mut i64) -> c_int {
    guard(|| {
        let r = handle(r, "row")?;
        let x = match value_of(r, col)? {
            Value::I64(i) | Value::Timestamp(i) | Value::Duration(i) => *i,
            Value::Bool(b) => *b as i64,
            other => return Err(Fail(WL_E_TYPE, format!("column {col} is {:?}, not an integer", type_of(other)), None)),
        };
        out(v, x);
        Ok(WL_OK)
    })
}

/// Floats and integers. `WL_E_TYPE` otherwise.
#[no_mangle]
pub unsafe extern "C" fn wl_row_get_f64(r: *mut wl_row, col: u32, v: *mut f64) -> c_int {
    guard(|| {
        let r = handle(r, "row")?;
        let x = match value_of(r, col)? {
            Value::F64(f) => *f,
            Value::I64(i) => *i as f64,
            other => return Err(Fail(WL_E_TYPE, format!("column {col} is {:?}, not a number", type_of(other)), None)),
        };
        out(v, x);
        Ok(WL_OK)
    })
}

#[no_mangle]
pub unsafe extern "C" fn wl_row_get_bool(r: *mut wl_row, col: u32, v: *mut c_int) -> c_int {
    guard(|| {
        let r = handle(r, "row")?;
        match value_of(r, col)? {
            Value::Bool(b) => out(v, *b as c_int),
            other => return Err(Fail(WL_E_TYPE, format!("column {col} is {:?}, not a bool", type_of(other)), None)),
        }
        Ok(WL_OK)
    })
}

/// The record id (seq) when the row is still one record: returns 1 and sets `*seq`, else 0.
#[no_mangle]
pub unsafe extern "C" fn wl_row_id(r: *mut wl_row, seq: *mut u64) -> c_int {
    guard(|| {
        let r = handle(r, "row")?;
        match r.row.id {
            Some(id) => {
                out(seq, id);
                Ok(1)
            }
            None => Ok(0),
        }
    })
}

/// The row as a JSON object keyed by column name (NULL on error). Valid until the next call on
/// the cursor.
#[no_mangle]
pub unsafe extern "C" fn wl_row_json(r: *mut wl_row) -> *const c_char {
    let mut p = std::ptr::null();
    guard(|| {
        let r = handle(r, "row")?;
        let mut m = serde_json::Map::new();
        for (c, v) in r.columns.iter().zip(&r.row.values) {
            m.insert(c.clone(), v.to_json());
        }
        r.json = Some(CString::new(serde_json::Value::Object(m).to_string()).unwrap_or_default());
        p = r.json.as_ref().unwrap().as_ptr();
        Ok(WL_OK)
    });
    p
}

/// Looks up a record by seq: returns 1 and sets `*out` (free with `wl_record_free`), 0 when the
/// store does not hold it (any more), or a negative error.
#[no_mangle]
pub unsafe extern "C" fn wl_record_get(s: *mut wl_snapshot, seq: u64, out_rec: *mut *mut wl_record) -> c_int {
    guard(|| {
        let s = handle(s, "snapshot")?;
        match s.snap.get(seq)? {
            Some(r) => {
                let json = CString::new(wl::record_json(&r)).unwrap_or_default();
                out(out_rec, Box::into_raw(Box::new(wl_record { json })));
                Ok(1)
            }
            None => {
                out(out_rec, std::ptr::null_mut());
                Ok(0)
            }
        }
    })
}

/// The whole record as JSON. Valid until the record is freed.
#[no_mangle]
pub unsafe extern "C" fn wl_record_json(r: *mut wl_record) -> *const c_char {
    match r.as_ref() {
        Some(r) => r.json.as_ptr(),
        None => std::ptr::null(),
    }
}

#[no_mangle]
pub unsafe extern "C" fn wl_record_free(r: *mut wl_record) {
    if !r.is_null() {
        drop(Box::from_raw(r));
    }
}

/// Checks the store's seq rules (read-only). Returns 0 and sets `*findings` (0 means the store
/// is fine) and, if `report_json` is not NULL, a JSON report to free with `wl_string_free`.
/// `quick` reads block metadata only.
#[no_mangle]
pub unsafe extern "C" fn wl_store_verify(root: *const c_char, quick: c_int, findings: *mut u64, report_json: *mut *mut c_char) -> c_int {
    guard(|| {
        let rep = wl::verify_store(cstr(root, "root")?, quick != 0, 1000)?;
        out(findings, rep.finding_count);
        if !report_json.is_null() {
            let j = serde_json::json!({
                "ok": rep.ok(),
                "segments": rep.segments,
                "blocks": rep.blocks,
                "records": rep.records,
                "wal_records": rep.wal_records,
                "full": rep.full,
                "finding_count": rep.finding_count,
                "findings": rep.findings.iter().map(|f| f.describe()).collect::<Vec<_>>(),
                "writers": rep.writers.iter().map(|(k, n)| serde_json::json!({ "kind": k.name(), "segments": n })).collect::<Vec<_>>(),
            });
            *report_json = CString::new(j.to_string()).unwrap_or_default().into_raw();
        }
        Ok(WL_OK)
    })
}

// ---------- writing ----------

/// Opens a writer. See `wl_writer_mode` for what each mode accepts; mismatches are refused with
/// their own code (`WL_E_DAEMON_STORE`, `WL_E_NO_DAEMON`, `WL_E_LOCKED`, `WL_E_NOT_FOUND`).
#[no_mangle]
pub unsafe extern "C" fn wl_writer_open(root: *const c_char, mode: c_int, opts: *const wl_writer_opts, out_w: *mut *mut wl_writer) -> c_int {
    guard(|| {
        if out_w.is_null() {
            return Err(invalid("out is NULL"));
        }
        let root = cstr(root, "root")?;
        let mode = match mode {
            1 => wl::WriteMode::Shared,
            2 => wl::WriteMode::Private,
            3 => wl::WriteMode::Exclusive,
            m => return Err(invalid(format!("unknown writer mode {m}"))),
        };
        let mut o = wl::WriterOptions::new(mode);
        o.creator = format!("wartlog-ffi {}", env!("CARGO_PKG_VERSION"));
        let mut default_source = "app".to_string();
        if let Some(x) = opts.as_ref() {
            if (x.size as usize) < std::mem::size_of::<wl_writer_opts>() {
                return Err(invalid("wl_writer_opts.size is too small (set it to sizeof(wl_writer_opts))"));
            }
            if x.sync_every_ms > 0 {
                o.sync_every = Duration::from_millis(x.sync_every_ms as u64);
            }
            if let Some(s) = opt_cstr(x.socket, "socket")? {
                o.socket = Some(PathBuf::from(s));
            }
            if let Some(c) = opt_cstr(x.creator, "creator")? {
                o.creator = c.to_string();
            }
            if let Some(d) = opt_cstr(x.default_source, "default_source")? {
                default_source = d.to_string();
            }
            o.retention_max_bytes = (x.retention_max_bytes > 0).then_some(x.retention_max_bytes);
            o.retention_max_age = (x.retention_max_age_s > 0).then(|| Duration::from_secs(x.retention_max_age_s));
        }
        let w = wl::Writer::open(root, o)?;
        *out_w = Box::into_raw(Box::new(wl_writer { w, default_source }));
        Ok(WL_OK)
    })
}

/// Normalizes one NDJSON (or plain text) line as wartlogd's NDJSON receivers do, and writes it.
#[no_mangle]
pub unsafe extern "C" fn wl_writer_push_json(w: *mut wl_writer, line: *const c_char, len: usize, seq: *mut u64) -> c_int {
    guard(|| {
        let w = handle(w, "writer")?;
        let line = text(line, len, "line")?;
        let s = w.w.push_json(line, &w.default_source)?;
        out(seq, s);
        Ok(WL_OK)
    })
}

/// A source descriptor ("who sent this"): a kind and identifying attributes.
#[no_mangle]
pub unsafe extern "C" fn wl_source_new(kind: c_int, out_src: *mut *mut wl_source) -> c_int {
    guard(|| {
        if out_src.is_null() {
            return Err(invalid("out is NULL"));
        }
        let kind = match kind {
            1..=255 => SourceKind::from_u8(kind as u8).unwrap(),
            k => return Err(invalid(format!("unknown source kind {k}"))),
        };
        *out_src = Box::into_raw(Box::new(wl_source { kind, attrs: Vec::new() }));
        Ok(WL_OK)
    })
}

/// Adds an identifying attribute (`service.name` for otlp, `source.name` for ndjson, ...). Keep
/// per-run values (pids, instance ids) off the source: put them on records.
#[no_mangle]
pub unsafe extern "C" fn wl_source_set_attr(s: *mut wl_source, key: *const c_char, klen: usize, val: *const c_char, vlen: usize) -> c_int {
    guard(|| {
        let s = handle(s, "source")?;
        let k = text(key, klen, "key")?.to_string();
        let v = text(val, vlen, "value")?.to_string();
        s.attrs.retain(|(a, _)| *a != k);
        s.attrs.push((k, v));
        Ok(WL_OK)
    })
}

#[no_mangle]
pub unsafe extern "C" fn wl_source_free(s: *mut wl_source) {
    if !s.is_null() {
        drop(Box::from_raw(s));
    }
}

/// A new log record from this source (copied: the source may be freed or reused afterwards),
/// timestamped now with severity 9 (info). NULL if `src` is NULL. Consumed by `wl_writer_push`,
/// or freed with `wl_record_builder_free`.
#[no_mangle]
pub unsafe extern "C" fn wl_record_new(src: *const wl_source) -> *mut wl_record_builder {
    let Some(s) = src.as_ref() else {
        set_error("source is NULL", None);
        return std::ptr::null_mut();
    };
    let source = Arc::new(Source::new(s.kind, s.attrs.clone()));
    Box::into_raw(Box::new(wl_record_builder { r: Record::log(source, wl::now_ns(), 9, "") }))
}

#[no_mangle]
pub unsafe extern "C" fn wl_record_builder_free(b: *mut wl_record_builder) {
    if !b.is_null() {
        drop(Box::from_raw(b));
    }
}

/// Event time, ns since the epoch.
#[no_mangle]
pub unsafe extern "C" fn wl_record_set_time(b: *mut wl_record_builder, ns: i64) -> c_int {
    guard(|| {
        handle(b, "record")?.r.time_ns = ns;
        Ok(WL_OK)
    })
}

/// OpenTelemetry severity number (1..24: 5 debug, 9 info, 13 warn, 17 error, 21 fatal) and
/// optional text (NULL/0 for none).
#[no_mangle]
pub unsafe extern "C" fn wl_record_set_severity(b: *mut wl_record_builder, number: u8, t: *const c_char, len: usize) -> c_int {
    guard(|| {
        let b = handle(b, "record")?;
        if number > 24 {
            return Err(invalid(format!("severity {number} is above 24")));
        }
        b.r.severity = number;
        b.r.severity_text = if t.is_null() || len == 0 { None } else { Some(text(t, len, "severity text")?.to_string()) };
        Ok(WL_OK)
    })
}

#[no_mangle]
pub unsafe extern "C" fn wl_record_set_body(b: *mut wl_record_builder, p: *const c_char, len: usize) -> c_int {
    guard(|| {
        let b = handle(b, "record")?;
        b.r.body = text(p, len, "body")?.to_string();
        Ok(WL_OK)
    })
}

/// A message template (`Order {OrderId} shipped`), rendered from attributes of the same names.
#[no_mangle]
pub unsafe extern "C" fn wl_record_set_template(b: *mut wl_record_builder, p: *const c_char, len: usize) -> c_int {
    guard(|| {
        let b = handle(b, "record")?;
        b.r.template = Some(text(p, len, "template")?.to_string());
        Ok(WL_OK)
    })
}

/// Trace and span ids (16 and 8 bytes). `span_id` may be NULL.
#[no_mangle]
pub unsafe extern "C" fn wl_record_set_trace(b: *mut wl_record_builder, trace_id: *const u8, span_id: *const u8) -> c_int {
    guard(|| {
        let b = handle(b, "record")?;
        if trace_id.is_null() {
            return Err(invalid("trace_id is NULL"));
        }
        b.r.trace_id = Some(std::slice::from_raw_parts(trace_id, 16).try_into().unwrap());
        b.r.span_id = (!span_id.is_null()).then(|| std::slice::from_raw_parts(span_id, 8).try_into().unwrap());
        Ok(WL_OK)
    })
}

fn set_attr(b: &mut wl_record_builder, k: &str, v: Value) {
    b.r.attrs.retain(|(a, _)| a != k);
    b.r.attrs.push((k.to_string(), v));
}

#[no_mangle]
pub unsafe extern "C" fn wl_record_set_attr_str(b: *mut wl_record_builder, k: *const c_char, kl: usize, v: *const c_char, vl: usize) -> c_int {
    guard(|| {
        let b = handle(b, "record")?;
        let (k, v) = (text(k, kl, "key")?, text(v, vl, "value")?);
        set_attr(b, k, Value::Str(v.to_string()));
        Ok(WL_OK)
    })
}

#[no_mangle]
pub unsafe extern "C" fn wl_record_set_attr_i64(b: *mut wl_record_builder, k: *const c_char, kl: usize, v: i64) -> c_int {
    guard(|| {
        let b = handle(b, "record")?;
        let k = text(k, kl, "key")?;
        set_attr(b, k, Value::I64(v));
        Ok(WL_OK)
    })
}

#[no_mangle]
pub unsafe extern "C" fn wl_record_set_attr_f64(b: *mut wl_record_builder, k: *const c_char, kl: usize, v: f64) -> c_int {
    guard(|| {
        let b = handle(b, "record")?;
        let k = text(k, kl, "key")?;
        set_attr(b, k, Value::F64(v));
        Ok(WL_OK)
    })
}

#[no_mangle]
pub unsafe extern "C" fn wl_record_set_attr_bool(b: *mut wl_record_builder, k: *const c_char, kl: usize, v: c_int) -> c_int {
    guard(|| {
        let b = handle(b, "record")?;
        let k = text(k, kl, "key")?;
        set_attr(b, k, Value::Bool(v != 0));
        Ok(WL_OK)
    })
}

/// Writes the record and consumes the builder (also on error: do not free it afterwards).
/// `*seq` gets the record's id.
#[no_mangle]
pub unsafe extern "C" fn wl_writer_push(w: *mut wl_writer, b: *mut wl_record_builder, seq: *mut u64) -> c_int {
    let rec = (!b.is_null()).then(|| Box::from_raw(b));
    guard(|| {
        let w = handle(w, "writer")?;
        let mut rec = rec.ok_or_else(|| invalid("record is NULL"))?.r;
        if let Some(t) = &rec.template {
            // render args from same-named attributes, as receivers do for structured logs
            rec.args = wl::template::holes(t)
                .iter()
                .map(|h| rec.attrs.iter().find(|(k, _)| *k == h.name).map(|(_, v)| v.clone()).unwrap_or(Value::Null))
                .collect();
        }
        let s = w.w.push(rec)?;
        out(seq, s);
        Ok(WL_OK)
    })
}

/// Blocks until everything pushed is durable; `*durable_seq` gets the highest durable seq.
#[no_mangle]
pub unsafe extern "C" fn wl_writer_flush(w: *mut wl_writer, durable_seq: *mut u64) -> c_int {
    guard(|| {
        let w = handle(w, "writer")?;
        let d = w.w.flush()?;
        out(durable_seq, d);
        Ok(WL_OK)
    })
}

/// Flushes, seals everything (private/exclusive: no WAL left to recover) and frees the writer,
/// also when it returns an error.
#[no_mangle]
pub unsafe extern "C" fn wl_writer_close(w: *mut wl_writer) -> c_int {
    let w = (!w.is_null()).then(|| Box::from_raw(w));
    guard(|| {
        let w = w.ok_or_else(|| invalid("writer is NULL"))?;
        w.w.close()?;
        Ok(WL_OK)
    })
}
