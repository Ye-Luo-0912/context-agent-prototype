using System.Text;
using System.Text.Json;
using System.Threading.Channels;

using FocusAgent.Desktop.Infrastructure;
using FocusAgent.Desktop.ViewModels;

using Xunit;

namespace FocusAgent.Client.Tests;

/// <summary>
/// C3 drills for the desktop workbench's read-only review surface (B3
/// routes): task detail / change journal / artifact bytes / context summary.
/// Every read is an observation — never a model round, never a mutation —
/// and every panel is honest: "unavailable" before data, on failure, and on
/// disconnect; stale-era results are dropped; context rows only route and
/// bound engine tags, never re-interpret them. All assertions are typed DTO
/// facts or the workbench's own honest-state text.
/// </summary>
public class WorkbenchReviewTests
{
    private const string TaskId = "00000000-0000-4000-8000-000000000022";

    /// <summary>Review drill fixture: a connection whose four B3 read routes
    /// are programmable handlers plus the minimal snapshot/event surface the
    /// workbench's connect path needs.</summary>
    private sealed class ReviewStub : IAgentConnection
    {
        private readonly Channel<WorkEventNotification> _events =
            Channel.CreateBounded<WorkEventNotification>(8);

        public Func<CancellationToken, Task<WorkSnapshotResponse>> SnapshotHandler { get; set; } =
            _ => Task.FromResult(new WorkSnapshotResponse
            {
                RunStarted = false,
                RunCompleted = false,
                Watermark = 0,
                Focus = null,
                Tasks = [],
                PendingApprovals = [],
                ResyncRequired = false,
            });

        public Func<string, CancellationToken, Task<WorkTaskDetailResponse>> TaskDetailHandler { get; set; } =
            (_, _) => Task.FromException<WorkTaskDetailResponse>(
                new NotSupportedException("review drills set TaskDetailHandler"));

        public Func<CancellationToken, Task<WorkChangesResponse>> ChangesHandler { get; set; } =
            _ => Task.FromException<WorkChangesResponse>(
                new NotSupportedException("review drills set ChangesHandler"));

        public Func<string, uint?, CancellationToken, Task<WorkArtifactResponse>> ArtifactHandler { get; set; } =
            (_, _, _) => Task.FromException<WorkArtifactResponse>(
                new NotSupportedException("review drills set ArtifactHandler"));

        public Func<CancellationToken, Task<WorkContextResponse>> ContextHandler { get; set; } =
            _ => Task.FromException<WorkContextResponse>(
                new NotSupportedException("review drills set ContextHandler"));

        public bool IsConnected { get; private set; } = true;

        public ChannelReader<WorkEventNotification> Events => _events.Reader;

        public Task<WorkSnapshotResponse> SnapshotAsync(CancellationToken cancellationToken = default) =>
            SnapshotHandler(cancellationToken);

        public Task<WorkSubmitResponse> SubmitWorkAsync(string goal, string clientRequestId, CancellationToken cancellationToken = default) =>
            throw new NotSupportedException("review drills do not submit");

        public Task<WorkContinueResponse> ContinueAsync(CancellationToken cancellationToken = default) =>
            throw new NotSupportedException("review drills do not continue");

        public Task<WorkCancelResponse> CancelCurrentTurnAsync(CancellationToken cancellationToken = default) =>
            throw new NotSupportedException("review drills do not cancel");

        public Task<WorkSubscribeResponse> SubscribeAsync(ulong? replayAfterSeq = null, CancellationToken cancellationToken = default) =>
            throw new NotSupportedException("review drills do not subscribe");

        public Task<ApprovalRespondResponse> RespondApprovalAsync(string requestId, ApprovalDecision decision, CancellationToken cancellationToken = default) =>
            throw new NotSupportedException("review drills do not answer approvals");

        public Task<WorkTaskDetailResponse> TaskDetailAsync(string taskId, CancellationToken cancellationToken = default) =>
            TaskDetailHandler(taskId, cancellationToken);

        public Task<WorkChangesResponse> ReadChangesAsync(int? limit = null, string? afterTx = null, CancellationToken cancellationToken = default) =>
            ChangesHandler(cancellationToken);

        public Task<WorkArtifactResponse> ReadArtifactAsync(string reference, uint? maxBytes = null, CancellationToken cancellationToken = default) =>
            ArtifactHandler(reference, maxBytes, cancellationToken);

