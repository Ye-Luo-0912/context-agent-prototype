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
        // PLATFORM-3: the snapshot names the run and workspace that produced
        // it — a reconnecting client can tell whose facts it is reading.
        Assert.Equal("00000000-0000-4000-8000-000000000002", snapshot.RunId);
        Assert.Equal("/workspaces/alpha", snapshot.WorkspaceRoot);
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
        // EXEC-6 (R2-02): the restore-evidence degradation is a typed,
        // re-obtainable snapshot fact. The shared fixture carries an empty
        // list (nothing degraded); a populated list survives the round trip
        // and an over-cap list fails validation closed.
        Assert.Empty(snapshot.RestoreEvidenceDegraded);
        var degraded = snapshot with
        {
            RestoreEvidenceDegraded = Enumerable.Range(0, WorkSnapshotResponse.MaxDegradedRuns + 1)
                .Select(_ => "00000000-0000-4000-8000-000000000009")
                .ToList(),
        };
        Assert.Throws<AgentContractViolationException>(() => degraded.Validate());
        var bounded = snapshot with
        {
            RestoreEvidenceDegraded = ["00000000-0000-4000-8000-000000000009"],
        };
        bounded.Validate();
        Assert.Single(bounded.RestoreEvidenceDegraded);
    }

    [Fact]
    public void Task_completion_fixture_pins_the_retired_fact_cross_language()
    {
        // EXEC-8 (R2-09): a retired completion reads back from the durable
        // journal as typed evidence; the shared fixture round-trips
        // byte-identically so a .NET client decodes exactly what the
        // runtime serialized.
        var responseText = Read("task_completion_response.json");
        AssertRoundTripByteIdentical<PlatformEnvelope<PlatformResponse<WorkTaskCompletionResponse>>>(
            responseText, "task_completion_response.json");
        var response = JsonSerializer.Deserialize<PlatformEnvelope<PlatformResponse<WorkTaskCompletionResponse>>>(
            responseText, AgentJson.Options)!;
        response.Payload.Validate();
        var value = response.Payload.ExpectValue();
        Assert.Equal("00000000-0000-4000-8000-000000000033", value.TaskId);
        var retired = Assert.IsType<WorkCompletionFactRetired>(value.Fact);
        Assert.Equal("migrated the retry table", retired.Summary);
        Assert.Equal(3ul, retired.AnchorRevision);
        var artifact = Assert.Single(retired.Artifacts);
        Assert.StartsWith("artifact://v1/", artifact);
        Assert.Equal(new string('a', 64), retired.FinalOutputDigest);
        // A beyond-window fact is an honestly bounded unknown, and an
        // over-cap retired fact fails validation closed.
        new WorkCompletionFactBeyondJournalWindow().Validate();
        var oversized = retired with
        {
            Artifacts = Enumerable.Range(0, WorkCompletionFactRetired.MaxArtifacts + 1)
                .Select(_ => "artifact://v1/x/y/z").ToList(),
        };
        Assert.Throws<AgentContractViolationException>(() => oversized.Validate());
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
            OldContentArtifact = "artifact://run/changes/tx-1-before",
        };
        var wire = JsonSerializer.Serialize(prepared, AgentJson.Options);
        Assert.Contains("\"kind\":\"mutation_prepared\"", wire, StringComparison.Ordinal);
        // PLATFORM-2: the located reference travels; the journal's internal
        // raw `old_content` body never does.
        Assert.Contains("\"old_content_artifact\":", wire, StringComparison.Ordinal);
        Assert.DoesNotContain("\"old_content\":", wire, StringComparison.Ordinal);
        var decoded = JsonSerializer.Deserialize<ChangeSummary>(wire, AgentJson.Options)!;
        Assert.Equal(ChangeSummaryKind.MutationPrepared, decoded.Kind);
        Assert.Equal("docs/plan.md", decoded.Path);
        Assert.Equal("artifact://run/changes/tx-1-before", decoded.OldContentArtifact);
        Assert.True(
            JsonSerializer.Serialize(decoded, AgentJson.Options) == wire,
            $"change summary must round-trip byte-identically.{Environment.NewLine}expected: {wire}{Environment.NewLine}actual:   {JsonSerializer.Serialize(decoded, AgentJson.Options)}");

        // Without a located reference the field is absent from the wire, and
        // the raw `old_content` capture stays unacceptable in every shape.
        var captureless = prepared with { OldContentArtifact = null };
        var capturelessWire = JsonSerializer.Serialize(captureless, AgentJson.Options);
        Assert.DoesNotContain("old_content", capturelessWire, StringComparison.Ordinal);
        var forgedCapture = capturelessWire.Replace(
            "\"after_hash\":\"b2\"",
            "\"after_hash\":\"b2\",\"old_content\":\"stale\"",
            StringComparison.Ordinal);
        Assert.Throws<JsonException>(() => JsonSerializer.Deserialize<ChangeSummary>(forgedCapture, AgentJson.Options));

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

        var truncated = full with { SizeBytes = 100, Truncated = true, NextOffset = (ulong)body.LongLength };
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

    /// <summary>R06: the snapshot and task-detail goal bounds must match the
    /// Rust side (200_000), not the old 2_000 — a legal long goal accepted
    /// by submit would fault every snapshot/reconnect otherwise. 2,001 chars
    /// (over the old bound) must validate; over the real bound must not.</summary>
    [Fact]
    public void Snapshot_and_task_detail_accept_a_long_legal_goal_like_the_host_does()
    {
        var longGoal = new string('g', 2_001);
        Assert.True(longGoal.Length > 2_000, "the regression must exceed the old broken bound");
        Assert.True(longGoal.Length <= WorkSnapshotResponse.MaxGoalChars);

        var snapshot = new WorkSnapshotResponse
        {
            RunStarted = true,
            RunCompleted = false,
            Watermark = 1,
            RunId = RunId,
            WorkspaceRoot = "/workspaces/alpha",
            Focus = new FocusSnapshot { TaskId = TaskId, Goal = longGoal, AnchorRevision = 1 },
            Tasks = [new TaskSnapshotEntry { TaskId = TaskId, Goal = longGoal, Status = TaskSnapshotStatus.Active }],
            PendingApprovals = [],
            ResyncRequired = false,
        };
        snapshot.Validate();

        var detail = new WorkTaskDetailResponse
        {
            TaskId = TaskId,
            Goal = longGoal,
            Status = TaskSnapshotStatus.Active,
            AnchorRevision = 1,
            Anchor = new TaskAnchorView { Revision = 1, OriginalGoal = longGoal, CurrentInterpretation = longGoal },
        };
        detail.Validate();

        // Over the real shared bound is still refused.
        var overGoal = new string('g', WorkSnapshotResponse.MaxGoalChars + 1);
        Assert.Throws<AgentContractViolationException>(() =>
            (detail with { Goal = overGoal }).Validate());
        Assert.Throws<AgentContractViolationException>(() =>
            (snapshot with
            {
                Focus = new FocusSnapshot { TaskId = TaskId, Goal = overGoal, AnchorRevision = 1 },
            }).Validate());
    }

    // -----------------------------------------------------------------------
    // PLATFORM-1 (F06): the exact-request submission receipt query. Wire
    // shapes pinned like the B3 routes (independent of the older fixture
    // set), plus the cross-language digest golden the conflict
    // classification depends on.
    // -----------------------------------------------------------------------

    private const string RunId = "00000000-0000-4000-8000-000000000031";

    [Fact]
    public void Submit_result_disposition_pins_snake_case_wire_and_fact_binding()
    {
        // A conflict answer: snake_case disposition on the wire, the recorded
        // task, and the digest the id was originally admitted for.
        const string conflict = """
            {"run_id":"00000000-0000-4000-8000-000000000031","client_request_id":"client-1",
             "disposition":"known_rejected","task_id":"00000000-0000-4000-8000-000000000022",
             "accepted_payload_digest":"f9e5a123a1495f7c02973725850272a97e070a586e6640b570dd06db44170016"}
            """;
        var compact = JsonSerializer.Serialize(
            JsonSerializer.Deserialize<JsonElement>(conflict, AgentJson.Options), AgentJson.Options);
        var decoded = JsonSerializer.Deserialize<WorkSubmitResultResponse>(compact, AgentJson.Options)!;
        decoded.Validate();
        Assert.Equal(WorkSubmitResultDisposition.KnownRejected, decoded.Disposition);
        Assert.Equal(TaskId, decoded.TaskId);
        Assert.False(decoded.Disposition.IsAdmitted());
        Assert.False(decoded.Disposition.IsIndeterminate());

        var reencoded = JsonSerializer.Serialize(decoded, AgentJson.Options);
        Assert.True(reencoded == compact,
            $"submit result must round-trip byte-identically.{Environment.NewLine}expected: {compact}{Environment.NewLine}actual:   {reencoded}");

        // An indeterminate answer proves nothing and must not manufacture a
        // task; an admitted answer must name the task it bound.
        const string unknown = """
            {"run_id":"00000000-0000-4000-8000-000000000031","client_request_id":"client-1",
             "disposition":"unknown"}
            """;
        var unknownDecoded = JsonSerializer.Deserialize<WorkSubmitResultResponse>(
            JsonSerializer.Serialize(JsonSerializer.Deserialize<JsonElement>(unknown, AgentJson.Options), AgentJson.Options),
            AgentJson.Options)!;
        unknownDecoded.Validate();
        Assert.True(unknownDecoded.Disposition.IsIndeterminate());

        Assert.Throws<AgentContractViolationException>(() =>
            (unknownDecoded with { TaskId = TaskId }).Validate());
        // The recorded-payload digest belongs to a conflict only: any other
        // disposition carrying one is a contradiction.
        Assert.Throws<AgentContractViolationException>(() =>
            (decoded with { Disposition = WorkSubmitResultDisposition.Accepted }).Validate());
        Assert.Throws<AgentContractViolationException>(() =>
            (unknownDecoded with { Disposition = WorkSubmitResultDisposition.Unknown, AcceptedPayloadDigest = "aa" }).Validate());

        // Unknown fields are a decode failure, not silently ignored facts.
        var forged = compact.Replace(
            "\"disposition\":\"known_rejected\"",
            "\"disposition\":\"known_rejected\",\"goal\":\"sneaky\"",
            StringComparison.Ordinal);
        Assert.Throws<JsonException>(() => JsonSerializer.Deserialize<WorkSubmitResultResponse>(forged, AgentJson.Options));
    }

    /// <summary>The conflict classification only means anything if the .NET
    /// client and the Rust host hash identically. The golden hex pins the
    /// documented domain-separated preimage
    /// (DOMAIN || 0x00 || utf8(goal)); the Rust side pins the same
    /// construction in agent-platform-protocol's
    /// <c>submission_payload_digest_is_stable_and_content_bound</c>.</summary>
    [Fact]
    public void Submit_payload_digest_matches_the_host_golden_preimage()
    {
        Assert.Equal(
            "f9e5a123a1495f7c02973725850272a97e070a586e6640b570dd06db44170016",
            SubmitPayloadDigest.Compute("migrate the retry table"));
        Assert.NotEqual(
            SubmitPayloadDigest.Compute("migrate the retry table"),
            SubmitPayloadDigest.Compute("migrate the retry tablE"));
    }

    [Fact]
    public void Snapshot_identity_facts_are_mandatory_and_bounded()
    {
        var snapshot = new WorkSnapshotResponse
        {
            RunStarted = true,
            RunCompleted = false,
            Watermark = 1,
            RunId = RunId,
            WorkspaceRoot = "/workspaces/alpha",
            Tasks = [],
            PendingApprovals = [],
            ResyncRequired = false,
        };
        snapshot.Validate();

        // A nil run id is not a run identity.
        Assert.Throws<AgentContractViolationException>(
            () => (snapshot with { RunId = "00000000-0000-0000-0000-000000000000" }).Validate());
        // An empty workspace root is not an identity either.
        Assert.Throws<AgentContractViolationException>(
            () => (snapshot with { WorkspaceRoot = "" }).Validate());
        Assert.Throws<AgentContractViolationException>(
            () => (snapshot with { WorkspaceRoot = "/workspaces/alpha" }).Validate());
        // Over the shared path bound is refused; at the bound is legal.
        Assert.Throws<AgentContractViolationException>(
            () => (snapshot with { WorkspaceRoot = new string('/', WorkSnapshotResponse.MaxWorkspaceRootBytes + 1) }).Validate());
        (snapshot with { WorkspaceRoot = new string('/', WorkSnapshotResponse.MaxWorkspaceRootBytes) }).Validate();
    }

    /// <summary>PLATFORM-3: the endpoint discriminator hashes the RESOLVED
    /// workspace root — the host canonicalizes before hashing, so the client
    /// must too, or a relative/symlinked spelling derives a different
    /// endpoint than the one the host bound. The byte primitive stays exact
    /// (the fixture pins it); resolution happens above it, and the
    /// snapshot's host-side root remains the equality authority.</summary>
    [Fact]
    public void Workspace_identity_resolves_before_hashing()
    {
        var existing = Path.Combine(Path.GetTempPath(), $"p3-resolve-{Guid.NewGuid():N}", "sub");
        Directory.CreateDirectory(existing);

        try
        {
            var absolute = WorkspaceIdentity.Resolve(existing);
            var relative = WorkspaceIdentity.Resolve(Path.Combine(existing, "sub", ".."));

            Assert.Equal(absolute.EndpointSuffix, relative.EndpointSuffix);
            Assert.Equal(Path.GetFullPath(existing), absolute.Root);
            Assert.Contains(absolute.Root, absolute.Display);

            // The spelling-sensitive primitive stays byte-exact above
            // resolution: two spellings of one directory legitimately differ
            // there — which is exactly why Default* go through Resolve.
            Assert.NotEqual(
                AgentTransports.WorkspaceEndpointSuffix(
                    Path.Combine(existing, "sub", "..")),
                AgentTransports.WorkspaceEndpointSuffix(existing));

            // A drive root never collapses to a drive-relative spelling.
            var driveRoot = Path.GetPathRoot(Path.GetTempPath())!;
            Assert.Equal(driveRoot, WorkspaceIdentity.Resolve(driveRoot).Root);
        }
        finally
        {
            try { Directory.Delete(Path.GetDirectoryName(existing)!, recursive: true); } catch { }
        }

        Assert.Throws<ArgumentException>(() => WorkspaceIdentity.Resolve("  "));
    }

    /// <summary>PLATFORM-4: the usage facts a GUI renders for "why did this
    /// cost anything" are pinned cross-language — the same fixture bytes the
    /// Rust side decodes into the typed kernel envelope feed the .NET
    /// accessor here.</summary>
    [Fact]
    public void Event_fixture_pins_model_usage_facts()
    {
        var text = Read("event_model_used.json");
        AssertRoundTripByteIdentical<PlatformEnvelope<WorkEventNotification>>(text, "event_model_used.json");
        var notification = JsonSerializer.Deserialize<PlatformEnvelope<WorkEventNotification>>(text, AgentJson.Options)!;
        notification.Validate(FixtureIdentity);
        var envelope = notification.Payload.Envelope;
        Assert.Equal(41ul, envelope.Seq);
        Assert.Equal("00000000-0000-4000-8000-000000000002", envelope.RunId);
        Assert.False(envelope.IsLiveOnlyProgress);

        Assert.True(envelope.TryGetModelUsage(out var usage));
        Assert.Equal(100ul, usage.InputTokens);
        Assert.Equal(5ul, usage.OutputTokens);
        Assert.Equal(80ul, usage.CachedInputTokens);
        Assert.Equal(1u, usage.Attempts);
        Assert.Equal("observed", usage.UsageIdentity);
        Assert.True(usage.IsObserved);
        Assert.False(usage.IsIndeterminate);

        // A non-usage event yields no facts — that is about the event, not
        // about consumption.
        var nonUsage = JsonSerializer.Deserialize<PlatformEnvelope<WorkEventNotification>>(
            text.Replace("\"type\":\"model_used\"", "\"type\":\"turn_completed\"", StringComparison.Ordinal),
            AgentJson.Options)!;
        Assert.False(nonUsage.Payload.Envelope.TryGetModelUsage(out _));
    }

    /// <summary>COST-6 (R2-10): the cache-write and cache-miss counters are
    /// readable at the SDK off the same fixture bytes the Rust side pins.
    /// The counters stay null when the provider did not report them —
    /// a missing report is never read as a zero.</summary>
    [Fact]
    public void Event_fixture_pins_cache_write_and_miss_facts()
    {
        var text = Read("event_model_used_cache_fields.json");
        AssertRoundTripByteIdentical<PlatformEnvelope<WorkEventNotification>>(text, "event_model_used_cache_fields.json");
        var notification = JsonSerializer.Deserialize<PlatformEnvelope<WorkEventNotification>>(text, AgentJson.Options)!;
        notification.Validate(FixtureIdentity);

        Assert.True(notification.Payload.Envelope.TryGetModelUsage(out var usage));
        Assert.Equal(80ul, usage.CachedInputTokens);
        Assert.Equal(10ul, usage.CacheWriteInputTokens);
        Assert.Equal(20ul, usage.CacheMissInputTokens);
        Assert.Equal("main", usage.Role);
        Assert.True(usage.IsObserved);

        // The legacy fixture (no cache write/miss on the wire) keeps them
        // null — an unreported counter is not an invented zero.
        var legacy = Read("event_model_used.json");
        var legacyNotification = JsonSerializer.Deserialize<PlatformEnvelope<WorkEventNotification>>(legacy, AgentJson.Options)!;
        Assert.True(legacyNotification.Payload.Envelope.TryGetModelUsage(out var legacyUsage));
        Assert.Null(legacyUsage.CacheWriteInputTokens);
        Assert.Null(legacyUsage.CacheMissInputTokens);
    }

    /// <summary>COST-1 (E05.3): the compaction event carries its own usage
    /// identity across the language boundary — the GUI classifies the cost
    /// row from the wire fact instead of guessing.</summary>
    [Fact]
    public void Event_fixture_pins_compaction_usage_identity()
    {
        var text = Read("event_context_compacted.json");
        AssertRoundTripByteIdentical<PlatformEnvelope<WorkEventNotification>>(text, "event_context_compacted.json");
        var notification = JsonSerializer.Deserialize<PlatformEnvelope<WorkEventNotification>>(text, AgentJson.Options)!;
        notification.Validate(FixtureIdentity);
        var envelope = notification.Payload.Envelope;
        Assert.Equal("context_compacted", envelope.EventType);
        Assert.False(envelope.IsLiveOnlyProgress);
        Assert.Equal("estimated", envelope.Event.GetProperty("usage_identity").GetString());
        Assert.Equal(12000ul, envelope.Event.GetProperty("input_tokens").GetUInt64());
    }

    private const string TaskId = "00000000-0000-4000-8000-000000000022";
}
