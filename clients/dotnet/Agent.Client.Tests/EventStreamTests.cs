using System.Net;
using System.Net.Sockets;
using System.Text.Json;
using System.Threading.Channels;

using Xunit;

namespace FocusAgent.Client.Tests;

/// <summary>
/// N3/F06 (client half): host event notifications — kind=notification frames
/// without a request_id — are typed-consumed instead of being treated as bad
/// frames, notifications never pair with responses, and the bounded queue's
/// class-aware overflow policy never sheds approval/terminal facts.
/// </summary>
public class EventStreamTests
{
    private const string RunId = "00000000-0000-4000-8000-000000000001";

    /// <summary>A loopback host whose accepted connections are each driven by
    /// a per-ordinal script delegate (answers handshakes, interleaves
    /// notifications, or sends deliberately bad frames).</summary>
    private sealed class ScriptedEventHost : IAsyncDisposable
    {
        private readonly TcpListener _listener = new(IPAddress.Loopback, 0);
        private readonly CancellationTokenSource _stopped = new();
        private readonly Task _serveLoop;
        private int _connectionsAccepted;

        public ScriptedEventHost()
        {
            _listener.Start();
            _serveLoop = Task.Run(() => ServeAsync(_stopped.Token));
        }

        public int Port => ((IPEndPoint)_listener.LocalEndpoint).Port;

        public int ConnectionsAccepted => Volatile.Read(ref _connectionsAccepted);

        public Func<int, Stream, CancellationToken, Task> Script { get; set; } =
            static (ordinal, stream, cancellationToken) => Task.CompletedTask;

        private async Task ServeAsync(CancellationToken cancellationToken)
        {
            try
            {
                while (!cancellationToken.IsCancellationRequested)
                {
                    var client = await _listener.AcceptTcpClientAsync(cancellationToken);
                    var ordinal = Interlocked.Increment(ref _connectionsAccepted) - 1;
                    _ = Task.Run(() => ServeClientAsync(client, ordinal, cancellationToken), cancellationToken);
                }
            }
            catch (OperationCanceledException)
            {
            }
            catch (ObjectDisposedException)
            {
            }
            catch (SocketException)
            {
                // listener stopped during accept: the drill is over
            }
        }

        private async Task ServeClientAsync(TcpClient client, int ordinal, CancellationToken cancellationToken)
        {
            using var owned = client;
            var stream = client.GetStream();
            try
            {
                await Script(ordinal, stream, cancellationToken).ConfigureAwait(false);
            }
            catch (OperationCanceledException)
            {
            }
            catch (Exception)
            {
                // A dropped drill socket lands here; the drill asserts on the
                // client side stay authoritative.
            }
        }

        public async ValueTask DisposeAsync()
        {
            await _stopped.CancelAsync();
            _listener.Stop();
            try
            {
                await _serveLoop.ConfigureAwait(false);
            }
            catch
            {
            }
        }
    }

    private static JsonElement ProtocolElement() =>
        JsonSerializer.SerializeToElement(AgentConnection.DefaultProtocolIdentity, AgentJson.Options);

    /// <summary>Builds one work/event notification frame exactly as the Rust
    /// host forwards a <c>WorkEventNotification</c>: kind=notification, no
    /// request_id, payload <c>{"envelope": …}</c>. The options add a
    /// request_id or move the route for the bad-frame drills.</summary>
    private static byte[] NotificationFrame(
        ulong seq, string eventJson, string? requestId = null, string? operation = null)
    {
        var messageId = Guid.NewGuid().ToString("D");
        var frame = new Dictionary<string, object?>
        {
            ["protocol"] = ProtocolElement(),
            ["message_id"] = messageId,
            ["kind"] = "notification",
            ["route"] = new { @namespace = "work", operation = operation ?? "event" },
            ["causality"] = new { correlation_id = messageId },
            ["payload"] = new
            {
                envelope = new
                {
                    run_id = RunId,
                    seq = seq,
                    timestamp_ms = 1_000ul + seq,
                    @event = JsonSerializer.Deserialize<JsonElement>(eventJson),
                },
            },
        };
        if (requestId is not null)
        {
            frame["request_id"] = requestId;
        }
        return JsonSerializer.SerializeToUtf8Bytes(frame, AgentJson.Options);
    }

