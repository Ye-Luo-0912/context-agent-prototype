using System.Text.Json;

using Xunit;

namespace FocusAgent.Client.Tests;

/// <summary>
/// F5: the client-side contract for the long-flow control routes.
///
/// Three properties are pinned here, each because getting it wrong would let a
/// client believe something that did not happen:
///
/// * the receipts are internally consistent — an admitted correction names its
///   task, a refused one names its typed reason, and a cancel that matched
///   nothing may not carry a cancelled acknowledgement;
/// * the new expectation fields are additive on the wire, so a server that
///   answers an old-style request produces the historical bytes;
/// * a request the client itself refused is reported as never sent, not as an
///   unknown outcome.
/// </summary>
public class LongFlowControlContractTests
{
    private const string TaskA = "00000000-0000-4000-8000-000000000022";
    private const string TaskB = "00000000-0000-4000-8000-000000000044";

    /// <summary>An admitted correction must name the task it landed on; a
    /// refused one must name its reason and no task. A half-filled receipt is
    /// not a fact a workbench may render.</summary>
    [Fact]
    public void Steer_receipts_stay_internally_consistent()
    {
        var admittedWithoutTask = new WorkSteerResponse
        {
            Disposition = WorkSteerDisposition.Applied,
        };
        Assert.Equal(
            "work.steer.task_id",
            Assert.Throws<AgentContractViolationException>(admittedWithoutTask.Validate).Field);

        new WorkSteerResponse
        {
            Disposition = WorkSteerDisposition.Queued,
            TaskId = TaskA,
        }.Validate();

        var refusedWithoutReason = new WorkSteerResponse
        {
            Disposition = WorkSteerDisposition.Rejected,
            ActiveTaskId = TaskA,
        };
        Assert.Equal(
            "work.steer.rejection",
            Assert.Throws<AgentContractViolationException>(refusedWithoutReason.Validate).Field);

        var contradictory = new WorkSteerResponse
        {
            Disposition = WorkSteerDisposition.Rejected,
            Rejection = WorkSteerRejection.ExpectedTaskMismatch,
            TaskId = TaskA,
            ActiveTaskId = TaskB,
        };
        Assert.Equal(
            "work.steer.task_id",
            Assert.Throws<AgentContractViolationException>(contradictory.Validate).Field);

        // A queued correction is admitted but not executed; the accessor must
        // not blur that into "done".
        Assert.True(new WorkSteerResponse
        {
            Disposition = WorkSteerDisposition.Queued,
            TaskId = TaskA,
        }.IsAdmitted);
        Assert.False(new WorkSteerResponse
        {
            Disposition = WorkSteerDisposition.Rejected,
            Rejection = WorkSteerRejection.QueueFull,
        }.IsAdmitted);
    }

    /// <summary>A continuation receipt cannot claim both outcomes, and a
    /// suspension may only name a task it actually suspended.</summary>
    [Fact]
    public void Continue_and_suspend_receipts_never_claim_two_truths()
    {
        var continuedWithoutTask = new WorkContinueResponse();
        Assert.Equal(
            "work.continue.task_id",
            Assert.Throws<AgentContractViolationException>(continuedWithoutTask.Validate).Field);

        var rejectedNamingAContinuation = new WorkContinueResponse
        {
            TaskId = TaskA,
            Disposition = WorkContinueDisposition.ExpectedTaskMismatch,
            ActiveTaskId = TaskA,
        };
        Assert.Equal(
            "work.continue.task_id",
            Assert.Throws<AgentContractViolationException>(rejectedNamingAContinuation.Validate).Field);

        new WorkContinueResponse
        {
            Disposition = WorkContinueDisposition.ExpectedTaskMismatch,
            ActiveTaskId = TaskB,
        }.Validate();

        var suspendedWithoutTask = new WorkSuspendResponse
        {
            Disposition = WorkSuspendDisposition.Suspended,
        };
        Assert.Equal(
            "work.suspend.task_id",
            Assert.Throws<AgentContractViolationException>(suspendedWithoutTask.Validate).Field);

        var nothingHappenedButNamedATask = new WorkSuspendResponse
        {
            Disposition = WorkSuspendDisposition.NoActiveTask,
            TaskId = TaskA,
        };
        Assert.Equal(
            "work.suspend.task_id",
            Assert.Throws<AgentContractViolationException>(nothingHappenedButNamedATask.Validate).Field);
    }

