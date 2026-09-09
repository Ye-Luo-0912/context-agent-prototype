using System.Collections.ObjectModel;
using System.IO;
using System.Text;
using System.Windows.Input;
using FocusAgent.Client;
using FocusAgent.Desktop.Fixture;
using FocusAgent.Desktop.Infrastructure;

namespace FocusAgent.Desktop.ViewModels;

public sealed class TaskItemViewModel
{
    public string TaskId { get; init; } = string.Empty;
    public string Goal { get; init; } = string.Empty;
    public string StatusLine { get; init; } = string.Empty;

    public static TaskItemViewModel From(TaskSnapshotEntry entry, bool isFocused) => new()
    {
        TaskId = entry.TaskId,
        Goal = entry.Goal,
        StatusLine = $"状态：{StatusText(entry.Status)} · anchor r{entry.AnchorRevision}"
            + (isFocused ? " · 焦点" : string.Empty),
    };

    private static string StatusText(TaskSnapshotStatus status) => status switch
    {
        TaskSnapshotStatus.Active => "活动",
        TaskSnapshotStatus.Suspended => "已挂起",
        TaskSnapshotStatus.Completed => "已完成（操作员接受语义随 P2 类型化字段区分）",
        _ => status.ToString(),
    };
}

/// <summary>
/// C3: one change-journal row (B3 <c>work.changes</c>). Renders only the
/// typed fields the server sent — the journal's internal <c>old_content</c>
/// never travels, so nothing here is prose-inferred or reconstructed.
/// </summary>
public sealed class ChangeRowViewModel
{
    public string KindText { get; init; } = string.Empty;
    public string Line { get; init; } = string.Empty;

    public static ChangeRowViewModel From(ChangeSummary change)
    {
        var kind = KindTextFor(change.Kind);
        var line = change.Kind switch
        {
            ChangeSummaryKind.MutationPrepared or ChangeSummaryKind.DirectoryPrepared =>
                $"{change.Tool}·{change.Path}（{change.BytesBefore}→{change.BytesAfter} 字节，prepared）",
            ChangeSummaryKind.MutationCommitted or ChangeSummaryKind.DirectoryCommitted =>
                $"提交 {change.TxId}：{change.Path ?? change.EntryIdentity ?? "（缺路径）"}",
            ChangeSummaryKind.MutationRolledBack or ChangeSummaryKind.DirectoryRolledBack =>
                $"回滚 {change.TxId}：{change.Reason ?? "（无原因说明）"}",
            _ => $"变更 {change.TxId}",
        };
        return new ChangeRowViewModel { KindText = kind, Line = line };
    }

    private static string KindTextFor(ChangeSummaryKind kind) => kind switch
    {
        ChangeSummaryKind.MutationPrepared => "修改·准备",
        ChangeSummaryKind.MutationCommitted => "修改·已提交",
        ChangeSummaryKind.MutationRolledBack => "修改·已回滚",
        ChangeSummaryKind.DirectoryPrepared => "目录·准备",
        ChangeSummaryKind.DirectoryCommitted => "目录·已提交",
        ChangeSummaryKind.DirectoryRolledBack => "目录·已回滚",
        _ => kind.ToString(),
    };
}

/// <summary>
/// C3: one read-only context row (B3 <c>work.context</c>). Shows the
/// summary's own bounded fields (id, kind label, importance, source); the
/// type-tagged engine dimensions stay raw labels — the client routes and
/// bounds context, it never re-interprets engine internals or attention.
/// </summary>
public sealed class ContextItemRowViewModel
{
    public string Id { get; init; } = string.Empty;
    public string KindLabel { get; init; } = string.Empty;
    public string Line { get; init; } = string.Empty;

    public static ContextItemRowViewModel From(ContextItemSummary item)
    {
        var kind = item.Kind.ValueKind == System.Text.Json.JsonValueKind.String
            ? item.Kind.GetString() ?? "?"
            : "?";
        var source = string.IsNullOrEmpty(item.Source) ? "（无来源标注）" : item.Source;
        return new ContextItemRowViewModel
        {
            Id = item.Id,
            KindLabel = kind,
            Line = $"{source} · 重要性 {item.Importance:0.##}",
        };
    }
}

/// <summary>
/// N4/F13: one pending approval's stable row. The row is created ONCE per
/// server request id and updated in place from typed snapshot facts — its
/// Allow/Deny commands are registered exactly once for the row's lifetime
/// and released with it. Fields the snapshot does not carry are shown as
/// unavailable; nothing is inferred from tool output or prose.
/// </summary>
public sealed class ApprovalItemViewModel : ObservableObject
{
    private string _callName = string.Empty;
    private string _riskLine = string.Empty;
    private string _targetLine = string.Empty;

    public ApprovalItemViewModel(string requestId, RelayCommand allowCommand, RelayCommand denyCommand)
    {
        RequestId = requestId;
        AllowCommand = allowCommand;
        DenyCommand = denyCommand;
    }

    public string RequestId { get; }

    public string CallName { get => _callName; private set { if (Set(ref _callName, value)) Raise(nameof(Title)); } }

    public string RiskLine { get => _riskLine; private set => Set(ref _riskLine, value); }

    public string TargetLine { get => _targetLine; private set => Set(ref _targetLine, value); }

    public string Title => $"{CallName} · {RequestId}";

    public RelayCommand AllowCommand { get; }

    public RelayCommand DenyCommand { get; }

    /// <summary>Applies one typed snapshot entry. <see cref="ApprovalRisk"/>
    /// is the gate's own declaration; the target summary is the server's
    /// bounded projection — when absent, the row says so instead of guessing.
    /// </summary>
    public void UpdateFrom(PendingApprovalSnapshot entry)
    {
        CallName = entry.CallName;
        string riskText;
        if (entry.Risk == ApprovalRisk.WorkspaceWrite)
        {
            riskText = "工作区写入";
        }
        else if (entry.Risk == ApprovalRisk.ProcessExecution)
        {
            riskText = "进程执行";
        }
        else
        {
            riskText = entry.Risk.ToString();
        }
        RiskLine = $"风险：{riskText}（服务端 gate 标注）";
        TargetLine = entry.TargetSummary is { Length: > 0 } target
            ? $"目标：{target}"
            : "目标：不可用（审批请求未携带路径/命令事实）";
    }
}

