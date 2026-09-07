using System.Net;
using System.Net.Sockets;
using System.Text.Json;

using Xunit;

namespace FocusAgent.Client.Tests;

/// <summary>
/// N2 client-safety drills: unknown mutation results are never re-sent
/// (F07), connection faults are terminal (F08), a session never opens a
/// second concurrent connection nor revives a disposed one (F09), and
/// payloads are validated on both wire edges (F19).
/// </summary>
public class ClientSafetyTests
{
    /// <summary>
    /// A countable scripted loopback host. Each accepted connection is driven
    /// by its per-ordinal plan; every request frame is counted by route
    /// operation, so tests can assert exactly how many mutation frames ever
    /// hit the wire.
    /// </summary>
    internal sealed class CountingLoopHost : IAsyncDisposable
    {
        public enum SubmitAnswer
        {
            Accept,
            /// <summary>The submit frame is read and counted, then the socket
            /// is dropped without an answer: the mutation-outcome drill.</summary>
            DropWithoutAnswer,
            /// <summary>Answered "accepted" but with a malformed task_id
            /// (not a canonical UUID): the response-validation drill.</summary>
            InvalidTaskId,
            /// <summary>Answered "accepted" with an empty task_id — what a
            /// response missing the field decodes to: the response-validation
            /// drill.</summary>
            MissingTaskId,
            /// <summary>Answered with a disposition value outside the
            /// contract enum: the response-decode drill.</summary>
            UnknownEnumValue,
        }

        public sealed class ConnectionPlan
        {
            public SubmitAnswer Submit { get; init; } = SubmitAnswer.Accept;

            /// <summary>The handshake snapshot is answered; the SECOND
            /// snapshot frame on this connection (the operation query) is read
            /// and counted, then the socket is dropped: the mid-query loss
            /// drill for the single reconnect-retry.</summary>
            public bool DropOnSecondSnapshot { get; init; }

            /// <summary>Snapshot answers carry this many task entries — set
            /// above the contract bound for the response-bound drill.</summary>
            public int SnapshotTaskCount { get; init; }
        }

        private const string SampleTaskId = "00000000-0000-4000-8000-000000000001";

        private readonly TcpListener _listener = new(IPAddress.Loopback, 0);
        private readonly CancellationTokenSource _stopped = new();
        private readonly Task _serveLoop;
        private int _connectionsAccepted;
        private readonly Dictionary<string, int> _frames = new();
        private readonly object _framesGate = new();

        public CountingLoopHost()
        {
            _listener.Start();
            _serveLoop = Task.Run(() => ServeAsync(_stopped.Token));
        }

        public Func<int, ConnectionPlan> PlanFor { get; set; } = _ => new ConnectionPlan();

        public int Port => ((IPEndPoint)_listener.LocalEndpoint).Port;

        public int ConnectionsAccepted => Volatile.Read(ref _connectionsAccepted);

        public int SubmitFrames => FrameCount("submit");

        public int SnapshotFrames => FrameCount("snapshot");

        private int FrameCount(string operation)
        {
            lock (_framesGate)
            {
                return _frames.TryGetValue(operation, out var count) ? count : 0;
            }
        }

        private void Count(string operation)
        {
            lock (_framesGate)
            {
                _frames[operation] = FrameCount(operation) + 1;
            }
        }

