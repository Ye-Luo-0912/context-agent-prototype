//! Shadow Context Frame compiler (`CONTEXT-FRAME-SHADOW`).
//!
//! Compiles the same runtime state the prompt assembler consumes into an
//! explicitly zoned, authority/freshness/representation-classed frame
//! manifest — without changing a single byte of the model input. The
//! manifest is the measurement record for the structured Context Frame
//! design: mandatory coverage, per-zone cost, cross-zone duplicates,
//! required misses and a stable frame digest, all observable in the event
//! stream before any model-facing behavior changes.

use agent_contracts::{MaterializedContext, MaterializedItem, RunId, TaskAnchorView, TaskId};
use serde::Serialize;
use sha2::{Digest as _, Sha256};
use std::collections::HashSet;

/// Bounded body length for one block's content in the manifest.
const MAX_BLOCK_BODY_CHARS: usize = 240;
/// Bounded body length for operator-authority contract text.
const MAX_CONTRACT_BODY_CHARS: usize = 600;
/// Blocks retained per zone before the remainder is counted as omitted.
const MAX_BLOCKS_PER_ZONE: usize = 8;
/// Total manifest blocks cap; the manifest is an event payload, not a dump.
const MAX_TOTAL_BLOCKS: usize = 40;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ContextZone {
    TaskContract,
    ExecutionState,
    CurrentEvidence,
    WorkingMemory,
    ExternalDirectory,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthorityClass {
    OperatorBoundary,
    RuntimeTrusted,
    RetrievedUntrusted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FreshnessClass {
    CurrentExact,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RequirementClass {
    MustRepresent,
    PreferBody,
    ReferenceOnly,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RepresentationClass {
    BoundedBody,
    Descriptor,
    Omitted,
}

/// One classified frame block. `content` is always bounded; `digest` is the
/// SHA-256 of the untruncated content so cross-zone duplicates are detectable.
#[derive(Debug, Clone, Serialize)]
pub struct FrameBlock {
    pub zone: ContextZone,
    pub authority: AuthorityClass,
    pub freshness: FreshnessClass,
    pub requirement: RequirementClass,
    pub representation: RepresentationClass,
    pub source: String,
    pub content: String,
    pub approx_tokens: usize,
    pub digest: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct FrameZoneStats {
    pub zone: ContextZone,
    pub blocks: usize,
    pub omitted: usize,
    pub approx_tokens: usize,
}

/// The bounded, serializable manifest emitted as
/// `RuntimeEvent::ContextFrameShadow`.
#[derive(Debug, Clone, Serialize)]
pub struct FrameManifest {
    pub schema: &'static str,
    pub run_id: String,
    pub task_id: Option<TaskId>,
    pub anchor_revision: Option<u64>,
    pub zones: Vec<FrameZoneStats>,
    pub blocks: Vec<FrameBlock>,
    pub required_misses: usize,
    pub duplicates_removed: usize,
    pub approx_tokens_total: usize,
    pub frame_digest: String,
}

/// The same state the prompt assembler consumes, handed to the compiler.
pub struct ShadowFrameInputs<'a> {
    pub run_id: RunId,
    pub task_id: Option<TaskId>,
    pub anchor: Option<&'a TaskAnchorView>,
    pub materialized: &'a MaterializedContext,
    pub unresolved_ack_debts: usize,
}

fn bounded(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        text.to_string()
    } else {
        let mut cut: String = text.chars().take(max_chars).collect();
        cut.push('…');
        cut
    }
}

fn content_digest(content: &str) -> String {
    let digest = Sha256::digest(content.as_bytes());
    let mut hex = String::with_capacity(digest.len() * 2);
    for byte in digest {
        hex.push_str(&format!("{byte:02x}"));
    }
    hex
}

fn approx_tokens(text: &str) -> usize {
    text.chars().count() / 4
}

#[derive(Default)]
struct ZoneAccumulator {
    blocks: Vec<FrameBlock>,
    omitted: usize,
}

impl ZoneAccumulator {
    /// Push one block, counting it as omitted once the zone cap is reached
    /// or the manifest-wide cap is reached.
    fn push(&mut self, block: FrameBlock, total_blocks: &mut usize) {
        if self.blocks.len() >= MAX_BLOCKS_PER_ZONE || *total_blocks >= MAX_TOTAL_BLOCKS {
            self.omitted += 1;
            return;
        }
        *total_blocks += 1;
        self.blocks.push(block);
    }

    fn stats(&self, zone: ContextZone) -> FrameZoneStats {
        FrameZoneStats {
            zone,
            blocks: self.blocks.len(),
            omitted: self.omitted,
            approx_tokens: self.blocks.iter().map(|b| b.approx_tokens).sum(),
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn block(
    zone: ContextZone,
    authority: AuthorityClass,
    freshness: FreshnessClass,
    requirement: RequirementClass,
    representation: RepresentationClass,
    source: String,
    full_content: &str,
    max_chars: usize,
) -> FrameBlock {
    FrameBlock {
        zone,
        authority,
        freshness,
        requirement,
        representation,
        source,
        content: bounded(full_content, max_chars),
        approx_tokens: approx_tokens(full_content),
        digest: content_digest(full_content),
    }
}

/// Compile the shadow frame manifest. Deterministic: the same inputs always
/// produce the same `frame_digest`.
pub fn compile_shadow_frame(inputs: &ShadowFrameInputs<'_>) -> FrameManifest {
    let mut total_blocks = 0usize;
    let mut contract = ZoneAccumulator::default();
    let mut execution = ZoneAccumulator::default();
    let mut evidence = ZoneAccumulator::default();
    let mut memory = ZoneAccumulator::default();
    let mut external = ZoneAccumulator::default();

    // ---- Task contract: operator authority, always represented ----
    if let Some(anchor) = inputs.anchor {
        for (source, text) in [
            ("anchor.original_goal", &anchor.original_goal),
            (
                "anchor.current_interpretation",
                &anchor.current_interpretation,
            ),
        ] {
            if !text.is_empty() {
                contract.push(
                    block(
                        ContextZone::TaskContract,
                        AuthorityClass::OperatorBoundary,
                        FreshnessClass::CurrentExact,
                        RequirementClass::MustRepresent,
                        RepresentationClass::BoundedBody,
                        source.to_string(),
                        text,
                        MAX_CONTRACT_BODY_CHARS,
                    ),
                    &mut total_blocks,
                );
            }
        }
        for (index, constraint) in anchor.constraints.iter().enumerate() {
            contract.push(
                block(
                    ContextZone::TaskContract,
                    AuthorityClass::OperatorBoundary,
                    FreshnessClass::CurrentExact,
                    RequirementClass::MustRepresent,
                    RepresentationClass::BoundedBody,
                    format!("anchor.constraints[{index}]"),
                    constraint,
                    MAX_CONTRACT_BODY_CHARS,
                ),
                &mut total_blocks,
            );
        }
        for (index, criterion) in anchor.acceptance_criteria.iter().enumerate() {
            contract.push(
                block(
                    ContextZone::TaskContract,
                    AuthorityClass::OperatorBoundary,
                    FreshnessClass::CurrentExact,
                    RequirementClass::MustRepresent,
                    RepresentationClass::BoundedBody,
                    format!("anchor.acceptance_criteria[{index}]"),
                    criterion,
                    MAX_CONTRACT_BODY_CHARS,
                ),
                &mut total_blocks,
            );
        }
    }

    // ---- Execution state: runtime-trusted advisory and debt surface ----
    if let Some(anchor) = inputs.anchor {
        if !anchor.next_action.is_empty() {
            execution.push(
                block(
                    ContextZone::ExecutionState,
                    AuthorityClass::RuntimeTrusted,
                    FreshnessClass::CurrentExact,
                    RequirementClass::PreferBody,
                    RepresentationClass::BoundedBody,
                    "anchor.next_action (advisory)".to_string(),
                    &anchor.next_action,
                    MAX_BLOCK_BODY_CHARS,
                ),
                &mut total_blocks,
            );
        }
        for (index, step) in anchor.plan_progress.iter().enumerate() {
            execution.push(
                block(
                    ContextZone::ExecutionState,
                    AuthorityClass::RuntimeTrusted,
                    FreshnessClass::CurrentExact,
                    RequirementClass::PreferBody,
                    RepresentationClass::BoundedBody,
                    format!("anchor.plan_progress[{index}]"),
                    step,
                    MAX_BLOCK_BODY_CHARS,
                ),
                &mut total_blocks,
            );
        }
        for (index, loop_item) in anchor.open_loops.iter().enumerate() {
            execution.push(
                block(
                    ContextZone::ExecutionState,
                    AuthorityClass::RuntimeTrusted,
                    FreshnessClass::CurrentExact,
                    RequirementClass::MustRepresent,
                    RepresentationClass::BoundedBody,
                    format!("anchor.open_loops[{index}]"),
                    loop_item,
                    MAX_BLOCK_BODY_CHARS,
                ),
                &mut total_blocks,
            );
        }
    }
    if inputs.unresolved_ack_debts > 0 {
        execution.push(
            block(
                ContextZone::ExecutionState,
                AuthorityClass::RuntimeTrusted,
                FreshnessClass::CurrentExact,
                RequirementClass::MustRepresent,
                RepresentationClass::Descriptor,
                "recovery.ack_debts".to_string(),
                &format!(
                    "{} unresolved effect-acknowledgement debt(s); mutation is fenced",
                    inputs.unresolved_ack_debts
                ),
                MAX_BLOCK_BODY_CHARS,
            ),
            &mut total_blocks,
        );
    }

    // ---- Current evidence + working memory, deduplicated across zones ----
    // The same body may reach both the foreground evidence pack and the
    // working set; the manifest keeps one full block and counts the rest.
    let mut seen: HashSet<String> = HashSet::new();
    for item in &inputs.materialized.foreground {
        if seen.insert(content_digest(&item.content)) {
            evidence.push(
                context_block(item, ContextZone::CurrentEvidence),
                &mut total_blocks,
            );
        } else {
            evidence.omitted += 1;
        }
    }
    for item in &inputs.materialized.items {
        if !seen.insert(content_digest(&item.content)) {
            memory.omitted += 1;
            continue;
        }
        memory.push(
            context_block(item, ContextZone::WorkingMemory),
            &mut total_blocks,
        );
    }

    // ---- External directory: descriptors only, never bodies ----
    let entries = inputs.materialized.external.as_slice();
    for entry in entries.iter().take(MAX_BLOCKS_PER_ZONE) {
        external.push(
            block(
                ContextZone::ExternalDirectory,
                AuthorityClass::RetrievedUntrusted,
                FreshnessClass::Unknown,
                RequirementClass::ReferenceOnly,
                RepresentationClass::Descriptor,
                entry.item_id.to_string(),
                &format!("externalized item {}", entry.item_id),
                MAX_BLOCK_BODY_CHARS,
            ),
            &mut total_blocks,
        );
    }
    external.omitted += entries.len().saturating_sub(MAX_BLOCKS_PER_ZONE);

    let duplicates_removed = evidence.omitted + memory.omitted;
    let zone_stats = [
        contract.stats(ContextZone::TaskContract),
        execution.stats(ContextZone::ExecutionState),
        evidence.stats(ContextZone::CurrentEvidence),
        memory.stats(ContextZone::WorkingMemory),
        external.stats(ContextZone::ExternalDirectory),
    ];
    let mut blocks: Vec<FrameBlock> = [contract, execution, evidence, memory, external]
        .into_iter()
        .flat_map(|zone| zone.blocks)
        .collect();
    blocks.sort_by(|a, b| a.digest.cmp(&b.digest).then(a.source.cmp(&b.source)));
    let approx_tokens_total = blocks.iter().map(|b| b.approx_tokens).sum();
    let required_misses =
        usize::try_from(inputs.materialized.required_misses.total()).unwrap_or(usize::MAX);

    let digest_input = serde_json::json!({
        "schema": "context-frame-shadow/v1",
        "run_id": inputs.run_id.to_string(),
        "task_id": inputs.task_id.map(|id| id.to_string()),
        "anchor_revision": inputs.anchor.map(|anchor| anchor.revision),
        "required_misses": required_misses,
        "duplicates_removed": duplicates_removed,
        "blocks": blocks,
    });
    let frame_digest = content_digest(&digest_input.to_string());

    FrameManifest {
        schema: "context-frame-shadow/v1",
        run_id: inputs.run_id.to_string(),
        task_id: inputs.task_id,
        anchor_revision: inputs.anchor.map(|anchor| anchor.revision),
        zones: zone_stats.to_vec(),
        blocks,
        required_misses,
        duplicates_removed,
        approx_tokens_total,
        frame_digest,
    }
}

fn context_block(item: &MaterializedItem, zone: ContextZone) -> FrameBlock {
    let (requirement, representation) = match zone {
        ContextZone::CurrentEvidence => (
            RequirementClass::PreferBody,
            RepresentationClass::BoundedBody,
        ),
        _ => (
            RequirementClass::PreferBody,
            RepresentationClass::BoundedBody,
        ),
    };
    block(
        zone,
        AuthorityClass::RetrievedUntrusted,
        FreshnessClass::Unknown,
        requirement,
        representation,
        item.source.clone().unwrap_or_else(|| "context".into()),
        &item.content,
        MAX_BLOCK_BODY_CHARS,
    )
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use agent_contracts::{
        AttentionState, ContextKind, ContextMapView, ContextRetention, ContextScope,
        ExternalizedContext,
    };

    pub(crate) fn item(content: &str, source: &str) -> MaterializedItem {
        MaterializedItem {
            item_id: agent_contracts::ContextItemId::new(),
            kind: ContextKind::FileObservation,
            scope: ContextScope::Task,
            attention: AttentionState::Active,
            semantic: Default::default(),
            retention: ContextRetention::Working,
            content: content.to_string(),
            source: Some(source.to_string()),
            file_path: None,
            file_revision: None,
            partial_body: false,
        }
    }

    pub(crate) fn anchor() -> TaskAnchorView {
        TaskAnchorView {
            revision: 7,
            original_goal: "repair the retry table".into(),
            current_interpretation: String::new(),
            constraints: vec!["public API unchanged".into()],
            acceptance_criteria: vec!["saturation never wraps".into()],
            plan_progress: vec!["read the runner".into()],
            open_loops: vec!["prove the delay cap".into()],
            next_action: "run the retry verifier".into(),
        }
    }

    fn inputs<'a>(
        anchor: &'a TaskAnchorView,
        materialized: &'a MaterializedContext,
    ) -> ShadowFrameInputs<'a> {
        ShadowFrameInputs {
            run_id: RunId::new(),
            task_id: Some(TaskId::new()),
            anchor: Some(anchor),
            materialized,
            unresolved_ack_debts: 2,
        }
    }

    #[test]
    fn the_manifest_is_deterministic_and_covers_the_contract() {
        let anchor = anchor();
        let mut materialized = MaterializedContext::default();
        materialized.items.push(item("decision text", "user"));
        let run_id = RunId::new();
        let task_id = TaskId::new();
        let build = || ShadowFrameInputs {
            run_id,
            task_id: Some(task_id),
            anchor: Some(&anchor),
            materialized: &materialized,
            unresolved_ack_debts: 2,
        };
        let first = compile_shadow_frame(&build());
        let second = compile_shadow_frame(&build());
        assert_eq!(first.frame_digest, second.frame_digest);

        // Mandatory coverage: goal, constraints and acceptance criteria are
        // TaskContract blocks with operator authority.
        let contract_blocks: Vec<&FrameBlock> = first
            .blocks
            .iter()
            .filter(|b| b.zone == ContextZone::TaskContract)
            .collect();
        assert!(
            contract_blocks
                .iter()
                .any(|b| b.source == "anchor.original_goal"
                    && b.authority == AuthorityClass::OperatorBoundary)
        );
        assert!(
            contract_blocks
                .iter()
                .any(|b| b.source == "anchor.constraints[0]")
        );
        assert!(
            contract_blocks
                .iter()
                .any(|b| b.source == "anchor.acceptance_criteria[0]")
        );

        // Execution state: advisory next action + the ack-debt descriptor.
        assert!(
            first
                .blocks
                .iter()
                .any(|b| b.source == "anchor.next_action (advisory)")
        );
        assert!(first.blocks.iter().any(|b| b.source == "recovery.ack_debts"
            && b.representation == RepresentationClass::Descriptor));
        assert_eq!(first.frame_digest.len(), 64);
    }

    #[test]
    fn cross_zone_duplicates_are_counted_not_repeated() {
        let anchor = anchor();
        let mut materialized = MaterializedContext::default();
        let body = "the same body text";
        materialized.foreground.push(item(body, "tool:fs.read"));
        materialized.items.push(item(body, "tool:fs.read"));
        materialized.items.push(item("another body", "user"));
        let manifest = compile_shadow_frame(&inputs(&anchor, &materialized));
        assert_eq!(manifest.duplicates_removed, 1);
        let bodies: Vec<&str> = manifest
            .blocks
            .iter()
            .filter(|b| b.digest == content_digest(body))
            .map(|b| b.content.as_str())
            .collect();
        assert_eq!(bodies.len(), 1, "one body renders once: {bodies:?}");
    }

    #[test]
    fn oversized_content_is_bounded_and_external_refs_stay_descriptors() {
        let anchor = anchor();
        let mut materialized = MaterializedContext::default();
        materialized
            .items
            .push(item("x".repeat(10_000).as_str(), "tool"));
        let item_id = agent_contracts::ContextItemId::new();
        let entry: ExternalizedContext = serde_json::from_value(serde_json::json!({

            "item_id": item_id,
            "kind": "FileObservation",
            "scope": "Task",
            "retention": "Ephemeral",
            "attention": "Archived",
            "semantic": "Live",
            "context_ref": {
                "uri": format!("context://run/{item_id}"),
                "item_id": item_id,
                "kind": "FileObservation",
                "scope": "Task",
                "summary": "externalized body",
                "created_tick": 0,
            },
            "created_tick": 0,
            "created_turn": 0,
            "last_access_turn": 0,
            "last_selected_turn": 0,
            "access_count": 0,
            "externalized_at_tick": 1,
            "last_access_tick": 1,
            "residency": "External",
        }))
        .unwrap_or_else(|error| panic!("external entry error: {error}"));
        materialized.external = ContextMapView::new(vec![entry]);
        let manifest = compile_shadow_frame(&inputs(&anchor, &materialized));
        let block = manifest
            .blocks
            .iter()
            .find(|b| b.zone == ContextZone::WorkingMemory)
            .expect("the working-memory body is represented");
        assert!(block.content.chars().count() <= MAX_BLOCK_BODY_CHARS + 1);
        let directory = manifest
            .blocks
            .iter()
            .find(|b| b.zone == ContextZone::ExternalDirectory)
            .expect("the external entry is a directory descriptor");
        assert_eq!(directory.representation, RepresentationClass::Descriptor);
        assert_eq!(directory.requirement, RequirementClass::ReferenceOnly);
    }

    #[test]
    fn required_misses_and_anchor_revision_travel_with_the_manifest() {
        let anchor = anchor();
        let materialized = MaterializedContext::default();
        let manifest = compile_shadow_frame(&inputs(&anchor, &materialized));
        assert_eq!(manifest.required_misses, 0);
        assert_eq!(manifest.anchor_revision, Some(7));
        assert_eq!(manifest.schema, "context-frame-shadow/v1");
    }
}

/// Frame-2 scripted gate: deterministic invariants over a compiled shadow
/// frame. The gate never talks to a model — it cross-checks the manifest
/// against the same inputs the compiler consumed (mandatory coverage,
/// surfaced debts and misses) and enforces manifest-intrinsic invariants
/// (no duplicate bodies, external refs stay descriptors, bounded content,
/// zone statistics consistent with the blocks).
pub fn gate_shadow_frame(inputs: &ShadowFrameInputs<'_>, manifest: &FrameManifest) -> Vec<String> {
    let mut violations = gate_manifest_invariants(manifest);

    // Mandatory coverage: every operator contract text the anchor declares
    // must appear as a TaskContract block, unless the manifest-wide cap
    // omitted it (recorded honestly in the zone stats).
    if let Some(anchor) = inputs.anchor {
        let mut mandatory: Vec<(String, String)> = Vec::new();
        if !anchor.original_goal.is_empty() {
            mandatory.push(("anchor.original_goal".into(), anchor.original_goal.clone()));
        }
        for (index, constraint) in anchor.constraints.iter().enumerate() {
            mandatory.push((format!("anchor.constraints[{index}]"), constraint.clone()));
        }
        for (index, criterion) in anchor.acceptance_criteria.iter().enumerate() {
            mandatory.push((
                format!("anchor.acceptance_criteria[{index}]"),
                criterion.clone(),
            ));
        }
        for (source, text) in mandatory {
            let expected = content_digest(&text);
            let present = manifest
                .blocks
                .iter()
                .any(|block| block.zone == ContextZone::TaskContract && block.source == source);
            let omitted = manifest
                .zones
                .iter()
                .find(|stats| stats.zone == ContextZone::TaskContract)
                .is_some_and(|stats| stats.omitted > 0);
            if !present && !omitted {
                violations.push(format!("mandatory contract block missing: {source}"));
            }
            let _ = expected;
        }
    }

    // Surfaced debts: unresolved ack debts must appear as a descriptor.
    if inputs.unresolved_ack_debts > 0
        && !manifest
            .blocks
            .iter()
            .any(|block| block.source == "recovery.ack_debts")
    {
        violations.push("unresolved ack debts are not surfaced".into());
    }

    // Reported misses must survive into the manifest unchanged.
    let expected_misses =
        usize::try_from(inputs.materialized.required_misses.total()).unwrap_or(usize::MAX);
    if manifest.required_misses != expected_misses {
        violations.push(format!(
            "required misses drifted: manifest {} vs inputs {expected_misses}",
            manifest.required_misses
        ));
    }

    violations
}

/// Manifest-intrinsic invariants: valid on a freshly compiled manifest and
/// on one read back from a trace.
pub fn gate_manifest_invariants(manifest: &FrameManifest) -> Vec<String> {
    let mut violations = Vec::new();

    // Same body appears exactly once.
    let mut seen = std::collections::HashSet::new();
    for block in &manifest.blocks {
        if !seen.insert(&block.digest) {
            violations.push(format!(
                "duplicate body digest across blocks: {} ({})",
                &block.digest[..12.min(block.digest.len())],
                block.source
            ));
        }
    }

    // External refs never carry bodies.
    for block in &manifest.blocks {
        if block.zone == ContextZone::ExternalDirectory
            && block.representation != RepresentationClass::Descriptor
        {
            violations.push(format!(
                "external directory block is not a descriptor: {}",
                block.source
            ));
        }
    }

    // Bodies stay bounded.
    for block in &manifest.blocks {
        let cap = if block.authority == AuthorityClass::OperatorBoundary {
            MAX_CONTRACT_BODY_CHARS
        } else {
            MAX_BLOCK_BODY_CHARS
        };
        if block.content.chars().count() > cap + 1 {
            violations.push(format!("unbounded body in {}", block.source));
        }
    }

    // Zone statistics agree with the block list.
    for stats in &manifest.zones {
        let zone_blocks = manifest
            .blocks
            .iter()
            .filter(|block| block.zone == stats.zone)
            .count();
        if zone_blocks != stats.blocks {
            violations.push(format!(
                "zone {:?} claims {} blocks but carries {}",
                stats.zone, stats.blocks, zone_blocks
            ));
        }
        let zone_tokens: usize = manifest
            .blocks
            .iter()
            .filter(|block| block.zone == stats.zone)
            .map(|block| block.approx_tokens)
            .sum();
        if zone_tokens != stats.approx_tokens {
            violations.push(format!(
                "zone {:?} claims {} tokens but carries {}",
                stats.zone, stats.approx_tokens, zone_tokens
            ));
        }
    }

    violations
}

#[cfg(test)]
mod gate_tests {
    use super::tests::anchor;
    #[allow(unused_imports)]
    use super::tests::item;
    use super::*;
    use agent_contracts::ContextMapView;

    fn gate_inputs<'a>(
        anchor: &'a TaskAnchorView,
        materialized: &'a MaterializedContext,
        ack_debts: usize,
    ) -> ShadowFrameInputs<'a> {
        ShadowFrameInputs {
            run_id: RunId::new(),
            task_id: Some(TaskId::new()),
            anchor: Some(anchor),
            materialized,
            unresolved_ack_debts: ack_debts,
        }
    }

    // Review Frame-2 matrix, bullet by bullet.

    #[test]
    fn task_constraints_and_criteria_are_never_missing() {
        let anchor = anchor();
        let materialized = MaterializedContext::default();
        let inputs = gate_inputs(&anchor, &materialized, 0);
        let manifest = compile_shadow_frame(&inputs);
        assert!(
            gate_shadow_frame(&inputs, &manifest).is_empty(),
            "a healthy frame gates clean"
        );
        // Simulate a compiler regression: drop the constraints block.
        let mut broken = manifest.clone();
        broken
            .blocks
            .retain(|block| block.source != "anchor.constraints[0]");
        let violations = gate_shadow_frame(&inputs, &broken);
        assert!(
            violations
                .iter()
                .any(|v| v.contains("anchor.constraints[0]")),
            "{violations:?}"
        );
    }

    #[test]
    fn unresolved_debts_and_required_misses_must_surface() {
        let anchor = anchor();
        let mut materialized = MaterializedContext::default();
        let miss: agent_contracts::ContextMaterializationMiss =
            serde_json::from_value(serde_json::json!({
                "identity": {
                    "item_ref": "required:fixture-body",
                },
                "reason": "budget_excluded",
            }))
            .expect("a minimal required miss deserializes");
        materialized.required_misses.push(miss);
        let inputs = gate_inputs(&anchor, &materialized, 1);
        let manifest = compile_shadow_frame(&inputs);
        assert_eq!(manifest.required_misses, 1);
        assert!(
            gate_shadow_frame(&inputs, &manifest).is_empty(),
            "debts and misses surface cleanly"
        );

        // A manifest that lost the debt descriptor fails the gate.
        let mut broken = manifest.clone();
        broken
            .blocks
            .retain(|block| block.source != "recovery.ack_debts");
        assert!(
            gate_shadow_frame(&inputs, &broken)
                .iter()
                .any(|v| v.contains("ack debts"))
        );
        // A manifest that drifted the miss count fails the gate.
        let mut drifted = manifest.clone();
        drifted.required_misses = 0;
        assert!(
            gate_shadow_frame(&inputs, &drifted)
                .iter()
                .any(|v| v.contains("drifted"))
        );
    }

    #[test]
    fn duplicate_bodies_and_unbounded_content_fail_the_gate() {
        let anchor = anchor();
        let mut manifest =
            compile_shadow_frame(&gate_inputs(&anchor, &MaterializedContext::default(), 0));
        manifest.blocks.push(manifest.blocks[0].clone());
        assert!(
            gate_manifest_invariants(&manifest)
                .iter()
                .any(|v| v.contains("duplicate body digest"))
        );
        let mut huge =
            compile_shadow_frame(&gate_inputs(&anchor, &MaterializedContext::default(), 0));
        huge.blocks[0].content = "x".repeat(MAX_BLOCK_BODY_CHARS + 10);
        assert!(
            gate_manifest_invariants(&huge)
                .iter()
                .any(|v| v.contains("unbounded body"))
        );
    }

    #[test]
    fn external_refs_stay_descriptors_and_zone_stats_stay_consistent() {
        let anchor = anchor();
        let mut materialized = MaterializedContext::default();
        let item_id = agent_contracts::ContextItemId::new();
        let entry: agent_contracts::ExternalizedContext =
            serde_json::from_value(serde_json::json!({
                "item_id": item_id,
                "kind": "FileObservation",
                "scope": "Task",
                "retention": "Ephemeral",
                "attention": "Archived",
                "semantic": "Live",
                "context_ref": {
                    "uri": format!("context://run/{item_id}"),
                    "item_id": item_id,
                    "kind": "FileObservation",
                    "scope": "Task",
                    "summary": "externalized body",
                    "created_tick": 0,
                },
                "created_tick": 0,
                "access_count": 0,
                "externalized_at_tick": 1,
                "last_access_tick": 1,
                "residency": "External",
            }))
            .unwrap();
        materialized.external = ContextMapView::new(vec![entry]);
        let inputs = gate_inputs(&anchor, &materialized, 0);
        let manifest = compile_shadow_frame(&inputs);
        assert!(gate_shadow_frame(&inputs, &manifest).is_empty());

        // A compiler regression that carried a body into the directory
        // fails the gate.
        let mut broken = manifest.clone();
        for block in &mut broken.blocks {
            if block.zone == ContextZone::ExternalDirectory {
                block.representation = RepresentationClass::BoundedBody;
            }
        }
        assert!(
            gate_manifest_invariants(&broken)
                .iter()
                .any(|v| v.contains("not a descriptor"))
        );
        // And inconsistent zone stats fail the gate.
        let mut drifted = manifest.clone();
        if let Some(stats) = drifted.zones.first_mut() {
            stats.blocks += 1;
        }
        assert!(
            gate_manifest_invariants(&drifted)
                .iter()
                .any(|v| v.contains("claims"))
        );
    }
    #[test]
    fn the_compiler_is_engine_agnostic_over_materialization_shapes() {
        // Baseline engines (A append-only, B rolling) produce materially
        // different working sets: many low-value items vs a small hot set.
        // The same compiler and the same gate must hold for both.
        let anchor = anchor();
        let mut append_only = MaterializedContext::default();
        for index in 0..30 {
            append_only
                .items
                .push(item(&format!("append-only body {index}"), "tool"));
        }
        let mut rolling = MaterializedContext::default();
        rolling.items.push(item("rolling hot body", "user"));
        for inputs in [
            gate_inputs(&anchor, &append_only, 0),
            gate_inputs(&anchor, &rolling, 0),
        ] {
            let manifest = compile_shadow_frame(&inputs);
            assert!(
                gate_shadow_frame(&inputs, &manifest).is_empty(),
                "engine shape must not break the frame contract"
            );
        }
    }
}