    private static readonly object SnapshotPayload = new
    {
        run_started = true,
        run_completed = false,
        watermark = 41ul,
        focus = (object?)null,
        tasks = Array.Empty<object>(),
        pending_approvals = Array.Empty<object>(),
        resync_required = false,
    };

    private static byte[] ResponseFrame(JsonElement request, object payload)
    {
        return JsonSerializer.SerializeToUtf8Bytes(new
        {
            protocol = request.GetProperty("protocol"),
            message_id = Guid.NewGuid().ToString("D"),
            request_id = request.GetProperty("request_id"),
            kind = "response",
            route = request.GetProperty("route"),
            causality = new
            {
                correlation_id = request.GetProperty("causality").GetProperty("correlation_id"),
                causation_id = request.GetProperty("message_id"),
            },
            payload,
        }, AgentJson.Options);
    }

    private static byte[] EncodeFrame(object frame) => JsonSerializer.SerializeToUtf8Bytes(frame, AgentJson.Options);

    /// <summary>Answers the resumable handshake (snapshot, then subscribe) on
    /// one scripted connection.</summary>
    private static async Task AnswerHandshakeAsync(Stream stream, CancellationToken cancellationToken)
    {
        await AnswerOneRequestAsync(stream, SnapshotPayload, cancellationToken);
        await AnswerOneRequestAsync(stream, new { watermark = 41ul, resync_required = false }, cancellationToken);
    }

    /// <summary>Answers one request with a success-wrapped payload — the
    /// status-tagged shape the shared <c>PlatformResponse</c> contract
    /// requires.</summary>
    private static async Task AnswerOneRequestAsync(
        Stream stream, object payload, CancellationToken cancellationToken)
    {
        var request = await FrameCodec.ReadFrameAsync(stream, FrameCodec.DefaultMaxFrameBytes, cancellationToken)
            ?? throw new InvalidOperationException("client closed before answering");
        using var document = JsonDocument.Parse(request);
        await WriteFrameAsync(
            stream, ResponseFrame(document.RootElement, new { status = "success", value = payload }),
            cancellationToken);
    }

    private static Task WriteFrameAsync(Stream stream, byte[] frame, CancellationToken cancellationToken) =>
        FrameCodec.WriteFrameAsync(stream, frame, FrameCodec.DefaultMaxFrameBytes, cancellationToken);

    private static async Task<Stream> ConnectAsync(int port)
    {
        var client = new TcpClient();
        await client.ConnectAsync(IPAddress.Loopback, port);
        return client.GetStream();
    }

    private static async Task<WorkEventNotification> ReadEventAsync(
        ChannelReader<WorkEventNotification> reader, TimeSpan budget)
    {
        using var cancel = new CancellationTokenSource(budget);
        while (true)
        {
            if (reader.TryRead(out var notification))
            {
                return notification;
            }
            Assert.True(
                await reader.WaitToReadAsync(cancel.Token),
                "event stream ended before the expected notification");
        }
    }

    [Fact]
    public async Task Notification_without_request_id_is_delivered_as_a_typed_event()
    {
        await using var host = new ScriptedEventHost();
        host.Script = async (_, stream, cancellationToken) =>
        {
            // The notification frame goes out BEFORE the subscribe response:
            // the read loop must route it to the event path without
            // disturbing the request/response pairing.
            await WriteFrameAsync(stream, NotificationFrame(7, "{\"type\":\"run_started\"}"), cancellationToken);
            await AnswerOneRequestAsync(
                stream, new { watermark = 41ul, resync_required = false }, cancellationToken);
            // Hold the socket open: a script return would close it and take
            // the connection down the terminal path mid-drill.
            await Task.Delay(Timeout.Infinite, cancellationToken);
        };

        await using var connection = new AgentConnection(await ConnectAsync(host.Port));
        var subscribe = connection.SubscribeAsync();
        var notification = await ReadEventAsync(connection.Events, TimeSpan.FromSeconds(10));

        Assert.Equal(RunId, notification.Envelope.RunId);
        Assert.Equal(7ul, notification.Envelope.Seq);
        Assert.Equal(1007ul, notification.Envelope.TimestampMs);
        Assert.Equal("run_started", notification.EventType);
        Assert.False(notification.IsLiveOnlyProgress);

        var response = await subscribe.WaitAsync(TimeSpan.FromSeconds(10));
        Assert.Equal(41ul, response.Watermark);
        Assert.True(connection.IsConnected);
    }