public enum TransportKind
{
    /// <summary>The real platform host on its default local endpoint — the
    /// same derivation agent-host binds (N4). This is the default channel.</summary>
    RealHost = 0,
    WindowsNamedPipe = 1,
    UnixSocket = 2,
    /// <summary>Explicit layout preview over the fixture connection. NOT an
    /// executor and never the default.</summary>
    LayoutPreview = 3,
}

public sealed class MainWindowViewModel : ObservableObject, IAsyncDisposable
{
    private static readonly string[] TransportLabels =
    {
        "真实宿主（默认端点）",
        "Windows Named Pipe",
        "Unix Socket",
        "布局预览（非执行器）",
    };

    /// <summary>G3: bounded retained data — the lists never grow past these.</summary>
    private const int MaxRenderedTasks = 200;
    /// <summary>N4: output retention bounds, drilled by the lifecycle tests.</summary>
    internal const int MaxOutputLines = 400;
    internal const int MaxOutputBytes = 64 * 1024;

    private readonly IUiDispatcher _ui;
    private readonly CancellationTokenSource _lifetime = new();
    private IAgentConnection? _connection;
    private DeltaCoalescer? _deltaCoalescer;
    private Task? _eventPump;

    /// <summary>F13: connection era. Every await that ends in ApplySnapshot
    /// captures the era it started under; a result from a previous era (or a
    /// disconnected one) is dropped, never applied.</summary>
    private int _generation;

    /// <summary>F13: snapshot refresh single-flight.</summary>
    private int _refreshInFlight;

    /// <summary>Event-driven refresh coalescing: at most one queued refresh.</summary>
    private int _refreshQueued;

    private int _disposed;

    /// <summary>F13: stable approval rows keyed by the server's request id.</summary>
    private readonly Dictionary<string, ApprovalItemViewModel> _approvalRows = new();

    /// <summary>A submit whose server-side outcome is unknown keeps its
    /// client_request_id here so a retry of the same goal is idempotent.
    /// The pair is resolved from snapshot FACTS the next time one is
    /// applied — never replayed blindly: a host restart retires the
    /// client_request_id, so a resubmission of the same goal would be a
    /// NEW admission, not an idempotent re-read.</summary>
    private string? _outstandingSubmitId;
    private string? _outstandingSubmitGoal;

    /// <summary>Connection-loss observations for the CURRENT connection
    /// era (UI-thread confined). Zeroed on every successful rebuild and
    /// on disconnect; the banner uses it to separate a transient drop
    /// from a host that stays gone.</summary>
    private int _reconnectFailures;

    private readonly StringBuilder _outputBuilder = new();
    private readonly DispatcherTimerHolder _fallbackTimer;

    private string _connectionLabel = "未连接";
    private bool _isConnected;
    private string _goalInput = string.Empty;
    private string _endpoint = AgentTransports.DefaultEndpoint();
    private int _transportIndex = (int)TransportKind.RealHost;
    private ulong _watermark;
    private string _focusText = "—";
    private string _runStateText = "运行状态：快照未获取（unavailable）。";
    private string _planText = "计划 / open loops：快照未提供（unavailable）——不从事件文字推断。";
    private string _bannerText = string.Empty;
    private string _outputText = string.Empty;
    private bool _autoRefresh = true;

    // -----------------------------------------------------------------------
    // C3 review surface (B3 read-only routes). Everything here is a run-scoped
    // OBSERVATION: it never starts a model round, never mutates state and
    // never consults Core's approval gate. Rendering is honest — before a read
    // the panels say "unavailable", a failed or stale-era read never
    // overwrites a fresh one, and the client never re-interprets engine
    // internals (the context kind/attention/semantic tags are bounded raw
    // labels, not authority).
    // -----------------------------------------------------------------------

    /// <summary>C3: cap on change records one review refresh renders.</summary>
    internal const int MaxRenderedChanges = 64;

    /// <summary>C3: cap on context entries one refresh renders.</summary>
    internal const int MaxRenderedContextItems = 256;

    private TaskItemViewModel? _selectedTask;
    private string _taskDetailText = "任务详情：未选择任务（unavailable）。";
    private string _changesStatusText = "变更日志：未读取（unavailable）。";
    private string _artifactReference = string.Empty;
    private string _artifactText = "工件：未读取（unavailable）。只显示服务端有界返回的正文。";
    private string _contextStatusText = "只读 Context：未读取（unavailable）。";

    public MainWindowViewModel(IUiDispatcher? uiDispatcher = null)
    {
        _ui = uiDispatcher ?? AvaloniaUiDispatcher.Instance;
        AsyncCommands = new AsyncCommandGroup();
        ConnectCommand = AsyncCommands.Add(
            () => ConnectAsync(),
            () => !IsConnected,
            error => AppendOutput($"连接失败：{error.Message}"));
        DisconnectCommand = AsyncCommands.Add(
            () => DisconnectAsync(),
            () => IsConnected);
        SubmitCommand = AsyncCommands.Add(
            () => SubmitAsync(),
            () => IsConnected && GoalInput.Trim().Length > 0,
            error => AppendOutput($"提交失败：{error.Message}"));
        ContinueCommand = AsyncCommands.Add(
            () => ContinueAsync(),
            () => IsConnected,
            error => AppendOutput($"继续失败：{error.Message}"));
        CancelCommand = AsyncCommands.Add(
            () => CancelAsync(),
            () => IsConnected,
            error => AppendOutput($"取消失败：{error.Message}"));
        RefreshCommand = AsyncCommands.Add(
            () => RefreshSnapshotNowAsync(),
            () => IsConnected,
            error => AppendOutput($"刷新失败：{error.Message}"));

        // C3 B3 read-only routes: observation only — never a model round,
        // never a mutation. Each command refreshes its panel from the server
        // and renders only typed facts.
        RefreshChangesCommand = AsyncCommands.Add(
            () => RefreshChangesAsync(),
            () => IsConnected,
            error => AppendOutput($"读取变更失败：{error.Message}"));
        ReadArtifactCommand = AsyncCommands.Add(
            () => ReadArtifactAsync(),
            () => IsConnected && ArtifactReference.Trim().Length > 0,
            error => AppendOutput($"读取工件失败：{error.Message}"));
        RefreshContextCommand = AsyncCommands.Add(
            () => RefreshContextAsync(),
            () => IsConnected,
            error => AppendOutput($"读取上下文失败：{error.Message}"));

        // Fallback safety-net refresh; the PRIMARY driver is the event
        // stream. Inert under the inline drill dispatcher.
        _fallbackTimer = new DispatcherTimerHolder(
            _ui.StartPeriodicTimer(TimeSpan.FromSeconds(10), TickFallbackRefresh));
    }

