using System.Collections.Concurrent;
using System.Text.Json;
using System.Threading.Channels;

namespace FocusAgent.Client;

/// <summary>Connection options shared by client and host deployments.</summary>
public sealed record AgentConnectionOptions
{
    /// <summary>Must match the host's negotiated profile exactly.</summary>
    public string ProtocolName { get; init; } = "focus-agent.platform";

    public ushort ProtocolMajor { get; init; } = 1;
    public ushort ProtocolMinor { get; init; } = 0;

    /// <summary>Hex digest binding the negotiated session contract surface.
    /// Defaults to the seed the Rust host negotiates; both sides refuse
    /// drift.</summary>
    public string SchemaDigest { get; init; } =
        "79eda3b0421ca507b2d9eaca68471dcaba350e3a971eda26efaeb691d60678cf";

    /// <summary>Per-request bound; a request that outlives it fails the wait
    /// but never fabricates a response.</summary>
    public TimeSpan RequestTimeout { get; init; } = TimeSpan.FromSeconds(30);

    public int MaxFrameBytes { get; init; } = FrameCodec.DefaultMaxFrameBytes;

    /// <summary>Bounded notification queue. When full, new notifications set
    /// <see cref="NotificationsDropped"/> instead of blocking the read loop.</summary>
    public int NotificationCapacity { get; init; } = 1_024;
}

/// <summary>
/// Raised when a request is refused because the connection has already
/// reached its terminal fault state. The original fault reason travels
/// along. The state is never revived: recover by opening a new connection.
/// </summary>
public sealed class AgentConnectionFaultedException : Exception
{
    public Exception? Reason { get; }

    public AgentConnectionFaultedException(Exception? reason)
        : base(reason is null
            ? "connection is faulted; no further requests are accepted"
            : $"connection is faulted; no further requests are accepted ({reason.Message})")
    {
        Reason = reason;
    }
}

/// <summary>
/// One live client session over a local transport. Request correlation is by
/// request id; cancelling a request's wait never cancels the server-side
/// operation — an explicit cancel goes through
/// <see cref="CancelCurrentTurnAsync"/>.
///
/// Faults are terminal (F08). A read-loop failure, a contract-violating
/// frame, or a failed/interrupted frame write (the frame boundary is lost —
/// half a frame may be on the wire) faults the whole connection exactly
/// once: the first fault reason wins, every pending request — including
/// requests still queued for the writer — fails with that reason, the
/// stream is closed to release the read loop, later requests are refused,
/// and <see cref="IsConnected"/> turns false. Nothing is silently retried
/// here and a faulted connection is never revived. One deliberate
/// distinction: a local wait cancellation or timeout after a fully sent
/// frame says nothing about the server side, so it abandons only the wait
/// and does not fault the connection.
/// </summary>
public sealed class AgentConnection : IAgentConnection
{
    /// <summary>
    /// The profile the Rust host (<c>agent-host</c>) negotiates. The schema
    /// digest is SHA-256 of the host's session-contract seed
    /// ("focus-agent.platform.work.v1|run-scoped"); both sides hard-code the
    /// same pairing and refuse any drift.
    /// </summary>
    public static ProtocolIdentity DefaultProtocolIdentity { get; } = new()
    {
        Name = "focus-agent.platform",
        Version = new ProtocolVersion { Major = 1, Minor = 0 },
        ActiveFeatures = new ActiveFeatures(),
        SchemaDigest = "79eda3b0421ca507b2d9eaca68471dcaba350e3a971eda26efaeb691d60678cf",
    };

    private readonly ProtocolIdentity _identity;
    private readonly AgentConnectionOptions _options;
    private readonly Stream _stream;
    private readonly SemaphoreSlim _writeLock = new(1, 1);
    private readonly ConcurrentDictionary<string, TaskCompletionSource<JsonElement>> _pending = new();
    private readonly CancellationTokenSource _disposed = new();
    private readonly Task _readLoop;
    private long _notificationsDropped;

    // F08: the single terminal fault state. 0 = live, 1 = faulted. The first
    // fault publishes its reason, then flips the flag; later faults are
    // no-ops and the state is never revived.
    private long _faulted;
    private Exception? _faultReason;

    public AgentConnection(Stream stream, AgentConnectionOptions? options = null)
    {
        _options = options ?? new AgentConnectionOptions();
        _identity = new ProtocolIdentity
        {
            Name = _options.ProtocolName,
            Version = new ProtocolVersion { Major = _options.ProtocolMajor, Minor = _options.ProtocolMinor },
            ActiveFeatures = new ActiveFeatures(),
            SchemaDigest = _options.SchemaDigest,
        };
        _stream = stream;
        _notifications = Channel.CreateBounded<JsonElement>(new BoundedChannelOptions(_options.NotificationCapacity)
        {
            SingleReader = false,
            SingleWriter = true,
        });
        _readLoop = Task.Run(() => ReadLoopAsync(_disposed.Token));
    }

