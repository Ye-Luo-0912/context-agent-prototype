using System.Text.Json;

using Xunit;

namespace FocusAgent.Client.Tests;

/// <summary>
/// C0 cross-language conformance: the JSON fixture files live with the Rust
/// protocol crate and both suites decode, validate, and re-encode the same
/// physical bytes. A shape drift fails here and in
/// crates/agent-platform-protocol/tests/work_fixtures.rs at the same time.
/// </summary>
public class FixtureConformanceTests
{
    private static string FixturesDir() => ContractFixtures.Dir();

    /// <summary>The negotiated identity the fixtures were written against.</summary>
    private static readonly ProtocolIdentity FixtureIdentity = new()
    {
        Name = "focus-agent.platform",
        Version = new ProtocolVersion { Major = 1, Minor = 0 },
        ActiveFeatures = new ActiveFeatures(),
        SchemaDigest = new string('1', 64),
    };

    private static string Read(string name) =>
        File.ReadAllText(Path.Combine(FixturesDir(), name)).TrimEnd('\r', '\n');

    private static void AssertRoundTripByteIdentical<T>(string text, string name)
    {
        var decoded = JsonSerializer.Deserialize<T>(text, AgentJson.Options);
        Assert.NotNull(decoded);
        var encoded = JsonSerializer.Serialize(decoded, AgentJson.Options);
        Assert.True(encoded == text,
            $"{name} must round-trip byte-identically.{Environment.NewLine}expected: {text}{Environment.NewLine}actual:   {encoded}");
    }

    [Fact]
    public void Submit_fixture_pair_means_acceptance_with_task_binding()
    {
        var requestText = Read("submit_request.json");
        var responseText = Read("submit_response.json");
        AssertRoundTripByteIdentical<PlatformEnvelope<WorkSubmitRequest>>(requestText, "submit_request.json");
        AssertRoundTripByteIdentical<PlatformEnvelope<PlatformResponse<WorkSubmitResponse>>>(
            responseText, "submit_response.json");

        var request = JsonSerializer.Deserialize<PlatformEnvelope<WorkSubmitRequest>>(requestText, AgentJson.Options)!;
        var response = JsonSerializer.Deserialize<PlatformEnvelope<PlatformResponse<WorkSubmitResponse>>>(
            responseText, AgentJson.Options)!;
        request.Validate(FixtureIdentity);
        response.ValidateAnswer(request, FixtureIdentity);
        response.Payload.Validate();

        Assert.Equal("migrate the retry table", request.Payload.Goal);
        Assert.Equal("client-1", request.Payload.ClientRequestId);
        Assert.True(response.Payload.IsSuccess);
        Assert.Equal(WorkSubmitDisposition.Accepted, response.Payload.ExpectValue().Disposition);
        Assert.NotEqual(Guid.Empty, Guid.Parse(response.Payload.ExpectValue().TaskId));
    }

    [Fact]
    public void Error_fixture_carries_the_structured_conflict_fact()
    {
        var requestText = Read("submit_request.json");
        var responseText = Read("error_response.json");
        var request = JsonSerializer.Deserialize<PlatformEnvelope<WorkSubmitRequest>>(requestText, AgentJson.Options)!;
        var response = JsonSerializer.Deserialize<PlatformEnvelope<PlatformResponse<WorkSubmitResponse>>>(
            responseText, AgentJson.Options)!;
        response.ValidateAnswer(request, FixtureIdentity);
        response.Payload.Validate();

        Assert.False(response.Payload.IsSuccess);
        var failure = Assert.Throws<AgentProtocolException>(() => response.Payload.ExpectValue());
        Assert.Equal("work.goal_conflict", failure.Error.Code);
        Assert.Equal(RetryDisposition.Never, failure.Error.Retry);
    }

    [Fact]
    public void Cancel_fixture_reports_core_truth_not_optimism()
    {
        var requestText = Read("cancel_request.json");
        var responseText = Read("cancel_response.json");
        AssertRoundTripByteIdentical<PlatformEnvelope<WorkCancelRequest>>(requestText, "cancel_request.json");
        AssertRoundTripByteIdentical<PlatformEnvelope<PlatformResponse<WorkCancelResponse>>>(
            responseText, "cancel_response.json");

        var response = JsonSerializer.Deserialize<PlatformEnvelope<PlatformResponse<WorkCancelResponse>>>(
            responseText, AgentJson.Options)!;
        response.Payload.Validate();
        var ack = response.Payload.ExpectValue().Ack;
        Assert.Equal(TurnCancelAckStatus.Cancelled, ack.Status);
        Assert.Equal(3ul, ack.CancelledGeneration);
        Assert.True(ack.EffectiveGeneration >= ack.CancelledGeneration);
    }

