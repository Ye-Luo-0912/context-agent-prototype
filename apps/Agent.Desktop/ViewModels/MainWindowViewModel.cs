using System.Collections.ObjectModel;
using System.Text;
using System.Windows.Input;
using Avalonia.Threading;
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

public sealed class ApprovalItemViewModel
{
    public string RequestId { get; init; } = string.Empty;
    public string CallName { get; init; } = string.Empty;
    public string Title => $"{CallName} · {RequestId}";
    public ICommand AllowCommand { get; init; } = new RelayCommand(() => Task.CompletedTask, () => false);
    public ICommand DenyCommand { get; init; } = new RelayCommand(() => Task.CompletedTask, () => false);
}

public enum TransportKind
{
    FixtureLayout,
    WindowsNamedPipe,
    UnixSocket,
}

public sealed class MainWindowViewModel : ObservableObject
{
    private static readonly string[] TransportLabels =
    {
        "布局夹具（非执行器）",
        "Windows Named Pipe",
        "Unix Socket",
    };

    /// <summary>G3: bounded retained data — the list never grows past this.</summary>
    private const int MaxRenderedTasks = 200;
    private const int MaxOutputLines = 400;

    private ResumableSession? _session;
    private FixtureAgentConnection? _fixture;
    private readonly DispatcherTimer? _refreshTimer;
    private string _connectionLabel = "未连接";
    private bool _isConnected;
    private string _goalInput = string.Empty;
    private string _endpoint = AgentTransports.DefaultPipeName;
    private int _transportIndex = (int)TransportKind.FixtureLayout;
    private ulong _watermark;
    private string _focusText = "—";
    private string _planText = "计划随平台快照的锚点字段接入（P2）后显示。";
    private string _bannerText = string.Empty;
    private string _outputText = string.Empty;
    private bool _autoRefresh = true;

    public MainWindowViewModel()
    {
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
            () => RefreshSnapshotAsync(),
            () => IsConnected,
            error => AppendOutput($"刷新失败：{error.Message}"));

