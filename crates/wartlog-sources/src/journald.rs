//! journald collector through sd-journal. libsystemd is loaded at run time (`libsystemd.so.0`),
//! so building WartLog needs no systemd development files and a missing library is a clear error.

use anyhow::{anyhow, bail, Result};
use std::ffi::{c_char, c_int, c_void, CStr, CString};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use wartlog_model::{severity_from_syslog, time::now_ns, Record, Source, SourceKind, Value};

use crate::{Collector, CursorStore, Shutdown, Sink};

const SD_JOURNAL_LOCAL_ONLY: c_int = 1;
const SD_JOURNAL_SYSTEM: c_int = 4;
const SD_JOURNAL_CURRENT_USER: c_int = 8;

#[repr(C)]
struct SdJournal {
    _p: [u8; 0],
}

type OpenFn = unsafe extern "C" fn(*mut *mut SdJournal, c_int) -> c_int;
type OpenDirFn = unsafe extern "C" fn(*mut *mut SdJournal, *const c_char, c_int) -> c_int;
type CloseFn = unsafe extern "C" fn(*mut SdJournal);
type NextFn = unsafe extern "C" fn(*mut SdJournal) -> c_int;
type U64Fn = unsafe extern "C" fn(*mut SdJournal, *mut u64) -> c_int;
type EnumFn = unsafe extern "C" fn(*mut SdJournal, *mut *const c_void, *mut usize) -> c_int;
type RestartFn = unsafe extern "C" fn(*mut SdJournal);
type GetCursorFn = unsafe extern "C" fn(*mut SdJournal, *mut *mut c_char) -> c_int;
type SeekCursorFn = unsafe extern "C" fn(*mut SdJournal, *const c_char) -> c_int;
type TestCursorFn = unsafe extern "C" fn(*mut SdJournal, *const c_char) -> c_int;
type SeekUsecFn = unsafe extern "C" fn(*mut SdJournal, u64) -> c_int;
type WaitFn = unsafe extern "C" fn(*mut SdJournal, u64) -> c_int;
type ThresholdFn = unsafe extern "C" fn(*mut SdJournal, usize) -> c_int;
type GetDataFn = unsafe extern "C" fn(*mut SdJournal, *const c_char, *mut *const c_void, *mut usize) -> c_int;
type FreeFn = unsafe extern "C" fn(*mut c_void);

struct Lib {
    _lib: libloading::Library,
    _libc: libloading::Library,
    open: OpenFn,
    open_dir: OpenDirFn,
    close: CloseFn,
    next: NextFn,
    realtime: U64Fn,
    enumerate: EnumFn,
    restart: RestartFn,
    get_cursor: GetCursorFn,
    seek_cursor: SeekCursorFn,
    test_cursor: TestCursorFn,
    seek_realtime: SeekUsecFn,
    wait: WaitFn,
    threshold: ThresholdFn,
    get_data: GetDataFn,
    free: FreeFn,
}

fn lib() -> Result<&'static Lib> {
    static LIB: std::sync::OnceLock<Result<Lib, String>> = std::sync::OnceLock::new();
    LIB.get_or_init(|| unsafe {
        let l = libloading::Library::new("libsystemd.so.0").map_err(|e| format!("cannot load libsystemd.so.0: {e}"))?;
        let c = libloading::Library::new("libc.so.6").map_err(|e| format!("cannot load libc: {e}"))?;
        macro_rules! sym {
            ($lib:expr, $name:literal) => {
                *$lib.get($name).map_err(|e| format!("libsystemd lacks {}: {e}", String::from_utf8_lossy($name)))?
            };
        }
        Ok(Lib {
            open: sym!(l, b"sd_journal_open"),
            open_dir: sym!(l, b"sd_journal_open_directory"),
            close: sym!(l, b"sd_journal_close"),
            next: sym!(l, b"sd_journal_next"),
            realtime: {
                let f: libloading::Symbol<unsafe extern "C" fn(*mut SdJournal, *mut u64) -> c_int> = l.get(b"sd_journal_get_realtime_usec").map_err(|e| e.to_string())?;
                *f
            },
            enumerate: sym!(l, b"sd_journal_enumerate_data"),
            restart: sym!(l, b"sd_journal_restart_data"),
            get_cursor: sym!(l, b"sd_journal_get_cursor"),
            seek_cursor: sym!(l, b"sd_journal_seek_cursor"),
            test_cursor: sym!(l, b"sd_journal_test_cursor"),
            seek_realtime: sym!(l, b"sd_journal_seek_realtime_usec"),
            wait: sym!(l, b"sd_journal_wait"),
            threshold: sym!(l, b"sd_journal_set_data_threshold"),
            get_data: sym!(l, b"sd_journal_get_data"),
            free: sym!(c, b"free"),
            _lib: l,
            _libc: c,
        })
    })
    .as_ref()
    .map_err(|e| anyhow!("{e}"))
}

