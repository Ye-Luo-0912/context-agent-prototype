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

    /// <summary>Bounded work/event notification queue. When full the policy
    /// in <see cref="BoundedEventQueue"/> applies: live-only progress
    /// (<c>model_delta</c>/<c>model_retrying</c>) sheds oldest-first, while
    /// approval/terminal notifications are never dropped — a queue that
    /// cannot admit a durable fact without shedding anything faults the
    /// connection instead.</summary>
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
///
/// F19: every typed API validates its request payload before any byte is
/// written and its response payload once the answer is accepted. A
/// validation failure is a protocol fault — it takes the same terminal path,
/// so a contract-violating request never reaches the wire and nothing is
/// re-sent for it.
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
    private readonly BoundedEventQueue _events;

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
        _events = new BoundedEventQueue(_options.NotificationCapacity);
        _readLoop = Task.Run(() => ReadLoopAsync(_disposed.Token));
    }

    public ProtocolIdentity NegotiatedIdentity => _identity;

    public bool IsConnected =>
        !_readLoop.IsCompleted
        && !_disposed.IsCancellationRequested
        && Interlocked.Read(ref _faulted) == 0;

    /// <summary>The typed work/event notification stream (N3). Notifications
    /// arrive in host order and are never paired with request/response
    /// traffic; the queue bound and its overflow policy are documented on
    /// <see cref="BoundedEventQueue"/>.</summary>
    public ChannelReader<WorkEventNotification> Events => _events.Reader;

    /// <summary>True once the bounded event queue had to shed live-only
    /// progress notifications under pressure; the caller must re-snapshot
    /// rather than trust the merged stream. Approval/terminal notifications
    /// are never shed — an unsheddable overflow faults the connection
    /// instead.</summary>
    public bool EventsDropped => _events.DroppedCount > 0;

    public async Task<TResponse> SendAsync<TRequest, TResponse>(
        Route route, TRequest payload, CancellationToken cancellationToken = default)
        where TRequest : notnull, IProtocolPayload
        where TResponse : notnull, IProtocolPayload
    {
        ObjectDisposedException.ThrowIf(_disposed.IsCancellationRequested, this);
        if (Interlocked.Read(ref _faulted) != 0)
        {
            // F08: the fault state is terminal; every later request is
            // refused, including ones the caller still believes queued.
            throw new AgentConnectionFaultedException(_faultReason);
        }
        var request = SessionEnvelope.Request(route, payload);

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
            try
            {
                // F19: the typed request payload validator runs before any
                // byte is written; a violation is a protocol fault, so a
                // malformed request can never reach the wire (and a mutation
                // is never re-sent for it).
                payload.Validate();
                request.Validate(_identity);

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
                        // If the reader pump had already taken the terminal
                        // state with a different reason (a bad server frame
                        // racing this write), surface THAT instead of the raw
                        // write/disposal error — waiters see one honest
                        // diagnosis, not an implementation detail. A contract
                        // violation is thrown as itself so callers can name
                        // the exact guard that fired.
                        var standing = Volatile.Read(ref _faultReason);
                        if (standing is not null && !ReferenceEquals(standing, writeFailure))
                        {
                            throw standing is AgentContractViolationException violation
                                ? violation
                                : new AgentConnectionFaultedException(standing);
                        }
                        throw;
                    }
                }
                finally
                {
                    _writeLock.Release();
                }

                var responseElement = await completion.Task.WaitAsync(timeout.Token).ConfigureAwait(false);
                var response = JsonSerializer.Deserialize<PlatformEnvelope<PlatformResponse<TResponse>>>(
                        responseElement.GetRawText(), AgentJson.Options)
                    ?? throw new AgentContractViolationException("envelope.decode", "response envelope was empty");
                response.ValidateAnswer(request, _identity);
                var body = response.Payload;
                body.Validate();
                var value = body.ExpectValue();
                // F19: the response payload validator runs once the answer is
                // accepted; a contract-violating payload is a protocol fault.
                value.Validate();
                return value;
            }
            catch (Exception failure) when (failure is AgentContractViolationException or JsonException)
            {
                // F19: a contract-violating request composition or response is
                // a protocol fault (F08 terminal path) — the peer is not
                // speaking the bounded profile, and the client never re-sends
                // the request for it.
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

    /// <summary>
    /// N3/F06 (client half): frames are dispatched by their <c>kind</c>
    /// BEFORE anything is required of <c>request_id</c>. Host event
    /// notifications (<c>kind=notification</c>, route work/event) carry no
    /// request id by contract and are consumed as typed
    /// <see cref="WorkEventNotification"/>s; only request/response frames
    /// take the pairing path, which requires a request id. Every bad frame —
    /// missing kind, a request/response without request_id, a malformed
    /// notification, a foreign route — hits the same terminal fault path as
    /// any other contract violation (N2).
    /// </summary>
    private void Dispatch(JsonDocument document)
    {
        using (document)
        {
            var root = document.RootElement.Clone();
            if (root.ValueKind != JsonValueKind.Object || !root.TryGetProperty("kind", out var kindElement))
            {
                Fault(new AgentContractViolationException("envelope", "frame is missing kind"));
                return;
            }
            switch (kindElement.GetString())
            {
                case "response":
                case "request":
                    // Both paired kinds share the request-id pairing path; a
                    // client connection solicits responses, never requests,
                    // so a stray host->client request simply finds no waiter.
                    DispatchPaired(root);
                    return;
                case "notification":
                    DispatchNotification(root);
                    return;
                case null:
                    Fault(new AgentContractViolationException("envelope.kind", "must be a string"));
                    return;
                default:
                    Fault(new AgentContractViolationException(
                        "envelope.kind", $"unexpected frame kind '{kindElement.GetString()}'"));
                    return;
            }
        }
    }

    /// <summary>Paired frames (request/response) must carry a request id and
    /// are matched to their waiter by it. A frame whose request already
    /// timed out — or one the client never sent — has no waiter and is
    /// dropped on purpose: the wait was abandoned, not the server-side
    /// work.</summary>
    private void DispatchPaired(JsonElement root)
    {
        if (!root.TryGetProperty("request_id", out var requestIdElement)
            || requestIdElement.GetString() is not { Length: > 0 } requestId)
        {
            Fault(new AgentContractViolationException("envelope.request_id", "is required for a request/response"));
            return;
        }
        if (_pending.TryRemove(requestId, out var completion))
        {
            completion.TrySetResult(root);
        }
    }

    /// <summary>Notifications (kind=notification, no request id) are decoded
    /// against the full envelope contract — protocol identity, route,
    /// causality, request_id absence, and the work/event payload — and then
    /// delivered as typed <see cref="WorkEventNotification"/>s. Any violation
    /// is the same terminal fault as any other bad frame.</summary>
    private void DispatchNotification(JsonElement root)
    {
        WorkEventNotification notification;
        try
        {
            var envelope = JsonSerializer.Deserialize<PlatformEnvelope<WorkEventNotification>>(
                    root.GetRawText(), AgentJson.Options)
                ?? throw new AgentContractViolationException("envelope.decode", "notification envelope was empty");
            envelope.Validate(_identity);
            if (envelope.Route.Namespace != Route.WorkNamespace
                || envelope.Route.Operation != Route.WorkEvent)
            {
                throw new AgentContractViolationException(
                    "envelope.route", "notification must use the work/event route");
            }
            notification = envelope.Payload;
            notification.Validate();
        }
        catch (Exception failure) when (failure is AgentContractViolationException or JsonException)
        {
            Fault(failure);
            return;
        }
        if (!_events.TryEnqueue(notification))
        {
            // The queue refused a durable notification with nothing left to
            // shed: the consumer stopped draining, and silently losing an
            // approval/terminal fact is not an option. Terminal fault; the
            // recovery is a fresh connection rebuilt from a snapshot.
            Fault(new AgentContractViolationException(
                "work.event.queue",
                "overflowed with undroppable approval/terminal notifications; rebuild from a snapshot"));
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
        // The first reason stands: a later Fault (a write racing the
        // reader's terminal path) must not overwrite the diagnosis waiters
        // and the event stream already carry.
        if (Interlocked.CompareExchange(ref _faulted, 1, 0) != 0)
        {
            return; // already terminal: the first reason stands
        }
        Volatile.Write(ref _faultReason, failure);
        foreach (var entry in _pending)
        {
            if (_pending.TryRemove(entry.Key, out var completion))
            {
                completion.TrySetException(failure);
            }
        }
        // Completing the event queue (with the fault reason) wakes every
        // event consumer: the stream ends in the connection's terminal state.
        _events.TryComplete(failure);
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