        if (Dispatcher.UIThread is { } ui)
        {
            _refreshTimer = new DispatcherTimer { Interval = TimeSpan.FromSeconds(3) };
            _refreshTimer.Tick += async (_, _) =>
            {
                if (AutoRefresh && IsConnected)
                {
                    try
                    {
                        await RefreshSnapshotCoreAsync();
                    }
                    catch
                    {
                        // Bounded polling only; the next tick or manual refresh
                        // retries. Action-triggered refreshes surface errors.
                    }
                }
            };
            _refreshTimer.Start();
        }
    }

    private AsyncCommandGroup AsyncCommands { get; }

    public ICommand ConnectCommand { get; }
    public ICommand DisconnectCommand { get; }
    public ICommand SubmitCommand { get; }
    public ICommand ContinueCommand { get; }
    public ICommand CancelCommand { get; }
    public ICommand RefreshCommand { get; }

    public ObservableCollection<TaskItemViewModel> Tasks { get; } = [];
    public ObservableCollection<ApprovalItemViewModel> Approvals { get; } = [];

    public IReadOnlyList<string> Transports => TransportLabels;

    public int TransportIndex
    {
        get => _transportIndex;
        set
        {
            if (Set(ref _transportIndex, value))
            {
                Endpoint = value switch
                {
                    (int)TransportKind.WindowsNamedPipe => AgentTransports.DefaultPipeName,
                    (int)TransportKind.UnixSocket => AgentTransports.DefaultSocketPath,
                    _ => AgentTransports.DefaultPipeName,
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

    public string PlanText { get => _planText; private set => Set(ref _planText, value); }

    /// <summary>G2 banner: resync / connection-loss state, never silently hidden.</summary>
    public string BannerText { get => _bannerText; private set => Set(ref _bannerText, value); }

    public string OutputText { get => _outputText; private set => Set(ref _outputText, value); }

    private void AppendOutput(string line)
    {
        var builder = new StringBuilder(OutputText);
        builder.AppendLine(line);
        var lines = builder.ToString().Split('\n');
        if (lines.Length > MaxOutputLines)
        {
            builder = new StringBuilder(string.Join('\n', lines[^MaxOutputLines..]));
        }
        OutputText = builder.ToString();
    }

    private IAgentTransport BuildTransport() => (TransportKind)TransportIndex switch
    {
        TransportKind.WindowsNamedPipe => new NamedPipeTransport(Endpoint),
        _ => new UnixDomainSocketTransport(Endpoint),
    };

    private async Task ConnectAsync()
    {
        await DisconnectAsync();
        if ((TransportKind)TransportIndex == TransportKind.FixtureLayout)
        {
            _fixture = new FixtureAgentConnection();
            _session = null;
            IsConnected = true;
            ConnectionLabel = "已连接（布局夹具）";
            BannerText = "布局夹具：仅驱动界面布局，不是执行器，不代表任何真实任务状态。";
            ApplySnapshot(await _fixture.SnapshotAsync());
            return;
        }
        var session = new ResumableSession(
            () => BuildTransport().ConnectAsync(CancellationToken.None));
        session.Resynced += snapshot => Dispatcher.UIThread.Post(() =>
        {
            BannerText = "已从快照重建（重连）。挂起的审批以服务器快照为准，不会自动通过。";
            ApplySnapshot(snapshot);
        });
        session.ConnectionLost += failure => Dispatcher.UIThread.Post(() =>
        {
            BannerText = "连接丢失。挂起的审批不会自动通过；正在重连并从快照重建。";
            AppendOutput($"连接丢失：{failure.Message}");
        });
        _session = session;
        IsConnected = true;
        AppendOutput($"已连接：{TransportLabels[TransportIndex]} / {Endpoint}");
        ApplySnapshot(await session.SnapshotAsync());
    }

    private async Task DisconnectAsync()
    {
        _fixture = null;
        if (_session is not null)
        {
            await _session.DisposeAsync();
            _session = null;
        }
        IsConnected = false;
        BannerText = string.Empty;
        Tasks.ReplaceWith([]);
        Approvals.ReplaceWith([]);
    }

    private async Task SubmitAsync()
    {
        if (_session is null)
        {
            if (_fixture is not null)
            {
                var fixtureReceipt = await _fixture.SubmitWorkAsync(GoalInput.Trim(), ClientRequestIds.Next());
                AppendOutput($"夹具受理：task {fixtureReceipt.TaskId}。仅布局，不执行。");
                GoalInput = string.Empty;
                await RefreshSnapshotCoreAsync();
            }
            return;
        }
        var receipt = await _session.SubmitWorkAsync(GoalInput.Trim(), ClientRequestIds.Next());
        AppendOutput($"已受理：task {receipt.TaskId}（{receipt.Disposition}）。受理 ≠ 完成，完成以快照与事件为准。");
        GoalInput = string.Empty;
        await RefreshSnapshotCoreAsync();
    }

    private async Task ContinueAsync()
    {
        if (_session is null)
        {
            if (_fixture is not null)
            {
                AppendOutput("夹具模式不模拟继续；连接真实宿主后可用。");
            }
            return;
        }
        var receipt = await _session.ContinueAsync();
        AppendOutput($"已继续活动任务 {receipt.TaskId}。");
        await RefreshSnapshotCoreAsync();
    }

    private async Task CancelAsync()
    {
        if (_session is null)
        {
            return;
        }
        var receipt = await _session.CancelCurrentTurnAsync();
        AppendOutput(receipt.Ack.Status switch
        {
            TurnCancelAckStatus.Cancelled => $"取消已过屏障：generation {receipt.Ack.CancelledGeneration} → {receipt.Ack.EffectiveGeneration}。",
            _ => "当前没有活动轮次（这是事实，不是失败）。",
        });
        await RefreshSnapshotCoreAsync();
    }

    private Task RefreshSnapshotAsync()
    {
        if (_session is null)
        {
            return Task.CompletedTask;
        }
        return RefreshSnapshotCoreAsync();
    }

    private async Task RefreshSnapshotCoreAsync()
    {
        switch (_session, _fixture)
        {
            case (not null, _):
                ApplySnapshot(await _session.SnapshotAsync());
                break;
            case (_, not null):
                ApplySnapshot(await _fixture.SnapshotAsync());
                break;
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
        Approvals.ReplaceWith(snapshot.PendingApprovals.Select(entry => new ApprovalItemViewModel
        {
            RequestId = entry.RequestId,
            CallName = entry.CallName,
            AllowCommand = AsyncCommands.Add(
                () => RespondApprovalAsync(entry.RequestId, ApprovalDecision.Allow),
                onError: error => AppendOutput($"审批失败：{error.Message}")),
            DenyCommand = AsyncCommands.Add(
                () => RespondApprovalAsync(entry.RequestId, ApprovalDecision.Deny),
                onError: error => AppendOutput($"审批失败：{error.Message}")),
        }));
        Watermark = snapshot.Watermark;
        FocusText = snapshot.Focus is null
            ? "无焦点任务"
            : $"{snapshot.Focus.Goal}（anchor r{snapshot.Focus.AnchorRevision}）";
        if (snapshot.ResyncRequired)
        {
            BannerText = "事件流出现缺口（resync_required）：显示状态已由本快照整体重建。";
        }
        AsyncCommands.RaiseCanExecute();
    }

    private async Task RespondApprovalAsync(string requestId, ApprovalDecision decision)
    {
        if (_session is null)
        {
            if (_fixture is not null)
            {
                await _fixture.RespondApprovalAsync(requestId, decision);
                AppendOutput($"夹具审批 {requestId}：{decision}。仅布局。");
                await RefreshSnapshotCoreAsync();
            }
            return;
        }
        var outcome = await _session.RespondApprovalAsync(requestId, decision);
        AppendOutput(outcome.Outcome == ApprovalRespondOutcome.Delivered
            ? $"审批 {requestId} 已送达：{decision}。"
            : $"审批 {requestId} 已不在待决（迟到或重复），当前事实被返回。");
        await RefreshSnapshotCoreAsync();
    }
}
