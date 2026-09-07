using System.Text;

namespace FocusAgent.Client;

/// <summary>
/// G3: coalesces streaming text deltas into small flush windows so long
/// sessions never re-render per token. The window flushes when either the
/// character budget or the flush interval is exceeded; a terminal flush
/// drains the buffer. This coalescer is presentation-only and never drops
/// characters — durability is the server's journal, not this buffer.
/// </summary>
public sealed class DeltaCoalescer
{
    private readonly StringBuilder _pending = new();
    private readonly int _flushCharBudget;
    private readonly TimeSpan _flushInterval;
    private readonly Action<string> _flush;
    private DateTimeOffset _lastFlush = DateTimeOffset.UtcNow;
    private readonly object _gate = new();

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
            if (_pending.Length >= _flushCharBudget
                || DateTimeOffset.UtcNow - _lastFlush >= _flushInterval)
            {
                FlushCore();
            }
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

    private void FlushCore()
    {
        if (_pending.Length == 0)
        {
            return;
        }
        var text = _pending.ToString();
        _pending.Clear();
        _lastFlush = DateTimeOffset.UtcNow;
        _flush(text);
    }
}
