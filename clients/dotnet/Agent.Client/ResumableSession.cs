using System.Threading.Channels;

namespace FocusAgent.Client;

/// <summary>
/// F07: a mutating operation (submit/continue/cancel) whose connection
/// failed has an UNKNOWN server-side outcome — the request frame may or may
/// not have been delivered. This typed exception is the "result unknown"
/// surface: the session never re-sends the mutation, and the caller must
/// re-snapshot (or otherwise re-query) and then explicitly decide what to do
/// — a resubmission stays idempotent through the same
/// <c>client_request_id</c>.
/// </summary>
public sealed class AgentUnknownOutcomeException : Exception
{
    /// <summary>The underlying connection failure that made the outcome
    /// unknowable.</summary>
    public Exception Failure { get; }

    public AgentUnknownOutcomeException(Exception failure)
        : base($"operation outcome unknown: the connection failed ({failure.Message}); "
            + "the request may or may not have been delivered — re-snapshot before deciding to retry")
    {
        Failure = failure;
    }
}

/// <summary>
/// G2 operational-chain support on the client side. A resumable session owns
/// the reconnect policy: every reconnect rebuilds from a fresh snapshot and
/// reports <see cref="Resynced"/>; pending approvals are NEVER re-answered or
/// auto-approved across a reconnect — they are re-listed from the server's
/// snapshot, because the client cannot know what happened while offline.
///
/// F07 splits queries from mutations. Queries (snapshot/subscribe) carry no
/// effect, so a faulted connection still triggers one automatic
/// reconnect-and-retry. Mutations (submit/continue/cancel) are sent exactly
/// once: a connection failure surfaces as
/// <see cref="AgentUnknownOutcomeException"/>, never as an automatic
/// re-send.
///
/// N3: the session also owns ONE stable typed event stream
/// (<see cref="Events"/>) across reconnects. Every installed connection is
/// pumped into the session-level bounded queue (same class-aware overflow
/// policy); once a connection stops being the live one, its notifications
/// stop being relayed — a reconnect never interleaves an old connection's
/// leftovers into the new stream. B1: every (re)connect subscribes BEFORE
/// snapshotting (the host registers the receiver before its snapshot
/// barrier), so no durable change can fall between the stream and the
/// state read; the snapshot's watermark dedups the stream (durable events
/// at or below it are relayed no more than once), and installing a new
/// connection resets the stream — the old connection's unread backlog is
/// dropped with the snapshot rebuilding all durable state.
///
/// N4: the session is an <see cref="IAgentConnection"/>, so a UI shell can
/// hold one connection abstraction whether it talks through this resumable
/// session or a single-shot connection.
/// </summary>
public sealed class ResumableSession : IAgentConnection, IAsyncDisposable
{
    private readonly AgentConnectionOptions _options;
    private readonly Func<Task<Stream>> _connect;
    private readonly object _gate = new();
    // R13: the session event stream is re-allocatable — a reconnect after a
    // session-level overflow REBUILDS a fresh live queue rather than trying to
    // un-complete a monotonically closed generation.
    private BoundedEventQueue _events;
    private AgentConnection? _connection;

    /// <summary>R13: set once the session queue refused a durable
    /// notification (terminal overflow). The current event stream is a closed,
    /// monotonically-done generation; the next successful reconnect rebuilds
    /// it so new events become reachable again.</summary>
    private bool _eventsOverflowed;

    // F09: at most one connect attempt in flight per session (single-flight);
    // a generation counter plus the disposed flag veto stale installs, so a
    // late connect can never open a parallel connection or revive a disposed
    // session.
    private Task<AgentConnection>? _connecting;
    private long _generation;
    private bool _disposed;

    public ResumableSession(Func<Task<Stream>> connect, AgentConnectionOptions? options = null)
    {
        _connect = connect;
        _options = options ?? new AgentConnectionOptions();
        _events = new BoundedEventQueue(_options.NotificationCapacity);
    }

    /// <summary>Raised after every (re)connect with the fresh snapshot. The
    /// snapshot's <see cref="WorkSnapshotResponse.Watermark"/> is the
    /// consumer's dedup cursor (B1): durable stream events at or below it
    /// are already reflected in this snapshot; live-only progress
    /// supersedes by turn/operation identity.</summary>
    public event Action<WorkSnapshotResponse>? Resynced;

