//! Versioned, trace-backed acceptance for the focused production editor.
//!
//! This gate is intentionally independent of the frozen context analysis
//! schema. It joins the model's exact `edit.patch` arguments to the matching
//! terminal output, then combines that trace contract with raw-byte hidden
//! verification. A green file alone is insufficient: fallback mutation,
//! malformed first calls, unsettled commits, and avoidable confirmation reads
//! remain visible failures.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use agent_contracts::{RuntimeEvent, RuntimeEventEnvelope, ToolFailureClass};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::tool_edit_pack::{
    self, ConflictContract, FixtureMutationRecord, ToolEditOp, ToolEditTask,
};

pub const SCHEMA: &str = "agent-eval.tool-surface-edit.v3";

/// Fingerprint the exact analyzer implementation used to produce a gate.
/// This complements the semantic schema version and prevents a report from
/// silently mixing cells scored by different source revisions.
pub fn implementation_sha256() -> String {
    use sha2::{Digest, Sha256};

    format!("{:x}", Sha256::digest(include_bytes!("tool_edit_gate.rs")))
}

const MAX_EVENTS: usize = 8_192;
const MAX_TRACKED_CALLS: usize = 256;
const MAX_VIOLATIONS: usize = 32;
const MAX_VIOLATION_CHARS: usize = 240;
const MAX_PATH_CHARS: usize = 512;
const MAX_PATCH_FILES: usize = 16;
const MAX_PATCH_HUNKS: usize = 64;
const MAX_HUNK_TEXT_BYTES: usize = 64 * 1024;
const MAX_PATCH_TEXT_BYTES: usize = 256 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize, Eq, PartialEq)]
pub struct ToolEditGateReport {
    pub schema: String,
    pub fixture_id: String,
    pub passed: bool,
    pub strict_passed: bool,
    pub session_ok: bool,
    pub trace_complete: bool,
    pub seq_contiguous: bool,
    pub analysis_truncated: bool,
    pub model_rounds: u64,
    pub patch_attempts: u64,
    pub patch_started: u64,
    pub patch_finished: u64,
    pub patch_raw_successes: u64,
    pub patch_changed_successes: u64,
    pub patch_noops: u64,
    pub patch_failures: u64,
    pub stale_refusals: u64,
    pub non_stale_failures: u64,
    pub unfinished_patch_calls: u64,
    pub first_patch_shape_valid: bool,
    pub first_patch_has_required_revisions: bool,
    #[serde(default)]
    pub first_patch_revisions_from_latest_reads: bool,
    #[serde(default)]
    pub first_patch_exact_hunks: bool,
    pub first_patch_raw_success: bool,
    pub first_patch_changed_success: bool,
    pub valid_call_first_attempt_success: bool,
    pub target_files_covered: Vec<String>,
    pub target_read_successes: u64,
    #[serde(default)]
    pub read_identity_failures: u64,
    pub fs_read_bytes: u64,
    pub confirm_reads_after_success: u64,
    pub forbidden_calls: u64,
    pub forbidden_tool_counts: BTreeMap<String, u64>,
    pub commit_not_applied: u64,
    pub commit_recovery_required: u64,
    pub commit_unknown: u64,
    pub edit_latency_ms_p50: u64,
    pub edit_latency_ms_p95: u64,
    pub edit_to_green_ms: Option<u64>,
    pub changed_bytes_before: u64,
    pub changed_bytes_after: u64,
    #[serde(default)]
    pub patch_revision_provenance_failures: u64,
    #[serde(default)]
    pub patch_target_mismatches: u64,
    #[serde(default)]
    pub patch_hunk_contract_failures: u64,
    #[serde(default)]
    pub fixture_mutation_evidence_valid: bool,
    #[serde(default)]
    pub conflict_route: Option<String>,
    #[serde(default)]
    pub duplicate_call_ids: u64,
    #[serde(default)]
    pub orphan_tool_finishes: u64,
    pub violations: Vec<String>,
}

#[derive(Debug, Clone, Default)]
struct PatchShape {
    valid: bool,
    has_required_revisions: bool,
    revisions_from_latest_reads: bool,
    exact_hunks: bool,
    paths: Vec<String>,
    revisions: Vec<(String, String)>,
    hunk_fingerprints: Vec<HunkFingerprint>,
    hunks: usize,
}

#[derive(Debug)]
struct PatchShapeAccumulator {
    paths: Vec<String>,
    revision_pairs: Vec<(String, String)>,
    hunk_fingerprints: Vec<HunkFingerprint>,
    hunks: usize,
    hunk_text_bytes: usize,
    all_revisions_valid: bool,
}

impl Default for PatchShapeAccumulator {
    fn default() -> Self {
        Self {
            paths: Vec::new(),
            revision_pairs: Vec::new(),
            hunk_fingerprints: Vec::new(),
            hunks: 0,
            hunk_text_bytes: 0,
            all_revisions_valid: true,
        }
    }
}

#[derive(Debug, Clone, Eq, PartialEq, Ord, PartialOrd)]
struct HunkFingerprint {
    path: String,
    old_sha256: String,
    new_sha256: String,
}

#[derive(Debug, Clone)]
struct ReadEvidence {
    path: String,
    revision: String,
    finished_seq: u64,
}

#[derive(Debug, Clone, Default)]
struct MutationAssessment {
    valid: bool,
    boundary_seq: Option<u64>,
    path: Option<String>,
    before_sha256: Option<String>,
    after_sha256: Option<String>,
}

#[derive(Debug, Clone)]
struct StartedCall {
    name: String,
    started_ms: u64,
    path: Option<String>,
    patch: Option<PatchShape>,
    patch_ordinal: Option<u64>,
}

