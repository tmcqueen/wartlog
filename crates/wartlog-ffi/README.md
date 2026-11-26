# wartlog-ffi: the WartLog C ABI

`libwartlog.so` (and `libwartlog.a`) read and write WartLog stores from C, C#, Python, Go or
anything else with a C FFI. It wraps the [`wartlog`](../wartlog) crate; Rust programs should use
that crate directly instead. The header is [`include/wartlog.h`](include/wartlog.h).

```sh
cargo build --release -p wartlog-ffi        # target/release/libwartlog.so
cc app.c -Icrates/wartlog-ffi/include -Ltarget/release -lwartlog
```

## Rules

- Every handle is opaque. Records go in through a builder (`wl_record_new`, `wl_record_set_*`,
  `wl_writer_push`), rows come out through a cursor (`wl_query`, `wl_cursor_next`, `wl_row_get_*`).
- Every function returns a status: 0 (`WL_OK`), a positive answer where documented (`wl_cursor_next`
  and `wl_record_get` return 1 for "yes"), or a negative `WL_E_*` with the message in
  `wl_last_error()` (thread-local, valid until the next call on that thread). Rust panics are
  caught and become `WL_E_PANIC`.
- Strings passed in are UTF-8. Paths and KQL are NUL-terminated; keys, values and bodies are
  pointer plus length. Strings returned are owned by the library and stay valid until the owning
  handle is freed or the next call on it, except where a function says to free them with
  `wl_string_free`.
- A snapshot may be shared by threads for reading. A writer, a cursor and a builder belong to one
  thread at a time.
- Option structs (`wl_query_opts`, `wl_writer_opts`) are zero-initialized with `size` set to their
  `sizeof`, so later versions can add fields. NULL means defaults.
- `wl_abi_version()` only grows; new versions only add functions and enum values.

## Writer modes

| Mode | Store | Refused with |
|---|---|---|
| `WL_WRITER_SHARED` | the store the running wartlogd serves (each push is an `Ingest` request) | `WL_E_NO_DAEMON` |
| `WL_WRITER_PRIVATE` | a store the library owns, by convention `<name>.wartlogx/`, created if absent | `WL_E_DAEMON_STORE` on a daemon store |
| `WL_WRITER_EXCLUSIVE` | a daemon store while wartlogd is stopped (takes the lock) | `WL_E_LOCKED`, `WL_E_NOT_FOUND` |

Private and exclusive writers run their own maintenance thread (WAL fsync every 250 ms, block
sealing, segment rollover, optional retention). `wl_writer_flush` blocks until everything pushed is
durable. `wl_writer_close` seals everything, so a clean close leaves no WAL to recover. There is
no mode that takes the lock "when the daemon happens to be stopped"; see the design spec for why.

## Inside a host process

- No async runtime is started. The daemon client uses blocking sockets.
- Queries use 2 threads unless `wl_query_opts.threads` says otherwise, and plain row results are
  capped at 10 000 rows unless `limit` says otherwise.
- Segments that may still grow are read with `pread`, not `mmap`, so crash recovery truncating a
  file cannot raise SIGBUS in the host.

## Known gaps

- Query results are produced before the first row is returned (aggregations already stream
  internally). The cursor API does not change when this is lifted.
- Spans cannot be written yet (`wl_span_new` comes later); logs only.

## Tests

`cargo test -p wartlog-ffi` builds the library, compiles `tests/c/smoke.c` with `-Wall -Werror`
against the header and runs it, and checks the committed header matches `cbindgen` output (when
cbindgen is installed). Regenerate the header with:

```sh
cd crates/wartlog-ffi && cbindgen --config cbindgen.toml --crate wartlog-ffi --output include/wartlog.h
```