    /// <summary>Raised when the live connection faults; the session stays
    /// usable — the next operation reconnects — but nothing is fabricated.</summary>
    public event Action<Exception>? ConnectionLost;

    public bool IsConnected
    {
        get
        {
            lock (_gate)
            {
                return _connection?.IsConnected == true;
            }
        }
    }

    /// <summary>
    /// F09: the single-flight gate. At most one connect attempt exists per
    /// session at any moment — concurrent callers await the SAME attempt, so
    /// no parallel connection is ever opened. A finished attempt is never
    /// reused: either it installed (and that connection no longer qualifies
    /// as live) or it failed.
    /// </summary>
    private async Task<AgentConnection> LiveAsync(CancellationToken cancellationToken)
    {
        AgentConnection? stale = null;
        Task<AgentConnection> connecting;
        lock (_gate)
        {
            if (_disposed)
            {
                throw new ObjectDisposedException(nameof(ResumableSession));
            }
            if (_connection?.IsConnected == true)
            {
                return _connection;
            }
            if (_connecting is { IsCompleted: true })
            {
                _connecting = null;
            }
            if (_connecting is null)
            {
                stale = _connection;
                if (stale is not null)
                {
                    // Discarding the dead connection advances the generation:
                    // anything still connecting against the old generation
                    // must not install.
                    _connection = null;
                    _generation++;
                }
                var generation = _generation;
                _connecting = ConnectAsyncCore(generation);
            }
            connecting = _connecting;
        }
        if (stale is not null)
        {
            await stale.DisposeAsync().ConfigureAwait(false);
        }
        // Every concurrent caller awaits the same shared attempt; nobody else
        // reaches the transport factory while this one is in flight.
        return await connecting.WaitAsync(cancellationToken).ConfigureAwait(false);
    }

    /// <summary>
    /// One shared connect attempt, bound to the session generation it was
    /// started for. The handshake runs before the install check: only a
    /// still-current, live session adopts the new connection — otherwise it
    /// is discarded and released, never installed.
    /// </summary>
    private async Task<AgentConnection> ConnectAsyncCore(long generation)
    {
        var stream = await _connect().ConfigureAwait(false);
        var fresh = new AgentConnection(stream, _options);
        WorkSnapshotResponse snapshot;
        try
        {
            // B1 SNAP-GAP: subscribe FIRST, then snapshot. The host's
            // subscribe registers this connection's event receiver BEFORE its
            // own snapshot barrier, so from this point every subsequent
            // runtime event is captured by this connection — nothing can
            // fall between the stream and the state read that follows it.
            // (Snapshot-first left every durable change between the two
            // calls in neither the snapshot nor the stream.)
            // The shared attempt must not inherit one waiter's cancellation
            // token, and each request stays bounded by its own timeout.
            try
            {
                await fresh.SubscribeAsync(cancellationToken: CancellationToken.None).ConfigureAwait(false);
            }
            catch (AgentProtocolException)
            {
                // Subscribe refusal never blocks the snapshot path.
            }
            snapshot = await fresh.SnapshotAsync(CancellationToken.None).ConfigureAwait(false);
        }
        catch
        {
            await fresh.DisposeAsync().ConfigureAwait(false);
            throw;
        }

        bool install;
        lock (_gate)
        {
            install = !_disposed && generation == _generation;
            if (install)
            {
                _connection = fresh;
            }
        }
        if (!install)
        {
            // F09: the session moved on while this attempt was in flight
            // (disposed, or a newer generation took over). The new connection
            // is dropped and released — a late connect never changes session
            // state and never revives a disposed session.
            await fresh.DisposeAsync().ConfigureAwait(false);
            throw new ObjectDisposedException(nameof(ResumableSession));
        }
        // R13: ordered reset/rebuild boundary. A NORMAL reconnect clears the
        // unread backlog on the SAME session stream (the ONE stable reader
        // across reconnects — the fresh snapshot rebuilds all durable state,
        // so a replaced connection's unread facts never resurface after
        // <see cref="Resynced"/>). After a session-level overflow the current
        // stream is a monotonically-closed generation, so this reconnect
        // REBUILDS a fresh live queue instead — new events become reachable
        // again for a consumer that re-reads <see cref="Events"/>, while the
        // faulted stream stays closed for anyone still holding it. Recovery is
        // explicit: the lost backlog is never replayed, only re-snapshotted.
        if (_eventsOverflowed)
        {
            lock (_gate)
            {
                _events = new BoundedEventQueue(_options.NotificationCapacity);
                _eventsOverflowed = false;
            }
        }
        else
        {
            _events.Clear();
        }
        // N3: one pump per installed connection relays its typed events into
        // the session-level stream; the pump ends when the connection stops
        // being the live one. The snapshot's watermark is the dedup cursor:
        // durable events at or below it are already reflected in the
        // snapshot raised below (and in this reconnect's reset), so the pump
        // relays them no more than once.
        _ = PumpEventsAsync(fresh, snapshot.Watermark);
        Resynced?.Invoke(snapshot);
        return fresh;
    }

