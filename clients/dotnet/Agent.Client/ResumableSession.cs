using System.Text.Json;
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
/// </summary>
public sealed class ResumableSession : IAsyncDisposable
{
    private readonly AgentConnectionOptions _options;
    private readonly Func<Task<Stream>> _connect;
    private readonly object _gate = new();
    private AgentConnection? _connection;

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
    }

    /// <summary>Raised after every (re)connect with the fresh snapshot.</summary>
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
            // A reconnect rebuilds from a snapshot; the subscribe handshake is
            // issued so the host starts the stream at the current watermark.
            // The shared attempt must not inherit one waiter's cancellation
            // token, and each request stays bounded by its own timeout.
            snapshot = await fresh.SnapshotAsync(CancellationToken.None).ConfigureAwait(false);
            try
            {
                await fresh.SubscribeAsync(cancellationToken: CancellationToken.None).ConfigureAwait(false);
            }
            catch (AgentProtocolException)
            {
                // Subscribe refusal never blocks the snapshot path.
            }
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
        Resynced?.Invoke(snapshot);
        return fresh;
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

    /// <summary>The live connection's bounded notification stream, if any.</summary>
    public ChannelReader<JsonElement>? Notifications => _connection?.Notifications;

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
        // An in-flight connect attempt is deliberately not awaited here
        // (disposal must not hang on a stalled transport): its install check
        // refuses the disposed generation and releases the connection itself.
    }
}
