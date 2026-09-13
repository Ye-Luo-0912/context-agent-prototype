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
        run_id = "00000000-0000-4000-8000-000000000031",
        workspace_root = "/workspaces/test",
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

    /// <summary>Answers the resumable handshake (subscribe first, then
    /// snapshot — the B1 order the session issues them in) on one scripted
    /// connection.</summary>
    private static async Task AnswerHandshakeAsync(Stream stream, CancellationToken cancellationToken)
    {
        await AnswerOneRequestAsync(stream, new { watermark = 41ul, resync_required = false }, cancellationToken);
        await AnswerOneRequestAsync(stream, SnapshotPayload, cancellationToken);
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

    /// <summary>B1 SNAP-GAP: the connect handshake subscribes BEFORE it
    /// snapshots, and the snapshot's watermark dedups the stream — a durable
    /// event at or below it never reaches the session stream twice (it is
    /// already reflected in the snapshot), while live-only progress at the
    /// same cursor relays (no snapshot ever carried it).</summary>
    [Fact]
    public async Task Connect_subscribes_before_snapshot_and_dedupes_the_stream_at_the_snapshot_watermark()
    {
        await using var host = new ScriptedEventHost();
        var handshakeOrder = new List<string>();
        host.Script = async (_, stream, cancellationToken) =>
        {
            // First frame: the subscribe must be issued before the snapshot.
            var first = await FrameCodec.ReadFrameAsync(
                stream, FrameCodec.DefaultMaxFrameBytes, cancellationToken)
                ?? throw new InvalidOperationException("client closed mid-handshake");
            using (var document = JsonDocument.Parse(first))
            {
                var operation = document.RootElement.GetProperty("route").GetProperty("operation").GetString();
                handshakeOrder.Add(operation!);
                Assert.Equal("subscribe", operation);
                await WriteFrameAsync(
                    stream,
                    ResponseFrame(document.RootElement, new { status = "success", value = new { watermark = 41ul, resync_required = false } }),
                    cancellationToken);
            }

            // Second frame: the snapshot, taken at watermark 41.
            var second = await FrameCodec.ReadFrameAsync(
                stream, FrameCodec.DefaultMaxFrameBytes, cancellationToken)
                ?? throw new InvalidOperationException("client closed mid-handshake");
            using (var document = JsonDocument.Parse(second))
            {
                var operation = document.RootElement.GetProperty("route").GetProperty("operation").GetString();
                handshakeOrder.Add(operation!);
                Assert.Equal("snapshot", operation);
                await WriteFrameAsync(
                    stream,
                    ResponseFrame(document.RootElement, new { status = "success", value = SnapshotPayload }),
                    cancellationToken);
            }

            // Stream traffic at and around the snapshot cut: the durable
            // fact at the cut is IN the snapshot (must not relay), the
            // live-only delta repeating the cut's cursor is not (must
            // relay), and the durable fact above the cut is stream-only
            // (must relay).
            await WriteFrameAsync(stream, NotificationFrame(41ul, "{\"type\":\"task_completed\"}"), cancellationToken);
            await WriteFrameAsync(stream, NotificationFrame(41ul, "{\"type\":\"model_delta\"}"), cancellationToken);
            await WriteFrameAsync(stream, NotificationFrame(42ul, "{\"type\":\"run_started\"}"), cancellationToken);
            // The query that installed this connection re-issues its own
            // snapshot on it — answer exactly that one, then park.
            await AnswerOneRequestAsync(stream, SnapshotPayload, cancellationToken);
            await Task.Delay(Timeout.Infinite, cancellationToken);
        };

        var session = new ResumableSession(() => ConnectAsync(host.Port));
        try
        {
            var snapshot = await session.SnapshotAsync().WaitAsync(TimeSpan.FromSeconds(10));
            Assert.Equal(41ul, snapshot.Watermark);
            Assert.Equal(new[] { "subscribe", "snapshot" }, handshakeOrder);

            var reader = session.Events;
            var delta = await ReadEventAsync(reader, TimeSpan.FromSeconds(10));
            Assert.Equal("model_delta", delta.EventType);
            Assert.Equal(41ul, delta.Envelope.Seq);
            Assert.True(delta.IsLiveOnlyProgress);

            var above = await ReadEventAsync(reader, TimeSpan.FromSeconds(10));
            Assert.Equal("run_started", above.EventType);
            Assert.Equal(42ul, above.Envelope.Seq);

            // The durable fact at the cut was dropped in the pump, and
            // nothing else leaked: exactly the two expected notifications.
            Assert.False(reader.TryRead(out _));
        }
        finally
        {
            await session.DisposeAsync();
        }
    }

    /// <summary>B1 reset boundary: installing the reconnecting connection
    /// clears the session stream's unread backlog — the fresh snapshot
    /// rebuilds all durable state, so an old connection's unread durable
    /// fact never resurfaces after the new snapshot.</summary>
    [Fact]
    public async Task Reconnect_resets_the_stream_and_old_unread_events_do_not_survive()
    {
        await using var host = new ScriptedEventHost();
        var dropFirst = new TaskCompletionSource(TaskCreationOptions.RunContinuationsAsynchronously);
        host.Script = async (ordinal, stream, cancellationToken) =>
        {
            await AnswerHandshakeAsync(stream, cancellationToken);
            await WriteFrameAsync(
                stream,
                NotificationFrame(
                    100ul + (ulong)ordinal,
                    ordinal == 0 ? "{\"type\":\"turn_completed\"}" : "{\"type\":\"task_completed\"}"),
                cancellationToken);
            if (ordinal == 0)
            {
                // Answer the drill query that installed this connection, then
                // park until the drill drops the socket.
                await AnswerOneRequestAsync(stream, SnapshotPayload, cancellationToken);
                await dropFirst.Task.WaitAsync(cancellationToken);
            }
            else
            {
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
            var first = await session.SnapshotAsync().WaitAsync(TimeSpan.FromSeconds(10));

            // The old connection's announcement reached the session stream,
            // but the drill deliberately does NOT read it.
            var reader = session.Events;
            Assert.True(
                await reader.WaitToReadAsync(new CancellationTokenSource(TimeSpan.FromSeconds(10)).Token),
                "the old connection's notification never reached the session stream");
            Assert.Equal(1, reader.Count);

            // Deterministic mid-stream loss, then the reconnect.
            dropFirst.TrySetResult();
            var second = await session.SnapshotAsync().WaitAsync(TimeSpan.FromSeconds(10));
            Assert.Equal(41ul, second.Watermark);

            // The reset dropped the unread backlog: the next stream fact is
            // the NEW connection's announcement, never the old one's.
            var next = await ReadEventAsync(reader, TimeSpan.FromSeconds(10));
            Assert.Equal("task_completed", next.EventType);
            Assert.Equal(101ul, next.Envelope.Seq);
            Assert.False(reader.TryRead(out _));
            Assert.Single(losses);
            Assert.Equal(2, host.ConnectionsAccepted);
            Assert.True(session.IsConnected);
        }
        finally
        {
            await session.DisposeAsync();
        }
    }

    /// <summary>R13: a session-level queue overflow completes the current event
    /// stream (a monotonic terminal generation), but the session itself is NOT
    /// dead. A successful reconnect REBUILDS a fresh live stream, so a consumer
    /// re-reading <see cref="ResumableSession.Events"/> after the resync
    /// receives new events again — a "connected but events never arrive again"
    /// half-recovered session must not be the outcome.</summary>
    [Fact]
    public async Task Overflowed_session_rebuilds_its_event_stream_on_reconnect_and_delivers_new_events()
    {
        await using var host = new ScriptedEventHost();
        var dropFirst = new TaskCompletionSource(TaskCreationOptions.RunContinuationsAsynchronously);
        host.Script = async (ordinal, stream, cancellationToken) =>
        {
            await AnswerHandshakeAsync(stream, cancellationToken);
            if (ordinal == 0)
            {
                // Overflow connection: three DURABLE notifications, each with a
                // gap so the connection's own (source) queue drains between
                // sends — ONLY the session-level queue (capacity 2) overflows.
                for (ulong i = 0; i < 3; i++)
                {
                    await WriteFrameAsync(
                        stream, NotificationFrame(100 + i, "{\"type\":\"task_completed\"}"), cancellationToken);
                    await Task.Delay(50, cancellationToken);
                }
                // Answer the query that installed this connection, then park
                // until the drill drops the socket to trigger the reconnect.
                await AnswerOneRequestAsync(stream, SnapshotPayload, cancellationToken);
                await dropFirst.Task.WaitAsync(cancellationToken);
            }
            else
            {
                // Reconnect connection: announce ONE new durable event, then
                // serve the reconnect query's snapshot.
                await WriteFrameAsync(stream, NotificationFrame(200, "{\"type\":\"run_started\"}"), cancellationToken);
                while (!cancellationToken.IsCancellationRequested)
                {
                    await AnswerOneRequestAsync(stream, SnapshotPayload, cancellationToken);
                }
            }
        };

        var session = new ResumableSession(
            () => ConnectAsync(host.Port),
            new AgentConnectionOptions { NotificationCapacity = 2 });
        try
        {
            var initial = await session.SnapshotAsync().WaitAsync(TimeSpan.FromSeconds(10));
            Assert.Equal(41ul, initial.Watermark);

            // The (now overflown) session stream is the pre-reconnect generation.
            var reader0 = session.Events;
            // Drain the two durable events that fit under the capacity-2 session
            // queue; the 3rd durable arrival overflows it, completing the stream
            // with the honest reason (observed once the backlog is drained).
            Assert.Equal(100ul, (await ReadEventAsync(reader0, TimeSpan.FromSeconds(10))).Envelope.Seq);
            Assert.Equal(101ul, (await ReadEventAsync(reader0, TimeSpan.FromSeconds(10))).Envelope.Seq);
            var overflow = await Assert.ThrowsAsync<AgentContractViolationException>(async () =>
            {
                var wait = reader0.WaitToReadAsync(CancellationToken.None).AsTask();
                await wait.WaitAsync(TimeSpan.FromSeconds(10));
            });
            Assert.StartsWith("invalid work.event.queue", overflow.Message);
            // Being able to refresh does not mean events are reachable: the
            // connection itself is still live, only the current stream is done.
            Assert.True(session.IsConnected);

            // The reconnect is driven by the next query once the dead
            // connection is dropped; wait for the loss to be observed, then
            // rebuild from the fresh snapshot (which REBUILDS the event stream).
            dropFirst.TrySetResult();
            var deadline = DateTimeOffset.UtcNow + TimeSpan.FromSeconds(10);
            while (session.IsConnected)
            {
                Assert.True(DateTimeOffset.UtcNow < deadline, "the dropped connection never faulted");
                await Task.Delay(20);
            }
            var rebuilt = await session.SnapshotAsync().WaitAsync(TimeSpan.FromSeconds(10));
            Assert.Equal(41ul, rebuilt.Watermark);
            Assert.Equal(2, host.ConnectionsAccepted);

            // A consumer re-reading Events sees the NEW generation — the
            // reconnect's event is delivered (the old, closed stream is not).
            var reader1 = session.Events;
            var fresh = await ReadEventAsync(reader1, TimeSpan.FromSeconds(10));
            Assert.Equal("run_started", fresh.EventType);
            Assert.Equal(200ul, fresh.Envelope.Seq);
            Assert.False(reader0.TryRead(out _)); // the old generation stays closed
            Assert.True(session.IsConnected);
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

    /// <summary>B1 reset boundary: Clear drops the whole backlog atomically
    /// and leaves the stream open — new arrivals flow and completion state
    /// is untouched.</summary>
    [Fact]
    public void Clear_drops_the_backlog_and_keeps_the_stream_open()
    {
        var queue = new BoundedEventQueue(4);
        queue.TryEnqueue(Terminal(1));
        queue.TryEnqueue(Progress(1));
        queue.TryEnqueue(Terminal(2));

        Assert.Equal(3, queue.Clear());
        Assert.Equal(0, queue.Reader.Count);

        // The stream stays open: a new arrival is readable and the dropped
        // backlog is gone for good. (Completion semantics are the B2
        // QUEUE-COMPLETION slice, deliberately not asserted here.)
        Assert.True(queue.TryEnqueue(Terminal(3)));
        Assert.True(queue.Reader.TryRead(out var notification));
        Assert.Equal(3ul, notification.Envelope.Seq);
        Assert.Equal(0, queue.DroppedCount);
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

    /// <summary>B2 QUEUE-COMPLETION: Reader.Completion is the channel
    /// contract — pending on a live queue (empty or not, drained or not, as
    /// long as writes are still possible), never already-complete.</summary>
    [Fact]
    public void Reader_completion_stays_pending_while_the_queue_is_live()
    {
        var queue = new BoundedEventQueue(4);
        Assert.False(queue.Reader.Completion.IsCompleted); // empty, writable
        queue.TryEnqueue(Terminal(1));
        Assert.False(queue.Reader.Completion.IsCompleted); // readable, writable
        Assert.True(queue.Reader.TryRead(out _));
        Assert.False(queue.Reader.Completion.IsCompleted); // drained, writable
    }

    /// <summary>B2 QUEUE-COMPLETION: closing the write side defers the
    /// completion task until the backlog is drained — it settles on the
    /// LAST read, not on the close.</summary>
    [Fact]
    public void Reader_completion_settles_on_the_last_read_after_close()
    {
        var queue = new BoundedEventQueue(4);
        queue.TryEnqueue(Terminal(1));
        queue.TryEnqueue(Terminal(2));
        Assert.True(queue.TryComplete());
        Assert.False(queue.Reader.Completion.IsCompleted); // backlog remains
        Assert.True(queue.Reader.TryRead(out _));
        Assert.False(queue.Reader.Completion.IsCompleted); // one item remains
        Assert.True(queue.Reader.TryRead(out _));
        Assert.True(queue.Reader.Completion.IsCompleted); // drained: settled
    }

    /// <summary>B2 QUEUE-COMPLETION: a close that finds the queue empty
    /// settles the completion task immediately — write side closed, no
    /// backlog, nothing further to read.</summary>
    [Fact]
    public void Reader_completion_settles_immediately_when_close_finds_an_empty_queue()
    {
        var queue = new BoundedEventQueue(4);
        Assert.True(queue.TryComplete());
        Assert.True(queue.Reader.Completion.IsCompleted);
    }

    /// <summary>B2 QUEUE-COMPLETION: the completion task faults with the
    /// queue's own error only after the backlog has been drained — the fault
    /// is observed by a Completion waiter, not swallowed.</summary>
    [Fact]
    public async Task Reader_completion_faults_with_the_reason_after_drain()
    {
        var queue = new BoundedEventQueue(4);
        queue.TryEnqueue(Terminal(1));
        var reason = new AgentContractViolationException("drill", "terminal overflow");
        queue.TryComplete(reason);
        Assert.False(queue.Reader.Completion.IsCompleted); // backlog first
        Assert.True(queue.Reader.TryRead(out _));
        var failure = await Assert.ThrowsAsync<AgentContractViolationException>(
            () => queue.Reader.Completion);
        Assert.Same(reason, failure);
    }

    /// <summary>B2 QUEUE-COMPLETION on the B1 reset boundary: Clear drops
    /// the backlog of a completed-but-undrained queue, which counts as
    /// drained — the deferred completion settles. On a live queue Clear
    /// leaves it pending (the stream stays open for the next connection).</summary>
    [Fact]
    public void Clear_settles_a_completed_queue_and_leaves_a_live_one_pending()
    {
        var completed = new BoundedEventQueue(4);
        completed.TryEnqueue(Terminal(1));
        Assert.True(completed.TryComplete());
        Assert.False(completed.Reader.Completion.IsCompleted); // backlog remains
        Assert.Equal(1, completed.Clear());
        Assert.True(completed.Reader.Completion.IsCompleted); // dropped = drained

        var live = new BoundedEventQueue(4);
        live.TryEnqueue(Terminal(1));
        Assert.Equal(1, live.Clear());
        Assert.False(live.Reader.Completion.IsCompleted); // still writable
    }
}
