//! The API server on the UNIX socket (spec section 10).

use crate::ingest::{Cmd, IngestStats};
use crate::manager::Manager;
use anyhow::Result;
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::io::AsyncReadExt;
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{broadcast, oneshot};
use wartlog_api::ops;
use wartlog_api::proto::{batches, read_frame, write_frame, Request, Response, Status, Table};
use wartlog_api::PROTOCOL_VERSION;
use wartlog_model::Record;
use wartlog_store::Snapshot;

const BATCH_ROWS: usize = 2000;

pub struct Shared {
    pub root: PathBuf,
    pub config_path: PathBuf,
    pub config_error: Mutex<Option<String>>,
    pub manager: tokio::sync::Mutex<Manager>,
    pub ingest: Arc<IngestStats>,
    pub cmds: Mutex<std::sync::mpsc::Sender<Cmd>>,
    pub tail: broadcast::Sender<Arc<Record>>,
    pub started: Instant,
    /// set after a config edit so the reload loop picks it up at once
    pub reload: tokio::sync::Notify,
}

pub async fn serve(l: UnixListener, sh: Arc<Shared>, mut shutdown: tokio::sync::watch::Receiver<bool>) {
    loop {
        tokio::select! {
            _ = shutdown.changed() => return,
            a = l.accept() => match a {
                Ok((s, _)) => {
                    let sh = sh.clone();
                    tokio::spawn(async move {
                        if let Err(e) = conn(s, sh).await {
                            tracing::debug!("api connection: {e:#}");
                        }
                    });
                }
                Err(e) => {
                    tracing::warn!("api accept: {e}");
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            }
        }
    }
}

async fn snapshot(root: PathBuf) -> Result<Snapshot> {
    tokio::task::spawn_blocking(move || Snapshot::open(&root)).await?
}

/// Runs blocking work that needs a snapshot.
async fn with_snap<T: Send + 'static>(sh: &Shared, f: impl FnOnce(&Snapshot) -> T + Send + 'static) -> Result<T> {
    let root = sh.root.clone();
    tokio::task::spawn_blocking(move || Snapshot::open(&root).map(|s| f(&s))).await?
}

async fn conn(mut s: UnixStream, sh: Arc<Shared>) -> Result<()> {
    match read_frame::<_, Request>(&mut s).await? {
        Some(Request::Hello { version, .. }) if version == PROTOCOL_VERSION => {
            write_frame(&mut s, &Response::Hello { version: PROTOCOL_VERSION, server: format!("wartlogd {}", env!("CARGO_PKG_VERSION")) }).await?;
        }
        Some(Request::Hello { version, .. }) => {
            write_frame(&mut s, &Response::error(format!("wartlogd speaks API version {PROTOCOL_VERSION}, the client speaks {version}"))).await?;
            return Ok(());
        }
        _ => {
            write_frame(&mut s, &Response::error("the first message must be hello")).await?;
            return Ok(());
        }
    }
    while let Some(req) = read_frame::<_, Request>(&mut s).await? {
        if let Request::Tail { kql, backfill } = req {
            return tail(s, &sh, kql, backfill).await;
        }
        for m in handle(req, &sh).await {
            write_frame(&mut s, &m).await?;
        }
    }
    Ok(())
}

fn err(e: impl std::fmt::Display) -> Vec<Response> {
    vec![Response::error(format!("{e:#}"))]
}