        private async Task ServeAsync(CancellationToken cancellationToken)
        {
            try
            {
                while (!cancellationToken.IsCancellationRequested)
                {
                    var client = await _listener.AcceptTcpClientAsync(cancellationToken);
                    _ = Task.Run(() => ServeClientAsync(client, cancellationToken), cancellationToken);
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

        private async Task ServeClientAsync(TcpClient client, CancellationToken cancellationToken)
        {
            var ordinal = Interlocked.Increment(ref _connectionsAccepted) - 1;
            var plan = PlanFor(ordinal);
            using var owned = client;
            var stream = client.GetStream();
            var snapshots = 0;
            try
            {
                while (!cancellationToken.IsCancellationRequested)
                {
                    var request = await FrameCodec.ReadFrameAsync(stream, FrameCodec.DefaultMaxFrameBytes, cancellationToken);
                    if (request is null)
                    {
                        return;
                    }
                    using var document = JsonDocument.Parse(request);
                    var root = document.RootElement.Clone();
                    var operation = root.GetProperty("route").GetProperty("operation").GetString() ?? string.Empty;
                    Count(operation);
                    if (operation == "snapshot")
                    {
                        snapshots++;
                        if (plan.DropOnSecondSnapshot && snapshots == 2)
                        {
                            // The frame was read and counted; the socket is
                            // dropped without an answer — the drill's
                            // mid-query connection loss.
                            return;
                        }
                        var tasks = Enumerable.Range(0, plan.SnapshotTaskCount)
                            .Select(i => new
                            {
                                task_id = $"00000000-0000-4000-8000-{i:D12}",
                                goal = $"drill task {i}",
                                status = "active",
                                anchor_revision = 1ul,
                                tool_requirement_revision = 0ul,
                                tool_requirement_count = 0u,
                            })
                            .ToArray();
                        await WriteResponseAsync(stream, root, new
                        {
                            status = "success",
                            value = new
                            {
                                run_started = true,
                                run_completed = false,
                                watermark = 41ul,
                                focus = (object?)null,
                                tasks,
                                pending_approvals = Array.Empty<object>(),
                                resync_required = false,
                            },
                        }, cancellationToken);
                        continue;
                    }
                    if (operation == "subscribe")
                    {
                        await WriteResponseAsync(stream, root, new
                        {
                            status = "success",
                            value = new { watermark = 41ul, resync_required = false },
                        }, cancellationToken);
                        continue;
                    }
                    if (operation == "submit")
                    {
                        if (plan.Submit == SubmitAnswer.DropWithoutAnswer)
                        {
                            // Read and counted, never answered.
                            return;
                        }
                        var submitValue = plan.Submit switch
                        {
                            SubmitAnswer.InvalidTaskId => (object)new
                            {
                                disposition = "accepted",
                                task_id = "not-a-canonical-uuid",
                            },
                            SubmitAnswer.MissingTaskId => new
                            {
                                disposition = "accepted",
                                task_id = string.Empty, // what a missing field decodes to
                            },
                            SubmitAnswer.UnknownEnumValue => new
                            {
                                disposition = "totally-bogus",
                                task_id = SampleTaskId,
                            },
                            _ => new
                            {
                                disposition = "accepted",
                                task_id = SampleTaskId,
                            },
                        };
                        await WriteResponseAsync(stream, root, new
                        {
                            status = "success",
                            value = submitValue,
                        }, cancellationToken);
                        continue;
                    }
                    // continue/cancel/approval.respond are not part of these drills.
                    return;
                }
            }
            catch (OperationCanceledException)
            {
            }
            catch (Exception)
            {
                // A dropped drill socket lands here; the frame counters stay
                // authoritative.
            }
        }

        private static async Task WriteResponseAsync(
            Stream stream, JsonElement request, object payload, CancellationToken cancellationToken)
        {
            var response = new
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
            };
            await FrameCodec.WriteFrameAsync(
                stream, JsonSerializer.SerializeToUtf8Bytes(response, AgentJson.Options),
                FrameCodec.DefaultMaxFrameBytes, cancellationToken);
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

    private static async Task<Stream> ConnectAsync(int port)
    {
        var client = new TcpClient();
        await client.ConnectAsync(IPAddress.Loopback, port);
        return client.GetStream();
    }

    private static async Task WaitUntilAsync(Func<bool> condition)
    {
        foreach (var _ in Enumerable.Range(0, 500))
        {
            if (condition())
            {
                return;
            }
            await Task.Delay(10);
        }
        Assert.True(condition(), "condition not met within the polling budget");
    }

    [Fact]
    public async Task Faulted_connection_leaves_submit_unknown_and_never_re_sends()
    {
        await using var host = new CountingLoopHost();
        // The first connection reads the submit frame and answers with
        // silence; any later connection answers normally.
        host.PlanFor = ordinal => ordinal == 0
            ? new CountingLoopHost.ConnectionPlan { Submit = CountingLoopHost.SubmitAnswer.DropWithoutAnswer }
            : new CountingLoopHost.ConnectionPlan();

        var session = new ResumableSession(() => ConnectAsync(host.Port));
        try
        {
            var unknown = await Assert.ThrowsAsync<AgentUnknownOutcomeException>(
                () => session.SubmitWorkAsync("n2: mutation must not be re-sent", ClientRequestIds.Next())
                    .WaitAsync(TimeSpan.FromSeconds(10)));
            Assert.IsType<AgentContractViolationException>(unknown.Failure);
            Assert.Equal(1, host.SubmitFrames); // exactly one submit frame ever hit the wire

            // A later operation reconnects and rebuilds from a snapshot; the
            // mutation stays sent-exactly-once.
            var snapshot = await session.SnapshotAsync().WaitAsync(TimeSpan.FromSeconds(10));
            Assert.Equal(41ul, snapshot.Watermark);
            Assert.Equal(1, host.SubmitFrames);
            Assert.Equal(2, host.ConnectionsAccepted); // the reconnect really happened
        }
        finally
        {
            await session.DisposeAsync();
            await host.DisposeAsync();
        }
    }

    [Fact]
    public async Task Query_still_reconnects_once_and_succeeds_after_a_fault()
    {
        await using var host = new CountingLoopHost();
        // The first connection loses the socket exactly while the operation
        // query is in flight; any later connection answers normally.
        host.PlanFor = ordinal => ordinal == 0
            ? new CountingLoopHost.ConnectionPlan { DropOnSecondSnapshot = true }
            : new CountingLoopHost.ConnectionPlan();

        var losses = new List<Exception>();
        var session = new ResumableSession(() => ConnectAsync(host.Port));
        session.ConnectionLost += failure => losses.Add(failure);
        try
        {
            var snapshot = await session.SnapshotAsync().WaitAsync(TimeSpan.FromSeconds(10));
            Assert.Equal(41ul, snapshot.Watermark);
            Assert.Equal(2, host.ConnectionsAccepted); // exactly one reconnect-retry
            Assert.Equal(0, host.SubmitFrames);        // a query drill never sends a mutation
            Assert.Single(losses);
        }
        finally
        {
            await session.DisposeAsync();
            await host.DisposeAsync();
        }
    }

    [Fact]
    public async Task Fault_fails_pending_waiters_and_refuses_new_requests_without_sending()
    {
        await using var host = new CountingLoopHost();
        host.PlanFor = _ => new CountingLoopHost.ConnectionPlan
        {
            Submit = CountingLoopHost.SubmitAnswer.DropWithoutAnswer,
        };

        await using var connection = new AgentConnection(await ConnectAsync(host.Port));

        // The submit is fully sent; the host reads the frame and drops the
        // socket without answering. The terminal fault completes the waiter
        // with the fault reason — it never hangs and never invents a reply.
        var failure = await Assert.ThrowsAsync<AgentContractViolationException>(
            () => connection.SubmitWorkAsync("n2: waiter drill", "req-waiter").WaitAsync(TimeSpan.FromSeconds(10)));
        Assert.Contains("host closed the stream", failure.Message, StringComparison.Ordinal);
        Assert.False(connection.IsConnected);

        // New requests are refused without a single further byte on the wire.
        await Assert.ThrowsAsync<AgentConnectionFaultedException>(
            () => connection.SubmitWorkAsync("n2: refused", "req-refused").WaitAsync(TimeSpan.FromSeconds(10)));
        Assert.Equal(1, host.SubmitFrames);
    }

    [Fact]
    public async Task Short_write_poisons_the_connection_and_fails_every_waiter()
    {
        var stream = new FaultInjectionStream();
        await using var connection = new AgentConnection(stream);

        // A holds the write lock inside its stalled payload write; B queues
        // behind it with its waiter already registered.
        var taskA = connection.SnapshotAsync();
        await stream.PayloadWriteStalled.WaitAsync(TimeSpan.FromSeconds(10));
        var taskB = connection.SnapshotAsync();
        await Task.Delay(100); // let B register and queue on the write lock

        // Half a frame lands on the "wire", then the write explodes: the
        // frame boundary is lost, so the connection must be poisoned.
        stream.ReleasePayloadWrite();

        await Assert.ThrowsAsync<IOException>(() => taskA.WaitAsync(TimeSpan.FromSeconds(10)));
        await Assert.ThrowsAnyAsync<Exception>(() => taskB.WaitAsync(TimeSpan.FromSeconds(10)));
        Assert.False(connection.IsConnected);

        // The poisoned connection writes no further bytes and refuses new work.
        var writesAtFault = stream.WriteCalls;
        var bytesAtFault = stream.BytesWritten;
        await Assert.ThrowsAsync<AgentConnectionFaultedException>(
            () => connection.SnapshotAsync().WaitAsync(TimeSpan.FromSeconds(10)));
        Assert.Equal(writesAtFault, stream.WriteCalls);
        Assert.Equal(bytesAtFault, stream.BytesWritten);
    }

    [Fact]
    public async Task Ten_concurrent_operations_share_one_connection()
    {
        await using var host = new CountingLoopHost(); // answers everything
        var connectCalls = 0;
        var session = new ResumableSession(() =>
        {
            Interlocked.Increment(ref connectCalls);
            return ConnectAsync(host.Port);
        });
        try
        {
            var tasks = Enumerable.Range(0, 10).Select(_ => session.SnapshotAsync()).ToArray();
            var snapshots = await Task.WhenAll(tasks).WaitAsync(TimeSpan.FromSeconds(15));
            Assert.All(snapshots, snapshot => Assert.Equal(41ul, snapshot.Watermark));
            Assert.Equal(1, Volatile.Read(ref connectCalls)); // single-flight: one attempt backs all ten callers
            Assert.Equal(1, host.ConnectionsAccepted);
            Assert.Equal(11, host.SnapshotFrames);            // one handshake + ten operations, one connection
        }
        finally
        {
            await session.DisposeAsync();
            await host.DisposeAsync();
        }
    }

    [Fact]
    public async Task Concurrent_callers_share_one_failed_connect_attempt()
    {
        var connectCalls = 0;
        var release = new TaskCompletionSource(TaskCreationOptions.RunContinuationsAsynchronously);
        var session = new ResumableSession(async () =>
        {
            Interlocked.Increment(ref connectCalls);
            await release.Task.WaitAsync(TimeSpan.FromSeconds(10));
            var client = new TcpClient();
            await client.ConnectAsync(IPAddress.Loopback, 1); // port 1: refused fast
            return client.GetStream();
        });
        try
        {
            // Mutations never retry a failed attempt (nothing was sent, and
            // the query-retry rule does not apply), so the attempt count is
            // deterministic: all ten callers await the SAME refused attempt.
            var tasks = Enumerable.Range(0, 10)
                .Select(_ => session.SubmitWorkAsync("n2: shared refused attempt", ClientRequestIds.Next()))
                .ToArray();
            await WaitUntilAsync(() => Volatile.Read(ref connectCalls) == 1);
            await Task.Delay(100); // let every caller pile onto the single in-flight attempt
            release.TrySetResult();

            await Task.WhenAll(tasks.Select(task => Assert.ThrowsAnyAsync<Exception>(() => task)))
                .WaitAsync(TimeSpan.FromSeconds(15));
            Assert.Equal(1, Volatile.Read(ref connectCalls));
        }
        finally
        {
            await session.DisposeAsync();
        }
    }

    [Fact]
    public async Task A_connect_arriving_after_dispose_is_discarded_not_installed()
    {
        await using var host = new CountingLoopHost(); // would answer the handshake
        var connectCalls = 0;
        var gate = new TaskCompletionSource(TaskCreationOptions.RunContinuationsAsynchronously);
        var session = new ResumableSession(async () =>
        {
            Interlocked.Increment(ref connectCalls);
            await gate.Task.WaitAsync(TimeSpan.FromSeconds(10)); // park the attempt mid-flight
            return await ConnectAsync(host.Port);
        });

        var operation = session.SnapshotAsync();
        await WaitUntilAsync(() => Volatile.Read(ref connectCalls) == 1);

        // Dispose while the connect is parked, THEN let the connect arrive:
        // the session is dead and must stay dead.
        await session.DisposeAsync();
        gate.TrySetResult();

        await Assert.ThrowsAsync<ObjectDisposedException>(() => operation.WaitAsync(TimeSpan.FromSeconds(10)));
        Assert.Equal(1, Volatile.Read(ref connectCalls)); // no revival: the factory never runs again
        Assert.False(session.IsConnected);
        await Assert.ThrowsAsync<ObjectDisposedException>(() => session.SnapshotAsync());
    }

    private static async Task AssertRejectedBeforeAnyWireBytes(Func<AgentConnection, Task> send)
    {
        var stream = new FaultInjectionStream();
        await using var connection = new AgentConnection(stream);

        await Assert.ThrowsAsync<AgentContractViolationException>(
            () => send(connection).WaitAsync(TimeSpan.FromSeconds(10)));
        Assert.Equal(0, stream.WriteCalls);   // rejected before a single byte hit the wire
        Assert.False(connection.IsConnected); // F19: validation failure is a protocol fault

        // The terminal path refuses everything afterwards, still byte-silent.
        await Assert.ThrowsAsync<AgentConnectionFaultedException>(
            () => connection.SnapshotAsync().WaitAsync(TimeSpan.FromSeconds(10)));
        Assert.Equal(0, stream.WriteCalls);
    }

    [Fact]
    public async Task Invalid_requests_are_rejected_before_any_wire_bytes()
    {
        // Missing identifiers, over-bound text, control characters, empty
        // ids: every bounded request validator rides the same pre-send path.
        await AssertRejectedBeforeAnyWireBytes(c => c.SubmitWorkAsync("", "req-1")); // missing goal
        await AssertRejectedBeforeAnyWireBytes(c => c.SubmitWorkAsync(new string('x', WorkSubmitRequest.MaxGoalChars + 1), "req-1"));
        await AssertRejectedBeforeAnyWireBytes(c => c.SubmitWorkAsync("a goal", "")); // missing client_request_id
        await AssertRejectedBeforeAnyWireBytes(c => c.SubmitWorkAsync("bad\ncontrol", "req-1"));
        await AssertRejectedBeforeAnyWireBytes(c => c.RespondApprovalAsync("", ApprovalDecision.Allow)); // missing request_id
    }

    [Fact]
    public async Task Invalid_response_payload_faults_the_connection_and_is_not_re_sent()
    {
        foreach (var submitAnswer in new[]
                 {
                     CountingLoopHost.SubmitAnswer.InvalidTaskId, // illegal UUID form
                     CountingLoopHost.SubmitAnswer.MissingTaskId, // missing task_id
                 })
        {
            await using var host = new CountingLoopHost();
            var firstConnection = true;
            host.PlanFor = _ => new CountingLoopHost.ConnectionPlan
            {
                Submit = Interlocked.Exchange(ref firstConnection, false)
                    ? submitAnswer
                    : CountingLoopHost.SubmitAnswer.Accept,
            };

            var session = new ResumableSession(() => ConnectAsync(host.Port));
            try
            {
                var unknown = await Assert.ThrowsAsync<AgentUnknownOutcomeException>(
                    () => session.SubmitWorkAsync("n2: response validation", ClientRequestIds.Next())
                        .WaitAsync(TimeSpan.FromSeconds(10)));
                Assert.IsType<AgentContractViolationException>(unknown.Failure);
                Assert.False(session.IsConnected);  // the fault is terminal on the receiving side too
                Assert.Equal(1, host.SubmitFrames); // sent exactly once; the client never re-sends

                // An explicit, later query operation reconnects and rebuilds;
                // the mutation stays sent-exactly-once.
                var snapshot = await session.SnapshotAsync().WaitAsync(TimeSpan.FromSeconds(10));
                Assert.Equal(41ul, snapshot.Watermark);
                Assert.Equal(1, host.SubmitFrames);
            }
            finally
            {
                await session.DisposeAsync();
                await host.DisposeAsync();
            }
        }
    }

    [Fact]
    public async Task Unknown_enum_value_in_a_response_faults_the_connection_and_is_not_re_sent()
    {
        await using var host = new CountingLoopHost();
        var firstConnection = true;
        host.PlanFor = _ => new CountingLoopHost.ConnectionPlan
        {
            Submit = Interlocked.Exchange(ref firstConnection, false)
                ? CountingLoopHost.SubmitAnswer.UnknownEnumValue
                : CountingLoopHost.SubmitAnswer.Accept,
        };

        var session = new ResumableSession(() => ConnectAsync(host.Port));
        try
        {
            var unknown = await Assert.ThrowsAsync<AgentUnknownOutcomeException>(
                () => session.SubmitWorkAsync("n2: enum drill", ClientRequestIds.Next())
                    .WaitAsync(TimeSpan.FromSeconds(10)));
            Assert.IsType<JsonException>(unknown.Failure);
            Assert.False(session.IsConnected);
            Assert.Equal(1, host.SubmitFrames); // decode failure: one frame, no re-send
        }
        finally
        {
            await session.DisposeAsync();
            await host.DisposeAsync();
        }
    }

    [Fact]
    public async Task Over_limit_snapshot_response_faults_the_connection_after_the_single_query_retry()
    {
        await using var host = new CountingLoopHost();
        host.PlanFor = _ => new CountingLoopHost.ConnectionPlan
        {
            SnapshotTaskCount = WorkSnapshotResponse.MaxTasks + 1, // 257 entries: above the bound
        };

        var losses = new List<Exception>();
        var session = new ResumableSession(() => ConnectAsync(host.Port));
        session.ConnectionLost += failure => losses.Add(failure);
        try
        {
            // Even the query path never accepts an over-bound snapshot: the
            // retry runs once (existing semantics), fails the same way, and
            // the honest contract violation surfaces.
            await Assert.ThrowsAsync<AgentContractViolationException>(
                () => session.SnapshotAsync().WaitAsync(TimeSpan.FromSeconds(10)));
            Assert.Equal(2, host.ConnectionsAccepted); // exactly one reconnect-retry, as before
            Assert.False(session.IsConnected);         // both attempts ended in the terminal fault
            Assert.Single(losses);                     // the loss is reported once; the retry failure surfaces as the exception
        }
        finally
        {
            await session.DisposeAsync();
            await host.DisposeAsync();
        }
    }
}

/// <summary>
/// A stream whose never-answered reads park until dispose and whose writes
/// are scripted: full header, then a stalled payload write that ends in a
/// short write plus IOException — the half-frame poison drill.
/// </summary>
internal sealed class FaultInjectionStream : Stream
{
    private readonly object _gate = new();
    private readonly TaskCompletionSource _payloadStallEntered = new(TaskCreationOptions.RunContinuationsAsynchronously);
    private readonly TaskCompletionSource _payloadStallRelease = new(TaskCreationOptions.RunContinuationsAsynchronously);
    private readonly TaskCompletionSource _readParking = new(TaskCreationOptions.RunContinuationsAsynchronously);
    private int _writeCalls;
    private int _bytesWritten;
    private bool _disposed;

    public int WriteCalls
    {
        get { lock (_gate) { return _writeCalls; } }
    }

    public int BytesWritten
    {
        get { lock (_gate) { return _bytesWritten; } }
    }

    /// <summary>Completes when the payload write has reached the stall point.</summary>
    public Task PayloadWriteStalled => _payloadStallEntered.Task;

    public void ReleasePayloadWrite() => _payloadStallRelease.TrySetResult();

    public override bool CanRead => true;
    public override bool CanSeek => false;
    public override bool CanWrite => true;

    public override async ValueTask WriteAsync(ReadOnlyMemory<byte> buffer, CancellationToken cancellationToken = default)
    {
        int call;
        lock (_gate)
        {
            if (_disposed)
            {
                throw new ObjectDisposedException(nameof(FaultInjectionStream));
            }
            call = ++_writeCalls;
        }
        if (call == 2)
        {
            _payloadStallEntered.TrySetResult();
            await _payloadStallRelease.Task.WaitAsync(cancellationToken).ConfigureAwait(false);
            lock (_gate)
            {
                _bytesWritten += buffer.Length / 2;
            }
            throw new IOException("injected short payload write");
        }
        if (call > 2)
        {
            throw new IOException("a poisoned connection must never write again");
        }
        lock (_gate)
        {
            _bytesWritten += buffer.Length;
        }
    }

    public override async ValueTask<int> ReadAsync(Memory<byte> buffer, CancellationToken cancellationToken = default)
    {
        // The fake host never answers: park until the stream is disposed.
        await _readParking.Task.WaitAsync(cancellationToken).ConfigureAwait(false);
        throw new IOException("fault-injection stream disposed");
    }

    public override ValueTask DisposeAsync()
    {
        lock (_gate)
        {
            _disposed = true;
        }
        _payloadStallEntered.TrySetResult();
        _payloadStallRelease.TrySetResult();
        _readParking.TrySetResult();
        return ValueTask.CompletedTask;
    }

    public override void Flush()
    {
    }

    public override int Read(byte[] buffer, int offset, int count) => throw new NotSupportedException();

    public override long Seek(long offset, SeekOrigin origin) => throw new NotSupportedException();

    public override void SetLength(long value) => throw new NotSupportedException();

    public override void Write(byte[] buffer, int offset, int count) => throw new NotSupportedException();

    public override long Length => throw new NotSupportedException();

    public override long Position
    {
        get => throw new NotSupportedException();
        set => throw new NotSupportedException();
    }
}
