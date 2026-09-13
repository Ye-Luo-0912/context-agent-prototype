using System.Text.Json;
using System.Text.Json.Serialization;

namespace FocusAgent.Client;

public static class ProtocolIds
{
    /// <summary>Parses and re-renders a canonical lowercase hyphenated UUID.</summary>
    public static string NewMessageId() => Guid.NewGuid().ToString("D");

    public static void ValidateCanonical(string field, string value)
    {
        if (!Guid.TryParseExact(value, "D", out var guid)
            || guid.ToString("D") != value
            || guid == Guid.Empty)
        {
            throw new AgentContractViolationException(
                field, "expected the canonical lowercase hyphenated non-nil UUID form");
        }
    }
}

public sealed class ActiveFeaturesConverter : JsonConverter<ActiveFeatures>
{
    public override ActiveFeatures Read(ref Utf8JsonReader reader, Type typeToConvert, JsonSerializerOptions options)
    {
        if (reader.TokenType != JsonTokenType.StartArray)
        {
            throw new JsonException("active_features must be an array");
        }
        var entries = new List<string>();
        while (reader.Read())
        {
            if (reader.TokenType == JsonTokenType.EndArray)
            {
                return new ActiveFeatures { Entries = entries };
            }
            if (reader.TokenType != JsonTokenType.String)
            {
                throw new JsonException("active_features entries must be strings");
            }
            entries.Add(reader.GetString()!);
        }
        throw new JsonException("unterminated active_features array");
    }

    public override void Write(Utf8JsonWriter writer, ActiveFeatures value, JsonSerializerOptions options)
    {
        writer.WriteStartArray();
        foreach (var entry in value.Entries)
        {
            writer.WriteStringValue(entry);
        }
        writer.WriteEndArray();
    }
}

public sealed record ProtocolVersion
{
    [JsonPropertyName("major")]
    public ushort Major { get; init; }

    [JsonPropertyName("minor")]
    public ushort Minor { get; init; }

    public void Validate()
    {
        if (Major == 0)
        {
            throw new AgentContractViolationException("protocol.version.major", "must be non-zero");
        }
    }
}

/// <summary>
/// The exact active feature set; canonical form is sorted, unique. Serde
/// declares this type transparent, so the wire value is the bare array.
/// </summary>
[JsonConverter(typeof(ActiveFeaturesConverter))]
public sealed record ActiveFeatures
{
    public IReadOnlyList<string> Entries { get; init; } = [];

    public static readonly ActiveFeatures Empty = new();

    public void Validate()
    {
        const int MaxFeatures = 32;
        const int MaxNameBytes = 64;
        if (Entries.Count > MaxFeatures)
        {
            throw new AgentContractViolationException(
                "protocol.active_features", $"contains {Entries.Count} entries, above the {MaxFeatures} entry bound");
        }
        for (var i = 0; i < Entries.Count; i++)
        {
            ContractText.ValidateIdentifier("protocol.active_features", Entries[i], MaxNameBytes);
            if (i > 0 && string.CompareOrdinal(Entries[i - 1], Entries[i]) >= 0)
            {
                throw new AgentContractViolationException(
                    "protocol.active_features", "must be strictly sorted and unique");
            }
        }
    }
}

public sealed record ProtocolIdentity
{
    [JsonPropertyName("name")]
    public string Name { get; init; } = string.Empty;

    [JsonPropertyName("version")]
    public ProtocolVersion Version { get; init; } = new();

    [JsonPropertyName("active_features")]
    public ActiveFeatures ActiveFeatures { get; init; } = new();

    [JsonPropertyName("schema_digest")]
    public string SchemaDigest { get; init; } = string.Empty;

    public void Validate()
    {
        ContractText.ValidateIdentifier("protocol.name", Name, 64);
        Version.Validate();
        ActiveFeatures.Validate();
        if (SchemaDigest.Length != 64 || SchemaDigest.Any(c => c is not (>= '0' and <= '9' or >= 'a' and <= 'f')))
        {
            throw new AgentContractViolationException(
                "protocol.schema_digest", "expected exactly 64 lowercase hexadecimal characters");
        }
    }

