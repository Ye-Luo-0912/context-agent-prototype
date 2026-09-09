using System.Text.Json;
using System.Threading.Channels;

using FocusAgent.Desktop.Infrastructure;
using FocusAgent.Desktop.ViewModels;

using Xunit;

namespace FocusAgent.Client.Tests;

/// <summary>
/// R07 + R14 regression drills on the desktop workbench view model.
///
/// R07: an outstanding submit keeps its idempotency key while its receipt is
/// pending AND after a timed-out/timed-out-in-flight failure — only a receipt
/// or a snapshot fact for that exact key resolves it. A same-goal retry must
/// re-enter on the SAME client_request_id.
///
/// R14: the event pump hands work to the UI through an explicit, bounded
/// single-flight drain; a slow/paused dispatcher accumulates at most one
/// pending callback plus a source-capped text buffer, never one callback per
/// arrival.
///
/// These are focused state-logic drills — no full GUI, no real host, no
/// execution semantics.
/// </summary>
public class WorkbenchBackpressureAndIdempotencyTests
{
    private static readonly string RunId = "00000000-0000-4000-8000-000000000001";

    private static WorkSnapshotResponse EmptySnapshot() => new()
    {
        RunStarted = false,
        RunCompleted = false,
        Tasks = [],
        PendingApprovals = [],
        ResyncRequired = false,
    };

    /// <summary>A pausing dispatcher: posts are recorded but never run, so a
    /// test can hold the UI frozen and observe exactly how much work is
    /// outstanding at the source.</summary>
    private sealed class QueuingUiDispatcher : IUiDispatcher
    {
        public readonly List<Action> Queued = new();

        public int Posted => Queued.Count;

        public void Post(Action action) => Queued.Add(action);

        public IDisposable StartPeriodicTimer(TimeSpan interval, Action tick) => new NullHandle();

        private sealed class NullHandle : IDisposable
        {
            public void Dispose()
            {
            }
        }
    }

    /// <summary>Drill connection with a writable event stream and a
    /// snapshot handler, so the pump can be fed deterministically.</summary>
    private sealed class WritableEventConnection : IAgentConnection
    {
        private readonly Channel<WorkEventNotification> _events =
            Channel.CreateBounded<WorkEventNotification>(64);

        public ChannelWriter<WorkEventNotification> EventWriter => _events.Writer;

        public bool IsConnected { get; private set; } = true;

        public ChannelReader<WorkEventNotification> Events => _events.Reader;

        public Task<WorkSnapshotResponse> SnapshotAsync(CancellationToken cancellationToken = default) =>
            Task.FromResult(EmptySnapshot());

        public Task<WorkSubmitResponse> SubmitWorkAsync(string goal, string clientRequestId, CancellationToken cancellationToken = default) =>
            throw new NotSupportedException("drills do not submit over this connection");

        public Task<WorkContinueResponse> ContinueAsync(CancellationToken cancellationToken = default) =>
            throw new NotSupportedException("drills do not continue");

        public Task<WorkCancelResponse> CancelCurrentTurnAsync(CancellationToken cancellationToken = default) =>
            throw new NotSupportedException("drills do not cancel");

        public Task<WorkSubscribeResponse> SubscribeAsync(ulong? replayAfterSeq = null, CancellationToken cancellationToken = default) =>
            throw new NotSupportedException("drills do not subscribe");

        public Task<ApprovalRespondResponse> RespondApprovalAsync(string requestId, ApprovalDecision decision, CancellationToken cancellationToken = default) =>
            throw new NotSupportedException("drills do not answer approvals");

        public Task<WorkTaskDetailResponse> TaskDetailAsync(string taskId, CancellationToken cancellationToken = default) =>
            throw new NotSupportedException("drills do not read task detail");

        public Task<WorkChangesResponse> ReadChangesAsync(int? limit = null, string? afterTx = null, CancellationToken cancellationToken = default) =>
            throw new NotSupportedException("drills do not read changes");

