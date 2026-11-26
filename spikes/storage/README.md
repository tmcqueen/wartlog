# Storage spike (throwaway)

A quick, throwaway proof of the storage design in
[`docs/superpowers/specs/2026-11-25-wartlog-mvp-design.md`](../../docs/superpowers/specs/2026-11-25-wartlog-mvp-design.md), section 7.
It is **not** the real `wartlog-store` crate and has minimal tests. Nothing here is meant to be kept as-is.

It checks four things:

1. The binary format stores and returns data (segments, checksummed zstd blocks, per-block string tables, 72-byte fixed record headers).
2. Block summaries and the `.widx` block index prune queries as planned.
3. Indexes rebuild from the segments alone, byte-identical.
4. Queries work with no index files (reading summaries from segments), and with no pruning at all.

Then `bench.sh` compares it against `journalctl` on your user journal.

```sh
./bench.sh                 # full run, prints a Markdown report
SCALE=1 ./bench.sh         # skip the 10x scale test
COLD=1 ./bench.sh          # add cold-cache runs (needs passwordless sudo)
```

Left out on purpose: KQL (queries are flags), the WAL, traces, templates, the daemon.
