# WartLog for .NET

A thin wrapper over the WartLog C ABI (`libwartlog.so`, [crates/wartlog-ffi](../../crates/wartlog-ffi)):
`[LibraryImport]` declarations, UTF-8 marshalling, a `SafeHandle` per native handle, and status
codes turned into `WartLogException` with an `ErrorCode`. Linux x64, .NET 8.

```csharp
using WartLog;

// read any store, with or without wartlogd running
using var snap = Snapshot.Open(WartLogLibrary.DefaultStore);
using var rows = snap.Query("logs | where Level == 'error' | take 20 | project Timestamp, Source, Body");
foreach (var row in rows)
    Console.WriteLine($"{row["Timestamp"]} {row["Source"]} {row["Body"]}");

// write a store this program owns
using var w = Writer.Open("myapp.wartlogx", WriterMode.Private);
var id = w.Push(new LogRecord
{
    Source = LogSource.Service("myapp"),
    Severity = 17,
    Template = "Payment {PaymentId} failed after {Ms} ms",
    Attributes = new Dictionary<string, object?> { ["PaymentId"] = "p-42", ["Ms"] = 812 },
});
w.Flush();   // durable
w.Close();   // sealed: no WAL left to recover
```

Writer modes (see the [C ABI README](../../crates/wartlog-ffi/README.md)):

- `WriterMode.Shared` writes through the running wartlogd into its store. Each `Push` is one
  round trip and returns the record id.
- `WriterMode.Private` writes a store the program owns. wartlogd refuses such a store unless it
  is adopted (`wl store adopt`).
- `WriterMode.Exclusive` writes the daemon's store while wartlogd is stopped.

Mismatches are refused with `ErrorCode.DaemonStore`, `NoDaemon`, `Locked` or `NotFound`, never
warned about.

Rows are copied out of the library as they are read (`string`, `long`, `double`, `bool`,
`DateTimeOffset`, `TimeSpan`, `byte[]` or `null`), so a `Row` stays valid after the enumeration
moves on. A `QueryResult` can be enumerated once.

## Build and test

```sh
cargo build -p wartlog-ffi                      # target/debug/libwartlog.so
dotnet test bindings/dotnet/tests/WartLog.Tests
dotnet run --project bindings/dotnet/samples/WartLog.Sample -- write /tmp/demo.wartlogx
dotnet run --project bindings/dotnet/samples/WartLog.Sample -- read /tmp/demo.wartlogx
dotnet run --project bindings/dotnet/samples/WartLog.Sample -- shared   # needs wartlogd
```

The tests and the sample copy `libwartlog.so` from `target/debug` (override with
`-p:WartLogLibDir=...`). Packing: `cargo build --release -p wartlog-ffi`, then `dotnet pack
bindings/dotnet/src/WartLog` puts `target/release/libwartlog.so` under
`runtimes/linux-x64/native/` in the package.