    /// <summary>An unmatched cancel expectation cancelled nothing. A cancelled
    /// acknowledgement beside the mismatch would claim a stop that never
    /// happened for the turn the caller asked about.</summary>
    [Fact]
    public void An_unmatched_cancel_expectation_cannot_carry_a_cancelled_ack()
    {
        var lying = new WorkCancelResponse
        {
            Ack = new TurnCancelAck
            {
                Status = TurnCancelAckStatus.Cancelled,
                TurnId = "00000000-0000-4000-8000-000000000055",
                TaskId = TaskA,
                CancelledGeneration = 1,
                EffectiveGeneration = 2,
            },
            IdentityMismatch = new WorkTurnIdentity { TaskId = TaskA },
        };
        Assert.Equal(
            "work.cancel.identity_mismatch",
            Assert.Throws<AgentContractViolationException>(lying.Validate).Field);

        new WorkCancelResponse
        {
            Ack = new TurnCancelAck { Status = TurnCancelAckStatus.NoActiveTurn },
            IdentityMismatch = new WorkTurnIdentity { TaskId = TaskA },
        }.Validate();
    }

    /// <summary>The expectation fields are additive: a request that names none,
    /// and a continuation that happened, encode exactly the historical bytes —
    /// so a server's answer to an old-style client gains no new field.</summary>
    [Fact]
    public void Expectation_fields_do_not_change_the_historical_wire_bytes()
    {
        Assert.Equal("{}", JsonSerializer.Serialize(new WorkContinueRequest(), AgentJson.Options));
        Assert.Equal("{}", JsonSerializer.Serialize(new WorkCancelRequest(), AgentJson.Options));
        Assert.Equal(
            $"{{\"task_id\":\"{TaskA}\"}}",
            JsonSerializer.Serialize(
                new WorkContinueResponse { TaskId = TaskA }, AgentJson.Options));
        Assert.Equal(
            "{\"ack\":{\"status\":\"no_active_turn\"}}",
            JsonSerializer.Serialize(
                new WorkCancelResponse
                {
                    Ack = new TurnCancelAck { Status = TurnCancelAckStatus.NoActiveTurn },
                },
                AgentJson.Options));

        // A precise request carries exactly the fields it named.
        Assert.Equal(
            $"{{\"expected_task_id\":\"{TaskA}\"}}",
            JsonSerializer.Serialize(
                new WorkContinueRequest { ExpectedTaskId = TaskA }, AgentJson.Options));
    }

    /// <summary>A checkpoint artifact is a store NAME, not a path: the client
    /// refuses a traversal attempt itself, before any frame is composed.</summary>
    [Fact]
    public void Checkpoint_artifacts_are_store_names_not_paths()
    {
        foreach (var hostile in new[]
        {
            "../outside.json", "sub/checkpoint-1.json", "..", ".",
            "c:\\checkpoint-1.json", "dir\\checkpoint-1.json",
        })
        {
            var request = new WorkRestoreRequest { Artifact = hostile };
            Assert.Equal(
                "work.restore.artifact",
                Assert.Throws<AgentContractViolationException>(request.Validate).Field);
        }

        new WorkRestoreRequest { Artifact = "checkpoint-1737000000-abcdef.json" }.Validate();
        // Asking for the newest verifiable artifact is legal.
        new WorkRestoreRequest().Validate();
    }

    /// <summary>The reported configuration must be usable as a fact: a zero
    /// round budget could never finish a turn, and "ready" may not appear beside
    /// a blocker.</summary>
    [Fact]
    public void Reported_run_config_and_continue_readiness_stay_self_consistent()
    {
        var config = new WorkRunConfig
        {
            ContextPolicy = "rolling",
            MaxModelRounds = 16,
            MaxModelRoundsSource = "kernel_default",
            MaintenanceMaxCallsPerMaintain = 4,
        };
        config.Validate();
        Assert.Equal(
            "work.snapshot.effective_config.max_model_rounds",
            Assert.Throws<AgentContractViolationException>(
                (config with { MaxModelRounds = 0 }).Validate).Field);

        Assert.Equal(
            "work.snapshot.continue_readiness",
            Assert.Throws<AgentContractViolationException>(
                new WorkContinueReadiness
                {
                    CanContinue = true,
                    Reason = WorkContinueReason.TurnRunning,
                }.Validate).Field);

        new WorkContinueReadiness
        {
            CanContinue = false,
            Reason = WorkContinueReason.RecoveryRequired,
        }.Validate();
    }