/// Which journal to read.
#[derive(Clone, Debug, PartialEq)]
pub enum JournalKind {
    User,
    System,
    /// journal files in a directory (for imports and tests)
    Directory(PathBuf),
}

impl JournalKind {
    pub fn label(&self) -> &str {
        match self {
            JournalKind::User => "user",
            JournalKind::System => "system",
            JournalKind::Directory(_) => "directory",
        }
    }
}

/// An open journal. Not thread-safe; lives on one blocking thread.
pub struct Journal {
    j: *mut SdJournal,
    l: &'static Lib,
}

// The handle is only ever used from the thread that owns it; moving it between threads is fine.
unsafe impl Send for Journal {}

fn check(r: c_int, what: &str) -> Result<c_int> {
    if r < 0 {
        bail!("{what}: {}", std::io::Error::from_raw_os_error(-r));
    }
    Ok(r)
}

impl Journal {
    pub fn open(kind: &JournalKind) -> Result<Journal> {
        let l = lib()?;
        let mut j = std::ptr::null_mut();
        unsafe {
            match kind {
                JournalKind::User => check((l.open)(&mut j, SD_JOURNAL_LOCAL_ONLY | SD_JOURNAL_CURRENT_USER), "sd_journal_open (user)")?,
                JournalKind::System => check((l.open)(&mut j, SD_JOURNAL_LOCAL_ONLY | SD_JOURNAL_SYSTEM), "sd_journal_open (system)")?,
                JournalKind::Directory(p) => {
                    let c = CString::new(p.as_os_str().as_encoded_bytes())?;
                    check((l.open_dir)(&mut j, c.as_ptr(), 0), "sd_journal_open_directory")?
                }
            };
            (l.threshold)(j, 64 * 1024);
        }
        Ok(Journal { j, l })
    }

    /// Advances; false at the end.
    pub fn advance(&mut self) -> Result<bool> {
        Ok(check(unsafe { (self.l.next)(self.j) }, "sd_journal_next")? > 0)
    }

    pub fn cursor(&mut self) -> Result<String> {
        let mut p: *mut c_char = std::ptr::null_mut();
        check(unsafe { (self.l.get_cursor)(self.j, &mut p) }, "sd_journal_get_cursor")?;
        let s = unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned();
        unsafe { (self.l.free)(p as *mut c_void) };
        Ok(s)
    }

    /// Positions after the entry at `cursor` (the next `next()` returns the entry after it).
    pub fn seek_after_cursor(&mut self, cursor: &str) -> Result<()> {
        let c = CString::new(cursor)?;
        check(unsafe { (self.l.seek_cursor)(self.j, c.as_ptr()) }, "sd_journal_seek_cursor")?;
        // seeking puts us before the cursor entry: step onto it, and if it is the entry itself, stay
        if self.advance()? && unsafe { (self.l.test_cursor)(self.j, c.as_ptr()) } <= 0 {
            // the cursor entry is gone (rotated away); we are already on the next entry, so step back
            // by re-seeking and not consuming it
            check(unsafe { (self.l.seek_cursor)(self.j, c.as_ptr()) }, "sd_journal_seek_cursor")?;
        }
        Ok(())
    }

    pub fn seek_realtime(&mut self, ns: i64) -> Result<()> {
        check(unsafe { (self.l.seek_realtime)(self.j, (ns.max(0) / 1000) as u64) }, "sd_journal_seek_realtime_usec")?;
        Ok(())
    }

