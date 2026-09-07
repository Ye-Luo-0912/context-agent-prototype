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
    /// The host and every client must agree on this value.</summary>
    public string SchemaDigest { get; init; } = new('0', 64);

    /// <summary>Per-request bound; a request that outlives it fails the wait
    /// but never fabricates a response.</summary>
    public TimeSpan RequestTimeout { get; init; } = TimeSpan.FromSeconds(30);

    public int MaxFrameBytes { get; init; } = FrameCodec.DefaultMaxFrameBytes;

    /// <summary>Bounded notification queue. When full, new notifications set
    /// <see cref="NotificationsDropped"/> instead of blocking the read loop.</summary>
    public int NotificationCapacity { get; init; } = 1_024;
}

/// <summary>
/// One live client session over a local transport. Request correlation is by
/// request id; cancelling a request's wait never cancels the server-side
/// operation — an explicit cancel goes through
/// <see cref="CancelCurrentTurnAsync"/>. A read-loop failure or a frame that
/// violates the contract faults the whole connection: every pending request
/// fails with the fault reason and nothing is silently retried.
/// </summary>
public sealed class AgentConnection : IAgentConnection
{
    public static ProtocolIdentity DefaultProtocolIdentity { get; } = new()
    {
        Name = "focus-agent.platform",
        Version = new ProtocolVersion { Major = 1, Minor = 0 },
        ActiveFeatures = new ActiveFeatures(),
        SchemaDigest = new string('0', 64),
    };

    private readonly ProtocolIdentity _identity;
    private readonly AgentConnectionOptions _options;
    private readonly Stream _stream;
    private readonly SemaphoreSlim _writeLock = new(1, 1);
    private readonly ConcurrentDictionary<string, TaskCompletionSource<JsonElement>> _pending = new();
    private readonly CancellationTokenSource _disposed = new();
    private readonly Task _readLoop;
    private long _notificationsDropped;

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

    public bool IsConnected => !_readLoop.IsCompleted && !_disposed.IsCancellationRequested;

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
        var request = SessionEnvelope.Request(route, payload);
        request.Validate(_identity);

        var requestId = request.RequestId!;
        var completion = new TaskCompletionSource<JsonElement>(TaskCreationOptions.RunContinuationsAsynchronously);
        _pending[requestId] = completion;
        try
        {
            var encoded = JsonSerializer.SerializeToUtf8Bytes(request, AgentJson.Options);
            using var timeout = CancellationTokenSource.CreateLinkedTokenSource(cancellationToken, _disposed.Token);
            timeout.CancelAfter(_options.RequestTimeout);
            await _writeLock.WaitAsync(timeout.Token).ConfigureAwait(false);
            try
            {
                await FrameCodec.WriteFrameAsync(_stream, encoded, _options.MaxFrameBytes, timeout.Token)
                    .ConfigureAwait(false);
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
            return body.ExpectValue();
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

    private void Fault(Exception failure)
    {
        foreach (var entry in _pending)
        {
            if (_pending.TryRemove(entry.Key, out var completion))
            {
                completion.TrySetException(failure);
            }
        }
        _notifications.Writer.TryComplete(failure);
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