    /// <summary>The negotiated profile pins the identity exactly.</summary>
    public void ValidateMatches(ProtocolIdentity negotiated)
    {
        Validate();
        if (Name != negotiated.Name
            || Version.Major != negotiated.Version.Major
            || Version.Minor != negotiated.Version.Minor
            || SchemaDigest != negotiated.SchemaDigest
            || !ActiveFeatures.Entries.SequenceEqual(negotiated.ActiveFeatures.Entries))
        {
            throw new AgentContractViolationException(
                "protocol.identity", "does not match the negotiated profile");
        }
    }
}

public sealed record Route
{
    public const string WorkNamespace = "work";
    public const string ApprovalNamespace = "approval";

    public const string WorkSubmit = "submit";
    public const string WorkContinue = "continue";
    public const string WorkCancel = "cancel";
    public const string WorkSnapshot = "snapshot";
    public const string WorkSubscribe = "subscribe";
    public const string WorkEvent = "event";
    // B3 read-only routes (run-scoped; never start a model round).
    public const string WorkTaskDetail = "task_detail";
    /// <summary>EXEC-8 (R2-09): read-only cold lookup of one completed
    /// task's outcome.</summary>
    public const string WorkTaskCompletion = "task_completion";
    public const string WorkChanges = "changes";
    public const string WorkArtifact = "artifact";
    public const string WorkContext = "context";
    // PLATFORM-1 (F06): exact-request submission receipt query (run-scoped,
    // read-only).
    public const string WorkSubmitResult = "submit_result";
    public const string ApprovalRespond = "respond";

    [JsonPropertyName("namespace")]
    public string Namespace { get; init; } = string.Empty;

    [JsonPropertyName("operation")]
    public string Operation { get; init; } = string.Empty;

    public static Route WorkSubmitRoute() => new() { Namespace = WorkNamespace, Operation = WorkSubmit };
    public static Route WorkContinueRoute() => new() { Namespace = WorkNamespace, Operation = WorkContinue };
    public static Route WorkCancelRoute() => new() { Namespace = WorkNamespace, Operation = WorkCancel };
    public static Route WorkSnapshotRoute() => new() { Namespace = WorkNamespace, Operation = WorkSnapshot };
    public static Route WorkSubscribeRoute() => new() { Namespace = WorkNamespace, Operation = WorkSubscribe };
    public static Route WorkTaskDetailRoute() => new() { Namespace = WorkNamespace, Operation = WorkTaskDetail };
    public static Route WorkTaskCompletionRoute() => new() { Namespace = WorkNamespace, Operation = WorkTaskCompletion };
    public static Route WorkChangesRoute() => new() { Namespace = WorkNamespace, Operation = WorkChanges };
    public static Route WorkArtifactRoute() => new() { Namespace = WorkNamespace, Operation = WorkArtifact };
    public static Route WorkContextRoute() => new() { Namespace = WorkNamespace, Operation = WorkContext };
    public static Route WorkSubmitResultRoute() => new() { Namespace = WorkNamespace, Operation = WorkSubmitResult };
    public static Route WorkEventRoute() => new() { Namespace = WorkNamespace, Operation = WorkEvent };
    public static Route ApprovalRespondRoute() => new() { Namespace = ApprovalNamespace, Operation = ApprovalRespond };

    /// <summary>Run-scoped routes carry session-bound identity and no work identity.</summary>
    [JsonIgnore]
    public bool IsRunScoped => Namespace is WorkNamespace or ApprovalNamespace;

    [JsonIgnore]
    public bool IsSupportedSessionRoute =>
        IsRunScoped; // this contract version defines only these run-scoped routes

    public void Validate()
    {
        ContractText.ValidateIdentifier("route.namespace", Namespace, 64);
        ContractText.ValidateIdentifier("route.operation", Operation, 96);
    }
}

public enum EnvelopeKind
{
    Request,
    Response,
    Notification,
    Ping,
    Pong,
}

public sealed record Causality
{
    [JsonPropertyName("correlation_id")]
    public string CorrelationId { get; init; } = string.Empty;

    [JsonPropertyName("causation_id")]
    public string? CausationId { get; init; }