/// Analyze one persisted/live cell. Work is bounded even if the supplied
/// trace is corrupt: excess events/calls fail closed instead of allocating
/// without limit.
pub fn analyze_cell(
    task: &ToolEditTask,
    events: &[RuntimeEventEnvelope],
    fixture_mutations: &[FixtureMutationRecord],
    strict_passed: bool,
    session_ok: bool,
) -> ToolEditGateReport {
    let contract = &task.file.trace;
    let targets: BTreeSet<String> = contract
        .target_files
        .iter()
        .map(|path| normalize_path(path))
        .collect();
    let forbidden: BTreeSet<&str> = contract
        .forbidden_tools
        .iter()
        .map(String::as_str)
        .collect();
    let mutation = assess_fixture_mutation(task, fixture_mutations, events);
    let expected_hunks = expected_hunk_fingerprints(task);

    let mut open: HashMap<String, StartedCall> = HashMap::new();
    let mut seen_call_ids = BTreeSet::new();
    let mut finished_call_ids = BTreeSet::new();
    let mut patch_attempt_ids = BTreeSet::new();
    let mut covered = BTreeSet::new();
    let mut forbidden_ids = BTreeSet::new();
    let mut forbidden_tool_counts = BTreeMap::new();
    let mut violations = Vec::new();
    let mut latencies = Vec::new();
    let mut model_rounds = 0_u64;
    let mut patch_started = 0_u64;
    let mut patch_finished = 0_u64;
    let mut raw_successes = 0_u64;
    let mut changed_successes = 0_u64;
    let mut noops = 0_u64;
    let mut failures = 0_u64;
    let mut stale_refusals = 0_u64;
    let mut non_stale_failures = 0_u64;
    let mut target_read_successes = 0_u64;
    let mut read_identity_failures = 0_u64;
    let mut latest_read_revisions: BTreeMap<String, String> = BTreeMap::new();
    let mut target_reads = Vec::new();
    let mut fs_read_bytes = 0_u64;
    let mut confirm_reads = 0_u64;
    let mut commit_not_applied = 0_u64;
    let mut commit_recovery_required = 0_u64;
    let mut commit_unknown = 0_u64;
    let mut changed_bytes_before = 0_u64;
    let mut changed_bytes_after = 0_u64;
    let mut first_patch_id: Option<String> = None;
    let mut first_patch_timestamp_ms: Option<u64> = None;
    let mut first_patch_shape_valid = false;
    let mut first_patch_has_required_revisions = false;
    let mut first_patch_revisions_from_latest_reads = false;
    let mut first_patch_exact_hunks = false;
    let mut first_patch_revisions = Vec::new();
    let mut first_patch_raw_success = false;
    let mut first_patch_changed_success = false;
    let mut first_patch_stale_refusal = false;
    let mut second_patch_changed_success = false;
    let mut first_stale_finished_seq = None;
    let mut first_stale_identity_valid = false;
    let mut first_patch_started_seq = None;
    let mut second_patch_started_seq = None;
    let mut first_green_timestamp_ms: Option<u64> = None;
    let mut successful_patch_seen = false;
    let mut success_shape_failures = 0_u64;
    let mut patch_revision_provenance_failures = 0_u64;
    let mut patch_target_mismatches = 0_u64;
    let mut patch_hunk_contract_failures = 0_u64;
    let mut duplicate_call_ids = 0_u64;
    let mut orphan_tool_finishes = 0_u64;
    let mut analysis_truncated = events.len() > MAX_EVENTS;
    let mut previous_seq = None;
    let mut seq_contiguous = true;
    let single_run_id = events.first().is_some_and(|first| {
        events
            .iter()
            .all(|envelope| envelope.run_id == first.run_id)
    });
    let durable_barriers = events
        .first()
        .is_some_and(|first| first.seq == 1 && matches!(first.event, RuntimeEvent::RunStarted))
        && events
            .last()
            .is_some_and(|last| matches!(last.event, RuntimeEvent::RunCompleted));

    for envelope in events.iter().take(MAX_EVENTS) {
        if !matches!(envelope.event, RuntimeEvent::ModelDelta { .. }) {
            if let Some(previous) = previous_seq
                && envelope.seq != previous + 1
            {
                seq_contiguous = false;
            }
            previous_seq = Some(envelope.seq);
        }

        match &envelope.event {
            RuntimeEvent::ModelStarted { .. } => model_rounds += 1,
            RuntimeEvent::ToolStarted { call } => {
                if !seen_call_ids.insert(call.id.clone()) {
                    duplicate_call_ids += 1;
                    continue;
                }
                if forbidden.contains(call.name.as_str()) && forbidden_ids.insert(call.id.clone()) {
                    *forbidden_tool_counts.entry(call.name.clone()).or_insert(0) += 1;
                }

                let path = call
                    .arguments
                    .get("path")
                    .and_then(Value::as_str)
                    .map(normalize_path);
                if call.name == "fs.read"
                    && successful_patch_seen
                    && path.as_ref().is_some_and(|path| targets.contains(path))
                {
                    confirm_reads += 1;
                }

                let mut patch_ordinal = None;
                let patch = if call.name == "edit.patch" {
                    patch_started += 1;
                    patch_ordinal = Some(patch_started);
                    if patch_started == 2 {
                        second_patch_started_seq = Some(envelope.seq);
                    }
                    record_attempt(&mut patch_attempt_ids, &call.id, &mut analysis_truncated);
                    let mut shape =
                        validate_patch_shape(&call.arguments, contract.require_base_revision);
                    shape.revisions_from_latest_reads = !contract.require_base_revision
                        || (shape.revisions.len() == shape.paths.len()
                            && shape.revisions.iter().all(|(path, revision)| {
                                latest_read_revisions.get(path) == Some(revision)
                            }));
                    if !shape.revisions_from_latest_reads {
                        patch_revision_provenance_failures += 1;
                    }
                    shape.exact_hunks = shape.hunk_fingerprints == expected_hunks;
                    if !shape.exact_hunks {
                        patch_hunk_contract_failures += 1;
                    }
                    let patch_targets: BTreeSet<&str> =
                        shape.paths.iter().map(String::as_str).collect();
                    if shape.valid
                        && patch_targets
                            != targets.iter().map(String::as_str).collect::<BTreeSet<_>>()
                    {
                        patch_target_mismatches += 1;
                    }
                    if first_patch_id.is_none() {
                        first_patch_id = Some(call.id.clone());
                        first_patch_started_seq = Some(envelope.seq);
                        first_patch_timestamp_ms = Some(envelope.timestamp_ms);
                        first_patch_shape_valid = shape.valid;
                        first_patch_has_required_revisions = shape.has_required_revisions;
                        first_patch_revisions_from_latest_reads = shape.revisions_from_latest_reads;
                        first_patch_exact_hunks = shape.exact_hunks;
                        first_patch_revisions = shape.revisions.clone();
                    }
                    Some(shape)
                } else {
                    None
                };

                if open.len() >= MAX_TRACKED_CALLS && !open.contains_key(&call.id) {
                    analysis_truncated = true;
                    continue;
                }
                open.insert(
                    call.id.clone(),
                    StartedCall {
                        name: call.name.clone(),
                        started_ms: envelope.timestamp_ms,
                        path,
                        patch,
                        patch_ordinal,
                    },
                );
            }
            RuntimeEvent::ToolFinished { output } => {
                if !finished_call_ids.insert(output.call_id.clone()) {
                    duplicate_call_ids += 1;
                    continue;
                }
                if forbidden.contains(output.tool_name.as_str())
                    && forbidden_ids.insert(output.call_id.clone())
                {
                    *forbidden_tool_counts
                        .entry(output.tool_name.clone())
                        .or_insert(0) += 1;
                }
                let started = open.remove(&output.call_id);
                if started
                    .as_ref()
                    .is_none_or(|call| call.name != output.tool_name)
                {
                    orphan_tool_finishes += 1;
                }

                if output.tool_name == "fs.read" && output.ok {
                    fs_read_bytes =
                        fs_read_bytes.saturating_add(metadata_u64(&output.metadata, "bytes"));
                    let output_path = output
                        .metadata
                        .get("path")
                        .and_then(Value::as_str)
                        .map(normalize_path);
                    let started_path = started.as_ref().and_then(|call| call.path.as_ref());
                    let revision = output
                        .metadata
                        .get("revision")
                        .and_then(Value::as_str)
                        .filter(|revision| is_sha256(revision));
                    let identity_valid =
                        started.as_ref().is_some_and(|call| call.name == "fs.read")
                            && started_path == output_path.as_ref()
                            && revision.is_some();
                    let touches_target = output_path
                        .as_ref()
                        .is_some_and(|path| targets.contains(path))
                        || started_path.is_some_and(|path| targets.contains(path));
                    if touches_target {
                        if identity_valid {
                            let path = output_path.expect("validated target read path");
                            latest_read_revisions.insert(
                                path.clone(),
                                revision
                                    .expect("validated target read revision")
                                    .to_string(),
                            );
                            target_read_successes += 1;
                            target_reads.push(ReadEvidence {
                                path,
                                revision: revision
                                    .expect("validated target read revision")
                                    .to_string(),
                                finished_seq: envelope.seq,
                            });
                        } else {
                            read_identity_failures += 1;
                        }
                    }
                }

                if output.tool_name != "edit.patch" {
                    continue;
                }
                patch_finished += 1;
                record_attempt(
                    &mut patch_attempt_ids,
                    &output.call_id,
                    &mut analysis_truncated,
                );
                if first_patch_id.is_none() {
                    first_patch_id = Some(output.call_id.clone());
                    first_patch_timestamp_ms = Some(envelope.timestamp_ms);
                }
                let is_first = first_patch_id.as_deref() == Some(output.call_id.as_str());
                let shape = started.as_ref().and_then(|call| call.patch.as_ref());
                let patch_ordinal = started.as_ref().and_then(|call| call.patch_ordinal);
                if let Some(started) = &started {
                    latencies.push(envelope.timestamp_ms.saturating_sub(started.started_ms));
                }

                match output.metadata.get("commit_state").and_then(Value::as_str) {
                    Some("not_applied" | "rejected") => commit_not_applied += 1,
                    Some("not_applied_authority_recovery_required") => {
                        commit_not_applied += 1;
                        commit_recovery_required += 1;
                    }
                    Some("applied_recovery_required" | "applied_authority_recovery_required") => {
                        commit_recovery_required += 1;
                    }
                    Some("unsettled") => commit_unknown += 1,
                    Some(state) if state.starts_with("unknown") => commit_unknown += 1,
                    _ => {}
                }

                if output.ok {
                    raw_successes += 1;
                    if is_first {
                        first_patch_raw_success = true;
                    }
                    let changed =
                        output.metadata.get("changed").and_then(Value::as_bool) == Some(true);
                    if changed {
                        changed_successes += 1;
                        successful_patch_seen = true;
                        first_green_timestamp_ms.get_or_insert(envelope.timestamp_ms);
                        if is_first {
                            first_patch_changed_success = true;
                        }
                        if patch_ordinal == Some(2) {
                            second_patch_changed_success = true;
                        }
                        let output_paths = changed_output_paths(&output.metadata);
                        covered.extend(output_paths.iter().cloned());
                        let (files, hunks, before, after) = changed_output_shape(&output.metadata);
                        changed_bytes_before = changed_bytes_before.saturating_add(before);
                        changed_bytes_after = changed_bytes_after.saturating_add(after);
                        if files != contract.expected_files_per_success
                            || hunks < contract.min_hunks_per_success
                            || shape.is_none_or(|shape| {
                                !shape.valid
                                    || (contract.require_base_revision
                                        && !shape.has_required_revisions)
                                    || !shape.revisions_from_latest_reads
                                    || !shape.exact_hunks
                                    || shape.paths.iter().cloned().collect::<BTreeSet<_>>()
                                        != output_paths
                                    || shape.hunks < contract.min_hunks_per_success
                            })
                        {
                            success_shape_failures += 1;
                        }
                    } else {
                        noops += 1;
                    }
                } else {
                    failures += 1;
                    if output.failure_class() == Some(ToolFailureClass::StaleRevision) {
                        stale_refusals += 1;
                        if is_first {
                            first_patch_stale_refusal = true;
                            first_stale_finished_seq = Some(envelope.seq);
                            first_stale_identity_valid =
                                stale_output_matches_mutation(&output.metadata, &mutation);
                        }
                    } else {
                        non_stale_failures += 1;
                    }
                }
            }
            _ => {}
        }
    }

    let unfinished_patch_calls = open
        .values()
        .filter(|call| call.name == "edit.patch")
        .count() as u64;
    let trace_complete = seq_contiguous
        && !analysis_truncated
        && open.is_empty()
        && single_run_id
        && durable_barriers
        && duplicate_call_ids == 0
        && orphan_tool_finishes == 0;
    let patch_attempts = patch_attempt_ids.len() as u64;
    let target_files_covered: Vec<String> = covered.iter().cloned().collect();

    if !session_ok {
        push_violation(&mut violations, "runtime session did not finish cleanly");
    }
    if !strict_passed {
        push_violation(&mut violations, "raw-byte hidden verification failed");
    }
    if !trace_complete {
        push_violation(
            &mut violations,
            "event trace is incomplete, non-contiguous, or truncated",
        );
    }
    if model_rounds < u64::from(task.file.target_rounds_lo)
        || model_rounds > u64::from(task.file.target_rounds_hi)
    {
        push_violation(
            &mut violations,
            format!(
                "model rounds {model_rounds} outside {}..{}",
                task.file.target_rounds_lo, task.file.target_rounds_hi
            ),
        );
    }
    if patch_attempts > contract.max_patch_calls as u64 {
        push_violation(
            &mut violations,
            format!(
                "patch attempts {patch_attempts} exceed {}",
                contract.max_patch_calls
            ),
        );
    }
    if changed_successes != contract.required_successful_patch_calls as u64 {
        push_violation(
            &mut violations,
            format!(
                "changed patch successes {changed_successes} != required {}",
                contract.required_successful_patch_calls
            ),
        );
    }
    if !first_patch_shape_valid {
        push_violation(
            &mut violations,
            "first edit.patch call was missing or non-canonical",
        );
    }
    if contract.require_base_revision && !first_patch_has_required_revisions {
        push_violation(
            &mut violations,
            "first edit.patch call did not carry every required base_revision",
        );
    }
    if contract.require_base_revision && !first_patch_revisions_from_latest_reads {
        push_violation(
            &mut violations,
            "first edit.patch revisions did not come from the latest successful fs.read of each path",
        );
    }
    if !first_patch_exact_hunks {
        push_violation(
            &mut violations,
            "first edit.patch call did not use the fixture's exact local hunks",
        );
    }
    if read_identity_failures > 0 {
        push_violation(
            &mut violations,
            format!(
                "{read_identity_failures} target fs.read result(s) lacked matching path/revision identity"
            ),
        );
    }
    if patch_revision_provenance_failures > 0 {
        push_violation(
            &mut violations,
            format!(
                "{patch_revision_provenance_failures} patch call(s) used revisions other than the latest successful fs.read"
            ),
        );
    }
    if patch_target_mismatches > 0 {
        push_violation(
            &mut violations,
            format!(
                "{patch_target_mismatches} patch call(s) targeted a set other than the fixture files"
            ),
        );
    }
    if patch_hunk_contract_failures > 0 {
        push_violation(
            &mut violations,
            format!(
                "{patch_hunk_contract_failures} patch call(s) violated the exact local-hunk contract"
            ),
        );
    }
    if !mutation.valid {
        push_violation(
            &mut violations,
            "fixture mutation evidence did not match the frozen op and completed-turn boundary",
        );
    }
    if contract.first_patch_must_succeed && !first_patch_changed_success {
        push_violation(
            &mut violations,
            "first edit.patch call did not commit a changed result",
        );
    }
    if covered != targets {
        push_violation(
            &mut violations,
            "changed patch target coverage did not match the fixture",
        );
    }
    if success_shape_failures > 0 {
        push_violation(
            &mut violations,
            format!(
                "{success_shape_failures} successful patch call(s) violated file/hunk/revision shape"
            ),
        );
    }
    if noops > 0 {
        push_violation(
            &mut violations,
            format!("{noops} edit.patch no-op result(s) are not accepted by this gate"),
        );
    }
    let pre_mutation_read = mutation
        .boundary_seq
        .zip(mutation.path.as_deref())
        .zip(mutation.before_sha256.as_deref())
        .is_some_and(|((boundary, path), revision)| {
            target_reads.iter().any(|read| {
                read.path == path && read.revision == revision && read.finished_seq <= boundary
            })
        });
    let post_mutation_read_before_first_patch = mutation
        .boundary_seq
        .zip(first_patch_started_seq)
        .zip(mutation.path.as_deref())
        .zip(mutation.after_sha256.as_deref())
        .is_some_and(|(((boundary, patch), path), revision)| {
            target_reads.iter().any(|read| {
                read.path == path
                    && read.revision == revision
                    && boundary < read.finished_seq
                    && read.finished_seq < patch
            })
        });
    let first_patch_uses_pre_mutation_revision = mutation
        .path
        .as_deref()
        .zip(mutation.before_sha256.as_deref())
        .is_some_and(|(path, revision)| {
            first_patch_revisions
                .iter()
                .any(|(actual_path, actual_revision)| {
                    actual_path == path && actual_revision == revision
                })
        });
    let first_patch_after_mutation = mutation
        .boundary_seq
        .zip(first_patch_started_seq)
        .is_some_and(|(boundary, patch)| boundary < patch);
    let intervening_revalidation = first_stale_finished_seq
        .zip(second_patch_started_seq)
        .zip(mutation.path.as_deref())
        .zip(mutation.after_sha256.as_deref())
        .is_some_and(|(((stale, retry), path), revision)| {
            target_reads.iter().any(|read| {
                read.path == path
                    && read.revision == revision
                    && stale < read.finished_seq
                    && read.finished_seq < retry
            })
        });
    let mut conflict_route = None;
    match contract.conflict {
        ConflictContract::None if failures > 0 => push_violation(
            &mut violations,
            format!("non-conflict fixture had {failures} failed patch call(s)"),
        ),
        ConflictContract::StaleOrRevalidated => {
            let proactive = mutation.valid
                && patch_attempts == 1
                && first_patch_changed_success
                && failures == 0
                && pre_mutation_read
                && post_mutation_read_before_first_patch;
            let reactive = mutation.valid
                && patch_attempts == 2
                && first_patch_stale_refusal
                && first_stale_identity_valid
                && first_patch_after_mutation
                && first_patch_uses_pre_mutation_revision
                && !post_mutation_read_before_first_patch
                && second_patch_changed_success
                && stale_refusals == 1
                && failures == 1
                && non_stale_failures == 0
                && intervening_revalidation;
            if proactive {
                conflict_route = Some("proactive".to_string());
            } else if reactive {
                conflict_route = Some("reactive".to_string());
            }
            if !proactive && !reactive {
                push_violation(
                    &mut violations,
                    "stale fixture followed neither the proactive-read nor stale-refusal/read/retry state machine",
                );
            }
            if non_stale_failures > 0 {
                push_violation(
                    &mut violations,
                    format!("stale fixture had {non_stale_failures} unrelated patch failure(s)"),
                );
            }
        }
        ConflictContract::None => {}
    }
    if confirm_reads > contract.max_confirm_reads_after_success as u64 {
        push_violation(
            &mut violations,
            format!(
                "post-edit confirmation reads {confirm_reads} exceed {}",
                contract.max_confirm_reads_after_success
            ),
        );
    }
    if !forbidden_ids.is_empty() {
        push_violation(
            &mut violations,
            format!(
                "{} forbidden fallback call(s) observed",
                forbidden_ids.len()
            ),
        );
    }
    if commit_recovery_required + commit_unknown > 0 {
        push_violation(
            &mut violations,
            "an edit.patch settlement required recovery or had unknown state",
        );
    }
    if unfinished_patch_calls > 0 {
        push_violation(
            &mut violations,
            format!("{unfinished_patch_calls} edit.patch call(s) never finished"),
        );
    }

    latencies.sort_unstable();
    let edit_to_green_ms = first_patch_timestamp_ms
        .zip(first_green_timestamp_ms)
        .map(|(first, green)| green.saturating_sub(first));
    let valid_call_first_attempt_success = first_patch_shape_valid
        && first_patch_revisions_from_latest_reads
        && first_patch_exact_hunks
        && patch_target_mismatches == 0
        && first_patch_changed_success;
    let passed = violations.is_empty();
    ToolEditGateReport {
        schema: SCHEMA.to_string(),
        fixture_id: task.id().to_string(),
        passed,
        strict_passed,
        session_ok,
        trace_complete,
        seq_contiguous,
        analysis_truncated,
        model_rounds,
        patch_attempts,
        patch_started,
        patch_finished,
        patch_raw_successes: raw_successes,
        patch_changed_successes: changed_successes,
        patch_noops: noops,
        patch_failures: failures,
        stale_refusals,
        non_stale_failures,
        unfinished_patch_calls,
        first_patch_shape_valid,
        first_patch_has_required_revisions,
        first_patch_revisions_from_latest_reads,
        first_patch_exact_hunks,
        first_patch_raw_success,
        first_patch_changed_success,
        valid_call_first_attempt_success,
        target_files_covered,
        target_read_successes,
        read_identity_failures,
        fs_read_bytes,
        confirm_reads_after_success: confirm_reads,
        forbidden_calls: forbidden_ids.len() as u64,
        forbidden_tool_counts,
        commit_not_applied,
        commit_recovery_required,
        commit_unknown,
        edit_latency_ms_p50: percentile(&latencies, 50),
        edit_latency_ms_p95: percentile(&latencies, 95),
        edit_to_green_ms,
        changed_bytes_before,
        changed_bytes_after,
        patch_revision_provenance_failures,
        patch_target_mismatches,
        patch_hunk_contract_failures,
        fixture_mutation_evidence_valid: mutation.valid,
        conflict_route,
        duplicate_call_ids,
        orphan_tool_finishes,
        violations,
    }
}