    [Fact]
    public async Task Bad_notifications_fault_the_connection()
    {
        var messageIdForEnvelopeDrill = Guid.NewGuid().ToString("D");
        var badFrames = new Dictionary<string, byte[]>
        {
            // request_id must be absent from a notification
            ["notification_with_request_id"] =
                NotificationFrame(1, "{\"type\":\"run_started\"}", requestId: Guid.NewGuid().ToString("D")),
            // the only negotiated notification route is work/event
            ["notification_on_foreign_route"] =
                NotificationFrame(1, "{\"type\":\"run_started\"}", operation: "snapshot"),
            // the event body must carry a non-empty "type" tag
            ["event_without_type_tag"] =
                NotificationFrame(1, "{}"),
            // the payload must carry the envelope (causality stays valid so
            // the payload guard is what fires)
            ["notification_without_envelope"] =
                JsonSerializer.SerializeToUtf8Bytes(new Dictionary<string, object?>
                {
                    ["protocol"] = ProtocolElement(),
                    ["message_id"] = messageIdForEnvelopeDrill,
                    ["kind"] = "notification",
                    ["route"] = new { @namespace = "work", operation = "event" },
                    ["causality"] = new { correlation_id = messageIdForEnvelopeDrill },
                    ["payload"] = new { },
                }, AgentJson.Options),
            // dispatch needs a kind before anything else
            ["frame_without_kind"] =
                JsonSerializer.SerializeToUtf8Bytes(new Dictionary<string, object?>
                {
                    ["protocol"] = ProtocolElement(),
                    ["message_id"] = Guid.NewGuid().ToString("D"),
                    ["route"] = new { @namespace = "work", operation = "event" },
                    ["payload"] = new { envelope = new { } },
                }, AgentJson.Options),
        };

        foreach (var (name, frame) in badFrames)
        {
            await using var host = new ScriptedEventHost();
            host.Script = async (_, stream, cancellationToken) =>
            {
                // Bad frame FIRST: the pending subscribe waiter must die with
                // the fault reason instead of hanging.
                await WriteFrameAsync(stream, frame, cancellationToken);
                await AnswerOneRequestAsync(
                    stream, new { watermark = 41ul, resync_required = false }, cancellationToken);
            };

            await using var connection = new AgentConnection(await ConnectAsync(host.Port));
            var failure = await Assert.ThrowsAsync<AgentContractViolationException>(
                () => connection.SubscribeAsync().WaitAsync(TimeSpan.FromSeconds(10)));
            Assert.False(connection.IsConnected, $"{name} must fault the connection");
            // The assert names the exact guard that fired, not just any fault.
            Assert.StartsWith("invalid " + FailureField(name), failure.Message);

            // The terminal state refuses later requests without new bytes.
            await Assert.ThrowsAsync<AgentConnectionFaultedException>(
                () => connection.SubmitWorkAsync("n3: refused", ClientRequestIds.Next())
                    .WaitAsync(TimeSpan.FromSeconds(10)));

            // The event stream ends in the same terminal state, with the reason.
            await Assert.ThrowsAsync<AgentContractViolationException>(
                () => connection.Events.WaitToReadAsync(CancellationToken.None).AsTask()
                    .WaitAsync(TimeSpan.FromSeconds(10)));
        }
    }

    /// <summary>The envelope field each bad-frame drill is expected to name,
    /// so the assert proves WHICH guard fired rather than any fault.</summary>
    private static string FailureField(string drill) => drill switch
    {
        "notification_with_request_id" => "envelope.request_id:",
        "notification_on_foreign_route" => "envelope.route:",
        "event_without_type_tag" => "work.event.envelope.event.type:",
        "notification_without_envelope" => "work.event.envelope.run_id:",
        "frame_without_kind" => "envelope:",
        _ => throw new InvalidOperationException(drill),
    };

