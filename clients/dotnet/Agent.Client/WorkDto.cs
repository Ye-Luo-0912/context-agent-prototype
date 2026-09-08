using System.Text.Json;
using System.Text.Json.Serialization;

namespace FocusAgent.Client;

/// <summary>Mirrors Rust <c>WorkSubmitDisposition</c>.</summary>
public enum WorkSubmitDisposition
{
    Accepted,
    AlreadyAccepted,
}

/// <summary>
/// One long-task submission. The same <c>client_request_id</c> with the same
/// goal is an idempotent retry returning the original admission; the same id
/// with a different goal is a conflict. The id is process-scoped and becomes
/// unknown after a host restart.
/// </summary>
public sealed record WorkSubmitRequest : IProtocolPayload
{
    [JsonPropertyName("goal")]
    public string Goal { get; init; } = string.Empty;

    [JsonPropertyName("client_request_id")]
    public string ClientRequestId { get; init; } = string.Empty;

    public const int MaxGoalChars = 200_000;
    public const int MaxClientRequestIdBytes = 128;

    public void Validate()
    {
        ContractText.ValidateText("work.submit.goal", Goal, MaxGoalChars);
        ContractText.ValidateOpaque("work.submit.client_request_id", ClientRequestId, MaxClientRequestIdBytes);
    }
}

public sealed record WorkSubmitResponse : IProtocolPayload
{
    [JsonPropertyName("disposition")]
    public WorkSubmitDisposition Disposition { get; init; }

    [JsonPropertyName("task_id")]
    public string TaskId { get; init; } = string.Empty;

    public void Validate() =>
        ContractText.ValidateTaskId("work.submit.task_id", TaskId);
}

/// <summary>Continue the run's active task. Empty body by contract.</summary>
public sealed record WorkContinueRequest : IProtocolPayload
{
    public void Validate()
    {
    }
}

public sealed record WorkContinueResponse : IProtocolPayload
{
    [JsonPropertyName("task_id")]
    public string TaskId { get; init; } = string.Empty;

    public void Validate() =>
        ContractText.ValidateTaskId("work.continue.task_id", TaskId);
}

/// <summary>Cancel the run's current in-flight turn. Empty body by contract.</summary>
public sealed record WorkCancelRequest : IProtocolPayload
{
    public void Validate()
    {
    }
}

public enum TurnCancelAckStatus
{
    NoActiveTurn,
    Cancelled,
}

/// <summary>
/// Core's exact post-cancellation truth. <see cref="TurnCancelAckStatus.Cancelled"/>
/// proves the durable barrier; <see cref="TurnCancelAckStatus.NoActiveTurn"/> is a
/// fact, not a failure.
/// </summary>
public sealed record TurnCancelAck
{
    [JsonPropertyName("status")]
    public TurnCancelAckStatus Status { get; init; }

    [JsonPropertyName("turn_id")]
    public string? TurnId { get; init; }

    [JsonPropertyName("task_id")]
    public string? TaskId { get; init; }

    [JsonPropertyName("operation_id")]
    public string? OperationId { get; init; }

    [JsonPropertyName("cancelled_generation")]
    public ulong? CancelledGeneration { get; init; }

    [JsonPropertyName("effective_generation")]
    public ulong? EffectiveGeneration { get; init; }
}

internal sealed class TurnCancelAckConverter : JsonConverter<TurnCancelAck>
{
    public override TurnCancelAck Read(ref Utf8JsonReader reader, Type typeToConvert, JsonSerializerOptions options)
    {
        using var doc = JsonDocument.ParseValue(ref reader);
        var root = doc.RootElement;
        if (root.ValueKind != JsonValueKind.Object || !root.TryGetProperty("status", out var status))
        {
            throw new JsonException("turn cancel ack must be a status-tagged object");
        }
        var statusText = status.GetString();
        if (statusText == "no_active_turn")
        {
            if (root.EnumerateObject().Any(p => p.Name != "status"))
            {
                throw new JsonException(
                    $"no_active_turn ack carries unknown fields: {root.GetRawText()}");
            }
            return new TurnCancelAck { Status = TurnCancelAckStatus.NoActiveTurn };
        }
        if (statusText == "cancelled")
        {
            foreach (var property in root.EnumerateObject())
            {
                if (property.Name is not ("status" or "turn_id" or "task_id" or "operation_id"
                    or "cancelled_generation" or "effective_generation"))
                {
                    throw new JsonException(
                        $"cancelled ack carries unknown field '{property.Name}': {root.GetRawText()}");
                }
            }
            return new TurnCancelAck
            {
                Status = TurnCancelAckStatus.Cancelled,
                TurnId = RequiredString(root, "turn_id"),
                TaskId = OptionalString(root, "task_id"),
                OperationId = OptionalString(root, "operation_id"),
                CancelledGeneration = RequiredUInt64(root, "cancelled_generation"),
                EffectiveGeneration = RequiredUInt64(root, "effective_generation"),
            };
        }
        throw new JsonException($"unknown turn cancel ack status '{statusText}'");
    }