fn validate_patch_shape(arguments: &Value, require_revision: bool) -> PatchShape {
    let Some(object) = arguments.as_object() else {
        return PatchShape::default();
    };
    // V3 measures the one model-visible wire shape. The runtime still accepts
    // the older top-level shortcut for compatibility, but a benchmark call is
    // canonical only when every target and revision is explicit in `files[]`.
    if object.keys().any(|key| key != "files") {
        return PatchShape::default();
    }

    let mut accumulated = PatchShapeAccumulator::default();
    let valid = object
        .get("files")
        .and_then(Value::as_array)
        .is_some_and(|files| {
            !files.is_empty()
                && files.len() <= MAX_PATCH_FILES
                && files
                    .iter()
                    .all(|file| validate_patch_file(file, require_revision, &mut accumulated))
        });
    let unique: BTreeSet<&str> = accumulated.paths.iter().map(String::as_str).collect();
    accumulated.hunk_fingerprints.sort_unstable();
    PatchShape {
        valid: valid
            && !accumulated.paths.is_empty()
            && unique.len() == accumulated.paths.len()
            && (1..=MAX_PATCH_HUNKS).contains(&accumulated.hunks)
            && (!require_revision || accumulated.all_revisions_valid),
        has_required_revisions: !require_revision || accumulated.all_revisions_valid,
        revisions_from_latest_reads: false,
        exact_hunks: false,
        paths: accumulated.paths,
        revisions: accumulated.revision_pairs,
        hunk_fingerprints: accumulated.hunk_fingerprints,
        hunks: accumulated.hunks,
    }
}

