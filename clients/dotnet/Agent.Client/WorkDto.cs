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

/// <summary>Mirrors Rust <c>ApprovalRisk</c> (protocol): the gate's own
/// declared risk for one pending approval — an operator-facing fact, not a
/// permission.</summary>
public enum ApprovalRisk
{
    ReadOnly,
    WorkspaceWrite,
    ProcessExecution,
}

/// <summary>
/// One approval awaiting a decision. <see cref="Risk"/> is the gate's own
/// declared risk; <see cref="TargetSummary"/> is a bounded operator display
/// projection of the call's structured arguments, <c>null</c> when the
/// arguments carry none of the well-known keys — the UI then shows
/// "unavailable" instead of guessing.
/// </summary>
public sealed record PendingApprovalSnapshot
{
    public const int MaxTargetSummaryChars = 256;

    [JsonPropertyName("request_id")]
    public string RequestId { get; init; } = string.Empty;

    [JsonPropertyName("call_name")]
    public string CallName { get; init; } = string.Empty;

    [JsonPropertyName("risk")]
    [JsonRequired]
    public ApprovalRisk Risk { get; init; }

    [JsonPropertyName("target_summary")]
    public string? TargetSummary { get; init; }

    public void Validate()
    {
        ContractText.ValidateOpaque("work.snapshot.approval.request_id", RequestId, WorkSubmitRequest.MaxClientRequestIdBytes);
        ContractText.ValidateIdentifier("work.snapshot.approval.call_name", CallName, WorkSnapshotResponse.MaxCallNameBytes);
        if (!Enum.IsDefined(typeof(ApprovalRisk), Risk))
        {
            throw new AgentContractViolationException(
                "work.snapshot.approval.risk", $"is not a defined approval risk: {Risk}");
        }
        if (TargetSummary is not null)
        {
            ContractText.ValidateText("work.snapshot.approval.target_summary", TargetSummary, MaxTargetSummaryChars);
        }
    }
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
    /// Must match the Rust `MAX_SNAPSHOT_GOAL_CHARS` (200_000) exactly
    /// (R06): the host projects accepted goals verbatim, so a snapshot
    /// bound smaller than the submit bound would fault every reconnect for a
    /// legal long goal.
    public const int MaxGoalChars = 200_000;
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
            approval.Validate();
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

// ---------------------------------------------------------------------------
// B3 read-only routes: task detail / change journal / artifact bytes /
// context summary. All four are run-scoped reads (Envelope.cs routes) — they
// never start a model round, never mutate state and never consult Core's
// approval gate. The Rust side mirrors the workspace journal; bytes travel
// base64 because artifacts are binary-safe and the wire is JSON.
// ---------------------------------------------------------------------------

/// <summary>One task's full anchor card (B3).</summary>
public sealed record WorkTaskDetailRequest : IProtocolPayload
{
    [JsonPropertyName("task_id")]
    public string TaskId { get; init; } = string.Empty;

    public void Validate() =>
        ContractText.ValidateTaskId("work.task_detail.task_id", TaskId);
}

/// <summary>Rust <c>TaskAnchorView</c>: the bounded prompt projection of a
/// task anchor (plan/acceptance/open loops). Not an authority surface.</summary>
public sealed record TaskAnchorView
{
    [JsonPropertyName("revision")]
    public ulong Revision { get; init; }

    [JsonPropertyName("original_goal")]
    public string OriginalGoal { get; init; } = string.Empty;

    [JsonPropertyName("current_interpretation")]
    public string CurrentInterpretation { get; init; } = string.Empty;

    [JsonPropertyName("constraints")]
    public IReadOnlyList<string> Constraints { get; init; } = [];

    [JsonPropertyName("acceptance_criteria")]
    public IReadOnlyList<string> AcceptanceCriteria { get; init; } = [];

    [JsonPropertyName("plan_progress")]
    public IReadOnlyList<string> PlanProgress { get; init; } = [];

    [JsonPropertyName("open_loops")]
    public IReadOnlyList<string> OpenLoops { get; init; } = [];

    [JsonPropertyName("next_action")]
    public string NextAction { get; init; } = string.Empty;
}

public sealed record WorkTaskDetailResponse : IProtocolPayload
{
    [JsonPropertyName("task_id")]
    public string TaskId { get; init; } = string.Empty;

    [JsonPropertyName("goal")]
    public string Goal { get; init; } = string.Empty;

    [JsonPropertyName("status")]
    public TaskSnapshotStatus Status { get; init; }

    [JsonPropertyName("anchor_revision")]
    public ulong AnchorRevision { get; init; }

    [JsonPropertyName("anchor")]
    public TaskAnchorView Anchor { get; init; } = new();

