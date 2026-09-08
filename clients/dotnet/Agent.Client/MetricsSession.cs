using System.Diagnostics;
using System.Text.Json;
using System.Text.Json.Serialization;

namespace FocusAgent.Client;

/// <summary>What a sample's process-tree walk actually covered. The report
/// labels every measurement honestly: a platform that cannot enumerate
/// parents measures the registered roots only and says so — never a fake
/// whole-tree number.</summary>
public enum TreeCoverage
{
    /// <summary>Only the registered root processes were measured; parent
    /// enumeration is unavailable on this platform (Windows), so descendants
    /// are not discoverable from this recorder.</summary>
    RootOnly,

    /// <summary>Every descendant reachable through the parent map was
    /// measured exactly once (Linux /proc enumeration).</summary>
    FullTree,

    /// <summary>The walk could not be completed confidently; the totals are
    /// a lower bound at best (a process vanished mid-enumeration).</summary>
    Unknown,
}

/// <summary>
/// G3/N7 resource measurement recorder. Samples the process tree (the
/// desktop app plus any host process the caller registers) while a scenario
/// runs and writes one bounded JSON report. Metrics that were never sampled
/// are recorded as NOT_RUN — the report never invents numbers.
///
/// N7/F14: each sample builds ONE parent map from a bounded process snapshot
/// and walks it with a single global visit set, so shared descendants are
/// counted exactly once across roots; the report labels its own coverage
/// (root_only / full_tree / unknown); samples live in a bounded ring while
/// count/max/last stay running aggregates; the idle reading is an explicit
/// scenario mark, never a guess at the last sample.
/// </summary>
public sealed class MetricsSession : IAsyncDisposable
{
    private readonly string _scenario;
    private readonly string _outputPath;
    private readonly List<int> _processIds = [];
    private readonly SampleRing _samples;
    private long _sampleCount;
    private long _peak;
    private (TimeSpan At, long WorkingSetBytes)? _last;
    private long? _idle;
    private TreeCoverage? _lastCoverage;
    private readonly Stopwatch _clock = Stopwatch.StartNew();
    private readonly CancellationTokenSource _cancelled = new();
    private Task? _sampler;

    /// <summary>N7 test seam: injectable per-sample process snapshot. The
    /// production path enumerates the OS.</summary>
    internal Func<ProcessGraph>? SnapshotFactory;

    public MetricsSession(string scenario, string outputPath, int maxRingSamples = 1024)
    {
        _scenario = scenario;
        _outputPath = outputPath;
        _samples = new SampleRing(maxRingSamples > 0 ? maxRingSamples : 1);
    }

    /// <summary>Registers a process (and its descendants, discovered per
    /// sample) whose whole-tree resources are measured.</summary>
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

    /// <summary>Explicitly marks the current moment as the scenario's idle
    /// reading: the report's idle field is set ONLY by this call, never
    /// inferred from the last sample (a last sample may be mid-scenario).</summary>
    public void MarkIdle()
    {
        var graph = (SnapshotFactory ?? SnapshotProcesses)();
        long total;
        lock (_processIds)
        {
            total = graph.TotalForRoots(_processIds);
        }
        _idle = total;
    }

    /// <summary>N7 drill observation: how many samples the bounded ring
    /// currently holds (the report's count keeps growing regardless).</summary>
    internal int RingCount
    {
        get
        {
            lock (_samples)
            {
                return _samples.Count;
            }
        }
    }

    private void SampleOnce()
    {
        var graph = (SnapshotFactory ?? SnapshotProcesses)();
        long total;
        lock (_processIds)
        {
            total = graph.TotalForRoots(_processIds);
        }
        lock (_samples)
        {
            var sample = (_clock.Elapsed, total);
            _samples.Add(sample);
            _sampleCount++;
            if (total > _peak)
            {
                _peak = total;
            }
            _last = sample;
            _lastCoverage = graph.Coverage;
        }
    }

