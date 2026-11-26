# WartLog

A local log and trace store for one developer machine. A small daemon (`wartlogd`) collects
journald, OpenTelemetry (gRPC and HTTP), NDJSON or plain text from pipes and sockets, and
Kubernetes pod logs into one compact store. You search it with a subset of KQL from the `wl` CLI
or the Qt desktop viewer. Both read the store files directly, so history works even when the
daemon is stopped.

Status: MVP. See [the design](docs/superpowers/specs/2026-11-25-wartlog-mvp-design.md) and
[the on-disk format](docs/format.md).

## Build

Rust 1.93.1 or newer. Linux only for now (journald, systemd, flock).

```sh
cargo build --release          # wartlogd and wl
# journald support loads libsystemd at run time, so no -dev package is needed to build.
# Leave out a source kind with, for example:
#   cargo build --release -p wartlogd --no-default-features --features otlp,journald
```

The viewer needs Qt 6 and builds separately; see [crates/wartlog-viewer](crates/wartlog-viewer/README.md).

## Run

```sh
wl setup                       # writes ~/.config/wartlog/wartlog.toml, installs and starts the
                               # systemd user service, and adds a .desktop entry for the viewer
wl status
wl query 'logs | where Level == "error"' --since 1h
wl tail 'logs | where Body has "timeout"'
my-app 2>&1 | wl send --source my-app
wl templates 'logs | where Source == "orders"' --since 1d
```

Or run the daemon in the foreground: `wartlogd --config wartlog.toml`. It reloads the config file
when it changes. `wartlogd --print-default-config` shows every option with comments.

Point an Aspire or other OpenTelemetry app at it with
`OTEL_EXPORTER_OTLP_ENDPOINT=http://127.0.0.1:4317`. The OTLP receiver can forward everything on to
the Aspire dashboard (`forward = [...]` in the config).

## How it compares

These numbers come from the storage spike (`spikes/storage`, full write-up in
[FINDINGS.md](spikes/storage/FINDINGS.md)), which prototyped the design the store now uses: per-source
blocks, a term index per segment, no Bloom filters. They have not been re-measured with the shipped
`wartlogd` and `wl` yet. Each comparison returned the same rows from both tools.

**Against SQLite** (2 million synthetic journal entries, 4 CPUs, in-process, median of 5). SQLite has
B-tree indexes on time, unit and severity, plus FTS5 on the message.

| | SQLite + FTS5 | WartLog |
|---|---|---|
| On disk | 1.24 GB | 74 MB |
| Rare term | <0.05 ms | 0.9 ms |
| Unit + time window (1,952 rows) | 5.7 ms | 3.3 ms |
| Unit, whole log (20k rows) | 61 ms | 20 ms |
| Errors and worse (40k rows) | 327 ms | 96 ms |
| Everything in a time window (200k rows) | 373 ms | 180 ms |
| Substring (`LIKE` / `contains`) | 420 ms | 81 ms |

SQLite still wins exact term lookups, by under a millisecond. WartLog's broad scans lean on
parallelism; on one thread it roughly ties SQLite on unit lookups and loses the large scans.

**Against DuckDB on Parquet** (117 million events from a real dataset, 4 CPUs, fresh processes
printing every row, median of 5)

| | DuckDB 2.0.1 | WartLog |
|---|---|---|
| On disk | 858 MB | 1.45 GB |
| One guest's events (34 rows) | 288 ms | 11 ms |
| One attraction, one day | 96 ms | 9 ms |
| One attraction, all years (792k rows) | 4.18 s | 1.49 s |
| Broad scans across a whole year | faster by about 1.5x | |

DuckDB has no index for a random id, so point lookups favor WartLog by 7 to 25x; columnar
execution wins whole-table scans, and Parquet is still 1.7x smaller.

**Against journald** (a real desktop user journal, 65k entries): 71 MB on disk for journald vs 2.5 MB for
WartLog, and a rare-term search in 10 ms vs 239 ms for `journalctl -g`. At this size both tools
answer field lookups instantly.

## Workspace

| Crate | What it is |
|---|---|
| `wartlog-model` | record, span, source and value types; message template rendering |
| `wartlog-store` | WAL, segments, blocks, term index, catalog, retention, read-only snapshots |
| `wartlog-query` | the KQL subset: parser, planner (pruning by time, source, terms, trace id) and executor |
| `wartlog-sources` | receivers and collectors |
| `wartlog-api` | the daemon API: CBOR messages over a Unix socket, and shared query operations |
| `wartlogd` | the daemon |
| `wartlog-cli` | `wl` |
| `wartlog-viewer` | the desktop viewer (Qt Quick, cxx-qt), not a workspace member |

## Test

```sh
cargo test --workspace
cargo clippy --workspace --all-targets
```

`crates/wartlogd/tests/crash.rs` kills the real daemon with SIGKILL during ingest several times
and checks recovery.
