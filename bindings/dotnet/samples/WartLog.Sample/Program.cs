// A sample reader and writer.
//
//   dotnet run -- write <store>        write 1000 records to a private store (<store>.wartlogx)
//   dotnet run -- read <store> [kql]   query any store (default: the daemon's), no daemon needed
//   dotnet run -- shared               write 3 records through the running wartlogd
using WartLog;

var cmd = args.FirstOrDefault() ?? "read";
switch (cmd)
{
    case "write":
    {
        var store = args.ElementAtOrDefault(1) ?? "sample.wartlogx";
        using var w = Writer.Open(store, WriterMode.Private, new WriterOptions { Creator = "WartLog.Sample" });
        var src = LogSource.Service("sample-app");
        var rng = new Random(1);
        ulong last = 0;
        for (int i = 0; i < 1000; i++)
        {
            last = w.Push(new LogRecord
            {
                Source = src,
                Severity = (byte)(rng.Next(20) == 0 ? 17 : 9),
                Template = "Request {Path} took {Ms} ms",
                Attributes = new Dictionary<string, object?> { ["Path"] = $"/items/{rng.Next(50)}", ["Ms"] = rng.Next(1, 500) },
            });
        }
        Console.WriteLine($"wrote ids 1..{last}, durable up to {w.Flush()}");
        w.Close();
        break;
    }
    case "read":
    {
        var store = args.ElementAtOrDefault(1) ?? WartLogLibrary.DefaultStore;
        var kql = args.ElementAtOrDefault(2) ?? "logs | summarize n = count(), errors = countif(Level == 'error') by Source | sort by n desc | take 10";
        using var s = Snapshot.Open(store);
        Console.WriteLine($"{store}: {s.RecordCount} records");
        using var r = s.Query(kql);
        Console.WriteLine(string.Join("\t", r.Columns));
        foreach (var row in r)
            Console.WriteLine(string.Join("\t", row.Values.Select(v => v?.ToString() ?? "")));
        break;
    }
    case "shared":
    {
        using var w = Writer.Open(WartLogLibrary.DefaultStore, WriterMode.Shared);
        for (int i = 0; i < 3; i++)
            Console.WriteLine($"id {w.PushJson($"{{\"@mt\":\"hello {{N}} from .NET\",\"N\":{i}}}")}");
        Console.WriteLine($"durable up to {w.Flush()}");
        w.Close();
        break;
    }
    default:
        Console.Error.WriteLine("usage: write <store> | read [store] [kql] | shared");
        return 2;
}
return 0;
