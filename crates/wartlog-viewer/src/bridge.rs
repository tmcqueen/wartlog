//! The one QObject the QML talks to. Heavy work runs on a tokio runtime; results come back to the
//! Qt thread through `qt_thread().queue`, as JSON strings the QML parses.

#[cxx_qt::bridge]
pub mod qobject {
    unsafe extern "C++" {
        include!("cxx-qt-lib/qstring.h");
        type QString = cxx_qt_lib::QString;
    }

    #[auto_cxx_name]
    extern "RustQt" {
        #[qobject]
        #[qml_element]
        #[qproperty(QString, status)]
        #[qproperty(QString, error)]
        #[qproperty(i32, error_start)]
        #[qproperty(i32, error_end)]
        #[qproperty(bool, live)]
        #[qproperty(bool, daemon)]
        #[qproperty(bool, busy)]
        #[qproperty(QString, sources)]
        #[qproperty(QString, details)]
        #[qproperty(QString, context)]
        #[qproperty(QString, trace)]
        #[qproperty(QString, templates)]
        #[qproperty(QString, views)]
        #[qproperty(QString, store_path)]
        /// set from WARTLOG_SCREENSHOT: the UI saves a screenshot there and quits (for checks)
        #[qproperty(QString, screenshot)]
        #[qproperty(QString, screenshot_step)]
        type Backend = super::BackendRust;

        /// History query (in-process, read-only); replaces the rows.
        #[qinvokable]
        fn run_query(self: Pin<&mut Backend>, query: &QString, range: &QString);
        /// Starts or stops live tail through the daemon; new rows are prepended.
        #[qinvokable]
        fn set_tail(self: Pin<&mut Backend>, on: bool, query: &QString);
        #[qinvokable]
        fn show_details(self: Pin<&mut Backend>, id: f64);
        #[qinvokable]
        fn load_context(self: Pin<&mut Backend>, id: f64, scope: &QString);
        #[qinvokable]
        fn load_trace(self: Pin<&mut Backend>, trace_id: &QString);
        #[qinvokable]
        fn load_templates(self: Pin<&mut Backend>, query: &QString, range: &QString);
        /// Sources, schema (for completion) and daemon state.
        #[qinvokable]
        fn refresh(self: Pin<&mut Backend>);
        #[qinvokable]
        fn complete(self: &Backend, text: &QString, cursor: i32) -> QString;
        #[qinvokable]
        fn add_filter(self: &Backend, query: &QString, column: &QString, value_json: &QString, exclude: bool) -> QString;
        #[qinvokable]
        fn only_source(self: &Backend, query: &QString, display: &QString, kind: &QString) -> QString;
        #[qinvokable]
        fn save_view(self: Pin<&mut Backend>, name: &QString, query: &QString, range: &QString);
        #[qinvokable]
        fn delete_view(self: Pin<&mut Backend>, name: &QString);
        #[qinvokable]
        fn start_daemon(self: Pin<&mut Backend>);
        #[qinvokable]
        fn add_source(self: Pin<&mut Backend>, role: &QString, config: &QString);
        #[qinvokable]
        fn remove_source(self: Pin<&mut Backend>, name: &QString);

        /// `json` is `logic::rows_json`; `replace` false means prepend (live tail).
        #[qsignal]
        fn rows_ready(self: Pin<&mut Backend>, json: QString, replace: bool);
        /// a one-line message for the status bar or a toast
        #[qsignal]
        fn notice(self: Pin<&mut Backend>, text: QString);
    }

    impl cxx_qt::Threading for Backend {}
}

use crate::logic;
use core::pin::Pin;
use cxx_qt::Threading;
use cxx_qt_lib::QString;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use wartlog_api::proto::{ContextScope, Request, Response, Schema};
use wartlog_api::{ops, Client};
use wartlog_store::Snapshot;

/// Template argument highlight color (works on light and dark themes).
const ACCENT: &str = "#3daee9";
const ROW_LIMIT: u64 = 5000;

fn rt() -> &'static tokio::runtime::Runtime {
    static RT: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    RT.get_or_init(|| tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().thread_name("wartlog-viewer").build().expect("runtime"))
}

pub struct BackendRust {
    status: QString,
    error: QString,
    error_start: i32,
    error_end: i32,
    live: bool,
    daemon: bool,
    busy: bool,
    sources: QString,
    details: QString,
    context: QString,
    trace: QString,
    templates: QString,
    views: QString,
    store_path: QString,
    screenshot: QString,
    screenshot_step: QString,
    root: PathBuf,
    socket: PathBuf,
    schema: Arc<Mutex<Schema>>,
    /// bumped by every query, so a slow old query cannot overwrite a newer one
    generation: Arc<AtomicU64>,
    tail_stop: Arc<Mutex<Option<Arc<AtomicBool>>>>,
}