    private void TickFallbackRefresh()
    {
        if (AutoRefresh && IsConnected)
        {
            _ = RefreshSnapshotCoreAsync(silent: true);
        }
    }

    private AsyncCommandGroup AsyncCommands { get; }

    public ICommand ConnectCommand { get; }
    public ICommand DisconnectCommand { get; }
    public ICommand SubmitCommand { get; }
    public ICommand ContinueCommand { get; }
    public ICommand CancelCommand { get; }
    public ICommand RefreshCommand { get; }

    // C3 read-only review commands (B3 routes).
    public ICommand RefreshChangesCommand { get; }
    public ICommand ReadArtifactCommand { get; }
    public ICommand RefreshContextCommand { get; }

    public ObservableCollection<TaskItemViewModel> Tasks { get; } = [];
    public ObservableCollection<ApprovalItemViewModel> Approvals { get; } = [];

    // C3 review state (B3 read-only routes). All reads are observation-only;
    // every panel starts and resets to an explicit "unavailable".
    public ObservableCollection<ChangeRowViewModel> Changes { get; } = [];
    public ObservableCollection<ContextItemRowViewModel> ContextItems { get; } = [];

    public IReadOnlyList<string> Transports => TransportLabels;

    public int TransportIndex
    {
        get => _transportIndex;
        set
        {
            if (Set(ref _transportIndex, value))
            {
                Endpoint = (TransportKind)value switch
                {
                    TransportKind.WindowsNamedPipe => AgentTransports.DefaultPipeName,
                    TransportKind.UnixSocket => AgentTransports.DefaultSocketPathFor(Environment.CurrentDirectory),
                    _ => AgentTransports.DefaultEndpoint(),
                };
            }
        }
    }

    public string Endpoint { get => _endpoint; set => Set(ref _endpoint, value); }

    public bool AutoRefresh { get => _autoRefresh; set => Set(ref _autoRefresh, value); }

    public string GoalInput
    {
        get => _goalInput;
        set
        {
            if (Set(ref _goalInput, value))
            {
                AsyncCommands.RaiseCanExecute();
            }
        }
    }

    /// <summary>C3: the currently selected task; selecting one loads its full
    /// anchor card over the B3 task-detail read. Selection itself is
    /// observation — nothing is submitted or continued by clicking a row.</summary>
    public TaskItemViewModel? SelectedTask
    {
        get => _selectedTask;
        set
        {
            if (Set(ref _selectedTask, value))
            {
                if (value is not null)
                {
                    _ = LoadTaskDetailAsync(value.TaskId);
                }
            }
        }
    }

    /// <summary>C3: the task-detail panel's honest text — "unavailable" until
    /// a typed <c>work.task_detail</c> read lands, then the anchor card's own
    /// fields (plan/open loops are the task anchor's projection, never
    /// reconstructed from event prose).</summary>
    public string TaskDetailText { get => _taskDetailText; private set => Set(ref _taskDetailText, value); }

    /// <summary>C3: status line of the change journal panel.</summary>
    public string ChangesStatusText { get => _changesStatusText; private set => Set(ref _changesStatusText, value); }

    /// <summary>C3: artifact reference input; read with
    /// <see cref="ReadArtifactCommand"/>.</summary>
    public string ArtifactReference
    {
        get => _artifactReference;
        set
        {
            if (Set(ref _artifactReference, value))
            {
                AsyncCommands.RaiseCanExecute();
            }
        }
    }

    /// <summary>C3: the artifact panel's honest text — decoded bounded body
    /// plus the size/truncated facts the server returned; "unavailable"
    /// before any read.</summary>
    public string ArtifactText { get => _artifactText; private set => Set(ref _artifactText, value); }

    /// <summary>C3: status line of the read-only context panel.</summary>
    public string ContextStatusText { get => _contextStatusText; private set => Set(ref _contextStatusText, value); }

    public bool IsConnected
    {
        get => _isConnected;
        private set
        {
            if (Set(ref _isConnected, value))
            {
                ConnectionLabel = value ? "已连接" : "未连接";
                AsyncCommands.RaiseCanExecute();
            }
        }
    }

    public string ConnectionLabel { get => _connectionLabel; private set => Set(ref _connectionLabel, value); }

    public ulong Watermark { get => _watermark; private set => Set(ref _watermark, value); }

    public string FocusText { get => _focusText; private set => Set(ref _focusText, value); }

    /// <summary>Honest execution state rendered only from typed snapshot
    /// fields; before the first snapshot it says so.</summary>
    public string RunStateText { get => _runStateText; private set => Set(ref _runStateText, value); }

    /// <summary>The snapshot carries no plan/open-loops projection yet — the
    /// panel says so instead of reconstructing one from event prose.</summary>
    public string PlanText { get => _planText; private set => Set(ref _planText, value); }

    /// <summary>G2 banner: resync / connection-loss state, never silently hidden.</summary>
    public string BannerText { get => _bannerText; private set => Set(ref _bannerText, value); }

    public string OutputText { get => _outputText; private set => Set(ref _outputText, value); }

    /// <summary>F13: registered command count — lifecycle drill observation,
    /// not a UI state.</summary>
    internal int RegisteredCommandCount => AsyncCommands.Count;

    /// <summary>Lifecycle drill seam: applies one snapshot exactly like the
    /// live paths do, so row/command stability is observable deterministically.</summary>
    internal void ApplySnapshotForTests(WorkSnapshotResponse snapshot) => ApplySnapshot(snapshot);

    /// <summary>Restore-walkthrough drill observation: connection-loss
    /// observations counted for the current connection era.</summary>
    internal int ReconnectFailuresForTests => _reconnectFailures;

    /// <summary>Lifecycle drill seam: runs the single-flight refresh path so
    /// a drill can hold a stub's answer open across a disconnect.</summary>
    internal Task RefreshOnceForTestsAsync() => RefreshSnapshotCoreAsync(silent: false);

    /// <summary>Pending-approval rows currently held (drill observation).</summary>
    internal IReadOnlyCollection<string> PendingApprovalRequestIdsForTests => _approvalRows.Keys.ToList();

    /// <summary>Integration drill seam: installs a real (or drill) connection
    /// through the same path production connect takes — event pump included —
    /// so the scripted-host chain exercises the live wiring.</summary>
    internal async Task ConnectForTestsAsync(IAgentConnection connection)
    {
        await DisconnectCoreAsync();
        _connection = connection;
        Interlocked.Increment(ref _generation);
        IsConnected = true;
        StartEventPump(connection);
    }

