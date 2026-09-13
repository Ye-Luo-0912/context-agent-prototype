using System.Net;
using System.Net.Sockets;
using System.Text.Json;

using FocusAgent.Desktop.Infrastructure;
using FocusAgent.Desktop.ViewModels;

using Xunit;

namespace FocusAgent.Client.Tests;

/// <summary>
/// C3 front-half drills: the desktop workbench across a connection loss and
/// a rebuild — the cold-restore path a real operator walks when the host
/// dies and comes back (with or without `--restore-latest`). Everything is
/// driven through the REAL <see cref="ResumableSession"/> wiring the
/// production connect path installs, over scripted hosts. Asserted facts
/// are typed protocol facts and the workbench's own honest-state output —
/// never parsed prose from tool output.
/// </summary>
public class RestoreWalkthroughTests
{
    private const string TaskIdFirst = "00000000-0000-4000-8000-000000000031";
    private const string TaskIdSecond = "00000000-0000-4000-8000-000000000032";

    /// <summary>A loopback host that serves each accepted connection with a
    /// caller script; the script sees its per-host connection index, so one
    /// host can play "the instance that dies mid-submit" and another "the
    /// restarted instance".</summary>
    private sealed class ScriptedHost : IAsyncDisposable
    {
        private readonly TcpListener _listener = new(IPAddress.Loopback, 0);
        private readonly CancellationTokenSource _stopped = new();
        private readonly Task _serveLoop;
        private int _connections;