    [Fact]
    public void Snapshot_fixture_is_bounded_typed_and_watermarked()
    {
        var responseText = Read("snapshot_response.json");
        AssertRoundTripByteIdentical<PlatformEnvelope<PlatformResponse<WorkSnapshotResponse>>>(
            responseText, "snapshot_response.json");
        var response = JsonSerializer.Deserialize<PlatformEnvelope<PlatformResponse<WorkSnapshotResponse>>>(
            responseText, AgentJson.Options)!;
        response.Payload.Validate();
        var snapshot = response.Payload.ExpectValue();

        Assert.True(snapshot.RunStarted);
        Assert.False(snapshot.RunCompleted);
        Assert.Equal(41ul, snapshot.Watermark);
        Assert.Equal(1ul, snapshot.Focus!.AnchorRevision);
        Assert.Single(snapshot.Tasks);
        Assert.Equal(TaskSnapshotStatus.Active, snapshot.Tasks[0].Status);
        Assert.Single(snapshot.PendingApprovals);
        Assert.Equal("fs.write", snapshot.PendingApprovals[0].CallName);
        // F12: the snapshot carries the gate's own risk plus a bounded
        // target summary — informed approval, not a bare request id.
        Assert.Equal(ApprovalRisk.WorkspaceWrite, snapshot.PendingApprovals[0].Risk);
        Assert.Equal("docs/plan.md", snapshot.PendingApprovals[0].TargetSummary);
        snapshot.PendingApprovals[0].Validate();
        Assert.False(snapshot.ResyncRequired);
    }

    /// <summary>The endpoint suffix rule is pinned cross-language: the host
    /// binds by it and the desktop derives the same default endpoint from
    /// the same bytes.</summary>
    [Fact]
    public void Endpoint_derivation_fixture_pins_the_shared_suffix_rule()
    {
        var value = JsonSerializer.Deserialize<JsonElement>(Read("endpoint_derivation.json"), AgentJson.Options);
        var root = value.GetProperty("workspace_root").GetString()!;
        var expected = value.GetProperty("endpoint_suffix").GetString()!;
        Assert.Equal(expected, AgentTransports.WorkspaceEndpointSuffix(root));
    }

    /// <summary>F12: an approval's target summary is a bounded display
    /// projection — oversized or control-bearing summaries fail validation,
    /// and a snapshot whose approval omits the risk fact fails decode.</summary>
    [Fact]
    public void Approval_snapshot_details_fail_closed()
    {
        var responseText = Read("snapshot_response.json");
        var response = JsonSerializer.Deserialize<PlatformEnvelope<PlatformResponse<WorkSnapshotResponse>>>(
            responseText, AgentJson.Options)!;
        var snapshot = response.Payload.ExpectValue();

        var oversized = snapshot.PendingApprovals[0] with { TargetSummary = new string('x', PendingApprovalSnapshot.MaxTargetSummaryChars + 1) };
        Assert.Throws<AgentContractViolationException>(() => oversized.Validate());

        var control = snapshot.PendingApprovals[0] with { TargetSummary = "bad\u0001path" };
        Assert.Throws<AgentContractViolationException>(() => control.Validate());

        var missingRisk = responseText.Replace(
            "\"risk\":\"workspace_write\",",
            "",
            StringComparison.Ordinal);
        Assert.Throws<JsonException>(() =>
            JsonSerializer.Deserialize<PlatformEnvelope<PlatformResponse<WorkSnapshotResponse>>>(
                missingRisk, AgentJson.Options));
    }

    [Fact]
    public void Approval_fixture_decision_is_the_exact_serde_variant_name()
    {
        var requestText = Read("approval_respond_request.json");
        // ApprovalDecision keeps serde's verbatim variant names: "Allow"/"Deny".
        Assert.Contains("\"decision\":\"Allow\"", requestText, StringComparison.Ordinal);
        AssertRoundTripByteIdentical<PlatformEnvelope<ApprovalRespondRequest>>(
            requestText, "approval_respond_request.json");

        var request = JsonSerializer.Deserialize<PlatformEnvelope<ApprovalRespondRequest>>(
            requestText, AgentJson.Options)!;
        Assert.Equal(ApprovalDecision.Allow, request.Payload.Decision);

        var deny = JsonSerializer.Deserialize<PlatformEnvelope<ApprovalRespondRequest>>(
            requestText.Replace("\"Allow\"", "\"Deny\"", StringComparison.Ordinal), AgentJson.Options)!;
        Assert.Equal(ApprovalDecision.Deny, deny.Payload.Decision);

        var lateText = Read("approval_respond_response.json")
            .Replace("\"delivered\"", "\"no_longer_pending\"", StringComparison.Ordinal);
        var late = JsonSerializer.Deserialize<PlatformEnvelope<PlatformResponse<ApprovalRespondResponse>>>(
            lateText, AgentJson.Options)!;
        Assert.Equal(ApprovalRespondOutcome.NoLongerPending, late.Payload.ExpectValue().Outcome);
    }