        public Task<WorkArtifactResponse> ReadArtifactAsync(string reference, uint? maxBytes = null, CancellationToken cancellationToken = default) =>
            throw new NotSupportedException("drills do not read artifacts");

        public Task<WorkContextResponse> ReadContextAsync(uint? limit = null, CancellationToken cancellationToken = default) =>
            throw new NotSupportedException("drills do not read context");

        public ValueTask DisposeAsync()
        {
            IsConnected = false;
            _events.Writer.TryComplete();
            return ValueTask.CompletedTask;
        }
    }

    /// <summary>Drill connection that records every submit's
    /// <c>client_request_id</c> and lets a test control the in-flight result
    /// (open / faulted), so the idempotency-key lifecycle is observable.</summary>
    private sealed class RecordingSubmitConnection : IAgentConnection
    {
        public readonly List<string> SubmittedIds = new();
        public readonly List<string> SubmittedGoals = new();

        public TaskCompletionSource<WorkSubmitResponse> CurrentSubmit { get; set; } =
            new(TaskCreationOptions.RunContinuationsAsynchronously);

        public bool IsConnected { get; private set; } = true;

        public ChannelReader<WorkEventNotification> Events { get; } =
            Channel.CreateUnbounded<WorkEventNotification>().Reader;

        public Task<WorkSnapshotResponse> SnapshotAsync(CancellationToken cancellationToken = default) =>
            Task.FromResult(EmptySnapshot());

        public Task<WorkSubmitResponse> SubmitWorkAsync(string goal, string clientRequestId, CancellationToken cancellationToken = default)
        {
            SubmittedIds.Add(clientRequestId);
            SubmittedGoals.Add(goal);
            return CurrentSubmit.Task;
        }

        public Task<WorkContinueResponse> ContinueAsync(CancellationToken cancellationToken = default) =>
            throw new NotSupportedException("drills do not continue");

        public Task<WorkCancelResponse> CancelCurrentTurnAsync(CancellationToken cancellationToken = default) =>
            throw new NotSupportedException("drills do not cancel");

        public Task<WorkSubscribeResponse> SubscribeAsync(ulong? replayAfterSeq = null, CancellationToken cancellationToken = default) =>
            throw new NotSupportedException("drills do not subscribe");

        public Task<ApprovalRespondResponse> RespondApprovalAsync(string requestId, ApprovalDecision decision, CancellationToken cancellationToken = default) =>
            throw new NotSupportedException("drills do not answer approvals");

        public Task<WorkTaskDetailResponse> TaskDetailAsync(string taskId, CancellationToken cancellationToken = default) =>
            throw new NotSupportedException("drills do not read task detail");

        public Task<WorkChangesResponse> ReadChangesAsync(int? limit = null, string? afterTx = null, CancellationToken cancellationToken = default) =>
            throw new NotSupportedException("drills do not read changes");

        public Task<WorkArtifactResponse> ReadArtifactAsync(string reference, uint? maxBytes = null, CancellationToken cancellationToken = default) =>
            throw new NotSupportedException("drills do not read artifacts");

        public Task<WorkContextResponse> ReadContextAsync(uint? limit = null, CancellationToken cancellationToken = default) =>
            throw new NotSupportedException("drills do not read context");

        public ValueTask DisposeAsync()
        {
            IsConnected = false;
            return ValueTask.CompletedTask;
        }
    }

    private static WorkEventNotification ModelDelta(string text, ulong seq) => new()
    {
        Envelope = new RuntimeEventEnvelope
        {
            RunId = RunId,
            Seq = seq,
            TimestampMs = seq,
            Event = JsonSerializer.Deserialize<JsonElement>(
                $"{{\"type\":\"model_delta\",\"delta\":{JsonSerializer.Serialize(text)}}}"),
        },
    };

    private static async Task WaitUntilAsync(Func<bool> condition, string what)
    {
        var deadline = DateTimeOffset.UtcNow + TimeSpan.FromSeconds(10);
        while (!condition())
        {
            Assert.True(DateTimeOffset.UtcNow < deadline, $"timed out waiting for {what}");
            await Task.Delay(20);
        }
    }

