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

public sealed class AsyncCommandGroup
{
    private readonly List<RelayCommand> _commands = [];

    public RelayCommand Add(Func<Task> execute, Func<bool>? canExecute = null, Action<Exception>? onError = null)
    {
        var command = new RelayCommand(execute, canExecute, onError);
        _commands.Add(command);
        return command;
    }

    public void RaiseCanExecute()
    {
        foreach (var command in _commands)
        {
            command.RaiseCanExecute();
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