    /// <summary>Restore-walkthrough drill seam: installs a REAL
    /// <see cref="ResumableSession"/> wired exactly like the production
    /// connect path (resync banner, loss counter, event pump), over a
    /// caller-supplied transport factory — so reconnect/resync behavior is
    /// drillable against scripted hosts.</summary>
    internal Task ConnectSessionForTestsAsync(Func<Task<Stream>> connectFactory)
        => ConnectResumableSessionAsync(connectFactory);

    /// <summary>Integration drill seam: the real submit path (typed request,
    /// receipt rendering, follow-up refresh).</summary>
    internal Task SubmitForTestsAsync() => SubmitAsync();

    /// <summary>Integration drill seam: the real approval-answer path.</summary>
    internal Task RespondApprovalForTestsAsync(string requestId, ApprovalDecision decision) =>
        RespondApprovalAsync(requestId, decision);

    /// <summary>C3 drill seam: the task-detail read path a selection triggers.</summary>
    internal Task LoadTaskDetailForTestsAsync(string taskId) => LoadTaskDetailAsync(taskId);

    /// <summary>C3 drill seam: the change-journal read path.</summary>
    internal Task RefreshChangesForTestsAsync() => RefreshChangesAsync();

    /// <summary>C3 drill seam: the artifact read path.</summary>
    internal Task ReadArtifactForTestsAsync(string reference)
    {
        ArtifactReference = reference;
        return ReadArtifactAsync();
    }

    /// <summary>C3 drill seam: the read-only context read path.</summary>
    internal Task RefreshContextForTestsAsync() => RefreshContextAsync();

    /// <summary>Bound drill seam: writes through the same bounded output
    /// path the events and receipts use.</summary>
    internal void AppendOutputForTests(string line) => AppendOutput(line);

    private void AppendOutput(string line)
    {
        _outputBuilder.AppendLine(line);
        var text = _outputBuilder.ToString();
        var lines = text.Split('\n');
        if (lines.Length > MaxOutputLines)
        {
            text = string.Join('\n', lines[^MaxOutputLines..]);
        }
        // N4: byte bound on top of the line bound — drop whole OLDEST lines
        // (never mid-line) until the retained output fits the budget.
        while (Encoding.UTF8.GetByteCount(text) > MaxOutputBytes)
        {
            var newline = text.IndexOf('\n');
            if (newline < 0 || newline == text.Length - 1)
            {
                text = string.Empty;
                break;
            }
            text = text[(newline + 1)..];
        }
        _outputBuilder.Clear();
        _outputBuilder.Append(text);
        OutputText = text;
    }

    private IAgentTransport BuildTransport() => (TransportKind)TransportIndex switch
    {
        TransportKind.UnixSocket => new UnixDomainSocketTransport(Endpoint),
        _ => new NamedPipeTransport(Endpoint),
    };

    private async Task ConnectAsync()
    {
        await DisconnectCoreAsync();
        if ((TransportKind)TransportIndex == TransportKind.LayoutPreview)
        {
            // Explicit, clearly-labeled layout preview: drives bindings only,
            // never pretends to execute work.
            var fixture = new FixtureAgentConnection();
            _connection = fixture;
            IsConnected = true;
            ConnectionLabel = "已连接（布局预览，非执行器）";
            BannerText = "布局预览：仅驱动界面布局，不是执行器，不代表任何真实任务状态。默认通道是真实宿主。";
            StartEventPump(fixture);
            ApplySnapshot(await fixture.SnapshotAsync());
            return;
        }
        await ConnectResumableSessionAsync(() => BuildTransport().ConnectAsync(_lifetime.Token));
    }

    /// <summary>The real-host connection path, shared by production connect
    /// and the restore-walkthrough drill seam: one resumable session whose
    /// resync/loss facts drive the banner, the event pump, and the first
    /// snapshot. A failed first snapshot leaves no task facts behind.</summary>
    private async Task ConnectResumableSessionAsync(Func<Task<Stream>> connectFactory)
    {
        var session = new ResumableSession(connectFactory);
        session.Resynced += snapshot => _ui.Post(() =>
        {
            if (!ReferenceEquals(_connection, session))
            {
                return; // the session was replaced or disconnected meanwhile
            }
            _reconnectFailures = 0; // a successful rebuild ends the loss streak
            BannerText = "已从快照重建（重连）。挂起的审批以服务器快照为准，不会自动通过。";
            ApplySnapshot(snapshot);
        });
        session.ConnectionLost += failure => _ui.Post(() =>
        {
            if (!ReferenceEquals(_connection, session))
            {
                return; // not the live connection anymore; nothing to announce
            }
            _reconnectFailures++;
            BannerText = _reconnectFailures == 1
                ? "连接丢失。挂起的审批不会自动通过；正在重连并从快照重建。"
                : $"连接丢失：已观测到 {_reconnectFailures} 次连接失败（最近：{BoundBannerDetail(failure.Message)}）。"
                    + "宿主可能已停止；宿主恢复后将自动从快照重建。挂起的审批不会自动通过。";
            AppendOutput($"连接丢失：{failure.Message}");
        });
        _connection = session;
        Interlocked.Increment(ref _generation);
        IsConnected = true;
        AppendOutput($"已连接：{TransportLabels[TransportIndex]} / {Endpoint}");
        StartEventPump(session);
        var generation = Volatile.Read(ref _generation);
        WorkSnapshotResponse first;
        try
        {
            first = await session.SnapshotAsync(_lifetime.Token);
        }
        catch (Exception failure)
        {
            AppendOutput($"握手失败：{failure.Message}。未建立任何任务事实，可重试连接。");
            await DisconnectCoreAsync();
            return;
        }
        if (generation != Volatile.Read(ref _generation) || !ReferenceEquals(_connection, session))
        {
            return; // a newer connection era owns the view now
        }
        ApplySnapshot(first);
    }

    /// <summary>One bounded fact inside the banner: failures' own messages
    /// are untrusted length-wise, so only a prefix is shown.</summary>
    private static string BoundBannerDetail(string message)
    {
        message = message.ReplaceLineEndings(" ");
        return message.Length <= 160 ? message : message[..160] + "…";
    }

