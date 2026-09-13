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
        public Task<WorkTaskCompletionResponse> TaskCompletionAsync(string taskId, CancellationToken cancellationToken = default) =>
            Task.FromResult(new WorkTaskCompletionResponse { TaskId = taskId, Fact = new WorkCompletionFactBeyondJournalWindow() });

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

        public Func<string, uint?, ulong?, CancellationToken, Task<WorkArtifactResponse>> ArtifactHandler { get; set; } =
            (_, _, _, _) => Task.FromException<WorkArtifactResponse>(
                new NotSupportedException("review drills set ArtifactHandler"));

        public Func<CancellationToken, Task<WorkContextResponse>> ContextHandler { get; set; } =
            _ => Task.FromException<WorkContextResponse>(
                new NotSupportedException("review drills set ContextHandler"));

        public bool IsConnected { get; private set; } = true;

        public ChannelReader<WorkEventNotification> Events => _events.Reader;

        /// <summary>C4 drills: write typed facts into the live event stream.</summary>
        public ChannelWriter<WorkEventNotification> EventWriter => _events.Writer;

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

        public Task<WorkArtifactResponse> ReadArtifactAsync(string reference, uint? maxBytes = null, ulong? offset = null, CancellationToken cancellationToken = default) =>
            ArtifactHandler(reference, maxBytes, offset, cancellationToken);

        public Task<WorkContextResponse> ReadContextAsync(uint? limit = null, CancellationToken cancellationToken = default) =>
            ContextHandler(cancellationToken);

        public Task<WorkSubmitResultResponse> SubmitResultAsync(string clientRequestId, string? payloadDigest = null, CancellationToken cancellationToken = default) =>
            throw new NotSupportedException("review drills do not query the submit ledger");

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

    /// <summary>F07: a late FAILURE from a replaced connection era must not
    /// clear a panel the newer connection just filled. The old code only
    /// fenced the SUCCESS path; the catch block wrote "unavailable"
    /// unconditionally, so an in-flight error landing after a reconnect wiped
    /// fresh server facts. The fixture holds the old read open across a
    /// disconnect + reconnect, then faults it.</summary>
    [Fact]
    public async Task Late_era_failure_does_not_clear_a_fresh_panel()
    {
        var viewModel = new MainWindowViewModel(new InlineUiDispatcher());
        var oldStub = new ReviewStub();
        var newStub = new ReviewStub();
        var holdDetail = new TaskCompletionSource<WorkTaskDetailResponse>(
            TaskCreationOptions.RunContinuationsAsynchronously);
        var holdChanges = new TaskCompletionSource<WorkChangesResponse>(
            TaskCreationOptions.RunContinuationsAsynchronously);
        oldStub.TaskDetailHandler = (_, _) => holdDetail.Task;
        oldStub.ChangesHandler = _ => holdChanges.Task;

        // The NEW connection serves real facts.
        newStub.TaskDetailHandler = (_, _) => Task.FromResult(Detail());
        newStub.ChangesHandler = _ => Task.FromResult(new WorkChangesResponse
        {
            Changes = [Change(ChangeSummaryKind.MutationCommitted, "fresh", "docs/new.md")],
        });
        try
        {
            await viewModel.ConnectForTestsAsync(oldStub);
            var detailRead = viewModel.LoadTaskDetailForTestsAsync(TaskId);
            var changesRead = viewModel.RefreshChangesForTestsAsync();

            // Era moves on: drop the old connection and install a new one that
            // fills both panels with valid data.
            await viewModel.DisconnectAsync();
            await viewModel.ConnectForTestsAsync(newStub);
            await viewModel.LoadTaskDetailForTestsAsync(TaskId);
            await viewModel.RefreshChangesForTestsAsync();
            Assert.Contains("open loops", viewModel.TaskDetailText);
            Assert.Single(viewModel.Changes);
            var freshDetail = viewModel.TaskDetailText;
            var freshChanges = viewModel.ChangesStatusText;

            // Now the OLD requests settle — as FAILURES.
            holdDetail.SetException(new IOException("old connection died"));
            holdChanges.SetException(new IOException("old connection died"));
            await detailRead;
            await changesRead;

            // The stale failures must NOT have painted the panels: the fresh
            // era's facts survive verbatim.
            Assert.Equal(freshDetail, viewModel.TaskDetailText);
            Assert.Equal(freshChanges, viewModel.ChangesStatusText);
            Assert.Single(viewModel.Changes);
            Assert.DoesNotContain("读取失败", viewModel.TaskDetailText);
            Assert.DoesNotContain("读取失败", viewModel.ChangesStatusText);
        }
        finally
        {
            await viewModel.DisposeAsync();
        }
    }

    /// <summary>F07: a slow detail read for task A must never paint the panel
    /// after the user selects task B — the read is bound to the selection
    /// ordinal it was issued for, not just the connection era.</summary>
    [Fact]
    public async Task Slow_detail_for_a_previous_selection_cannot_paint_the_new_one()
    {
        var viewModel = new MainWindowViewModel(new InlineUiDispatcher());
        var stub = new ReviewStub();
        var taskAId = "00000000-0000-4000-8000-0000000000aa";
        var taskBId = "00000000-0000-4000-8000-0000000000bb";
        var holdA = new TaskCompletionSource<WorkTaskDetailResponse>(
            TaskCreationOptions.RunContinuationsAsynchronously);
        stub.TaskDetailHandler = (id, _) => id == taskAId
            ? holdA.Task
            : Task.FromResult(Detail() with { TaskId = taskBId, Goal = "任务 B 的目标" });
        try
        {
            await viewModel.ConnectForTestsAsync(stub);

            // Select B first so its card is on the panel...
            viewModel.SelectedTask = new TaskItemViewModel { TaskId = taskBId, Goal = "任务 B 的目标" };
            await WaitUntilAsync(() => viewModel.TaskDetailText.Contains("任务 B 的目标"), "task B card");
            var taskBText = viewModel.TaskDetailText;

            // ...then select A (slow): the panel keeps B's card while A loads
            // (an in-flight read never clears the panel it will replace).
            viewModel.SelectedTask = new TaskItemViewModel { TaskId = taskAId, Goal = "任务 A 的目标" };
            await Task.Delay(50);
            Assert.DoesNotContain("任务 A", viewModel.TaskDetailText);

            // Reselect B before A answers.
            viewModel.SelectedTask = new TaskItemViewModel { TaskId = taskBId, Goal = "任务 B 的目标" };
            await WaitUntilAsync(() => viewModel.TaskDetailText.Contains("任务 B 的目标"), "task B reselected");

            // A's answer lands late — it must be retired by the selection fence.
            holdA.SetResult(Detail() with { TaskId = taskAId, Goal = "任务 A 的目标" });
            await Task.Delay(100);

            Assert.Equal(taskBText, viewModel.TaskDetailText);
            Assert.DoesNotContain("任务 A", viewModel.TaskDetailText);
        }
        finally
        {
            await viewModel.DisposeAsync();
        }
    }

    /// <summary>F05: the PRODUCTION connect entry must build the
    /// platform-appropriate transport for the RealHost default — the actual
    /// <c>BuildTransport</c> branch, not a test-injected session.
    ///
    /// The defect this pins is platform-conditional (the old default arm
    /// hard-coded <c>NamedPipeTransport</c>), so it is only observable off
    /// Windows. The decision is therefore driven for BOTH OS values
    /// explicitly — a Windows-only assertion would pass vacuously and hide
    /// the Linux regression the audit found.</summary>
    [Fact]
    public async Task RealHost_default_selects_the_platform_transport()
    {
        await using var viewModel = new MainWindowViewModel(new InlineUiDispatcher());
        Assert.Equal((int)TransportKind.RealHost, viewModel.TransportIndex);

        // The live production entry agrees with the SDK's own platform choice
        // for THIS host.
        var expected = AgentTransports.DefaultLocal();
        var transport = viewModel.BuildTransport();
        Assert.Equal(expected.GetType(), transport.GetType());
        Assert.Equal(expected.DisplayName, transport.DisplayName);

        // Linux/macOS: RealHost must be a UDS — a pipe here is the F05 bug,
        // and this branch is exercised regardless of the test host's OS.
        var unix = MainWindowViewModel.BuildTransport(
            TransportKind.RealHost, "ignored", isWindows: false);
        Assert.IsType<UnixDomainSocketTransport>(unix);

        // Windows: RealHost is the shared pipe name.
        var windows = MainWindowViewModel.BuildTransport(
            TransportKind.RealHost, "ignored", isWindows: true);
        Assert.IsType<NamedPipeTransport>(windows);

        // Explicit pipe selection builds a pipe verbatim (custom endpoint
        // preserved) on either host...
        viewModel.TransportIndex = (int)TransportKind.WindowsNamedPipe;
        var pipe = Assert.IsType<NamedPipeTransport>(viewModel.BuildTransport());
        Assert.Equal(AgentTransports.DefaultPipeName, viewModel.Endpoint);
        Assert.Contains(AgentTransports.DefaultPipeName, pipe.DisplayName);

        // ...and explicit UDS builds the socket transport for the user's own
        // typed path, preserving it verbatim.
        viewModel.TransportIndex = (int)TransportKind.UnixSocket;
        viewModel.Endpoint = "/tmp/focus-agent-drill.sock";
        var socket = Assert.IsType<UnixDomainSocketTransport>(viewModel.BuildTransport());
        Assert.Contains("/tmp/focus-agent-drill.sock", socket.DisplayName);
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
    public async Task Artifact_read_shows_identity_window_and_eof_facts()
    {
        var viewModel = new MainWindowViewModel(new InlineUiDispatcher());
        var stub = new ReviewStub();
        var body = "diff --git a/docs/AUDIT_TODO.md b/docs/AUDIT_TODO.md";
        var seenMaxBytes = (uint?)0;
        var seenOffset = ulong.MaxValue;
        stub.ArtifactHandler = (reference, maxBytes, offset, _) =>
        {
            seenMaxBytes = maxBytes;
            seenOffset = offset ?? 0;
            return Task.FromResult(new WorkArtifactResponse
            {
                Reference = reference,
                SizeBytes = 200,
                Offset = 0,
                Truncated = true,
                NextOffset = (ulong)Encoding.UTF8.GetByteCount(body),
                ContentBase64 = Convert.ToBase64String(Encoding.UTF8.GetBytes(body)),
            });
        };
        try
        {
            await viewModel.ConnectForTestsAsync(stub);
            await viewModel.ReadArtifactForTestsAsync("artifact://run/diff.json");

            // The client asks for at most the protocol's artifact cap, from
            // the beginning on a fresh read.
            Assert.Equal(WorkArtifactRequest.MaxArtifactReadBytes, seenMaxBytes);
            Assert.Equal(0UL, seenOffset);
            // The header carries identity, size, window and continuation facts.
            Assert.Contains("artifact://run/diff.json", viewModel.ArtifactText);
            Assert.Contains("200 字节", viewModel.ArtifactText);
            Assert.Contains("窗口 [0,", viewModel.ArtifactText);
            Assert.Contains("后续还有", viewModel.ArtifactText);
            Assert.Contains(body, viewModel.ArtifactText);
            Assert.Equal((ulong)Encoding.UTF8.GetByteCount(body), viewModel.ArtifactNextOffset);
        }
        finally
        {
            await viewModel.DisposeAsync();
        }
    }

    [Fact]
    public async Task Artifact_pages_continue_to_eof_and_reassemble_multibyte_text()
    {
        // PLATFORM-2 (GUI-2): a large multibyte artifact is read page by
        // page from the server's own cursors; a character cut by a page
        // boundary is completed by the next page (never rendered as a
        // replacement character), and the final page declares eof.
        var viewModel = new MainWindowViewModel(new InlineUiDispatcher());
        var stub = new ReviewStub();
        // 8 repetitions of four CJK characters (3 UTF-8 bytes each) plus a
        // tail character: page boundaries of 8 bytes cut characters often.
        var content = string.Concat(Enumerable.Repeat("工件审阅", 8)) + "尾字";
        var bytes = Encoding.UTF8.GetBytes(content);
        stub.ArtifactHandler = (reference, maxBytes, offset, _) =>
        {
            var start = offset ?? 0;
            var take = (int)Math.Min(maxBytes ?? WorkArtifactRequest.MaxArtifactReadBytes, bytes.Length - (int)start);
            var page = bytes.Skip((int)start).Take(take).ToArray();
            var end = start + (ulong)page.Length;
            return Task.FromResult(new WorkArtifactResponse
            {
                Reference = reference,
                SizeBytes = (ulong)bytes.Length,
                Offset = start,
                Truncated = end < (ulong)bytes.Length,
                NextOffset = end < (ulong)bytes.Length ? end : null,
                ContentBase64 = Convert.ToBase64String(page),
            });
        };
        try
        {
            await viewModel.ConnectForTestsAsync(stub);
            await viewModel.ReadArtifactForTestsAsync("artifact://run/report.txt");

            ulong? cursor = viewModel.ArtifactNextOffset;
            var pages = 1;
            while (cursor.HasValue)
            {
                Assert.True(pages < 100, "paging must terminate");
                await viewModel.ReadNextArtifactPageForTestsAsync();
                cursor = viewModel.ArtifactNextOffset;
                pages++;
            }

            // The reassembled window is the complete content — including the
            // multibyte characters that page boundaries cut through — and the
            // header states eof.
            Assert.Contains(content, viewModel.ArtifactText);
            Assert.Contains("已到文件末尾（eof）", viewModel.ArtifactText);
            Assert.Null(viewModel.ArtifactNextOffset);
        }
        finally
        {
            await viewModel.DisposeAsync();
        }
    }

    [Fact]
    public async Task Artifact_tail_read_jumps_to_the_last_window()
    {
        var viewModel = new MainWindowViewModel(new InlineUiDispatcher());
        var stub = new ReviewStub();
        var seenOffsets = new List<ulong>();
        var bytes = Encoding.UTF8.GetBytes(new string('t', (int)WorkArtifactRequest.MaxArtifactReadBytes + 300));
        stub.ArtifactHandler = (reference, maxBytes, offset, _) =>
        {
            var start = offset ?? 0;
            seenOffsets.Add(start);
            var requested = maxBytes ?? WorkArtifactRequest.MaxArtifactReadBytes;
            if (requested == 1)
            {
                // the tail probe: one byte, cursor to the rest
                return Task.FromResult(new WorkArtifactResponse
                {
                    Reference = reference,
                    SizeBytes = (ulong)bytes.Length,
                    Offset = 0,
                    Truncated = true,
                    NextOffset = 1,
                    ContentBase64 = Convert.ToBase64String(bytes[..1]),
                });
            }
            var take = (int)Math.Min(requested, bytes.Length - (int)start);
            var page = bytes.Skip((int)start).Take(take).ToArray();
            var end = start + (ulong)page.Length;
            return Task.FromResult(new WorkArtifactResponse
            {
                Reference = reference,
                SizeBytes = (ulong)bytes.Length,
                Offset = start,
                Truncated = end < (ulong)bytes.Length,
                NextOffset = end < (ulong)bytes.Length ? end : null,
                ContentBase64 = Convert.ToBase64String(page),
            });
        };
        try
        {
            await viewModel.ConnectForTestsAsync(stub);
            viewModel.ArtifactReference = "artifact://run/tail.txt";
            await viewModel.ReadArtifactTailForTestsAsync();

            // The probe learned the size, and the window jumped straight to
            // the LAST page (size - page cap) — without reading the leading
            // bytes page by page.
            var expectedTail = (ulong)bytes.Length - WorkArtifactRequest.MaxArtifactReadBytes;
            Assert.Equal(expectedTail, seenOffsets.Max());
            Assert.Equal(2, seenOffsets.Count); // probe + last window, nothing between
            Assert.Contains($"窗口 [{expectedTail},", viewModel.ArtifactText);
            Assert.Contains("已到文件末尾（eof）", viewModel.ArtifactText);
            Assert.Null(viewModel.ArtifactNextOffset);
        }
        finally
        {
            await viewModel.DisposeAsync();
        }
    }

    [Fact]
    public async Task Model_deltas_fill_the_output_panel_while_receipts_fill_the_log()
    {
        var viewModel = new MainWindowViewModel(new InlineUiDispatcher());
        var stub = new ReviewStub();
        try
        {
            await viewModel.ConnectForTestsAsync(stub);

            // An operational receipt lands in the LOG, never in the output.
            stub.ContextHandler = _ => Task.FromException<WorkContextResponse>(
                new NotSupportedException("no context route in this drill"));
            await viewModel.RefreshContextForTestsAsync();
            Assert.Contains("上下文读取失败", viewModel.LogText);
            Assert.DoesNotContain("上下文读取失败", viewModel.OutputText);

            // The output panel is the model's content; the log never carries it.
            viewModel.AppendOutputForTests("模型正文片段");
            viewModel.AppendLogForTests("运行日志条目");
            Assert.Contains("模型正文片段", viewModel.OutputText);
            Assert.DoesNotContain("运行日志条目", viewModel.OutputText);
            Assert.Contains("运行日志条目", viewModel.LogText);
            Assert.DoesNotContain("模型正文片段", viewModel.LogText);
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

    /// <summary>C4: a model_used fact with a given identity.</summary>
    private const string CostRunId = "00000000-0000-4000-8000-000000000001";

    private static WorkEventNotification ModelUsed(
        ulong seq, string identity, ulong input, ulong output, ulong cached,
        string role = "main") => new()
    {
        Envelope = new RuntimeEventEnvelope
        {
            RunId = CostRunId,
            Seq = seq,
            TimestampMs = seq,
            Event = JsonSerializer.Deserialize<JsonElement>(
                "{\"type\":\"model_used\",\"input_tokens\":" + input +
                ",\"output_tokens\":" + output +
                ",\"cached_input_tokens\":" + cached +
                ",\"attempts\":1,\"retries\":0,\"usage_identity\":\"" + identity + "\"" +
                ",\"role\":\"" + role + "\"}"),
        },
    };

    /// <summary>C4: the three usage identities account separately — observed
    /// is the bill, estimated is labelled as runtime-derived, and unknown
    /// rounds are COUNTED, never folded into observed nor shown as zero.</summary>
    [Fact]
    public async Task Model_used_events_classify_cost_by_identity_and_never_sum_unknown_as_zero()
    {
        var viewModel = new MainWindowViewModel(new InlineUiDispatcher());
        var stub = new ReviewStub();
        try
        {
            await viewModel.ConnectForTestsAsync(stub);

            stub.EventWriter.TryWrite(ModelUsed(1, "observed", 9000, 120, 2500));
            await WaitUntilAsync(
                () => viewModel.CostSummaryText.Contains("实测 1 轮"), "the observed round");
            Assert.Contains("输入 9000 · 输出 120 · 缓存读 2500", viewModel.CostSummaryText);
            Assert.Contains("模型消耗（实测·主调用）", viewModel.LogText);

            stub.EventWriter.TryWrite(ModelUsed(2, "estimated", 700, 30, 0));
            await WaitUntilAsync(
                () => viewModel.CostSummaryText.Contains("估算 1 轮"), "the estimated round");
            Assert.Contains("非 provider 账单", viewModel.LogText);

            stub.EventWriter.TryWrite(ModelUsed(3, "unknown", 0, 0, 0));
            await WaitUntilAsync(
                () => viewModel.CostSummaryText.Contains("未知 1 轮"), "the unknown round");
            Assert.Contains("不计为零", viewModel.LogText);
            // An unknown round must never be summed as observed consumption.
            Assert.DoesNotContain("实测 2 轮", viewModel.CostSummaryText);
        }
        finally
        {
            await viewModel.DisposeAsync();
        }
    }

    /// <summary>COST-7 (R2-11): a maintenance-lane model_used row keeps its
    /// own accumulators — it never blurs into the main-round bill, and the
    /// lane is named in the log so the reader knows where the cost sits.</summary>
    [Fact]
    public async Task Maintenance_lane_usage_is_accounted_separately_from_main_rounds()
    {
        var viewModel = new MainWindowViewModel(new InlineUiDispatcher());
        var stub = new ReviewStub();
        try
        {
            await viewModel.ConnectForTestsAsync(stub);

            stub.EventWriter.TryWrite(ModelUsed(1, "observed", 9000, 120, 2500));
            await WaitUntilAsync(
                () => viewModel.CostSummaryText.Contains("实测 1 轮"), "the main round");

            stub.EventWriter.TryWrite(ModelUsed(2, "observed", 500, 40, 0, "maintenance"));
            await WaitUntilAsync(
                () => viewModel.CostSummaryText.Contains("维护压缩调用 1 轮"), "the maintenance row");
            Assert.Contains("模型消耗（实测·维护压缩调用）", viewModel.LogText);
            // The maintenance tokens stay OUT of the main-round bill.
            Assert.DoesNotContain("实测 2 轮", viewModel.CostSummaryText);
            Assert.Contains("输入 500 · 输出 40", viewModel.CostSummaryText);
        }
        finally
        {
            await viewModel.DisposeAsync();
        }
    }

    /// <summary>COST-9 (R3-14): a model_used row squeezed out of the
    /// bounded render queue still reaches the cost accumulators — the shed
    /// path feeds the same fixed-size totals exactly once, so the account
    /// never loses a billed call to the rendering cap.</summary>
    [Fact]
    public async Task Cost_facts_survive_the_render_queue_cap()
    {
        var viewModel = new MainWindowViewModel(new InlineUiDispatcher());
        var stub = new ReviewStub();
        try
        {
            await viewModel.ConnectForTestsAsync(stub);

            // One billed round, then enough durable filler events to push it
            // out of the 64-entry render queue before any drain runs.
            stub.EventWriter.TryWrite(ModelUsed(1, "observed", 9000, 120, 2500));
            for (ulong seq = 2; seq <= 70; seq++)
            {
                stub.EventWriter.TryWrite(new WorkEventNotification
                {
                    Envelope = new RuntimeEventEnvelope
                    {
                        RunId = CostRunId,
                        Seq = seq,
                        TimestampMs = seq,
                        Event = JsonSerializer.Deserialize<JsonElement>(
                            "{\"type\":\"tool_finished\"}"),
                    },
                });
            }
            await WaitUntilAsync(
                () => viewModel.CostSummaryText.Contains("实测 1 轮"),
                "the shed model row must still be accounted");
            // Exactly once: the shed row never reaches the render drain, and
            // no surviving row double-counts it.
            Assert.DoesNotContain("实测 2 轮", viewModel.CostSummaryText);
            Assert.Contains("输入 9000 · 输出 120", viewModel.CostSummaryText);
        }
        finally
        {
            await viewModel.DisposeAsync();
        }
    }

    /// <summary>COST-9 (R3-14): a context_compacted row shed by the render
    /// cap still lands in the compaction totals.</summary>
    [Fact]
    public async Task Compaction_cost_survives_the_render_queue_cap()
    {
        var viewModel = new MainWindowViewModel(new InlineUiDispatcher());
        var stub = new ReviewStub();
        try
        {
            await viewModel.ConnectForTestsAsync(stub);

            stub.EventWriter.TryWrite(Compacted(1, "rolling_fold", 34600, 1300, 8));
            for (ulong seq = 2; seq <= 70; seq++)
            {
                stub.EventWriter.TryWrite(new WorkEventNotification
                {
                    Envelope = new RuntimeEventEnvelope
                    {
                        RunId = CostRunId,
                        Seq = seq,
                        TimestampMs = seq,
                        Event = JsonSerializer.Deserialize<JsonElement>(
                            "{\"type\":\"tool_finished\"}"),
                    },
                });
            }
            await WaitUntilAsync(
                () => viewModel.CostSummaryText.Contains("压缩 1 次"),
                "the shed compaction row must still be accounted");
            Assert.Contains("输入 34600", viewModel.CostSummaryText);
            Assert.DoesNotContain("压缩 2 次", viewModel.CostSummaryText);
        }
        finally
        {
            await viewModel.DisposeAsync();
        }
    }

    /// <summary>C4: a context_compacted event's typed fields.</summary>
    private static WorkEventNotification Compacted(
        ulong seq, string reason, ulong input, ulong output, ulong items) => new()
    {
        Envelope = new RuntimeEventEnvelope
        {
            RunId = CostRunId,
            Seq = seq,
            TimestampMs = seq,
            Event = JsonSerializer.Deserialize<JsonElement>(
                "{\"type\":\"context_compacted\",\"reason\":\"" + reason +
                "\",\"input_tokens\":" + input +
                ",\"output_tokens\":" + output +
                ",\"source_items\":" + items + "}"),
        },
    };

    /// <summary>C4: compaction cost is rendered from the typed
    /// context_compacted fields as SERVICE-REPORTED cost — the event carries
    /// no usage identity, so the counters stay outside the observed model
    /// bill and the two classes are never merged in the summary.</summary>
    [Fact]
    public async Task Compaction_events_show_service_reported_cost_outside_the_model_bill()
    {
        var viewModel = new MainWindowViewModel(new InlineUiDispatcher());
        var stub = new ReviewStub();
        try
        {
            await viewModel.ConnectForTestsAsync(stub);

            stub.EventWriter.TryWrite(Compacted(1, "rolling_fold", 12000, 800, 14));
            await WaitUntilAsync(
                () => viewModel.CostSummaryText.Contains("压缩 1 次"), "the compaction entry");
            Assert.Contains("压缩消耗（服务端报告，未带实测身份）", viewModel.LogText);
            Assert.Contains("不并入主调用实测合计", viewModel.LogText);
            // The compaction counters never leak into the observed model bill.
            Assert.Contains("实测 0 轮", viewModel.CostSummaryText);

            // A model round afterwards still accounts separately; the summary
            // carries both facts without merging them.
            stub.EventWriter.TryWrite(ModelUsed(2, "observed", 9000, 120, 2500));
            await WaitUntilAsync(
                () => viewModel.CostSummaryText.Contains("实测 1 轮"), "the observed round");
            Assert.Contains("压缩 1 次", viewModel.CostSummaryText);
            Assert.Contains("输入 9000", viewModel.CostSummaryText);
        }
        finally
        {
            await viewModel.DisposeAsync();
        }
    }

    /// <summary>COST-1 (E05.3): a compaction event that CARRIES its usage
    /// identity is classified from the wire fact; a legacy event without the
    /// field keeps the honest "no identity" wording. Neither is upgraded to
    /// measured by the GUI, and neither joins the model bill.</summary>
    [Fact]
    public async Task Compaction_rows_classify_by_wire_identity_and_keep_legacy_honest()
    {
        var viewModel = new MainWindowViewModel(new InlineUiDispatcher());
        var stub = new ReviewStub();
        try
        {
            await viewModel.ConnectForTestsAsync(stub);

            // The platform fixture's shape: the event names its own identity.
            var identified = JsonSerializer.Deserialize<JsonElement>(
                "{\"type\":\"context_compacted\",\"reason\":\"rolling_fold\"," +
                "\"input_tokens\":12000,\"output_tokens\":800,\"source_items\":14," +
                "\"usage_identity\":\"estimated\"}");
            stub.EventWriter.TryWrite(new WorkEventNotification
            {
                Envelope = new RuntimeEventEnvelope
                {
                    RunId = CostRunId,
                    Seq = 1,
                    TimestampMs = 1,
                    Event = identified,
                },
            });
            await WaitUntilAsync(
                () => viewModel.CostSummaryText.Contains("压缩 1 次"), "the identified compaction");
            Assert.Contains("压缩消耗（估算（运行时近似，非 provider 账单））", viewModel.LogText);

            // A legacy event without the field: same counters policy, honest
            // "no identity" wording, still outside the model bill.
            var legacy = JsonSerializer.Deserialize<JsonElement>(
                "{\"type\":\"context_compacted\",\"reason\":\"task_completed\"," +
                "\"input_tokens\":0,\"output_tokens\":0,\"source_items\":2}");
            stub.EventWriter.TryWrite(new WorkEventNotification
            {
                Envelope = new RuntimeEventEnvelope
                {
                    RunId = CostRunId,
                    Seq = 2,
                    TimestampMs = 2,
                    Event = legacy,
                },
            });
            await WaitUntilAsync(
                () => viewModel.CostSummaryText.Contains("压缩 2 次"), "the legacy compaction");
            Assert.Contains("压缩消耗（服务端报告，未带实测身份）", viewModel.LogText);
            Assert.Contains("实测 0 轮", viewModel.CostSummaryText);
        }
        finally
        {
            await viewModel.DisposeAsync();
        }
    }

    /// <summary>C4: context rows render the typed freshness fact — actually
    /// sent, stored pointer-only, or unknown when a legacy server omits the
    /// field. Nothing is inferred from kind or prose.</summary>
    [Fact]
    public async Task Context_rows_render_freshness_from_typed_fields_and_admit_missing_as_unknown()
    {
        var viewModel = new MainWindowViewModel(new InlineUiDispatcher());
        var stub = new ReviewStub();
        try
        {
            await viewModel.ConnectForTestsAsync(stub);
            static JsonElement Tag(string value) =>
                JsonSerializer.Deserialize<JsonElement>($"\"{value}\"");
            stub.ContextHandler = _ => Task.FromResult(new WorkContextResponse
            {
                Items = new[]
                {
                    new ContextItemSummary
                    {
                        Id = "sent-1",
                        Kind = Tag("Note"),
                        Scope = Tag("task"),
                        Importance = 0.5,
                        Residency = "resident",
                        SelectedCurrentTurn = true,
                    },
                    new ContextItemSummary
                    {
                        Id = "stored-1",
                        Kind = Tag("Note"),
                        Scope = Tag("task"),
                        Importance = 0.4,
                        Residency = "external",
                        SelectedCurrentTurn = false,
                    },
                    new ContextItemSummary
                    {
                        Id = "legacy-1",
                        Kind = Tag("Note"),
                        Scope = Tag("task"),
                        Importance = 0.3,
                    },
                },
            });

            await viewModel.RefreshContextForTestsAsync();

            var lines = viewModel.ContextItems.Select(row => row.Line).ToArray();
            Assert.Contains(lines, line => line.Contains("驻留 · 本轮已发送"));
            Assert.Contains(lines, line => line.Contains("存储中（仅摘要指针）"));
            Assert.Contains(lines, line => line.Contains("新鲜度未知"));
        }
        finally
        {
            await viewModel.DisposeAsync();
        }
    }

}