fn validate_patch_file(
    file: &Value,
    require_revision: bool,
    accumulated: &mut PatchShapeAccumulator,
) -> bool {
    let Some(object) = file.as_object() else {
        return false;
    };
    if object
        .keys()
        .any(|key| !matches!(key.as_str(), "path" | "base_revision" | "hunks"))
    {
        return false;
    }
    let Some(path) = object.get("path").and_then(Value::as_str) else {
        return false;
    };
    if path.is_empty() || path.chars().count() > MAX_PATH_CHARS {
        return false;
    }
    let path = normalize_path(path);
    if !is_clean_relative(&path) {
        return false;
    }
    let revision = object.get("base_revision").and_then(Value::as_str);
    let revision_ok = revision.is_some_and(is_sha256);
    if require_revision && !revision_ok {
        accumulated.all_revisions_valid = false;
    }
    if let Some(revision) = revision.filter(|revision| is_sha256(revision)) {
        accumulated
            .revision_pairs
            .push((path.clone(), revision.to_string()));
    }
    accumulated.paths.push(path.clone());
    let Some(hunks) = object.get("hunks").and_then(Value::as_array) else {
        return false;
    };
    if hunks.is_empty() || accumulated.hunks.saturating_add(hunks.len()) > MAX_PATCH_HUNKS {
        return false;
    }
    accumulated.hunks += hunks.len();
    hunks.iter().all(|hunk| {
        let Some((old, new)) = validate_hunk(hunk) else {
            return false;
        };
        let Some(next_bytes) = accumulated
            .hunk_text_bytes
            .checked_add(old.len())
            .and_then(|total| total.checked_add(new.len()))
        else {
            return false;
        };
        if old.len() > MAX_HUNK_TEXT_BYTES
            || new.len() > MAX_HUNK_TEXT_BYTES
            || next_bytes > MAX_PATCH_TEXT_BYTES
        {
            return false;
        }
        accumulated.hunk_text_bytes = next_bytes;
        accumulated
            .hunk_fingerprints
            .push(hunk_fingerprint(&path, old, new));
        true
    })
}