    [Fact]
    public async Task Events_and_responses_share_one_connection_without_confusion()
    {
        await using var host = new ScriptedEventHost();
        host.Script = async (_, stream, cancellationToken) =>
        {
            for (var turn = 0; turn < 3; turn++)
            {
                var request = await FrameCodec.ReadFrameAsync(
                    stream, FrameCodec.DefaultMaxFrameBytes, cancellationToken)
                    ?? throw new InvalidOperationException("client closed early");
                using var document = JsonDocument.Parse(request);
                var operation = document.RootElement.GetProperty("route").GetProperty("operation").GetString();
                var seq = (ulong)(turn + 1);
                // Every response is preceded by an event: only kind-based
                // dispatch can keep the two lanes apart.
                await WriteFrameAsync(stream, NotificationFrame(
                    seq, turn == 0 ? "{\"type\":\"model_delta\",\"delta\":\"…\"}" : "{\"type\":\"turn_completed\"}"),
                    cancellationToken);
                var payload = operation switch
                {
                    "submit" => (object)new
                    {
                        status = "success",
                        value = new { disposition = "accepted", task_id = "00000000-0000-4000-8000-000000000022" },
                    },
                    "snapshot" => (object)new { status = "success", value = SnapshotPayload },
                    _ => (object)new { status = "success", value = new { watermark = 41ul, resync_required = false } },
                };
                await WriteFrameAsync(stream, ResponseFrame(document.RootElement, payload), cancellationToken);
            }
            await Task.Delay(Timeout.Infinite, cancellationToken);
        };

        await using var connection = new AgentConnection(await ConnectAsync(host.Port));
        var submit = connection.SubmitWorkAsync("n3: mixed lanes", "mix-1");
        var snapshot = connection.SnapshotAsync();
        var subscribe = connection.SubscribeAsync();

        Assert.Equal(WorkSubmitDisposition.Accepted, (await submit.WaitAsync(TimeSpan.FromSeconds(10))).Disposition);
        Assert.Equal(41ul, (await snapshot.WaitAsync(TimeSpan.FromSeconds(10))).Watermark);
        Assert.Equal(41ul, (await subscribe.WaitAsync(TimeSpan.FromSeconds(10))).Watermark);

        var first = await ReadEventAsync(connection.Events, TimeSpan.FromSeconds(10));
        var second = await ReadEventAsync(connection.Events, TimeSpan.FromSeconds(10));
        var third = await ReadEventAsync(connection.Events, TimeSpan.FromSeconds(10));
        Assert.Equal(new ulong[] { 1, 2, 3 }, new[] { first.Envelope.Seq, second.Envelope.Seq, third.Envelope.Seq });
        Assert.True(first.IsLiveOnlyProgress);      // model_delta
        Assert.False(second.IsLiveOnlyProgress);    // turn_completed
        Assert.False(third.IsLiveOnlyProgress);
        Assert.True(connection.IsConnected);
    }

    [Fact]
    public async Task Approval_and_terminal_notifications_survive_queue_pressure()
    {
        const int capacity = 8;
        await using var host = new ScriptedEventHost();
        host.Script = async (_, stream, cancellationToken) =>
        {
            var subscribe = await FrameCodec.ReadFrameAsync(
                stream, FrameCodec.DefaultMaxFrameBytes, cancellationToken)
                ?? throw new InvalidOperationException("client closed early");
            using var document = JsonDocument.Parse(subscribe);
            // 20 live-only progress events flood the (unread) queue, then one
            // terminal event arrives with the queue full. The subscribe
            // response is written LAST, so it completes only after the read
            // loop has dispatched every notification.
            for (ulong seq = 1; seq <= 20; seq++)
            {
                await WriteFrameAsync(stream, NotificationFrame(seq, "{\"type\":\"model_delta\",\"delta\":\"d\"}"), cancellationToken);
            }
            await WriteFrameAsync(stream, NotificationFrame(21, "{\"type\":\"task_completed\"}"), cancellationToken);
            await WriteFrameAsync(
                stream,
                ResponseFrame(
                    document.RootElement,
                    new { status = "success", value = new { watermark = 41ul, resync_required = false } }),
                cancellationToken);
            await Task.Delay(Timeout.Infinite, cancellationToken);
        };

        await using var connection = new AgentConnection(
            await ConnectAsync(host.Port),
            new AgentConnectionOptions { NotificationCapacity = capacity });

        await connection.SubscribeAsync().WaitAsync(TimeSpan.FromSeconds(10));

        // The queue kept exactly its capacity: the seven NEWEST progress
        // notifications plus the terminal one — the terminal fact was never
        // shed, and the queue never refused (the connection stays live).
        var drained = new List<WorkEventNotification>();
        while (connection.Events.TryRead(out var notification))
        {
            drained.Add(notification);
        }
        Assert.Equal(capacity, drained.Count);
        Assert.Equal("task_completed", drained[^1].EventType);
        Assert.Equal(21ul, drained[^1].Envelope.Seq);
        Assert.All(drained.Take(capacity - 1), notification => Assert.Equal("model_delta", notification.EventType));
        Assert.Equal(new ulong[] { 14, 15, 16, 17, 18, 19, 20 }, drained.Take(capacity - 1).Select(n => n.Envelope.Seq).ToArray());
        Assert.True(connection.EventsDropped);
        Assert.True(connection.IsConnected);
    }

