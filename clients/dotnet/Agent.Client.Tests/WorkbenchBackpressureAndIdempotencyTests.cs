using System.Text.Json;
using System.Threading.Channels;

using FocusAgent.Desktop.Infrastructure;
using FocusAgent.Desktop.ViewModels;

using Xunit;

namespace FocusAgent.Client.Tests;

/// <summary>
/// R07 + R14 + GUI-3/F06 regression drills on the desktop workbench view
/// model.
///
/// R07: an outstanding submit keeps its idempotency key while its receipt is
/// pending AND after a timed-out/timed-out-in-flight failure — only a receipt
/// or a ledger fact for that exact key resolves it. A same-goal retry must
/// re-enter on the SAME client_request_id.
///
/// GUI-3/F06: the outstanding UNKNOWN submit is resolved by the
/// exact-request ledger query (client_request_id + payload digest), never by
/// matching the goal text against a snapshot; every disposition is rendered
/// as the fact it is, and Unknown/Expired keep the key and claim nothing.
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

    private static WorkSnapshotResponse SnapshotWithTask(string goal) => new()
    {
        RunStarted = false,
        RunCompleted = false,
        Tasks =
        [
            new TaskSnapshotEntry
            {
                TaskId = "00000000-0000-4000-8000-00000000006f",
                Goal = goal,
                Status = TaskSnapshotStatus.Active,
                AnchorRevision = 1,
                ToolRequirementRevision = 0,
                ToolRequirementCount = 0,
            },
        ],
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
        public Task<WorkTaskCompletionResponse> TaskCompletionAsync(string taskId, CancellationToken cancellationToken = default) =>
            Task.FromResult(new WorkTaskCompletionResponse { TaskId = taskId, Fact = new WorkCompletionFactBeyondJournalWindow() });

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

        public Task<WorkArtifactResponse> ReadArtifactAsync(string reference, uint? maxBytes = null, ulong? offset = null, CancellationToken cancellationToken = default) =>
            throw new NotSupportedException("drills do not read artifacts");

        public Task<WorkContextResponse> ReadContextAsync(uint? limit = null, CancellationToken cancellationToken = default) =>
            throw new NotSupportedException("drills do not read context");

        public Task<WorkSubmitResultResponse> SubmitResultAsync(string clientRequestId, string? payloadDigest = null, CancellationToken cancellationToken = default) =>
            throw new NotSupportedException("drills do not query the submit ledger");

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
        public Task<WorkTaskCompletionResponse> TaskCompletionAsync(string taskId, CancellationToken cancellationToken = default) =>
            Task.FromResult(new WorkTaskCompletionResponse { TaskId = taskId, Fact = new WorkCompletionFactBeyondJournalWindow() });

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

        public Task<WorkArtifactResponse> ReadArtifactAsync(string reference, uint? maxBytes = null, ulong? offset = null, CancellationToken cancellationToken = default) =>
            throw new NotSupportedException("drills do not read artifacts");

        public Task<WorkContextResponse> ReadContextAsync(uint? limit = null, CancellationToken cancellationToken = default) =>
            throw new NotSupportedException("drills do not read context");

        public Task<WorkSubmitResultResponse> SubmitResultAsync(string clientRequestId, string? payloadDigest = null, CancellationToken cancellationToken = default) =>
            Task.FromResult(new WorkSubmitResultResponse
            {
                RunId = RunId,
                ClientRequestId = clientRequestId,
                Disposition = WorkSubmitResultDisposition.Unknown,
            });

        public ValueTask DisposeAsync()
        {
            IsConnected = false;
            return ValueTask.CompletedTask;
        }
    }

    /// <summary>GUI-3/F06 drill connection: every submit's receipt is LOST
    /// (the outcome is unknown), and the exact-request ledger query is
    /// programmable. Queries are recorded with the id and digest they asked
    /// about, and the answer models a correct server by echoing the queried
    /// id — so the resolution path's identity handling is observable.</summary>
    private sealed class LostReceiptConnection : IAgentConnection
    {
        public Task<WorkTaskCompletionResponse> TaskCompletionAsync(string taskId, CancellationToken cancellationToken = default) =>
            Task.FromResult(new WorkTaskCompletionResponse { TaskId = taskId, Fact = new WorkCompletionFactBeyondJournalWindow() });

        public readonly List<string> SubmittedIds = new();
        public readonly List<(string Id, string? Digest)> LedgerQueries = new();

        /// <summary>(disposition, taskId, acceptedPayloadDigest) answered for
        /// a query; the default knows nothing, like an evicted ledger.</summary>
        public Func<string, (WorkSubmitResultDisposition Disposition, string? TaskId, string? AcceptedDigest)> LedgerAnswer { get; set; } =
            _ => (WorkSubmitResultDisposition.Unknown, null, null);

        public bool IsConnected { get; private set; } = true;

        public ChannelReader<WorkEventNotification> Events { get; } =
            Channel.CreateUnbounded<WorkEventNotification>().Reader;

        public Task<WorkSnapshotResponse> SnapshotAsync(CancellationToken cancellationToken = default) =>
            Task.FromResult(EmptySnapshot());

        public Task<WorkSubmitResponse> SubmitWorkAsync(string goal, string clientRequestId, CancellationToken cancellationToken = default)
        {
            SubmittedIds.Add(clientRequestId);
            return Task.FromException<WorkSubmitResponse>(
                new TaskCanceledException("receipt lost after the frame was sent"));
        }

        public Task<WorkSubmitResultResponse> SubmitResultAsync(string clientRequestId, string? payloadDigest = null, CancellationToken cancellationToken = default)
        {
            LedgerQueries.Add((clientRequestId, payloadDigest));
            var (disposition, taskId, acceptedDigest) = LedgerAnswer(clientRequestId);
            return Task.FromResult(new WorkSubmitResultResponse
            {
                RunId = RunId,
                ClientRequestId = clientRequestId,
                Disposition = disposition,
                TaskId = taskId,
                AcceptedPayloadDigest = acceptedDigest,
            });
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

        public Task<WorkArtifactResponse> ReadArtifactAsync(string reference, uint? maxBytes = null, ulong? offset = null, CancellationToken cancellationToken = default) =>
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

    /// <summary>GUI-3/F06: an outstanding UNKNOWN submit is resolved by
    /// asking the run's ledger about the EXACT client_request_id with the
    /// payload digest of the caller's own goal — not by matching the goal
    /// text in a snapshot. The ledger's Accepted answer resolves the unknown,
    /// names the task, and releases the key. (Pre-fix, the workbench never
    /// queried the ledger at all: with an empty snapshot it declared the
    /// result unverifiable even though the ledger could testify.)</summary>
    [Fact]
    public async Task An_unknown_submit_is_resolved_by_the_exact_request_query_not_goal_matching()
    {
        const string goal = "f06 精确账本核对（同名任务不参与判定）";
        var viewModel = new MainWindowViewModel(new InlineUiDispatcher());
        var stub = new LostReceiptConnection
        {
            LedgerAnswer = _ => (
                WorkSubmitResultDisposition.Accepted,
                "00000000-0000-4000-8000-00000000007a",
                null),
        };
        await viewModel.ConnectForTestsAsync(stub);
        try
        {
            viewModel.GoalInput = goal;
            await viewModel.SubmitForTestsAsync();

            // The ledger was asked about THIS id with THIS goal's digest —
            // identity, not prose.
            var id = Assert.Single(stub.SubmittedIds);
            await WaitUntilAsync(() => stub.LedgerQueries.Count == 1, "the ledger query");
            Assert.Equal((id, SubmitPayloadDigest.Compute(goal)), stub.LedgerQueries[0]);

            // The Accepted answer resolves the unknown as a ledger fact.
            await WaitUntilAsync(
                () => viewModel.OutstandingSubmitIdForTests is null,
                "the ledger's acceptance to release the outstanding key");
            Assert.Contains("账本", viewModel.LogText);
            Assert.Contains("00000000-0000-4000-8000-00000000007a", viewModel.LogText);
        }
        finally
        {
            await viewModel.DisposeAsync();
        }
    }

    /// <summary>GUI-3/F06: a same-goal task visible in the snapshot is NOT
    /// evidence that this client_request_id was admitted — an old or renamed
    /// task can carry the same goal, and a bounded list proves nothing either
    /// way. While the ledger cannot testify (Unknown), the unknown stays
    /// unknown: the key survives so a same-goal retry remains an idempotent
    /// re-read, and the output claims no resolution. (Pre-fix, this exact
    /// snapshot cleared the key and announced the unknown as resolved.)</summary>
    [Fact]
    public async Task A_same_goal_task_in_the_snapshot_does_not_prove_admission_while_the_ledger_cannot_testify()
    {
        const string goal = "f06 同名任务不能证明受理";
        var viewModel = new MainWindowViewModel(new InlineUiDispatcher());
        var stub = new LostReceiptConnection(); // the ledger knows nothing
        await viewModel.ConnectForTestsAsync(stub);
        try
        {
            viewModel.GoalInput = goal;
            await viewModel.SubmitForTestsAsync();
            var id = Assert.Single(stub.SubmittedIds);
            await WaitUntilAsync(() => stub.LedgerQueries.Count == 1, "the ledger query");

            // The snapshot shows a task with the SAME goal — tempting the
            // goal-match heuristic.
            viewModel.ApplySnapshotForTests(SnapshotWithTask(goal));

            Assert.Equal(id, viewModel.OutstandingSubmitIdForTests);
            Assert.DoesNotContain("解除", viewModel.LogText);
            Assert.Contains("无法证明", viewModel.LogText);
        }
        finally
        {
            await viewModel.DisposeAsync();
        }
    }

    /// <summary>GUI-3/F06: a KnownRejected answer is a definitive fact — the
    /// id was admitted for a DIFFERENT payload, so this request was never
    /// admitted and the id is dead. The UI names the conflict (via the
    /// recorded digest, without shipping any goal text) and releases the id:
    /// keeping it would chain every future retry to an identity that can
    /// never succeed.</summary>
    [Fact]
    public async Task A_known_rejected_conflict_is_reported_and_the_dead_id_is_released()
    {
        const string goal = "f06 同 id 不同内容冲突";
        var acceptedDigest = new string('a', 64);
        var viewModel = new MainWindowViewModel(new InlineUiDispatcher());
        var stub = new LostReceiptConnection
        {
            LedgerAnswer = _ => (
                WorkSubmitResultDisposition.KnownRejected,
                "00000000-0000-4000-8000-00000000007b",
                acceptedDigest),
        };
        await viewModel.ConnectForTestsAsync(stub);
        try
        {
            viewModel.GoalInput = goal;
            await viewModel.SubmitForTestsAsync();

            await WaitUntilAsync(
                () => viewModel.OutstandingSubmitIdForTests is null,
                "the conflict verdict to release the dead id");
            Assert.Contains("从未被受理", viewModel.LogText);
            Assert.Contains(acceptedDigest[..16], viewModel.LogText);
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
    public async Task A_retired_eras_buffered_delta_never_reaches_the_new_panel()
    {
        // F07: the pump buffers delta text and pending events at the SOURCE and
        // hands them to the UI through one posted drain. If the connection is
        // retired before that drain runs, the buffered work belongs to the dead
        // era — flushing its delta text into the output panel would paint the
        // NEW session with the OLD session's prose. HandleEvent already drops a
        // retired era's events, but the coalesced delta text was appended with
        // no era check at all, so it leaked.
        var dispatcher = new QueuingUiDispatcher();
        var viewModel = new MainWindowViewModel(dispatcher);
        var oldStub = new WritableEventConnection();
        await viewModel.ConnectForTestsAsync(oldStub);

        // Old era streams a delta; the frozen UI queues it (no render yet).
        await oldStub.EventWriter.WriteAsync(ModelDelta("STALE-ERA-PROSE", 0));
        await WaitUntilAsync(() => viewModel.PendingUiDrainPostedForTests, "the drain to be posted");
        Assert.Equal(string.Empty, viewModel.OutputText); // still buffered, not rendered

        // The connection is retired before the queued drain runs.
        await viewModel.DisconnectAsync();

        // Now let the queued callbacks run — as a real UI thread eventually would.
        foreach (var action in dispatcher.Queued)
        {
            action();
        }

        Assert.DoesNotContain("STALE-ERA-PROSE", viewModel.OutputText);

        await viewModel.DisposeAsync();
    }

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