fn validate_hunk(hunk: &Value) -> Option<(&str, &str)> {
    let object = hunk.as_object()?;
    if object
        .keys()
        .any(|key| !matches!(key.as_str(), "old" | "new" | "occurrence"))
    {
        return None;
    }
    let old = object.get("old").and_then(Value::as_str);
    let new = object.get("new").and_then(Value::as_str);
    if old.is_none_or(str::is_empty) || new.is_none() || old == new {
        return None;
    }
    if !object
        .get("occurrence")
        .is_none_or(|value| value.as_u64().is_some_and(|occurrence| occurrence >= 1))
    {
        return None;
    }
    Some((
        old.expect("validated old hunk"),
        new.expect("validated new hunk"),
    ))
}

fn expected_hunk_fingerprints(task: &ToolEditTask) -> Vec<HunkFingerprint> {
    let mut expected: Vec<_> = task
        .file
        .trace
        .exact_hunks
        .iter()
        .map(|hunk| hunk_fingerprint(&normalize_path(&hunk.path), &hunk.old, &hunk.new))
        .collect();
    expected.sort_unstable();
    expected
}

fn hunk_fingerprint(path: &str, old: &str, new: &str) -> HunkFingerprint {
    let strip_terminal_newline = ends_with_logical_newline(old) && ends_with_logical_newline(new);
    HunkFingerprint {
        path: path.to_string(),
        old_sha256: logical_hunk_sha256(old, strip_terminal_newline),
        new_sha256: logical_hunk_sha256(new, strip_terminal_newline),
    }
}

fn ends_with_logical_newline(value: &str) -> bool {
    value.as_bytes().last() == Some(&b'\n')
}

/// Fingerprint the same LF logical view consumed by edit.patch without
/// allocating another copy. A terminal delimiter present on both sides is
/// anchor context, not a different edit span; lone CR remains literal.
fn logical_hunk_sha256(value: &str, strip_terminal_newline: bool) -> String {
    use sha2::{Digest as _, Sha256};

    let bytes = value.as_bytes();
    let mut end = bytes.len();
    if strip_terminal_newline && end > 0 && bytes[end - 1] == b'\n' {
        end -= 1;
        if end > 0 && bytes[end - 1] == b'\r' {
            end -= 1;
        }
    }
    let mut digest = Sha256::new();
    let mut index = 0usize;
    while index < end {
        if bytes[index] == b'\r' && index + 1 < end && bytes[index + 1] == b'\n' {
            digest.update(b"\n");
            index += 2;
        } else {
            digest.update([bytes[index]]);
            index += 1;
        }
    }
    format!("{:x}", digest.finalize())
}

fn assess_fixture_mutation(
    task: &ToolEditTask,
    records: &[FixtureMutationRecord],
    events: &[RuntimeEventEnvelope],
) -> MutationAssessment {
    let expected: Vec<_> = task
        .file
        .ops
        .iter()
        .enumerate()
        .filter_map(|(index, op)| match op {
            ToolEditOp::FixtureReplace {
                path,
                expected_sha256,
                content,
            } => Some((
                index + 1,
                normalize_path(path),
                expected_sha256.as_str(),
                tool_edit_pack::content_sha256(content.as_bytes()),
                content.len(),
            )),
            ToolEditOp::User { .. } => None,
        })
        .collect();
    if expected.is_empty() && records.is_empty() {
        return MutationAssessment {
            valid: true,
            ..MutationAssessment::default()
        };
    }
    if expected.len() != 1 || records.len() != 1 {
        return MutationAssessment::default();
    }
    let expected = &expected[0];
    let record = &records[0];
    let Some(boundary_seq) = record.event_seq_before else {
        return MutationAssessment::default();
    };
    let boundary_is_completed_turn = events.iter().any(|event| {
        event.seq == boundary_seq && matches!(event.event, RuntimeEvent::TurnCompleted)
    });
    let event_after_boundary = events.iter().any(|event| event.seq > boundary_seq);
    let record_path = normalize_path(&record.path);
    let valid = expected.0 == record.op_index
        && expected.1 == record_path
        && expected.2 == record.before_sha256
        && expected.3 == record.after_sha256
        && expected.4 == record.bytes_after
        && boundary_is_completed_turn
        && event_after_boundary;
    MutationAssessment {
        valid,
        boundary_seq: Some(boundary_seq),
        path: Some(record_path),
        before_sha256: Some(record.before_sha256.clone()),
        after_sha256: Some(record.after_sha256.clone()),
    }
}

fn stale_output_matches_mutation(metadata: &Value, mutation: &MutationAssessment) -> bool {
    let path = metadata
        .get("path")
        .and_then(Value::as_str)
        .map(normalize_path);
    let revision = metadata
        .get("revision")
        .and_then(Value::as_str)
        .filter(|revision| is_sha256(revision));
    mutation.valid
        && path.as_ref() == mutation.path.as_ref()
        && revision == mutation.after_sha256.as_deref()
}

fn changed_output_paths(metadata: &Value) -> BTreeSet<String> {
    metadata
        .get("files")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|file| file.get("changed").and_then(Value::as_bool) == Some(true))
        .filter_map(|file| file.get("path").and_then(Value::as_str))
        .map(normalize_path)
        .take(MAX_PATCH_FILES)
        .collect()
}

fn changed_output_shape(metadata: &Value) -> (usize, usize, u64, u64) {
    metadata
        .get("files")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|file| file.get("changed").and_then(Value::as_bool) == Some(true))
        .take(MAX_PATCH_FILES)
        .fold((0, 0, 0, 0), |(files, hunks, before, after), file| {
            (
                files + 1,
                hunks.saturating_add(metadata_u64(file, "hunks") as usize),
                before.saturating_add(metadata_u64(file, "bytes_before")),
                after.saturating_add(metadata_u64(file, "bytes_after")),
            )
        })
}

fn record_attempt(ids: &mut BTreeSet<String>, id: &str, truncated: &mut bool) {
    if ids.len() < MAX_TRACKED_CALLS || ids.contains(id) {
        ids.insert(id.to_string());
    } else {
        *truncated = true;
    }
}

