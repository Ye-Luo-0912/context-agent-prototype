using System.Collections.Generic;
using System.Threading;
using System.Threading.Channels;
using System.Threading.Tasks;

namespace FocusAgent.Client;

/// <summary>
/// Bounded, arrival-ordered queue for <see cref="WorkEventNotification"/>s
/// with a class-aware overflow policy (N3). The queue is the client's event
/// backpressure contract:
///
/// - Under capacity, every notification is appended in host arrival order.
/// - When full and the arrival is live-only progress
///   (<c>model_delta</c>/<c>model_retrying</c> — the envelope <c>seq</c>
///   repeats the preceding durable event, so progress is newest-wins by
///   contract), the OLDEST queued live-only progress item is dropped to
///   admit it; if the queue holds no progress item at all, the arrival
///   itself is dropped. Either way the drop is counted in
///   <see cref="DroppedCount"/> and never blocks the read loop.
/// - When full and the arrival is a durable lifecycle fact (every event type
///   other than the two live-only ones — including all approval-relevant and
///   terminal facts such as turn/task/run completion and checkpoint
///   barriers), the oldest live-only progress item is evicted to make room.
///   Approval/terminal notifications are NEVER dropped to shed load.
/// - When full with no evictable progress item and the arrival is itself
///   durable, the enqueue is REFUSED (<see cref="TryEnqueue"/> returns
///   false): the consumer has stopped draining entirely, silently losing an
///   approval or terminal fact is not an option, and the caller (the
///   connection read loop) takes the N2 terminal fault path. Recovery is the
///   standing rule: a fresh connection rebuilt from a snapshot.
///
/// The read surface is a real <see cref="ChannelReader{T}"/> (multiple
/// concurrent readers, <see cref="ChannelReader{T}.WaitToReadAsync"/> wakes
/// on the next arrival). A completed queue reads out its backlog first, then
/// ends the stream — or throws the completion error once drained, so a
/// terminal overflow (or a connection fault relayed through the queue) is
/// observed by the consumer, never swallowed.
/// </summary>
public sealed class BoundedEventQueue
{
    private readonly object _gate = new();
    private readonly List<WorkEventNotification> _items;
    private readonly int _capacity;
    private readonly QueueReader _reader;

    /// <summary>Idle→signalled latch: replaced under the lock whenever the
    /// queue state changes, so every parked waiter wakes exactly once.</summary>
    private TaskCompletionSource<bool> _signalled = NewLatch();

    private bool _completed;
    private Exception? _error;
    private long _dropped;
    private TaskCompletionSource _completion = NewCompletion(null);

    public BoundedEventQueue(int capacity)
    {
        if (capacity < 1)
        {
            throw new ArgumentOutOfRangeException(nameof(capacity));
        }
        _capacity = capacity;
        _items = new List<WorkEventNotification>(capacity);
        _reader = new QueueReader(this);
    }

    /// <summary>The consumer side of the queue.</summary>
    public ChannelReader<WorkEventNotification> Reader => _reader;

    /// <summary>How many live-only progress notifications have been shed by
    /// the overflow policy. Durable notifications are never counted here:
    /// they are either delivered or they fault the connection.</summary>
    public long DroppedCount
    {
        get { lock (_gate) { return _dropped; } }
    }

    private static TaskCompletionSource<bool> NewLatch() =>
        new(TaskCreationOptions.RunContinuationsAsynchronously);

    private static TaskCompletionSource NewCompletion(Exception? error)
    {
        var completion = new TaskCompletionSource(TaskCreationOptions.RunContinuationsAsynchronously);
        if (error is not null)
        {
            completion.SetException(error);
        }
        else
        {
            completion.SetResult();
        }
        return completion;
    }

    /// <summary>The overflow policy. Returns false only for the terminal
    /// refuse case (durable arrival, no sheddable progress, queue full).</summary>
    public bool TryEnqueue(WorkEventNotification notification)
    {
        lock (_gate)
        {
            if (_completed)
            {
                return false;
            }
            if (_items.Count < _capacity)
            {
                _items.Add(notification);
                SignalLocked();
                return true;
            }

            var sheddable = _items.FindIndex(static item => item.IsLiveOnlyProgress);
            if (sheddable >= 0)
            {
                // Full: the oldest live-only progress item gives way — to a
                // newer progress arrival (newest-wins) or to a durable fact
                // that must not be dropped.
                _items.RemoveAt(sheddable);
                _dropped++;
                _items.Add(notification);
                SignalLocked();
                return true;
            }
            if (notification.IsLiveOnlyProgress)
            {
                // Full of durable facts and the arrival is sheddable
                // progress: drop the arrival (the newest progress item is
                // the least valuable fact on the wire).
                _dropped++;
                return true;
            }
            // Full of durable facts and the arrival is durable: refusing is
            // the only honest move; the caller takes the terminal path.
            return false;
        }
    }

    /// <summary>Ends the stream: readers drain the backlog, then see the end
    /// (or the completion error, if one was given). Later enqueues are
    /// refused.</summary>
    public bool TryComplete(Exception? error = null)
    {
        lock (_gate)
        {
            if (_completed)
            {
                return false;
            }
            _completed = true;
            _error = error;
            _completion = NewCompletion(error);
            var latch = _signalled;
            _signalled = NewLatch();
            latch.TrySetResult(false);
            return true;
        }
    }

    private void SignalLocked()
    {
        var latch = _signalled;
        _signalled = NewLatch();
        latch.TrySetResult(true);
    }

    private sealed class QueueReader(BoundedEventQueue queue) : ChannelReader<WorkEventNotification>
    {
        public override bool CanCount => true;

        public override int Count
        {
            get { lock (queue._gate) { return queue._items.Count; } }
        }

        /// <summary>Completion faults when the queue was completed with an
        /// error (terminal overflow, connection fault), else completes.</summary>
        public override Task Completion => queue._completion.Task;

        public override bool TryRead(out WorkEventNotification item)
        {
            lock (queue._gate)
            {
                if (queue._items.Count > 0)
                {
                    item = queue._items[0];
                    queue._items.RemoveAt(0);
                    return true;
                }
            }
            item = null!;
            return false;
        }

        public override async ValueTask<bool> WaitToReadAsync(CancellationToken cancellationToken = default)
        {
            while (true)
            {
                Task<bool> latch;
                lock (queue._gate)
                {
                    if (queue._items.Count > 0)
                    {
                        return true;
                    }
                    if (queue._completed)
                    {
                        // Drained: end of stream — or the completion reason,
                        // so a faulted/overflowed stream is never mistaken
                        // for a quiet one.
                        if (queue._error is not null)
                        {
                            throw queue._error;
                        }
                        return false;
                    }
                    latch = queue._signalled.Task;
                }
                try
                {
                    await latch.WaitAsync(cancellationToken).ConfigureAwait(false);
                }
                catch (OperationCanceledException) when (cancellationToken.IsCancellationRequested)
                {
                    // The latch may have fired in the same instant the token
                    // cancelled: re-check once under the lock before
                    // honouring the cancellation.
                    lock (queue._gate)
                    {
                        if (queue._items.Count > 0)
                        {
                            return true;
                        }
                        if (queue._completed)
                        {
                            if (queue._error is null)
                            {
                                return false;
                            }
                            throw queue._error;
                        }
                    }
                    throw;
                }
            }
        }
    }
}
