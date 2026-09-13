using System.Threading.Channels;

using FocusAgent.Desktop.Infrastructure;
using FocusAgent.Desktop.ViewModels;

using Xunit;

namespace FocusAgent.Client.Tests;

/// <summary>
/// GUI-3 second-half drills: the workbench's continue and cancel surfaces.
///
/// The wire contract gives continue no task binding (it continues the run's
/// ACTIVE task), so the workbench must say WHICH task it continued and flag
/// a mismatch with the operator's selection instead of implying the
/// selection was honored. The cancel surface must show "requested" the
/// moment the request is sent, upgrade ONLY on a typed confirmation, keep
/// the side-effect warning when the outcome is undetermined, and let a
/// trusted snapshot re-derive the state — a sent request is never rendered
/// as a stop.
///
/// Focused state-logic drills — no full GUI, no real host.
/// </summary>
public class WorkbenchContinueCancelTests
{
    private const string TaskA = "00000000-0000-4000-8000-0000000000a1";
    private const string TaskB = "00000000-0000-4000-8000-0000000000b2";
    private static readonly string RunId = "00000000-0000-4000-8000-000000000001";

    private static WorkSnapshotResponse SnapshotWithTasks(params string[] taskIds) => new()
    {
        RunStarted = true,
        RunCompleted = false,
        Tasks = taskIds.Select(id => new TaskSnapshotEntry
        {
            TaskId = id,
            Goal = $"任务 {id}",
            Status = TaskSnapshotStatus.Active,
            AnchorRevision = 1,
            ToolRequirementRevision = 0,
            ToolRequirementCount = 0,
        }).ToArray(),
        PendingApprovals = [],
        ResyncRequired = false,
    };

    /// <summary>Drill connection with programmable continue/cancel results,
    /// including a cancel that can be held in flight across a snapshot.</summary>
    private sealed class ContinueCancelStub : IAgentConnection
    {
        public Task<WorkTaskCompletionResponse> TaskCompletionAsync(string taskId, CancellationToken cancellationToken = default) =>
            Task.FromResult(new WorkTaskCompletionResponse { TaskId = taskId, Fact = new WorkCompletionFactBeyondJournalWindow() });

        public Func<CancellationToken, Task<WorkContinueResponse>> ContinueHandler { get; set; } =
            _ => Task.FromException<WorkContinueResponse>(new NotSupportedException("drill sets ContinueHandler"));

        public Func<CancellationToken, Task<WorkCancelResponse>> CancelHandler { get; set; } =
            _ => Task.FromException<WorkCancelResponse>(new NotSupportedException("drill sets CancelHandler"));

        public bool IsConnected { get; private set; } = true;

        public ChannelReader<WorkEventNotification> Events { get; } =
            Channel.CreateUnbounded<WorkEventNotification>().Reader;

        public Task<WorkSnapshotResponse> SnapshotAsync(CancellationToken cancellationToken = default) =>
            Task.FromResult(SnapshotWithTasks());

        public Task<WorkSubmitResponse> SubmitWorkAsync(string goal, string clientRequestId, CancellationToken cancellationToken = default) =>
            throw new NotSupportedException("drills do not submit");

        public Task<WorkContinueResponse> ContinueAsync(CancellationToken cancellationToken = default) =>
            ContinueHandler(cancellationToken);

        public Task<WorkCancelResponse> CancelCurrentTurnAsync(CancellationToken cancellationToken = default) =>
            CancelHandler(cancellationToken);

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

    private static WorkContinueResponse Continued(string taskId) => new() { TaskId = taskId };

    private static WorkCancelResponse CancelledAck() => new()
    {
        Ack = new TurnCancelAck
        {
            Status = TurnCancelAckStatus.Cancelled,
            TurnId = "00000000-0000-4000-8000-0000000000c3",
            TaskId = TaskB,
            CancelledGeneration = 2,
            EffectiveGeneration = 3,
        },
    };