async fn handle(req: Request, sh: &Arc<Shared>) -> Vec<Response> {
    match req {
        Request::Hello { .. } => vec![Response::Hello { version: PROTOCOL_VERSION, server: "wartlogd".into() }],
        Request::Query { kql, time_range, limit } => match with_snap(sh, move |s| ops::query(s, &kql, time_range, limit)).await {
            Ok(Ok((t, stats))) => {
                let mut v: Vec<Response> = batches(t, BATCH_ROWS).into_iter().map(|table| Response::Rows { table }).collect();
                v.push(Response::Done { stats });
                v
            }
            Ok(Err(e)) => vec![Response::query_error(&e)],
            Err(e) => err(e),
        },
        Request::Tail { .. } => err("tail is handled on its own connection"),
        Request::SourcesList => {
            let configured = sh.manager.lock().await.infos();
            let seen = with_snap(sh, |s| ops::schema(s, None).sources).await.unwrap_or_default();
            vec![Response::Sources { configured, seen }]
        }
        Request::SourcesAdd { kind, config } => edit(sh, move |p| crate::config::add_source(p, &kind, &config)).await,
        Request::SourcesRemove { name } => edit(sh, move |p| crate::config::remove_source(p, &name)).await,
        Request::SourcesUpdate { name, config } => edit(sh, move |p| crate::config::update_source(p, &name, &config)).await,
        Request::Schema { time_range } => match with_snap(sh, move |s| ops::schema(s, time_range)).await {
            Ok(schema) => vec![Response::Schema { schema }],
            Err(e) => err(e),
        },
        Request::RecordGet { id } => match with_snap(sh, move |s| s.get(id)).await {
            Ok(Ok(record)) => vec![Response::Record { record }],
            Ok(Err(e)) | Err(e) => err(e),
        },
        Request::RecordContext { id, before, after, same } => match with_snap(sh, move |s| ops::record_context(s, id, before, after, same)).await {
            Ok(Ok((table, center))) => vec![Response::Context { table, center }],
            Ok(Err(e)) | Err(e) => err(e),
        },
        Request::TraceGet { trace_id } => match with_snap(sh, move |s| ops::trace_get(s, &trace_id)).await {
            Ok(Ok((spans, logs))) => vec![Response::Trace { spans, logs }],
            Ok(Err(e)) | Err(e) => err(e),
        },
        Request::TemplatesTop { kql, time_range, limit } => match with_snap(sh, move |s| ops::templates_top(s, &kql, time_range, limit)).await {
            Ok(Ok(templates)) => vec![Response::Templates { templates }],
            Ok(Err(e)) => vec![Response::query_error(&e)],
            Err(e) => err(e),
        },
        Request::Status => match status(sh).await {
            Ok(status) => vec![Response::Status { status }],
            Err(e) => err(e),
        },
        Request::IndexRebuild { segment } => {
            let (tx, rx) = oneshot::channel();
            if sh.cmds.lock().unwrap().send(Cmd::RebuildIndex { segment, reply: tx }).is_err() {
                return err("the writer is not running");
            }
            match rx.await {
                Ok(Ok(n)) => vec![Response::Ok { message: format!("rebuilt {n} term index(es)") }],
                Ok(Err(e)) => err(e),
                Err(_) => err("the writer stopped"),
            }
        }
    }
}

async fn edit(sh: &Arc<Shared>, f: impl FnOnce(&std::path::Path) -> Result<()> + Send + 'static) -> Vec<Response> {
    let p = sh.config_path.clone();
    match tokio::task::spawn_blocking(move || f(&p)).await {
        Ok(Ok(())) => {
            sh.reload.notify_one();
            vec![Response::Ok { message: format!("saved {}", sh.config_path.display()) }]
        }
        Ok(Err(e)) => err(e),
        Err(e) => err(e),
    }
}