    public ProtocolIdentity NegotiatedIdentity => _identity;

    public bool IsConnected =>
        !_readLoop.IsCompleted
        && !_disposed.IsCancellationRequested
        && Interlocked.Read(ref _faulted) == 0;

    /// <summary>Notifications (future event stream) as raw JSON elements.</summary>
    public ChannelReader<JsonElement> Notifications => _notifications.Reader;

    /// <summary>True once the bounded notification queue overflowed; the
    /// caller must re-snapshot rather than trust the merged stream.</summary>
    public bool NotificationsDropped => Interlocked.Read(ref _notificationsDropped) > 0;

    private readonly Channel<JsonElement> _notifications;

    public async Task<TResponse> SendAsync<TRequest, TResponse>(
        Route route, TRequest payload, CancellationToken cancellationToken = default)
        where TRequest : notnull
        where TResponse : notnull
    {
        ObjectDisposedException.ThrowIf(_disposed.IsCancellationRequested, this);
        if (Interlocked.Read(ref _faulted) != 0)
        {
            // F08: the fault state is terminal; every later request is
            // refused, including ones the caller still believes queued.
            throw new AgentConnectionFaultedException(_faultReason);
        }
        var request = SessionEnvelope.Request(route, payload);
        request.Validate(_identity);

        var requestId = request.RequestId!;
        var completion = new TaskCompletionSource<JsonElement>(TaskCreationOptions.RunContinuationsAsynchronously);
        _pending[requestId] = completion;
        try
        {
            if (Interlocked.Read(ref _faulted) != 0)
            {
                // The fault landed between the registration and the send:
                // fail without writing instead of queueing onto a dead pipe.
                throw new AgentConnectionFaultedException(_faultReason);
            }
            var encoded = JsonSerializer.SerializeToUtf8Bytes(request, AgentJson.Options);
            using var timeout = CancellationTokenSource.CreateLinkedTokenSource(cancellationToken, _disposed.Token);
            timeout.CancelAfter(_options.RequestTimeout);
            await _writeLock.WaitAsync(timeout.Token).ConfigureAwait(false);
            try
            {
                try
                {
                    await FrameCodec.WriteFrameAsync(_stream, encoded, _options.MaxFrameBytes, timeout.Token)
                        .ConfigureAwait(false);
                }
                catch (Exception writeFailure)
                {
                    // F08: a failed, timed-out, or cancelled write may have
                    // left a partial frame on the wire — the frame boundary
                    // is lost, so the connection is poisoned and never
                    // reused.
                    Fault(writeFailure);
                    throw;
                }
            }
            finally
            {
                _writeLock.Release();
            }

            var responseElement = await completion.Task.WaitAsync(timeout.Token).ConfigureAwait(false);
            try
            {
                var response = JsonSerializer.Deserialize<PlatformEnvelope<PlatformResponse<TResponse>>>(
                        responseElement.GetRawText(), AgentJson.Options)
                    ?? throw new AgentContractViolationException("envelope.decode", "response envelope was empty");
                response.ValidateAnswer(request, _identity);
                var body = response.Payload;
                body.Validate();
                return body.ExpectValue();
            }
            catch (Exception failure) when (failure is AgentContractViolationException or JsonException)
            {
                // A response that violates the negotiated bounded contract is
                // a connection fault: the peer is not speaking the profile.
                Fault(failure);
                throw;
            }
        }
        finally
        {
            _pending.TryRemove(requestId, out _);
        }
    }

    public Task<WorkSubmitResponse> SubmitWorkAsync(string goal, string clientRequestId, CancellationToken cancellationToken = default) =>
        SendAsync<WorkSubmitRequest, WorkSubmitResponse>(
            Route.WorkSubmitRoute(), new WorkSubmitRequest { Goal = goal, ClientRequestId = clientRequestId }, cancellationToken);

    public Task<WorkContinueResponse> ContinueAsync(CancellationToken cancellationToken = default) =>
        SendAsync<WorkContinueRequest, WorkContinueResponse>(Route.WorkContinueRoute(), new WorkContinueRequest(), cancellationToken);

    public Task<WorkCancelResponse> CancelCurrentTurnAsync(CancellationToken cancellationToken = default) =>
        SendAsync<WorkCancelRequest, WorkCancelResponse>(Route.WorkCancelRoute(), new WorkCancelRequest(), cancellationToken);

