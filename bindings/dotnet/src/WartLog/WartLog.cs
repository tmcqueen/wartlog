using System.Collections;
using System.Runtime.InteropServices;
using System.Text;
using WartLog.Interop;

namespace WartLog;

/// <summary>Status codes from libwartlog (<c>WL_E_*</c>).</summary>
public enum ErrorCode
{
    /// <summary>A null argument, bad UTF-8, a closed handle, or a value out of range.</summary>
    Invalid = -1,
    /// <summary>Another process holds the store lock.</summary>
    Locked = -2,
    /// <summary>Private mode on a store wartlogd owns.</summary>
    DaemonStore = -3,
    /// <summary>A daemon-only operation on a store the library created.</summary>
    ExternalStore = -4,
    /// <summary>Shared mode, but no running wartlogd serves the store.</summary>
    NoDaemon = -5,
    /// <summary>No store at the path.</summary>
    NotFound = -6,
    /// <summary>The store or a segment is newer than this library reads.</summary>
    NewerFormat = -7,
    /// <summary>The KQL text is wrong; see <see cref="WartLogException.Span"/>.</summary>
    Query = -8,
    /// <summary>A bug inside WartLog (a caught Rust panic).</summary>
    Panic = -9,
    /// <summary>A column holds a different type, or null.</summary>
    Type = -10,
    /// <summary>I/O, damage, or the daemon refusing a request.</summary>
    Other = -11,
}

/// <summary>An error reported by libwartlog.</summary>
public sealed class WartLogException : Exception
{
    /// <summary>What went wrong.</summary>
    public ErrorCode Code { get; }

    /// <summary>For <see cref="ErrorCode.Query"/>: the byte range of the KQL text the error is about.</summary>
    public (int Start, int End)? Span { get; }

    internal WartLogException(ErrorCode code, string message, (int, int)? span) : base(message)
    {
        Code = code;
        Span = span;
    }
}

/// <summary>How a <see cref="Writer"/> reaches the store.</summary>
public enum WriterMode
{
    /// <summary>Through the running wartlogd; the library never touches the files.</summary>
    Shared = 1,
    /// <summary>A store this program owns (by convention <c>name.wartlogx</c>), created if absent.</summary>
    Private = 2,
    /// <summary>A daemon store while wartlogd is stopped (takes the store lock).</summary>
    Exclusive = 3,
}

/// <summary>Kind of a <see cref="LogSource"/>.</summary>
public enum SourceKind
{
    /// <summary>OpenTelemetry (identified by <c>service.name</c>).</summary>
    Otlp = 1,
    /// <summary>The systemd journal.</summary>
    Journald = 2,
    /// <summary>Kubernetes.</summary>
    K8s = 3,
    /// <summary>Lines of JSON or text (identified by <c>source.name</c>).</summary>
    Ndjson = 4,
}

/// <summary>Column value types.</summary>
public enum ValueType
{
#pragma warning disable CS1591
    Null = 0, Bool = 1, I64 = 2, F64 = 3, Str = 4, Bytes = 5, Timestamp = 6, Duration = 7, Json = 8,
#pragma warning restore CS1591
}

internal static unsafe class Util
{
    internal static void Check(int rc)
    {
        if (rc < 0) throw Error(rc);
    }

    internal static WartLogException Error(int rc)
    {
        var msg = Str(Native.wl_last_error()) ?? "";
        (int, int)? span = null;
        if (rc == (int)ErrorCode.Query && Native.wl_last_error_span(out var a, out var b) == 1) span = ((int)a, (int)b);
        return new WartLogException((ErrorCode)rc, msg, span);
    }

    internal static string? Str(byte* p) => p == null ? null : Marshal.PtrToStringUTF8((nint)p);

    internal static string Str(byte* p, nuint len) => Encoding.UTF8.GetString(p, checked((int)len));

    /// <summary>NUL-terminated UTF-8 copy, for paths and KQL.</summary>
    internal static byte[] Z(string s)
    {
        var b = new byte[Encoding.UTF8.GetByteCount(s) + 1];
        Encoding.UTF8.GetBytes(s, 0, s.Length, b, 0);
        return b;
    }

    internal static byte[] U(string s) => Encoding.UTF8.GetBytes(s);