impl Default for BackendRust {
    fn default() -> Self {
        let root = std::env::var_os("WARTLOG_STORE").map(PathBuf::from).unwrap_or_else(wartlog_store::default_root);
        BackendRust {
            status: QString::from("Ready"),
            error: QString::default(),
            error_start: -1,
            error_end: -1,
            live: false,
            daemon: false,
            busy: false,
            sources: QString::from("{\"groups\":[],\"configured\":[]}"),
            details: QString::default(),
            context: QString::default(),
            trace: QString::default(),
            templates: QString::default(),
            views: QString::from(&logic::load_views().to_string()),
            store_path: QString::from(&root.display().to_string()),
            screenshot: QString::from(&std::env::var("WARTLOG_SCREENSHOT").unwrap_or_default()),
            screenshot_step: QString::from(&std::env::var("WARTLOG_SCREENSHOT_STEP").unwrap_or_default()),
            socket: std::env::var_os("WARTLOG_SOCKET").map(PathBuf::from).unwrap_or_else(wartlog_api::api_socket_path),
            root,
            schema: Arc::new(Mutex::new(Schema::default())),
            generation: Arc::new(AtomicU64::new(0)),
            tail_stop: Arc::new(Mutex::new(None)),
        }
    }
}

fn snapshot(root: &PathBuf) -> anyhow::Result<Snapshot> {
    if !root.exists() {
        anyhow::bail!("no store at {} yet: start wartlogd to begin collecting", root.display());
    }
    Snapshot::open(root)
}

impl qobject::Backend {
    fn run_query(mut self: Pin<&mut Self>, query: &QString, range: &QString) {
        let (q, range) = (query.to_string(), range.to_string());
        let root = self.root.clone();
        let generation = self.generation.fetch_add(1, Ordering::SeqCst) + 1;
        let gen_now = self.generation.clone();
        self.as_mut().set_busy(true);
        let qt = self.qt_thread();
        rt().spawn_blocking(move || {
            let res = snapshot(&root).and_then(|s| ops::query(&s, &q, logic::range_ns(&range), Some(ROW_LIMIT)).map_err(|e| anyhow::Error::new(e)));
            if gen_now.load(Ordering::SeqCst) != generation {
                return;
            }
            let _ = qt.queue(move |mut o| {
                o.as_mut().set_busy(false);
                match res {
                    Ok((t, st)) => {
                        o.as_mut().set_error(QString::default());
                        o.as_mut().set_error_start(-1);
                        o.as_mut().set_error_end(-1);
                        let more = if st.truncated { format!(" (newest {}; add take or narrow the range for more)", st.rows) } else { String::new() };
                        o.as_mut().set_status(QString::from(&format!(
                            "{} rows in {:.0} ms{more} · {} of {} blocks scanned",
                            st.rows, st.elapsed_ms, st.blocks_scanned, st.blocks
                        )));
                        let json = logic::rows_json(&t, ACCENT).to_string();
                        o.as_mut().rows_ready(QString::from(&json), true);
                    }
                    Err(e) => {
                        let (s, en) = match e.downcast_ref::<wartlog_query::QueryError>() {
                            Some(qe) if qe.end > qe.start => (qe.start as i32, qe.end as i32),
                            _ => (-1, -1),
                        };
                        o.as_mut().set_error(QString::from(&format!("{e:#}")));
                        o.as_mut().set_error_start(s);
                        o.as_mut().set_error_end(en);
                    }
                }
            });
        });
    }

    fn set_tail(mut self: Pin<&mut Self>, on: bool, query: &QString) {
        // stop any running tail
        if let Some(stop) = self.tail_stop.lock().unwrap().take() {
            stop.store(true, Ordering::SeqCst);
        }
        self.as_mut().set_live(on);
        if !on {
            return;
        }
        let stop = Arc::new(AtomicBool::new(false));
        *self.tail_stop.lock().unwrap() = Some(stop.clone());
        let q = query.to_string();
        let socket = self.socket.clone();
        let qt = self.qt_thread();
        rt().spawn(async move {
            let res: anyhow::Result<()> = async {
                let mut c = Client::connect(&socket, "wartlog-viewer").await?;
                c.send(&Request::Tail { kql: q, backfill: 0 }).await?;
                // the first message answers the backfill (empty) or reports an error
                loop {
                    let next = tokio::time::timeout(std::time::Duration::from_millis(250), c.recv()).await;
                    if stop.load(Ordering::SeqCst) {
                        return Ok(());
                    }
                    let m = match next {
                        Err(_) => continue,
                        Ok(m) => m?,
                    };
                    match m {
                        Some(Response::Rows { table }) => {
                            if table.rows.is_empty() {
                                continue;
                            }
                            let json = logic::rows_json(&table, ACCENT).to_string();
                            let _ = qt.queue(move |mut o| o.as_mut().rows_ready(QString::from(&json), false));
                        }
                        Some(Response::Lagged { missed }) => {
                            let _ = qt.queue(move |mut o| o.as_mut().notice(QString::from(&format!("Live tail fell behind; skipped {missed} records"))));
                        }
                        Some(Response::Error { message, .. }) => anyhow::bail!("{message}"),
                        Some(_) => {}
                        None => anyhow::bail!("wartlogd closed the connection"),
                    }
                }
            }
            .await;
            if let Err(e) = res {
                if !stop.load(Ordering::SeqCst) {
                    let msg = format!("Live tail stopped: {e:#}");
                    let _ = qt.queue(move |mut o| {
                        o.as_mut().set_live(false);
                        o.as_mut().notice(QString::from(&msg));
                    });
                }
            }
        });
    }