    /// <summary>
    /// Relays one connection's typed notifications into the session-level
    /// event stream while that connection is the installed one. The
    /// staleness gate is checked per notification: once a reconnect installs
    /// a different connection (or the session is disposed), this pump stops
    /// mid-queue — an old connection's buffered notifications never leak
    /// into the new stream. Completion of the connection's queue (fault or
    /// dispose) simply ends the pump; the session-level stream stays open
    /// for the next connection.
    ///
    /// B1: <paramref name="durableCursor"/> is the handshake snapshot's
    /// watermark. Durable notifications at or below it are already reflected
    /// in that snapshot, so the pump drops them (no double-count); live-only
    /// progress (<c>model_delta</c>/<c>model_retrying</c>) repeats the
    /// preceding durable cursor and never enters any snapshot, so it always
    /// relays — its supersession fence is turn/operation identity, the
    /// consumer's concern.
    /// </summary>
    private async Task PumpEventsAsync(AgentConnection connection, ulong durableCursor)
    {
        try
        {
            var source = connection.Events;
            while (await source.WaitToReadAsync(CancellationToken.None).ConfigureAwait(false))
            {
                while (source.TryRead(out var notification))
                {
                    bool current;
                    lock (_gate)
                    {
                        current = !_disposed && _connection == connection;
                    }
                    if (!current)
                    {
                        return;
                    }
                    if (!notification.IsLiveOnlyProgress
                        && notification.Envelope.Seq <= durableCursor)
                    {
                        continue; // already reflected in the handshake snapshot
                    }
                    if (!_events.TryEnqueue(notification))
                    {
                        lock (_gate)
                        {
                            if (_disposed)
                            {
                                return;
                            }
                            // Terminal overflow at session level (the same
                            // class-aware policy as the connection queue):
                            // the current event stream ends with the honest
                            // reason. R13: the session itself is NOT terminal —
                            // the overflow is recorded so the next successful
                            // reconnect rebuilds a fresh stream.
                            _events.TryComplete(new AgentContractViolationException(
                                "work.event.queue",
                                "overflowed with undroppable approval/terminal notifications; rebuild from a snapshot"));
                            _eventsOverflowed = true;
                            return;
                        }
                    }
                }
            }
        }
        catch
        {
            // The connection's queue completed with its fault reason; the
            // fault already took the connection's terminal path and the
            // session's own state is untouched.
        }
    }

    /// <summary>Runs one query; a faulted connection reconnects once and
    /// rebuilds from a snapshot before the caller sees anything. Queries are
    /// effect-free, so the single automatic re-issue is safe.</summary>
    private async Task<T> RunQueryAsync<T>(
        Func<AgentConnection, CancellationToken, Task<T>> operation, CancellationToken cancellationToken)
    {
        try
        {
            var connection = await LiveAsync(cancellationToken).ConfigureAwait(false);
            return await operation(connection, cancellationToken).ConfigureAwait(false);
        }
        catch (Exception failure) when (failure is not OperationCanceledException
            && failure is not AgentProtocolException)
        {
            ConnectionLost?.Invoke(failure);
            var connection = await LiveAsync(cancellationToken).ConfigureAwait(false);
            return await operation(connection, cancellationToken).ConfigureAwait(false);
        }
    }

