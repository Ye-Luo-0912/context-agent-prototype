using System.Text.Json;
using System.Threading.Channels;

namespace FocusAgent.Client;

/// <summary>
/// G2 operational-chain support on the client side. A resumable session owns
/// the reconnect policy: every reconnect rebuilds from a fresh snapshot and
/// reports <see cref="Resynced"/>; pending approvals are NEVER re-answered or
/// auto-approved across a reconnect — they are re-listed from the server's
/// snapshot, because the client cannot know what happened while offline.
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

    /// <summary>Runs one operation; a faulted connection reconnects once and
    /// rebuilds from a snapshot before the caller sees anything.</summary>
    private async Task<T> RunAsync<T>(
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

    public Task<WorkSnapshotResponse> SnapshotAsync(CancellationToken cancellationToken = default) =>
        RunAsync((connection, token) => connection.SnapshotAsync(token), cancellationToken);

    public Task<WorkSubmitResponse> SubmitWorkAsync(string goal, string clientRequestId, CancellationToken cancellationToken = default) =>
        RunAsync((connection, token) => connection.SubmitWorkAsync(goal, clientRequestId, token), cancellationToken);

    public Task<WorkContinueResponse> ContinueAsync(CancellationToken cancellationToken = default) =>
        RunAsync((connection, token) => connection.ContinueAsync(token), cancellationToken);

    public Task<WorkCancelResponse> CancelCurrentTurnAsync(CancellationToken cancellationToken = default) =>
        RunAsync((connection, token) => connection.CancelCurrentTurnAsync(token), cancellationToken);

    public Task<WorkSubscribeResponse> SubscribeAsync(ulong? replayAfterSeq = null, CancellationToken cancellationToken = default) =>
        RunAsync((connection, token) => connection.SubscribeAsync(replayAfterSeq, token), cancellationToken);

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