    public override void Write(Utf8JsonWriter writer, TurnCancelAck value, JsonSerializerOptions options)
    {
        writer.WriteStartObject();
        writer.WriteString("status", value.Status switch
        {
            TurnCancelAckStatus.NoActiveTurn => "no_active_turn",
            TurnCancelAckStatus.Cancelled => "cancelled",
            _ => throw new JsonException("unknown turn cancel ack status"),
        });
        if (value.Status == TurnCancelAckStatus.Cancelled)
        {
            // Rust serializes every Cancelled field (options emit null), so
            // byte-fidelity requires always writing these three strings.
            writer.WriteString("turn_id", value.TurnId);
            writer.WriteString("task_id", value.TaskId);
            writer.WriteString("operation_id", value.OperationId);
            writer.WriteNumber("cancelled_generation", value.CancelledGeneration ?? 0);
            writer.WriteNumber("effective_generation", value.EffectiveGeneration ?? 0);
        }
        writer.WriteEndObject();
    }

    private static string RequiredString(JsonElement root, string name) =>
        root.TryGetProperty(name, out var element) && element.ValueKind == JsonValueKind.String
            ? element.GetString()!
            : throw new JsonException($"cancelled ack requires string {name}");

    private static string? OptionalString(JsonElement root, string name) =>
        root.TryGetProperty(name, out var element) && element.ValueKind == JsonValueKind.String
            ? element.GetString()
            : null;

    private static ulong RequiredUInt64(JsonElement root, string name) =>
        root.TryGetProperty(name, out var element) && element.TryGetUInt64(out var value)
            ? value
            : throw new JsonException($"cancelled ack requires u64 {name}");
}

public sealed record WorkCancelResponse : IProtocolPayload
{
    [JsonPropertyName("ack")]
    [JsonConverter(typeof(TurnCancelAckConverter))]
    public TurnCancelAck Ack { get; init; } = new();

    public void Validate()
    {
    }
}

public sealed record WorkSnapshotRequest : IProtocolPayload
{
    public void Validate()
    {
    }
}

public enum TaskSnapshotStatus
{
    Active,
    Suspended,
    Completed,
}

public sealed record TaskSnapshotEntry
{
    [JsonPropertyName("task_id")]
    public string TaskId { get; init; } = string.Empty;

    [JsonPropertyName("goal")]
    public string Goal { get; init; } = string.Empty;

    [JsonPropertyName("status")]
    public TaskSnapshotStatus Status { get; init; }

    /// <summary>This task's own anchor revision; never compared across tasks.</summary>
    [JsonPropertyName("anchor_revision")]
    public ulong AnchorRevision { get; init; }

    [JsonPropertyName("tool_requirement_revision")]
    public ulong ToolRequirementRevision { get; init; }

    [JsonPropertyName("tool_requirement_count")]
    public uint ToolRequirementCount { get; init; }
}

public sealed record FocusSnapshot
{
    [JsonPropertyName("task_id")]
    public string TaskId { get; init; } = string.Empty;

    [JsonPropertyName("goal")]
    public string Goal { get; init; } = string.Empty;

    [JsonPropertyName("anchor_revision")]
    public ulong AnchorRevision { get; init; }
}

public sealed record PendingApprovalSnapshot
{
    [JsonPropertyName("request_id")]
    public string RequestId { get; init; } = string.Empty;

    [JsonPropertyName("call_name")]
    public string CallName { get; init; } = string.Empty;
}

