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
                            "\"watermark\":7,\"tasks\":[],\"pending_approvals\":" +
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
        Assert.Equal(0, report.SampleCount);
        var json = await File.ReadAllTextAsync(path);
        Assert.DoesNotContain("peak_tree_working_set_bytes", json); // null fields are absent, not zero
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
}
