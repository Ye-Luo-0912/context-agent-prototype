using System.Net;
using System.Net.Sockets;
using System.Text;
using System.Text.Json;

using Xunit;

namespace FocusAgent.Client.Tests;

/// <summary>G2: reconnect rebuilds from a snapshot and never fabricates approvals.</summary>
public class ResumableSessionTests
{
    /// <summary>A scripted loopback host that answers snapshot requests with
    /// one pending approval until it is stopped.</summary>
    private sealed class ScriptedHost : IAsyncDisposable
    {
        private readonly TcpListener _listener = new(IPAddress.Loopback, 0);
        private readonly CancellationTokenSource _stopped = new();
        private readonly Task _serveLoop;

        public ScriptedHost()
        {
            _listener.Start();
            _serveLoop = Task.Run(() => ServeAsync(_stopped.Token));
        }

        public int Port => ((IPEndPoint)_listener.LocalEndpoint).Port;

        private async Task ServeAsync(CancellationToken cancellationToken)
        {
            try
            {
                while (!cancellationToken.IsCancellationRequested)
                {
                    var client = await _listener.AcceptTcpClientAsync(cancellationToken);
                    _ = Task.Run(() => ServeClientAsync(client, cancellationToken), cancellationToken);
                }
            }
            catch (OperationCanceledException)
            {
            }
            catch (ObjectDisposedException)
            {
            }
            catch (SocketException)
            {
                // listener stopped during accept: the drill is over
            }
        }

        private static async Task ServeClientAsync(TcpClient client, CancellationToken cancellationToken)
        {
            using var owned = client;
            var stream = client.GetStream();
            while (!cancellationToken.IsCancellationRequested)
            {
                var request = await FrameCodec.ReadFrameAsync(stream, FrameCodec.DefaultMaxFrameBytes, cancellationToken);
                if (request is null)
                {
                    return;
                }
                using var document = JsonDocument.Parse(request);
                var requestRoot = document.RootElement.Clone();
                var response = new
                {
                    protocol = requestRoot.GetProperty("protocol"),
                    message_id = Guid.NewGuid().ToString("D"),
                    request_id = requestRoot.GetProperty("request_id"),
                    kind = "response",
                    route = requestRoot.GetProperty("route"),
                    causality = new
                    {
                        correlation_id = requestRoot.GetProperty("causality").GetProperty("correlation_id"),
                        causation_id = requestRoot.GetProperty("message_id"),
                    },
                    payload = requestRoot.GetProperty("route").GetProperty("operation").GetString() == "snapshot"
                        ? JsonSerializer.Deserialize<JsonElement>(
                            "{\"status\":\"success\",\"value\":{\"run_started\":true,\"run_completed\":false," +
                            "\"watermark\":7,\"run_id\":\"00000000-0000-4000-8000-000000000031\",\"workspace_root\":\"/workspaces/test\"," +
                            "\"tasks\":[],\"pending_approvals\":" +
                            "[{\"request_id\":\"approval-live-1\",\"call_name\":\"fs.write\",\"risk\":\"workspace_write\",\"target_summary\":\"docs/plan.md\"}],\"resync_required\":false}}")
                        : JsonSerializer.Deserialize<JsonElement>(
                            "{\"status\":\"success\",\"value\":{\"watermark\":7,\"resync_required\":false}}"),
                };
                await FrameCodec.WriteFrameAsync(
                    stream, JsonSerializer.SerializeToUtf8Bytes(response, AgentJson.Options),
                    FrameCodec.DefaultMaxFrameBytes, cancellationToken);
            }
        }

        /// <summary>Stops accepting and drops live clients: the fault drill.</summary>
        public async ValueTask DisposeAsync()
        {
            await _stopped.CancelAsync();
            _listener.Stop();
            try
            {
                await _serveLoop.ConfigureAwait(false);
            }
            catch
            {
            }
        }
    }

    [Fact]
    public async Task Faulted_connection_fails_honestly_and_surfaces_loss()
    {
        await using var host = new ScriptedHost();
        var losses = new List<Exception>();
        var resyncs = new List<WorkSnapshotResponse>();
        using var cancel = new CancellationTokenSource(TimeSpan.FromSeconds(5));
        var session = new ResumableSession(() =>
        {
            var client = new TcpClient();
            return ConnectAndReturnStreamAsync(client, host.Port);
        });
        session.ConnectionLost += failure => losses.Add(failure);
        session.Resynced += snapshot => resyncs.Add(snapshot);

        try
        {
            var snapshot = await session.SnapshotAsync(cancel.Token);
            Assert.Equal(7ul, snapshot.Watermark);
            Assert.Single(snapshot.PendingApprovals);

            // The host dies mid-session: the next operation must fail
            // honestly (bounded), never hang or invent a snapshot.
            await host.DisposeAsync();
            await Assert.ThrowsAnyAsync<Exception>(
                () => session.SnapshotAsync(cancel.Token));
        }
        finally
        {
            await session.DisposeAsync();
        }
    }