    internal static string TakeString(byte* p)
    {
        try { return Str(p) ?? ""; }
        finally { Native.wl_string_free(p); }
    }

    internal static void EnsureAbi()
    {
        var v = Native.wl_abi_version();
        if (v < Native.AbiVersion)
            throw new WartLogException(ErrorCode.Other, $"libwartlog ABI {v} is older than the {Native.AbiVersion} this wrapper needs", null);
    }
}

internal sealed class SnapshotHandle : SafeHandle
{
    public SnapshotHandle() : base(0, true) { }
    public override bool IsInvalid => handle == 0;
    protected override bool ReleaseHandle() { Native.wl_snapshot_free(handle); return true; }
}

internal sealed class CursorHandle : SafeHandle
{
    public CursorHandle() : base(0, true) { }
    public override bool IsInvalid => handle == 0;
    protected override bool ReleaseHandle() { Native.wl_cursor_free(handle); return true; }
}

internal sealed class WriterHandle : SafeHandle
{
    public WriterHandle() : base(0, true) { }
    public override bool IsInvalid => handle == 0;
    // wl_writer_close seals and frees; errors here cannot be reported, use Writer.Close for that
    protected override bool ReleaseHandle() { Native.wl_writer_close(handle); return true; }
}

/// <summary>Library information.</summary>
public static unsafe class WartLogLibrary
{
    /// <summary>Version of the loaded libwartlog.</summary>
    public static string Version => Util.Str(Native.wl_version()) ?? "";

    /// <summary>ABI version of the loaded libwartlog.</summary>
    public static uint AbiVersion => Native.wl_abi_version();

    /// <summary>The daemon's store (<c>~/.local/share/wartlog</c> by default).</summary>
    public static string DefaultStore => Util.TakeString(Native.wl_default_store());

    /// <summary>Checks a store's seq rules (read-only). <paramref name="quick"/> reads block metadata only.</summary>
    public static VerifyResult VerifyStore(string root, bool quick = false)
    {
        Util.EnsureAbi();
        fixed (byte* r = Util.Z(root))
        {
            Util.Check(Native.wl_store_verify(r, quick ? 1 : 0, out var n, out var json));
            return new VerifyResult(n, Util.TakeString(json));
        }
    }
}

/// <summary>Result of <see cref="WartLogLibrary.VerifyStore"/>.</summary>
/// <param name="Findings">Number of problems found; 0 means the store is fine.</param>
/// <param name="ReportJson">The full report as JSON.</param>
public sealed record VerifyResult(ulong Findings, string ReportJson)
{
    /// <summary>True when nothing was found.</summary>
    public bool Ok => Findings == 0;
}

/// <summary>Query options.</summary>
public sealed class QueryOptions
{
    /// <summary>Row cap when the query has no take/top/aggregation (null: the library default, 10 000).</summary>
    public ulong? Limit { get; set; }
    /// <summary>No row cap at all.</summary>
    public bool Unlimited { get; set; }
    /// <summary>Query threads (0: the library default, 2).</summary>
    public uint Threads { get; set; }
    /// <summary>Used when the query has no time filter of its own.</summary>
    public DateTimeOffset? From { get; set; }
    /// <summary>Used when the query has no time filter of its own.</summary>
    public DateTimeOffset? To { get; set; }
}

/// <summary>A read-only view of a store at the moment it was opened. Works with or without
/// wartlogd. Safe to query from several threads. Dispose it when done.</summary>
public sealed unsafe class Snapshot : IDisposable
{
    private readonly SnapshotHandle _h;

    private Snapshot(SnapshotHandle h) => _h = h;

    /// <summary>Opens the store at <paramref name="root"/>.</summary>
    public static Snapshot Open(string root)
    {
        Util.EnsureAbi();
        fixed (byte* r = Util.Z(root))
        {
            var rc = Native.wl_snapshot_open(r, out var p);
            if (rc < 0) throw Util.Error(rc);
            var h = new SnapshotHandle();
            Marshal.InitHandle(h, p);
            return new Snapshot(h);
        }
    }

    /// <summary>Highest record id in the snapshot.</summary>
    public ulong MaxSeq { get { Stats(out var m, out _); return m; } }

