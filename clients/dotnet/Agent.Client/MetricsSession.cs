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
///
/// O1: structural coverage and numeric sampling completeness are separate
/// facts. A working-set read that fails is excluded from the totals and
/// counted in the report (never encoded as 0); a parent read that fails on
/// a parent-capable platform downgrades the coverage label to unknown; and
/// every aggregate (peak / final / idle) keeps the coverage quality of the
/// sample it actually came from.
/// </summary>
public sealed class MetricsSession : IAsyncDisposable
{
    private readonly string _scenario;
    private readonly string _outputPath;
    private readonly List<int> _processIds = [];
    private readonly SampleRing _samples;
    private long _sampleCount;
    private long _peak;
    private TreeCoverage? _peakCoverage;
    private int _peakReadFailures;
    private (TimeSpan At, long WorkingSetBytes, int ReadFailures)? _last;
    private long? _idle;
    private TreeCoverage? _idleCoverage;
    private int _idleReadFailures;
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
    /// inferred from the last sample (a last sample may be mid-scenario).
    /// O1: the idle aggregate keeps its OWN snapshot's coverage quality and
    /// read-failure count — it never borrows another sample's.</summary>
    public void MarkIdle()
    {
        var graph = (SnapshotFactory ?? SnapshotProcesses)();
        long total;
        lock (_processIds)
        {
            total = graph.TotalForRoots(_processIds);
        }
        lock (_samples)
        {
            _idle = total;
            _idleCoverage = graph.Coverage;
            _idleReadFailures = graph.WorkingSetReadFailures;
        }
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
            var sample = (_clock.Elapsed, total, graph.WorkingSetReadFailures);
            _samples.Add((sample.Item1, sample.Item2));
            _sampleCount++;
            if (total > _peak)
            {
                _peak = total;
                // O1: the peak aggregate keeps the coverage quality of the
                // sample that actually produced it, not the last sample's.
                _peakCoverage = graph.Coverage;
                _peakReadFailures = graph.WorkingSetReadFailures;
            }
            _last = sample;
            _lastCoverage = graph.Coverage;
        }
    }

    private static ProcessGraph SnapshotProcesses()
    {
        var reads = new List<ProcessRead>();
        var vanished = 0;
        foreach (var process in Process.GetProcesses())
        {
            try
            {
                long? read = TryReadWorkingSet(process, out var bytes)
                    ? bytes
                    : null; // missing, never a measured zero
                var parent = process.Parent();
                reads.Add(new ProcessRead(process.Id, read, parent.ParentId, parent.Failed));
            }
            catch (InvalidOperationException)
            {
                // A process vanished between enumeration and its property
                // reads; its neighbors still produce a (lower-bound) total.
                vanished++;
            }
            finally
            {
                process.Dispose();
            }
        }
        return BuildGraph(reads, vanished);
    }

    private static bool TryReadWorkingSet(Process process, out long workingSetBytes)
    {
        try
        {
            workingSetBytes = process.WorkingSet64;
            return true;
        }
        catch (Exception failure) when (failure is InvalidOperationException or System.ComponentModel.Win32Exception)
        {
            workingSetBytes = 0;
            return false; // the caller excludes and counts it; 0 is not a measurement
        }
    }

    /// <summary>N7 test seam: the pure classification core behind
    /// <see cref="SnapshotProcesses"/> — per-process read outcomes in, one
    /// labeled graph out, so partial-failure shapes are injectable without
    /// an OS process tableau.</summary>
    internal static ProcessGraph BuildGraph(IReadOnlyList<ProcessRead> reads, int vanishedProcesses)
    {
        // O1: structural coverage (is the parent map complete?) and numeric
        // sampling completeness (did every working-set read succeed?) are
        // separate facts. An unread working set is excluded from the total
        // and counted — never encoded as a measured 0; a failed parent read
        // is a structural hole, not a parentless root, and downgrades the
        // label to unknown.
        var workingSet = new Dictionary<int, long>();
        var unread = new List<int>();
        var parents = new Dictionary<int, int?>();
        var enumeratedParents = false;
        var parentReadFailures = 0;
        foreach (var read in reads)
        {
            if (read.WorkingSetBytes is { } bytes)
            {
                workingSet[read.ProcessId] = bytes;
            }
            else
            {
                unread.Add(read.ProcessId); // excluded from the total, counted in the report
            }
            parents[read.ProcessId] = read.ParentProcessId;
            if (read.ParentProcessId is not null)
            {
                enumeratedParents = true;
            }
            if (read.ParentReadFailed)
            {
                parentReadFailures++;
            }
        }
        var coverage = !enumeratedParents
            ? TreeCoverage.RootOnly
            : vanishedProcesses > 0 || parentReadFailures > 0
                ? TreeCoverage.Unknown
                : TreeCoverage.FullTree;
        return new ProcessGraph(workingSet, parents, coverage, unread, parentReadFailures);
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
                PeakCoverage = _sampleCount > 0 ? _peakCoverage : null,
                PeakWorkingSetReadFailures = _sampleCount > 0 ? _peakReadFailures : null,
                FinalTreeWorkingSetBytes = _last?.WorkingSetBytes,
                FinalWorkingSetReadFailures = _last?.ReadFailures,
                IdleTreeWorkingSetBytes = _idle,
                IdleCoverage = _idle is not null ? _idleCoverage : null,
                IdleWorkingSetReadFailures = _idle is not null ? _idleReadFailures : null,
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

/// <summary>
/// One bounded measurement record. A null metric means NOT_RUN:
/// it was not sampled, and no summary may present it as measured.
/// O1: every aggregate (peak / final / idle) additionally carries the
/// coverage quality and working-set read-failure count of the sample it
/// actually came from; the new fields are nullable additions, so readers
/// of the earlier schema keep working.
/// </summary>
public sealed record MetricsReport
{
    [JsonPropertyName("scenario")]
    public string Scenario { get; init; } = string.Empty;

    [JsonPropertyName("sample_count")]
    public long SampleCount { get; init; }

    [JsonPropertyName("peak_tree_working_set_bytes")]
    public long? PeakTreeWorkingSetBytes { get; init; }

    /// <summary>The structural coverage of the sample that produced
    /// <see cref="PeakTreeWorkingSetBytes"/> — not the last sample's. Null
    /// when nothing was sampled.</summary>
    [JsonPropertyName("peak_tree_coverage")]
    public TreeCoverage? PeakCoverage { get; init; }

    /// <summary>Working-set reads that failed in the peak sample; their bytes
    /// are excluded from the peak total (a lower bound), not counted as 0.
    /// Null when nothing was sampled.</summary>
    [JsonPropertyName("peak_working_set_read_failures")]
    public int? PeakWorkingSetReadFailures { get; init; }

    /// <summary>The LAST sample of the run, whatever phase it captured. Not
    /// an idle measurement: the idle field is set only by an explicit
    /// <see cref="MetricsSession.MarkIdle"/> mark.</summary>
    [JsonPropertyName("final_tree_working_set_bytes")]
    public long? FinalTreeWorkingSetBytes { get; init; }

    /// <summary>Working-set reads that failed in the last sample; excluded
    /// from the final total, not counted as 0. Null when nothing was
    /// sampled.</summary>
    [JsonPropertyName("final_working_set_read_failures")]
    public int? FinalWorkingSetReadFailures { get; init; }

    /// <summary>Set only by an explicit <see cref="MetricsSession.MarkIdle"/>
    /// call; null means the scenario never marked an idle reading.</summary>
    [JsonPropertyName("idle_tree_working_set_bytes")]
    public long? IdleTreeWorkingSetBytes { get; init; }

    /// <summary>The structural coverage of the idle mark's own snapshot —
    /// not the last sample's. Null when no idle mark was made.</summary>
    [JsonPropertyName("idle_tree_coverage")]
    public TreeCoverage? IdleCoverage { get; init; }

    /// <summary>Working-set reads that failed in the idle mark's snapshot;
    /// excluded from the idle total, not counted as 0. Null when no idle
    /// mark was made.</summary>
    [JsonPropertyName("idle_working_set_read_failures")]
    public int? IdleWorkingSetReadFailures { get; init; }

    /// <summary>What the LAST sample's walk actually covered (root_only /
    /// full_tree / unknown) — the same sample that produced
    /// <see cref="FinalTreeWorkingSetBytes"/>. The peak and idle aggregates
    /// carry their own coverage fields. Null when nothing was sampled.</summary>
    [JsonPropertyName("tree_coverage")]
    public TreeCoverage? Coverage { get; init; }

    [JsonPropertyName("duration_ms")]
    public ulong DurationMs { get; init; }
}

/// <summary>One per-process read outcome: the raw material
/// <see cref="MetricsSession.BuildGraph"/> classifies into a graph. A null
/// <see cref="WorkingSetBytes"/> marks a failed read (missing, never a
/// measured zero); <see cref="ParentReadFailed"/> marks a parent read that
/// failed on a parent-capable platform — structurally a hole, not a
/// parentless root.</summary>
internal readonly record struct ProcessRead(
    int ProcessId,
    long? WorkingSetBytes,
    int? ParentProcessId,
    bool ParentReadFailed);

/// <summary>One per-sample process snapshot: id → working set (successful
/// reads only), id → parent, and the honest coverage label derived from how
/// much the platform could enumerate. O1: structural coverage lives in
/// <see cref="Coverage"/>; numeric sampling completeness lives in
/// <see cref="WorkingSetReadFailures"/> (unread ids are excluded from every
/// total and counted here, never encoded as 0). Totals walk the children map
/// with ONE local visit set per call, so a shared descendant is counted
/// exactly once even when several roots reach it.</summary>
internal sealed class ProcessGraph
{
    private readonly IReadOnlyDictionary<int, long> _workingSet;
    private readonly IReadOnlyDictionary<int, List<int>> _children;

    public ProcessGraph(
        IReadOnlyDictionary<int, long> workingSet,
        IReadOnlyDictionary<int, int?> parents,
        TreeCoverage coverage)
        : this(workingSet, parents, coverage, [], 0)
    {
    }

    public ProcessGraph(
        IReadOnlyDictionary<int, long> workingSet,
        IReadOnlyDictionary<int, int?> parents,
        TreeCoverage coverage,
        IReadOnlyCollection<int> unreadWorkingSetIds,
        int parentReadFailures)
    {
        _workingSet = workingSet;
        Coverage = coverage;
        WorkingSetReadFailures = unreadWorkingSetIds.Count;
        ParentReadFailures = parentReadFailures;
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

    /// <summary>How many processes' working-set reads failed in this
    /// snapshot; their bytes are excluded from the totals, not zeroed.</summary>
    public int WorkingSetReadFailures { get; }

    /// <summary>How many parent reads failed on a parent-capable platform;
    /// each is a structural hole reflected in <see cref="Coverage"/>.</summary>
    public int ParentReadFailures { get; }

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

/// <summary>The outcome of one parent probe. <see cref="ParentId"/> is the
/// parent pid when known; <see cref="Supported"/> says whether the platform
/// has a parent map at all; <see cref="Failed"/> marks a read that failed on
/// a parent-capable platform — a structural hole, distinct from a root or
/// an unsupported platform.</summary>
internal readonly record struct ParentRead(int? ParentId, bool Supported, bool Failed)
{
    public static ParentRead Unsupported { get; } = new(null, Supported: false, Failed: false);

    public static ParentRead FromPpid(int ppid) => new(ppid, Supported: true, Failed: false);

    public static ParentRead Failure { get; } = new(null, Supported: true, Failed: true);
}

internal static class ProcessParentExtensions
{
    /// <summary>Parent pid via /proc (Unix) or heuristic-free Win32 path.
    /// Returns <see cref="ParentRead.Unsupported"/> when the platform cannot
    /// answer and <see cref="ParentRead.Failure"/> when a supported read or
    /// parse failed — never a silent null.</summary>
    public static ParentRead Parent(this Process process)
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
                return ParentRead.FromPpid(ppid);
            }
            catch (Exception failure) when (failure is IOException or FormatException or OverflowException or ArgumentException)
            {
                // The parent map has a hole here; the classifier must count
                // it instead of mistaking this process for a root.
                return ParentRead.Failure;
            }
        }
        if (OperatingSystem.IsWindows())
        {
            // NtQueryInformationProcess would be the exact route; without it,
            // the measurement report records the coverage as root_only rather
            // than approximating the tree.
            return ParentRead.Unsupported;
        }
        return ParentRead.Unsupported;
    }
}