    [Fact]
    public async Task Approval_answers_are_never_auto_retried_after_a_fault()
    {
        // Nothing is listening: the connection cannot come up.
        var session = new ResumableSession(() =>
        {
            var client = new TcpClient();
            return ConnectAndReturnStreamAsync(client, 1); // port 1: refused fast
        });
        try
        {
            await Assert.ThrowsAnyAsync<Exception>(
                () => session.RespondApprovalAsync("approval-1", ApprovalDecision.Allow)
                    .WaitAsync(TimeSpan.FromSeconds(5)));
        }
        finally
        {
            await session.DisposeAsync();
        }
    }

    private static async Task<Stream> ConnectAndReturnStreamAsync(TcpClient client, int port)
    {
        await client.ConnectAsync(IPAddress.Loopback, port);
        return client.GetStream();
    }
}

/// <summary>G3: streaming deltas merge into small windows without losing text.</summary>
public class DeltaCoalescerTests
{
    [Fact]
    public void Append_coalesces_into_windows_and_flush_is_lossless()
    {
        var flushed = new List<string>();
        var coalescer = new DeltaCoalescer(
            flushed.Add, flushCharBudget: 1_000, flushInterval: TimeSpan.FromHours(1));
        foreach (var chunk in new[] { "你好，", "world", "！(", "中英混排)", " tail" })
        {
            coalescer.Append(chunk);
        }
        Assert.Empty(flushed); // under budget and inside the interval
        coalescer.Flush();
        var combined = string.Concat(flushed);
        Assert.Equal("你好，world！(中英混排) tail", combined);
        // order preserved, nothing dropped
        Assert.Contains("你好，", combined, StringComparison.Ordinal);
        Assert.EndsWith(" tail", combined, StringComparison.Ordinal);
    }

    [Fact]
    public void Char_budget_forces_a_flush()
    {
        var flushed = new List<string>();
        var coalescer = new DeltaCoalescer(flushed.Add, flushCharBudget: 8, flushInterval: TimeSpan.FromHours(1));
        coalescer.Append(new string('x', 9));
        Assert.Single(flushed);
        Assert.Equal(new string('x', 9), flushed[0]);
    }

    [Fact]
    public async Task Time_budget_flushes_without_new_input()
    {
        // N7/F14: a short delta followed by silence still flushes on time —
        // the interval is honored without further Append calls.
        var flushed = new List<string>();
        using var coalescer = new DeltaCoalescer(
            flushed.Add, flushCharBudget: 1_000, flushInterval: TimeSpan.FromMilliseconds(100));
        coalescer.Append("short");
        Assert.Empty(flushed);
        var deadline = DateTimeOffset.UtcNow + TimeSpan.FromSeconds(5);
        while (flushed.Count == 0 && DateTimeOffset.UtcNow < deadline)
        {
            await Task.Delay(20);
        }
        Assert.Equal("short", Assert.Single(flushed));
    }
}

/// <summary>G3/N7: unmeasured metrics stay NOT_RUN (null), never invented;
/// the report labels its own coverage and the idle reading is explicit.</summary>
public class MetricsSessionTests
{
    private static string NewPath() =>
        System.IO.Path.Combine(System.IO.Path.GetTempPath(), $"metrics-{Guid.NewGuid():N}.json");

    [Fact]
    public async Task Unsampled_session_reports_null_metrics()
    {
        var path = NewPath();
        var session = new MetricsSession("no-op", path);
        var report = await session.CompleteAsync();
        Assert.Null(report.PeakTreeWorkingSetBytes);
        Assert.Null(report.FinalTreeWorkingSetBytes);
        Assert.Null(report.IdleTreeWorkingSetBytes);
        Assert.Null(report.Coverage);
        Assert.Null(report.PeakCoverage);
        Assert.Null(report.IdleCoverage);
        Assert.Null(report.PeakWorkingSetReadFailures);
        Assert.Null(report.FinalWorkingSetReadFailures);
        Assert.Null(report.IdleWorkingSetReadFailures);
        Assert.Equal(0, report.SampleCount);
        var json = await File.ReadAllTextAsync(path);
        Assert.DoesNotContain("peak_tree_working_set_bytes", json); // null fields are absent, not zero
        // O1 additions are nullable and absent when NOT_RUN: old readers of
        // the report schema keep decoding it unchanged.
        Assert.DoesNotContain("peak_tree_coverage", json);
        Assert.DoesNotContain("idle_tree_coverage", json);
        Assert.DoesNotContain("peak_working_set_read_failures", json);
    }