    public void Validate(string messageId)
    {
        ProtocolIds.ValidateCanonical("causality.correlation_id", CorrelationId);
        if (CausationId is null)
        {
            if (CorrelationId != messageId)
            {
                throw new AgentContractViolationException(
                    "causality.correlation_id", "a root message must correlate to its own message id");
            }
        }
        else
        {
            ProtocolIds.ValidateCanonical("causality.causation_id", CausationId);
            if (CausationId == messageId || CorrelationId == messageId)
            {
                throw new AgentContractViolationException(
                    "causality.causation_id", "a caused message cannot cause or root-correlate to itself");
            }
        }
    }
}

/// <summary>
/// Transport-independent envelope. The C# client speaks the run-scoped
/// session routes, which by contract carry no <c>work</c> identity — so the
/// property is deliberately absent here and a peer that adds one fails
/// decoding (deny-unknown-fields parity with Rust).
/// </summary>
public sealed record PlatformEnvelope<T>
{
    [JsonPropertyName("protocol")]
    public ProtocolIdentity Protocol { get; init; } = new();

    [JsonPropertyName("message_id")]
    public string MessageId { get; init; } = string.Empty;

    [JsonPropertyName("request_id")]
    public string? RequestId { get; init; }

    [JsonPropertyName("kind")]
    public EnvelopeKind Kind { get; init; }

    [JsonPropertyName("route")]
    public Route Route { get; init; } = new();

    [JsonPropertyName("causality")]
    public Causality Causality { get; init; } = new();

    [JsonPropertyName("payload")]
    public T Payload { get; init; } = default!;

    public void Validate(ProtocolIdentity negotiated)
    {
        Protocol.ValidateMatches(negotiated);
        ProtocolIds.ValidateCanonical("envelope.message_id", MessageId);
        Route.Validate();
        Causality.Validate(MessageId);

        if (!Route.IsRunScoped)
        {
            throw new AgentContractViolationException(
                "envelope.route", "is not part of the negotiated session route set");
        }
        switch (Kind)
        {
            case EnvelopeKind.Request:
            case EnvelopeKind.Response:
                if (RequestId is null)
                {
                    throw new AgentContractViolationException(
                        "envelope.request_id", "is required for a request/response");
                }
                ProtocolIds.ValidateCanonical("envelope.request_id", RequestId);
                break;
            case EnvelopeKind.Notification:
                if (RequestId is not null)
                {
                    throw new AgentContractViolationException(
                        "envelope.request_id", "must be absent from a notification");
                }
                break;
            default:
                throw new AgentContractViolationException(
                    "envelope.kind", "liveness must use the reserved liveness route");
        }
    }

    /// <summary>Physical pairing: profile, route, request id, one-hop cause.</summary>
    public void ValidateAnswer<TRequest>(PlatformEnvelope<TRequest> request, ProtocolIdentity negotiated)
        where TRequest : notnull
    {
        Validate(negotiated);
        request.Validate(negotiated);
        if (request.Kind != EnvelopeKind.Request || Kind != EnvelopeKind.Response)
        {
            throw new AgentContractViolationException(
                "envelope.kind", "session pairing requires request then response");
        }
        if (request.Protocol.Name != Protocol.Name
            || request.Route.Namespace != Route.Namespace
            || request.Route.Operation != Route.Operation
            || request.RequestId != RequestId
            || request.MessageId == MessageId
            || Causality.CorrelationId != request.Causality.CorrelationId
            || Causality.CausationId != request.MessageId)
        {
            throw new AgentContractViolationException(
                "envelope.pairing", "response does not physically pair with its request");
        }
    }
}

internal static class ContractText
{
    public static void ValidateIdentifier(string field, string value, int maxBytes)
    {
        if (value.Length == 0)
        {
            throw new AgentContractViolationException(field, "must not be empty");
        }
        if (value.Length > maxBytes)
        {
            throw new AgentContractViolationException(
                field, $"is {value.Length} bytes, above the {maxBytes} byte bound");
        }
        if (!char.IsAsciiLetterLower(value[0]))
        {
            throw new AgentContractViolationException(
                field, "must start with a lowercase ASCII letter");
        }
        foreach (var c in value)
        {
            if (!(char.IsAsciiLetterLower(c) || char.IsAsciiDigit(c) || c is '.' or '_' or '-'))
            {
                throw new AgentContractViolationException(
                    field, "must contain only lowercase ASCII letters, digits, '.', '_' or '-'");
            }
        }
    }