    private static ProcessGraph SnapshotProcesses()
    {
        var workingSet = new Dictionary<int, long>();
        var parents = new Dictionary<int, int?>();
        var enumeratedParents = false;
        var hadErrors = false;
        foreach (var process in Process.GetProcesses())
        {
            try
            {
                workingSet[process.Id] = SafeWorkingSet(process);
                var parent = process.Parent();
                parents[process.Id] = parent?.Id;
                enumeratedParents |= parent is not null;
            }
            catch (InvalidOperationException)
            {
                // A process vanished between enumeration and its property
                // reads; its neighbors still produce a (lower-bound) total.
                hadErrors = true;
            }
            finally
            {
                process.Dispose();
            }
        }
        var coverage = !enumeratedParents
            ? TreeCoverage.RootOnly
            : hadErrors
                ? TreeCoverage.Unknown
                : TreeCoverage.FullTree;
        return new ProcessGraph(workingSet, parents, coverage);
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
        MetricsReport report;
        lock (_samples)
        {
            report = new MetricsReport
            {
                Scenario = _scenario,
                SampleCount = _sampleCount,
                PeakTreeWorkingSetBytes = _sampleCount > 0 ? _peak : null,
                FinalTreeWorkingSetBytes = _last?.WorkingSetBytes,
                IdleTreeWorkingSetBytes = _idle,
                DurationMs = (ulong)_clock.ElapsedMilliseconds,
                Coverage = _lastCoverage,
            };
        }
        var json = JsonSerializer.Serialize(report, new JsonSerializerOptions
        {
            WriteIndented = true,
            DefaultIgnoreCondition = JsonIgnoreCondition.WhenWritingNull,
            PropertyNamingPolicy = JsonNamingPolicy.SnakeCaseLower,
            Converters = { new JsonStringEnumConverter(JsonNamingPolicy.SnakeCaseLower) },
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
    public long SampleCount { get; init; }

    [JsonPropertyName("peak_tree_working_set_bytes")]
    public long? PeakTreeWorkingSetBytes { get; init; }

    /// <summary>The LAST sample of the run, whatever phase it captured. Not
    /// an idle measurement: the idle field is set only by an explicit
    /// <see cref="MetricsSession.MarkIdle"/> mark.</summary>
    [JsonPropertyName("final_tree_working_set_bytes")]
    public long? FinalTreeWorkingSetBytes { get; init; }

    /// <summary>Set only by an explicit <see cref="MetricsSession.MarkIdle"/>
    /// call; null means the scenario never marked an idle reading.</summary>
    [JsonPropertyName("idle_tree_working_set_bytes")]
    public long? IdleTreeWorkingSetBytes { get; init; }

    /// <summary>What the last sample's walk actually covered (root_only /
    /// full_tree / unknown). Null when nothing was sampled.</summary>
    [JsonPropertyName("tree_coverage")]
    public TreeCoverage? Coverage { get; init; }

    [JsonPropertyName("duration_ms")]
    public ulong DurationMs { get; init; }
}

/// <summary>One per-sample process snapshot: id → working set, id → parent,
/// and the honest coverage label derived from how much the platform could
/// enumerate. Totals walk the children map with ONE local visit set per
/// call, so a shared descendant is counted exactly once even when several
/// roots reach it.</summary>
internal sealed class ProcessGraph
{
    private readonly IReadOnlyDictionary<int, long> _workingSet;
    private readonly IReadOnlyDictionary<int, List<int>> _children;

    public ProcessGraph(
        IReadOnlyDictionary<int, long> workingSet,
        IReadOnlyDictionary<int, int?> parents,
        TreeCoverage coverage)
    {
        _workingSet = workingSet;
        Coverage = coverage;
        var children = new Dictionary<int, List<int>>();
        foreach (var (id, parent) in parents)
        {
            if (parent is { } parentId && parents.ContainsKey(parentId))
            {
                if (!children.TryGetValue(parentId, out var list))
                {
                    children[parentId] = list = [];
                }
                list.Add(id);
            }
        }
        _children = children;
    }

    public TreeCoverage Coverage { get; }

    public long TotalForRoots(IReadOnlyList<int> roots)
    {
        // One visit set shared across every root: a process reachable from
        // two roots is counted once, never once per root.
        var visited = new HashSet<int>();
        long total = 0;
        foreach (var root in roots)
        {
            if (!visited.Add(root))
            {
                continue;
            }
            total += _workingSet.GetValueOrDefault(root);
            if (Coverage == TreeCoverage.RootOnly)
            {
                continue; // no parent map on this platform: descendants are not discoverable
            }
            var frontier = new Queue<int>();
            frontier.Enqueue(root);
            while (frontier.Count > 0)
            {
                var current = frontier.Dequeue();
                if (!_children.TryGetValue(current, out var list))
                {
                    continue;
                }
                foreach (var child in list)
                {
                    if (visited.Add(child))
                    {
                        total += _workingSet.GetValueOrDefault(child);
                        frontier.Enqueue(child);
                    }
                }
            }
        }
        return total;
    }
}

/// <summary>N7/F14: bounded ring of the most recent samples for diagnosis;
/// the report's aggregates (count/max/last) are running, so trimming the
/// ring never loses totals.</summary>
internal sealed class SampleRing
{
    private readonly int _capacity;
    private readonly (TimeSpan At, long WorkingSetBytes)[] _items;
    private int _count;
    private int _head; // index of the oldest live entry

    public SampleRing(int capacity)
    {
        _capacity = capacity;
        _items = new (TimeSpan At, long WorkingSetBytes)[capacity];
    }

    public int Count => _count;

    public void Add((TimeSpan At, long WorkingSetBytes) sample)
    {
        if (_count < _capacity)
        {
            _items[(_head + _count) % _capacity] = sample;
            _count++;
        }
        else
        {
            _items[_head] = sample;
            _head = (_head + 1) % _capacity;
        }
    }
}

internal static class ProcessParentExtensions
{
    /// <summary>Parent pid via /proc (Unix) or heuristic-free Win32 path;
    /// returns null when the platform cannot answer.</summary>
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
            // the measurement report records the coverage as root_only rather
            // than approximating the tree.
            return null;
        }
        return null;
    }
}