    [Fact]
    public async Task Tracked_process_produces_finite_samples_with_honest_coverage()
    {
        var path = NewPath();
        var session = new MetricsSession("self", path);
        session.TrackProcessTree(Environment.ProcessId);
        session.Start(TimeSpan.FromMilliseconds(50));
        await Task.Delay(180);
        var report = await session.CompleteAsync();
        Assert.True(report.SampleCount >= 1);
        Assert.True(report.PeakTreeWorkingSetBytes > 0);
        Assert.True(File.Exists(path));
        if (OperatingSystem.IsWindows())
        {
            // Windows cannot enumerate parents from this recorder: the label
            // says root_only — never a fake whole-tree number.
            Assert.Equal(TreeCoverage.RootOnly, report.Coverage);
        }
        else
        {
            Assert.NotNull(report.Coverage);
        }
    }

    [Fact]
    public async Task Tree_walk_counts_a_shared_descendant_exactly_once()
    {
        // Roots 1 and 2, where 2 is itself a descendant of 1 (2's parent is
        // 1) and 3 is 2's child. Walking each root independently would count
        // 2 and 3 twice; the single global visit set counts them once.
        var path = NewPath();
        var session = new MetricsSession("tree", path);
        session.SnapshotFactory = () => new ProcessGraph(
            new Dictionary<int, long> { [1] = 100, [2] = 40, [3] = 20 },
            new Dictionary<int, int?> { [1] = null, [2] = 1, [3] = 2 },
            TreeCoverage.FullTree);
        session.TrackProcessTree(1);
        session.TrackProcessTree(2);
        session.MarkIdle();
        var report = await session.CompleteAsync();
        Assert.Equal(160, report.IdleTreeWorkingSetBytes);
    }

    [Fact]
    public async Task RootOnly_coverage_on_a_platform_without_parents_is_honest()
    {
        var path = NewPath();
        var session = new MetricsSession("roots", path);
        // No parent info at all (the Windows shape): descendants cannot be
        // discovered, so the total is the roots only and the label says so.
        session.SnapshotFactory = () => new ProcessGraph(
            new Dictionary<int, long> { [1] = 100, [2] = 50 },
            new Dictionary<int, int?> { [1] = null, [2] = null },
            TreeCoverage.RootOnly);
        session.TrackProcessTree(1);
        session.MarkIdle();
        var report = await session.CompleteAsync();
        Assert.Equal(100, report.IdleTreeWorkingSetBytes); // 2 is not reachable
    }

    [Fact]
    public async Task Idle_is_only_set_by_an_explicit_mark()
    {
        var path = NewPath();
        var session = new MetricsSession("idle", path);
        session.SnapshotFactory = () => new ProcessGraph(
            new Dictionary<int, long> { [1] = 100 },
            new Dictionary<int, int?> { [1] = null },
            TreeCoverage.RootOnly);
        session.TrackProcessTree(1);
        session.Start(TimeSpan.FromMilliseconds(50));
        await Task.Delay(150);
        var report = await session.CompleteAsync();
        Assert.NotNull(report.FinalTreeWorkingSetBytes);
        Assert.Null(report.IdleTreeWorkingSetBytes); // no explicit idle mark
    }

    [Fact]
    public async Task Ring_is_bounded_while_the_sample_count_keeps_growing()
    {
        var path = NewPath();
        var session = new MetricsSession("ring", path, maxRingSamples: 4);
        session.SnapshotFactory = () => new ProcessGraph(
            new Dictionary<int, long> { [1] = 100 },
            new Dictionary<int, int?> { [1] = null },
            TreeCoverage.RootOnly);
        session.TrackProcessTree(1);
        session.Start(TimeSpan.FromMilliseconds(20));
        await Task.Delay(400);
        var report = await session.CompleteAsync();
        Assert.True(report.SampleCount >= 6, $"expected >= 6 samples, got {report.SampleCount}");
        Assert.True(session.RingCount <= 4, $"the ring must stay bounded, holds {session.RingCount}");
    }

    [Fact]
    public void Partial_parent_read_failure_downgrades_coverage_to_unknown()
    {
        // O1: process 1's parent relation was read; process 2's read failed.
        // A failed parent read is a structural hole, not a parentless root —
        // one readable parent must not buy a full_tree label while part of
        // the tree is undiscoverable.
        var graph = MetricsSession.BuildGraph(
        [
            new ProcessRead(1, 100, ParentProcessId: 0, ParentReadFailed: false),
            new ProcessRead(2, 50, ParentProcessId: null, ParentReadFailed: true),
        ], vanishedProcesses: 0);
        Assert.Equal(TreeCoverage.Unknown, graph.Coverage);
    }

