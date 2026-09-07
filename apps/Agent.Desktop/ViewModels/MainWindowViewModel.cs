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
        TaskSnapshotStatus.Completed => "已完成",
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

    private IAgentConnection? _connection;
    private readonly DispatcherTimer? _refreshTimer;
    private string _connectionLabel = "未连接";
    private bool _isConnected;
    private string _goalInput = string.Empty;
    private string _endpoint = AgentTransports.DefaultPipeName;
    private int _transportIndex = (int)TransportKind.FixtureLayout;
    private ulong _watermark;
    private string _focusText = "—";
    private string _planText = "计划随平台快照的锚点字段接入（P2）后显示。";
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
                if (AutoRefresh && IsConnected && _connection is not null)
                {
                    try
                    {
                        await RefreshSnapshotCoreAsync(_connection);
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
                ConnectionLabel = value ? _connection?.IsConnected == true ? "已连接" : "已连接" : "未连接";
                AsyncCommands.RaiseCanExecute();
            }
        }
    }

    public string ConnectionLabel { get => _connectionLabel; private set => Set(ref _connectionLabel, value); }

    public ulong Watermark { get => _watermark; private set => Set(ref _watermark, value); }

    public string FocusText { get => _focusText; private set => Set(ref _focusText, value); }

    public string PlanText { get => _planText; private set => Set(ref _planText, value); }

    public string OutputText { get => _outputText; private set => Set(ref _outputText, value); }

    private void AppendOutput(string line)
    {
        // Bounded console output: keep the tail, drop the head past 400 lines.
        var builder = new StringBuilder(OutputText);
        builder.AppendLine(line);
        var lines = builder.ToString().Split('\n');
        if (lines.Length > 400)
        {
            builder = new StringBuilder(string.Join('\n', lines[^400..]));
        }
        OutputText = builder.ToString();
    }

    private async Task ConnectAsync()
    {
        await DisconnectAsync();
        IAgentConnection connection;
        if ((TransportKind)TransportIndex == TransportKind.FixtureLayout)
        {
            connection = new FixtureAgentConnection();
        }
        else
        {
            IAgentTransport transport = (TransportKind)TransportIndex switch
            {
                TransportKind.WindowsNamedPipe => (IAgentTransport)new NamedPipeTransport(Endpoint),
                _ => new UnixDomainSocketTransport(Endpoint),
            };
            var stream = await transport.ConnectAsync(CancellationToken.None);
            connection = new AgentConnection(stream);
        }
        _connection = connection;
        IsConnected = true;
        AppendOutput($"已连接：{TransportLabels[TransportIndex]} / {Endpoint}");
        await RefreshSnapshotCoreAsync(connection);
    }

    private async Task DisconnectAsync()
    {
        if (_connection is not null)
        {
            await _connection.DisposeAsync();
            _connection = null;
            IsConnected = false;
            Tasks.ReplaceWith([]);
            Approvals.ReplaceWith([]);
            AppendOutput("已断开。");
        }
    }

    private async Task SubmitAsync()
    {
        if (_connection is null)
        {
            return;
        }
        var receipt = await _connection.SubmitWorkAsync(GoalInput.Trim(), ClientRequestIds.Next());
        AppendOutput($"已受理：task {receipt.TaskId}（{receipt.Disposition}）。受理 ≠ 完成，完成以快照与事件为准。");
        GoalInput = string.Empty;
        await RefreshSnapshotCoreAsync(_connection);
    }

    private async Task ContinueAsync()
    {
        if (_connection is null)
        {
            return;
        }
        var receipt = await _connection.ContinueAsync();
        AppendOutput($"已继续活动任务 {receipt.TaskId}。");
        await RefreshSnapshotCoreAsync(_connection);
    }

    private async Task CancelAsync()
    {
        if (_connection is null)
        {
            return;
        }
        var receipt = await _connection.CancelCurrentTurnAsync();
        AppendOutput(receipt.Ack.Status switch
        {
            TurnCancelAckStatus.Cancelled => $"取消已过屏障：generation {receipt.Ack.CancelledGeneration} → {receipt.Ack.EffectiveGeneration}。",
            _ => "当前没有活动轮次（这是事实，不是失败）。",
        });
        await RefreshSnapshotCoreAsync(_connection);
    }

    private async Task RespondApprovalAsync(string requestId, ApprovalDecision decision)
    {
        if (_connection is null)
        {
            return;
        }
        var outcome = await _connection.RespondApprovalAsync(requestId, decision);
        AppendOutput(outcome.Outcome == ApprovalRespondOutcome.Delivered
            ? $"审批 {requestId} 已送达：{decision}。"
            : $"审批 {requestId} 已不在待决（迟到或重复），当前事实被返回。");
        await RefreshSnapshotCoreAsync(_connection);
    }

    private Task RefreshSnapshotAsync()
    {
        if (_connection is null)
        {
            return Task.CompletedTask;
        }
        return RefreshSnapshotCoreAsync(_connection);
    }

    private async Task RefreshSnapshotCoreAsync(IAgentConnection connection)
    {
        var snapshot = await connection.SnapshotAsync();
        var focusId = snapshot.Focus?.TaskId;
        Tasks.ReplaceWith(snapshot.Tasks.Select(entry => TaskItemViewModel.From(entry, entry.TaskId == focusId)));
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
        PlanText = "计划随平台快照的锚点字段接入（P2）后显示。";
        AsyncCommands.RaiseCanExecute();
    }
}
