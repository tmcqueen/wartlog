//! `wl`: query, tail and feed WartLog from a shell (spec section 13).
//!
//! History queries run in-process against the store files (no daemon needed); `tail`, `sources`,
//! `status` and `index rebuild` talk to wartlogd.

mod output;
mod setup;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use std::io::{BufRead, IsTerminal, Write};
use std::path::PathBuf;
use wartlog_api::proto::{ContextScope, Request, Response, TimeRange};
use wartlog_api::{ops, Client};
use wartlog_model::time::{now_ns, parse_datetime, parse_duration};
use wartlog_store::Snapshot;

#[derive(Parser)]
#[command(name = "wl", version, about = "WartLog: query, tail and feed your local logs", after_help = EXAMPLES)]
struct Cli {
    /// store directory (default: $XDG_DATA_HOME/wartlog)
    #[arg(long, global = true, env = "WARTLOG_STORE")]
    store: Option<PathBuf>,
    /// daemon API socket (default: $XDG_RUNTIME_DIR/wartlog/api.sock)
    #[arg(long, global = true, env = "WARTLOG_SOCKET")]
    socket: Option<PathBuf>,
    #[command(subcommand)]
    cmd: Cmd,
}

const EXAMPLES: &str = r#"Examples:
  wl query 'logs | where Unit == "foo.service" and Level == "error"' --since 1h
  wl query 'logs | summarize count() by Source | sort by count_ desc'
  wl tail 'logs | where Body has "timeout"'
  my-app 2>&1 | wl send --source my-app
  wl templates 'logs | where Source == "orders"' --since 1d"#;

#[derive(clap::Args, Clone)]
struct Range {
    /// only records newer than this: a duration (15m, 2h, 7d) or a date/time
    #[arg(long)]
    since: Option<String>,
    /// only records older than this: a duration ago or a date/time
    #[arg(long)]
    until: Option<String>,
}

impl Range {
    fn get(&self) -> Result<Option<TimeRange>> {
        let at = |s: &str| -> Result<i64> {
            if let Some(d) = parse_duration(s) {
                return Ok(now_ns() - d);
            }
            parse_datetime(s).ok_or_else(|| anyhow::anyhow!("`{s}` is not a duration (15m, 2h) or a date/time (2026-11-26 10:00)"))
        };
        let lo = self.since.as_deref().map(at).transpose()?;
        let hi = self.until.as_deref().map(at).transpose()?;
        Ok((lo.is_some() || hi.is_some()).then_some((lo, hi)))
    }
}

#[derive(Subcommand)]
enum Cmd {
    /// Run a KQL query against the store (works without the daemon)
    #[command(alias = "q")]
    Query {
        kql: String,
        #[command(flatten)]
        range: Range,
        /// row cap when the query has no take (default 1000)
        #[arg(long, short = 'n')]
        limit: Option<u64>,
        /// one JSON object per row
        #[arg(long)]
        json: bool,
        /// print planning and scan statistics to stderr
        #[arg(long)]
        stats: bool,
    },
    /// Follow new records that match a query (needs wartlogd)
    #[command(alias = "f")]
    Tail {
        #[arg(default_value = "logs")]
        kql: String,
        /// show this many recent matches first
        #[arg(long, short = 'n', default_value_t = 20)]
        backfill: u64,
        #[arg(long)]
        json: bool,
    },
    /// Send lines from stdin (NDJSON or plain text) to wartlogd
    Send {
        /// source name for lines that do not name one
        #[arg(long, short = 's')]
        source: String,
        /// also copy stdin to stdout, like tee
        #[arg(long)]
        tee: bool,
        /// NDJSON socket (default: $XDG_RUNTIME_DIR/wartlog/ingest.sock)
        #[arg(long)]
        ingest_socket: Option<PathBuf>,
    },
    /// List configured sources and sources seen in the data
    Sources {
        #[arg(long)]
        json: bool,
    },
    /// Daemon and store status
    Status {
        #[arg(long)]
        json: bool,
    },
    /// Group a query's results by message template, with counts
    Templates {
        #[arg(default_value = "logs")]
        kql: String,
        #[command(flatten)]
        range: Range,
        #[arg(long, short = 'n', default_value_t = 25)]
        limit: u32,
    },
    /// Show what the same source, thread or trace did around a record
    Context {
        id: u64,
        #[arg(long, short = 'B', default_value_t = 10)]
        before: u32,
        #[arg(long, short = 'A', default_value_t = 10)]
        after: u32,
        /// source, thread or trace
        #[arg(long, default_value = "source")]
        same: String,
    },
    /// Show one record with every field
    Show {
        id: u64,
    },
    /// Show a trace: its spans and logs
    Trace {
        trace_id: String,
    },
    /// Term index maintenance
    Index {
        #[command(subcommand)]
        cmd: IndexCmd,
    },
    /// Install and start the systemd user service, and write the default config if there is none
    Setup {
        /// path of the wartlogd binary (default: next to wl, else on PATH)
        #[arg(long)]
        daemon: Option<PathBuf>,
        /// write the files but do not enable or start the service
        #[arg(long)]
        no_start: bool,
    },
}

