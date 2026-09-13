using FocusAgent.Client;

namespace FocusAgent.Client.Tests;

/// <summary>
/// Shared base for the view-model stubs that predate F5's long-flow control
/// routes (steering, activate/suspend, formal checkpoint capture/restore).
///
/// Those drills exercise other behaviour and never invoke these routes. Rather
/// than have each stub fabricate a plausible-looking receipt — which would let a
/// future test believe a steering correction or a restore was performed when
/// nothing happened — every member here refuses loudly. A drill that genuinely
/// needs one of these routes overrides it with an explicit fixture answer.
/// </summary>
internal abstract class LongFlowControlsNotExercised
{
    private static Task<T> NotExercised<T>(string route) =>
        Task.FromException<T>(new NotSupportedException(
            $"this stub does not serve {route}; a drill that needs it must answer it explicitly"));

    public virtual Task<WorkSteerResponse> SteerAsync(
        string instruction, string? expectedTaskId = null, CancellationToken cancellationToken = default) =>
        NotExercised<WorkSteerResponse>("work.steer");

    public virtual Task<WorkActivateResponse> ActivateTaskAsync(
        string taskId, CancellationToken cancellationToken = default) =>
        NotExercised<WorkActivateResponse>("work.activate");

    public virtual Task<WorkSuspendResponse> SuspendTaskAsync(
        string? expectedTaskId = null, CancellationToken cancellationToken = default) =>
        NotExercised<WorkSuspendResponse>("work.suspend");

    public virtual Task<WorkCheckpointResponse> CheckpointAsync(
        CancellationToken cancellationToken = default) =>
        NotExercised<WorkCheckpointResponse>("work.checkpoint");

    public virtual Task<WorkRestoreResponse> RestoreAsync(
        string? artifact = null, CancellationToken cancellationToken = default) =>
        NotExercised<WorkRestoreResponse>("work.restore");
}