fn normalize_path(path: &str) -> String {
    path.trim().replace('\\', "/")
}

fn is_clean_relative(path: &str) -> bool {
    !path.is_empty()
        && !path.starts_with('/')
        && !(path.len() >= 2 && path.as_bytes()[1] == b':')
        && path
            .split('/')
            .all(|part| !part.is_empty() && part != "." && part != "..")
}

fn is_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn metadata_u64(metadata: &Value, key: &str) -> u64 {
    metadata.get(key).and_then(Value::as_u64).unwrap_or(0)
}

fn percentile(samples: &[u64], percentile: usize) -> u64 {
    if samples.is_empty() {
        return 0;
    }
    crate::metrics::percentile(samples, percentile as u64)
}

fn push_violation(violations: &mut Vec<String>, message: impl Into<String>) {
    if violations.len() >= MAX_VIOLATIONS {
        return;
    }
    let message = message.into();
    violations.push(message.chars().take(MAX_VIOLATION_CHARS).collect());
}

#[cfg(test)]
mod tests {
    use agent_contracts::{
        OperationId, PromptLayerCosts, RunId, RuntimeEvent, RuntimeEventEnvelope, ToolCall,
        ToolOutput, TurnId,
    };
    use serde_json::json;

    use super::*;

    fn event(
        run_id: RunId,
        seq: u64,
        timestamp_ms: u64,
        event: RuntimeEvent,
    ) -> RuntimeEventEnvelope {
        RuntimeEventEnvelope {
            run_id,
            seq,
            timestamp_ms,
            event,
        }
    }

    fn model_started(run_id: RunId, seq: u64, timestamp_ms: u64) -> RuntimeEventEnvelope {
        event(
            run_id,
            seq,
            timestamp_ms,
            RuntimeEvent::ModelStarted {
                turn_id: TurnId::new(),
                operation_id: OperationId::new(),
                generation: 1,
                surface_revision: 1,
                model_round: seq as usize,
                prompt_layers: PromptLayerCosts::default(),
            },
        )
    }

    fn run_started(run_id: RunId) -> RuntimeEventEnvelope {
        event(run_id, 1, 1, RuntimeEvent::RunStarted)
    }

    fn run_completed(run_id: RunId, seq: u64, timestamp_ms: u64) -> RuntimeEventEnvelope {
        event(run_id, seq, timestamp_ms, RuntimeEvent::RunCompleted)
    }

    fn turn_completed(run_id: RunId, seq: u64, timestamp_ms: u64) -> RuntimeEventEnvelope {
        event(run_id, seq, timestamp_ms, RuntimeEvent::TurnCompleted)
    }

    fn mutation_record(
        task: &ToolEditTask,
        event_seq_before: Option<u64>,
    ) -> FixtureMutationRecord {
        let (index, path, expected_sha256, content) = task
            .file
            .ops
            .iter()
            .enumerate()
            .find_map(|(index, op)| match op {
                ToolEditOp::FixtureReplace {
                    path,
                    expected_sha256,
                    content,
                } => Some((index, path, expected_sha256, content)),
                ToolEditOp::User { .. } => None,
            })
            .expect("stale fixture mutation");
        FixtureMutationRecord {
            op_index: index + 1,
            path: path.clone(),
            before_sha256: expected_sha256.clone(),
            after_sha256: tool_edit_pack::content_sha256(content.as_bytes()),
            bytes_after: content.len(),
            event_seq_before,
        }
    }

    fn read_started(
        run_id: RunId,
        seq: u64,
        timestamp_ms: u64,
        id: &str,
        path: &str,
    ) -> RuntimeEventEnvelope {
        event(
            run_id,
            seq,
            timestamp_ms,
            RuntimeEvent::ToolStarted {
                call: ToolCall {
                    id: id.into(),
                    name: "fs.read".into(),
                    arguments: json!({"path": path}),
                },
            },
        )
    }

    fn read_finished(
        run_id: RunId,
        seq: u64,
        timestamp_ms: u64,
        id: &str,
        path: &str,
        revision: &str,
    ) -> RuntimeEventEnvelope {
        event(
            run_id,
            seq,
            timestamp_ms,
            RuntimeEvent::ToolFinished {
                output: ToolOutput {
                    call_id: id.into(),
                    tool_name: "fs.read".into(),
                    ok: true,
                    summary: "read".into(),
                    model_content: String::new(),
                    artifact_ref: None,
                    metadata: json!({"path": path, "revision": revision, "bytes": 32}),
                },
            },
        )
    }

    fn patch_started(
        run_id: RunId,
        seq: u64,
        timestamp_ms: u64,
        id: &str,
        path: &str,
        revision: &str,
    ) -> RuntimeEventEnvelope {
        event(
            run_id,
            seq,
            timestamp_ms,
            RuntimeEvent::ToolStarted {
                call: ToolCall {
                    id: id.into(),
                    name: "edit.patch".into(),
                    arguments: json!({
                        "files": [{
                            "path": path,
                            "base_revision": revision,
                            "hunks": [{"old": "limit=3", "new": "limit=5"}]
                        }]
                    }),
                },
            },
        )
    }

    fn patch_finished(
        run_id: RunId,
        seq: u64,
        timestamp_ms: u64,
        id: &str,
        path: &str,
        failure_class: Option<ToolFailureClass>,
        current_revision: Option<&str>,
    ) -> RuntimeEventEnvelope {
        let changed = failure_class.is_none();
        event(
            run_id,
            seq,
            timestamp_ms,
            RuntimeEvent::ToolFinished {
                output: ToolOutput {
                    call_id: id.into(),
                    tool_name: "edit.patch".into(),
                    ok: changed,
                    summary: if changed { "patched" } else { "refused" }.into(),
                    model_content: String::new(),
                    artifact_ref: None,
                    metadata: if let Some(class) = failure_class {
                        json!({
                            "failure_class": class.as_str(),
                            "path": path,
                            "revision": current_revision
                        })
                    } else {
                        json!({
                            "changed": true,
                            "files": [{
                                "path": path,
                                "changed": true,
                                "hunks": 1,
                                "bytes_before": 32,
                                "bytes_after": 32
                            }]
                        })
                    },
                },
            },
        )
    }

    #[test]
    fn canonical_patch_and_raw_green_pass_the_gate() {
        let pack = crate::tool_edit_pack::load_pack().unwrap();
        let task = pack.task("crlf_multi_hunk").unwrap();
        let revision = task.file.seed_files[0].sha256.clone();
        let run = RunId::new();
        let events = vec![
            run_started(run),
            model_started(run, 2, 10),
            event(
                run,
                3,
                20,
                RuntimeEvent::ToolStarted {
                    call: ToolCall {
                        id: "read".into(),
                        name: "fs.read".into(),
                        arguments: json!({"path": "src/settings.cfg"}),
                    },
                },
            ),
            event(
                run,
                4,
                30,
                RuntimeEvent::ToolFinished {
                    output: ToolOutput {
                        call_id: "read".into(),
                        tool_name: "fs.read".into(),
                        ok: true,
                        summary: "read".into(),
                        model_content: String::new(),
                        artifact_ref: None,
                        metadata: json!({
                            "path": "src/settings.cfg",
                            "revision": revision,
                            "bytes": 42
                        }),
                    },
                },
            ),
            model_started(run, 5, 40),
            event(
                run,
                6,
                50,
                RuntimeEvent::ToolStarted {
                    call: ToolCall {
                        id: "patch".into(),
                        name: "edit.patch".into(),
                        arguments: json!({
                            "files": [{
                                "path": "src/settings.cfg",
                                "base_revision": revision,
                                "hunks": [
                                    {"old": "mode=legacy", "new": "mode=strict"},
                                    {"old": "timeout=30", "new": "timeout=45"}
                                ]
                            }]
                        }),
                    },
                },
            ),
            event(
                run,
                7,
                70,
                RuntimeEvent::ToolFinished {
                    output: ToolOutput {
                        call_id: "patch".into(),
                        tool_name: "edit.patch".into(),
                        ok: true,
                        summary: "patched".into(),
                        model_content: String::new(),
                        artifact_ref: None,
                        metadata: json!({
                            "changed": true,
                            "files": [{
                                "path": "src/settings.cfg",
                                "changed": true,
                                "hunks": 2,
                                "bytes_before": 42,
                                "bytes_after": 42
                            }]
                        }),
                    },
                },
            ),
            run_completed(run, 8, 80),
        ];

        let report = analyze_cell(task, &events, &[], true, true);
        assert!(report.passed, "{:?}", report.violations);
        assert!(report.valid_call_first_attempt_success);
        assert_eq!(report.edit_to_green_ms, Some(20));
        assert_eq!(report.fs_read_bytes, 42);
    }