    public void Validate()
    {
        ContractText.ValidateTaskId("work.task_detail.task_id", TaskId);
        ContractText.ValidateText("work.task_detail.goal", Goal, WorkSnapshotResponse.MaxGoalChars);
    }
}

/// <summary>Wire mirror of the workspace change journal (B3). A single record
/// carries one transaction's journaled phase; the journal's internal
/// <c>old_content</c> capture never travels.</summary>
public enum ChangeSummaryKind
{
    MutationPrepared,
    MutationCommitted,
    MutationRolledBack,
    DirectoryPrepared,
    DirectoryCommitted,
    DirectoryRolledBack,
}

[JsonConverter(typeof(ChangeSummaryConverter))]
public sealed record ChangeSummary
{
    public ChangeSummaryKind Kind { get; init; }

    public string TxId { get; init; } = string.Empty;

    public ulong TimestampMs { get; init; }

    public string? Tool { get; init; }

    public string? Path { get; init; }

    public string? Action { get; init; }

    public ulong BytesBefore { get; init; }

    public ulong BytesAfter { get; init; }

    public string? BeforeHash { get; init; }

    public string? AfterHash { get; init; }

    public string? Reason { get; init; }

    public string? EntryIdentity { get; init; }
}

/// <summary>Rust serde <c>tag = "kind", rename_all = "snake_case"</c>: reads
/// strictly (unknown fields on a variant are rejected), the internal
/// <c>old_content</c> is never accepted from or written to the wire.</summary>
internal sealed class ChangeSummaryConverter : JsonConverter<ChangeSummary>
{
    private static readonly string[] PreparedFields =
        ["tx_id", "timestamp_ms", "tool", "path", "action", "bytes_before", "bytes_after", "before_hash", "after_hash"];
    private static readonly string[] CommittedFields = ["tx_id", "timestamp_ms"];
    private static readonly string[] RolledBackFields = ["tx_id", "timestamp_ms", "reason"];
    private static readonly string[] DirectoryPreparedFields = ["tx_id", "timestamp_ms", "tool", "path"];
    private static readonly string[] DirectoryCommittedFields = ["tx_id", "timestamp_ms", "entry_identity"];
    private static readonly string[] DirectoryRolledBackFields = ["tx_id", "timestamp_ms", "reason"];

    public override ChangeSummary Read(ref Utf8JsonReader reader, Type typeToConvert, JsonSerializerOptions options)
    {
        using var document = JsonDocument.ParseValue(ref reader);
        var root = document.RootElement;
        if (root.ValueKind != JsonValueKind.Object || !root.TryGetProperty("kind", out var kind))
        {
            throw new JsonException("change summary must be a kind-tagged object");
        }
        var (name, fields) = kind.GetString() switch
        {
            "mutation_prepared" => ("mutation_prepared", PreparedFields),
            "mutation_committed" => ("mutation_committed", CommittedFields),
            "mutation_rolled_back" => ("mutation_rolled_back", RolledBackFields),
            "directory_prepared" => ("directory_prepared", DirectoryPreparedFields),
            "directory_committed" => ("directory_committed", DirectoryCommittedFields),
            "directory_rolled_back" => ("directory_rolled_back", DirectoryRolledBackFields),
            var other => throw new JsonException($"unknown change summary kind '{other}'"),
        };
        foreach (var property in root.EnumerateObject())
        {
            if (property.Name != "kind" && !fields.Contains(property.Name))
            {
                throw new JsonException(
                    $"change summary '{name}' carries unknown field '{property.Name}': {root.GetRawText()}");
            }
        }
        string Required(string field) =>
            root.TryGetProperty(field, out var element) && element.ValueKind == JsonValueKind.String
                ? element.GetString()!
                : throw new JsonException($"change summary '{name}' requires string {field}");
        ulong RequiredU64(string field) =>
            root.TryGetProperty(field, out var element) && element.TryGetUInt64(out var value)
                ? value
                : throw new JsonException($"change summary '{name}' requires u64 {field}");
        var txId = Required("tx_id");
        var timestampMs = RequiredU64("timestamp_ms");
        return new ChangeSummary
        {
            Kind = name switch
            {
                "mutation_prepared" => ChangeSummaryKind.MutationPrepared,
                "mutation_committed" => ChangeSummaryKind.MutationCommitted,
                "mutation_rolled_back" => ChangeSummaryKind.MutationRolledBack,
                "directory_prepared" => ChangeSummaryKind.DirectoryPrepared,
                "directory_committed" => ChangeSummaryKind.DirectoryCommitted,
                _ => ChangeSummaryKind.DirectoryRolledBack,
            },
            TxId = txId,
            TimestampMs = timestampMs,
            Tool = root.TryGetProperty("tool", out var tool) && tool.ValueKind == JsonValueKind.String ? tool.GetString() : null,
            Path = root.TryGetProperty("path", out var path) && path.ValueKind == JsonValueKind.String ? path.GetString() : null,
            Action = root.TryGetProperty("action", out var action) && action.ValueKind == JsonValueKind.String ? action.GetString() : null,
            BytesBefore = root.TryGetProperty("bytes_before", out var bb) && bb.TryGetUInt64(out var bbv) ? bbv : 0,
            BytesAfter = root.TryGetProperty("bytes_after", out var ba) && ba.TryGetUInt64(out var bav) ? bav : 0,
            BeforeHash = root.TryGetProperty("before_hash", out var bh) && bh.ValueKind == JsonValueKind.String ? bh.GetString() : null,
            AfterHash = root.TryGetProperty("after_hash", out var ah) && ah.ValueKind == JsonValueKind.String ? ah.GetString() : null,
            Reason = root.TryGetProperty("reason", out var reason) && reason.ValueKind == JsonValueKind.String ? reason.GetString() : null,
            EntryIdentity = root.TryGetProperty("entry_identity", out var identity) && identity.ValueKind == JsonValueKind.String ? identity.GetString() : null,
        };
    }