        public ScriptedHost()
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
                    var index = Interlocked.Increment(ref _connections) - 1;
                    _ = Task.Run(() => ServeClientAsync(index, client, cancellationToken), cancellationToken);
                }
            }
            catch
            {
                // listener stopped: the drill is over
            }
        }

        private async Task ServeClientAsync(int index, TcpClient client, CancellationToken cancellationToken)
        {
            using var owned = client;
            var stream = client.GetStream();
            try
            {
                await Script(index, stream, cancellationToken).ConfigureAwait(false);
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

    private static object TaskEntry(string taskId, string goal) => new
    {
        task_id = taskId,
        goal,
        status = "active",
        anchor_revision = 3ul,
        tool_requirement_revision = 1ul,
        tool_requirement_count = 0u,
    };

    private static object SnapshotPayload(params object[] tasks) => new
    {
        run_started = true,
        run_completed = false,
        watermark = 7ul,
        run_id = "00000000-0000-4000-8000-000000000031",
        workspace_root = "/workspaces/test",
        focus = (object?)null,
        tasks,
        pending_approvals = Array.Empty<object>(),
        resync_required = false,
    };

    /// <summary>Answers the handshake routes and accepts submits under
    /// <paramref name="taskId"/>. <paramref name="onSubmit"/> observes every
    /// submit frame received (the no-auto-resend observation, with the
    /// frame's own client_request_id). <paramref name="submitResult"/>
    /// answers the GUI-3/F06 exact-request ledger query for a queried id —
    /// absent, the route answers like a ledger that cannot testify.</summary>
    private static Func<int, Stream, CancellationToken, Task> AnsweringScript(
        object snapshot,
        string taskId,
        Action<string>? onSubmit = null,
        Func<string, object?>? submitResult = null,
        string? runId = null)
    {
        var ledgerRunId = runId ?? "00000000-0000-4000-8000-0000000000b2";
        return async (_, stream, cancellationToken) =>
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
                    case "snapshot":
                        await FrameCodec.WriteFrameAsync(
                            stream,
                            ResponseFrame(document.RootElement, new { status = "success", value = snapshot }),
                            FrameCodec.DefaultMaxFrameBytes,
                            cancellationToken);
                        break;

                    case "subscribe":
                        await FrameCodec.WriteFrameAsync(
                            stream,
                            ResponseFrame(
                                document.RootElement,
                                new { status = "success", value = new { watermark = 7ul, resync_required = false } }),
                            FrameCodec.DefaultMaxFrameBytes,
                            cancellationToken);
                        break;

                    case "submit":
                        onSubmit?.Invoke(
                            document.RootElement.GetProperty("payload").GetProperty("client_request_id").GetString()!);
                        await FrameCodec.WriteFrameAsync(
                            stream,
                            ResponseFrame(
                                document.RootElement,
                                new { status = "success", value = new { disposition = "accepted", task_id = taskId } }),
                            FrameCodec.DefaultMaxFrameBytes,
                            cancellationToken);
                        break;

                    case "submit_result":
                        {
                            var queriedId = document.RootElement
                                .GetProperty("payload").GetProperty("client_request_id").GetString()!;
                            var answer = submitResult?.Invoke(queriedId) ?? new
                            {
                                run_id = ledgerRunId,
                                client_request_id = queriedId,
                                disposition = "unknown",
                            };
                            await FrameCodec.WriteFrameAsync(
                                stream,
                                ResponseFrame(document.RootElement, new { status = "success", value = answer }),
                                FrameCodec.DefaultMaxFrameBytes,
                                cancellationToken);
                            break;
                        }

                    default:
                        await FrameCodec.WriteFrameAsync(
                            stream,
                            ResponseFrame(document.RootElement, new { status = "success", value = string.Empty }),
                            FrameCodec.DefaultMaxFrameBytes,
                            cancellationToken);
                        break;
                }
            }
        };
    }

    /// <summary>The first-instance script: serves the handshake, then accepts
    /// the submit frame and drops the connection WITHOUT answering — the
    /// unknown outcome.</summary>
    private static Func<int, Stream, CancellationToken, Task> DyingInstanceScript()
    {
        return async (index, stream, cancellationToken) =>
        {
            if (index != 0)
            {
                return;
            }
            while (!cancellationToken.IsCancellationRequested)
            {
                var frame = await FrameCodec.ReadFrameAsync(
                    stream, FrameCodec.DefaultMaxFrameBytes, cancellationToken);
                if (frame is null)
                {
                    return;
                }
                using var document = JsonDocument.Parse(frame);
                var operation = document.RootElement.GetProperty("route").GetProperty("operation").GetString();
                if (operation == "submit")
                {
                    return; // drop the connection with the request unanswered
                }
                if (operation == "snapshot")
                {
                    await FrameCodec.WriteFrameAsync(
                        stream,
                        ResponseFrame(document.RootElement, new { status = "success", value = SnapshotPayload() }),
                        FrameCodec.DefaultMaxFrameBytes,
                        cancellationToken);
                }
                else if (operation == "subscribe")
                {
                    await FrameCodec.WriteFrameAsync(
                        stream,
                        ResponseFrame(
                            document.RootElement,
                            new { status = "success", value = new { watermark = 7ul, resync_required = false } }),
                        FrameCodec.DefaultMaxFrameBytes,
                        cancellationToken);
                }
            }
        };
    }

    private static async Task WaitUntilAsync(Func<bool> condition, string what)
    {
        var deadline = DateTimeOffset.UtcNow + TimeSpan.FromSeconds(10);
        while (!condition())
        {
            Assert.True(DateTimeOffset.UtcNow < deadline, $"timed out waiting for {what}");
            await Task.Delay(20);
        }
    }

    [Fact]
    public async Task Unknown_submit_is_resolved_by_the_ledger_receipt_without_any_replay()
    {
        const string goal = "c3 走查：丢回执后由受理账本核实（任务列表不参与判定）";
        var hostBSubmits = 0;
        var hostBLedgerQueries = 0;
        const string ledgerRunId = "00000000-0000-4000-8000-0000000000b2";

        // Host A: the instance that dies with the submit unanswered. Host B:
        // the ledger the original request actually reached (the connection
        // died, not the admission). Its snapshot does NOT carry the goal —
        // the task list is not the evidence, the ledger is; the GUI-3/F06
        // rule is that only the exact-request query can testify.
        await using var hostA = new ScriptedHost();
        hostA.Script = DyingInstanceScript();

        await using var hostB = new ScriptedHost();
        hostB.Script = AnsweringScript(
            SnapshotPayload(),
            TaskIdSecond,
            onSubmit: _ => Interlocked.Increment(ref hostBSubmits),
            submitResult: queriedId =>
            {
                Interlocked.Increment(ref hostBLedgerQueries);
                return new
                {
                    run_id = ledgerRunId,
                    client_request_id = queriedId,
                    disposition = "accepted",
                    task_id = TaskIdSecond,
                };
            });

        var connectionIndex = 0;
        var viewModel = new MainWindowViewModel(new InlineUiDispatcher());
        try
        {
            await viewModel.ConnectSessionForTestsAsync(async () =>
            {
                var index = connectionIndex++;
                return await ConnectAsync(index == 0 ? hostA.Port : hostB.Port);
            });

            // 1. The submit's outcome is UNKNOWN: the connection died with
            // the request unanswered. The desktop must not claim idempotent
            // replay and must not auto-resend.
            viewModel.GoalInput = goal;
            await viewModel.SubmitForTestsAsync();
            Assert.Contains("提交结果未知", viewModel.LogText);
            Assert.Contains("不会自动重发", viewModel.LogText);

            // 2. The ledger resolves the unknown by EXACT REQUEST identity —
            // queried with the id and the payload digest, never the goal
            // text; the snapshot's (empty) task list never mattered. The
            // rebuild ends the loss streak, and NO submit frame was replayed.
            await WaitUntilAsync(
                () => viewModel.OutstandingSubmitIdForTests is null,
                "the ledger receipt to release the outstanding key");
            Assert.True(hostBLedgerQueries >= 1, "the ledger was queried at least once");
            Assert.Equal(0, hostBSubmits);
            Assert.Contains("账本", viewModel.LogText);
            Assert.Contains(TaskIdSecond, viewModel.LogText);
            Assert.Equal(0, viewModel.ReconnectFailuresForTests);

            // 3. The unknown is closed: a DIFFERENT goal submits normally,
            // and exactly one submit frame reached the host — the original
            // request was never replayed behind the operator's back.
            viewModel.GoalInput = "c3 走查：未知解除后的新目标";
            await viewModel.SubmitForTestsAsync();
            Assert.Contains(TaskIdSecond, viewModel.LogText);
            Assert.Equal(1, hostBSubmits);
        }
        finally
        {
            await viewModel.DisposeAsync();
            await hostA.DisposeAsync();
            await hostB.DisposeAsync();
        }
    }

    [Fact]
    public async Task Unknown_submit_across_a_restart_stays_unknown_until_the_operator_resubmits()
    {
        const string goal = "c3 走查：跨重启无证据保持未知";
        var hostBSubmits = 0;
        var hostBSubmitIds = new System.Collections.Concurrent.ConcurrentQueue<string>();

        await using var hostA = new ScriptedHost();
        hostA.Script = DyingInstanceScript();

        // The restarted host is a FRESH ledger: it holds no evidence for the
        // id (the default submit_result answer is `unknown`), and its
        // snapshot does not carry the goal. Nothing — not the empty list,
        // not a name match — may turn that into a certainty.
        await using var hostB = new ScriptedHost();
        hostB.Script = AnsweringScript(
            SnapshotPayload(),
            TaskIdFirst,
            onSubmit: id =>
            {
                hostBSubmitIds.Enqueue(id);
                Interlocked.Increment(ref hostBSubmits);
            });

        var connectionIndex = 0;
        var viewModel = new MainWindowViewModel(new InlineUiDispatcher());
        try
        {
            await viewModel.ConnectSessionForTestsAsync(async () =>
            {
                var index = connectionIndex++;
                return await ConnectAsync(index == 0 ? hostA.Port : hostB.Port);
            });

            viewModel.GoalInput = goal;
            await viewModel.SubmitForTestsAsync();
            Assert.Contains("提交结果未知", viewModel.LogText);

            // The rebuilt host's ledger cannot testify: the unknown STAYS
            // unknown, the key stays outstanding (a same-goal retry remains
            // an idempotent re-entry on the SAME admission identity), and no
            // submit frame was sent automatically.
            await viewModel.RefreshOnceForTestsAsync();
            await WaitUntilAsync(
                () => viewModel.LogText.Contains("无法证明"),
                "the ledger's indeterminate verdict");
            Assert.DoesNotContain("解除", viewModel.LogText);
            var outstanding = viewModel.OutstandingSubmitIdForTests;
            Assert.NotNull(outstanding);
            Assert.Equal(0, hostBSubmits);

            // The operator resubmits the SAME goal: it re-enters on the SAME
            // client_request_id (one frame total), the fresh host admits it,
            // and the receipt — not a guess — closes the unknown.
            viewModel.GoalInput = goal;
            await viewModel.SubmitForTestsAsync();
            Assert.Equal(1, hostBSubmits);
            Assert.Equal(outstanding, Assert.Single(hostBSubmitIds));
            Assert.Null(viewModel.OutstandingSubmitIdForTests);
            Assert.Contains(TaskIdFirst, viewModel.LogText);
        }
        finally
        {
            await viewModel.DisposeAsync();
            await hostA.DisposeAsync();
            await hostB.DisposeAsync();
        }
    }

    [Fact]
    public async Task Repeated_connection_failures_are_counted_and_reset_by_a_rebuild()
    {
        await using var hostA = new ScriptedHost();
        hostA.Script = AnsweringScript(SnapshotPayload(), TaskIdFirst);

        ScriptedHost? hostB = null;
        var connectionIndex = 0;
        var viewModel = new MainWindowViewModel(new InlineUiDispatcher());
        try
        {
            await viewModel.ConnectSessionForTestsAsync(async () =>
            {
                var index = connectionIndex++;
                // The restarted instance only exists once the drill brings it
                // up; before that a reconnect fails like it does in the
                // field — nothing to connect to.
                var host = index == 0
                    ? hostA
                    : hostB ?? throw new IOException("the restarted instance is not up yet");
                return await ConnectAsync(host.Port);
            });

            Assert.Equal(0, viewModel.ReconnectFailuresForTests);

            // The first instance stops; nothing notifies a connected client
            // until the next operation. The next refresh observes the loss.
            await hostA.DisposeAsync();
            await viewModel.RefreshOnceForTestsAsync();
            Assert.True(viewModel.ReconnectFailuresForTests >= 1,
                $"expected the first failure to be observed, got {viewModel.ReconnectFailuresForTests}");

            // Every further failing refresh is one more observed failure —
            // the banner separates "transient drop" from "host stays gone".
            await viewModel.RefreshOnceForTestsAsync();
            await viewModel.RefreshOnceForTestsAsync();
            Assert.True(viewModel.ReconnectFailuresForTests >= 2,
                $"expected the loss streak to grow, got {viewModel.ReconnectFailuresForTests}");
            Assert.Contains("次连接失败", viewModel.BannerText);
            Assert.Contains("宿主可能已停止", viewModel.BannerText);

            // The restarted instance answers: the rebuild resets the streak
            // and states the honest rebuilt-from-snapshot banner.
            hostB = new ScriptedHost();
            hostB.Script = AnsweringScript(SnapshotPayload(TaskEntry(TaskIdFirst, "c3 走查：重建后的任务")), TaskIdFirst);
            await viewModel.RefreshOnceForTestsAsync();
            Assert.Equal(0, viewModel.ReconnectFailuresForTests);
            Assert.Equal("已从快照重建（重连）。挂起的审批以服务器快照为准，不会自动通过。", viewModel.BannerText);
        }
        finally
        {
            await viewModel.DisposeAsync();
            await hostA.DisposeAsync();
            if (hostB is not null)
            {
                await hostB.DisposeAsync();
            }
        }
    }
}
