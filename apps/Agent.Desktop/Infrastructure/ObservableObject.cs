using System.Collections.ObjectModel;
using System.ComponentModel;
using System.Runtime.CompilerServices;
using System.Windows.Input;

namespace FocusAgent.Desktop.Infrastructure;

public abstract class ObservableObject : INotifyPropertyChanged
{
    public event PropertyChangedEventHandler? PropertyChanged;

    protected void Raise([CallerMemberName] string? name = null) =>
        PropertyChanged?.Invoke(this, new PropertyChangedEventArgs(name));

    protected bool Set<T>(ref T field, T value, [CallerMemberName] string? name = null)
    {
        if (EqualityComparer<T>.Default.Equals(field, value))
        {
            return false;
        }
        field = value;
        Raise(name);
        return true;
    }
}

public sealed class RelayCommand : ICommand
{
    private readonly Func<Task> _execute;
    private readonly Func<bool>? _canExecute;
    private readonly Action<Exception>? _onError;
    private int _running;

    public RelayCommand(Func<Task> execute, Func<bool>? canExecute = null, Action<Exception>? onError = null)
    {
        _execute = execute;
        _canExecute = canExecute;
        _onError = onError;
    }

    public event EventHandler? CanExecuteChanged;

    public bool CanExecute(object? parameter) => _running == 0 && (_canExecute?.Invoke() ?? true);

    public async void Execute(object? parameter)
    {
        if (Interlocked.CompareExchange(ref _running, 1, 0) != 0)
        {
            return;
        }
        try
        {
            await _execute();
        }
        catch (Exception failure)
        {
            _onError?.Invoke(failure);
        }
        finally
        {
            Interlocked.Exchange(ref _running, 0);
            CanExecuteChanged?.Invoke(this, EventArgs.Empty);
        }
    }

    public void RaiseCanExecute() => CanExecuteChanged?.Invoke(this, EventArgs.Empty);
}

/// <summary>
/// N4/F13: a group of long-lived commands that can also RELEASE its members.
/// Per-approval Allow/Deny commands register here once per approval row and
/// are removed when the row goes away, so repeated snapshot refreshes can
/// never accumulate stale registrations.
/// </summary>
public sealed class AsyncCommandGroup
{
    private readonly List<RelayCommand> _commands = [];

    public RelayCommand Add(Func<Task> execute, Func<bool>? canExecute = null, Action<Exception>? onError = null)
    {
        var command = new RelayCommand(execute, canExecute, onError);
        _commands.Add(command);
        return command;
    }

    /// <summary>Releases one command's registration. Idempotent.</summary>
    public void Remove(RelayCommand command) => _commands.Remove(command);

    /// <summary>How many commands are registered right now (lifecycle drill
    /// and test observation).</summary>
    public int Count
    {
        get
        {
            lock (_commands)
            {
                return _commands.Count;
            }
        }
    }

    public void RaiseCanExecute()
    {
        RelayCommand[] snapshot;
        lock (_commands)
        {
            snapshot = [.. _commands];
        }
        foreach (var command in snapshot)
        {
            command.RaiseCanExecute();
        }
    }
}

/// <summary>
/// N4/F13: the UI-thread seam of the workbench view model. Production wires
/// the Avalonia dispatcher; lifecycle drills wire an inline dispatcher so
/// refresh/generation behavior is deterministic without a running UI.
/// </summary>
public interface IUiDispatcher
{
    /// <summary>Runs <paramref name="action"/> on the UI thread.</summary>
    void Post(Action action);

    /// <summary>Starts a periodic fallback timer; disposing the handle stops
    /// it. The view model treats it only as a safety net — the primary
    /// refresh driver is the event stream.</summary>
    IDisposable StartPeriodicTimer(TimeSpan interval, Action tick);
}

/// <summary>The production dispatcher: Avalonia's UI thread.</summary>
public sealed class AvaloniaUiDispatcher : IUiDispatcher
{
    public static AvaloniaUiDispatcher Instance { get; } = new();

    public void Post(Action action) => Avalonia.Threading.Dispatcher.UIThread.Post(action);

    public IDisposable StartPeriodicTimer(TimeSpan interval, Action tick)
    {
        var timer = new Avalonia.Threading.DispatcherTimer { Interval = interval };
        timer.Tick += (_, _) => tick();
        timer.Start();
        return new TimerHandle(timer);
    }

    private sealed class TimerHandle(Avalonia.Threading.DispatcherTimer timer) : IDisposable
    {
        public void Dispose() => timer.Stop();
    }
}

/// <summary>Deterministic dispatcher for lifecycle drills: actions run
/// inline, the fallback timer is inert.</summary>
public sealed class InlineUiDispatcher : IUiDispatcher
{
    public void Post(Action action) => action();

    public IDisposable StartPeriodicTimer(TimeSpan interval, Action tick) => new NullHandle();

    private sealed class NullHandle : IDisposable
    {
        public void Dispose()
        {
        }
    }
}

/// <summary>Collection wrapper with bounded retention support (G3 will tighten this).</summary>
public static class CollectionExtensions
{
    public static void ReplaceWith<T>(this ObservableCollection<T> target, IEnumerable<T> items)
    {
        target.Clear();
        foreach (var item in items)
        {
            target.Add(item);
        }
    }
}