/// <summary>
/// One consistent typed snapshot. <see cref="Watermark"/> is the durable event
/// sequence it reflects; a client whose stream is behind must treat
/// <see cref="ResyncRequired"/> as "rebuild from this snapshot", never splice.
/// </summary>
public sealed record WorkSnapshotResponse : IProtocolPayload
{
    public const int MaxTasks = 256;
    public const int MaxPendingApprovals = 16;
    public const int MaxGoalChars = 2_000;
    public const int MaxCallNameBytes = 128;

    [JsonPropertyName("run_started")]
    public bool RunStarted { get; init; }

    [JsonPropertyName("run_completed")]
    public bool RunCompleted { get; init; }

    [JsonPropertyName("watermark")]
    public ulong Watermark { get; init; }

    [JsonPropertyName("focus")]
    public FocusSnapshot? Focus { get; init; }

    [JsonPropertyName("tasks")]
    public IReadOnlyList<TaskSnapshotEntry> Tasks { get; init; } = [];

    [JsonPropertyName("pending_approvals")]
    public IReadOnlyList<PendingApprovalSnapshot> PendingApprovals { get; init; } = [];

    [JsonPropertyName("resync_required")]
    public bool ResyncRequired { get; init; }

    public void Validate()
    {
        if (Tasks.Count > MaxTasks)
        {
            throw new AgentContractViolationException(
                "work.snapshot.tasks", $"contains {Tasks.Count} entries, above the {MaxTasks} entry bound");
        }
        if (PendingApprovals.Count > MaxPendingApprovals)
        {
            throw new AgentContractViolationException(
                "work.snapshot.pending_approvals",
                $"contains {PendingApprovals.Count} entries, above the {MaxPendingApprovals} entry bound");
        }
        foreach (var task in Tasks)
        {
            ContractText.ValidateText("work.snapshot.task.goal", task.Goal, MaxGoalChars);
            ContractText.ValidateTaskId("work.snapshot.task.task_id", task.TaskId);
        }
        foreach (var approval in PendingApprovals)
        {
            ContractText.ValidateOpaque("work.snapshot.approval.request_id", approval.RequestId, WorkSubmitRequest.MaxClientRequestIdBytes);
            ContractText.ValidateIdentifier("work.snapshot.approval.call_name", approval.CallName, MaxCallNameBytes);
        }
        if (Focus is not null)
        {
            ContractText.ValidateText("work.snapshot.focus.goal", Focus.Goal, MaxGoalChars);
            ContractText.ValidateTaskId("work.snapshot.focus.task_id", Focus.TaskId);
        }
    }
}

public sealed record WorkSubscribeRequest : IProtocolPayload
{
    /// <summary>The most recent durable sequence a subscribe may replay from;
    /// older cursors get <c>resync_required</c> instead of a replay.</summary>
    [JsonPropertyName("replay_after_seq")]
    public ulong? ReplayAfterSeq { get; init; }

    public const ulong MaxReplayWindowEvents = 4_096;

    public void Validate()
    {
    }
}

public sealed record WorkSubscribeResponse : IProtocolPayload
{
    [JsonPropertyName("watermark")]
    public ulong Watermark { get; init; }

    [JsonPropertyName("resync_required")]
    public bool ResyncRequired { get; init; }

    public void Validate()
    {
    }
}

/// <summary>
/// One forwarded runtime event envelope, mirroring the kernel's
/// <c>agent_contracts::RuntimeEventEnvelope</c> (N3/F06 client half):
/// run id, the durable journal cursor and the event itself, forwarded
/// verbatim — the host never rewrites events.
/// </summary>
/// <remarks>
/// The event body is deliberately kept as a raw JSON element: the client
/// routes and bounds events by their <c>type</c> tag (serde
/// <c>tag = "type", rename_all = "snake_case"</c>), it does not interpret
/// kernel payloads — the typed view of run state is the snapshot's job, not
/// a second event algebra to keep in drift-sync. Unknown envelope FIELDS are
/// tolerated exactly like the Rust side (the kernel's
/// <c>RuntimeEventEnvelope</c> derives serde without
/// <c>deny_unknown_fields</c>); the notification wrapper around it stays
/// deny-strict.
/// </remarks>
[JsonUnmappedMemberHandling(JsonUnmappedMemberHandling.Skip)]
public sealed record RuntimeEventEnvelope
{
    /// <summary>Canonical run id (Rust <c>RunId</c>: a bare UUID).</summary>
    [JsonPropertyName("run_id")]
    public string RunId { get; init; } = string.Empty;