    pub fn wait(&mut self, timeout: Duration) -> Result<()> {
        check(unsafe { (self.l.wait)(self.j, timeout.as_micros() as u64) }, "sd_journal_wait")?;
        Ok(())
    }

    pub fn realtime_ns(&mut self) -> Result<i64> {
        let mut u = 0u64;
        check(unsafe { (self.l.realtime)(self.j, &mut u) }, "sd_journal_get_realtime_usec")?;
        Ok(u as i64 * 1000)
    }

    pub fn field(&mut self, name: &str) -> Option<String> {
        let c = CString::new(name).ok()?;
        let mut d: *const c_void = std::ptr::null();
        let mut n = 0usize;
        if unsafe { (self.l.get_data)(self.j, c.as_ptr(), &mut d, &mut n) } < 0 {
            return None;
        }
        let b = unsafe { std::slice::from_raw_parts(d as *const u8, n) };
        let v = &b[name.len() + 1..];
        Some(String::from_utf8_lossy(v).into_owned())
    }

    /// Every FIELD=value of the current entry.
    pub fn fields(&mut self) -> Vec<(String, String)> {
        let mut out = Vec::new();
        unsafe { (self.l.restart)(self.j) };
        loop {
            let mut d: *const c_void = std::ptr::null();
            let mut n = 0usize;
            let r = unsafe { (self.l.enumerate)(self.j, &mut d, &mut n) };
            if r <= 0 {
                break;
            }
            let b = unsafe { std::slice::from_raw_parts(d as *const u8, n) };
            if let Some(eq) = b.iter().position(|&c| c == b'=') {
                out.push((String::from_utf8_lossy(&b[..eq]).into_owned(), String::from_utf8_lossy(&b[eq + 1..]).into_owned()));
            }
        }
        out
    }
}

impl Drop for Journal {
    fn drop(&mut self) {
        unsafe { (self.l.close)(self.j) };
    }
}

const NUMERIC: &[&str] = &["_PID", "_UID", "_GID", "TID", "CODE_LINE", "ERRNO", "_AUDIT_SESSION", "_AUDIT_LOGINUID", "SYSLOG_PID", "SYSLOG_FACILITY"];

/// Maps one journal entry (spec "MVP collectors: journald").
pub fn entry_to_record(fields: Vec<(String, String)>, realtime_ns: i64, journal: &str) -> Record {
    let get = |k: &str| fields.iter().find(|(a, _)| a == k).map(|(_, v)| v.clone());
    let body = get("MESSAGE").unwrap_or_default();
    let prio: u8 = get("PRIORITY").and_then(|p| p.trim().parse().ok()).unwrap_or(6);
    let unit = if journal == "user" { get("_SYSTEMD_USER_UNIT").or_else(|| get("_SYSTEMD_UNIT")) } else { get("_SYSTEMD_UNIT").or_else(|| get("_SYSTEMD_USER_UNIT")) };
    let mut src = vec![("journal".to_string(), journal.to_string())];
    match unit {
        Some(u) => src.push(("systemd.unit".into(), u)),
        None => src.push(("syslog.identifier".into(), get("SYSLOG_IDENTIFIER").or_else(|| get("_COMM")).unwrap_or_else(|| "unknown".into()))),
    }
    if let Some(h) = get("_HOSTNAME") {
        src.push(("host.name".into(), h));
    }
    let mut attrs = Vec::with_capacity(fields.len());
    let mut thread = None;
    for (k, v) in fields {
        if k == "MESSAGE" || k == "PRIORITY" || k.starts_with("__") || k == "_SOURCE_REALTIME_TIMESTAMP" {
            continue;
        }
        if k == "TID" {
            thread = Some(v.clone());
        }
        let val = if NUMERIC.contains(&k.as_str()) { v.parse::<i64>().map(Value::I64).unwrap_or(Value::Str(v)) } else { Value::Str(v) };
        attrs.push((k, val));
    }
    if let Some(t) = thread.or_else(|| attrs.iter().find(|(k, _)| k == "_PID").map(|(_, v)| v.to_text())) {
        attrs.push(("thread.id".into(), t.parse::<i64>().map(Value::I64).unwrap_or(Value::Str(t))));
    }
    let src = Arc::new(Source::new(SourceKind::Journald, src));
    let mut r = Record::log(src, realtime_ns, severity_from_syslog(prio), body);
    r.observed_ns = now_ns();
    r.attrs = attrs;
    r
}