    /// <summary>
    /// A mutation the client refused before sending is reported as never sent —
    /// not as an unknown outcome. The distinction matters: an unknown outcome
    /// tells the operator to re-check the server (and tempts a resend), while
    /// this request provably never happened.
    /// </summary>
    [Fact]
    public async Task A_locally_refused_mutation_is_not_reported_as_an_unknown_outcome()
    {
        await using var host = new AnsweringHost();
        await using var session = new ResumableSession(() => host.ConnectAsync());

        // A live session first, so the refusal below cannot be confused with a
        // connection problem.
        var snapshot = await session.SnapshotAsync().WaitAsync(TimeSpan.FromSeconds(10));
        Assert.True(snapshot.RunStarted);
        var framesBefore = host.FramesServed;

        // An empty instruction cannot pass the steer validator, so the request
        // never reaches the wire. The session must propagate that as itself —
        // NOT as AgentUnknownOutcomeException, which would tell the operator the
        // correction may have been applied.
        var refused = await Assert.ThrowsAsync<AgentContractViolationException>(
            () => session.SteerAsync(string.Empty).WaitAsync(TimeSpan.FromSeconds(10)));
        Assert.Equal("work.steer.instruction", refused.Field);
        Assert.True(refused.RequestNotSent, "the request never reached the wire");
        Assert.Equal(framesBefore, host.FramesServed);
    }

    /// <summary>A scripted loopback host that answers snapshots (enough for a
    /// live session) and counts the frames it actually served, so "never sent"
    /// is measured rather than assumed.</summary>
    private sealed class AnsweringHost : IAsyncDisposable
    {
        private readonly System.Net.Sockets.TcpListener _listener =
            new(System.Net.IPAddress.Loopback, 0);
        private readonly CancellationTokenSource _stopped = new();
        private readonly Task _serveLoop;
        private int _framesServed;

        public AnsweringHost()
        {
            _listener.Start();
            _serveLoop = Task.Run(() => ServeAsync(_stopped.Token));
        }

        public int FramesServed => Volatile.Read(ref _framesServed);

        public async Task<Stream> ConnectAsync()
        {
            var client = new System.Net.Sockets.TcpClient();
            await client.ConnectAsync(
                System.Net.IPAddress.Loopback,
                ((System.Net.IPEndPoint)_listener.LocalEndpoint).Port);
            return client.GetStream();
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
            catch (Exception failure) when (failure is OperationCanceledException
                or ObjectDisposedException or System.Net.Sockets.SocketException)
            {
                // The drill is over; the listener was stopped.
            }
        }

        private async Task ServeClientAsync(
            System.Net.Sockets.TcpClient client, CancellationToken cancellationToken)
        {
            using var owned = client;
            var stream = client.GetStream();
            while (!cancellationToken.IsCancellationRequested)
            {
                var request = await FrameCodec.ReadFrameAsync(
                    stream, FrameCodec.DefaultMaxFrameBytes, cancellationToken);
                if (request is null)
                {
                    return;
                }
                Interlocked.Increment(ref _framesServed);
                using var document = JsonDocument.Parse(request);
                var root = document.RootElement.Clone();
                var operation = root.GetProperty("route").GetProperty("operation").GetString();
                var response = new
                {
                    protocol = root.GetProperty("protocol"),
                    message_id = Guid.NewGuid().ToString("D"),
                    request_id = root.GetProperty("request_id"),
                    kind = "response",
                    route = root.GetProperty("route"),
                    causality = new
                    {
                        correlation_id = root.GetProperty("causality").GetProperty("correlation_id"),
                        causation_id = root.GetProperty("message_id"),
                    },
                    payload = operation == "snapshot"
                        ? JsonSerializer.Deserialize<JsonElement>(
                            "{\"status\":\"success\",\"value\":{\"run_started\":true,"
                            + "\"run_completed\":false,\"watermark\":7,"
                            + "\"run_id\":\"00000000-0000-4000-8000-000000000031\","
                            + "\"workspace_root\":\"/workspaces/test\",\"tasks\":[],"
                            + "\"pending_approvals\":[],\"resync_required\":false}}")
                        : JsonSerializer.Deserialize<JsonElement>(
                            "{\"status\":\"success\",\"value\":{\"watermark\":7,\"resync_required\":false}}"),
                };
                await FrameCodec.WriteFrameAsync(
                    stream,
                    JsonSerializer.SerializeToUtf8Bytes(response, AgentJson.Options),
                    FrameCodec.DefaultMaxFrameBytes,
                    cancellationToken);
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
}