    public static void ValidateOpaque(string field, string value, int maxBytes)
    {
        if (value.Length == 0)
        {
            throw new AgentContractViolationException(field, "must not be empty");
        }
        if (value.Length > maxBytes)
        {
            throw new AgentContractViolationException(
                field, $"is {value.Length} bytes, above the {maxBytes} byte bound");
        }
        if (value.Any(char.IsControl))
        {
            throw new AgentContractViolationException(field, "must not contain control characters");
        }
    }

    /// Byte backstop mirroring the Rust <c>MAX_WORK_GOAL_BYTES</c> (the
    /// runtime's input cap) — the authoritative bound both languages agree
    /// on, because UTF-8 byte counts are language-independent.
    public const int MaxUserInputBytes = 262_144;

    public static void ValidateText(string field, string value, int maxChars)
    {
        if (value.Length == 0)
        {
            throw new AgentContractViolationException(field, "must not be empty");
        }
        if (value.Length > maxChars)
        {
            throw new AgentContractViolationException(
                field, $"is {value.Length} chars, above the {maxChars} char bound");
        }
        if (System.Text.Encoding.UTF8.GetByteCount(value) > MaxUserInputBytes)
        {
            throw new AgentContractViolationException(
                field,
                $"is {System.Text.Encoding.UTF8.GetByteCount(value)} bytes, "
                    + $"above the {MaxUserInputBytes} byte bound");
        }
        if (value.Any(c => char.IsControl(c) && c is not ('\n' or '\r' or '\t')))
        {
            throw new AgentContractViolationException(
                field, "must not contain control characters (LF/CR/TAB are allowed)");
        }
    }

    public static void ValidateTaskId(string field, string value)
    {
        ProtocolIds.ValidateCanonical(field, value);
    }
}

/// <summary>Envelope builders enforcing the run-scoped identity discipline.</summary>
public static class SessionEnvelope
{
    public static PlatformEnvelope<TPayload> Request<TPayload>(Route route, TPayload payload)
        where TPayload : notnull
    {
        var messageId = ProtocolIds.NewMessageId();
        return new PlatformEnvelope<TPayload>
        {
            Protocol = AgentConnection.DefaultProtocolIdentity,
            MessageId = messageId,
            RequestId = ProtocolIds.NewMessageId(),
            Kind = EnvelopeKind.Request,
            Route = route,
            Causality = new Causality { CorrelationId = messageId },
            Payload = payload,
        };
    }

    public static PlatformEnvelope<PlatformResponse<TResponse>> Response<TRequest, TResponse>(
        PlatformEnvelope<TRequest> request, TResponse value)
        where TRequest : notnull
        where TResponse : notnull
    {
        return Response(request, new PlatformResponse<TResponse> { Status = "success", Value = value });
    }

    public static PlatformEnvelope<PlatformResponse<TResponse>> ResponseError<TRequest, TResponse>(
        PlatformEnvelope<TRequest> request, PlatformError error)
        where TRequest : notnull
        where TResponse : notnull
    {
        return Response(request, new PlatformResponse<TResponse> { Status = "error", Error = error });
    }

    private static PlatformEnvelope<PlatformResponse<TResponse>> Response<TRequest, TResponse>(
        PlatformEnvelope<TRequest> request, PlatformResponse<TResponse> payload)
        where TRequest : notnull
        where TResponse : notnull
    {
        return new PlatformEnvelope<PlatformResponse<TResponse>>
        {
            Protocol = request.Protocol,
            MessageId = ProtocolIds.NewMessageId(),
            RequestId = request.RequestId,
            Kind = EnvelopeKind.Response,
            Route = request.Route,
            Causality = new Causality
            {
                CorrelationId = request.Causality.CorrelationId,
                CausationId = request.MessageId,
            },
            Payload = payload,
        };
    }
}
