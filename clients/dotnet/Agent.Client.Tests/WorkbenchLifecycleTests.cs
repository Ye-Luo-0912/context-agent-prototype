using System.Text;
using System.Threading.Channels;

using FocusAgent.Desktop.Infrastructure;
using FocusAgent.Desktop.ViewModels;

using Xunit;

namespace FocusAgent.Client.Tests;

/// <summary>
/// N4/F13 lifecycle drills for the desktop workbench view model, driven on
/// the inline dispatcher so refresh/generation behavior is deterministic:
/// approval rows are stable per request id with constant command
/// registrations, removals release their commands, stale connection-era
/// refresh results are dropped, and the output area keeps both its line and
/// byte bounds. No UI and no execution semantics are involved — the stub
/// connection exists only as a drill fixture.
/// </summary>
public class WorkbenchLifecycleTests
{
    private const int BaseCommandCount = 6; // connect/disconnect/submit/continue/cancel/refresh

    private static WorkSnapshotResponse SnapshotWith(params PendingApprovalSnapshot[] approvals) => new()
    {
        RunStarted = true,
        RunCompleted = false,
        Watermark = 41,
        Focus = null,
        Tasks = [],
        PendingApprovals = approvals,
        ResyncRequired = false,
    };

    private static PendingApprovalSnapshot Approval(string requestId = "req-1") => new()
    {
        RequestId = requestId,
        CallName = "shell.exec",
        Risk = ApprovalRisk.ProcessExecution,
        TargetSummary = "cargo test --workspace",
    };

    /// <summary>Drill fixture: a connection whose snapshot answers are
    /// programmable and whose event stream is a channel the drill writes.</summary>
    private sealed class StubConnection : IAgentConnection
    {
        private readonly Channel<WorkEventNotification> _events =
            Channel.CreateBounded<WorkEventNotification>(64);

        public Func<CancellationToken, Task<WorkSnapshotResponse>> SnapshotHandler { get; set; } =
            _ => Task.FromResult(SnapshotWith());

        public ChannelWriter<WorkEventNotification> EventWriter => _events.Writer;

        public bool IsConnected { get; private set; } = true;

        public ChannelReader<WorkEventNotification> Events => _events.Reader;

        public Task<WorkSnapshotResponse> SnapshotAsync(CancellationToken cancellationToken = default) =>
            SnapshotHandler(cancellationToken);

        public Task<WorkSubmitResponse> SubmitWorkAsync(string goal, string clientRequestId, CancellationToken cancellationToken = default) =>
            throw new NotSupportedException("lifecycle drills do not submit");

        public Task<WorkContinueResponse> ContinueAsync(CancellationToken cancellationToken = default) =>
            throw new NotSupportedException("lifecycle drills do not continue");

        public Task<WorkCancelResponse> CancelCurrentTurnAsync(CancellationToken cancellationToken = default) =>
            throw new NotSupportedException("lifecycle drills do not cancel");

        public Task<WorkSubscribeResponse> SubscribeAsync(ulong? replayAfterSeq = null, CancellationToken cancellationToken = default) =>
            throw new NotSupportedException("lifecycle drills do not subscribe");

        public Task<ApprovalRespondResponse> RespondApprovalAsync(string requestId, ApprovalDecision decision, CancellationToken cancellationToken = default) =>
            throw new NotSupportedException("lifecycle drills do not answer approvals");

        public Task<WorkTaskDetailResponse> TaskDetailAsync(string taskId, CancellationToken cancellationToken = default) =>
            throw new NotSupportedException("lifecycle drills do not read task detail");

        public Task<WorkChangesResponse> ReadChangesAsync(int? limit = null, string? afterTx = null, CancellationToken cancellationToken = default) =>
            throw new NotSupportedException("lifecycle drills do not read changes");

        public Task<WorkArtifactResponse> ReadArtifactAsync(string reference, uint? maxBytes = null, CancellationToken cancellationToken = default) =>
            throw new NotSupportedException("lifecycle drills do not read artifacts");

        public Task<WorkContextResponse> ReadContextAsync(uint? limit = null, CancellationToken cancellationToken = default) =>
            throw new NotSupportedException("lifecycle drills do not read context");

        public ValueTask DisposeAsync()
        {
            IsConnected = false;
            _events.Writer.TryComplete();
            return ValueTask.CompletedTask;
        }
    }

    private static async Task WaitUntilAsync(Func<bool> condition, string what)
    {
        var deadline = DateTimeOffset.UtcNow + TimeSpan.FromSeconds(10);
        while (!condition())
        {
            Assert.True(DateTimeOffset.UtcNow < deadline, $"timed out waiting for {what}");
            await Task.Delay(20);
        }
    }

    [Fact]
    public void Same_request_id_across_200_refreshes_keeps_command_registrations_constant()
    {
        var viewModel = new MainWindowViewModel(new InlineUiDispatcher());
        var snapshot = SnapshotWith(Approval("req-1"));

        Assert.Equal(BaseCommandCount, viewModel.RegisteredCommandCount);

        viewModel.ApplySnapshotForTests(snapshot);
        var row = viewModel.Approvals.Single();
        for (var i = 1; i < 200; i++)
        {
            viewModel.ApplySnapshotForTests(snapshot);
            Assert.Same(row, viewModel.Approvals.Single()); // stable row, updated in place
        }

        // Exactly the row's two commands registered, no accumulation: the
        // row is REUSED per request id, never rebuilt per refresh.
        Assert.Equal(BaseCommandCount + 2, viewModel.RegisteredCommandCount);
        Assert.Equal(["req-1"], viewModel.PendingApprovalRequestIdsForTests);

        // The row renders the typed facts from the snapshot.
        Assert.Equal("shell.exec · req-1", row.Title);
        Assert.Contains("进程执行", row.RiskLine);
        Assert.Contains("cargo test --workspace", row.TargetLine);
    }

