using System.Text.Encodings.Web;
using System.Text.Json;
using System.Text.Json.Serialization;

namespace FocusAgent.Client;

/// <summary>Shared JSON conventions mirroring the Rust serde contract.</summary>
/// <remarks>
/// Mirrors, exactly: snake_case property names via explicit
/// <see cref="JsonPropertyName"/> attributes (serde's rename_all on fields),
/// snake_case enum values via <see cref="JsonNamingPolicy.SnakeCaseLower"/>,
/// unknown-field rejection via <see cref="JsonUnmappedMemberHandling.Disallow"/>
/// (serde deny_unknown_fields), and null-skipped optionals via
/// <see cref="JsonIgnoreCondition.WhenWritingNull"/>. The one deliberate
/// divergence: serde emits variant names verbatim for enums without
/// rename_all, which only <see cref="ApprovalDecision"/> does — it keeps its
/// own non-snake converter.
/// </remarks>
public static class AgentJson
{
    public static JsonSerializerOptions Options { get; } = Create(writeIndented: false);

    private static JsonSerializerOptions Create(bool writeIndented)
    {
        var options = new JsonSerializerOptions
        {
            WriteIndented = writeIndented,
            PropertyNamingPolicy = JsonNamingPolicy.SnakeCaseLower,
            DictionaryKeyPolicy = JsonNamingPolicy.SnakeCaseLower,
            UnmappedMemberHandling = JsonUnmappedMemberHandling.Disallow,
            DefaultIgnoreCondition = JsonIgnoreCondition.WhenWritingNull,
            Encoder = JavaScriptEncoder.UnsafeRelaxedJsonEscaping,
            Converters =
            {
                // Order matters: this specific converter must precede the
                // general snake_case enum converter, which would otherwise
                // claim ApprovalDecision and emit "allow" instead of "Allow".
                new ApprovalDecisionConverter(),
                new PlatformResponseConverterFactory(),
                new JsonStringEnumConverter(JsonNamingPolicy.SnakeCaseLower),
            },
        };
        return options;
    }
}

/// <summary>
/// Rust's <c>ApprovalDecision</c> derives serde without rename_all, so the
/// wire values are the verbatim variant names "Allow"/"Deny", not snake_case.
/// </summary>
public sealed class ApprovalDecisionConverter : JsonConverter<ApprovalDecision>
{
    public override ApprovalDecision Read(ref Utf8JsonReader reader, Type typeToConvert, JsonSerializerOptions options)
    {
        var value = reader.GetString();
        return value switch
        {
            "Allow" => ApprovalDecision.Allow,
            "Deny" => ApprovalDecision.Deny,
            _ => throw new JsonException($"unknown approval decision '{value}'"),
        };
    }

    public override void Write(Utf8JsonWriter writer, ApprovalDecision value, JsonSerializerOptions options)
    {
        writer.WriteStringValue(value switch
        {
            ApprovalDecision.Allow => "Allow",
            ApprovalDecision.Deny => "Deny",
            _ => throw new JsonException($"unknown approval decision '{value}'"),
        });
    }
}