    /// <summary>
    /// Runs one mutating operation (submit/continue/cancel). The mutation is
    /// sent exactly once: a connection failure never triggers a re-send —
    /// the server-side outcome would be unknowable — and the caller gets
    /// <see cref="AgentUnknownOutcomeException"/> instead. Reconnecting is
    /// left to the next operation. A structured error answer
    /// (<see cref="AgentProtocolException"/>) is a definitive server fact and
    /// propagates unchanged; a local wait cancellation says nothing about the
    /// server side (a cancelled wait is not a cancelled turn) and propagates
    /// unchanged too.
    /// </summary>
    private async Task<T> RunMutationAsync<T>(
        Func<AgentConnection, CancellationToken, Task<T>> operation, CancellationToken cancellationToken)
    {
        // Failures to obtain any connection at all happen strictly before the
        // request is sent, so the raw failure propagates: nothing went out,
        // and honesty beats a fabricated "unknown".
        var connection = await LiveAsync(cancellationToken).ConfigureAwait(false);
        try
        {
            return await operation(connection, cancellationToken).ConfigureAwait(false);
        }
        catch (Exception failure) when (failure is not OperationCanceledException
            && failure is not AgentProtocolException)
        {
            ConnectionLost?.Invoke(failure);
            throw new AgentUnknownOutcomeException(failure);
        }
    }

    /// <summary>Fetches one consistent typed snapshot. A faulted connection
    /// reconnects once and rebuilds from a snapshot first (queries are
    /// effect-free, so one automatic re-issue is safe).</summary>
    public Task<WorkSnapshotResponse> SnapshotAsync(CancellationToken cancellationToken = default) =>
        RunQueryAsync((connection, token) => connection.SnapshotAsync(token), cancellationToken);

    /// <summary>Submits a new long-task goal. Sent exactly once: on a
    /// connection failure the outcome is unknown —
    /// <see cref="AgentUnknownOutcomeException"/>, never an automatic
    /// re-send. A retry is the caller's explicit decision and stays
    /// idempotent through the same <paramref name="clientRequestId"/>.</summary>
    public Task<WorkSubmitResponse> SubmitWorkAsync(string goal, string clientRequestId, CancellationToken cancellationToken = default) =>
        RunMutationAsync((connection, token) => connection.SubmitWorkAsync(goal, clientRequestId, token), cancellationToken);

    /// <summary>Continues the run's active task. Sent exactly once: on a
    /// connection failure the outcome is unknown —
    /// <see cref="AgentUnknownOutcomeException"/>, never an automatic
    /// re-send.</summary>
    public Task<WorkContinueResponse> ContinueAsync(CancellationToken cancellationToken = default) =>
        RunMutationAsync((connection, token) => connection.ContinueAsync(token), cancellationToken);

    /// <summary>Cancels the run's current turn. Sent exactly once: on a
    /// connection failure the outcome is unknown — the turn may or may not
    /// have been cancelled server-side — so
    /// <see cref="AgentUnknownOutcomeException"/> is thrown instead of
    /// re-arming a second cancel.</summary>
    public Task<WorkCancelResponse> CancelCurrentTurnAsync(CancellationToken cancellationToken = default) =>
        RunMutationAsync((connection, token) => connection.CancelCurrentTurnAsync(token), cancellationToken);

    public Task<WorkSubscribeResponse> SubscribeAsync(ulong? replayAfterSeq = null, CancellationToken cancellationToken = default) =>
        RunQueryAsync((connection, token) => connection.SubscribeAsync(replayAfterSeq, token), cancellationToken);

    /// <summary>
    /// Approval answers are deliberately NOT retried on a faulted
    /// connection: a lost frame could mean the decision was or was not
    /// delivered, and silently re-sending an Allow would fabricate consent.
    /// The error surfaces to the caller, who can re-list pending approvals
    /// and decide again with the fresh snapshot.
    /// </summary>
    public Task<ApprovalRespondResponse> RespondApprovalAsync(
        string requestId, ApprovalDecision decision, CancellationToken cancellationToken = default)
    {
        var connection = _connection;
        return connection is not null
            ? connection.RespondApprovalAsync(requestId, decision, cancellationToken)
            : Task.FromException<ApprovalRespondResponse>(
                new AgentContractViolationException("approval.respond", "no live connection; re-snapshot before answering"));
    }