    /// <summary>Number of records in the snapshot.</summary>
    public ulong RecordCount { get { Stats(out _, out var n); return n; } }

    private void Stats(out ulong maxSeq, out ulong records)
    {
        bool added = false;
        _h.DangerousAddRef(ref added);
        try { Util.Check(Native.wl_snapshot_stats(_h.DangerousGetHandle(), out maxSeq, out records)); }
        finally { if (added) _h.DangerousRelease(); }
    }

    /// <summary>Runs a KQL query (<c>logs | ...</c> or <c>spans | ...</c>).</summary>
    public QueryResult Query(string kql, QueryOptions? options = null)
    {
        var o = new Native.QueryOpts { Size = (uint)sizeof(Native.QueryOpts), FromNs = long.MinValue, ToNs = long.MaxValue };
        if (options != null)
        {
            o.Threads = options.Threads;
            o.Limit = options.Unlimited ? ulong.MaxValue : options.Limit ?? 0;
            if (options.From is { } f) o.FromNs = ToNs(f);
            if (options.To is { } t) o.ToNs = ToNs(t);
        }
        bool added = false;
        _h.DangerousAddRef(ref added);
        try
        {
            fixed (byte* q = Util.Z(kql))
            {
                var rc = Native.wl_query(_h.DangerousGetHandle(), q, &o, out var c);
                if (rc < 0) throw Util.Error(rc);
                var ch = new CursorHandle();
                Marshal.InitHandle(ch, c);
                return new QueryResult(ch);
            }
        }
        finally { if (added) _h.DangerousRelease(); }
    }

    /// <summary>The record with this id as JSON, or null when the store no longer holds it.</summary>
    public string? GetRecordJson(ulong seq)
    {
        bool added = false;
        _h.DangerousAddRef(ref added);
        try
        {
            var rc = Native.wl_record_get(_h.DangerousGetHandle(), seq, out var rec);
            if (rc < 0) throw Util.Error(rc);
            if (rc == 0) return null;
            try { return Util.Str(Native.wl_record_json(rec)); }
            finally { Native.wl_record_free(rec); }
        }
        finally { if (added) _h.DangerousRelease(); }
    }

    internal static long ToNs(DateTimeOffset t) => (t.UtcTicks - DateTimeOffset.UnixEpoch.UtcTicks) * 100;

    /// <inheritdoc/>
    public void Dispose() => _h.Dispose();
}

/// <summary>One result row, copied out of the library: safe to keep after the enumeration moves on.</summary>
public sealed class Row
{
    private readonly IReadOnlyList<string> _columns;

    internal Row(IReadOnlyList<string> columns, object?[] values, ulong? id)
    {
        _columns = columns;
        Values = values;
        Id = id;
    }

    /// <summary>The record id when the row is still one whole record (not projected or aggregated).</summary>
    public ulong? Id { get; }

    /// <summary>Values in column order: <c>string</c>, <c>long</c>, <c>double</c>, <c>bool</c>,
    /// <c>DateTimeOffset</c> (timestamps), <c>TimeSpan</c> (durations), <c>byte[]</c>, or null.
    /// JSON values are strings.</summary>
    public object?[] Values { get; }

    /// <summary>Value by column index.</summary>
    public object? this[int column] => Values[column];

    /// <summary>Value by column name.</summary>
    public object? this[string column]
    {
        get
        {
            for (int i = 0; i < _columns.Count; i++) if (_columns[i] == column) return Values[i];
            throw new KeyNotFoundException($"no column {column}");
        }
    }
}

/// <summary>Rows of a query, read once, in order. Dispose it (or enumerate to the end).</summary>
public sealed unsafe class QueryResult : IEnumerable<Row>, IDisposable
{
    private readonly CursorHandle _h;
    private bool _started;

    /// <summary>Column names.</summary>
    public IReadOnlyList<string> Columns { get; }

    internal QueryResult(CursorHandle h)
    {
        _h = h;
        var c = h.DangerousGetHandle();
        Util.Check(Native.wl_cursor_columns(c, out var n));
        var cols = new string[n];
        for (uint i = 0; i < n; i++)
        {
            Util.Check(Native.wl_cursor_column_name(c, i, out var p, out var len));
            cols[i] = Util.Str(p, len);
        }
        Columns = cols;
    }