    /// <summary>N4: one background consumer reads the typed event stream of
    /// the connected session and merges its work onto the UI thread. The
    /// client's bounded queue already protects approval/terminal facts; the
    /// pump only renders and refreshes, it never drops facts.</summary>
    private void StartEventPump(IAgentConnection connection)
    {
        _deltaCoalescer = new DeltaCoalescer(
            text => _ui.Post(() => AppendOutput(text)),
            flushCharBudget: 256);
        _eventPump = Task.Run(() => PumpEventsAsync(connection, _lifetime.Token));
    }

    private async Task PumpEventsAsync(IAgentConnection connection, CancellationToken token)
    {
        try
        {
            while (await connection.Events.WaitToReadAsync(token).ConfigureAwait(false))
            {
                while (connection.Events.TryRead(out var notification))
                {
                    if (token.IsCancellationRequested)
                    {
                        return;
                    }
                    // UI mutations happen on the UI thread, in stream order.
                    var captured = notification;
                    _ui.Post(() => HandleEvent(connection, captured));
                }
            }
        }
        catch (OperationCanceledException)
        {
            // Window closed / disconnected: the pump ends quietly.
        }
        catch (Exception failure)
        {
            _ui.Post(() => AppendOutput(
                $"事件流结束：{failure.Message}。请从快照重建；挂起的审批不会自动通过。"));
        }
    }

    /// <summary>Renders one typed notification. Dispatch is by the event's
    /// type tag only — the GUI never parses tool output or event prose into
    /// state; state comes from snapshots.</summary>
    private void HandleEvent(IAgentConnection connection, WorkEventNotification notification)
    {
        if (_lifetime.IsCancellationRequested
            || !ReferenceEquals(_connection, connection))
        {
            return; // a previous connection era's leftover; drop it
        }
        var envelope = notification.Envelope;
        switch (notification.EventType)
        {
            case "model_delta":
                if (TryEventString(envelope.Event, "delta", out var delta) && delta.Length > 0)
                {
                    _deltaCoalescer?.Append(delta);
                }
                break;
            case "assistant_message":
                _deltaCoalescer?.Flush();
                AppendOutput($"[{envelope.Seq}] assistant 消息已入账。");
                break;
            case "run_started":
                AppendOutput($"[{envelope.Seq}] 运行已启动。");
                break;
            case "focus_changed":
                AppendOutput(TryEventString(envelope.Event, "goal", out var goal) && goal.Length > 0
                    ? $"[{envelope.Seq}] 焦点任务：{goal}"
                    : $"[{envelope.Seq}] 焦点任务已变更。");
                break;
            case "turn_completed":
                AppendOutput($"[{envelope.Seq}] 本轮结束（以快照为准）。");
                break;
            case "turn_cancelled":
                AppendOutput($"[{envelope.Seq}] 本轮已取消（以快照为准）。");
                break;
            case "task_completed":
                AppendOutput($"[{envelope.Seq}] 任务到达终态（结果与产出以快照为准）。");
                break;
            case "tool_started":
            case "tool_finished":
                AppendOutput($"[{envelope.Seq}] {notification.EventType}（工具事件；结果内容不在此渲染）。");
                break;
            default:
                // Accounting/diagnostic events are not rendered (bounded
                // output) but still count as durable facts worth one
                // coalesced refresh.
                break;
        }
        if (!notification.IsLiveOnlyProgress)
        {
            ScheduleSnapshotRefresh();
        }
    }

    private static bool TryEventString(System.Text.Json.JsonElement @event, string property, out string value)
    {
        // Typed direct render of a named, string-typed event field.
        if (@event.ValueKind == System.Text.Json.JsonValueKind.Object
            && @event.TryGetProperty(property, out var element)
            && element.ValueKind == System.Text.Json.JsonValueKind.String)
        {
            value = element.GetString() ?? string.Empty;
            return true;
        }
        value = string.Empty;
        return false;
    }

    /// <summary>Coalesced, event-driven snapshot refresh: many arriving
    /// events collapse into at most one queued refresh, and any event that
    /// lands during a refresh schedules exactly one catch-up.</summary>
    private void ScheduleSnapshotRefresh()
    {
        if (Interlocked.CompareExchange(ref _refreshQueued, 1, 0) != 0)
        {
            return;
        }
        _ui.Post(() => _ = RunQueuedRefreshAsync());
    }

    private async Task RunQueuedRefreshAsync()
    {
        while (true)
        {
            Interlocked.Exchange(ref _refreshQueued, 0);
            await RefreshSnapshotCoreAsync(silent: true);
            if (Interlocked.CompareExchange(ref _refreshQueued, 0, 1) != 1)
            {
                return;
            }
        }
    }

    private Task RefreshSnapshotNowAsync()
    {
        if (_connection is null)
        {
            return Task.CompletedTask;
        }
        return RefreshSnapshotCoreAsync(silent: false);
    }

    /// <summary>F13: single-flight refresh with connection-era veto. At most
    /// one refresh is in flight; a result that arrives after the connection
    /// era moved on (disconnect/reconnect) is dropped, never applied.</summary>
    private async Task RefreshSnapshotCoreAsync(bool silent)
    {
        var connection = _connection;
        if (connection is null)
        {
            return;
        }
        if (Interlocked.CompareExchange(ref _refreshInFlight, 1, 0) != 0)
        {
            return;
        }
        try
        {
            var generation = Volatile.Read(ref _generation);
            WorkSnapshotResponse snapshot;
            try
            {
                snapshot = await connection.SnapshotAsync(_lifetime.Token);
            }
            catch (Exception failure)
            {
                if (!silent && !_lifetime.IsCancellationRequested)
                {
                    AppendOutput($"刷新失败：{failure.Message}（下一轮或手动刷新会重试。）");
                }
                return;
            }
            if (generation != Volatile.Read(ref _generation)
                || !ReferenceEquals(_connection, connection))
            {
                return; // stale era: drop, never apply
            }
            ApplySnapshot(snapshot);
        }
        finally
        {
            Interlocked.Exchange(ref _refreshInFlight, 0);
        }
    }