pub async fn status(sh: &Arc<Shared>) -> Result<Status> {
    let snap = snapshot(sh.root.clone()).await?;
    let st = &sh.ingest;
    let mut warnings = Vec::new();
    for (p, e) in &snap.errors {
        warnings.push(format!("{}: {e}", p.display()));
    }
    if let Some(e) = st.last_error.lock().unwrap().clone() {
        warnings.push(format!("writer: {e}"));
    }
    let indexed = snap.segments.iter().filter(|s| s.tix.is_some()).count();
    let sources = sh.manager.lock().await.infos();
    Ok(Status {
        version: env!("CARGO_PKG_VERSION").into(),
        pid: std::process::id(),
        uptime_s: sh.started.elapsed().as_secs(),
        store_root: sh.root.display().to_string(),
        disk_bytes: snap.disk_bytes(),
        records: snap.record_count(),
        segments: snap.segments.len(),
        blocks: snap.block_count(),
        wal_records: snap.wal.len() as u64,
        last_seq: st.last_seq.load(Ordering::Relaxed),
        durable_seq: st.durable_seq.load(Ordering::Relaxed),
        ingest_rate: st.rate(),
        segments_indexed: indexed,
        segments_unindexed: snap.segments.len() - indexed,
        sources,
        config_path: sh.config_path.display().to_string(),
        config_error: sh.config_error.lock().unwrap().clone(),
        store_origin: wartlog_store::Manifest::origin_of(&sh.root).map(|o| o.name().to_string()).unwrap_or_else(|e| format!("unreadable: {e:#}")),
        segment_writers: {
            let mut m: std::collections::BTreeMap<String, usize> = Default::default();
            for h in &snap.segments {
                *m.entry(h.header.writer.name()).or_default() += 1;
            }
            m.into_iter().collect()
        },
        warnings,
    })
}

/// Live tail: backfill, then matching new records in batches every 100 ms, until the client hangs up.
async fn tail(mut s: UnixStream, sh: &Arc<Shared>, kql: String, backfill: u64) -> Result<()> {
    // subscribe before the backfill so nothing falls between them
    let mut rx = sh.tail.subscribe();
    let kql = if kql.trim().is_empty() { "logs".to_string() } else { kql };
    let k2 = kql.clone();
    let prep = with_snap(sh, move |snap| -> Result<(wartlog_query::TailFilter, Table), wartlog_query::QueryError> {
        let f = wartlog_query::TailFilter::compile(&k2, snap)?;
        let mut t = Table::default();
        if backfill > 0 {
            let q = format!("{k2} | sort by Timestamp desc | take {backfill}");
            t = ops::query(snap, &q, None, None)?.0;
            t.rows.reverse();
            t.ids.reverse();
        }
        Ok((f, t))
    })
    .await?;
    let (filter, back) = match prep {
        Ok(x) => x,
        Err(e) => {
            write_frame(&mut s, &Response::query_error(&e)).await?;
            return Ok(());
        }
    };
    let after = back.ids.iter().flatten().max().copied().unwrap_or(0);
    let mut columns = back.columns.clone();
    write_frame(&mut s, &Response::Rows { table: back }).await?;
    let (mut rd, mut wr) = s.into_split();
    let mut pending = Table { columns: columns.clone(), ..Default::default() };
    let mut tick = tokio::time::interval(Duration::from_millis(100));
    let mut probe = [0u8; 1];
    loop {
        tokio::select! {
            r = rx.recv() => match r {
                Ok(rec) => {
                    if rec.seq <= after {
                        continue;
                    }
                    if let Some((cols, vals)) = filter.apply(&rec) {
                        if pending.columns != cols {
                            if !pending.rows.is_empty() {
                                write_frame(&mut wr, &Response::Rows { table: std::mem::take(&mut pending) }).await?;
                            }
                            columns = cols;
                            pending.columns = columns.clone();
                        }
                        pending.rows.push(vals);
                        pending.ids.push(Some(rec.seq));
                        if pending.rows.len() >= BATCH_ROWS {
                            write_frame(&mut wr, &Response::Rows { table: std::mem::replace(&mut pending, Table { columns: columns.clone(), ..Default::default() }) }).await?;
                        }
                    }
                }
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    write_frame(&mut wr, &Response::Lagged { missed: n }).await?;
                }
                Err(broadcast::error::RecvError::Closed) => return Ok(()),
            },
            _ = tick.tick() => {
                if !pending.rows.is_empty() {
                    write_frame(&mut wr, &Response::Rows { table: std::mem::replace(&mut pending, Table { columns: columns.clone(), ..Default::default() }) }).await?;
                }
            }
            // the client sends nothing during a tail; a read returning means it hung up
            _ = rd.read(&mut probe) => return Ok(()),
        }
    }
}
