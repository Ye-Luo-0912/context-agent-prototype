using System.Net;
using System.Net.Sockets;
using System.Text.Json;

using FocusAgent.Desktop.Infrastructure;
using FocusAgent.Desktop.ViewModels;

using Xunit;

namespace FocusAgent.Client.Tests;

/// <summary>
/// N4 completion drill: the desktop workbench consumes the REAL typed event
/// stream and REAL typed responses end to end — over a ScriptedEventHost
/// (the same style as <see cref="EventStreamTests"/>) driving a live
/// <see cref="AgentConnection"/>. The chain: typed submit receipt → a typed
/// tool event notification → an approval row carrying the gate's risk and
/// bounded target summary → an Allow answered with the typed Delivered
/// receipt → the terminal event and the refreshed snapshot disagreeing
/// nowhere. Everything asserted is typed protocol fact, never parsed prose.
/// </summary>
public class WorkbenchIntegrationTests
{
    private const string RunId = "00000000-0000-4000-8000-000000000001";
    private const string TaskId = "00000000-0000-4000-8000-000000000022";
    private const string ApprovalRequestId = "req-n4-1";

    /// <summary>A loopback host whose accepted connections are each driven by
    /// a route-dispatching script (answers handshakes, interleaves typed
    /// notifications, flips the approval state when the answer arrives).</summary>
    private sealed class ScriptedEventHost : IAsyncDisposable
    {
        private readonly TcpListener _listener = new(IPAddress.Loopback, 0);
        private readonly CancellationTokenSource _stopped = new();
        private readonly Task _serveLoop;

        public ScriptedEventHost()
        {
            _listener.Start();
            _serveLoop = Task.Run(() => ServeAsync(_stopped.Token));
        }

        public int Port => ((IPEndPoint)_listener.LocalEndpoint).Port;

        public Func<int, Stream, CancellationToken, Task> Script { get; set; } =
            static (_, _, _) => Task.CompletedTask;

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
            catch
            {
                // listener stopped: the drill is over
            }
        }