    #[test]
    fn malformed_first_patch_and_shell_fallback_fail_separately_from_file_truth() {
        let pack = crate::tool_edit_pack::load_pack().unwrap();
        let task = pack.task("crlf_multi_hunk").unwrap();
        let run = RunId::new();
        let events = vec![
            run_started(run),
            model_started(run, 2, 10),
            event(
                run,
                3,
                20,
                RuntimeEvent::ToolStarted {
                    call: ToolCall {
                        id: "bad".into(),
                        name: "edit.patch".into(),
                        arguments: json!({
                            "path": "src/settings.cfg",
                            "hunks": [{"old": "mode=legacy", "new": "mode=strict"}]
                        }),
                    },
                },
            ),
            event(
                run,
                4,
                30,
                RuntimeEvent::ToolFinished {
                    output: ToolOutput {
                        call_id: "bad".into(),
                        tool_name: "edit.patch".into(),
                        ok: false,
                        summary: "failed".into(),
                        model_content: String::new(),
                        artifact_ref: None,
                        metadata: json!({"failure_class": "no_exact_match"}),
                    },
                },
            ),
            event(
                run,
                5,
                40,
                RuntimeEvent::ToolStarted {
                    call: ToolCall {
                        id: "fallback".into(),
                        name: "shell.exec".into(),
                        arguments: json!({"command": "write file"}),
                    },
                },
            ),
            event(
                run,
                6,
                50,
                RuntimeEvent::ToolFinished {
                    output: ToolOutput {
                        call_id: "fallback".into(),
                        tool_name: "shell.exec".into(),
                        ok: true,
                        summary: "ran".into(),
                        model_content: String::new(),
                        artifact_ref: None,
                        metadata: json!({}),
                    },
                },
            ),
            run_completed(run, 7, 60),
        ];

        let report = analyze_cell(task, &events, &[], true, true);
        assert!(!report.passed);
        assert!(!report.first_patch_shape_valid);
        assert_eq!(report.forbidden_calls, 1);
        assert_eq!(report.non_stale_failures, 1);
        assert!(!report.valid_call_first_attempt_success);
    }

    #[test]
    fn hardcoded_revision_without_a_prior_read_fails_closed() {
        let pack = crate::tool_edit_pack::load_pack().unwrap();
        let task = pack.task("crlf_multi_hunk").unwrap();
        let revision = task.file.seed_files[0].sha256.clone();
        let run = RunId::new();
        let events = vec![
            run_started(run),
            model_started(run, 2, 10),
            model_started(run, 3, 20),
            event(
                run,
                4,
                30,
                RuntimeEvent::ToolStarted {
                    call: ToolCall {
                        id: "patch".into(),
                        name: "edit.patch".into(),
                        arguments: json!({
                            "files": [{
                                "path": "src/settings.cfg",
                                "base_revision": revision,
                                "hunks": [
                                    {"old": "mode=legacy", "new": "mode=strict"},
                                    {"old": "timeout=30", "new": "timeout=45"}
                                ]
                            }]
                        }),
                    },
                },
            ),
            event(
                run,
                5,
                40,
                RuntimeEvent::ToolFinished {
                    output: ToolOutput {
                        call_id: "patch".into(),
                        tool_name: "edit.patch".into(),
                        ok: true,
                        summary: "patched".into(),
                        model_content: String::new(),
                        artifact_ref: None,
                        metadata: json!({
                            "changed": true,
                            "files": [{
                                "path": "src/settings.cfg",
                                "changed": true,
                                "hunks": 2,
                                "bytes_before": 42,
                                "bytes_after": 42
                            }]
                        }),
                    },
                },
            ),
            run_completed(run, 6, 50),
        ];

        let report = analyze_cell(task, &events, &[], true, true);
        assert!(!report.passed);
        assert!(!report.first_patch_revisions_from_latest_reads);
        assert_eq!(report.patch_revision_provenance_failures, 1);
        assert!(!report.valid_call_first_attempt_success);
    }

    #[test]
    fn gate_percentiles_share_the_metrics_rounding_contract() {
        assert_eq!(percentile(&[], 95), 0);
        assert_eq!(percentile(&[10], 95), 10);
        assert_eq!(percentile(&[10, 20], 50), 20);
        assert_eq!(percentile(&[10, 20], 95), 20);
        assert_eq!(percentile(&[10, 20, 30, 40], 95), 40);
    }

    #[test]
    fn stale_gate_accepts_only_explicit_proactive_or_reactive_routes() {
        let pack = crate::tool_edit_pack::load_pack().unwrap();
        let task = pack.task("stale_revision_recovery").unwrap();
        let path = "src/retry.cfg";
        let old_revision = task.file.seed_files[0].sha256.as_str();
        let mutation = mutation_record(task, Some(6));
        let current_revision = mutation.after_sha256.as_str();

        let proactive_run = RunId::new();
        let proactive = vec![
            run_started(proactive_run),
            model_started(proactive_run, 2, 10),
            read_started(proactive_run, 3, 20, "old-read", path),
            read_finished(proactive_run, 4, 30, "old-read", path, old_revision),
            model_started(proactive_run, 5, 40),
            turn_completed(proactive_run, 6, 50),
            model_started(proactive_run, 7, 60),
            read_started(proactive_run, 8, 70, "current-read", path),
            read_finished(proactive_run, 9, 80, "current-read", path, current_revision),
            model_started(proactive_run, 10, 90),
            patch_started(proactive_run, 11, 100, "patch", path, current_revision),
            patch_finished(proactive_run, 12, 110, "patch", path, None, None),
            run_completed(proactive_run, 13, 120),
        ];
        let report = analyze_cell(
            task,
            &proactive,
            std::slice::from_ref(&mutation),
            true,
            true,
        );
        assert!(report.passed, "{:?}", report.violations);
        assert_eq!(report.patch_attempts, 1);
        assert_eq!(report.conflict_route.as_deref(), Some("proactive"));

        let reactive_run = RunId::new();
        let reactive = vec![
            run_started(reactive_run),
            model_started(reactive_run, 2, 10),
            read_started(reactive_run, 3, 20, "old-read", path),
            read_finished(reactive_run, 4, 30, "old-read", path, old_revision),
            model_started(reactive_run, 5, 40),
            turn_completed(reactive_run, 6, 50),
            model_started(reactive_run, 7, 60),
            patch_started(reactive_run, 8, 70, "stale", path, old_revision),
            patch_finished(
                reactive_run,
                9,
                80,
                "stale",
                path,
                Some(ToolFailureClass::StaleRevision),
                Some(current_revision),
            ),
            model_started(reactive_run, 10, 90),
            read_started(reactive_run, 11, 100, "current-read", path),
            read_finished(
                reactive_run,
                12,
                110,
                "current-read",
                path,
                current_revision,
            ),
            model_started(reactive_run, 13, 120),
            patch_started(reactive_run, 14, 130, "retry", path, current_revision),
            patch_finished(reactive_run, 15, 140, "retry", path, None, None),
            run_completed(reactive_run, 16, 150),
        ];
        let report = analyze_cell(task, &reactive, std::slice::from_ref(&mutation), true, true);
        assert!(report.passed, "{:?}", report.violations);
        assert_eq!(report.patch_attempts, 2);
        assert_eq!(report.stale_refusals, 1);
        assert_eq!(report.conflict_route.as_deref(), Some("reactive"));
    }

