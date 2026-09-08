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

    /// <summary>Continues the run's active task.</summary>
    Task<WorkContinueResponse> ContinueAsync(CancellationToken cancellationToken = default);

    /// <summary>Explicit cancel command for the current turn. Distinct from
    /// cancelling a local request wait.</summary>
    Task<WorkCancelResponse> CancelCurrentTurnAsync(CancellationToken cancellationToken = default);

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
    Task<WorkChangesResponse> ReadChangesAsync(
        int? limit = null, string? afterTx = null, CancellationToken cancellationToken = default);

    /// <summary>Reads one run-scoped artifact's bounded body (B3). Run-scoped
    /// read; never starts a model round.</summary>
    Task<WorkArtifactResponse> ReadArtifactAsync(
        string reference, uint? maxBytes = null, CancellationToken cancellationToken = default);

    /// <summary>Reads the context engine's bounded item summary (B3).
    /// Run-scoped read; never starts a model round.</summary>
    Task<WorkContextResponse> ReadContextAsync(
        uint? limit = null, CancellationToken cancellationToken = default);
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
