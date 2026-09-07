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
        }

        public sealed class ConnectionPlan
        {
            public SubmitAnswer Submit { get; init; } = SubmitAnswer.Accept;

            /// <summary>The handshake snapshot is answered; the SECOND
            /// snapshot frame on this connection (the operation query) is read
            /// and counted, then the socket is dropped: the mid-query loss
            /// drill for the single reconnect-retry.</summary>
            public bool DropOnSecondSnapshot { get; init; }
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
                        await WriteResponseAsync(stream, root, new
                        {
                            status = "success",
                            value = new
                            {
                                run_started = true,
                                run_completed = false,
                                watermark = 41ul,
                                tasks = Array.Empty<object>(),
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
                        await WriteResponseAsync(stream, root, new
                        {
                            status = "success",
                            value = new { disposition = "accepted", task_id = SampleTaskId },
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
}