    /// <summary>R07: neither a concurrent snapshot during a PENDING submit nor
    /// a timed-out in-flight failure may clear the idempotency key — a retry
    /// of the same target must stay on the SAME admission identity.</summary>
    [Fact]
    public async Task Concurrent_snapshot_and_timeout_keep_the_outstanding_submit_key()
    {
        const string goal = "r07 同目标幂等重试";
        var viewModel = new MainWindowViewModel(new InlineUiDispatcher());
        var stub = new RecordingSubmitConnection();
        await viewModel.ConnectForTestsAsync(stub);
        try
        {
            // The submit is now PENDING (awaiting its receipt); the key has
            // been allocated and is held outstanding.
            viewModel.GoalInput = goal;
            var submit = viewModel.SubmitForTestsAsync();
            var id1 = Assert.Single(stub.SubmittedIds);
            Assert.Equal(id1, viewModel.OutstandingSubmitIdForTests);

            // R07 bug 1: a concurrent refresh while the submit is PENDING must
            // not clear the key (pre-fix, ApplySnapshot cleared it and the
            // retry became a NEW admission).
            viewModel.ApplySnapshotForTests(EmptySnapshot());
            Assert.Equal(id1, viewModel.OutstandingSubmitIdForTests);

            // R07 bug 2: a timed-out / faulted in-flight request leaves the
            // receipt UNKNOWN — the key stays outstanding (Pending -> Unknown
            // keeps k), resolved later by snapshot facts, never cleared here.
            stub.CurrentSubmit.SetException(new TaskCanceledException("receipt timeout after the frame was sent"));
            await submit;
            Assert.Equal(id1, viewModel.OutstandingSubmitIdForTests);

            // The retry of the SAME target re-enters on the SAME key (pre-fix
            // it picked a brand-new client_request_id => a second admission).
            stub.CurrentSubmit = new TaskCompletionSource<WorkSubmitResponse>(
                TaskCreationOptions.RunContinuationsAsynchronously);
            stub.CurrentSubmit.SetException(new TaskCanceledException("drill timeout"));
            viewModel.GoalInput = goal;
            await viewModel.SubmitForTestsAsync();
            Assert.Equal(new[] { id1, id1 }, stub.SubmittedIds);
            Assert.Equal([goal, goal], stub.SubmittedGoals);
        }
        finally
        {
            await viewModel.DisposeAsync();
        }
    }

    /// <summary>R14: with the UI frozen, the pump must not turn the bounded
    /// event source into an unbounded dispatcher backlog — at most ONE
    /// pending drain callback exists, and the buffered delta text is capped at
    /// the source by the output budget.</summary>
    [Fact]
    public async Task Paused_dispatcher_bounds_the_pending_ui_backlog_at_the_source()
    {
        var dispatcher = new QueuingUiDispatcher();
        var viewModel = new MainWindowViewModel(dispatcher);
        var stub = new WritableEventConnection();
        await viewModel.ConnectForTestsAsync(stub);

        var deltaText = new string('a', 4096); // > the coalescer's flush budget
        for (var i = 0; i < 200; i++)
        {
            await stub.EventWriter.WriteAsync(ModelDelta(deltaText, (ulong)i));
        }

        // The source cap trims pending delta text, and the single posted drain
        // is the ONLY pending UI callback — the frozen UI has rendered nothing.
        await WaitUntilAsync(() => viewModel.PendingUiDrainPostedForTests, "the bounded drain to be posted");
        await WaitUntilAsync(
            () => viewModel.PendingUiDeltaCharsForTests >= MainWindowViewModel.MaxOutputBytes,
            "the source-side text cap to be reached");

        Assert.Equal(1, dispatcher.Posted); // single-flight drain, not per event
        Assert.Equal(MainWindowViewModel.MaxOutputBytes, viewModel.PendingUiDeltaCharsForTests);
        Assert.Equal(string.Empty, viewModel.OutputText); // nothing rendered while paused

        await viewModel.DisposeAsync();
    }
}