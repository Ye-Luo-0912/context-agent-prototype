using System.Buffers.Binary;
using System.Net;
using System.Net.Sockets;
using System.Text;
using System.Text.Json;

using Xunit;

namespace FocusAgent.Client.Tests;

/// <summary>Framing edge cases: half frames, coalesced frames, oversize, EOF.</summary>
public class FramingTests
{
    private static (MemoryStream ClientSide, MemoryStream ServerSide) DuplexPair()
    {
        // One bidirectional pipe: what "client" writes, "server" reads.
        var stream = new MemoryStream();
        return (stream, stream);
    }

    [Fact]
    public async Task Frame_round_trips_exact_bytes()
    {
        using var stream = new MemoryStream();
        var payload = Encoding.UTF8.GetBytes("{\"hello\":\"world\"}");
        await FrameCodec.WriteFrameAsync(stream, payload, FrameCodec.DefaultMaxFrameBytes, CancellationToken.None);
        stream.Position = 0;
        var read = await FrameCodec.ReadFrameAsync(stream, FrameCodec.DefaultMaxFrameBytes, CancellationToken.None);
        Assert.Equal(payload, read);
    }

    [Fact]
    public async Task Coalesced_frames_are_read_in_sequence()
    {
        using var stream = new MemoryStream();
        await FrameCodec.WriteFrameAsync(stream, new byte[] { 1, 2, 3 }, FrameCodec.DefaultMaxFrameBytes, CancellationToken.None);
        await FrameCodec.WriteFrameAsync(stream, new byte[] { 4, 5 }, FrameCodec.DefaultMaxFrameBytes, CancellationToken.None);
        stream.Position = 0;
        var first = await FrameCodec.ReadFrameAsync(stream, FrameCodec.DefaultMaxFrameBytes, CancellationToken.None);
        var second = await FrameCodec.ReadFrameAsync(stream, FrameCodec.DefaultMaxFrameBytes, CancellationToken.None);
        Assert.Equal(new byte[] { 1, 2, 3 }, first);
        Assert.Equal(new byte[] { 4, 5 }, second);
        Assert.Null(await FrameCodec.ReadFrameAsync(stream, FrameCodec.DefaultMaxFrameBytes, CancellationToken.None));
    }

    [Fact]
    public async Task Oversize_frame_is_rejected_not_truncated()
    {
        var stream = new MemoryStream();
        var header = new byte[4];
        BinaryPrimitives.WriteUInt32LittleEndian(header, (uint)FrameCodec.DefaultMaxFrameBytes + 1);
        await stream.WriteAsync(header);
        stream.Position = 0;
        await Assert.ThrowsAsync<AgentContractViolationException>(() =>
            FrameCodec.ReadFrameAsync(stream, FrameCodec.DefaultMaxFrameBytes, CancellationToken.None));
    }

    [Fact]
    public async Task Zero_length_frame_is_rejected()
    {
        var stream = new MemoryStream(new byte[4]);
        stream.Position = 0;
        await Assert.ThrowsAsync<AgentContractViolationException>(() =>
            FrameCodec.ReadFrameAsync(stream, FrameCodec.DefaultMaxFrameBytes, CancellationToken.None));
    }

    [Fact]
    public async Task Half_frame_at_eof_is_a_contract_violation()
    {
        var stream = new MemoryStream(new byte[] { 0, 0, 0, 9, 1, 2 });
        stream.Position = 0;
        await Assert.ThrowsAsync<AgentContractViolationException>(() =>
            FrameCodec.ReadFrameAsync(stream, FrameCodec.DefaultMaxFrameBytes, CancellationToken.None));
    }

    [Fact]
    public async Task Write_above_the_cap_is_rejected_before_any_io()
    {
        using var stream = new MemoryStream();
        await Assert.ThrowsAsync<AgentContractViolationException>(() =>
            FrameCodec.WriteFrameAsync(
                stream, new byte[FrameCodec.DefaultMaxFrameBytes + 1], FrameCodec.DefaultMaxFrameBytes, CancellationToken.None));
        Assert.Equal(0, stream.Length);
    }
}

/// <summary>
/// Full-connection behaviour over a real TCP loopback: the scripted server
/// speaks the same framing and the shared fixtures' wire shapes.
/// </summary>
public class ConnectionTests
{
    private sealed record ScriptedServer(TcpClient Client)
    {
        public async Task ServeAsync(int turns, CancellationToken cancellationToken)
        {
            var stream = Client.GetStream();
            for (var i = 0; i < turns; i++)
            {
                var request = await FrameCodec.ReadFrameAsync(stream, FrameCodec.DefaultMaxFrameBytes, cancellationToken)
                    ?? throw new InvalidOperationException("client closed early");
                using var document = JsonDocument.Parse(request);
                var response = BuildResponse(document.RootElement);
                await FrameCodec.WriteFrameAsync(stream, response, FrameCodec.DefaultMaxFrameBytes, cancellationToken);
            }
        }

