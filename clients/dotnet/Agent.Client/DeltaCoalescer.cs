using System.Text;

namespace FocusAgent.Client;

/// <summary>
/// G3/N7: coalesces streaming text deltas into small flush windows so long
/// sessions never re-render per token. The window flushes when the character
/// budget is exceeded or the flush interval elapses; N7 arms a one-shot
/// timer when content is pending, so a short delta followed by silence still
/// flushes on time — the interval is honored without new input. A terminal
/// flush drains the buffer. This coalescer is presentation-only and never
/// drops characters — durability is the server's journal, not this buffer.
/// </summary>
public sealed class DeltaCoalescer : IDisposable
{
    private readonly StringBuilder _pending = new();
    private readonly int _flushCharBudget;
    private readonly TimeSpan _flushInterval;
    private readonly Action<string> _flush;
    private DateTimeOffset _lastFlush = DateTimeOffset.UtcNow;
    private DateTimeOffset _deadline;
    private readonly object _gate = new();
    private System.Threading.Timer? _timer;

    public DeltaCoalescer(Action<string> flush, int flushCharBudget = 256, TimeSpan? flushInterval = null)
    {
        _flush = flush;
        _flushCharBudget = flushCharBudget;
        _flushInterval = flushInterval ?? TimeSpan.FromMilliseconds(50);
    }

    public void Append(string text)
    {
        if (text.Length == 0)
        {
            return;
        }
        lock (_gate)
        {
            _pending.Append(text);
            if (_pending.Length >= _flushCharBudget)
            {
                FlushCore();
                return;
            }
            ArmTimerCore();
        }
    }

    /// <summary>Drains whatever is pending; call on turn end / view close.</summary>
    public void Flush()
    {
        lock (_gate)
        {
            FlushCore();
        }
    }

    /// <summary>Stops the pending flush timer. Pending text is NOT flushed
    /// here (the owner already flushed or is tearing down); a later
    /// <see cref="Flush"/> still drains it.</summary>
    public void Dispose()
    {
        lock (_gate)
        {
            _timer?.Dispose();
            _timer = null;
        }
    }

    /// <summary>Arms (or re-arms) the one-shot timer so the interval is
    /// honored even when no further input arrives. Called under the gate.</summary>
    private void ArmTimerCore()
    {
        if (_pending.Length == 0)
        {
            return;
        }
        var deadline = _lastFlush + _flushInterval;
        var delay = deadline - DateTimeOffset.UtcNow;
        if (delay <= TimeSpan.Zero)
        {
            FlushCore();
            return;
        }
        _deadline = deadline;
        if (_timer is null)
        {
            _timer = new System.Threading.Timer(
                static state => ((DeltaCoalescer)state!).FlushIfDue(),
                this,
                delay,
                Timeout.InfiniteTimeSpan);
        }
        else
        {
            _timer.Change(delay, Timeout.InfiniteTimeSpan);
        }
    }

    private void FlushIfDue()
    {
        lock (_gate)
        {
            if (_pending.Length == 0)
            {
                return;
            }
            if (DateTimeOffset.UtcNow < _deadline)
            {
                // Early fire (timer granularity): re-arm for the remainder.
                var remaining = _deadline - DateTimeOffset.UtcNow;
                if (remaining > TimeSpan.Zero)
                {
                    _timer?.Change(remaining, Timeout.InfiniteTimeSpan);
                    return;
                }
            }
            FlushCore();
        }
    }

    private void FlushCore()
    {
        if (_pending.Length == 0)
        {
            return;
        }
        var text = _pending.ToString();
        _pending.Clear();
        _lastFlush = DateTimeOffset.UtcNow;
        _timer?.Dispose();
        _timer = null;
        _flush(text);
    }
}