        private async Task ServeClientAsync(TcpClient client, CancellationToken cancellationToken)
        {
            using var owned = client;
            var stream = client.GetStream();
            try
            {
                await Script(0, stream, cancellationToken).ConfigureAwait(false);
            }
            catch
            {
                // A dropped drill socket lands here; the client-side asserts
                // stay authoritative.
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

    private static byte[] NotificationFrame(ulong seq, string eventJson)
    {
        var messageId = Guid.NewGuid().ToString("D");
        return JsonSerializer.SerializeToUtf8Bytes(new Dictionary<string, object?>
        {
            ["protocol"] = ProtocolElement(),
            ["message_id"] = messageId,
            ["kind"] = "notification",
            ["route"] = new { @namespace = "work", operation = "event" },
            // A root message correlates to its own message id — the same
            // rule the shared EventStreamTests frames follow.
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
        }, AgentJson.Options);
    }

    private static byte[] ResponseFrame(JsonElement request, object payload) =>
        JsonSerializer.SerializeToUtf8Bytes(new
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

    private static async Task<Stream> ConnectAsync(int port)
    {
        var client = new TcpClient();
        await client.ConnectAsync(IPAddress.Loopback, port);
        return client.GetStream();
    }

    private static async Task WaitUntilAsync(Func<bool> condition, string what, Func<string>? diagnostics = null)
    {
        var deadline = DateTimeOffset.UtcNow + TimeSpan.FromSeconds(10);
        while (!condition())
        {
            Assert.True(DateTimeOffset.UtcNow < deadline,
                $"timed out waiting for {what}. Diagnostics:{Environment.NewLine}{diagnostics?.Invoke() ?? string.Empty}");
            await Task.Delay(20);
        }
    }

    private static object SnapshotPayload(bool approved) => new
    {
        run_started = true,
        run_completed = approved,
        watermark = approved ? 42ul : 40ul,
        run_id = "00000000-0000-4000-8000-000000000031",
        workspace_root = "/workspaces/test",
        focus = (object?)null,
        tasks = Array.Empty<object>(),
        pending_approvals = approved
            ? Array.Empty<object>()
            : new object[]
            {
                new
                {
                    request_id = ApprovalRequestId,
                    call_name = "shell.exec",
                    risk = "process_execution",
                    target_summary = "cargo test --workspace",
                },
            },
        resync_required = false,
    };

    [Fact]
    public async Task Submit_event_approval_delivered_terminal_chain_is_fully_typed()
    {
        var approved = false;
        await using var host = new ScriptedEventHost();
        host.Script = async (_, stream, cancellationToken) =>
        {
            while (!cancellationToken.IsCancellationRequested)
            {
                var frame = await FrameCodec.ReadFrameAsync(
                    stream, FrameCodec.DefaultMaxFrameBytes, cancellationToken);
                if (frame is null)
                {
                    return; // the drill connection closed
                }
                using var document = JsonDocument.Parse(frame);
                var operation = document.RootElement.GetProperty("route").GetProperty("operation").GetString();
                switch (operation)
                {
                    case "submit":
                        await FrameCodec.WriteFrameAsync(
                            stream,
                            ResponseFrame(
                                document.RootElement,
                                new { status = "success", value = new { disposition = "accepted", task_id = TaskId } }),
                            FrameCodec.DefaultMaxFrameBytes,
                            cancellationToken);
                        // A typed tool event rides the stream after the receipt.
                        await FrameCodec.WriteFrameAsync(
                            stream,
                            NotificationFrame(5ul, "{\"type\":\"tool_started\",\"name\":\"shell.exec\"}"),
                            FrameCodec.DefaultMaxFrameBytes,
                            cancellationToken);
                        break;

                    case "snapshot":
                        await FrameCodec.WriteFrameAsync(
                            stream,
                            ResponseFrame(document.RootElement, new { status = "success", value = SnapshotPayload(approved) }),
                            FrameCodec.DefaultMaxFrameBytes,
                            cancellationToken);
                        break;

                    case "respond":
                        approved = true;
                        await FrameCodec.WriteFrameAsync(
                            stream,
                            ResponseFrame(
                                document.RootElement,
                                new { status = "success", value = new { outcome = "delivered" } }),
                            FrameCodec.DefaultMaxFrameBytes,
                            cancellationToken);
                        // The terminal fact follows the decision on the stream.
                        await FrameCodec.WriteFrameAsync(
                            stream,
                            NotificationFrame(41ul, "{\"type\":\"task_completed\"}"),
                            FrameCodec.DefaultMaxFrameBytes,
                            cancellationToken);
                        break;

                    default:
                        await FrameCodec.WriteFrameAsync(
                            stream,
                            ResponseFrame(
                                document.RootElement,
                                new { status = "success", value = new { watermark = 40ul, resync_required = false } }),
                            FrameCodec.DefaultMaxFrameBytes,
                            cancellationToken);
                        break;
                }
            }
        };

        await using var connection = new AgentConnection(await ConnectAsync(host.Port));
        var viewModel = new MainWindowViewModel(new InlineUiDispatcher());
        try
        {
            await viewModel.ConnectForTestsAsync(connection);

            // 1. Typed submit receipt.
            viewModel.GoalInput = "n4 集成链路：类型化提交到事件审批终态";
            await viewModel.SubmitForTestsAsync();
            Assert.Contains("已受理", viewModel.LogText);
            Assert.Contains(TaskId, viewModel.LogText);

            // 2. The typed tool event reached the pump and was rendered.
            await WaitUntilAsync(
                () => viewModel.LogText.Contains("tool_started"),
                "typed tool event line",
                diagnostics: () => $"IsConnected={connection.IsConnected}{Environment.NewLine}output=<<{viewModel.OutputText}>>log=<<{viewModel.LogText}>>");

            // 3. The approval row carries the gate's typed risk and the
            // bounded target summary — informed approval, not a bare id.
            await WaitUntilAsync(() => viewModel.Approvals.Count == 1, "approval row");
            var row = viewModel.Approvals[0];
            Assert.Equal(ApprovalRequestId, row.RequestId);
            Assert.Equal("shell.exec · " + ApprovalRequestId, row.Title);
            Assert.Contains("进程执行", row.RiskLine);
            Assert.Contains("cargo test --workspace", row.TargetLine);

            // 4. The operator answers; the receipt is the typed Delivered
            // outcome and the honest fact is rendered.
            await viewModel.RespondApprovalForTestsAsync(ApprovalRequestId, ApprovalDecision.Allow);
            Assert.Contains($"审批 {ApprovalRequestId} 已送达：Allow", viewModel.LogText);

            // 5. The follow-up refresh (driven by the respond path AND the
            // terminal event) applies the server's post-decision snapshot:
            // the answered approval is gone and run_completed is rendered.
            await WaitUntilAsync(() => viewModel.Approvals.Count == 0, "approval removed after the real receipt");
            await WaitUntilAsync(
                () => viewModel.RunStateText.Contains("已完成"),
                "run_completed rendered from the snapshot");

            // 6. The terminal event was rendered as a fact (never as state).
            await WaitUntilAsync(
                () => viewModel.LogText.Contains("任务到达终态"),
                "terminal event line");

            // The answered approval's commands were released with the row.
            Assert.Equal(11, viewModel.RegisteredCommandCount);
            Assert.Equal(42ul, viewModel.Watermark);
        }
        finally
        {
            await viewModel.DisposeAsync();
        }
    }
}