    public override void Write(Utf8JsonWriter writer, ChangeSummary value, JsonSerializerOptions options)
    {
        writer.WriteStartObject();
        writer.WriteString("kind", value.Kind switch
        {
            ChangeSummaryKind.MutationPrepared => "mutation_prepared",
            ChangeSummaryKind.MutationCommitted => "mutation_committed",
            ChangeSummaryKind.MutationRolledBack => "mutation_rolled_back",
            ChangeSummaryKind.DirectoryPrepared => "directory_prepared",
            ChangeSummaryKind.DirectoryCommitted => "directory_committed",
            _ => "directory_rolled_back",
        });
        writer.WriteString("tx_id", value.TxId);
        writer.WriteNumber("timestamp_ms", value.TimestampMs);
        switch (value.Kind)
        {
            case ChangeSummaryKind.MutationPrepared:
                writer.WriteString("tool", value.Tool);
                writer.WriteString("path", value.Path);
                writer.WriteString("action", value.Action);
                writer.WriteNumber("bytes_before", value.BytesBefore);
                writer.WriteNumber("bytes_after", value.BytesAfter);
                writer.WriteString("before_hash", value.BeforeHash);
                writer.WriteString("after_hash", value.AfterHash);
                break;
            case ChangeSummaryKind.MutationRolledBack:
                writer.WriteString("reason", value.Reason);
                break;
            case ChangeSummaryKind.DirectoryPrepared:
                writer.WriteString("tool", value.Tool);
                writer.WriteString("path", value.Path);
                break;
            case ChangeSummaryKind.DirectoryCommitted:
                writer.WriteString("entry_identity", value.EntryIdentity);
                break;
            case ChangeSummaryKind.DirectoryRolledBack:
                writer.WriteString("reason", value.Reason);
                break;
            case ChangeSummaryKind.MutationCommitted:
                break;
        }
        writer.WriteEndObject();
    }
}

public sealed record WorkChangesRequest : IProtocolPayload
{
    public const int MaxChangesLimit = 256;
    public const int MaxTxIdBytes = 64;

    [JsonPropertyName("limit")]
    public int? Limit { get; init; }

    [JsonPropertyName("after_tx")]
    public string? AfterTx { get; init; }

    public void Validate()
    {
        if (Limit is { } limit && (limit is <= 0 or > MaxChangesLimit))
        {
            throw new AgentContractViolationException("work.changes.limit", $"must be in 1..={MaxChangesLimit}");
        }
        if (AfterTx is { } afterTx)
        {
            ContractText.ValidateOpaque("work.changes.after_tx", afterTx, MaxTxIdBytes);
        }
    }
}

public sealed record WorkChangesResponse : IProtocolPayload
{
    [JsonPropertyName("changes")]
    public IReadOnlyList<ChangeSummary> Changes { get; init; } = [];

    public void Validate()
    {
        if (Changes.Count > WorkChangesRequest.MaxChangesLimit)
        {
            throw new AgentContractViolationException(
                "work.changes.changes",
                $"contains {Changes.Count} entries, above the {WorkChangesRequest.MaxChangesLimit} entry bound");
        }
        if (Changes.Any(c => string.IsNullOrEmpty(c.TxId)))
        {
            throw new AgentContractViolationException("work.changes.tx_id", "must not be empty");
        }
    }
}

public sealed record WorkArtifactRequest : IProtocolPayload
{
    public const uint MaxArtifactReadBytes = 64 * 1024;