    private static WorkCancelResponse NoActiveTurnAck() => new()
    {
        Ack = new TurnCancelAck { Status = TurnCancelAckStatus.NoActiveTurn },
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

    [Fact]
    public async Task Continue_reports_which_task_it_continued_and_flags_a_selection_mismatch()
    {
        var viewModel = new MainWindowViewModel(new InlineUiDispatcher());
        var stub = new ContinueCancelStub
        {
            ContinueHandler = _ => Task.FromResult(Continued(TaskB)),
        };
        await viewModel.ConnectForTestsAsync(stub);
        try
        {
            // The operator selected task A; the run's active task is B and
            // the wire's continue carries no binding, so the workbench must
            // name what it continued and flag the difference — never imply
            // the selection drove it.
            viewModel.ApplySnapshotForTests(SnapshotWithTasks(TaskA, TaskB));
            viewModel.SelectedTask = viewModel.Tasks.First(t => t.TaskId == TaskA);

            await viewModel.ContinueForTestsAsync();
            Assert.Contains(TaskB, viewModel.LogText);
            Assert.Contains(TaskA, viewModel.LogText);
            Assert.Contains("不是当前选中的任务", viewModel.LogText);

            // Without a selection the receipt is reported plainly, with no
            // mismatch caveat.
            viewModel.SelectedTask = null;
            await viewModel.ContinueForTestsAsync();
            var lines = viewModel.LogText.Split('\n');
            var last = lines.Last(line => line.Contains("已继续"));
            Assert.DoesNotContain("不是当前选中的任务", last);
        }
        finally
        {
            await viewModel.DisposeAsync();
        }
    }

    [Fact]
    public async Task Cancel_shows_requested_until_a_typed_confirmation()
    {
        var viewModel = new MainWindowViewModel(new InlineUiDispatcher());
        var gate = new TaskCompletionSource<WorkCancelResponse>(TaskCreationOptions.RunContinuationsAsynchronously);
        var stub = new ContinueCancelStub { CancelHandler = _ => gate.Task };
        await viewModel.ConnectForTestsAsync(stub);
        try
        {
            // The moment the request is SENT the surface says so — a sent
            // request is not a stop.
            var cancel = viewModel.CancelForTestsAsync();
            await WaitUntilAsync(
                () => viewModel.CancelPhase == CancelRequestPhase.Requested,
                "the cancel request to be shown as requested");
            Assert.Contains("已发出", viewModel.CancelStateText);
            Assert.Contains("等待", viewModel.CancelStateText);

            // A snapshot while the request is still in flight must NOT
            // pretend the outcome is known.
            viewModel.ApplySnapshotForTests(SnapshotWithTasks(TaskB));
            Assert.Equal(CancelRequestPhase.Requested, viewModel.CancelPhase);

            // Only the typed ack upgrades the state to a confirmed stop.
            gate.SetResult(CancelledAck());
            await cancel;
            Assert.Equal(CancelRequestPhase.Cancelled, viewModel.CancelPhase);
            Assert.Contains("已确认取消", viewModel.CancelStateText);
        }
        finally
        {
            await viewModel.DisposeAsync();
        }
    }

    [Fact]
    public async Task Cancel_without_an_active_turn_is_a_fact_not_a_failure()
    {
        var viewModel = new MainWindowViewModel(new InlineUiDispatcher());
        var stub = new ContinueCancelStub { CancelHandler = _ => Task.FromResult(NoActiveTurnAck()) };
        await viewModel.ConnectForTestsAsync(stub);
        try
        {
            await viewModel.CancelForTestsAsync();
            Assert.Equal(CancelRequestPhase.NoActiveTurn, viewModel.CancelPhase);
            Assert.Contains("没有活动轮次", viewModel.CancelStateText);
        }
        finally
        {
            await viewModel.DisposeAsync();
        }
    }

    [Fact]
    public async Task Cancel_unknown_outcome_keeps_the_warning_until_a_snapshot_rederives_it()
    {
        var viewModel = new MainWindowViewModel(new InlineUiDispatcher());
        var stub = new ContinueCancelStub
        {
            CancelHandler = _ => Task.FromException<WorkCancelResponse>(
                new TaskCanceledException("receipt lost after the frame was sent")),
        };
        await viewModel.ConnectForTestsAsync(stub);
        try
        {
            // The outcome is undetermined: the turn may or may not have
            // stopped. The surface keeps the warning — it never claims a
            // stop, and nothing is re-sent automatically.
            await viewModel.CancelForTestsAsync();
            Assert.Equal(CancelRequestPhase.Unknown, viewModel.CancelPhase);
            Assert.Contains("不可确定", viewModel.CancelStateText);

            // The next trusted snapshot re-derives the state from typed
            // facts and says so.
            viewModel.ApplySnapshotForTests(SnapshotWithTasks(TaskB));
            Assert.Equal(CancelRequestPhase.Idle, viewModel.CancelPhase);
            Assert.Contains("以快照为准", viewModel.CancelStateText);
        }
        finally
        {
            await viewModel.DisposeAsync();
        }
    }

    [Fact]
    public async Task Disconnect_retires_an_open_cancel_request_state()
    {
        var viewModel = new MainWindowViewModel(new InlineUiDispatcher());
        var gate = new TaskCompletionSource<WorkCancelResponse>(TaskCreationOptions.RunContinuationsAsynchronously);
        var stub = new ContinueCancelStub { CancelHandler = _ => gate.Task };
        await viewModel.ConnectForTestsAsync(stub);
        try
        {
            var cancel = viewModel.CancelForTestsAsync();
            await WaitUntilAsync(
                () => viewModel.CancelPhase == CancelRequestPhase.Requested,
                "the cancel request to be shown as requested");

            await viewModel.DisconnectAsync();
            Assert.Equal(CancelRequestPhase.Idle, viewModel.CancelPhase);
            Assert.Contains("已断开", viewModel.CancelStateText);

            // The in-flight cancel settles against the dead era; it must not
            // resurrect a phase on the disconnected view model.
            gate.SetResult(CancelledAck());
            await cancel;
            Assert.Equal(CancelRequestPhase.Idle, viewModel.CancelPhase);
        }
        finally
        {
            await viewModel.DisposeAsync();
        }
    }
}
