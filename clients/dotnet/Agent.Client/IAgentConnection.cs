using System.Threading.Channels;

namespace FocusAgent.Client;

/// <summary>
/// The operations the desktop shell (or any other .NET host application)
/// needs from a platform connection. The real implementation talks to the
/// Rust host over a local transport; test/fixture implementations must not
/// pretend to execute work — they exist for layout and protocol drills.
/// </summary>
public interface IAgentConnection : IAsyncDisposable
{
    bool IsConnected { get; }

    /// <summary>The typed work/event notification stream: host event
    /// notifications (kind=notification frames, no request id) decoded into
    /// <see cref="WorkEventNotification"/>s and delivered in host order.
    /// The stream is bounded — its overflow policy sheds live-only progress
    /// first and never drops approval/terminal facts (see
    /// <see cref="BoundedEventQueue"/>).</summary>
    ChannelReader<WorkEventNotification> Events { get; }

    /// <summary>Submits a new long-task goal; returns the acceptance receipt
    /// (admission, not completion).</summary>
    Task<WorkSubmitResponse> SubmitWorkAsync(string goal, string clientRequestId, CancellationToken cancellationToken = default);

    /// <summary>Continues the run's active task. F5: pass
    /// <paramref name="expectedTaskId"/> to continue only when the run is on
    /// exactly that task — the server compares it inside the actor and starts no
    /// turn on a mismatch.</summary>
    Task<WorkContinueResponse> ContinueAsync(
        string? expectedTaskId = null, CancellationToken cancellationToken = default);

    /// <summary>Explicit cancel command for the current turn. Distinct from
    /// cancelling a local request wait. F5: naming the task/turn the caller
    /// observed makes the cancel precise — an expectation that no longer matches
    /// cancels nothing and reports the live identity.</summary>
    Task<WorkCancelResponse> CancelCurrentTurnAsync(
        string? expectedTaskId = null,
        string? expectedTurnId = null,
        CancellationToken cancellationToken = default);

    /// <summary>F5: applies an in-task correction to work already running. This
    /// is NOT a submission: it never creates a task and never re-focuses, and
    /// naming <paramref name="expectedTaskId"/> refuses unless the runtime is on
    /// exactly that task. The receipt distinguishes applied, queued (admitted
    /// into the running turn's single slot) and refused.
    /// <para>Sent exactly once: a lost reply is an unknown outcome, never an
    /// automatic re-send — re-issuing a correction could double-apply it.</para></summary>
    Task<WorkSteerResponse> SteerAsync(
        string instruction, string? expectedTaskId = null, CancellationToken cancellationToken = default);

    /// <summary>F5: activates an existing task through the same RuntimeActor
    /// that owns the task table. An unknown or completed task is refused.</summary>
    Task<WorkActivateResponse> ActivateTaskAsync(
        string taskId, CancellationToken cancellationToken = default);

    /// <summary>F5: suspends a task without completing it. Suspension is not
    /// completion; the response never implies one.</summary>
    Task<WorkSuspendResponse> SuspendTaskAsync(
        string? expectedTaskId = null, CancellationToken cancellationToken = default);

    /// <summary>F5: captures one FORMAL cross-plane checkpoint (actor, context
    /// and host capability planes) into the run's own store and returns the
    /// artifact name <see cref="RestoreAsync"/> accepts.</summary>
    Task<WorkCheckpointResponse> CheckpointAsync(CancellationToken cancellationToken = default);

    /// <summary>F5: restores one checkpoint through the full cross-plane
    /// transaction. <paramref name="artifact"/> is a store artifact name — never
    /// a path; omitting it restores the newest artifact that fully verifies.</summary>
    Task<WorkRestoreResponse> RestoreAsync(
        string? artifact = null, CancellationToken cancellationToken = default);

    /// <summary>Fetches one consistent typed snapshot.</summary>
    Task<WorkSnapshotResponse> SnapshotAsync(CancellationToken cancellationToken = default);

    /// <summary>Subscribes the session event stream; the returned watermark
    /// is where live delivery starts.</summary>
    Task<WorkSubscribeResponse> SubscribeAsync(ulong? replayAfterSeq = null, CancellationToken cancellationToken = default);

    /// <summary>Answers one pending approval bound by its server-side
    /// request id; late/duplicate answers report the current fact.</summary>
    Task<ApprovalRespondResponse> RespondApprovalAsync(
        string requestId, ApprovalDecision decision, CancellationToken cancellationToken = default);

    /// <summary>Reads one task's full anchor card (B3). Run-scoped read;
    /// never starts a model round.</summary>
    Task<WorkTaskDetailResponse> TaskDetailAsync(
        string taskId, CancellationToken cancellationToken = default);

    /// <summary>Reads the workspace change journal, newest first (B3).
    /// Run-scoped read; never starts a model round.</summary>
    Task<WorkTaskCompletionResponse> TaskCompletionAsync(
        string taskId, CancellationToken cancellationToken = default);

    Task<WorkChangesResponse> ReadChangesAsync(
        int? limit = null, string? afterTx = null, CancellationToken cancellationToken = default);

    /// <summary>Reads one run-scoped artifact's bounded body (B3), starting
    /// at the byte <paramref name="offset"/> when paging (PLATFORM-2/F08).
    /// Run-scoped read; never starts a model round. The response's
    /// <c>next_offset</c>/<c>truncated</c> facts drive the continuation.</summary>
    Task<WorkArtifactResponse> ReadArtifactAsync(
        string reference, uint? maxBytes = null, ulong? offset = null, CancellationToken cancellationToken = default);

    /// <summary>Reads the context engine's bounded item summary (B3).
    /// Run-scoped read; never starts a model round.</summary>
    Task<WorkContextResponse> ReadContextAsync(
        uint? limit = null, CancellationToken cancellationToken = default);

    /// <summary>PLATFORM-1 (F06): asks the run's submission ledger what became
    /// of THIS caller's exact <paramref name="clientRequestId"/> — never a
    /// goal-text match. <paramref name="payloadDigest"/> (the caller's own
    /// <see cref="SubmitPayloadDigest.Compute"/> token) lets the answer
    /// distinguish "this exact payload was admitted" from "a different payload
    /// holds this id". Run-scoped read; never starts a model round and never
    /// mutates state. <c>Unknown</c>/<c>Expired</c> prove nothing in either
    /// direction and must never be read as "not executed".</summary>
    Task<WorkSubmitResultResponse> SubmitResultAsync(
        string clientRequestId, string? payloadDigest = null, CancellationToken cancellationToken = default);
}

/// <summary>
/// Bounded, monotonic client request ids for idempotent submission retries.
/// Process-scoped by design: after an app restart a fresh prefix starts, and
/// the host treats unknown ids as new submissions.
/// </summary>
public static class ClientRequestIds
{
    private static long _sequence;

    public static string Next() => $"ui-{DateTimeOffset.UtcNow.ToUnixTimeMilliseconds():x}-{Interlocked.Increment(ref _sequence):x}";
}