pub struct JournaldCollector {
    pub name: String,
    pub kind: JournalKind,
    /// how far back to import on first run
    pub backfill: Duration,
    /// units to include (empty = all) and to exclude
    pub include: Vec<String>,
    pub exclude: Vec<String>,
    /// stop at the end instead of following (imports)
    pub follow: bool,
}

impl JournaldCollector {
    fn cursor_key(&self) -> String {
        match &self.kind {
            JournalKind::Directory(p) => format!("journald:dir:{}", p.display()),
            k => format!("journald:{}", k.label()),
        }
    }

    fn wanted(&self, r: &Record) -> bool {
        let unit = r.source.get("systemd.unit").or_else(|| r.source.get("syslog.identifier")).unwrap_or("");
        (self.include.is_empty() || self.include.iter().any(|u| u == unit)) && !self.exclude.iter().any(|u| u == unit)
    }

    /// The blocking read loop.
    pub fn run_blocking(&self, sink: &Sink, cursors: &CursorStore, shutdown: &Shutdown, rt: &tokio::runtime::Handle) -> Result<u64> {
        let mut j = Journal::open(&self.kind)?;
        let key = self.cursor_key();
        match cursors.get(&key) {
            Some(c) => j.seek_after_cursor(&c)?,
            None => j.seek_realtime(now_ns() - self.backfill.as_nanos() as i64)?,
        }
        let journal = match &self.kind {
            JournalKind::Directory(_) => "user",
            k => k.label(),
        }
        .to_string();
        let mut total = 0u64;
        let mut batch = Vec::with_capacity(1024);
        let mut last_cursor: Option<String> = None;
        loop {
            if *shutdown.borrow() {
                break;
            }
            let more = j.advance()?;
            if more {
                let ts = j.realtime_ns()?;
                let r = entry_to_record(j.fields(), ts, &journal);
                if self.wanted(&r) {
                    batch.push(r);
                }
                last_cursor = Some(j.cursor()?);
            }
            if batch.len() >= 1024 || (!more && (last_cursor.is_some() || !batch.is_empty())) {
                let n = batch.len() as u64;
                if !batch.is_empty() {
                    rt.block_on(sink.send_durable(std::mem::take(&mut batch)))?;
                }
                if let Some(c) = last_cursor.take() {
                    cursors.set(&key, &c)?;
                }
                total += n;
            }
            if !more {
                if !self.follow {
                    break;
                }
                j.wait(Duration::from_millis(500))?;
            }
        }
        Ok(total)
    }
}

#[async_trait::async_trait]
impl Collector for JournaldCollector {
    fn name(&self) -> &str {
        &self.name
    }

    async fn run(self: Box<Self>, sink: Sink, cursors: CursorStore, shutdown: Shutdown) -> Result<()> {
        let rt = tokio::runtime::Handle::current();
        let me = *self;
        tokio::task::spawn_blocking(move || {
            let r = me.run_blocking(&sink, &cursors, &shutdown, &rt);
            if let Err(e) = &r {
                let mut msg = format!("{e:#}");
                if me.kind == JournalKind::System && msg.contains("ermission") {
                    msg.push_str(" (to read the system journal, add yourself to the systemd-journal group)");
                }
                sink.status.error(&msg);
            }
            r.map(|_| ())
        })
        .await?
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_entries() {
        let f = vec![
            ("MESSAGE".to_string(), "Started foo".to_string()),
            ("PRIORITY".into(), "3".into()),
            ("_SYSTEMD_USER_UNIT".into(), "foo.service".into()),
            ("_PID".into(), "42".into()),
            ("_HOSTNAME".into(), "valkyrie".into()),
            ("__CURSOR".into(), "x".into()),
        ];
        let r = entry_to_record(f, 5, "user");
        assert_eq!(r.severity, 17);
        assert_eq!(r.body, "Started foo");
        assert_eq!(r.source.display_name(), "foo.service");
        assert_eq!(r.attr("_PID"), Some(&Value::I64(42)));
        assert_eq!(r.attr("thread.id"), Some(&Value::I64(42)));
        assert!(r.attr("__CURSOR").is_none());
    }
}