        public Task<WorkContextResponse> ReadContextAsync(uint? limit = null, CancellationToken cancellationToken = default) =>
            ContextHandler(cancellationToken);

        public ValueTask DisposeAsync()
        {
            IsConnected = false;
            _events.Writer.TryComplete();
            return ValueTask.CompletedTask;
        }
    }

    private static WorkTaskDetailResponse Detail() => new()
    {
        TaskId = TaskId,
        Goal = "修复 supervision 台账的清理确认",
        Status = TaskSnapshotStatus.Active,
        AnchorRevision = 3,
        Anchor = new TaskAnchorView
        {
            Revision = 3,
            OriginalGoal = "修复 supervision 台账的清理确认",
            CurrentInterpretation = "E2E 内联脚本的清理确认改为原子等待",
            Constraints = ["不可误杀其它进程"],
            AcceptanceCriteria = ["agent-process 29 项全绿"],
            PlanProgress = ["台账记录/读取/结束返回 Result"],
            OpenLoops = ["冷恢复孤儿跟随 B1 验收"],
            NextAction = "补清理确认的读取测试",
        },
    };

    private static ChangeSummary Change(ChangeSummaryKind kind, string tx, string? path = null) => new()
    {
        Kind = kind,
        TxId = tx,
        TimestampMs = 1_000,
        Tool = "edit",
        Path = path,
        Action = "patch",
        BytesBefore = 10,
        BytesAfter = 20,
        BeforeHash = "h1",
        AfterHash = "h2",
        Reason = "denied",
    };