#[derive(Subcommand)]
enum IndexCmd {
    /// Rebuild term indexes (through the daemon if it runs, else directly)
    Rebuild {
        #[arg(long)]
        segment: Option<String>,
    },
    /// Check every term index against its segment
    Verify,
}

fn store_root(cli: &Cli) -> PathBuf {
    cli.store.clone().unwrap_or_else(wartlog_store::default_root)
}

fn open_snapshot(cli: &Cli) -> Result<Snapshot> {
    let root = store_root(cli);
    if !root.exists() {
        bail!("no store at {} yet (start wartlogd, or pass --store)", root.display());
    }
    Snapshot::open(&root).with_context(|| format!("opening {}", root.display()))
}

async fn client(cli: &Cli) -> Result<Client> {
    let p = cli.socket.clone().unwrap_or_else(wartlog_api::api_socket_path);
    Client::connect(&p, "wl").await
}

fn print_query_error(kql: &str, e: &wartlog_query::QueryError) -> anyhow::Error {
    if e.end > e.start && e.end <= kql.len() {
        eprintln!("  {kql}");
        eprintln!("  {}{}", " ".repeat(kql[..e.start].chars().count()), "^".repeat(kql[e.start..e.end].chars().count().max(1)));
    }
    anyhow::anyhow!("{}", e.message)
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    if let Err(e) = run(cli).await {
        // `wl query ... | head` closes the pipe early; that is not an error
        if e.chain().any(|c| c.downcast_ref::<std::io::Error>().is_some_and(|io| io.kind() == std::io::ErrorKind::BrokenPipe)) {
            std::process::exit(0);
        }
        eprintln!("wl: {e:#}");
        std::process::exit(1);
    }
}

