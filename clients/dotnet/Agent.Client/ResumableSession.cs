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

    private async Task<AgentConnection> LiveAsync(CancellationToken cancellationToken)
    {
        lock (_gate)
        {
            if (_connection?.IsConnected == true)
            {
                return _connection;
            }
        }
        var stale = Interlocked.Exchange(ref _connection, null);
        if (stale is not null)
        {
            await stale.DisposeAsync().ConfigureAwait(false);
        }
        var stream = await _connect().ConfigureAwait(false);
        var fresh = new AgentConnection(stream, _options);
        Interlocked.Exchange(ref _connection, fresh);
        // A reconnect rebuilds from a snapshot; the subscribe handshake is
        // issued so the host starts the stream at the current watermark.
        var snapshot = await fresh.SnapshotAsync(cancellationToken).ConfigureAwait(false);
        try
        {
            await fresh.SubscribeAsync(cancellationToken: cancellationToken).ConfigureAwait(false);
        }
        catch (AgentProtocolException)
        {
            // Subscribe refusal never blocks the snapshot path.
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
        var connection = Interlocked.Exchange(ref _connection, null);
        if (connection is not null)
        {
            await connection.DisposeAsync().ConfigureAwait(false);
        }
    }
}