    private static ContextItemSummary ContextItem(string id, string kind, string source) => new()
    {
        Id = id,
        Kind = JsonSerializer.Deserialize<JsonElement>($"\"{kind}\""),
        Scope = JsonSerializer.Deserialize<JsonElement>("\"task\""),
        ScopeId = TaskId,
        Attention = JsonSerializer.Deserialize<JsonElement>("\"selected\""),
        Semantic = JsonSerializer.Deserialize<JsonElement>("\"raw\""),
        Importance = 0.9,
        Relevance = 0.5,
        CreatedTick = 1,
        CreatedTurn = 2,
        LastAccessTurn = 3,
        LastSelectedTurn = 3,
        AccessCount = 4,
        Dependencies = [],
        KeepAlive = false,
        LeaseUntilTurn = null,
        Source = source,
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
    public async Task Review_panels_start_honestly_unavailable()
    {
        var viewModel = new MainWindowViewModel(new InlineUiDispatcher());
        try
        {
            Assert.Equal("任务详情：未选择任务（unavailable）。", viewModel.TaskDetailText);
            Assert.Equal("变更日志：未读取（unavailable）。", viewModel.ChangesStatusText);
            Assert.Equal("工件：未读取（unavailable）。只显示服务端有界返回的正文。", viewModel.ArtifactText);
            Assert.Equal("只读 Context：未读取（unavailable）。", viewModel.ContextStatusText);
            Assert.Empty(viewModel.Changes);
            Assert.Empty(viewModel.ContextItems);
        }
        finally
        {
            await viewModel.DisposeAsync();
        }
    }

    [Fact]
    public async Task Task_detail_renders_the_typed_anchor_card()
    {
        var viewModel = new MainWindowViewModel(new InlineUiDispatcher());
        var stub = new ReviewStub();
        stub.TaskDetailHandler = (_, _) => Task.FromResult(Detail());
        try
        {
            await viewModel.ConnectForTestsAsync(stub);
            await viewModel.LoadTaskDetailForTestsAsync(TaskId);

            // The anchor card's own typed fields — plan/open loops are the
            // task anchor's projection, never reconstructed from prose.
            Assert.Contains($"任务 {TaskId} · Active · anchor r3", viewModel.TaskDetailText);
            Assert.Contains("目标：修复 supervision 台账的清理确认", viewModel.TaskDetailText);
            Assert.Contains("约束：", viewModel.TaskDetailText);
            Assert.Contains("- 不可误杀其它进程", viewModel.TaskDetailText);
            Assert.Contains("open loops：", viewModel.TaskDetailText);
            Assert.Contains("- 冷恢复孤儿跟随 B1 验收", viewModel.TaskDetailText);
            Assert.Contains("下一步（建议，非完成判定）：补清理确认的读取测试", viewModel.TaskDetailText);
        }
        finally
        {
            await viewModel.DisposeAsync();
        }
    }

    [Fact]
    public async Task Task_detail_when_busy_stays_nonexecuting_and_failure_is_honest()
    {
        var viewModel = new MainWindowViewModel(new InlineUiDispatcher());
        var stub = new ReviewStub();
        stub.TaskDetailHandler = (_, _) => Task.FromException<WorkTaskDetailResponse>(
            new AgentContractViolationException("work.task_detail.task_id", "not found"));
        try
        {
            await viewModel.ConnectForTestsAsync(stub);
            await viewModel.LoadTaskDetailForTestsAsync(TaskId);
            Assert.Equal("任务详情：读取失败（unavailable）。", viewModel.TaskDetailText);

            // A failed read never invents a plan/loop from event prose.
            Assert.DoesNotContain("open loops", viewModel.TaskDetailText);
            Assert.DoesNotContain("下一步", viewModel.TaskDetailText);
        }
        finally
        {
            await viewModel.DisposeAsync();
        }
    }

    [Fact]
    public async Task Changes_render_bounded_rows_and_their_status()
    {
        var viewModel = new MainWindowViewModel(new InlineUiDispatcher());
        var stub = new ReviewStub();
        var changes = new[]
        {
            Change(ChangeSummaryKind.MutationPrepared, "t1", "docs/a.md"),
            Change(ChangeSummaryKind.MutationCommitted, "t1", "docs/a.md"),
            Change(ChangeSummaryKind.DirectoryPrepared, "t2", "docs/"),
            Change(ChangeSummaryKind.DirectoryCommitted, "t2"),
            Change(ChangeSummaryKind.MutationRolledBack, "t3"),
            Change(ChangeSummaryKind.DirectoryRolledBack, "t4"),
        };
        stub.ChangesHandler = _ => Task.FromResult(new WorkChangesResponse { Changes = changes });
        try
        {
            await viewModel.ConnectForTestsAsync(stub);
            await viewModel.RefreshChangesForTestsAsync();

            Assert.Equal(6, viewModel.Changes.Count);
            Assert.Contains("修改·准备", viewModel.Changes[0].KindText);
            Assert.Contains("docs/a.md", viewModel.Changes[0].Line);
            Assert.Contains("修改·已提交", viewModel.Changes[1].KindText);
            Assert.Contains("提交 t1：docs/a.md", viewModel.Changes[1].Line);
            Assert.Contains("目录·准备", viewModel.Changes[2].KindText);
            Assert.Contains("docs/", viewModel.Changes[2].Line);
            Assert.Contains("修改·已回滚", viewModel.Changes[4].KindText);
            Assert.Contains("回滚 t3", viewModel.Changes[4].Line);
            Assert.Contains("6 条", viewModel.ChangesStatusText);
        }
        finally
        {
            await viewModel.DisposeAsync();
        }
    }

    [Fact]
    public async Task Artifact_read_shows_size_truncated_and_the_bounded_body()
    {
        var viewModel = new MainWindowViewModel(new InlineUiDispatcher());
        var stub = new ReviewStub();
        var body = "diff --git a/docs/AUDIT_TODO.md b/docs/AUDIT_TODO.md";
        var seenMaxBytes = (uint?)0;
        stub.ArtifactHandler = (reference, maxBytes, _) =>
        {
            seenMaxBytes = maxBytes;
            return Task.FromResult(new WorkArtifactResponse
            {
                Reference = reference,
                SizeBytes = 200,
                Truncated = true,
                ContentBase64 = Convert.ToBase64String(Encoding.UTF8.GetBytes(body)),
            });
        };
        try
        {
            await viewModel.ConnectForTestsAsync(stub);
            await viewModel.ReadArtifactForTestsAsync("artifact://run/diff.json");

            // The client asks for at most the protocol's artifact cap.
            Assert.Equal(WorkArtifactRequest.MaxArtifactReadBytes, seenMaxBytes);
            Assert.Contains("artifact://run/diff.json", viewModel.ArtifactText);
            Assert.Contains("200 字节（已截断）", viewModel.ArtifactText);
            Assert.Contains(body, viewModel.ArtifactText);
        }
        finally
        {
            await viewModel.DisposeAsync();
        }
    }

    [Fact]
    public async Task Artifact_read_without_reference_stays_honest()
    {
        var viewModel = new MainWindowViewModel(new InlineUiDispatcher());
        var stub = new ReviewStub();
        try
        {
            await viewModel.ConnectForTestsAsync(stub);
            await viewModel.ReadArtifactForTestsAsync("   ");
            Assert.Equal("工件：未提供引用（unavailable）。", viewModel.ArtifactText);
        }
        finally
        {
            await viewModel.DisposeAsync();
        }
    }

    [Fact]
    public async Task Context_renders_routed_labels_not_reinterpreted_engine_internals()
    {
        var viewModel = new MainWindowViewModel(new InlineUiDispatcher());
        var stub = new ReviewStub();
        stub.ContextHandler = _ => Task.FromResult(new WorkContextResponse
        {
            Items =
            [
                ContextItem("item-1", "file", "docs/AUDIT_TODO.md"),
                ContextItem("item-2", "tool", "fs.read"),
            ],
        });
        try
        {
            await viewModel.ConnectForTestsAsync(stub);
            await viewModel.RefreshContextForTestsAsync();

            Assert.Equal(2, viewModel.ContextItems.Count);
            // The kind travel as raw labels; the client never claims to
            // understand engine internals, only to route and bound them.
            Assert.Equal("file", viewModel.ContextItems[0].KindLabel);
            Assert.Equal("tool", viewModel.ContextItems[1].KindLabel);
            Assert.Contains("docs/AUDIT_TODO.md · 重要性 0.9", viewModel.ContextItems[0].Line);
            Assert.Equal("只读 Context：2 条。", viewModel.ContextStatusText);
        }
        finally
        {
            await viewModel.DisposeAsync();
        }
    }

    [Fact]
    public async Task Stale_era_review_results_are_dropped()
    {
        var viewModel = new MainWindowViewModel(new InlineUiDispatcher());
        var stub = new ReviewStub();
        var detailGate = new TaskCompletionSource<WorkTaskDetailResponse>(
            TaskCreationOptions.RunContinuationsAsynchronously);
        var changesGate = new TaskCompletionSource<WorkChangesResponse>(
            TaskCreationOptions.RunContinuationsAsynchronously);
        stub.TaskDetailHandler = (_, _) => detailGate.Task;
        stub.ChangesHandler = _ => changesGate.Task;
        try
        {
            await viewModel.ConnectForTestsAsync(stub);

            var detailRead = viewModel.LoadTaskDetailForTestsAsync(TaskId);
            var changesRead = viewModel.RefreshChangesForTestsAsync();

            // The connection era moves on while both reads are in flight.
            await viewModel.DisconnectAsync();
            Assert.Equal("任务详情：未连接（unavailable）。", viewModel.TaskDetailText);
            Assert.Equal("变更日志：未连接（unavailable）。", viewModel.ChangesStatusText);

            // The stale-era results land but must NOT be applied.
            detailGate.SetResult(Detail());
            changesGate.SetResult(new WorkChangesResponse { Changes = [Change(ChangeSummaryKind.MutationCommitted, "late")] });
            await detailRead;
            await changesRead;

            Assert.Equal("任务详情：未连接（unavailable）。", viewModel.TaskDetailText);
            Assert.Equal("变更日志：未连接（unavailable）。", viewModel.ChangesStatusText);
            Assert.Empty(viewModel.Changes);
        }
        finally
        {
            await viewModel.DisposeAsync();
        }
    }

    [Fact]
    public async Task Disconnect_resets_the_review_panels()
    {
        var viewModel = new MainWindowViewModel(new InlineUiDispatcher());
        var stub = new ReviewStub();
        stub.TaskDetailHandler = (_, _) => Task.FromResult(Detail());
        stub.ChangesHandler = _ => Task.FromResult(new WorkChangesResponse
        {
            Changes = [Change(ChangeSummaryKind.MutationCommitted, "t1", "docs/a.md")],
        });
        try
        {
            await viewModel.ConnectForTestsAsync(stub);
            await viewModel.LoadTaskDetailForTestsAsync(TaskId);
            await viewModel.RefreshChangesForTestsAsync();
            Assert.Contains("任务 ", viewModel.TaskDetailText);
            Assert.Single(viewModel.Changes);

            // Disconnect: no stale server facts survive into the next era.
            await viewModel.DisconnectAsync();
            Assert.Equal("任务详情：未连接（unavailable）。", viewModel.TaskDetailText);
            Assert.Equal("变更日志：未连接（unavailable）。", viewModel.ChangesStatusText);
            Assert.Empty(viewModel.Changes);
            Assert.Empty(viewModel.ContextItems);
        }
        finally
        {
            await viewModel.DisposeAsync();
        }
    }
}