    [Fact]
    public async Task Resumable_session_relay_stops_for_the_old_connection_and_continues_with_the_new()
    {
        await using var host = new ScriptedEventHost();
        var dropFirst = new TaskCompletionSource(TaskCreationOptions.RunContinuationsAsynchronously);
        host.Script = async (ordinal, stream, cancellationToken) =>
        {
            await AnswerHandshakeAsync(stream, cancellationToken);
            // Each connection announces itself with one terminal event.
            await WriteFrameAsync(
                stream,
                NotificationFrame(
                    100ul + (ulong)ordinal,
                    ordinal == 0 ? "{\"type\":\"turn_completed\"}" : "{\"type\":\"task_completed\"}"),
                cancellationToken);
            if (ordinal == 0)
            {
                // The query that installed this connection re-issues its
                // snapshot on it — answer exactly that one, then park: the
                // drill's next query stays unanswered until the socket drops.
                await AnswerOneRequestAsync(stream, SnapshotPayload, cancellationToken);
                await dropFirst.Task.WaitAsync(cancellationToken);
            }
            else
            {
                // Keep serving the reconnect drills until the host stops.
                while (!cancellationToken.IsCancellationRequested)
                {
                    await AnswerOneRequestAsync(stream, SnapshotPayload, cancellationToken);
                }
            }
        };

        var losses = new List<Exception>();
        var session = new ResumableSession(() => ConnectAsync(host.Port));
        session.ConnectionLost += failure => losses.Add(failure);
        try
        {
            var snapshot = await session.SnapshotAsync().WaitAsync(TimeSpan.FromSeconds(10));
            Assert.Equal(41ul, snapshot.Watermark);

            // ONE stable session-level reader spans the reconnect.
            var reader = session.Events;
            var first = await ReadEventAsync(reader, TimeSpan.FromSeconds(10));
            Assert.Equal("turn_completed", first.EventType);
            Assert.Equal(100ul, first.Envelope.Seq);

            // A query parked on connection 0 (its script never answers after
            // the handshake), then the socket is dropped: the deterministic
            // mid-query loss — exactly one reconnect-retry follows.
            var drill = session.SnapshotAsync();
            dropFirst.TrySetResult();
            Assert.Equal(41ul, (await drill.WaitAsync(TimeSpan.FromSeconds(10))).Watermark);

            var second = await ReadEventAsync(reader, TimeSpan.FromSeconds(10));
            Assert.Equal("task_completed", second.EventType);
            Assert.Equal(101ul, second.Envelope.Seq);

            // Exactly the two live notifications were relayed — the old
            // connection's stream stopped at the switch, nothing leaked.
            Assert.False(reader.TryRead(out _));
            Assert.Single(losses);
            Assert.Equal(2, host.ConnectionsAccepted);
            Assert.True(session.IsConnected);
            Assert.False(session.EventsDropped);
        }
        finally
        {
            await session.DisposeAsync();
        }
    }
}

/// <summary>The queue's class-aware overflow policy, drilled directly.</summary>
public class BoundedEventQueuePolicyTests
{
    private static WorkEventNotification Notification(string eventType, ulong seq)
    {
        var envelope = new RuntimeEventEnvelope
        {
            RunId = "00000000-0000-4000-8000-000000000001",
            Seq = seq,
            TimestampMs = seq,
            Event = JsonSerializer.Deserialize<JsonElement>(eventType),
        };
        envelope.Validate();
        return new WorkEventNotification { Envelope = envelope };
    }

    private static WorkEventNotification Progress(ulong seq) => Notification("{\"type\":\"model_delta\"}", seq);