async fn run(cli: Cli) -> Result<()> {
    let color = std::io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none();
    match &cli.cmd {
        Cmd::Query { kql, range, limit, json, stats } => {
            let snap = open_snapshot(&cli)?;
            let (t, st) = ops::query(&snap, kql, range.get()?, Some(limit.unwrap_or(1000))).map_err(|e| print_query_error(kql, &e))?;
            output::table(&t, *json, color, output::display_oldest_first(kql))?;
            if *stats {
                eprintln!(
                    "{} rows in {:.1} ms; blocks: {} total, {} pruned, {} scanned ({} via term index); {} records scanned, {} from the WAL{}",
                    st.rows,
                    st.elapsed_ms,
                    st.blocks,
                    st.blocks_pruned,
                    st.blocks_scanned,
                    st.used_term_index,
                    st.records_scanned,
                    st.wal_records,
                    if st.truncated { "; truncated (raise --limit or add take)" } else { "" }
                );
            } else if st.truncated && std::io::stderr().is_terminal() {
                eprintln!("(showing the newest {} rows; raise --limit or add `| take N`)", st.rows);
            }
            Ok(())
        }
        Cmd::Tail { kql, backfill, json } => {
            let mut c = client(&cli).await?;
            c.send(&Request::Tail { kql: kql.clone(), backfill: *backfill }).await?;
            let mut out = std::io::stdout().lock();
            loop {
                match c.recv().await? {
                    Some(Response::Rows { table }) => {
                        output::rows(&mut out, &table, *json, color)?;
                        out.flush()?;
                    }
                    Some(Response::Lagged { missed }) => eprintln!("wl: fell behind, skipped {missed} records"),
                    Some(Response::Error { message, start, end }) => {
                        let e = wartlog_query::QueryError { message, start: start.unwrap_or(0), end: end.unwrap_or(0) };
                        return Err(print_query_error(kql, &e));
                    }
                    Some(other) => bail!("unexpected reply {other:?}"),
                    None => bail!("wartlogd closed the connection"),
                }
            }
        }
        Cmd::Send { source, tee, ingest_socket } => send(source, *tee, ingest_socket.clone().unwrap_or_else(wartlog_api::ingest_socket_path)).await,
        Cmd::Sources { json } => {
            match client(&cli).await {
                Ok(mut c) => match c.call_one(&Request::SourcesList).await? {
                    Response::Sources { configured, seen } => output::sources(&configured, &seen, *json),
                    other => bail!("unexpected reply {other:?}"),
                },
                Err(e) => {
                    // without the daemon, list what the data holds
                    eprintln!("wl: {e:#}; listing sources seen in the store only");
                    let snap = open_snapshot(&cli)?;
                    output::sources(&[], &ops::schema(&snap, None).sources, *json)
                }
            }
        }
        Cmd::Status { json } => {
            let mut c = client(&cli).await?;
            match c.call_one(&Request::Status).await? {
                Response::Status { status } => output::status(&status, *json),
                other => bail!("unexpected reply {other:?}"),
            }
        }
        Cmd::Templates { kql, range, limit } => {
            let snap = open_snapshot(&cli)?;
            let t = ops::templates_top(&snap, kql, range.get()?, *limit).map_err(|e| print_query_error(kql, &e))?;
            output::templates(&t);
            Ok(())
        }
        Cmd::Context { id, before, after, same } => {
            let same = match same.as_str() {
                "source" => ContextScope::Source,
                "thread" => ContextScope::Thread,
                "trace" => ContextScope::Trace,
                s => bail!("--same must be source, thread or trace, not `{s}`"),
            };
            let snap = open_snapshot(&cli)?;
            let (t, center) = ops::record_context(&snap, *id, *before, *after, same)?;
            output::context(&t, center, color)
        }
        Cmd::Show { id } => {
            let snap = open_snapshot(&cli)?;
            match snap.get(*id)? {
                Some(r) => output::record(&r),
                None => bail!("no record with id {id} (it may have aged out)"),
            }
        }
        Cmd::Trace { trace_id } => {
            let snap = open_snapshot(&cli)?;
            let (spans, logs) = ops::trace_get(&snap, trace_id)?;
            output::trace(&spans, &logs, color)
        }
        Cmd::Index { cmd: IndexCmd::Rebuild { segment } } => {
            match client(&cli).await {
                Ok(mut c) => match c.call_one(&Request::IndexRebuild { segment: segment.clone() }).await? {
                    Response::Ok { message } => println!("{message}"),
                    other => bail!("unexpected reply {other:?}"),
                },
                Err(_) => {
                    let root = store_root(&cli);
                    let n = tokio::task::spawn_blocking(move || wartlog_store::writer::rebuild_indexes(&root)).await??;
                    println!("rebuilt {n} term index(es)");
                }
            }
            Ok(())
        }
        Cmd::Index { cmd: IndexCmd::Verify } => {
            let root = store_root(&cli);
            let mut bad = 0;
            let segs = wartlog_store::list_segments(&root);
            for p in &segs {
                let name = p.file_name().unwrap_or_default().to_string_lossy().to_string();
                match wartlog_store::tix::verify(p) {
                    Ok(true) => println!("ok       {name}"),
                    Ok(false) => {
                        bad += 1;
                        println!("STALE    {name}");
                    }
                    Err(e) => {
                        bad += 1;
                        println!("MISSING  {name}  ({e:#})");
                    }
                }
            }
            if bad > 0 {
                bail!("{bad} of {} index(es) need a rebuild: wl index rebuild", segs.len());
            }
            Ok(())
        }
        Cmd::Setup { daemon, no_start } => setup::setup(daemon.clone(), *no_start),
    }
}

/// Streams stdin to the NDJSON socket, naming the source in the preamble.
async fn send(source: &str, tee: bool, path: PathBuf) -> Result<()> {
    use tokio::io::AsyncWriteExt;
    let mut s = tokio::net::UnixStream::connect(&path).await.with_context(|| format!("cannot reach wartlogd at {} (is it running?)", path.display()))?;
    s.write_all(format!("{}{}\n", wartlog_sources_preamble(), source).as_bytes()).await?;
    let (tx, mut rx) = tokio::sync::mpsc::channel::<Vec<u8>>(64);
    // stdin is read on a plain thread: it blocks, and line-at-a-time matters for `tail -f | wl send`
    std::thread::spawn(move || {
        let stdin = std::io::stdin();
        let mut lock = stdin.lock();
        let mut out = std::io::stdout();
        let mut buf = Vec::new();
        loop {
            buf.clear();
            match lock.read_until(b'\n', &mut buf) {
                Ok(0) | Err(_) => break,
                Ok(_) => {
                    if tee {
                        let _ = out.write_all(&buf);
                        let _ = out.flush();
                    }
                    if !buf.ends_with(b"\n") {
                        buf.push(b'\n');
                    }
                    if tx.blocking_send(buf.clone()).is_err() {
                        break;
                    }
                }
            }
        }
    });
    while let Some(l) = rx.recv().await {
        s.write_all(&l).await.context("wartlogd went away")?;
    }
    s.shutdown().await?;
    Ok(())
}

/// Kept in sync with `wartlog_sources::listeners::PREAMBLE` (wl does not link the sources crate).
fn wartlog_sources_preamble() -> &'static str {
    "@wartlog source="
}