    /// <summary>Planning and scan statistics, as JSON.</summary>
    public string StatsJson
    {
        get
        {
            Util.Check(Native.wl_cursor_stats_json(_h.DangerousGetHandle(), out var j));
            return Util.TakeString(j);
        }
    }

    /// <inheritdoc/>
    public IEnumerator<Row> GetEnumerator()
    {
        if (_started) throw new InvalidOperationException("a query result can be enumerated once");
        _started = true;
        return Enumerate();
    }

    private IEnumerator<Row> Enumerate()
    {
        while (true)
        {
            Row? r = Next();
            if (r == null) yield break;
            yield return r;
        }
    }

    private Row? Next()
    {
        bool added = false;
        _h.DangerousAddRef(ref added);
        try
        {
            var rc = Native.wl_cursor_next(_h.DangerousGetHandle(), out var row);
            if (rc < 0) throw Util.Error(rc);
            if (rc == 0) return null;
            var vals = new object?[Columns.Count];
            for (uint i = 0; i < vals.Length; i++) vals[i] = Read(row, i);
            ulong? id = Native.wl_row_id(row, out var seq) == 1 ? seq : null;
            return new Row(Columns, vals, id);
        }
        finally { if (added) _h.DangerousRelease(); }
    }

    private static object? Read(nint row, uint col)
    {
        Util.Check(Native.wl_row_type(row, col, out var t));
        switch ((ValueType)t)
        {
            case ValueType.Null:
                return null;
            case ValueType.Bool:
                Util.Check(Native.wl_row_get_bool(row, col, out var b));
                return b != 0;
            case ValueType.I64:
                Util.Check(Native.wl_row_get_i64(row, col, out var i));
                return i;
            case ValueType.F64:
                Util.Check(Native.wl_row_get_f64(row, col, out var f));
                return f;
            case ValueType.Timestamp:
                Util.Check(Native.wl_row_get_i64(row, col, out var ns));
                return DateTimeOffset.UnixEpoch.AddTicks(ns / 100);
            case ValueType.Duration:
                Util.Check(Native.wl_row_get_i64(row, col, out var d));
                return TimeSpan.FromTicks(d / 100);
            case ValueType.Bytes:
                {
                    Util.Check(Native.wl_row_get_str(row, col, out var p, out var len));
                    return new ReadOnlySpan<byte>(p, checked((int)len)).ToArray();
                }
            default:
                {
                    Util.Check(Native.wl_row_get_str(row, col, out var p, out var len));
                    return Util.Str(p, len);
                }
        }
    }

    IEnumerator IEnumerable.GetEnumerator() => GetEnumerator();

    /// <inheritdoc/>
    public void Dispose() => _h.Dispose();
}

/// <summary>"Who sent this": a kind and identifying attributes. Keep per-run values (pids,
/// instance ids) in record attributes instead, so the source stays the same across runs.</summary>
public sealed record LogSource(SourceKind Kind, IReadOnlyDictionary<string, string> Attributes)
{
    /// <summary>An NDJSON-style source named <paramref name="name"/> (<c>source.name</c>).</summary>
    public static LogSource Named(string name) => new(SourceKind.Ndjson, new Dictionary<string, string> { ["source.name"] = name });

    /// <summary>An OpenTelemetry-style source (<c>service.name</c>).</summary>
    public static LogSource Service(string name) => new(SourceKind.Otlp, new Dictionary<string, string> { ["service.name"] = name });
}

/// <summary>A log record to write.</summary>
public sealed class LogRecord
{
    /// <summary>Who sent it.</summary>
    public required LogSource Source { get; init; }
    /// <summary>Event time (default: now).</summary>
    public DateTimeOffset? Time { get; init; }
    /// <summary>OpenTelemetry severity number: 5 debug, 9 info, 13 warn, 17 error, 21 fatal.</summary>
    public byte Severity { get; init; } = 9;
    /// <summary>Optional severity text ("Information", "warning", ...).</summary>
    public string? SeverityText { get; init; }
    /// <summary>The message (ignored for display when <see cref="Template"/> is set).</summary>
    public string? Body { get; init; }
    /// <summary>A message template such as <c>Order {OrderId} shipped</c>, filled from attributes.</summary>
    public string? Template { get; init; }
    /// <summary>16-byte trace id.</summary>
    public byte[]? TraceId { get; init; }
    /// <summary>8-byte span id.</summary>
    public byte[]? SpanId { get; init; }
    /// <summary>Attributes: string, bool, integer and floating-point values (others are written as text).</summary>
    public IReadOnlyDictionary<string, object?> Attributes { get; init; } = new Dictionary<string, object?>();
}

