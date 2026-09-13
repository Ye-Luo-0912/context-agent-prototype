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

    /// <summary>C4: freshness/residency tag rendered from the PLATFORM-2
    /// typed fields. A missing field (legacy server) renders as unknown —
    /// never guessed from kind or prose.</summary>
    public string FreshnessLabel { get; init; } = string.Empty;

    public static ContextItemRowViewModel From(ContextItemSummary item)
    {
        var kind = item.Kind.ValueKind == System.Text.Json.JsonValueKind.String
            ? item.Kind.GetString() ?? "?"
            : "?";
        var source = string.IsNullOrEmpty(item.Source) ? "（无来源标注）" : item.Source;
        var freshness = Freshness(item);
        return new ContextItemRowViewModel
        {
            Id = item.Id,
            KindLabel = kind,
            FreshnessLabel = freshness,
            Line = $"{source} · 重要性 {item.Importance:0.##} · {freshness}",
        };
    }

    /// <summary>C4: the four-state freshness fact — actually sent this turn,
    /// resident (available but not in the latest surface), stored
    /// (pointer-only without a fetch), or unknown when the server predates
    /// the typed fields. warm/cold read as stored: neither is the working
    /// set.</summary>
    internal static string Freshness(ContextItemSummary item)
    {
        if (string.IsNullOrEmpty(item.Residency))
        {
            return "新鲜度未知";
        }
        return item.Residency switch
        {
            "resident" when item.SelectedCurrentTurn is true => "驻留 · 本轮已发送",
            "resident" => "驻留（未在最新表面）",
            "warm" => "暖缓冲（正文可取回）",
            "cold" or "external" => "存储中（仅摘要指针）",
            _ => "新鲜度未知",
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

/// <summary>GUI-3: the cancel surface's honest phase. <see cref="Requested"/>
/// means the cancel request was SENT — not that the turn stopped; only a
/// typed ack (<c>TurnCancelAckStatus.Cancelled</c>) proves the barrier, a
/// <c>NoActiveTurn</c> ack is a fact (not a failure), and an undetermined
/// outcome keeps the side-effect warning until a trusted snapshot
/// re-derives the state.</summary>
public enum CancelRequestPhase
{
    Idle,
    Requested,
    Cancelled,
    NoActiveTurn,
    Unknown,
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

    /// <summary>R14: explicit bounds on the pump→UI handoff, applied at the
    /// SOURCE. At most one drain callback is ever posted to the dispatcher,
    /// and the batched delta text is capped by the output budget, so a
    /// slow/paused UI holds a bounded backlog — never one callback per
    /// arrival.</summary>
    private const int MaxPendingUiItems = 64;
    private const int MaxPendingDeltaChars = MaxOutputBytes;

    private readonly object _handoffGate = new();
    private List<(IAgentConnection Connection, WorkEventNotification Item)> _pendingUiEvents = new();
    private StringBuilder _pendingUiDelta = new();

    /// <summary>F07: the connection instance the currently buffered delta
    /// text was produced by. The delta buffer has no per-item stamp (it is one
    /// coalesced string), so the era is tracked alongside it: a queued drain
    /// flushes the text only while THIS connection is still the live one, and
    /// <see cref="DisconnectCoreAsync"/> drops the buffer outright. Without
    /// this, a delta buffered under a retired connection was appended to the
    /// output panel with no era check at all.</summary>
    private IAgentConnection? _deltaEraConnection;
    private bool _uiDrainPosted;

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
    /// GUI-3/F06: the pair is resolved ONLY by the exact-request ledger query
    /// (<c>work.submit_result</c>) — the snapshot's task list is never
    /// consulted, because a same-goal task is not evidence about this request
    /// id. The goal is kept solely to compute the query's payload digest; it
    /// never takes part in any matching.</summary>
    private string? _outstandingSubmitId;
    private string? _outstandingSubmitGoal;

    /// <summary>R07: true while the outstanding submit is still awaiting its
    /// receipt (Pending); false once the receipt is lost or unknown, so the
    /// ledger query may resolve the key from facts. A PENDING submit must
    /// never be resolved by a concurrent refresh — that is what keeps a
    /// same-goal retry on the SAME client_request_id instead of a new
    /// admission.</summary>
    private bool _outstandingSubmitAwaitingReceipt;

    /// <summary>GUI-3/F06: single-flight for the ledger resolution query —
    /// every applied snapshot re-asks while a key is outstanding, and at most
    /// one query is in flight at any time.</summary>
    private int _submitResultQueryInFlight;

    /// <summary>GUI-3/F06: the indeterminate (Unknown/Expired/failed) verdict
    /// for the CURRENT outstanding id has been reported once. Definitive
    /// verdicts always report (they release the key, so they happen at most
    /// once per id); repeating "still unknown" on every snapshot would only
    /// bury the output panel.</summary>
    private bool _outstandingSubmitResolutionReported;

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


    // ---------------------------------------------------------------------
    // C4 (GUI-4): session cost accounting, read straight off the typed
    // model_used facts (PLATFORM-4's TryGetModelUsage). The three identity
    // classes are NEVER summed together: observed tokens are the
    // provider-reported bill; estimated are runtime approximations;
    // unknown rounds are real costs whose evidence is lost (a cancelled
    // in-flight call) — they are COUNTED, never rendered as zero.
    // ---------------------------------------------------------------------

    private ulong _costObservedInput;
    private ulong _costObservedOutput;
    private ulong _costObservedCached;
    private int _costObservedRounds;
    private ulong _costEstimatedInput;
    private ulong _costEstimatedOutput;
    private int _costEstimatedRounds;
    private int _costUnknownRounds;
    // COST-9 (R3-14): render rows shed by the bounded queue after their
    // cost facts were accumulated — observability for the lossy log, not a
    // hole in the totals.
    private int _costRenderRowsShed;
    // COST-7: the maintenance/compactor lane's observed rows, kept apart
    // from the main-round bill (both are service-reported, but the lanes
    // are different cost centers).
    private int _costMaintenanceRounds;
    private ulong _costMaintenanceInput;
    private ulong _costMaintenanceOutput;
    // C4: compaction cost (context_compacted). The event carries NO usage
    // identity, so its counters are service-reported numbers kept OUTSIDE
    // the observed model bill — never upgraded into "measured" by the GUI.
    private int _costCompactions;
    private ulong _costCompactionInput;
    private ulong _costCompactionOutput;
    private string _costSummaryText = "成本账目：暂无模型调用。";

    public string CostSummaryText { get => _costSummaryText; private set => Set(ref _costSummaryText, value); }

    // ---------------------------------------------------------------------
    // C2 (GUI-2): bounded paging-review window over one artifact. The window
    // accumulates the paged reads' bytes (drop-oldest at a hard cap), so a
    // large artifact's tail is reachable and re-readable WITHOUT re-triggering
    // any model or tool side effect — every page is a read-only wire call that
    // re-verifies the same sealed identity.
    // ---------------------------------------------------------------------

    /// <summary>C2: hard cap on the accumulated review window. Beyond it the
    /// OLDEST bytes are released (never silently: the panel says so) — the
    /// full artifact stays on the host, readable page by page.</summary>
    internal const int MaxArtifactWindowBytes = 1024 * 1024;

    private readonly List<byte> _artifactWindow = new();
    private string? _artifactSessionReference;
    private ulong _artifactWindowStart;
    private int _artifactReadInFlight;

    public MainWindowViewModel(IUiDispatcher? uiDispatcher = null)
    {
        _ui = uiDispatcher ?? AvaloniaUiDispatcher.Instance;
        AsyncCommands = new AsyncCommandGroup();
        ConnectCommand = AsyncCommands.Add(
            () => ConnectAsync(),
            () => !IsConnected,
            error => AppendLog($"连接失败：{error.Message}"));
        DisconnectCommand = AsyncCommands.Add(
            () => DisconnectAsync(),
            () => IsConnected);
        SubmitCommand = AsyncCommands.Add(
            () => SubmitAsync(),
            () => IsConnected && GoalInput.Trim().Length > 0,
            error => AppendLog($"提交失败：{error.Message}"));
        ContinueCommand = AsyncCommands.Add(
            () => ContinueAsync(),
            () => IsConnected,
            error => AppendLog($"继续失败：{error.Message}"));
        CancelCommand = AsyncCommands.Add(
            () => CancelAsync(),
            () => IsConnected,
            error => AppendLog($"取消失败：{error.Message}"));
        RefreshCommand = AsyncCommands.Add(
            () => RefreshSnapshotNowAsync(),
            () => IsConnected,
            error => AppendLog($"刷新失败：{error.Message}"));

        // C3 B3 read-only routes: observation only — never a model round,
        // never a mutation. Each command refreshes its panel from the server
        // and renders only typed facts.
        RefreshChangesCommand = AsyncCommands.Add(
            () => RefreshChangesAsync(),
            () => IsConnected,
            error => AppendLog($"读取变更失败：{error.Message}"));
        ReadArtifactCommand = AsyncCommands.Add(
            () => ReadArtifactAsync(),
            () => IsConnected && ArtifactReference.Trim().Length > 0,
            error => AppendLog($"读取工件失败：{error.Message}"));
        // C2 (GUI-2): paging continues from the server's own cursor and is
        // disabled at eof — a read-only continuation, never a re-trigger of
        // any work.
        NextArtifactPageCommand = AsyncCommands.Add(
            () => ReadNextArtifactPageAsync(),
            () => IsConnected && ArtifactNextOffset.HasValue,
            error => AppendLog($"读取工件下一页失败：{error.Message}"));
        ReadArtifactTailCommand = AsyncCommands.Add(
            () => ReadArtifactTailAsync(),
            () => IsConnected && ArtifactReference.Trim().Length > 0,
            error => AppendLog($"读取工件尾部失败：{error.Message}"));
        RefreshContextCommand = AsyncCommands.Add(
            () => RefreshContextAsync(),
            () => IsConnected,
            error => AppendLog($"读取上下文失败：{error.Message}"));

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

    // C2 (GUI-2): artifact paging commands (PLATFORM-2 continuation cursor).
    public ICommand NextArtifactPageCommand { get; }
    public ICommand ReadArtifactTailCommand { get; }

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
                // F07: every selection change (including to null) advances the
                // ordinal, so a detail read still in flight for the previous
                // selection is retired and can never paint this one's panel.
                var selection = Interlocked.Increment(ref _selectionSequence);
                if (value is not null)
                {
                    _ = LoadTaskDetailAsync(value.TaskId, selection);
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

    /// <summary>C2 (GUI-2): the server's continuation cursor for the current
    /// artifact session — null once the last read reached eof (and before any
    /// read). Drives the「下一页」command's availability.</summary>
    private ulong? _artifactNextOffset;

    public ulong? ArtifactNextOffset { get => _artifactNextOffset; private set => Set(ref _artifactNextOffset, value); }

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

    /// <summary>GUI-3: the cancel request's phase and its honest text —
    /// "requested" from the moment the frame is sent, upgraded only by typed
    /// facts (ack / snapshot), never by hope.</summary>
    private CancelRequestPhase _cancelPhase = CancelRequestPhase.Idle;
    private string _cancelStateText = "尚未发出取消请求。";

    public CancelRequestPhase CancelPhase { get => _cancelPhase; private set => Set(ref _cancelPhase, value); }

    public string CancelStateText { get => _cancelStateText; private set => Set(ref _cancelStateText, value); }

    private void SetCancelPhase(CancelRequestPhase phase, string text)
    {
        CancelPhase = phase;
        CancelStateText = text;
    }

    /// <summary>GUI-3: while a cancel is in flight its outcome is owned by the
    /// in-flight request; snapshots must not re-derive the phase under it.</summary>
    private int _cancelInFlight;

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

    /// <summary>GUI-3 drill seam: the real continue path (typed receipt with
    /// selected-task comparison).</summary>
    internal Task ContinueForTestsAsync() => ContinueAsync();

    /// <summary>GUI-3 drill seam: the real cancel path (phase machine).</summary>
    internal Task CancelForTestsAsync() => CancelAsync();

    /// <summary>R07 drill observation: the client_request_id currently held
    /// outstanding (unchanged while Pending and after a lost receipt, so a
    /// same-goal retry stays on the same admission identity).</summary>
    internal string? OutstandingSubmitIdForTests => _outstandingSubmitId;

    /// <summary>Integration drill seam: the real approval-answer path.</summary>
    internal Task RespondApprovalForTestsAsync(string requestId, ApprovalDecision decision) =>
        RespondApprovalAsync(requestId, decision);

    /// <summary>C3 drill seam: the task-detail read path a selection triggers.
    /// F07: it runs under the CURRENT selection ordinal, exactly like a real
    /// selection, so era/selection fencing is exercised as production does.</summary>
    internal Task LoadTaskDetailForTestsAsync(string taskId) =>
        LoadTaskDetailAsync(taskId, Volatile.Read(ref _selectionSequence));

    /// <summary>C3 drill seam: the change-journal read path.</summary>
    internal Task RefreshChangesForTestsAsync() => RefreshChangesAsync();

    /// <summary>C3 drill seam: the artifact read path.</summary>
    internal Task ReadArtifactForTestsAsync(string reference)
    {
        ArtifactReference = reference;
        return ReadArtifactAsync();
    }

    /// <summary>C2 drill seam: the paging continuation path.</summary>
    internal Task ReadNextArtifactPageForTestsAsync() => ReadNextArtifactPageAsync();

    /// <summary>C2 drill seam: the tail-jump path.</summary>
    internal Task ReadArtifactTailForTestsAsync() => ReadArtifactTailAsync();

    /// <summary>C3 drill seam: the read-only context read path.</summary>
    internal Task RefreshContextForTestsAsync() => RefreshContextAsync();

    /// <summary>Bound drill seam: writes through the same bounded output
    /// path the events and receipts use.</summary>
    internal void AppendOutputForTests(string line) => AppendOutput(line);

    /// <summary>C2 drill seam: writes through the bounded log path.</summary>
    internal void AppendLogForTests(string line) => AppendLog(line);

    /// <summary>R14 drill observation: pending delta chars awaiting the UI,
    /// capped at the source by the output budget.</summary>
    internal int PendingUiDeltaCharsForTests
    {
        get { lock (_handoffGate) { return _pendingUiDelta.Length; } }
    }

    /// <summary>R14 drill observation: whether one bounded UI drain is posted
    /// (single-flight — at most one pending callback exists at a time).</summary>
    internal bool PendingUiDrainPostedForTests
    {
        get { lock (_handoffGate) { return _uiDrainPosted; } }
    }

    private void AppendOutput(string line) => OutputText = AppendBoundedLine(_outputBuilder, line);

    // -----------------------------------------------------------------------
    // C2 (GUI-2): 输出正文与运行日志分离。OutputText is the MODEL's own
    // content (streamed deltas only); LogText is the workbench's operational
    // record (receipts, failures, accounting). A log line is never presented
    // as the task's result, and the result panel is never padded with
    // operational prose.
    // -----------------------------------------------------------------------

    /// <summary>C2: bounded operational log (receipts/failures/accounting),
    /// same retention bounds as the output panel.</summary>
    private readonly StringBuilder _logBuilder = new();
    private string _logText = string.Empty;

    public string LogText { get => _logText; private set => Set(ref _logText, value); }

    private void AppendLog(string line) => LogText = AppendBoundedLine(_logBuilder, line);

    /// <summary>The shared bounded-append: whole-line retention cap plus the
    /// byte budget, dropping the OLDEST lines only.</summary>
    private static string AppendBoundedLine(StringBuilder builder, string line)
    {
        builder.AppendLine(line);
        var text = builder.ToString();
        var lines = text.Split('\n');
        if (lines.Length > MaxOutputLines)
        {
            text = string.Join('\n', lines[^MaxOutputLines..]);
        }
        // N4: byte bound on top of the line bound — drop whole OLDEST lines
        // (never mid-line) until the retained text fits the budget.
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
        builder.Clear();
        builder.Append(text);
        return text;
    }

    /// <summary>
    /// F05: the transport the production connect actually builds. The
    /// <see cref="TransportKind.RealHost"/> default is PLATFORM-AWARE — it
    /// asks the SDK for the local transport of THIS OS (named pipe on
    /// Windows, workspace-scoped UDS on Unix) instead of falling through to
    /// the Windows pipe and throwing <c>PlatformNotSupportedException</c> on
    /// Linux. The two explicit kinds keep the user's typed endpoint verbatim,
    /// and the pipe kind still fails closed off Windows (it never pretends
    /// the custom endpoint is a socket).
    /// </summary>
    internal IAgentTransport BuildTransport() => BuildTransport(
        (TransportKind)TransportIndex, Endpoint, OperatingSystem.IsWindows());

    /// <summary>
    /// F05: the platform-aware decision, factored from OS detection so the
    /// non-Windows branch is testable on any host — the defect this pins
    /// (RealHost → Windows pipe) only misbehaves off Windows, so the drill
    /// must be able to drive that branch regardless of where the tests run.
    /// </summary>
    internal static IAgentTransport BuildTransport(
        TransportKind kind, string endpoint, bool isWindows) => kind switch
    {
        TransportKind.WindowsNamedPipe => new NamedPipeTransport(endpoint),
        TransportKind.UnixSocket => new UnixDomainSocketTransport(endpoint),
        // RealHost (default) and LayoutPreview: the platform-correct LOCAL
        // endpoint, never an OS-specific guess.
        _ => isWindows
            ? new NamedPipeTransport(AgentTransports.DefaultPipeName)
            : new UnixDomainSocketTransport(
                AgentTransports.DefaultSocketPathFor(Environment.CurrentDirectory)),
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
            // R13: a session-level overflow completed the previous event stream;
            // this reconnect rebuilt it. If the earlier pump had ended (overflow
            // or stream fault), restart it on the rebuilt stream so new events
            // reach the UI again — the bounded drain keeps at most one callback
            // in flight regardless.
            if (_eventPump is { IsCompleted: true })
            {
                StartEventPump(session);
            }
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
            AppendLog($"连接丢失：{failure.Message}");
        });
        _connection = session;
        Interlocked.Increment(ref _generation);
        IsConnected = true;
        AppendLog($"已连接：{TransportLabels[TransportIndex]} / {Endpoint}");
        StartEventPump(session);
        var generation = Volatile.Read(ref _generation);
        WorkSnapshotResponse first;
        try
        {
            first = await session.SnapshotAsync(_lifetime.Token);
        }
        catch (Exception failure)
        {
            AppendLog($"握手失败：{failure.Message}。未建立任何任务事实，可重试连接。");
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
        _deltaCoalescer?.Dispose();
        // R14: the coalescer's flush feeds the SINGLE bounded UI drain (below)
        // instead of a fresh Post per flush, so a stalled UI cannot build an
        // unbounded callback backlog over time.
        _deltaCoalescer = new DeltaCoalescer(
            EnqueueDeltaForUi,
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
                    RelayToUi(connection, notification);
                }
            }
        }
        catch (OperationCanceledException)
        {
            // Window closed / disconnected: the pump ends quietly.
        }
        catch (Exception failure)
        {
            _ui.Post(() => AppendLog(
                $"事件流结束：{failure.Message}。请从快照重建；挂起的审批不会自动通过。"));
        }
    }

    /// <summary>
    /// R14: the BOUNDED handoff from the event pump to the UI thread. A
    /// notification never posts one callback of its own: deltas accumulate in
    /// the coalescer and every other event lands in a small pending batch, and
    /// at most ONE drain callback is posted to the dispatcher at a time. A
    /// stalled UI therefore holds a bounded backlog (one callback + a text
    /// buffer capped by the output budget at the source) — never one callback
    /// per arrival. Durable events schedule the already single-flight
    /// coalesced snapshot refresh HERE, so a render dropped under backpressure
    /// never loses state: recovery is the snapshot, not a status line.
    /// </summary>
    private void RelayToUi(IAgentConnection connection, WorkEventNotification notification)
    {
        if (!notification.IsLiveOnlyProgress)
        {
            // State is recovered by the snapshot, decoupled from the render.
            ScheduleSnapshotRefresh();
        }
        if (notification.EventType == "model_delta")
        {
            if (TryEventString(notification.Envelope.Event, "delta", out var delta) && delta.Length > 0)
            {
                _deltaCoalescer?.Append(delta);
            }
            return;
        }
        var shedCostFact = false;
        lock (_handoffGate)
        {
            if (_pendingUiEvents.Count >= MaxPendingUiItems)
            {
                // COST-9 (R3-14): a shed render row's COST FACTS are
                // accumulated before the row leaves the bounded queue — the
                // totals stay exact even though the log rendering is lossy.
                // Rendering is a read-only projection of these accumulators,
                // never their authority.
                var shed = _pendingUiEvents[0].Item;
                _pendingUiEvents.RemoveAt(0);
                _costRenderRowsShed++;
                AccumulateCostFactFromShedRow(shed);
                shedCostFact = true;
            }
            _pendingUiEvents.Add((connection, notification));
            if (!_uiDrainPosted)
            {
                _uiDrainPosted = true;
                _ui.Post(DrainPendingUiEvents);
            }
        }
        if (shedCostFact)
        {
            // The accumulators changed off the UI thread: refresh the
            // summary projection through the dispatcher.
            _ui.Post(() => CostSummaryText = BuildCostSummaryText());
        }
    }

    /// <summary>R14: the coalescer's flush feeds the SAME single-flight drain
    /// instead of a fresh per-flush Post, and the pending delta text is capped
    /// at the source by the output budget — a paused UI accumulates at most
    /// <see cref="MaxPendingDeltaChars"/> of text (drop-oldest) behind one
    /// pending callback.</summary>
    private void EnqueueDeltaForUi(string text)
    {
        if (text.Length == 0)
        {
            return;
        }
        lock (_handoffGate)
        {
            // F07: stamp the buffer with the connection that produced it, so
            // a drain that runs after a disconnect/rebuild cannot flush a
            // retired era's prose into the new session's panel.
            _deltaEraConnection = _connection;
            _pendingUiDelta.Append(text);
            if (_pendingUiDelta.Length > MaxPendingDeltaChars)
            {
                // Source cap: keep the newest text in budget (progress is
                // newest-wins by contract); the oldest delta is shed here,
                // before it ever queued for the UI.
                _pendingUiDelta.Remove(0, _pendingUiDelta.Length - MaxPendingDeltaChars);
            }
            if (!_uiDrainPosted)
            {
                _uiDrainPosted = true;
                _ui.Post(DrainPendingUiEvents);
            }
        }
    }

    /// <summary>
    /// R14: the single UI-side drain. Runs on the UI thread in stream order;
    /// flushes pending delta text through the existing bounded
    /// <see cref="AppendOutput"/> and renders each pending event, then
    /// re-posts only if new work arrived while draining (otherwise it clears
    /// the posted flag). At most one such callback exists at a time.
    /// </summary>
    private void DrainPendingUiEvents()
    {
        string deltaText;
        List<(IAgentConnection Connection, WorkEventNotification Item)> batch;
        lock (_handoffGate)
        {
            deltaText = _pendingUiDelta.ToString();
            _pendingUiDelta = new StringBuilder();
            batch = _pendingUiEvents;
            _pendingUiEvents = new List<(IAgentConnection Connection, WorkEventNotification Item)>();
        }
        // F07: the pending buffered work is ERA-SCOPED. DisconnectCoreAsync
        // clears both buffers, so anything captured here belongs to a live
        // handoff; the per-event loop re-checks the connection instance anyway,
        // and each delta already carries its own connection stamp when queued
        // (see EnqueueDeltaForUi), so a retired era's prose can never paint the
        // new session's output panel.
        if (deltaText.Length > 0 && _deltaEraConnection is { } deltaEra
            && ReferenceEquals(_connection, deltaEra))
        {
            AppendOutput(deltaText);
        }
        foreach (var (connection, notification) in batch)
        {
            HandleEvent(connection, notification);
        }
        lock (_handoffGate)
        {
            if (_pendingUiDelta.Length > 0 || _pendingUiEvents.Count > 0)
            {
                // Work arrived (and was buffered) while we drained: keep a
                // drain posted rather than let the buffer sit idle.
                _ui.Post(DrainPendingUiEvents);
            }
            else
            {
                _uiDrainPosted = false;
            }
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
                AppendLog($"[{envelope.Seq}] assistant 消息已入账。");
                break;
            case "run_started":
                AppendLog($"[{envelope.Seq}] 运行已启动。");
                break;
            case "focus_changed":
                AppendLog(TryEventString(envelope.Event, "goal", out var goal) && goal.Length > 0
                    ? $"[{envelope.Seq}] 焦点任务：{goal}"
                    : $"[{envelope.Seq}] 焦点任务已变更。");
                break;
            case "turn_completed":
                AppendLog($"[{envelope.Seq}] 本轮结束（以快照为准）。");
                break;
            case "turn_cancelled":
                AppendLog($"[{envelope.Seq}] 本轮已取消（以快照为准）。");
                break;
            case "task_completed":
                AppendLog($"[{envelope.Seq}] 任务到达终态（结果与产出以快照为准）。");
                break;
            case "tool_started":
            case "tool_finished":
                AppendLog($"[{envelope.Seq}] {notification.EventType}（工具事件；结果内容不在此渲染）。");
                break;
            case "model_used":
                if (notification.Envelope.TryGetModelUsage(out var usage))
                {
                    RecordModelUsage(envelope.Seq, usage);
                }
                break;
            case "context_compacted":
                RecordCompactionCost(envelope.Seq, envelope.Event);
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

    /// <summary>C4: files one model round into its identity class and keeps
    /// the summary line honest — estimated and unknown rounds are visible
    /// counts, never folded into the observed bill nor shown as zero.
    /// COST-7: the maintenance/compactor lane keeps its own accumulators so
    /// a compactor row never blurs into the main-round bill.</summary>
    private void RecordModelUsage(ulong seq, RuntimeEventEnvelope.ModelUsageFact usage)
    {
        AccumulateModelUsage(usage);
        var retryNote = usage.Retries > 0
            ? $" · 含 {usage.Retries} 次重试（失败尝试通常无用量，数值为下界）"
            : string.Empty;
        var laneNote = usage.Role == "maintenance" ? "维护压缩调用" : "主调用";
        switch (usage.UsageIdentity)
        {
            case "observed" when usage.Role == "maintenance":
                AppendLog(
                    $"[{seq}] 模型消耗（实测·{laneNote}）：输入 {usage.InputTokens} · 输出 {usage.OutputTokens}" +
                    retryNote);
                break;
            case "observed":
                AppendLog(
                    $"[{seq}] 模型消耗（实测·{laneNote}）：输入 {usage.InputTokens} · 输出 {usage.OutputTokens}" +
                    (usage.CachedInputTokens > 0 ? $" · 缓存读 {usage.CachedInputTokens}" : string.Empty) +
                    retryNote);
                break;
            case "estimated":
                AppendLog(
                    $"[{seq}] 模型消耗（估算·{laneNote}）：输入 {usage.InputTokens} · 输出 {usage.OutputTokens}" +
                    "——运行时近似推导，非 provider 账单" + retryNote);
                break;
            default:
                AppendLog(
                    $"[{seq}] 模型消耗（未知·{laneNote}）：本轮用量证据丢失（例如取消在飞调用）——按未知计，不计为零" +
                    retryNote);
                break;
        }
        CostSummaryText = BuildCostSummaryText();
    }

    /// <summary>COST-9 (R3-14): the fixed-size cost accumulators are the
    /// authority; the log rows are a lossy projection. Both the live render
    /// path and the shed-row path feed the SAME accumulators exactly once.
    /// </summary>
    private void AccumulateModelUsage(RuntimeEventEnvelope.ModelUsageFact usage)
    {
        switch (usage.UsageIdentity)
        {
            case "observed" when usage.Role == "maintenance":
                _costMaintenanceRounds++;
                _costMaintenanceInput += usage.InputTokens;
                _costMaintenanceOutput += usage.OutputTokens;
                break;
            case "observed":
                _costObservedInput += usage.InputTokens;
                _costObservedOutput += usage.OutputTokens;
                _costObservedCached += usage.CachedInputTokens;
                _costObservedRounds++;
                break;
            case "estimated":
                _costEstimatedInput += usage.InputTokens;
                _costEstimatedOutput += usage.OutputTokens;
                _costEstimatedRounds++;
                break;
            default:
                _costUnknownRounds++;
                break;
        }
    }

    /// <summary>COST-9 (R3-14): a shed render row still contributes its cost
    /// fact before it leaves the bounded queue — the account never loses a
    /// billed call to the rendering cap.</summary>
    private void AccumulateCostFactFromShedRow(WorkEventNotification notification)
    {
        var envelope = notification.Envelope;
        switch (notification.EventType)
        {
            case "model_used" when envelope.TryGetModelUsage(out var usage):
                AccumulateModelUsage(usage);
                break;
            case "context_compacted":
                AccumulateCompactionCost(envelope.Event);
                break;
        }
    }

    /// <summary>C4: compaction cost read straight off the typed
    /// <c>context_compacted</c> fields. The event carries no usage identity,
    /// so the row says "service-reported" and stays OUTSIDE the observed
    /// model bill — the GUI never upgrades a reported number into a measured
    /// one.</summary>
    private void RecordCompactionCost(ulong seq, System.Text.Json.JsonElement @event)
    {
        ulong U64(string name) =>
            @event.ValueKind == System.Text.Json.JsonValueKind.Object
            && @event.TryGetProperty(name, out var element)
            && element.TryGetUInt64(out var value)
                ? value
                : 0UL;
        var reason = @event.ValueKind == System.Text.Json.JsonValueKind.Object
            && @event.TryGetProperty("reason", out var reasonElement)
            && reasonElement.ValueKind == System.Text.Json.JsonValueKind.String
                ? reasonElement.GetString() ?? "?"
                : "?";
        // COST-1 (E05.3): newer events carry the compaction's own usage
        // identity; legacy events omit it. The row renders whichever fact
        // the wire provided — never an upgrade to "measured" by the GUI.
        var identity = @event.ValueKind == System.Text.Json.JsonValueKind.Object
            && @event.TryGetProperty("usage_identity", out var identityElement)
            && identityElement.ValueKind == System.Text.Json.JsonValueKind.String
                ? identityElement.GetString()
                : null;
        var identityText = identity switch
        {
            "observed" => "实测",
            "estimated" => "估算（运行时近似，非 provider 账单）",
            "unknown" => "usage 未知（按未知计，不计为零）",
            null or "" => "服务端报告，未带实测身份",
            var other => $"身份:{other}",
        };
        var input = U64("input_tokens");
        var output = U64("output_tokens");
        AccumulateCompactionCost(@event);
        // COST-2 (E05.4): the event now carries the compressor call's own
        // cache-read and attempt counters — rendered verbatim, still outside
        // the model bill.
        var cached = U64("cached_input_tokens");
        var attempts = U64("attempts");
        var counters = cached > 0 ? $" · 输入 {input} · 输出 {output} tokens（缓存读 {cached}）" : $" · 输入 {input} · 输出 {output} tokens";
        var attemptNote = attempts > 0 ? $" · 尝试 {attempts}" : string.Empty;
        AppendLog(
            $"[{seq}] 压缩消耗（{identityText}）：原因 {reason}{counters}{attemptNote} · 移出 {U64("source_items")} 条——不并入主调用实测合计");
        CostSummaryText = BuildCostSummaryText();
    }

    /// <summary>COST-9 (R3-14): compaction totals accumulate independently
    /// of the (lossy) log rendering — the shed-row path feeds the same
    /// fixed-size accumulators exactly once.</summary>
    private void AccumulateCompactionCost(System.Text.Json.JsonElement @event)
    {
        ulong U64(string name) =>
            @event.ValueKind == System.Text.Json.JsonValueKind.Object
            && @event.TryGetProperty(name, out var element)
            && element.TryGetUInt64(out var value)
                ? value
                : 0UL;
        _costCompactions++;
        _costCompactionInput += U64("input_tokens");
        _costCompactionOutput += U64("output_tokens");
    }

    /// <summary>C4: one summary line whose identity classes are never
    /// merged — observed is the bill, estimated/unknown are visible counts,
    /// and compaction is service-reported cost outside the bill.</summary>
    private string BuildCostSummaryText() =>
        $"成本账目：实测 {_costObservedRounds} 轮（输入 {_costObservedInput} · 输出 {_costObservedOutput}" +
        $" · 缓存读 {_costObservedCached}）" +
        (_costEstimatedRounds > 0
            ? $" · 估算 {_costEstimatedRounds} 轮（输入 {_costEstimatedInput} · 输出 {_costEstimatedOutput}）"
            : string.Empty) +
        (_costUnknownRounds > 0 ? $" · 未知 {_costUnknownRounds} 轮" : string.Empty) +
        (_costMaintenanceRounds > 0
            ? $" · 维护压缩调用 {_costMaintenanceRounds} 轮（输入 {_costMaintenanceInput} · 输出 {_costMaintenanceOutput}）"
            : string.Empty) +
        (_costCompactions > 0
            ? $" · 压缩 {_costCompactions} 次（输入 {_costCompactionInput} · 输出 {_costCompactionOutput}，服务端报告，未带实测身份）"
            : string.Empty);

    private void ResetCostAccount()
    {
        _costObservedInput = 0;
        _costObservedOutput = 0;
        _costObservedCached = 0;
        _costObservedRounds = 0;
        _costEstimatedInput = 0;
        _costEstimatedOutput = 0;
        _costEstimatedRounds = 0;
        _costUnknownRounds = 0;
        _costMaintenanceRounds = 0;
        _costMaintenanceInput = 0;
        _costMaintenanceOutput = 0;
        _costCompactions = 0;
        _costCompactionInput = 0;
        _costCompactionOutput = 0;
        _costRenderRowsShed = 0;
        CostSummaryText = "成本账目：暂无模型调用。";
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
                    AppendLog($"刷新失败：{failure.Message}（下一轮或手动刷新会重试。）");
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
            AppendLog($"任务列表超过 {MaxRenderedTasks} 条，界面只保留前 {MaxRenderedTasks} 条（有界保留）。");
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
        // GUI-3: a trusted snapshot re-derives an unresolved cancel phase
        // (Requested/Unknown) from typed facts — but never while the request
        // that owns the phase is still in flight.
        if (Volatile.Read(ref _cancelInFlight) == 0
            && (CancelPhase is CancelRequestPhase.Requested or CancelRequestPhase.Unknown))
        {
            SetCancelPhase(CancelRequestPhase.Idle, snapshot.RunStarted
                ? "以快照为准解除未决的取消请求：快照显示运行仍在进行；该请求是否已生效以快照与事件为准。"
                : "以快照为准解除未决的取消请求：快照显示当前没有进行中的运行。");
        }
        // GUI-3/F06: a new snapshot is a fresh chance for the ledger to
        // testify about an outstanding unknown submit — by exact request
        // identity, never by goal text.
        _ = ResolveOutstandingSubmitByQueryAsync();
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
                        onError: error => AppendLog($"审批失败：{error.Message}")),
                    AsyncCommands.Add(
                        () => RespondApprovalAsync(requestId, ApprovalDecision.Deny),
                        onError: error => AppendLog($"审批失败：{error.Message}")));
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

    /// <summary>GUI-3/F06: every applied snapshot (and every newly recorded
    /// unknown receipt) asks the run's submission ledger about the EXACT
    /// client_request_id — with the payload digest computed over the caller's
    /// own goal — via the read-only <c>work.submit_result</c> route. The
    /// snapshot's task list is NEVER consulted: a same-goal task (old,
    /// renamed, or coincidental) is not evidence about this request id, and a
    /// bounded list or a goal past the display cap proves nothing either way.
    ///
    /// Every disposition is rendered as the fact it is:
    ///   Accepted/AlreadyAccepted — the ledger testifies this exact request
    ///     was admitted; the unknown is resolved and the key released.
    ///   KnownRejected — the id was admitted for a DIFFERENT payload; this
    ///     request was never admitted and the id is dead, so it is released
    ///     (the operator's next submit takes a fresh identity).
    ///   Unknown/Expired — the ledger has no surviving evidence; the outcome
    ///     is genuinely undetermined, nothing is auto-resent, and the key
    ///     stays so a same-goal retry remains an idempotent re-read.
    ///
    /// The answer only lands while the connection era that served the query
    /// is still the live one and the echoed id matches — a retired era's or a
    /// mismatched receipt proves nothing here (the next snapshot re-asks).</summary>
    private async Task ResolveOutstandingSubmitByQueryAsync()
    {
        var id = _outstandingSubmitId;
        var goal = _outstandingSubmitGoal;
        if (id is null || goal is null || _outstandingSubmitAwaitingReceipt)
        {
            return;
        }
        var connection = _connection;
        if (connection is null)
        {
            return;
        }
        if (Interlocked.CompareExchange(ref _submitResultQueryInFlight, 1, 0) != 0)
        {
            return;
        }
        try
        {
            var generation = Volatile.Read(ref _generation);
            WorkSubmitResultResponse answer;
            try
            {
                answer = await connection.SubmitResultAsync(
                    id, SubmitPayloadDigest.Compute(goal), _lifetime.Token);
            }
            catch (OperationCanceledException)
            {
                return; // the window closed or the era ended; nothing resolved here
            }
            catch (Exception failure)
            {
                ReportOutstandingResolutionOnce(
                    $"提交核对失败：{failure.Message}。该提交保持未知；不会自动重发，之后每份快照都会再向受理账本核对一次。");
                return;
            }
            if (generation != Volatile.Read(ref _generation)
                || !ReferenceEquals(_connection, connection)
                || answer.ClientRequestId != id)
            {
                return; // a retired era's or mismatched answer is not evidence
            }
            switch (answer.Disposition)
            {
                case WorkSubmitResultDisposition.Accepted:
                case WorkSubmitResultDisposition.AlreadyAccepted:
                    ClearOutstandingSubmit();
                    AppendLog(
                        $"未知提交已由受理账本核实：client_request_id {id} 已受理为 task {answer.TaskId}"
                        + "（该结论来自精确请求查询，不凭同名目标；不会自动重发）。");
                    break;
                case WorkSubmitResultDisposition.KnownRejected:
                    ClearOutstandingSubmit();
                    var digest = answer.AcceptedPayloadDigest is { Length: > 0 } recorded
                        ? (recorded.Length <= 16 ? recorded : recorded[..16] + "…")
                        : "（无摘要）";
                    AppendLog(
                        $"提交核对完成：同一 client_request_id 曾以不同内容提交（原内容摘要前缀 {digest}）。"
                        + "这次提交从未被受理，该 id 已不可复用；再次提交同一目标将以新身份执行（需你再次操作）。");
                    break;
                default:
                    ReportOutstandingResolutionOnce(
                        $"提交核对完成：受理账本无法证明该提交的结果（{answer.Disposition}——无证据，或证据已随宿主进程终止而淘汰）。"
                        + "该提交保持未知：不会自动重发；同一目标再提交仍用同一身份幂等重试，账本核对会随每份快照继续。");
                    break;
            }
        }
        finally
        {
            Interlocked.Exchange(ref _submitResultQueryInFlight, 0);
        }
    }

    private void ClearOutstandingSubmit()
    {
        _outstandingSubmitId = null;
        _outstandingSubmitGoal = null;
        _outstandingSubmitAwaitingReceipt = false;
        _outstandingSubmitResolutionReported = false;
    }

    /// <summary>Indeterminate or failed resolutions report once per
    /// outstanding id — every snapshot re-asks the ledger while the key is
    /// outstanding, and repeating the same "still unknown" line would only
    /// bury the output panel.</summary>
    private void ReportOutstandingResolutionOnce(string line)
    {
        if (_outstandingSubmitResolutionReported)
        {
            return;
        }
        _outstandingSubmitResolutionReported = true;
        AppendLog(line);
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
            AppendLog("上一次提交的结果未知，且这次目标不同。先刷新快照（会自动向受理账本核对）；重复同一目标才会幂等重试。");
            return;
        }
        var clientRequestId = _outstandingSubmitId ?? ClientRequestIds.Next();
        _outstandingSubmitId = clientRequestId;
        _outstandingSubmitGoal = goal;
        _outstandingSubmitAwaitingReceipt = true;
        _outstandingSubmitResolutionReported = false;
        WorkSubmitResponse receipt;
        try
        {
            receipt = await connection.SubmitWorkAsync(goal, clientRequestId, _lifetime.Token);
        }
        catch (AgentUnknownOutcomeException unknown)
        {
            // Pending -> Unknown: the key stays outstanding ON PURPOSE, and
            // the exact-request ledger query — not a blind retry, and never a
            // goal-text match — resolves it.
            _outstandingSubmitAwaitingReceipt = false;
            AppendLog($"提交结果未知：连接在请求期间断开（{unknown.Failure.Message}）。不会自动重发；"
                + "正在以精确请求查询（client_request_id＋内容摘要）向受理账本核对，不凭同名目标猜测。");
            _ = ResolveOutstandingSubmitByQueryAsync();
            return;
        }
        catch (AgentProtocolException failure)
        {
            // A structured server answer is DEFINITIVE evidence that k
            // was denied — no admission was created, so clearing is safe and honest.
            ClearOutstandingSubmit();
            AppendLog($"提交被拒绝：{failure.Message}");
            return;
        }
        catch (Exception failure)
        {
            // R07: a timed-out / transport-faulted in-flight request may or may
            // not have been delivered — the receipt is UNKNOWN. Keep k so a
            // same-goal retry reuses the SAME admission identity; the ledger
            // query resolves it from facts. (Only clearing is what let each
            // retry turn into a new admission.)
            _outstandingSubmitAwaitingReceipt = false;
            AppendLog($"提交结果未知：{failure.Message}。不会自动重发；"
                + "同一目标会以相同身份幂等重试，账本核对会随快照继续。");
            _ = ResolveOutstandingSubmitByQueryAsync();
            return;
        }
        ClearOutstandingSubmit();
        AppendLog($"已受理：task {receipt.TaskId}（{receipt.Disposition}）。受理 ≠ 完成，完成以快照与事件为准。");
        GoalInput = string.Empty;
        await RefreshSnapshotCoreAsync(silent: true);
    }

    /// <summary>GUI-3: continue re-drives the run's ACTIVE task — the wire
    /// carries no task binding — so the receipt is compared with the
    /// operator's selection and any difference is stated plainly. Nothing
    /// here starts another task or switches the selection behind the
    /// operator's back.</summary>
    private async Task ContinueAsync()
    {
        var connection = _connection;
        if (connection is null)
        {
            return;
        }
        var selectedTaskId = SelectedTask?.TaskId;
        WorkContinueResponse receipt;
        try
        {
            receipt = await connection.ContinueAsync(_lifetime.Token);
        }
        catch (AgentUnknownOutcomeException unknown)
        {
            AppendLog($"继续结果未知：连接在请求期间断开（{unknown.Failure.Message}）。从快照核对当前状态后再决定。");
            return;
        }
        catch (Exception failure)
        {
            AppendLog($"继续失败：{failure.Message}");
            return;
        }
        var line = $"已继续活动任务 {receipt.TaskId}（沿用该任务当前指令；受理 ≠ 完成，以快照与事件为准）。";
        if (!string.IsNullOrEmpty(selectedTaskId) && selectedTaskId != receipt.TaskId)
        {
            line += $"注意：继续的是运行的活动任务，不是当前选中的任务 {selectedTaskId}——继续动作始终跟随活动任务，不隐式切换选择或新启任务。";
        }
        AppendLog(line);
        await RefreshSnapshotCoreAsync(silent: true);
    }

    /// <summary>GUI-3: the cancel surface's phase machine. The request is
    /// shown as REQUESTED from the moment it is sent; only typed facts move
    /// it — the ack's <c>Cancelled</c> proves the barrier, <c>NoActiveTurn</c>
    /// is a fact (not a failure), and an undetermined outcome keeps the
    /// side-effect warning (the turn may or may not have stopped; nothing is
    /// re-sent). A trusted snapshot re-derives an unresolved phase once no
    /// request is in flight.</summary>
    private async Task CancelAsync()
    {
        var connection = _connection;
        if (connection is null)
        {
            return;
        }
        if (Interlocked.CompareExchange(ref _cancelInFlight, 1, 0) != 0)
        {
            return;
        }
        try
        {
            var generation = Volatile.Read(ref _generation);
            SetCancelPhase(CancelRequestPhase.Requested, "取消请求已发出，等待服务端确认——在可信确认前不视为已停止。");
            WorkCancelResponse receipt;
            try
            {
                receipt = await connection.CancelCurrentTurnAsync(_lifetime.Token);
            }
            catch (AgentUnknownOutcomeException unknown)
            {
                // A retired era already retired this phase (disconnect); its
                // verdict must not resurrect a state on the new view.
                if (!IsCurrentEra(generation, connection))
                {
                    return;
                }
                SetCancelPhase(CancelRequestPhase.Unknown,
                    $"取消结果不可确定：连接在请求期间断开（{unknown.Failure.Message}）。"
                    + "该轮可能已停止也可能未停止——副作用以快照与事件为准；不会自动重发取消请求。");
                return;
            }
            catch (Exception failure)
            {
                if (!IsCurrentEra(generation, connection))
                {
                    return;
                }
                SetCancelPhase(CancelRequestPhase.Unknown,
                    $"取消结果不可确定：{failure.Message}。该轮可能已停止也可能未停止——副作用以快照与事件为准；不会自动重发取消请求。");
                return;
            }
            if (!IsCurrentEra(generation, connection))
            {
                return;
            }
            SetCancelPhase(receipt.Ack.Status switch
            {
                TurnCancelAckStatus.Cancelled => CancelRequestPhase.Cancelled,
                _ => CancelRequestPhase.NoActiveTurn,
            }, receipt.Ack.Status switch
            {
                TurnCancelAckStatus.Cancelled =>
                    $"已确认取消（屏障已过）：generation {receipt.Ack.CancelledGeneration} → {receipt.Ack.EffectiveGeneration}。",
                _ => "当前没有活动轮次（这是事实，不是失败）。",
            });
            await RefreshSnapshotCoreAsync(silent: true);
        }
        finally
        {
            Interlocked.Exchange(ref _cancelInFlight, 0);
        }
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
    /// <c>work.task_detail</c> read (triggered by selecting a task). F07: the
    /// read is bound to the selection ordinal it was issued for, so both a
    /// late success AND a late failure from a previous selection are retired
    /// instead of painting the panel that now belongs to another task.</summary>
    private async Task LoadTaskDetailAsync(string taskId, long selection)
    {
        var connection = _connection;
        if (connection is null)
        {
            TaskDetailText = "任务详情：未连接（unavailable）。";
            return;
        }
        // F07: capture the fencing token BEFORE the await; every panel write
        // below (success, failure) re-checks it against the live era.
        var era = CaptureRequestEra(selection);
        WorkTaskDetailResponse detail;
        try
        {
            detail = await connection.TaskDetailAsync(taskId, _lifetime.Token);
        }
        catch (Exception failure)
        {
            // F07: a failed read only reports "unavailable" while it is still
            // THIS era's and THIS selection's request. A late failure from a
            // replaced connection or a previous selection leaves the newer
            // panel untouched.
            if (!IsRequestCurrent(era, connection))
            {
                return;
            }
            if (!_lifetime.IsCancellationRequested)
            {
                AppendLog($"任务详情读取失败：{failure.Message}");
            }
            TaskDetailText = "任务详情：读取失败（unavailable）。";
            return;
        }
        if (!IsRequestCurrent(era, connection))
        {
            return; // a newer connection or selection owns the panel now
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
        var era = CaptureRequestEra();
        WorkChangesResponse changes;
        try
        {
            changes = await connection.ReadChangesAsync(
                limit: MaxRenderedChanges, cancellationToken: _lifetime.Token);
        }
        catch (Exception failure)
        {
            // F07: a late failure from a replaced era must not clear a fresh
            // panel — only report it while this request is still current.
            if (!IsRequestCurrent(era, connection))
            {
                return;
            }
            if (!_lifetime.IsCancellationRequested)
            {
                AppendLog($"变更读取失败：{failure.Message}");
            }
            ChangesStatusText = "变更日志：读取失败（unavailable）。";
            return;
        }
        if (!IsRequestCurrent(era, connection))
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

    /// <summary>C3/C2: reads one artifact by reference over the B3/PLATFORM-2
    /// paged read. A fresh read restarts the review window at byte 0; the
    /// response's own size/cursor facts drive the honest header and the
    /// 「下一页」 command. Reads are observations — they never re-trigger a
    /// model or tool side effect.</summary>
    private Task ReadArtifactAsync()
    {
        var connection = _connection;
        var reference = ArtifactReference.Trim();
        if (connection is null)
        {
            ArtifactText = "工件：未连接（unavailable）。";
            return Task.CompletedTask;
        }
        if (reference.Length == 0)
        {
            ArtifactText = "工件：未提供引用（unavailable）。";
            return Task.CompletedTask;
        }
        return ReadArtifactIntoWindowAsync(connection, reference, offset: 0, resetWindow: true);
    }

    /// <summary>C2: continues the current paging session from the server's
    /// own cursor. Disabled at eof by the command's gate; a stale-era or
    /// failed continuation leaves the window untouched.</summary>
    private Task ReadNextArtifactPageAsync()
    {
        var connection = _connection;
        if (connection is null
            || _artifactNextOffset is not { } next
            || _artifactSessionReference is not { } reference)
        {
            return Task.CompletedTask;
        }
        return ReadArtifactIntoWindowAsync(connection, reference, offset: next, resetWindow: false);
    }

    /// <summary>C2: jumps to the artifact's last window (a one-byte probe
    /// learns the size, then the final window is read fresh). Key conclusions
    /// at the tail of a large artifact are reachable in two read-only calls
    /// instead of paging through everything.</summary>
    private async Task ReadArtifactTailAsync()
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
        if (Interlocked.CompareExchange(ref _artifactReadInFlight, 1, 0) != 0)
        {
            return;
        }
        ulong tailOffset;
        try
        {
            var era = CaptureRequestEra();
            WorkArtifactResponse probe;
            try
            {
                probe = await connection.ReadArtifactAsync(
                    reference, maxBytes: 1, offset: 0, cancellationToken: _lifetime.Token);
            }
            catch (Exception failure)
            {
                if (!IsRequestCurrent(era, connection) || _lifetime.IsCancellationRequested)
                {
                    return;
                }
                AppendLog($"工件读取失败：{failure.Message}");
                ArtifactText = "工件：读取失败（unavailable）。";
                return;
            }
            if (!IsRequestCurrent(era, connection))
            {
                return;
            }
            tailOffset = probe.SizeBytes <= WorkArtifactRequest.MaxArtifactReadBytes
                ? 0UL
                : probe.SizeBytes - WorkArtifactRequest.MaxArtifactReadBytes;
        }
        finally
        {
            Interlocked.Exchange(ref _artifactReadInFlight, 0);
        }
        await ReadArtifactIntoWindowAsync(connection, reference, offset: tailOffset, resetWindow: true);
    }

    /// <summary>The one paged-read path: era-fenced wire call, then the
    /// window apply. The sealed artifact is immutable, so consecutive pages
    /// re-verify the same identity — paging cannot quietly switch versions.</summary>
    private async Task ReadArtifactIntoWindowAsync(
        IAgentConnection connection, string reference, ulong offset, bool resetWindow)
    {
        if (Interlocked.CompareExchange(ref _artifactReadInFlight, 1, 0) != 0)
        {
            return;
        }
        try
        {
            var era = CaptureRequestEra();
            WorkArtifactResponse page;
            try
            {
                page = await connection.ReadArtifactAsync(
                    reference,
                    maxBytes: WorkArtifactRequest.MaxArtifactReadBytes,
                    offset: offset,
                    cancellationToken: _lifetime.Token);
            }
            catch (Exception failure)
            {
                // F07 fencing: a stale era's failure never overwrites the panel.
                if (!IsRequestCurrent(era, connection))
                {
                    return;
                }
                if (!_lifetime.IsCancellationRequested)
                {
                    AppendLog($"工件读取失败：{failure.Message}");
                }
                ArtifactText = "工件：读取失败（unavailable）。";
                return;
            }
            if (!IsRequestCurrent(era, connection))
            {
                return;
            }
            ApplyArtifactPage(reference, page, resetWindow);
        }
        finally
        {
            Interlocked.Exchange(ref _artifactReadInFlight, 0);
        }
    }

    private void ApplyArtifactPage(string reference, WorkArtifactResponse page, bool resetWindow)
    {
        byte[] body;
        try
        {
            body = Convert.FromBase64String(page.ContentBase64);
        }
        catch (FormatException)
        {
            // The client-side validator already rejected invalid base64; this
            // defensive fallback never renders a guessed body.
            ArtifactText = "工件：读取失败（响应不是合法 base64）.";
            return;
        }
        if (resetWindow || _artifactSessionReference != page.Reference)
        {
            _artifactWindow.Clear();
            _artifactWindowStart = page.Offset;
        }
        _artifactSessionReference = page.Reference;
        _artifactWindow.AddRange(body);

        // Bounded window: release the OLDEST bytes at the hard cap, never
        // silently — the panel names the release, and the full artifact stays
        // on the host (re-「读取」 restarts from byte 0).
        var releaseNote = string.Empty;
        while (_artifactWindow.Count > MaxArtifactWindowBytes)
        {
            var cut = _artifactWindow.Count / 4;
            while (cut < _artifactWindow.Count && (_artifactWindow[cut] & 0xC0) == 0x80)
            {
                cut++; // never release half a UTF-8 sequence: cut before a lead byte
            }
            _artifactWindow.RemoveRange(0, cut);
            _artifactWindowStart += (ulong)cut;
            releaseNote = "…（窗口已达上限，最旧的字节已从面板释放；重新「读取」可回到开头）…\n";
        }

        _artifactNextOffset = page.NextOffset;
        ArtifactNextOffset = page.NextOffset;

        // Incremental UTF-8 across page boundaries: a multibyte character cut
        // by the page edge is held back (bytes stay in the window) instead of
        // rendering a replacement character; the next page completes it. At
        // eof everything is decoded as-is.
        var displayBytes = page.Truncated
            ? _artifactWindow.GetRange(0, _artifactWindow.Count - IncompleteUtf8TailLength(_artifactWindow))
            : _artifactWindow.ToList();
        var text = Encoding.UTF8.GetString(displayBytes.ToArray());
        var tailNote = page.Truncated && displayBytes.Count < _artifactWindow.Count
            ? "\n（末尾多字节字符被页边界切分，读取下一页后补全）"
            : string.Empty;
        var eofNote = page.Truncated
            ? $"后续还有（下一页从字节 {page.NextOffset} 开始）"
            : "已到文件末尾（eof）";
        var header = $"工件 {page.Reference}：{page.SizeBytes} 字节"
            + $" · 窗口 [{_artifactWindowStart},{_artifactWindowStart + (ulong)_artifactWindow.Count})"
            + $" · {eofNote}"
            + $" · 每页至多 {WorkArtifactRequest.MaxArtifactReadBytes} 字节";
        ArtifactText = $"{header}\n{releaseNote}{text}{tailNote}";
        AsyncCommands.RaiseCanExecute();
    }

    /// <summary>C2: length of an incomplete trailing UTF-8 sequence in the
    /// window (its bytes stay buffered; only the display holds them back).
    /// Trailing ASCII (and complete sequences) return 0.</summary>
    private static int IncompleteUtf8TailLength(List<byte> buffer)
    {
        var continuations = 0;
        var i = buffer.Count;
        while (i > 0 && continuations < 4 && (buffer[i - 1] & 0xC0) == 0x80)
        {
            i--;
            continuations++;
        }
        if (i == 0)
        {
            return continuations; // no lead byte in the window at all
        }
        var lead = buffer[i - 1];
        var expected =
            (lead & 0xF8) == 0xF0 ? 4 :
            (lead & 0xF0) == 0xE0 ? 3 :
            (lead & 0xE0) == 0xC0 ? 2 : 1;
        return continuations + 1 < expected ? continuations + 1 : 0;
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
        var era = CaptureRequestEra();
        WorkContextResponse context;
        try
        {
            context = await connection.ReadContextAsync(
                limit: MaxRenderedContextItems, cancellationToken: _lifetime.Token);
        }
        catch (Exception failure)
        {
            if (!IsRequestCurrent(era, connection))
            {
                return; // stale era: never overwrite a newer panel with a failure
            }
            if (!_lifetime.IsCancellationRequested)
            {
                AppendLog($"上下文读取失败：{failure.Message}");
            }
            ContextStatusText = "只读 Context：读取失败（unavailable）。";
            return;
        }
        if (!IsRequestCurrent(era, connection))
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

    /// <summary>
    /// F13-era guard: true only while the captured generation is still the
    /// live one AND the connection that served the read is still
    /// installed — a read from a replaced or disconnected era is dropped.
    /// </summary>
    private bool IsCurrentEra(int generation, IAgentConnection connection) =>
        generation == Volatile.Read(ref _generation) && ReferenceEquals(_connection, connection);

    /// <summary>
    /// F07: one read-only request's fencing token. A panel update is only
    /// allowed when ALL of the following still hold:
    /// <list type="bullet">
    /// <item>the connection epoch (generation) the request started under is
    /// still the live one — a disconnect or reconnect retires it;</item>
    /// <item>the exact connection instance that served the request is still
    /// installed — a rebuilt session is a different object;</item>
    /// <item>for per-selection reads, the selection the request was issued
    /// for is still the current one — task A's late answer must never land on
    /// task B's panel.</item>
    /// </list>
    /// The SAME token gates the success, failure and finally paths, so a late
    /// error from a replaced era can no longer overwrite a fresh panel with
    /// "unavailable". Captured once per request at issue time; evaluated
    /// exactly once when the await settles.
    /// </summary>
    private readonly record struct RequestEra(long Selection)
    {
        /// <summary>A read not bound to any particular task selection
        /// (changes / artifact / context / snapshot).</summary>
        public static readonly RequestEra Unbound = new(0);
    }

    /// <summary>Monotonic selection ordinal: incremented every time
    /// <see cref="SelectedTask"/> changes (including to null), so a slow
    /// task-detail read for a previous selection is retired.</summary>
    private long _selectionSequence;

    /// <summary>F07: capture the fencing token for a read-only request issued
    /// right now. <paramref name="selection"/> is the ordinal a
    /// selection-bound read was issued for, or 0 for an unbound read.</summary>
    private RequestEra CaptureRequestEra(long selection = 0) =>
        new(selection);

    /// <summary>F07: whether a captured request's panel update may still be
    /// applied. Evaluated on every visible state write of a read path —
    /// success, catch and finally alike.</summary>
    private bool IsRequestCurrent(RequestEra era, IAgentConnection connection) =>
        IsCurrentEra(Volatile.Read(ref _generation), connection)
        && (era.Selection == 0 || era.Selection == Volatile.Read(ref _selectionSequence));

    /// <summary>F07: the current selection ordinal (drill observation).</summary>
    internal long SelectionSequenceForTests => Volatile.Read(ref _selectionSequence);

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
            AppendLog($"审批 {requestId} 未送达：{failure.Message}。请刷新快照后基于事实重新决定；不会自动重发。");
            await RefreshSnapshotCoreAsync(silent: true);
            return;
        }
        AppendLog(outcome.Outcome == ApprovalRespondOutcome.Delivered
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
        // F07: drop the retired era's buffered handoff — its pending events
        // carry a dead connection (dropped by HandleEvent) and its coalesced
        // delta text has no per-item stamp. The delta buffer's own era alone
        // would not retire it: if a NEW connection had already been installed
        // when a stale drain ran, the stamp would still match a live object
        // only by accident of ordering, so the buffer is cleared here, at the
        // single era boundary, together with the panels.
        lock (_handoffGate)
        {
            _pendingUiEvents.Clear();
            _pendingUiDelta.Clear();
            _deltaEraConnection = null;
            // C4: the cost account belongs to the connected era. A retired
            // connection's round counters are dropped with its buffers —
            // they are re-accumulated from typed facts on the live era.
            ResetCostAccount();
        }
        IsConnected = false;
        BannerText = string.Empty;
        // GUI-3: a disconnected era can never confirm a pending cancel; the
        // phase is retired here (a late ack is dropped by the era guard in
        // CancelAsync's snapshot path — the state below stays honest).
        SetCancelPhase(CancelRequestPhase.Idle, "已断开：未决的取消请求状态失效；重连后以快照为准。");
        Tasks.ReplaceWith([]);
        ApplyApprovals([]);
        // C3: the review panels are honest on disconnect — no stale server
        // facts survive into the next era.
        SelectedTask = null;
        TaskDetailText = "任务详情：未连接（unavailable）。";
        Changes.ReplaceWith([]);
        ChangesStatusText = "变更日志：未连接（unavailable）。";
        ArtifactText = "工件：未连接（unavailable）。";
        // C2: the paging session belongs to the retired era — window, cursor
        // and identity all reset, so nothing leaks into the next connection.
        _artifactWindow.Clear();
        _artifactSessionReference = null;
        _artifactNextOffset = null;
        ArtifactNextOffset = null;
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
