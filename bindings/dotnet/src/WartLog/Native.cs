using System.Runtime.InteropServices;

namespace WartLog.Interop;

/// <summary>The C ABI (crates/wartlog-ffi/include/wartlog.h). Handles are raw pointers here and
/// wrapped in <see cref="System.Runtime.InteropServices.SafeHandle"/> subclasses one level up.</summary>
internal static unsafe partial class Native
{
    private const string Lib = "wartlog";

    /// <summary>The ABI version these declarations were written for.</summary>
    public const uint AbiVersion = 1;

    [StructLayout(LayoutKind.Sequential)]
    public struct QueryOpts
    {
        public uint Size;
        public uint Threads;
        public ulong Limit;
        public long FromNs;
        public long ToNs;
    }

    [StructLayout(LayoutKind.Sequential)]
    public struct WriterOpts
    {
        public uint Size;
        public uint SyncEveryMs;
        public byte* Socket;
        public byte* Creator;
        public byte* DefaultSource;
        public ulong RetentionMaxBytes;
        public ulong RetentionMaxAgeS;
    }

    [LibraryImport(Lib)] public static partial uint wl_abi_version();
    [LibraryImport(Lib)] public static partial byte* wl_version();
    [LibraryImport(Lib)] public static partial byte* wl_last_error();
    [LibraryImport(Lib)] public static partial int wl_last_error_span(out nuint start, out nuint end);
    [LibraryImport(Lib)] public static partial void wl_string_free(byte* s);
    [LibraryImport(Lib)] public static partial byte* wl_default_store();

    [LibraryImport(Lib)] public static partial int wl_snapshot_open(byte* root, out nint snap);
    [LibraryImport(Lib)] public static partial void wl_snapshot_free(nint snap);
    [LibraryImport(Lib)] public static partial int wl_snapshot_stats(nint snap, out ulong maxSeq, out ulong records);
    [LibraryImport(Lib)] public static partial int wl_query(nint snap, byte* kql, QueryOpts* opts, out nint cursor);
    [LibraryImport(Lib)] public static partial void wl_cursor_free(nint cursor);
    [LibraryImport(Lib)] public static partial int wl_cursor_columns(nint cursor, out uint count);
    [LibraryImport(Lib)] public static partial int wl_cursor_column_name(nint cursor, uint col, out byte* p, out nuint len);
    [LibraryImport(Lib)] public static partial int wl_cursor_next(nint cursor, out nint row);
    [LibraryImport(Lib)] public static partial int wl_cursor_stats_json(nint cursor, out byte* json);
    [LibraryImport(Lib)] public static partial int wl_row_type(nint row, uint col, out int type);
    [LibraryImport(Lib)] public static partial int wl_row_get_str(nint row, uint col, out byte* p, out nuint len);
    [LibraryImport(Lib)] public static partial int wl_row_get_i64(nint row, uint col, out long v);
    [LibraryImport(Lib)] public static partial int wl_row_get_f64(nint row, uint col, out double v);
    [LibraryImport(Lib)] public static partial int wl_row_get_bool(nint row, uint col, out int v);
    [LibraryImport(Lib)] public static partial int wl_row_id(nint row, out ulong seq);
    [LibraryImport(Lib)] public static partial int wl_record_get(nint snap, ulong seq, out nint record);
    [LibraryImport(Lib)] public static partial byte* wl_record_json(nint record);
    [LibraryImport(Lib)] public static partial void wl_record_free(nint record);
    [LibraryImport(Lib)] public static partial int wl_store_verify(byte* root, int quick, out ulong findings, out byte* reportJson);

    [LibraryImport(Lib)] public static partial int wl_writer_open(byte* root, int mode, WriterOpts* opts, out nint writer);
    [LibraryImport(Lib)] public static partial int wl_writer_push_json(nint writer, byte* line, nuint len, out ulong seq);
    [LibraryImport(Lib)] public static partial int wl_source_new(int kind, out nint source);
    [LibraryImport(Lib)] public static partial int wl_source_set_attr(nint source, byte* k, nuint kl, byte* v, nuint vl);
    [LibraryImport(Lib)] public static partial void wl_source_free(nint source);
    [LibraryImport(Lib)] public static partial nint wl_record_new(nint source);
    [LibraryImport(Lib)] public static partial void wl_record_builder_free(nint builder);
    [LibraryImport(Lib)] public static partial int wl_record_set_time(nint b, long ns);
    [LibraryImport(Lib)] public static partial int wl_record_set_severity(nint b, byte number, byte* text, nuint len);
    [LibraryImport(Lib)] public static partial int wl_record_set_body(nint b, byte* p, nuint len);
    [LibraryImport(Lib)] public static partial int wl_record_set_template(nint b, byte* p, nuint len);
    [LibraryImport(Lib)] public static partial int wl_record_set_trace(nint b, byte* traceId, byte* spanId);
    [LibraryImport(Lib)] public static partial int wl_record_set_attr_str(nint b, byte* k, nuint kl, byte* v, nuint vl);
    [LibraryImport(Lib)] public static partial int wl_record_set_attr_i64(nint b, byte* k, nuint kl, long v);
    [LibraryImport(Lib)] public static partial int wl_record_set_attr_f64(nint b, byte* k, nuint kl, double v);
    [LibraryImport(Lib)] public static partial int wl_record_set_attr_bool(nint b, byte* k, nuint kl, int v);
    [LibraryImport(Lib)] public static partial int wl_writer_push(nint writer, nint builder, out ulong seq);
    [LibraryImport(Lib)] public static partial int wl_writer_flush(nint writer, out ulong durableSeq);
    [LibraryImport(Lib)] public static partial int wl_writer_close(nint writer);
}
