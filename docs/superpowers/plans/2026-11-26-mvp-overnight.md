# WartLog MVP overnight build plan (2026-11-26)

Tim asked (00:34 CST) for the whole MVP overnight: "If you run into any questions, just think 'What would Tim do?' and do that."
Spec: `docs/superpowers/specs/2026-11-25-wartlog-mvp-design.md`. Spike lessons: `spikes/storage/FINDINGS.md`.

## Ground rules

- Branch `claude/project-thread-9tx7n0`, commit after each working step, push often.
- MSRV 1.93.1 (Tim's machine). Build and test with `cargo +1.93.1`. `rust-version = "1.93"` in the workspace.
- No `protoc` on Tim's machine: OTLP types come from `opentelemetry-proto` (pre-generated code) or a vendored protoc.
- Tim's machine: Qt 6.10 + Kirigami, libsystemd 259. Cloud: Qt 6.4, no Kirigami 6. The viewer must build on Tim's machine; verify there through Remote Control (verification only, no commits there).
- Real, tested increments beat a pile of stubs. Anything not done gets listed honestly in the morning report.

## Design changes from the spike (fold into the spec)

- Per-source blocks: the writer keeps one open block per source; seal on size (256 KiB), wall-clock age (5 s, live) and segment close. Never by event-time span.
- Term index `.wtix` per segment (FST of `t:` body terms and `f:key\0value` field terms, block postings), mmap'd, two-pass bounded-memory build. Replaces the Bloom filter for pruning. Bloom filters dropped from the MVP format.
- Block summary keeps time range, seq range, severity bitmap, sources, key hashes (no Bloom).
- `.widx` dropped: the `.wtix` block table holds everything planning needs. Readers fall back to scanning block headers + summaries when it's missing.

## Milestones (build order)

1. Workspace + `wartlog-model` + `wartlog-store` (format, WAL, writer, reader, term index, rebuild, lock, retention) with tests.
2. `wartlog-query`: KQL subset lexer/parser/planner/executor with tests.
3. `wartlog-sources`: NDJSON normalization + unix/tcp/http receivers, journald collector (libsystemd), OTLP gRPC/HTTP (logs + traces, metrics dropped), Kubernetes collector.
4. `wartlog-api` + `wartlogd` (config, wiring, API server, live tail bus, sd_notify) + `wl` CLI (query, tail, send, sources, status, index, setup).
5. `wartlog-viewer` (cxx-qt + QML), verified on Tim's machine.
6. Hardening: corruption/crash tests, reader/writer stress, benchmarks; spec update; PR.
