using WartLog;
using Xunit;

public sealed class WartLogTests : IDisposable
{
    private readonly string _dir = Directory.CreateTempSubdirectory("wartlog-test-").FullName;
    private string Store => Path.Combine(_dir, "app.wartlogx");

    public void Dispose() => Directory.Delete(_dir, recursive: true);

    private static readonly DateTimeOffset T0 = new(2026, 11, 26, 10, 0, 0, TimeSpan.Zero);

    private void WriteSample(int n)
    {
        using var w = Writer.Open(Store, WriterMode.Private, new WriterOptions { Creator = "tests" });
        var src = LogSource.Named("orders");
        for (int i = 0; i < n; i++)
        {
            var id = w.Push(new LogRecord
            {
                Source = src,
                Time = T0.AddSeconds(i),
                Severity = (byte)(i % 10 == 0 ? 17 : 9),
                Template = "Order {OrderId} shipped in {Ms} ms",
                Attributes = new Dictionary<string, object?> { ["OrderId"] = i, ["Ms"] = i * 0.5, ["Express"] = i % 2 == 0, ["Region"] = "eu" },
            });
            Assert.Equal((ulong)i + 1, id);
        }
        Assert.True(w.Flush() >= (ulong)n);
        w.Close();
    }

    [Fact]
    public void LibraryLoads()
    {
        Assert.True(WartLogLibrary.AbiVersion >= 1);
        Assert.False(string.IsNullOrEmpty(WartLogLibrary.Version));
    }

    [Fact]
    public void WriteThenQuery()
    {
        WriteSample(40);
        using var s = Snapshot.Open(Store);
        Assert.Equal(40UL, s.RecordCount);
        Assert.Equal(40UL, s.MaxSeq);

        using (var r = s.Query("logs | where Level == 'error' | count"))
        {
            Assert.Equal(new[] { "Count" }, r.Columns);
            Assert.Equal(4L, r.Single()[0]);
        }

        using (var r = s.Query("logs | where OrderId >= 37 | sort by OrderId asc | project OrderId, Body, Ms, Express, Timestamp"))
        {
            var rows = r.ToList();
            Assert.Equal(3, rows.Count);
            Assert.Equal(37L, rows[0]["OrderId"]);
            Assert.Equal("Order 37 shipped in 18.5 ms", rows[0]["Body"]);
            Assert.Equal(18.5, rows[0]["Ms"]);
            Assert.Equal(false, rows[0]["Express"]);
            Assert.Equal(T0.AddSeconds(37), rows[0]["Timestamp"]);
            Assert.Null(rows[0].Id); // projected rows are no longer one record
        }

        using (var r = s.Query("logs | where OrderId == 5"))
        {
            var row = r.Single();
            Assert.Equal(6UL, row.Id);
            Assert.Contains("Order {OrderId}", s.GetRecordJson(row.Id!.Value));
        }
        Assert.Null(s.GetRecordJson(12345));
    }

    [Fact]
    public void PushJsonLines()
    {
        using (var w = Writer.Open(Store, WriterMode.Private, new WriterOptions { DefaultSource = "lines" }))
        {
            w.PushJson("{\"@t\":\"2026-11-26T10:00:00Z\",\"@mt\":\"User {User} logged in\",\"User\":\"tim\",\"@l\":\"Warning\"}");
            w.PushJson("just text");
            w.Close();
        }
        using var s = Snapshot.Open(Store);
        using var r = s.Query("logs | where Source == 'lines' and Level == 'warn' | project Body");
        Assert.Equal("User tim logged in", r.Single()[0]);
    }

    [Fact]
    public void ErrorsHaveCodes()
    {
        var e = Assert.Throws<WartLogException>(() => Snapshot.Open(Path.Combine(_dir, "missing")));
        Assert.Equal(ErrorCode.NotFound, e.Code);

        WriteSample(1);
        using var s = Snapshot.Open(Store);
        e = Assert.Throws<WartLogException>(() => s.Query("logs | frobnicate"));
        Assert.Equal(ErrorCode.Query, e.Code);
        Assert.NotNull(e.Span);

        using var held = Writer.Open(Store, WriterMode.Private);
        e = Assert.Throws<WartLogException>(() => Writer.Open(Store, WriterMode.Exclusive));
        Assert.Equal(ErrorCode.Locked, e.Code);
        e = Assert.Throws<WartLogException>(() => Writer.Open(Store, WriterMode.Shared));
        Assert.Equal(ErrorCode.NoDaemon, e.Code);
        e = Assert.Throws<WartLogException>(() => Writer.Open(Path.Combine(_dir, "nope"), WriterMode.Exclusive));
        Assert.Equal(ErrorCode.NotFound, e.Code);
    }

    [Fact]
    public void VerifyAStore()
    {
        WriteSample(10);
        var v = WartLogLibrary.VerifyStore(Store);
        Assert.True(v.Ok, v.ReportJson);
        Assert.Contains("\"private\"", v.ReportJson);
        Assert.Equal(ErrorCode.NotFound, Assert.Throws<WartLogException>(() => WartLogLibrary.VerifyStore(Path.Combine(_dir, "nope"))).Code);
    }

    [Fact]
    public void DisposeClosesTheWriter()
    {
        var w = Writer.Open(Store, WriterMode.Private);
        w.PushJson("{\"msg\":\"x\"}");
        w.Dispose();
        Assert.Throws<ObjectDisposedException>(() => w.PushJson("y"));
        // closed and sealed: nothing is left in the WAL, and another writer can open it
        Assert.Empty(Directory.GetFiles(Path.Combine(Store, "wal")));
        using var w2 = Writer.Open(Store, WriterMode.Private);
        Assert.Equal(2UL, w2.PushJson("{\"msg\":\"y\"}"));
    }
}