    #[test]
    fn post_boundary_double_read_is_not_proactive_and_old_sidecars_fail_closed() {
        let pack = crate::tool_edit_pack::load_pack().unwrap();
        let task = pack.task("stale_revision_recovery").unwrap();
        let path = "src/retry.cfg";
        let mutation = mutation_record(task, Some(3));
        let current_revision = mutation.after_sha256.as_str();
        let run = RunId::new();
        let events = vec![
            run_started(run),
            model_started(run, 2, 10),
            turn_completed(run, 3, 20),
            model_started(run, 4, 30),
            read_started(run, 5, 40, "current-1", path),
            read_finished(run, 6, 50, "current-1", path, current_revision),
            model_started(run, 7, 60),
            read_started(run, 8, 70, "current-2", path),
            read_finished(run, 9, 80, "current-2", path, current_revision),
            model_started(run, 10, 90),
            patch_started(run, 11, 100, "patch", path, current_revision),
            patch_finished(run, 12, 110, "patch", path, None, None),
            run_completed(run, 13, 120),
        ];

        let report = analyze_cell(task, &events, std::slice::from_ref(&mutation), true, true);
        assert!(!report.passed);
        assert!(report.fixture_mutation_evidence_valid);
        assert_eq!(report.conflict_route, None);
        assert!(report.violations.iter().any(|violation| {
            violation.contains("neither the proactive-read nor stale-refusal")
        }));

        let mut legacy_record = mutation;
        legacy_record.event_seq_before = None;
        let encoded = serde_json::to_value(&legacy_record).unwrap();
        let mut legacy_json = encoded.as_object().unwrap().clone();
        legacy_json.remove("event_seq_before");
        let legacy_record: FixtureMutationRecord =
            serde_json::from_value(Value::Object(legacy_json)).unwrap();
        let report = analyze_cell(task, &events, &[legacy_record], true, true);
        assert!(!report.fixture_mutation_evidence_valid);
        assert!(!report.passed);
    }

    #[test]
    fn whole_file_or_sentinel_hunks_do_not_match_the_exact_edit_contract() {
        let pack = crate::tool_edit_pack::load_pack().unwrap();
        let task = pack.task("crlf_multi_hunk").unwrap();
        let args = json!({
            "files": [{
                "path": "src/settings.cfg",
                "base_revision": task.file.seed_files[0].sha256,
                "hunks": [
                    {
                        "old": "mode=legacy\ntimeout=30\nsentinel=keep\n",
                        "new": "mode=strict\ntimeout=45\nsentinel=keep\n"
                    },
                    {"old": "sentinel=keep", "new": "sentinel=temporary"}
                ]
            }]
        });
        let shape = validate_patch_shape(&args, true);
        assert!(shape.valid);
        assert_ne!(shape.hunk_fingerprints, expected_hunk_fingerprints(task));
    }

    #[test]
    fn exact_hunk_fingerprints_accept_equivalent_logical_newline_anchors() {
        let pack = crate::tool_edit_pack::load_pack().unwrap();
        let task = pack.task("mixed_eol").unwrap();
        let args = json!({
            "files": [{
                "path": "src/mixed.cfg",
                "base_revision": task.file.seed_files[0].sha256,
                "hunks": [
                    {"old": "alpha=old\r\nbeta=keep\n", "new": "alpha=new\r\nbeta=keep\n"},
                    {"old": "gamma=old\r\ndelta=keep", "new": "gamma=new\r\ndelta=keep"}
                ]
            }]
        });
        let shape = validate_patch_shape(&args, true);
        assert!(shape.valid);
        assert_eq!(shape.hunk_fingerprints, expected_hunk_fingerprints(task));
    }

    #[test]
    fn batch_shape_requires_unique_revisioned_files_and_bounded_hunks() {
        let pack = crate::tool_edit_pack::load_pack().unwrap();
        let task = pack.task("batch_two_file").unwrap();
        let args = json!({
            "files": [
                {
                    "path": "src/library.cfg",
                    "base_revision": task.file.seed_files[0].sha256,
                    "hunks": [{"old": "api=v1", "new": "api=v2"}]
                },
                {
                    "path": "src/client.cfg",
                    "base_revision": task.file.seed_files[1].sha256,
                    "hunks": [{"old": "uses=v1", "new": "uses=v2", "occurrence": 1}]
                }
            ]
        });
        let shape = validate_patch_shape(&args, true);
        assert!(shape.valid);
        assert!(shape.has_required_revisions);
        assert_eq!(shape.hunks, 2);

        let duplicate = json!({
            "files": [
                {"path": "src/x", "base_revision": "a".repeat(64), "hunks": [{"old":"a", "new":"b"}]},
                {"path": "src/x", "base_revision": "b".repeat(64), "hunks": [{"old":"b", "new":"c"}]}
            ]
        });
        assert!(!validate_patch_shape(&duplicate, true).valid);

        let one_file = json!({
            "files": [{
                "path": "src/library.cfg",
                "base_revision": task.file.seed_files[0].sha256,
                "hunks": [{"old": "api=v1", "new": "api=v2"}]
            }]
        });
        let shape = validate_patch_shape(&one_file, true);
        assert!(
            shape.valid,
            "one files[] entry is the canonical single-file shape"
        );
        assert!(shape.has_required_revisions);

        let legacy_top_revision = json!({
            "base_revision": task.file.seed_files[0].sha256,
            "files": [{
                "path": "src/library.cfg",
                "hunks": [{"old": "api=v1", "new": "api=v2"}]
            }]
        });
        assert!(!validate_patch_shape(&legacy_top_revision, true).valid);

        let legacy_shortcut = json!({
            "path": "src/library.cfg",
            "base_revision": task.file.seed_files[0].sha256,
            "hunks": [{"old": "api=v1", "new": "api=v2"}]
        });
        assert!(!validate_patch_shape(&legacy_shortcut, true).valid);

        let unknown_field = json!({
            "files": [{
                "path": "src/library.cfg",
                "base_revision": task.file.seed_files[0].sha256,
                "hunks": [{"old": "api=v1", "new": "api=v2", "junk": true}]
            }]
        });
        assert!(!validate_patch_shape(&unknown_field, true).valid);

        let dummy_hunk = json!({
            "files": [{
                "path": "src/library.cfg",
                "base_revision": task.file.seed_files[0].sha256,
                "hunks": [{"old": "api=v1", "new": "api=v1"}]
            }]
        });
        assert!(!validate_patch_shape(&dummy_hunk, true).valid);
    }
}