    // -----------------------------------------------------------------------
    // B3 read-only routes. All four are queries (effect-free), so a faulted
    // connection reconnects once and re-issues exactly as SnapshotAsync does.
    // -----------------------------------------------------------------------

    public Task<WorkTaskDetailResponse> TaskDetailAsync(
        string taskId, CancellationToken cancellationToken = default) =>
        RunQueryAsync((connection, token) => connection.TaskDetailAsync(taskId, token), cancellationToken);

    /// <summary>EXEC-8 (R2-09): read-only cold completion lookup; a query,
    /// so a faulted connection reconnects once and re-issues.</summary>
    public Task<WorkTaskCompletionResponse> TaskCompletionAsync(
        string taskId, CancellationToken cancellationToken = default) =>
        RunQueryAsync((connection, token) => connection.TaskCompletionAsync(taskId, token), cancellationToken);

    public Task<WorkChangesResponse> ReadChangesAsync(
        int? limit = null, string? afterTx = null, CancellationToken cancellationToken = default) =>
        RunQueryAsync((connection, token) => connection.ReadChangesAsync(limit, afterTx, token), cancellationToken);

    public Task<WorkArtifactResponse> ReadArtifactAsync(
        string reference, uint? maxBytes = null, ulong? offset = null, CancellationToken cancellationToken = default) =>
        RunQueryAsync((connection, token) => connection.ReadArtifactAsync(reference, maxBytes, offset, token), cancellationToken);

    public Task<WorkContextResponse> ReadContextAsync(
        uint? limit = null, CancellationToken cancellationToken = default) =>
        RunQueryAsync((connection, token) => connection.ReadContextAsync(limit, token), cancellationToken);

    /// <summary>PLATFORM-1 (F06): queries the run's submission ledger for one
    /// exact <c>client_request_id</c>. A read like the B3 routes: a faulted
    /// connection reconnects once and re-issues, and an
    /// <c>Unknown</c>/<c>Expired</c> answer is a fact about missing evidence —
    /// never a license to resend.</summary>
    public Task<WorkSubmitResultResponse> SubmitResultAsync(
        string clientRequestId, string? payloadDigest = null, CancellationToken cancellationToken = default) =>
        RunQueryAsync((connection, token) => connection.SubmitResultAsync(clientRequestId, payloadDigest, token), cancellationToken);

    /// <summary>
    /// The session-level typed event stream (N3): every installed
    /// connection's work/event notifications are relayed here in host order,
    /// bounded by the same class-aware policy as the connection queue.
    /// Reconnects are seamless for the consumer: the old connection's
    /// notifications stop at the switch, its unread backlog is reset (B1 —
    /// the fresh snapshot rebuilds all durable state), the new connection
    /// starts from its subscribe watermark with the snapshot watermark
    /// deduping durable replays, and — on a normal reconnect — this reader
    /// never changes. The stream ends (with the reason) only if the queue
    /// must refuse an approval/terminal notification, or when the session is
    /// disposed. R13: after such an overflow the stream is a closed
    /// generation; a subsequent successful reconnect REBUILDS a fresh live
    /// stream, so a consumer re-reading <see cref="Events"/> after the
    /// resync receives new events again (the lost backlog is never replayed,
    /// only re-snapshot).
    public ChannelReader<WorkEventNotification> Events => _events.Reader;

    /// <summary>True once the session-level queue had to shed live-only
    /// progress notifications under pressure; re-snapshot rather than trust
    /// the merged stream. Approval/terminal notifications are never shed.</summary>
    public bool EventsDropped => _events.DroppedCount > 0;

    public async ValueTask DisposeAsync()
    {
        AgentConnection? connection;
        lock (_gate)
        {
            if (_disposed)
            {
                return;
            }
            _disposed = true;
            _generation++;
            connection = _connection;
            _connection = null;
        }
        if (connection is not null)
        {
            await connection.DisposeAsync().ConfigureAwait(false);
        }
        // The pump sees the staleness gate and stops; completing the queue
        // wakes every event consumer with the end of the stream.
        _events.TryComplete();
        // An in-flight connect attempt is deliberately not awaited here
        // (disposal must not hang on a stalled transport): its install check
        // refuses the disposed generation and releases the connection itself.
    }
}
