using System.Threading.Channels;
using FocusAgent.Client;

namespace FocusAgent.Desktop.Fixture;

/// <summary>
/// 布局夹具连接：只回放同契约 DTO 的固定快照与响应，用于窗口布局与绑定
/// 开发。它不是第二套执行器：不产出任何任务语义，也不用于协议一致性
/// （那由 Agent.Client.Tests 对共享 fixtures 负责）。P1/P2 可用后，真实
/// 宿主连接走 AgentConnection，本类仅保留给无宿主环境的布局预览。
/// </summary>
public sealed class FixtureAgentConnection : IAgentConnection
{
    private readonly List<TaskSnapshotEntry> _tasks =
    [
        new()
        {
            TaskId = "00000000-0000-4000-8000-000000000022",
            Goal = "修复 supervision 台账的清理确认（布局夹具示例）",
            Status = TaskSnapshotStatus.Active,
            AnchorRevision = 3,
            ToolRequirementRevision = 1,
            ToolRequirementCount = 2,
        },
        new()
        {
            TaskId = "00000000-0000-4000-8000-000000000023",
            Goal = "整理 NEXT_TASKS 队列（布局夹具示例）",
            Status = TaskSnapshotStatus.Suspended,
            AnchorRevision = 5,
            ToolRequirementRevision = 0,
            ToolRequirementCount = 0,
        },
    ];

    private readonly List<PendingApprovalSnapshot> _approvals =
    [
        new()
        {
            RequestId = "approval-fixture-1",
            CallName = "fs.write",
            Risk = ApprovalRisk.WorkspaceWrite,
            TargetSummary = "布局预览示例路径：docs/fixture.md（非真实任务事实）",
        },
    ];

    public bool IsConnected { get; private set; } = true;

    /// <summary>布局夹具不产出事件：一条已完结的空流即可。</summary>
    public ChannelReader<WorkEventNotification> Events { get; } = CreateEmptyEvents();

    private static ChannelReader<WorkEventNotification> CreateEmptyEvents()
    {
        var channel = Channel.CreateBounded<WorkEventNotification>(1);
        channel.Writer.TryComplete();
        return channel.Reader;
    }

    public Task<WorkSubmitResponse> SubmitWorkAsync(string goal, string clientRequestId, CancellationToken cancellationToken = default)
    {
        var taskId = Guid.NewGuid().ToString("D");
        _tasks.Insert(0, new TaskSnapshotEntry
        {
            TaskId = taskId,
            Goal = goal,
            Status = TaskSnapshotStatus.Active,
            AnchorRevision = 1,
            ToolRequirementRevision = 0,
            ToolRequirementCount = 0,
        });
        return Task.FromResult(new WorkSubmitResponse
        {
            Disposition = WorkSubmitDisposition.Accepted,
            TaskId = taskId,
        });
    }

    public Task<WorkContinueResponse> ContinueAsync(CancellationToken cancellationToken = default) =>
        Task.FromResult(new WorkContinueResponse { TaskId = _tasks[0].TaskId });

    public Task<WorkCancelResponse> CancelCurrentTurnAsync(CancellationToken cancellationToken = default) =>
        Task.FromResult(new WorkCancelResponse
        {
            Ack = new TurnCancelAck
            {
                Status = TurnCancelAckStatus.Cancelled,
                TurnId = Guid.NewGuid().ToString("D"),
                TaskId = _tasks[0].TaskId,
                CancelledGeneration = 2,
                EffectiveGeneration = 3,
            },
        });

    public Task<WorkSnapshotResponse> SnapshotAsync(CancellationToken cancellationToken = default) =>
        Task.FromResult(new WorkSnapshotResponse
        {
            RunStarted = true,
            RunCompleted = false,
            Watermark = 41,
            Focus = new FocusSnapshot
            {
                TaskId = _tasks[0].TaskId,
                Goal = _tasks[0].Goal,
                AnchorRevision = _tasks[0].AnchorRevision,
            },
            Tasks = _tasks.ToArray(),
            PendingApprovals = _approvals.ToArray(),
            ResyncRequired = false,
        });

    public Task<WorkSubscribeResponse> SubscribeAsync(ulong? replayAfterSeq = null, CancellationToken cancellationToken = default) =>
        Task.FromResult(new WorkSubscribeResponse { Watermark = 41, ResyncRequired = false });

    public Task<ApprovalRespondResponse> RespondApprovalAsync(
        string requestId, ApprovalDecision decision, CancellationToken cancellationToken = default)
    {
        _approvals.RemoveAll(a => a.RequestId == requestId);
        return Task.FromResult(new ApprovalRespondResponse { Outcome = ApprovalRespondOutcome.Delivered });
    }

    // -----------------------------------------------------------------------
    // B3 read-only routes: the layout fixture has no real journal/artifact/
    // context truths, so it only answers the task card from fixture tasks and
    // empty listings; wired-up reads belong to the C line's real host hookup.
    // -----------------------------------------------------------------------

    public Task<WorkTaskDetailResponse> TaskDetailAsync(string taskId, CancellationToken cancellationToken = default)
    {
        var task = _tasks.FirstOrDefault(t => t.TaskId == taskId);
        if (task is null)
        {
            return Task.FromException<WorkTaskDetailResponse>(
                new AgentContractViolationException("work.task_detail.task_id", "task not found in fixture"));
        }
        return Task.FromResult(new WorkTaskDetailResponse
        {
            TaskId = task.TaskId,
            Goal = task.Goal,
            Status = task.Status,
            AnchorRevision = task.AnchorRevision,
            Anchor = new TaskAnchorView
            {
                Revision = task.AnchorRevision,
                OriginalGoal = task.Goal,
                CurrentInterpretation = task.Goal,
                NextAction = string.Empty,
            },
        });
    }

    public Task<WorkChangesResponse> ReadChangesAsync(int? limit = null, string? afterTx = null, CancellationToken cancellationToken = default) =>
        Task.FromResult(new WorkChangesResponse { Changes = [] });

    public Task<WorkArtifactResponse> ReadArtifactAsync(string reference, uint? maxBytes = null, CancellationToken cancellationToken = default) =>
        Task.FromException<WorkArtifactResponse>(
            new AgentContractViolationException("work.artifact.reference", "fixture has no artifact store"));

    public Task<WorkContextResponse> ReadContextAsync(uint? limit = null, CancellationToken cancellationToken = default) =>
        Task.FromResult(new WorkContextResponse { Items = [] });

    public ValueTask DisposeAsync()
    {
        IsConnected = false;
        return ValueTask.CompletedTask;
    }
}