    fn show_details(self: Pin<&mut Self>, id: f64) {
        let root = self.root.clone();
        let qt = self.qt_thread();
        rt().spawn_blocking(move || {
            let r = snapshot(&root).and_then(|s| s.get(id as u64));
            let _ = qt.queue(move |mut o| match r {
                Ok(Some(rec)) => o.as_mut().set_details(QString::from(&logic::details_json(&rec).to_string())),
                Ok(None) => o.as_mut().notice(QString::from("That record is gone (aged out by retention)")),
                Err(e) => o.as_mut().notice(QString::from(&format!("{e:#}"))),
            });
        });
    }

    fn load_context(self: Pin<&mut Self>, id: f64, scope: &QString) {
        let same = match scope.to_string().as_str() {
            "thread" => ContextScope::Thread,
            "trace" => ContextScope::Trace,
            _ => ContextScope::Source,
        };
        let root = self.root.clone();
        let qt = self.qt_thread();
        rt().spawn_blocking(move || {
            let r = snapshot(&root).and_then(|s| ops::record_context(&s, id as u64, 50, 50, same));
            let _ = qt.queue(move |mut o| match r {
                Ok((t, center)) => {
                    let mut j = logic::rows_json(&t, ACCENT);
                    j["center"] = center.map(|c| c as i64).unwrap_or(-1).into();
                    o.as_mut().set_context(QString::from(&j.to_string()));
                }
                Err(e) => o.as_mut().notice(QString::from(&format!("{e:#}"))),
            });
        });
    }

    fn load_trace(self: Pin<&mut Self>, trace_id: &QString) {
        let t = trace_id.to_string();
        let root = self.root.clone();
        let qt = self.qt_thread();
        rt().spawn_blocking(move || {
            let r = snapshot(&root).and_then(|s| ops::trace_get(&s, &t));
            let _ = qt.queue(move |mut o| match r {
                Ok((spans, logs)) => {
                    let j = serde_json::json!({ "id": t, "spans": logic::spans_json(&spans), "logs": logic::rows_json(&logs, ACCENT) });
                    o.as_mut().set_trace(QString::from(&j.to_string()));
                }
                Err(e) => o.as_mut().notice(QString::from(&format!("{e:#}"))),
            });
        });
    }

    fn load_templates(self: Pin<&mut Self>, query: &QString, range: &QString) {
        let (q, range) = (query.to_string(), range.to_string());
        let root = self.root.clone();
        let qt = self.qt_thread();
        rt().spawn_blocking(move || {
            let r = snapshot(&root).and_then(|s| ops::templates_top(&s, &q, logic::range_ns(&range), 200).map_err(anyhow::Error::new));
            let _ = qt.queue(move |mut o| match r {
                Ok(t) => {
                    let j: Vec<serde_json::Value> = t
                        .iter()
                        .map(|x| serde_json::json!({ "template": x.template, "count": x.count as f64, "spark": x.sparkline, "first": logic::time_label(x.first_ns), "last": logic::time_label(x.last_ns) }))
                        .collect();
                    o.as_mut().set_templates(QString::from(&serde_json::Value::from(j).to_string()));
                }
                Err(e) => o.as_mut().notice(QString::from(&format!("{e:#}"))),
            });
        });
    }