    [Fact]
    public void Subscribe_fixture_starts_the_stream_at_the_watermark()
    {
        var requestText = Read("subscribe_request.json");
        AssertRoundTripByteIdentical<PlatformEnvelope<WorkSubscribeRequest>>(requestText, "subscribe_request.json");
        var request = JsonSerializer.Deserialize<PlatformEnvelope<WorkSubscribeRequest>>(
            requestText, AgentJson.Options)!;
        Assert.Equal(17ul, request.Payload.ReplayAfterSeq);
    }

    [Fact]
    public void Unknown_envelope_fields_fail_decode_like_deny_unknown_fields()
    {
        var drifted = Read("submit_request.json").Replace(
            "\"causality\":",
            "\"future_field\":true,\"causality\":",
            StringComparison.Ordinal);
        Assert.Throws<JsonException>(() =>
            JsonSerializer.Deserialize<PlatformEnvelope<WorkSubmitRequest>>(drifted, AgentJson.Options));
    }

    [Fact]
    public void Work_identity_on_a_run_scoped_envelope_fails_decode()
    {
        var drifted = Read("submit_request.json").Replace(
            "\"causality\":",
            "\"work\":{\"run_id\":\"00000000-0000-4000-8000-000000000021\",\"operation_id\":\"00000000-0000-4000-8000-000000000025\",\"generation\":1,\"attempt\":1,\"argument_digest\":\"" + new string('2', 32) + "\",\"deadline_remaining_ms\":1000},\"causality\":",
            StringComparison.Ordinal);
        // The C# envelope has no work member: an injected one is an unknown
        // field and must fail exactly like Rust's validator-side rejection.
        Assert.Throws<JsonException>(() =>
            JsonSerializer.Deserialize<PlatformEnvelope<WorkSubmitRequest>>(drifted, AgentJson.Options));
    }

    [Fact]
    public void Every_fixture_round_trips_byte_identically()
    {
        foreach (var file in Directory.GetFiles(FixturesDir(), "*.json"))
        {
            var name = Path.GetFileName(file);
            var text = File.ReadAllText(file).TrimEnd('\r', '\n');
            var element = JsonSerializer.Deserialize<JsonElement>(text, AgentJson.Options);
            var encoded = JsonSerializer.Serialize(element, AgentJson.Options);
            Assert.True(encoded == text, $"{name} must round-trip byte-identically");
        }
    }

    // -----------------------------------------------------------------------
    // B3 read-only routes: wire shapes pinned independently of the Rust
    // fixture set (the fixtures date from before these routes existed).
    // -----------------------------------------------------------------------

    [Fact]
    public void Task_detail_response_uses_snake_case_anchor_wire_shape()
    {
        const string text = """
            {"task_id":"00000000-0000-4000-8000-000000000022","goal":"migrate","status":"active","anchor_revision":1,
             "anchor":{"revision":1,"original_goal":"migrate","current_interpretation":"interpreted","constraints":["bounded"],"acceptance_criteria":["green"],"plan_progress":["step 1"],"open_loops":["verify"],"next_action":"continue"}}
            """;
        var compact = JsonSerializer.Serialize(
            JsonSerializer.Deserialize<JsonElement>(text, AgentJson.Options), AgentJson.Options);
        var decoded = JsonSerializer.Deserialize<WorkTaskDetailResponse>(compact, AgentJson.Options)!;
        decoded.Validate();
        Assert.Equal("migrate", decoded.Anchor.OriginalGoal);
        Assert.Equal(TaskSnapshotStatus.Active, decoded.Status);
        Assert.Single(decoded.Anchor.OpenLoops);

        var reencoded = JsonSerializer.Serialize(decoded, AgentJson.Options);
        Assert.True(reencoded == compact,
            $"task detail must round-trip byte-identically.{Environment.NewLine}expected: {compact}{Environment.NewLine}actual:   {reencoded}");

        var emptyGoal = decoded with { Goal = "" };
        Assert.Throws<AgentContractViolationException>(() => emptyGoal.Validate());
    }