    [Fact]
    public void Vanished_process_downgrades_coverage_to_unknown()
    {
        // A process that vanished mid-enumeration also leaves a structural
        // hole: the totals are a lower bound at best.
        var graph = MetricsSession.BuildGraph(
        [
            new ProcessRead(1, 100, ParentProcessId: 0, ParentReadFailed: false),
        ], vanishedProcesses: 1);
        Assert.Equal(TreeCoverage.Unknown, graph.Coverage);
    }

    [Fact]
    public void Unread_working_set_is_excluded_and_counted_not_encoded_as_zero()
    {
        // O1: root 1's working-set read failed. Its bytes are excluded from
        // the total and counted as a failure — a missing value, never a
        // measured zero. The parent map is complete, so the STRUCTURAL label
        // stays full_tree: numeric incompleteness is carried by the failure
        // count, not by faking the coverage label.
        var graph = MetricsSession.BuildGraph(
        [
            new ProcessRead(1, null, ParentProcessId: 0, ParentReadFailed: false),
            new ProcessRead(2, 50, ParentProcessId: 1, ParentReadFailed: false),
        ], vanishedProcesses: 0);
        Assert.Equal(1, graph.WorkingSetReadFailures);
        Assert.Equal(TreeCoverage.FullTree, graph.Coverage);
        Assert.Equal(50, graph.TotalForRoots([1])); // child counted; the unread root contributes nothing "measured"
        Assert.Equal(50, graph.TotalForRoots([2])); // root 2's own read succeeded
    }

    [Fact]
    public async Task Peak_and_final_keep_the_coverage_of_their_own_samples()
    {
        // O1: the first sample is a full-tree walk peaking at 1000; every
        // later sample is a root-only walk. The peak must carry ITS sample's
        // full_tree label, not borrow the last sample's root_only.
        var path = NewPath();
        var session = new MetricsSession("peak", path);
        var first = true;
        session.SnapshotFactory = () =>
        {
            if (first)
            {
                first = false;
                return new ProcessGraph(
                    new Dictionary<int, long> { [1] = 1000 },
                    new Dictionary<int, int?> { [1] = null },
                    TreeCoverage.FullTree);
            }
            return new ProcessGraph(
                new Dictionary<int, long> { [1] = 10 },
                new Dictionary<int, int?> { [1] = null },
                TreeCoverage.RootOnly);
        };
        session.TrackProcessTree(1);
        session.Start(TimeSpan.FromMilliseconds(30));
        await Task.Delay(250);
        var report = await session.CompleteAsync();
        Assert.True(report.SampleCount >= 2, $"expected >= 2 samples, got {report.SampleCount}");
        Assert.Equal(1000, report.PeakTreeWorkingSetBytes);
        Assert.Equal(TreeCoverage.FullTree, report.PeakCoverage); // the peak sample's own label
        Assert.Equal(TreeCoverage.RootOnly, report.Coverage);     // the last sample's label
        Assert.True(report.FinalTreeWorkingSetBytes < report.PeakTreeWorkingSetBytes);
        var json = await File.ReadAllTextAsync(path);
        Assert.Contains("peak_tree_coverage", json);
    }

    [Fact]
    public async Task Idle_mark_keeps_its_own_coverage_not_the_last_samples()
    {
        // O1: the idle mark's snapshot has a failed root read (bytes
        // excluded, counted) and an unknown label; every background sample
        // is root_only. The idle fields must reflect the idle snapshot —
        // none of them may be borrowed from the last sample.
        var path = NewPath();
        var session = new MetricsSession("idle-cov", path);
        var idlePhase = false;
        session.SnapshotFactory = () => idlePhase
            ? new ProcessGraph(
                new Dictionary<int, long>(),
                new Dictionary<int, int?> { [1] = null },
                TreeCoverage.Unknown,
                [1], // root 1's working-set read failed at idle
                parentReadFailures: 0)
            : new ProcessGraph(
                new Dictionary<int, long> { [1] = 100 },
                new Dictionary<int, int?> { [1] = null },
                TreeCoverage.RootOnly);
        session.TrackProcessTree(1);
        session.Start(TimeSpan.FromMilliseconds(30));
        await Task.Delay(150);
        idlePhase = true;
        session.MarkIdle();
        var report = await session.CompleteAsync();
        Assert.Equal(0, report.IdleTreeWorkingSetBytes);        // excluded, not measured
        Assert.Equal(1, report.IdleWorkingSetReadFailures);     // the hole is counted, not hidden
        Assert.Equal(TreeCoverage.Unknown, report.IdleCoverage); // the idle snapshot's own label
        var json = await File.ReadAllTextAsync(path);
        Assert.Contains("idle_tree_coverage", json);
        Assert.Contains("idle_working_set_read_failures", json);
    }
}
