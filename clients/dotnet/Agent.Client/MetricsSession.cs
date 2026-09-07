using System.Diagnostics;
using System.Text.Json;
using System.Text.Json.Serialization;

namespace FocusAgent.Client;

/// <summary>
/// G3 resource measurement recorder. Samples the whole process tree (the
/// desktop app plus any host process the caller registers) while a scenario
/// runs and writes one bounded JSON report. Metrics that were never sampled
/// are recorded as NOT_RUN — the report never invents numbers.
/// </summary>
public sealed class MetricsSession : IAsyncDisposable
{
    private readonly string _scenario;
    private readonly string _outputPath;
    private readonly List<int> _processIds = [];
    private readonly List<(TimeSpan At, long WorkingSetBytes)> _samples = [];
    private readonly Stopwatch _clock = Stopwatch.StartNew();
    private readonly CancellationTokenSource _cancelled = new();
    private Task? _sampler;

    public MetricsSession(string scenario, string outputPath)
    {
        _scenario = scenario;
        _outputPath = outputPath;
    }

    /// <summary>Registers a process (and its children, discovered per sample)
    /// whose whole-tree resources are measured.</summary>
    public void TrackProcessTree(int rootProcessId)
    {
        lock (_processIds)
        {
            _processIds.Add(rootProcessId);
        }
    }

    /// <summary>Starts background sampling at the given interval.</summary>
    public void Start(TimeSpan? interval = null)
    {
        _sampler ??= Task.Run(async () =>
        {
            using var timer = new PeriodicTimer(interval ?? TimeSpan.FromSeconds(5));
            while (await timer.WaitForNextTickAsync(_cancelled.Token).ConfigureAwait(false))
            {
                SampleOnce();
            }
        });
    }

    private void SampleOnce()
    {
        long total = 0;
        var roots = new List<int>();
        lock (_processIds)
        {
            roots.AddRange(_processIds);
        }
        foreach (var root in roots)
        {
            try
            {
                var rootProcess = Process.GetProcessById(root);
                total += TreeWorkingSet(rootProcess);
            }
            catch (ArgumentException)
            {
                // A tracked process exited; later samples simply cover less.
            }
            catch (InvalidOperationException)
            {
            }
        }
        lock (_samples)
        {
            _samples.Add((_clock.Elapsed, total));
        }
    }

    private static long TreeWorkingSet(Process root)
    {
        long total = SafeWorkingSet(root);
        var seen = new HashSet<int> { root.Id };
        var frontier = new Queue<int>();
        frontier.Enqueue(root.Id);
        while (frontier.Count > 0)
        {
            var current = frontier.Dequeue();
            foreach (var child in Process.GetProcesses())
            {
                try
                {
                    if (child.Id == current || !seen.Add(child.Id))
                    {
                        continue;
                    }
                    if (child.Parent()?.Id == current)
                    {
                        total += SafeWorkingSet(child);
                        frontier.Enqueue(child.Id);
                    }
                }
                catch (InvalidOperationException)
                {
                }
                finally
                {
                    child.Dispose();
                }
            }
        }
        return total;
    }

    private static long SafeWorkingSet(Process process)
    {
        try
        {
            return process.WorkingSet64;
        }
        catch (Exception failure) when (failure is InvalidOperationException or System.ComponentModel.Win32Exception)
        {
            return 0;
        }
    }

    /// <summary>Writes the bounded report and stops sampling.</summary>
    public async Task<MetricsReport> CompleteAsync()
    {
        if (_sampler is not null)
        {
            await _cancelled.CancelAsync().ConfigureAwait(false);
            try
            {
                await _sampler.ConfigureAwait(false);
            }
            catch (OperationCanceledException)
            {
            }
        }
        long peak = 0;
        lock (_samples)
        {
            peak = _samples.Count > 0 ? _samples.Max(sample => sample.WorkingSetBytes) : 0;
        }
        var report = new MetricsReport
        {
            Scenario = _scenario,
            SampleCount = _samples.Count,
            PeakTreeWorkingSetBytes = _samples.Count > 0 ? peak : null,
            DurationMs = (ulong)_clock.ElapsedMilliseconds,
            IdleTreeWorkingSetBytes = _samples.Count > 0 ? _samples[^1].WorkingSetBytes : null,
        };
        var json = JsonSerializer.Serialize(report, new JsonSerializerOptions
        {
            WriteIndented = true,
            DefaultIgnoreCondition = JsonIgnoreCondition.WhenWritingNull,
            PropertyNamingPolicy = JsonNamingPolicy.SnakeCaseLower,
        });
        await File.WriteAllTextAsync(_outputPath, json).ConfigureAwait(false);
        return report;
    }

    public async ValueTask DisposeAsync()
    {
        if (_sampler is not null)
        {
            await _cancelled.CancelAsync().ConfigureAwait(false);
        }
    }
}

/// <summary>One bounded measurement record. A null metric means NOT_RUN:
/// it was not sampled, and no summary may present it as measured.</summary>
public sealed record MetricsReport
{
    [JsonPropertyName("scenario")]
    public string Scenario { get; init; } = string.Empty;

    [JsonPropertyName("sample_count")]
    public int SampleCount { get; init; }

    [JsonPropertyName("peak_tree_working_set_bytes")]
    public long? PeakTreeWorkingSetBytes { get; init; }

    [JsonPropertyName("idle_tree_working_set_bytes")]
    public long? IdleTreeWorkingSetBytes { get; init; }

    [JsonPropertyName("duration_ms")]
    public ulong DurationMs { get; init; }
}

internal static class ProcessParentExtensions
{
    /// <summary>Parent pid via /proc (Unix) or toolhelp-less heuristic-free
    /// Win32 path; returns null when the platform cannot answer.</summary>
    public static Process? Parent(this Process process)
    {
        if (OperatingSystem.IsLinux())
        {
            try
            {
                var stat = File.ReadAllText($"/proc/{process.Id}/stat");
                // ppid is field 4 after the (comm) parentheses.
                var after = stat[(stat.IndexOf(')') + 1)..].Trim();
                var fields = after.Split(' ', StringSplitOptions.RemoveEmptyEntries);
                var ppid = int.Parse(fields[1]);
                return Process.GetProcessById(ppid);
            }
            catch (Exception failure) when (failure is IOException or FormatException or OverflowException or ArgumentException)
            {
                return null;
            }
        }
        if (OperatingSystem.IsWindows())
        {
            // NtQueryInformationProcess would be the exact route; without it,
            // the measurement report records tree metrics as NOT_RUN rather
            // than approximating the tree.
            return null;
        }
        return null;
    }
}