    [JsonPropertyName("reference")]
    public string Reference { get; init; } = string.Empty;

    [JsonPropertyName("max_bytes")]
    public uint? MaxBytes { get; init; }

    public void Validate()
    {
        ContractText.ValidateOpaque("work.artifact.reference", Reference, 256);
        if (MaxBytes is { } max && (max is 0 or > MaxArtifactReadBytes))
        {
            throw new AgentContractViolationException(
                "work.artifact.max_bytes", $"must be in 1..={MaxArtifactReadBytes}");
        }
    }
}

public sealed record WorkArtifactResponse : IProtocolPayload
{
    [JsonPropertyName("reference")]
    public string Reference { get; init; } = string.Empty;

    [JsonPropertyName("size_bytes")]
    public ulong SizeBytes { get; init; }

    [JsonPropertyName("truncated")]
    public bool Truncated { get; init; }

    [JsonPropertyName("content_base64")]
    public string ContentBase64 { get; init; } = string.Empty;

    public void Validate()
    {
        ContractText.ValidateOpaque("work.artifact.reference", Reference, 256);
        byte[] decoded;
        try
        {
            decoded = Convert.FromBase64String(ContentBase64);
        }
        catch (FormatException)
        {
            throw new AgentContractViolationException("work.artifact.content_base64", "is not valid base64");
        }
        var consistent = Truncated
            ? decoded.LongLength < (long)SizeBytes
            : (ulong)decoded.LongLength == SizeBytes;
        if (!consistent)
        {
            throw new AgentContractViolationException(
                "work.artifact.size_bytes",
                $"size {SizeBytes} and truncated {Truncated} disagree with {decoded.LongLength} decoded bytes");
        }
    }
}

public sealed record WorkContextRequest : IProtocolPayload
{
    public const int MaxContextItems = 512;

    [JsonPropertyName("limit")]
    public uint? Limit { get; init; }

    public void Validate()
    {
        if (Limit is { } limit && (limit is 0 or > MaxContextItems))
        {
            throw new AgentContractViolationException("work.context.limit", $"must be in 1..={MaxContextItems}");
        }
    }
}

/// <summary>Mirror of <c>agent_contracts::ContextItemSummary</c>. The
/// type-tagged dimensions (<c>kind</c>, <c>scope</c>, <c>attention</c>,
/// <c>semantic</c>) stay raw JSON elements exactly like the runtime event
/// body: the client routes and bounds context by what it already knows, it
/// never re-interprets engine internals.</summary>
public sealed record ContextItemSummary
{
    [JsonPropertyName("id")]
    public string Id { get; init; } = string.Empty;

    [JsonPropertyName("kind")]
    public JsonElement Kind { get; init; }

    [JsonPropertyName("scope")]
    public JsonElement Scope { get; init; }

    [JsonPropertyName("scope_id")]
    public string? ScopeId { get; init; }

    [JsonPropertyName("attention")]
    public JsonElement Attention { get; init; }

    [JsonPropertyName("semantic")]
    public JsonElement Semantic { get; init; }

    [JsonPropertyName("importance")]
    public double Importance { get; init; }

    [JsonPropertyName("relevance")]
    public double Relevance { get; init; }

    [JsonPropertyName("created_tick")]
    public ulong CreatedTick { get; init; }

    [JsonPropertyName("created_turn")]
    public ulong CreatedTurn { get; init; }

    [JsonPropertyName("last_access_turn")]
    public ulong LastAccessTurn { get; init; }

    [JsonPropertyName("last_selected_turn")]
    public ulong? LastSelectedTurn { get; init; }

    [JsonPropertyName("access_count")]
    public uint AccessCount { get; init; }

    [JsonPropertyName("dependencies")]
    public IReadOnlyList<string> Dependencies { get; init; } = [];

    [JsonPropertyName("keep_alive")]
    public bool KeepAlive { get; init; }

    [JsonPropertyName("lease_until_turn")]
    public ulong? LeaseUntilTurn { get; init; }

    [JsonPropertyName("source")]
    public string? Source { get; init; }
}

public sealed record WorkContextResponse : IProtocolPayload
{
    [JsonPropertyName("items")]
    public IReadOnlyList<ContextItemSummary> Items { get; init; } = [];

    public void Validate()
    {
        if (Items.Count > WorkContextRequest.MaxContextItems)
        {
            throw new AgentContractViolationException(
                "work.context.items",
                $"contains {Items.Count} entries, above the {WorkContextRequest.MaxContextItems} entry bound");
        }
        if (Items.Any(item => string.IsNullOrEmpty(item.Id)))
        {
            throw new AgentContractViolationException("work.context.items.id", "must not be empty");
        }
    }
}