    private void ApplySnapshot(WorkSnapshotResponse snapshot)
    {
        var focusId = snapshot.Focus?.TaskId;
        Tasks.ReplaceWith(
            snapshot.Tasks
                .Take(MaxRenderedTasks)
                .Select(entry => TaskItemViewModel.From(entry, entry.TaskId == focusId)));
        if (snapshot.Tasks.Count > MaxRenderedTasks)
        {
            AppendOutput($"任务列表超过 {MaxRenderedTasks} 条，界面只保留前 {MaxRenderedTasks} 条（有界保留）。");
        }
        ApplyApprovals(snapshot.PendingApprovals);
        Watermark = snapshot.Watermark;
        FocusText = snapshot.Focus is null
            ? "无焦点任务"
            : $"{snapshot.Focus.Goal}（anchor r{snapshot.Focus.AnchorRevision}）";
        RunStateText = (snapshot.RunStarted, snapshot.RunCompleted) switch
        {
            (true, true) => "运行状态：已完成（终态以操作员接受的语义为准）。",
            (true, false) => "运行状态：已启动 · 未完成（受理/完成是不同事实）。",
            _ => "运行状态：未启动。",
        };
        PlanText = "计划 / open loops：快照未提供（unavailable）——不从事件文字推断。";
        ResolveOutstandingSubmitFromSnapshot(snapshot);
        if (snapshot.ResyncRequired)
        {
            BannerText = "事件流出现缺口（resync_required）：显示状态已由本快照整体重建。";
        }
        AsyncCommands.RaiseCanExecute();
    }

    /// <summary>F13: approval rows are stable per request id — an existing
    /// row is updated in place (its commands stay registered), a vanished
    /// request releases its commands. New rows register exactly once.</summary>
    private void ApplyApprovals(IReadOnlyList<PendingApprovalSnapshot> pending)
    {
        var seen = new HashSet<string>();
        var rows = new List<ApprovalItemViewModel>(pending.Count);
        foreach (var entry in pending)
        {
            if (!seen.Add(entry.RequestId))
            {
                continue; // defensive: a duplicated request id renders once
            }
            if (!_approvalRows.TryGetValue(entry.RequestId, out var row))
            {
                var requestId = entry.RequestId;
                row = new ApprovalItemViewModel(
                    requestId,
                    AsyncCommands.Add(
                        () => RespondApprovalAsync(requestId, ApprovalDecision.Allow),
                        onError: error => AppendOutput($"审批失败：{error.Message}")),
                    AsyncCommands.Add(
                        () => RespondApprovalAsync(requestId, ApprovalDecision.Deny),
                        onError: error => AppendOutput($"审批失败：{error.Message}")));
                _approvalRows[requestId] = row;
            }
            row.UpdateFrom(entry);
            rows.Add(row);
        }
        foreach (var requestId in _approvalRows.Keys.Where(id => !seen.Contains(id)).ToList())
        {
            var stale = _approvalRows[requestId];
            _approvalRows.Remove(requestId);
            // Release the registrations WITH the row (F13): a removed
            // approval never leaves stale commands behind.
            AsyncCommands.Remove(stale.AllowCommand);
            AsyncCommands.Remove(stale.DenyCommand);
        }
        Approvals.ReplaceWith(rows);
    }

    /// <summary>Every applied snapshot resolves an outstanding UNKNOWN
    /// submit from facts — the cold-restore honesty rule. The desktop never
    /// claims idempotence across a rebuild: a host restart retires the
    /// client_request_id, so whether the same goal reappears decides what a
    /// resubmission WOULD mean (nothing is resent automatically).
    ///
    /// A goal longer than the snapshot's bounded goal field can never
    /// appear verbatim, so it always takes the conservative branch: the
    /// operator checks the task list themselves.</summary>
    private void ResolveOutstandingSubmitFromSnapshot(WorkSnapshotResponse snapshot)
    {
        if (_outstandingSubmitId is null || _outstandingSubmitGoal is null)
        {
            return;
        }
        var goal = _outstandingSubmitGoal;
        _outstandingSubmitId = null;
        _outstandingSubmitGoal = null;
        var visible = goal.Length <= WorkSnapshotResponse.MaxGoalChars
            && snapshot.Tasks.Any(entry => entry.Goal == goal);
        AppendOutput(visible
            ? $"未知提交已核对：同名目标任务「{BoundBannerDetail(goal)}」在快照中可见，该未知以此事实解除（不自动重发）。"
            : $"未知提交已核对：快照中没有同名目标任务「{BoundBannerDetail(goal)}」。旧提交的结果不可再核；再次提交同一目标将作为新任务执行。");
    }

    private async Task SubmitAsync()
    {
        var connection = _connection;
        if (connection is null)
        {
            return;
        }
        var goal = GoalInput.Trim();
        if (goal.Length == 0)
        {
            return;
        }
        // While a previous submit's outcome is UNKNOWN, the same goal is
        // retried under the SAME client_request_id — that is what makes the
        // retry an idempotent re-read of the original admission instead of a
        // second submission.
        if (_outstandingSubmitId is not null && _outstandingSubmitGoal != goal)
        {
            AppendOutput("上一次提交的结果未知，且这次目标不同。先刷新快照核对；重复同一目标才会幂等重试。");
            return;
        }
        var clientRequestId = _outstandingSubmitId ?? ClientRequestIds.Next();
        _outstandingSubmitId = clientRequestId;
        _outstandingSubmitGoal = goal;
        WorkSubmitResponse receipt;
        try
        {
            receipt = await connection.SubmitWorkAsync(goal, clientRequestId, _lifetime.Token);
        }
        catch (AgentUnknownOutcomeException unknown)
        {
            // The id stays outstanding ON PURPOSE, but the next snapshot —
            // not a blind retry — resolves it: a host restart retires the
            // client_request_id, so the same goal would be a NEW admission.
            AppendOutput($"提交结果未知：连接在请求期间断开（{unknown.Failure.Message}）。不会自动重发；"
                + "下一份快照按事实解除该未知（同名任务可见即受理成立，不可见则再次提交将是新任务）。");
            return;
        }
        catch (Exception failure)
        {
            _outstandingSubmitId = null;
            _outstandingSubmitGoal = null;
            AppendOutput($"提交失败：{failure.Message}");
            return;
        }
        _outstandingSubmitId = null;
        _outstandingSubmitGoal = null;
        AppendOutput($"已受理：task {receipt.TaskId}（{receipt.Disposition}）。受理 ≠ 完成，完成以快照与事件为准。");
        GoalInput = string.Empty;
        await RefreshSnapshotCoreAsync(silent: true);
    }

