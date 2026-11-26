# WartLog

A local log and trace store for one developer machine. A small daemon (`wartlogd`) collects
journald, OpenTelemetry (gRPC and HTTP), NDJSON or plain text from pipes and sockets, and
Kubernetes pod logs into one compact store. You search it with a subset of KQL from the `wl` CLI
or the Qt desktop viewer. Both read the store files directly, so history works even when the
daemon is stopped.

Status: MVP. See [the design](docs/superpowers/specs/2026-11-25-wartlog-mvp-design.md) and
[the on-disk format](docs/format.md). New to terms like WAL, segment or term index? The
[glossary](docs/glossary.md) explains them in plain language.

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

Measured with the shipped `wartlogd` and `wl` (release build) on one desktop: a 10-core,
20-thread Xeon E5-2660 v3 with 125 GB of RAM and a SATA SSD, warm page cache. Data goes in through
the daemon's own paths: `wl send` into the NDJSON receiver, and the journald collector. Every
WartLog time is a fresh `wl query --json` process printing each matching row, median of 5; an
empty result costs about 14 ms (30 ms on the big store) for process start and opening the store.
The DuckDB and journald runs returned identical rows from both tools (compared by event id and
timestamp); the SQLite runs returned the same row counts.

**Against SQLite** (2 million synthetic journal entries). SQLite has B-tree indexes on time, unit
and severity, plus FTS5 on the message. Its times are in-process with no startup cost.

| | SQLite + FTS5 | WartLog |
|---|---|---|
| On disk | 1.24 GB | 103 MB |
| Load | 43 s (insert, then indexes in bulk) | 83 s (24k records/s through `wl send`) |
| Rare term | <0.05 ms | 14 ms |
| Substring (`LIKE` / `contains`) | 859 ms | 157 ms |
| Unit + time window (1,952 rows) | 11 ms | 89 ms |
| Unit, whole log (20k rows) | 119 ms | 757 ms |
| Errors and worse (40k rows) | 792 ms | 1.59 s |
| Everything in a time window (200k rows) | 755 ms | 8.35 s |

WartLog is 12x smaller and wins substring search. SQLite wins every indexed lookup, and the gap
grows with the number of rows returned: WartLog spends about 25 to 40 µs per printed row.

**Against DuckDB on Parquet** (117 million ride events from a real theme park dataset). Both sides
are fresh processes printing every row as JSON.

| | DuckDB 2.0.1 | WartLog |
|---|---|---|
| On disk | 858 MB | 2.03 GB |
| Load | none (queries the files) | 30 min (65k records/s through `wl send`) |
| One guest's rides (34 rows) | 219 ms | 37 ms |
| One attraction, one day (286 rows) | 139 ms | 38 ms |
| One attraction, one year (109k rows) | 347 ms | 2.99 s |
| One attraction, all years (792k rows) | 1.40 s | 21.2 s |

DuckDB has no index for a random id, so point lookups favor WartLog by 4 to 6x. Anything that
returns many rows favors DuckDB by 9 to 15x, and Parquet is 2.4x smaller.

**Against journald** (a real desktop user journal, 2.14 million entries; `journalctl` is a fresh
process printing JSON). The collector read the whole journal in 171 s.

| | journald | WartLog |
|---|---|---|
| On disk | 962 MB | 58 MB |
| Rare term (`-g` / `has`, 6 rows) | 7.63 s | 15 ms |
| Substring (`-g` / `contains`) | 7.72 s | 164 ms |
| Unit + time window (9,144 rows) | 707 ms | 422 ms |
| Unit, whole journal (22k rows) | 1.80 s | 1.27 s |
| Errors and worse (`-p 3`, 6,172 rows) | 549 ms | 395 ms |
| Everything in a time window (214k rows) | 18.9 s | 11.8 s |

Known problems these runs found: `logs | count` holds every record in memory (it reached 120 GB on
the 117M store), and wartlogd's resident memory grew to 15 GB during the 117M load.

## Workspace

| Crate | What it is |
|---|---|
| `wartlog-model` | record, span, source and value types; message template rendering |
| `wartlog-store` | write-ahead log (WAL), segments, blocks, term index, catalog, retention, read-only snapshots |
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