/// <summary>Writer options.</summary>
public sealed class WriterOptions
{
    /// <summary>Shared mode: the daemon's API socket (default <c>$XDG_RUNTIME_DIR/wartlog/api.sock</c>).</summary>
    public string? Socket { get; set; }
    /// <summary>Recorded in the manifest of a store this writer creates.</summary>
    public string? Creator { get; set; }
    /// <summary>Source name for <see cref="Writer.PushJson"/> lines that do not name one (default "app").</summary>
    public string? DefaultSource { get; set; }
    /// <summary>Private/exclusive: WAL fsync interval of the background thread (default 250 ms).</summary>
    public TimeSpan? SyncEvery { get; set; }
    /// <summary>Private/exclusive retention by size (default: none).</summary>
    public ulong? RetentionMaxBytes { get; set; }
    /// <summary>Private/exclusive retention by age (default: none).</summary>
    public TimeSpan? RetentionMaxAge { get; set; }
}

/// <summary>Writes records to a store. Use from one thread at a time. Call <see cref="Close"/>
/// to see errors from the final seal; <see cref="Dispose"/> closes too, silently.</summary>
public sealed unsafe class Writer : IDisposable
{
    private readonly WriterHandle _h;
    // source handles by kind and attributes (LogSource compares its dictionary by reference)
    private readonly Dictionary<string, nint> _sources = new();

    private Writer(WriterHandle h) => _h = h;

    /// <summary>Opens a writer. Mismatches between the mode and the store are refused
    /// (<see cref="ErrorCode.DaemonStore"/>, <see cref="ErrorCode.NoDaemon"/>, <see cref="ErrorCode.Locked"/>, <see cref="ErrorCode.NotFound"/>).</summary>
    public static Writer Open(string root, WriterMode mode, WriterOptions? options = null)
    {
        Util.EnsureAbi();
        options ??= new WriterOptions();
        byte[]? sock = options.Socket is { } s ? Util.Z(s) : null;
        byte[]? creator = options.Creator is { } c ? Util.Z(c) : null;
        byte[]? def = options.DefaultSource is { } d ? Util.Z(d) : null;
        fixed (byte* r = Util.Z(root))
        fixed (byte* ps = sock)
        fixed (byte* pc = creator)
        fixed (byte* pd = def)
        {
            var o = new Native.WriterOpts
            {
                Size = (uint)sizeof(Native.WriterOpts),
                SyncEveryMs = options.SyncEvery is { } e ? (uint)Math.Max(1, e.TotalMilliseconds) : 0,
                Socket = ps,
                Creator = pc,
                DefaultSource = pd,
                RetentionMaxBytes = options.RetentionMaxBytes ?? 0,
                RetentionMaxAgeS = options.RetentionMaxAge is { } a ? (ulong)a.TotalSeconds : 0,
            };
            var rc = Native.wl_writer_open(r, (int)mode, &o, out var w);
            if (rc < 0) throw Util.Error(rc);
            var h = new WriterHandle();
            Marshal.InitHandle(h, w);
            return new Writer(h);
        }
    }

    private nint W => _h.IsClosed ? throw new ObjectDisposedException(nameof(Writer)) : _h.DangerousGetHandle();

    /// <summary>Normalizes one NDJSON or plain-text line as wartlogd's NDJSON receivers do, and
    /// writes it. Returns the record id.</summary>
    public ulong PushJson(string line)
    {
        var b = Util.U(line);
        fixed (byte* p = b)
        {
            Util.Check(Native.wl_writer_push_json(W, p, (nuint)b.Length, out var seq));
            return seq;
        }
    }