        private static byte[] BuildResponse(JsonElement request)
        {
            var messageId = Guid.NewGuid().ToString("D");
            var payloadKind = request.GetProperty("route").GetProperty("operation").GetString();
            object payload = payloadKind switch
            {
                "submit" => new
                {
                    status = "success",
                    value = new { disposition = "accepted", task_id = "00000000-0000-4000-8000-000000000022" },
                },
                "snapshot" => new
                {
                    status = "success",
                    value = new
                    {
                        run_started = true,
                        run_completed = false,
                        watermark = 41ul,
                        focus = (object?)null,
                        tasks = Array.Empty<object>(),
                        pending_approvals = Array.Empty<object>(),
                        resync_required = false,
                    },
                },
                _ => new { status = "error", error = new { @class = "protocol", code = "route.unsupported", message = "not served by the test", retry = "never", effect_state = "not_applicable" } },
            };

            var response = new
            {
                protocol = request.GetProperty("protocol"),
                message_id = messageId,
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
            return JsonSerializer.SerializeToUtf8Bytes(response, AgentJson.Options);
        }
    }

    private static string FixturePath(string name) => ContractFixtures.FilePath(name);

    [Fact]
    public async Task Interleaved_requests_are_correlated_by_request_id()
    {
        using var listener = new TcpListener(IPAddress.Loopback, 0);
        listener.Start();
        var port = ((IPEndPoint)listener.LocalEndpoint).Port;

        using var clientA = new TcpClient();
        using var clientB = new TcpClient();
        var connectA = clientA.ConnectAsync(IPAddress.Loopback, port);
        var connectB = clientB.ConnectAsync(IPAddress.Loopback, port);
        await Task.WhenAll(connectA, connectB);

        var serverA = new ScriptedServer(await listener.AcceptTcpClientAsync(CancellationToken.None));
        var serverB = new ScriptedServer(await listener.AcceptTcpClientAsync(CancellationToken.None));
        _ = serverA.ServeAsync(1, CancellationToken.None);
        _ = serverB.ServeAsync(1, CancellationToken.None);

        await using var connectionA = new AgentConnection(clientA.GetStream());
        await using var connectionB = new AgentConnection(clientB.GetStream());

        var snapshotA = connectionA.SnapshotAsync();
        var snapshotB = connectionB.SnapshotAsync();
        var results = await Task.WhenAll(snapshotA, snapshotB);
        Assert.All(results, snapshot => Assert.Equal(41ul, snapshot.Watermark));
    }

    [Fact]
    public async Task Structured_error_responses_become_protocol_exceptions()
    {
        using var listener = new TcpListener(IPAddress.Loopback, 0);
        listener.Start();
        var port = ((IPEndPoint)listener.LocalEndpoint).Port;
        using var client = new TcpClient();
        var connect = client.ConnectAsync(IPAddress.Loopback, port);
        var server = new ScriptedServer(await listener.AcceptTcpClientAsync(CancellationToken.None));
        await connect;
        _ = server.ServeAsync(1, CancellationToken.None);

        await using var connection = new AgentConnection(client.GetStream());
        // The scripted server serves only submit/snapshot; subscribe gets the
        // structured unsupported error, which must surface as a typed
        // exception rather than a fake success.
        var failure = await Assert.ThrowsAsync<AgentProtocolException>(
            () => connection.SubscribeAsync());
        Assert.Equal("route.unsupported", failure.Error.Code);
    }

    [Fact]
    public async Task Cancelled_request_wait_abandons_only_the_wait()
    {
        // The server never answers; the wait must time out while the
        // connection stays usable for dispose, and the server-side work is
        // untouched (there is none in this drill).
        using var listener = new TcpListener(IPAddress.Loopback, 0);
        listener.Start();
        var port = ((IPEndPoint)listener.LocalEndpoint).Port;
        using var client = new TcpClient();
        var connect = client.ConnectAsync(IPAddress.Loopback, port);
        await listener.AcceptTcpClientAsync(CancellationToken.None);
        await connect;

        await using var connection = new AgentConnection(client.GetStream());
        await Assert.ThrowsAnyAsync<OperationCanceledException>(
            () => connection.SnapshotAsync(new CancellationToken(canceled: true)));
        // The abandoned request never answered; the connection itself is
        // still alive and its read loop keeps serving further requests.
        Assert.True(connection.IsConnected);
    }
}