    private static WorkEventNotification Terminal(ulong seq) => Notification("{\"type\":\"task_completed\"}", seq);

    [Fact]
    public void Under_capacity_every_notification_is_delivered_in_order()
    {
        var queue = new BoundedEventQueue(4);
        for (ulong seq = 1; seq <= 4; seq++)
        {
            Assert.True(queue.TryEnqueue(Progress(seq)));
        }
        Assert.Equal(0, queue.DroppedCount);
        Assert.Equal(4, queue.Reader.Count);
        for (ulong seq = 1; seq <= 4; seq++)
        {
            Assert.True(queue.Reader.TryRead(out var notification));
            Assert.Equal(seq, notification.Envelope.Seq);
        }
    }

    [Fact]
    public void Full_queue_sheds_the_oldest_progress_for_a_newer_progress()
    {
        var queue = new BoundedEventQueue(4);
        for (ulong seq = 1; seq <= 4; seq++)
        {
            queue.TryEnqueue(Progress(seq));
        }
        Assert.True(queue.TryEnqueue(Progress(5)));
        Assert.Equal(1, queue.DroppedCount);
        Assert.Equal(4, queue.Reader.Count); // nothing left the queue but the shed seq 1
        Assert.True(queue.Reader.TryRead(out var first));
        Assert.Equal(2ul, first.Envelope.Seq); // seq 1 was shed, not seq 5
    }

    [Fact]
    public void Durable_arrival_evicts_the_oldest_progress_and_is_never_dropped()
    {
        var queue = new BoundedEventQueue(4);
        for (ulong seq = 1; seq <= 4; seq++)
        {
            queue.TryEnqueue(Progress(seq));
        }
        Assert.True(queue.TryEnqueue(Terminal(5)));
        Assert.Equal(1, queue.DroppedCount);
        var drained = new List<ulong>();
        while (queue.Reader.TryRead(out var notification))
        {
            drained.Add(notification.Envelope.Seq);
        }
        Assert.Equal(new ulong[] { 2, 3, 4, 5 }, drained); // oldest progress shed, terminal kept
    }

    [Fact]
    public void Progress_arrival_into_a_full_durable_queue_is_shed_not_the_durable_facts()
    {
        var queue = new BoundedEventQueue(4);
        for (ulong seq = 1; seq <= 4; seq++)
        {
            queue.TryEnqueue(Terminal(seq));
        }
        Assert.True(queue.TryEnqueue(Progress(9)));
        Assert.Equal(1, queue.DroppedCount);
        Assert.Equal(4, queue.Reader.Count);
        for (ulong seq = 1; seq <= 4; seq++)
        {
            Assert.True(queue.Reader.TryRead(out var notification));
            Assert.Equal(seq, notification.Envelope.Seq);
        }
    }

    [Fact]
    public void Durable_arrival_into_a_full_durable_queue_is_refused()
    {
        var queue = new BoundedEventQueue(4);
        for (ulong seq = 1; seq <= 4; seq++)
        {
            queue.TryEnqueue(Terminal(seq));
        }
        Assert.False(queue.TryEnqueue(Terminal(9)));
        Assert.Equal(4, queue.Reader.Count); // nothing was lost
    }

    [Fact]
    public async Task Completed_queue_ends_the_stream_after_draining_and_refuses_writes()
    {
        var queue = new BoundedEventQueue(4);
        queue.TryEnqueue(Terminal(1));
        Assert.True(queue.TryComplete());
        Assert.False(queue.TryEnqueue(Terminal(2)));
        Assert.True(queue.Reader.TryRead(out var drained));
        Assert.Equal(1ul, drained.Envelope.Seq);
        Assert.False(await queue.Reader.WaitToReadAsync(CancellationToken.None));
    }

    [Fact]
    public async Task Completion_error_surfaces_after_the_backlog_drains()
    {
        var queue = new BoundedEventQueue(4);
        queue.TryEnqueue(Terminal(1));
        var reason = new AgentContractViolationException("drill", "terminal overflow");
        queue.TryComplete(reason);
        Assert.True(queue.Reader.TryRead(out _));
        var failure = await Assert.ThrowsAsync<AgentContractViolationException>(
            () => queue.Reader.WaitToReadAsync(CancellationToken.None).AsTask());
        Assert.Same(reason, failure);
    }
}