    /// <summary>Cursor in the durable event journal. Journaled events are
    /// contiguous from 1; <c>model_delta</c> and <c>model_retrying</c> are
    /// live-only and repeat the cursor of the preceding durable event.</summary>
    [JsonPropertyName("seq")]
    public ulong Seq { get; init; }

    [JsonPropertyName("timestamp_ms")]
    public ulong TimestampMs { get; init; }

    /// <summary>The type-tagged event object, verbatim.</summary>
    [JsonPropertyName("event")]
    public JsonElement Event { get; init; }

    /// <summary>The event's snake_case type tag (empty when the frame is
    /// malformed; <see cref="Validate"/> refuses that shape).</summary>
    public string EventType =>
        Event.ValueKind == JsonValueKind.Object
        && Event.TryGetProperty("type", out var tag)
        && tag.ValueKind == JsonValueKind.String
            ? tag.GetString() ?? string.Empty
            : string.Empty;

    /// <summary>
    /// The two events the kernel documents as live-only
    /// (<c>model_delta</c>, <c>model_retrying</c>): they advance nothing
    /// durably and carry newest-wins progress. Every other event type —
    /// including every approval-relevant and terminal lifecycle fact — is
    /// durable by default (fail closed).
    /// </summary>
    public bool IsLiveOnlyProgress => EventType is "model_delta" or "model_retrying";

    public void Validate()
    {
        ProtocolIds.ValidateCanonical("work.event.envelope.run_id", RunId);
        if (Event.ValueKind != JsonValueKind.Object)
        {
            throw new AgentContractViolationException(
                "work.event.envelope.event", "must be a type-tagged object");
        }
        if (EventType.Length == 0)
        {
            throw new AgentContractViolationException(
                "work.event.envelope.event.type", "must be a non-empty string tag");
        }
    }
}

/// <summary>
/// One durable event forwarded to a subscribed session (the work/event
/// notification route), mirroring Rust
/// <c>agent-platform-protocol::work::WorkEventNotification</c>:
/// <c>{ "envelope": &lt;RuntimeEventEnvelope&gt; }</c>.
/// </summary>
public sealed record WorkEventNotification : IProtocolPayload
{
    [JsonPropertyName("envelope")]
    public RuntimeEventEnvelope Envelope { get; init; } = new();

    /// <summary>Queue-pressure classification derived from the event type:
    /// live-only progress may be shed under load; durable facts may not.</summary>
    public bool IsLiveOnlyProgress => Envelope.IsLiveOnlyProgress;

    /// <summary>The event's snake_case type tag — the classification key for
    /// queue shedding and the consumer's first-level dispatch key.</summary>
    public string EventType => Envelope.EventType;

    public void Validate() => Envelope.Validate();
}

/// <summary>Rust derives serde without rename_all: wire values are "Allow"/"Deny".</summary>
[JsonConverter(typeof(ApprovalDecisionConverter))]
public enum ApprovalDecision
{
    Allow,
    Deny,
}

public sealed record ApprovalRespondRequest : IProtocolPayload
{
    [JsonPropertyName("request_id")]
    public string RequestId { get; init; } = string.Empty;

    [JsonPropertyName("decision")]
    public ApprovalDecision Decision { get; init; }

    public void Validate() =>
        ContractText.ValidateOpaque("approval.respond.request_id", RequestId, WorkSubmitRequest.MaxClientRequestIdBytes);
}

public enum ApprovalRespondOutcome
{
    /// <summary>The pending waiter received this decision.</summary>
    Delivered,

    /// <summary>Already answered, expired or unknown: the current fact, not
    /// an error and not permission to retry blindly.</summary>
    NoLongerPending,
}

public sealed record ApprovalRespondResponse : IProtocolPayload
{
    [JsonPropertyName("outcome")]
    public ApprovalRespondOutcome Outcome { get; init; }

    public void Validate()
    {
    }
}