    [Fact]
    public void Change_summary_pins_the_kind_tag_and_rejects_old_content()
    {
        var prepared = new ChangeSummary
        {
            Kind = ChangeSummaryKind.MutationPrepared,
            TxId = "tx-1",
            TimestampMs = 30,
            Tool = "fs.write",
            Path = "docs/plan.md",
            Action = "overwrite",
            BytesBefore = 10,
            BytesAfter = 20,
            BeforeHash = "a1",
            AfterHash = "b2",
        };
        var wire = JsonSerializer.Serialize(prepared, AgentJson.Options);
        Assert.Contains("\"kind\":\"mutation_prepared\"", wire, StringComparison.Ordinal);
        Assert.DoesNotContain("old_content", wire, StringComparison.Ordinal);
        var decoded = JsonSerializer.Deserialize<ChangeSummary>(wire, AgentJson.Options)!;
        Assert.Equal(ChangeSummaryKind.MutationPrepared, decoded.Kind);
        Assert.Equal("docs/plan.md", decoded.Path);
        Assert.True(
            JsonSerializer.Serialize(decoded, AgentJson.Options) == wire,
            $"change summary must round-trip byte-identically.{Environment.NewLine}expected: {wire}{Environment.NewLine}actual:   {JsonSerializer.Serialize(decoded, AgentJson.Options)}");

        // The journal's internal old-content capture must never be accepted
        // from the wire: an unknown field on the variant is a decode failure.
        var forged = wire.Replace("\"after_hash\":\"b2\"", "\"after_hash\":\"b2\",\"old_content\":\"stale\"", StringComparison.Ordinal);
        Assert.Throws<JsonException>(() => JsonSerializer.Deserialize<ChangeSummary>(forged, AgentJson.Options));

        // Rolled-back variant carries its own field set.
        var rolled = new ChangeSummary { Kind = ChangeSummaryKind.MutationRolledBack, TxId = "tx-2", TimestampMs = 5, Reason = "conflict" };
        var rolledWire = JsonSerializer.Serialize(rolled, AgentJson.Options);
        Assert.Contains("\"kind\":\"mutation_rolled_back\"", rolledWire, StringComparison.Ordinal);
        Assert.Equal("conflict", JsonSerializer.Deserialize<ChangeSummary>(rolledWire, AgentJson.Options)!.Reason);
    }

    [Fact]
    public void Artifact_response_validates_truncation_truth_and_base64()
    {
        var body = System.Text.Encoding.UTF8.GetBytes("b3 artifact body");
        var base64 = Convert.ToBase64String(body);
        var full = new WorkArtifactResponse
        {
            Reference = "artifact://.focus-agent/artifacts/r/proof/aa",
            SizeBytes = (ulong)body.LongLength,
            Truncated = false,
            ContentBase64 = base64,
        };
        full.Validate();
        var reencoded = JsonSerializer.Deserialize<WorkArtifactResponse>(
            JsonSerializer.Serialize(full, AgentJson.Options), AgentJson.Options)!;
        reencoded.Validate();

        var truncated = full with { SizeBytes = 100, Truncated = true };
        truncated.Validate();

        var lies = full with { SizeBytes = 32, Truncated = false };
        Assert.Throws<AgentContractViolationException>(() => lies.Validate());

        var garbage = full with { ContentBase64 = "not base64!" };
        Assert.Throws<AgentContractViolationException>(() => garbage.Validate());
    }

    [Fact]
    public void Context_listing_mirrors_the_item_summary_elements()
    {
        const string wire = """
            {"items":[{"id":"9f823fc0-8ed4-4d18-a02d-2d75691f7f01","kind":"Goal","scope":"Task","attention":"Active","semantic":"Live",
             "importance":0.5,"relevance":0.3,"created_tick":1,"created_turn":1,"last_access_turn":1,"access_count":1,"dependencies":[],"keep_alive":false}]}
            """;
        var response = JsonSerializer.Deserialize<WorkContextResponse>(
            JsonSerializer.Serialize(JsonSerializer.Deserialize<JsonElement>(wire, AgentJson.Options), AgentJson.Options),
            AgentJson.Options)!;
        response.Validate();
        var item = Assert.Single(response.Items);
        Assert.Equal("9f823fc0-8ed4-4d18-a02d-2d75691f7f01", item.Id);
        Assert.Equal("Goal", item.Kind.GetString());
        Assert.Equal("Live", item.Semantic.GetString());

        // The bound must hold client-side too: a listing over the cap fails.
        var over = response with { Items = Enumerable.Repeat(item, WorkContextRequest.MaxContextItems + 1).ToArray() };
        Assert.Throws<AgentContractViolationException>(() => over.Validate());
    }
}