    private async Task ContinueAsync()
    {
        var connection = _connection;
        if (connection is null)
        {
            return;
        }
        WorkContinueResponse receipt;
        try
        {
            receipt = await connection.ContinueAsync(_lifetime.Token);
        }
        catch (AgentUnknownOutcomeException unknown)
        {
            AppendOutput($"继续结果未知：连接在请求期间断开（{unknown.Failure.Message}）。从快照核对当前状态后再决定。");
            return;
        }
        catch (Exception failure)
        {
            AppendOutput($"继续失败：{failure.Message}");
            return;
        }
        AppendOutput($"已继续活动任务 {receipt.TaskId}。");
        await RefreshSnapshotCoreAsync(silent: true);
    }

    private async Task CancelAsync()
    {
        var connection = _connection;
        if (connection is null)
        {
            return;
        }
        WorkCancelResponse receipt;
        try
        {
            receipt = await connection.CancelCurrentTurnAsync(_lifetime.Token);
        }
        catch (AgentUnknownOutcomeException unknown)
        {
            AppendOutput($"取消结果未知：连接在请求期间断开（{unknown.Failure.Message}）。服务端是否已过屏障以快照与事件为准。");
            return;
        }
        catch (Exception failure)
        {
            AppendOutput($"取消失败：{failure.Message}");
            return;
        }
        AppendOutput(receipt.Ack.Status switch
        {
            TurnCancelAckStatus.Cancelled => $"取消已过屏障：generation {receipt.Ack.CancelledGeneration} → {receipt.Ack.EffectiveGeneration}。",
            _ => "当前没有活动轮次（这是事实，不是失败）。",
        });
        await RefreshSnapshotCoreAsync(silent: true);
    }

    // -----------------------------------------------------------------------
    // C3: B3 read-only review reads. Each one captures the connection era,
    // awaits the server, and applies the result ONLY through the same era plus
    // live-connection checks — a stale/disconnected read never overwrites a
    // fresher panel, and every failure leaves the panel's honest "unavailable"
    // (or its previous content) in place. These are observations: no submit,
    // no continue, no approval, no mutation.
    // -----------------------------------------------------------------------

    /// <summary>C3: loads one task's full anchor card over the B3
    /// <c>work.task_detail</c> read (triggered by selecting a task).</summary>
    private async Task LoadTaskDetailAsync(string taskId)
    {
        var connection = _connection;
        if (connection is null)
        {
            TaskDetailText = "任务详情：未连接（unavailable）。";
            return;
        }
        var generation = Volatile.Read(ref _generation);
        Task<WorkTaskDetailResponse> read;
        try
        {
            read = connection.TaskDetailAsync(taskId, _lifetime.Token);
        }
        catch (Exception failure)
        {
            if (!_lifetime.IsCancellationRequested)
            {
                AppendOutput($"任务详情读取失败：{failure.Message}");
            }
            TaskDetailText = "任务详情：读取失败（unavailable）。";
            return;
        }
        WorkTaskDetailResponse detail;
        try
        {
            detail = await read;
        }
        catch (Exception failure)
        {
            if (!_lifetime.IsCancellationRequested)
            {
                AppendOutput($"任务详情读取失败：{failure.Message}");
            }
            TaskDetailText = "任务详情：读取失败（unavailable）。";
            return;
        }
        if (!IsCurrentEra(generation, connection))
        {
            return; // a newer connection owns the panel now; drop the stale read
        }
        TaskDetailText = RenderTaskDetail(detail);
    }

    /// <summary>Renders the typed anchor card. Every field is the task anchor's
    /// own projection (safe to show); the panel never invents a plan or open
    /// loop from event prose — those arrive only as typed anchor fields.</summary>
    private static string RenderTaskDetail(WorkTaskDetailResponse detail)
    {
        var lines = new List<string>
        {
            $"任务 {detail.TaskId} · {detail.Status} · anchor r{detail.AnchorRevision}",
            $"目标：{detail.Goal}",
        };
        var anchor = detail.Anchor;
        lines.Add($"当前解释：{anchor.CurrentInterpretation}");
        if (anchor.Constraints.Count > 0)
        {
            lines.Add("约束：");
            lines.AddRange(anchor.Constraints.Select(text => $"  - {text}"));
        }
        if (anchor.AcceptanceCriteria.Count > 0)
        {
            lines.Add("验收标准：");
            lines.AddRange(anchor.AcceptanceCriteria.Select(text => $"  - {text}"));
        }
        if (anchor.PlanProgress.Count > 0)
        {
            lines.Add("计划进度：");
            lines.AddRange(anchor.PlanProgress.Select(text => $"  - {text}"));
        }
        if (anchor.OpenLoops.Count > 0)
        {
            lines.Add("open loops：");
            lines.AddRange(anchor.OpenLoops.Select(text => $"  - {text}"));
        }
        if (anchor.NextAction.Length > 0)
        {
            lines.Add($"下一步（建议，非完成判定）：{anchor.NextAction}");
        }
        return string.Join('\n', lines);
    }

    /// <summary>C3: refreshes the change journal over the B3
    /// <c>work.changes</c> read (newest first, bounded).</summary>
    private async Task RefreshChangesAsync()
    {
        var connection = _connection;
        if (connection is null)
        {
            ChangesStatusText = "变更日志：未连接（unavailable）。";
            return;
        }
        var generation = Volatile.Read(ref _generation);
        WorkChangesResponse changes;
        try
        {
            changes = await connection.ReadChangesAsync(
                limit: MaxRenderedChanges, cancellationToken: _lifetime.Token);
        }
        catch (Exception failure)
        {
            if (!_lifetime.IsCancellationRequested)
            {
                AppendOutput($"变更读取失败：{failure.Message}");
            }
            ChangesStatusText = "变更日志：读取失败（unavailable）。";
            return;
        }
        if (!IsCurrentEra(generation, connection))
        {
            return;
        }
        var rows = changes.Changes
            .Take(MaxRenderedChanges)
            .Select(ChangeRowViewModel.From)
            .ToArray();
        Changes.ReplaceWith(rows);
        ChangesStatusText = changes.Changes.Count > MaxRenderedChanges
            ? $"变更日志：显示最新 {MaxRenderedChanges} 条（服务端返回 {changes.Changes.Count} 条）。"
            : $"变更日志：{rows.Length} 条（按序，最新优先）。";
    }