    public Task<WorkSnapshotResponse> SnapshotAsync(CancellationToken cancellationToken = default) =>
        SendAsync<WorkSnapshotRequest, WorkSnapshotResponse>(Route.WorkSnapshotRoute(), new WorkSnapshotRequest(), cancellationToken);

    public Task<WorkSubscribeResponse> SubscribeAsync(ulong? replayAfterSeq = null, CancellationToken cancellationToken = default) =>
        SendAsync<WorkSubscribeRequest, WorkSubscribeResponse>(
            Route.WorkSubscribeRoute(), new WorkSubscribeRequest { ReplayAfterSeq = replayAfterSeq }, cancellationToken);

    public Task<ApprovalRespondResponse> RespondApprovalAsync(
        string requestId, ApprovalDecision decision, CancellationToken cancellationToken = default) =>
        SendAsync<ApprovalRespondRequest, ApprovalRespondResponse>(
            Route.ApprovalRespondRoute(), new ApprovalRespondRequest { RequestId = requestId, Decision = decision }, cancellationToken);

    private async Task ReadLoopAsync(CancellationToken cancellationToken)
    {
        try
        {
            while (!cancellationToken.IsCancellationRequested)
            {
                var frame = await FrameCodec.ReadFrameAsync(_stream, _options.MaxFrameBytes, cancellationToken)
                    .ConfigureAwait(false);
                if (frame is null)
                {
                    throw new AgentContractViolationException("connection", "host closed the stream");
                }
                Dispatch(JsonDocument.Parse(frame));
            }
        }
        catch (Exception failure) when (failure is not OperationCanceledException)
        {
            Fault(failure);
        }
        finally
        {
            Fault(new ObjectDisposedException(nameof(AgentConnection)));
        }
    }

    private void Dispatch(JsonDocument document)
    {
        using (document)
        {
            var root = document.RootElement.Clone();
            if (!root.TryGetProperty("kind", out var kindElement)
                || !root.TryGetProperty("request_id", out var requestIdElement))
            {
                Fault(new AgentContractViolationException("envelope", "frame is missing kind/request_id"));
                return;
            }
            var kind = kindElement.GetString();
            var requestId = requestIdElement.GetString();
            if (kind == EnvelopeKind.Response.ToString().ToLowerInvariant() && requestId is not null)
            {
                if (_pending.TryRemove(requestId, out var completion))
                {
                    completion.TrySetResult(root);
                }
                // A response whose request already timed out is dropped on
                // purpose: the wait was abandoned, not the server-side work.
                return;
            }
            if (kind == EnvelopeKind.Notification.ToString().ToLowerInvariant())
            {
                if (!_notifications.Writer.TryWrite(root))
                {
                    Interlocked.Increment(ref _notificationsDropped);
                }
                return;
            }
            Fault(new AgentContractViolationException("envelope.kind", $"unexpected frame kind '{kind}'"));
        }
    }

    /// <summary>
    /// F08: the single terminal fault path. The first fault fixes the
    /// reason, fails every pending waiter (including requests queued before
    /// the fault), completes the bounded notification queue, and closes the
    /// stream so the read loop wakes up and exits. Later faults are no-ops:
    /// the state is terminal and never revived. Full resource cleanup stays
    /// with <see cref="DisposeAsync"/>, which tolerates the double dispose.
    /// </summary>
    private void Fault(Exception failure)
    {
        Volatile.Write(ref _faultReason, failure);
        if (Interlocked.CompareExchange(ref _faulted, 1, 0) != 0)
        {
            return; // already terminal: the first reason stands
        }
        foreach (var entry in _pending)
        {
            if (_pending.TryRemove(entry.Key, out var completion))
            {
                completion.TrySetException(failure);
            }
        }
        _notifications.Writer.TryComplete(failure);
        try
        {
            // Closing the stream unblocks a read parked on the socket.
            _stream.DisposeAsync().AsTask().GetAwaiter().GetResult();
        }
        catch
        {
            // The stream may already be gone; the fault reason is what counts.
        }
    }

    public async ValueTask DisposeAsync()
    {
        if (_disposed.IsCancellationRequested)
        {
            return;
        }
        await _disposed.CancelAsync().ConfigureAwait(false);
        try
        {
            await _readLoop.ConfigureAwait(false);
        }
        catch
        {
            // The read loop already faulted pending work with the real reason.
        }
        _writeLock.Dispose();
        await _stream.DisposeAsync().ConfigureAwait(false);
        _disposed.Dispose();
    }
}
