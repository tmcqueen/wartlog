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