    /// <summary>C3: reads one artifact by reference over the B3
    /// <c>work.artifact</c> read. The response's own size/truncated facts are
    /// shown verbatim; the body stays bounded (the client asks for at most
    /// the protocol's artifact cap) and never pretends to be prose.</summary>
    private async Task ReadArtifactAsync()
    {
        var connection = _connection;
        var reference = ArtifactReference.Trim();
        if (connection is null)
        {
            ArtifactText = "工件：未连接（unavailable）。";
            return;
        }
        if (reference.Length == 0)
        {
            ArtifactText = "工件：未提供引用（unavailable）。";
            return;
        }
        var generation = Volatile.Read(ref _generation);
        WorkArtifactResponse artifact;
        try
        {
            artifact = await connection.ReadArtifactAsync(
                reference,
                maxBytes: WorkArtifactRequest.MaxArtifactReadBytes,
                cancellationToken: _lifetime.Token);
        }
        catch (Exception failure)
        {
            if (!_lifetime.IsCancellationRequested)
            {
                AppendOutput($"工件读取失败：{failure.Message}");
            }
            ArtifactText = "工件：读取失败（unavailable）。";
            return;
        }
        if (!IsCurrentEra(generation, connection))
        {
            return;
        }
        string body;
        try
        {
            body = System.Text.Encoding.UTF8.GetString(Convert.FromBase64String(artifact.ContentBase64));
        }
        catch (FormatException)
        {
            // The client-side validator already rejected invalid base64; this
            // is a defensive fallback that never renders a guessed body.
            body = string.Empty;
        }
        ArtifactText = $"工件 {artifact.Reference}：{artifact.SizeBytes} 字节"
            + (artifact.Truncated ? "（已截断）" : "（完整）")
            + $"：\n{body}";
    }

    /// <summary>C3: refreshes the read-only context summary over the B3
    /// <c>work.context</c> read (bounded).</summary>
    private async Task RefreshContextAsync()
    {
        var connection = _connection;
        if (connection is null)
        {
            ContextStatusText = "只读 Context：未连接（unavailable）。";
            return;
        }
        var generation = Volatile.Read(ref _generation);
        WorkContextResponse context;
        try
        {
            context = await connection.ReadContextAsync(
                limit: MaxRenderedContextItems, cancellationToken: _lifetime.Token);
        }
        catch (Exception failure)
        {
            if (!_lifetime.IsCancellationRequested)
            {
                AppendOutput($"上下文读取失败：{failure.Message}");
            }
            ContextStatusText = "只读 Context：读取失败（unavailable）。";
            return;
        }
        if (!IsCurrentEra(generation, connection))
        {
            return;
        }
        var rows = context.Items
            .Take(MaxRenderedContextItems)
            .Select(ContextItemRowViewModel.From)
            .ToArray();
        ContextItems.ReplaceWith(rows);
        ContextStatusText = context.Items.Count > MaxRenderedContextItems
            ? $"只读 Context：显示 {MaxRenderedContextItems} 条（引擎共有 {context.Items.Count} 条）。"
            : $"只读 Context：{rows.Length} 条。";
    }

    /// <summary>F13-era guard: true only while the captured generation is
    /// still the live one AND the connection that served the read is still
    /// installed — a read from a replaced or disconnected era is dropped.</summary>
    private bool IsCurrentEra(int generation, IAgentConnection connection) =>
        generation == Volatile.Read(ref _generation) && ReferenceEquals(_connection, connection);

    private async Task RespondApprovalAsync(string requestId, ApprovalDecision decision)
    {
        var connection = _connection;
        if (connection is null)
        {
            return;
        }
        ApprovalRespondResponse outcome;
        try
        {
            outcome = await connection.RespondApprovalAsync(requestId, decision, _lifetime.Token);
        }
        catch (Exception failure)
        {
            // Approval answers are never retried here: a lost answer could
            // mean the decision was or was not delivered. Re-snapshot and
            // decide again from facts.
            AppendOutput($"审批 {requestId} 未送达：{failure.Message}。请刷新快照后基于事实重新决定；不会自动重发。");
            await RefreshSnapshotCoreAsync(silent: true);
            return;
        }
        AppendOutput(outcome.Outcome == ApprovalRespondOutcome.Delivered
            ? $"审批 {requestId} 已送达：{decision}。"
            : $"审批 {requestId} 已不在待决（迟到或重复），当前事实被返回。");
        await RefreshSnapshotCoreAsync(silent: true);
    }

    public async Task DisconnectAsync() => await DisconnectCoreAsync();

    /// <summary>The one disconnect path: drops the connection era, releases
    /// the session/fixture, stops the pump's input, and clears rows WITH
    /// their command registrations.</summary>
    private async Task DisconnectCoreAsync()
    {
        Interlocked.Increment(ref _generation);
        var connection = _connection;
        _connection = null;
        _deltaCoalescer?.Flush();
        _deltaCoalescer?.Dispose(); // stop the pending flush timer (N7)
        _deltaCoalescer = null;
        if (connection is not null)
        {
            await connection.DisposeAsync();
        }
        _reconnectFailures = 0; // a new connection era counts its own losses
        IsConnected = false;
        BannerText = string.Empty;
        Tasks.ReplaceWith([]);
        ApplyApprovals([]);
        // C3: the review panels are honest on disconnect — no stale server
        // facts survive into the next era.
        SelectedTask = null;
        TaskDetailText = "任务详情：未连接（unavailable）。";
        Changes.ReplaceWith([]);
        ChangesStatusText = "变更日志：未连接（unavailable）。";
        ArtifactText = "工件：未连接（unavailable）。";
        ContextItems.ReplaceWith([]);
        ContextStatusText = "只读 Context：未连接（unavailable）。";
        AsyncCommands.RaiseCanExecute();
    }

    /// <summary>Window close: cancels the lifetime (stopping the event pump
    /// and the fallback timer), releases the connection, and clears every
    /// pending wait. Idempotent.</summary>
    public async ValueTask DisposeAsync()
    {
        if (Interlocked.Exchange(ref _disposed, 1) != 0)
        {
            return;
        }
        await _lifetime.CancelAsync().ConfigureAwait(false);
        _fallbackTimer.Dispose();
        await DisconnectCoreAsync().ConfigureAwait(false);
        if (_eventPump is { } pump)
        {
            try
            {
                await pump.WaitAsync(TimeSpan.FromSeconds(2)).ConfigureAwait(false);
            }
            catch
            {
                // The pump ends with the cancelled lifetime or the completed
                // event stream; its exit is bounded, not load-bearing here.
            }
        }
        _lifetime.Dispose();
    }

    /// <summary>Owns the fallback timer handle so disposal stays total even
    /// when no dispatcher was available.</summary>
    private sealed class DispatcherTimerHolder(IDisposable handle) : IDisposable
    {
        public void Dispose() => handle.Dispose();
    }
}