    fn refresh(self: Pin<&mut Self>) {
        let root = self.root.clone();
        let socket = self.socket.clone();
        let schema = self.schema.clone();
        let qt = self.qt_thread();
        rt().spawn(async move {
            let daemon = async {
                let mut c = Client::connect(&socket, "wartlog-viewer").await?;
                match c.call_one(&Request::SourcesList).await? {
                    Response::Sources { configured, .. } => Ok::<_, anyhow::Error>(configured),
                    _ => Ok(vec![]),
                }
            }
            .await;
            let r2 = root.clone();
            let sc = tokio::task::spawn_blocking(move || snapshot(&r2).map(|s| ops::schema(&s, None))).await;
            let sc = match sc {
                Ok(Ok(s)) => s,
                _ => Schema { tables: vec!["logs".into(), "spans".into()], ..Default::default() },
            };
            let up = daemon.is_ok();
            let configured = daemon.unwrap_or_default();
            let json = logic::sources_json(&sc.sources, &configured).to_string();
            *schema.lock().unwrap() = sc;
            let _ = qt.queue(move |mut o| {
                o.as_mut().set_daemon(up);
                o.as_mut().set_sources(QString::from(&json));
            });
        });
    }

    fn complete(&self, text: &QString, cursor: i32) -> QString {
        let s = self.schema.lock().unwrap();
        QString::from(&logic::complete(&s, &text.to_string(), cursor.max(0) as usize).to_string())
    }

    fn add_filter(&self, query: &QString, column: &QString, value_json: &QString, exclude: bool) -> QString {
        let v = match serde_json::from_str::<serde_json::Value>(&value_json.to_string()) {
            Ok(serde_json::Value::Number(n)) if n.is_i64() => wartlog_model::Value::I64(n.as_i64().unwrap()),
            Ok(serde_json::Value::Number(n)) => wartlog_model::Value::F64(n.as_f64().unwrap_or(0.0)),
            Ok(serde_json::Value::Bool(b)) => wartlog_model::Value::Bool(b),
            Ok(serde_json::Value::String(s)) => wartlog_model::Value::Str(s),
            _ => wartlog_model::Value::Str(value_json.to_string()),
        };
        QString::from(&logic::add_filter(&query.to_string(), &column.to_string(), &v, exclude))
    }

    fn only_source(&self, query: &QString, display: &QString, kind: &QString) -> QString {
        QString::from(&logic::only_source(&query.to_string(), &display.to_string(), &kind.to_string()))
    }

    fn save_view(mut self: Pin<&mut Self>, name: &QString, query: &QString, range: &QString) {
        match logic::save_view(&name.to_string(), &query.to_string(), &range.to_string()) {
            Ok(v) => self.as_mut().set_views(QString::from(&v.to_string())),
            Err(e) => self.as_mut().notice(QString::from(&format!("Could not save the view: {e:#}"))),
        }
    }

    fn delete_view(mut self: Pin<&mut Self>, name: &QString) {
        match logic::delete_view(&name.to_string()) {
            Ok(v) => self.as_mut().set_views(QString::from(&v.to_string())),
            Err(e) => self.as_mut().notice(QString::from(&format!("Could not delete the view: {e:#}"))),
        }
    }

    fn start_daemon(self: Pin<&mut Self>) {
        let qt = self.qt_thread();
        rt().spawn(async move {
            let out = tokio::process::Command::new("systemctl").args(["--user", "start", "wartlogd.service"]).output().await;
            let msg = match out {
                Ok(o) if o.status.success() => "Started wartlogd".to_string(),
                Ok(o) => format!("Could not start wartlogd: {} (run `wl setup` once to install the service)", String::from_utf8_lossy(&o.stderr).trim()),
                Err(e) => format!("Could not run systemctl: {e}"),
            };
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            let _ = qt.queue(move |mut o| {
                o.as_mut().notice(QString::from(&msg));
                o.as_mut().refresh();
            });
        });
    }

    fn add_source(self: Pin<&mut Self>, role: &QString, config: &QString) {
        let req = Request::SourcesAdd { kind: role.to_string(), config: config.to_string() };
        self.daemon_call(req, "Source added; wartlogd is starting it");
    }

    fn remove_source(self: Pin<&mut Self>, name: &QString) {
        let req = Request::SourcesRemove { name: name.to_string() };
        self.daemon_call(req, "Source removed");
    }

    fn daemon_call(self: Pin<&mut Self>, req: Request, ok: &'static str) {
        let socket = self.socket.clone();
        let qt = self.qt_thread();
        rt().spawn(async move {
            let r = async {
                let mut c = Client::connect(&socket, "wartlog-viewer").await?;
                c.call_one(&req).await
            }
            .await;
            let msg = match r {
                Ok(_) => ok.to_string(),
                Err(e) => format!("{e:#}"),
            };
            tokio::time::sleep(std::time::Duration::from_millis(400)).await;
            let _ = qt.queue(move |mut o| {
                o.as_mut().notice(QString::from(&msg));
                o.as_mut().refresh();
            });
        });
    }
}