    /// <summary>Writes a record. Returns its id; it is durable after <see cref="Flush"/>.</summary>
    public ulong Push(LogRecord r)
    {
        var src = Source(r.Source);
        var b = Native.wl_record_new(src);
        if (b == 0) throw Util.Error((int)ErrorCode.Invalid);
        try
        {
            Util.Check(Native.wl_record_set_time(b, Snapshot.ToNs(r.Time ?? DateTimeOffset.UtcNow)));
            Bytes(r.SeverityText ?? "", (p, n) => Native.wl_record_set_severity(b, r.Severity, n == 0 ? null : p, n));
            if (r.Body != null) Bytes(r.Body, (p, n) => Native.wl_record_set_body(b, p, n));
            if (r.Template != null) Bytes(r.Template, (p, n) => Native.wl_record_set_template(b, p, n));
            if (r.TraceId != null)
            {
                if (r.TraceId.Length != 16 || r.SpanId is { Length: not 8 }) throw new ArgumentException("trace id must be 16 bytes and span id 8 bytes");
                fixed (byte* t = r.TraceId)
                fixed (byte* sp = r.SpanId)
                    Util.Check(Native.wl_record_set_trace(b, t, sp));
            }
            foreach (var (k, v) in r.Attributes)
            {
                var kb = Util.U(k);
                fixed (byte* kp = kb)
                {
                    var kl = (nuint)kb.Length;
                    switch (v)
                    {
                        case null: break;
                        case bool x: Util.Check(Native.wl_record_set_attr_bool(b, kp, kl, x ? 1 : 0)); break;
                        case sbyte or byte or short or ushort or int or uint or long:
                            Util.Check(Native.wl_record_set_attr_i64(b, kp, kl, Convert.ToInt64(v))); break;
                        case float or double or decimal:
                            Util.Check(Native.wl_record_set_attr_f64(b, kp, kl, Convert.ToDouble(v))); break;
                        default:
                            {
                                var vb = Util.U(Convert.ToString(v, System.Globalization.CultureInfo.InvariantCulture) ?? "");
                                fixed (byte* vp = vb) Util.Check(Native.wl_record_set_attr_str(b, kp, kl, vp, (nuint)vb.Length));
                                break;
                            }
                    }
                }
            }
        }
        catch
        {
            Native.wl_record_builder_free(b);
            throw;
        }
        // consumes the builder, also on error
        Util.Check(Native.wl_writer_push(W, b, out var seq));
        return seq;
    }

    private delegate int WithBytes(byte* p, nuint len);

    private static void Bytes(string s, WithBytes f)
    {
        var b = Util.U(s);
        fixed (byte* p = b) Util.Check(f(p, (nuint)b.Length));
    }

    private nint Source(LogSource s)
    {
        var key = ((int)s.Kind).ToString() + "\0" + string.Join("\0", s.Attributes.OrderBy(a => a.Key, StringComparer.Ordinal).Select(a => a.Key + "\0" + a.Value));
        if (_sources.TryGetValue(key, out var h)) return h;
        Util.Check(Native.wl_source_new((int)s.Kind, out h));
        foreach (var (k, v) in s.Attributes)
        {
            var kb = Util.U(k);
            var vb = Util.U(v);
            fixed (byte* kp = kb)
            fixed (byte* vp = vb)
                Util.Check(Native.wl_source_set_attr(h, kp, (nuint)kb.Length, vp, (nuint)vb.Length));
        }
        _sources[key] = h;
        return h;
    }

    /// <summary>Blocks until everything pushed is durable; returns the highest durable id.</summary>
    public ulong Flush()
    {
        Util.Check(Native.wl_writer_flush(W, out var d));
        return d;
    }

    /// <summary>Flushes, seals everything and frees the writer. Throws if the final seal fails.</summary>
    public void Close()
    {
        if (_h.IsClosed) return;
        var w = _h.DangerousGetHandle();
        _h.SetHandleAsInvalid(); // wl_writer_close frees it, whatever it returns
        FreeSources();
        Util.Check(Native.wl_writer_close(w));
    }

    private void FreeSources()
    {
        foreach (var s in _sources.Values) Native.wl_source_free(s);
        _sources.Clear();
    }

    /// <inheritdoc/>
    public void Dispose()
    {
        FreeSources();
        _h.Dispose();
    }
}