    [Fact]
    public void Removing_an_approval_releases_its_commands()
    {
        var viewModel = new MainWindowViewModel(new InlineUiDispatcher());

        viewModel.ApplySnapshotForTests(SnapshotWith(Approval("req-1"), Approval("req-2")));
        Assert.Equal(BaseCommandCount + 4, viewModel.RegisteredCommandCount);
        Assert.Equal(2, viewModel.Approvals.Count);

        // One of the two is answered server-side and vanishes from the
        // snapshot: its row AND its two command registrations go with it.
        viewModel.ApplySnapshotForTests(SnapshotWith(Approval("req-2")));
        Assert.Equal(BaseCommandCount + 2, viewModel.RegisteredCommandCount);
        Assert.Equal(["req-2"], viewModel.PendingApprovalRequestIdsForTests);

        viewModel.ApplySnapshotForTests(SnapshotWith());
        Assert.Equal(BaseCommandCount, viewModel.RegisteredCommandCount);
        Assert.Empty(viewModel.Approvals);
    }

    [Fact]
    public async Task Stale_connection_era_refresh_results_are_dropped()
    {
        var viewModel = new MainWindowViewModel(new InlineUiDispatcher());
        var stub = new StubConnection();
        await viewModel.ConnectForTestsAsync(stub);

        var gate = new TaskCompletionSource<WorkSnapshotResponse>(TaskCreationOptions.RunContinuationsAsynchronously);
        stub.SnapshotHandler = _ => gate.Task;
        var refresh = viewModel.RefreshOnceForTestsAsync();

        // The connection era moves on while the refresh is in flight.
        await viewModel.DisconnectAsync();
        Assert.Equal(BaseCommandCount, viewModel.RegisteredCommandCount);

        gate.SetResult(SnapshotWith(Approval("late-req")));
        await refresh;

        // The stale-era result was dropped, never applied: no row, no
        // registrations, no watermark from a connection that is gone.
        Assert.Empty(viewModel.Approvals);
        Assert.Equal(BaseCommandCount, viewModel.RegisteredCommandCount);
        Assert.Equal(0ul, viewModel.Watermark);

        // A current-era result applies normally (the veto is not sticky).
        await viewModel.ConnectForTestsAsync(new StubConnection());
        viewModel.ApplySnapshotForTests(SnapshotWith(Approval("live-req")));
        Assert.Single(viewModel.Approvals);
        Assert.Equal(BaseCommandCount + 2, viewModel.RegisteredCommandCount);

        await viewModel.DisposeAsync();
    }

    [Fact]
    public void Output_area_keeps_both_the_line_and_byte_bounds()
    {
        var viewModel = new MainWindowViewModel(new InlineUiDispatcher());

        // Line bound: more appends than the retained line cap.
        for (var i = 0; i < 600; i++)
        {
            viewModel.AppendOutputForTests($"行 {i}: bounded output drill");
        }
        var lineCount = viewModel.OutputText.Split('\n').Length;
        Assert.True(lineCount <= MainWindowViewModel.MaxOutputLines,
            $"output exceeded the line bound: {lineCount}");
        Assert.Contains("行 599", viewModel.OutputText); // newest survives

        // Byte bound: a few huge lines must shed OLDEST lines whole.
        var longLine = new string('x', 20_000);
        for (var i = 0; i < 10; i++)
        {
            viewModel.AppendOutputForTests($"{longLine} #{i}");
        }
        var bytes = Encoding.UTF8.GetByteCount(viewModel.OutputText);
        Assert.True(bytes <= MainWindowViewModel.MaxOutputBytes, $"output exceeded the byte bound: {bytes}");
        Assert.Contains("#9", viewModel.OutputText); // newest survives
    }

    [Fact]
    public async Task Event_pump_renders_terminal_facts_and_drives_a_coalesced_refresh()
    {
        var viewModel = new MainWindowViewModel(new InlineUiDispatcher());
        var stub = new StubConnection();
        var snapshotsServed = 0;
        stub.SnapshotHandler = _ =>
        {
            snapshotsServed++;
            return Task.FromResult(SnapshotWith());
        };
        await viewModel.ConnectForTestsAsync(stub);
        Assert.Equal(0ul, viewModel.Watermark);

        Assert.True(stub.EventWriter.TryWrite(Notification("task_completed", 7)));
        await WaitUntilAsync(() => viewModel.OutputText.Contains("任务到达终态"), "terminal fact line");
        // The durable event also drove a snapshot refresh (watermark applied
        // from the fresh snapshot, not inferred from the event text).
        await WaitUntilAsync(() => snapshotsServed > 0 && viewModel.Watermark == 41ul, "event-driven refresh");

        // Window close: one path cancels the pump and releases the session.
        await viewModel.DisposeAsync();
        Assert.False(stub.IsConnected);
        await viewModel.DisposeAsync(); // idempotent
    }

    private static WorkEventNotification Notification(string eventType, ulong seq) => new()
    {
        Envelope = new RuntimeEventEnvelope
        {
            RunId = "00000000-0000-4000-8000-000000000001",
            Seq = seq,
            TimestampMs = seq,
            Event = System.Text.Json.JsonSerializer.Deserialize<System.Text.Json.JsonElement>(
                $"{{\"type\":\"{eventType}\"}}"),
        },
    };
}
