using System.Text.Json;
using System.Text.Json.Serialization;

namespace FocusAgent.Client;

/// <summary>Raised when the host answers a request with a structured error.</summary>
public sealed class AgentProtocolException : Exception
{
    public PlatformError Error { get; }

    public AgentProtocolException(PlatformError error)
        : base($"{error.Class}:{error.Code}: {error.Message}")
    {
        Error = error;
    }
}

/// <summary>Raised when a frame or envelope violates the bounded contract.</summary>
public sealed class AgentContractViolationException : Exception
{
    public string Field { get; }

    public AgentContractViolationException(string field, string reason)
        : base($"invalid {field}: {reason}")
    {
        Field = field;
    }
}

public enum ProtocolErrorClass
{
    Protocol,
    Domain,
}

public enum RetryDisposition
{
    Never,
    SameOperation,
    QueryBeforeRetry,
}

public enum EffectStateDisposition
{
    NotApplicable,
    NotApplied,
    Applied,
    OutcomeUnknown,
}

/// <summary>Structured protocol/domain error mirroring the Rust algebra.</summary>
public sealed record PlatformError
{
    [JsonPropertyName("class")]
    public ProtocolErrorClass Class { get; init; }

    [JsonPropertyName("code")]
    public string Code { get; init; } = string.Empty;

    [JsonPropertyName("message")]
    public string Message { get; init; } = string.Empty;

    [JsonPropertyName("retry")]
    public RetryDisposition Retry { get; init; }

    [JsonPropertyName("effect_state")]
    public EffectStateDisposition EffectState { get; init; }

    [JsonPropertyName("retry_after_ms")]
    public uint? RetryAfterMs { get; init; }

    [JsonPropertyName("diagnostic_ref")]
    public string? DiagnosticRef { get; init; }

    public void Validate()
    {
        const int MaxCodeBytes = 96;
        const int MaxMessageBytes = 4_000;
        const int MaxDiagnosticRefBytes = 512;
        if (Code is [] or { Length: > MaxCodeBytes })
        {
            throw new AgentContractViolationException("error.code", "length out of contract bounds");
        }
        if (Message is [] or { Length: > MaxMessageBytes })
        {
            throw new AgentContractViolationException("error.message", "length out of contract bounds");
        }
        if (DiagnosticRef is { Length: > MaxDiagnosticRefBytes })
        {
            throw new AgentContractViolationException("error.diagnostic_ref", "length out of contract bounds");
        }

        var legal = (Retry, EffectState) switch
        {
            (RetryDisposition.SameOperation, EffectStateDisposition.NotApplicable) => true,
            (RetryDisposition.SameOperation, EffectStateDisposition.NotApplied) => true,
            (RetryDisposition.QueryBeforeRetry, EffectStateDisposition.OutcomeUnknown) => true,
            (RetryDisposition.Never, EffectStateDisposition.NotApplicable) => true,
            (RetryDisposition.Never, EffectStateDisposition.NotApplied) => true,
            (RetryDisposition.Never, EffectStateDisposition.Applied) => true,
            (RetryDisposition.Never, EffectStateDisposition.OutcomeUnknown) => true,
            _ => false,
        };
        if (!legal)
        {
            throw new AgentContractViolationException(
                "error.disposition", "retry and effect-state dispositions form an illegal combination");
        }
        if (RetryAfterMs is not null && Retry != RetryDisposition.SameOperation)
        {
            throw new AgentContractViolationException(
                "error.retry_after_ms", "is allowed only for same-operation retry");
        }
    }
}

/// <summary>
/// One explicit success/error response body. The wire shape is
/// <c>{"status":"success","value":...}</c> or
/// <c>{"status":"error","error":{...}}</c>.
/// </summary>
public sealed record PlatformResponse<T>
{
    [JsonPropertyName("status")]
    public string Status { get; init; } = string.Empty;

    [JsonPropertyName("value")]
    public T? Value { get; init; }

    [JsonPropertyName("error")]
    public PlatformError? Error { get; init; }

    public bool IsSuccess => Status == "success";

    public void Validate()
    {
        switch (Status)
        {
            case "success" when Value is not null:
            case "error" when Error is not null:
                break;
            default:
                throw new AgentContractViolationException(
                    "platform.response", "exactly one of value/error must match the status tag");
        }
        Error?.Validate();
    }

    /// <summary>The success value, or throws a structured protocol exception.</summary>
    public T ExpectValue()
    {
        if (!IsSuccess || Value is null)
        {
            throw new AgentProtocolException(
                Error ?? new PlatformError
                {
                    Class = ProtocolErrorClass.Protocol,
                    Code = "protocol.response_lost",
                    Message = "response carried no success value",
                    Retry = RetryDisposition.QueryBeforeRetry,
                    EffectState = EffectStateDisposition.OutcomeUnknown,
                });
        }
        return Value;
    }
}

/// <summary>Converts the status-tagged response shape for any payload type.</summary>
public sealed class PlatformResponseConverterFactory : JsonConverterFactory
{
    public override bool CanConvert(Type typeToConvert) =>
        typeToConvert.IsGenericType && typeToConvert.GetGenericTypeDefinition() == typeof(PlatformResponse<>);

    public override JsonConverter? CreateConverter(Type typeToConvert, JsonSerializerOptions options)
    {
        var payload = typeToConvert.GetGenericArguments()[0];
        var converterType = typeof(PlatformResponseConverter<>).MakeGenericType(payload);
        return (JsonConverter)Activator.CreateInstance(converterType)!;
    }
}

public sealed class PlatformResponseConverter<T> : JsonConverter<PlatformResponse<T>>
{
    public override PlatformResponse<T> Read(ref Utf8JsonReader reader, Type typeToConvert, JsonSerializerOptions options)
    {
        using var doc = JsonDocument.ParseValue(ref reader);
        var root = doc.RootElement;
        if (root.ValueKind != JsonValueKind.Object
            || !root.TryGetProperty("status", out var status))
        {
            throw new JsonException("platform response must be a status-tagged object");
        }
        var statusText = status.GetString();
        if (statusText == "success")
        {
            if (!root.TryGetProperty("value", out var valueElement))
            {
                throw new JsonException("success response requires value");
            }
            if (root.EnumerateObject().Any(p => p.Name is not ("status" or "value")))
            {
                throw new JsonException("success response carries unknown fields");
            }
            var value = valueElement.Deserialize<T>(options);
            return new PlatformResponse<T> { Status = "success", Value = value };
        }
        if (statusText == "error")
        {
            if (!root.TryGetProperty("error", out var errorElement))
            {
                throw new JsonException("error response requires error");
            }
            if (root.EnumerateObject().Any(p => p.Name is not ("status" or "error")))
            {
                throw new JsonException("error response carries unknown fields");
            }
            var error = errorElement.Deserialize<PlatformError>(options)
                ?? throw new JsonException("error response requires error");
            return new PlatformResponse<T> { Status = "error", Error = error };
        }
        throw new JsonException($"unknown platform response status '{statusText}'");
    }

    public override void Write(Utf8JsonWriter writer, PlatformResponse<T> value, JsonSerializerOptions options)
    {
        writer.WriteStartObject();
        writer.WriteString("status", value.Status);
        if (value.Status == "success")
        {
            writer.WritePropertyName("value");
            JsonSerializer.Serialize(writer, value.Value, options);
        }
        else if (value.Status == "error")
        {
            writer.WritePropertyName("error");
            JsonSerializer.Serialize(writer, value.Error, options);
        }
        else
        {
            throw new JsonException($"unknown platform response status '{value.Status}'");
        }
        writer.WriteEndObject();
    }
}
