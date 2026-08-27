//! Typed host/runtime-trusted execution facts substrate.
//!
//! Producer-stamped `metadata` keys became de-facto authority: the runtime
//! reads `path` / `revision` / `files[]` as resource facts, `verification`
//! / `intent` as verification, `mutates_workspace` as the mutation bound,
//! and the Core-owned `_runtime` block as the failure class. Operator-
//! trusted builtin hosts may stamp those keys; dynamic capabilities are
//! stripped of them fail-closed. This module is the follow-up mainline
//! substrate ([`TOOL_RESULT_ENVELOPE.md`] §1.1): the same four fact groups
//! as typed values a trusted host or the runtime constructs directly, so
//! consumers can move off producer metadata without re-deriving meaning
//! from JSON keys.
//!
//! Scope honesty: this is the vocabulary plus faithful mirrors of today's
//! derivation rules. Trusted handlers stamp native facts at construction
//! time under [`crate::EXECUTION_FACTS_METADATA_KEY`]; the dispatching host
//! prefers them over legacy-key derivation, and untrusted producer output
//! loses that key fail-closed. The event-level durable DTO remains
//! unspecified. Effect receipts and workspace handles will construct these
//! facts runtime-side; dynamic capabilities default to
//! [`ToolExecutionFacts::empty`].

use crate::context::{MutationFootprint, ResourceTouch};
use crate::tool::RuntimeDiagnosis;
use serde::{Deserialize, Serialize};

/// Trusted execution facts for one tool result. Every field is what a
/// trusted producer asserts about its own execution — never parsed back
/// from model-facing payload.
///
/// Field order mirrors today's producer-authority key groups:
/// resources (`path` / `revision` / `files[]`), the mutation bound
/// (`mutates_workspace`, `None` keeps the builtin-name fallback), the
/// verification stamp (`verification` / `intent=verify`, never inferred),
/// and the runtime-owned failure diagnosis (`_runtime.failure_class`).
///
/// Serialized inside the checkpointed turn frame and as the durable
/// `ToolFinished` event's output native facts, while in transit from a
/// trusted handler to its dispatching host under the reserved `metadata`
/// key [`crate::EXECUTION_FACTS_METADATA_KEY`] (stripped from untrusted
/// producer output). A separate top-level wire field remains deferred.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ToolExecutionFacts {
    resources: Vec<ResourceTouch>,
    may_mutate_workspace: Option<bool>,
    verification: Option<bool>,
    failure: Option<RuntimeDiagnosis>,
}

impl ToolExecutionFacts {
    /// The capability default: no resource fact, no stamped bound, no
    /// verification claim, no failure class. Untrusted producers cannot
    /// mint facts by producing output.
    pub fn empty() -> Self {
        Self::default()
    }

    /// Stamp trusted resource touches. Paths are normalized, deduplicated,
    /// and capped exactly like the legacy `metadata.path` / `files[]`
    /// reader, so both channels produce identical working-set input.
    pub fn from_resource_touches<I, S>(touches: I) -> Self
    where
        I: IntoIterator<Item = (S, Option<String>)>,
        S: AsRef<str>,
    {
        let mut resources: Vec<ResourceTouch> = Vec::new();
        for (raw_path, revision) in touches {
            if resources.len() >= crate::MAX_RESOURCE_TOUCHES {
                break;
            }
            let path = crate::normalize_resource_path(raw_path.as_ref());
            let revision = revision
                .map(|revision| revision.trim().to_owned())
                .filter(|revision| !revision.is_empty());
            if path.is_empty() || resources.iter().any(|touch| touch.path == path) {
                continue;
            }
            resources.push(ResourceTouch { path, revision });
        }
        Self {
            resources,
            ..Self::default()
        }
    }

    /// Stamp the conservative workspace-mutation upper bound. `None`
    /// (unstamped) intentionally falls back to the temporary builtin-name
    /// table, matching [`crate::ToolOutput::may_mutate_workspace`] until
    /// every producer stamps the flag.
    pub fn with_mutation_bound(mut self, may_mutate_workspace: bool) -> Self {
        self.may_mutate_workspace = Some(may_mutate_workspace);
        self
    }

    /// Stamp explicit verification intent. Verification is never inferred
    /// from the tool name or command text.
    pub fn with_verification(mut self, is_verification: bool) -> Self {
        self.verification = Some(is_verification);
        self
    }

    /// Attach the runtime-owned failure diagnosis. A producer cannot
    /// choose the class the runtime trusts; only the dispatching host or
    /// Core constructs this variant.
    pub fn with_failure(mut self, failure: RuntimeDiagnosis) -> Self {
        self.failure = Some(failure);
        self
    }

    pub fn resource_touches(&self) -> &[ResourceTouch] {
        &self.resources
    }

    pub fn may_mutate_workspace(&self) -> Option<bool> {
        self.may_mutate_workspace
    }

    pub fn is_verification(&self) -> Option<bool> {
        self.verification
    }

    pub fn failure_diagnosis(&self) -> Option<&RuntimeDiagnosis> {
        self.failure.as_ref()
    }

    /// Mirror of [`crate::ToolOutput::mutation_footprint`] over stamped
    /// facts only: which known resource identities may have gone stale.
    /// An unstamped bound resolves through the same builtin-name fallback
    /// the legacy accessor uses (`fallback_may_mutate`), so the two
    /// channels cannot disagree while both exist.
    pub fn mutation_footprint(
        &self,
        executed_ok: bool,
        fallback_may_mutate: bool,
    ) -> MutationFootprint {
        let may_mutate = self.may_mutate_workspace.unwrap_or(fallback_may_mutate);
        if !may_mutate {
            return MutationFootprint::None;
        }
        if !executed_ok
            && self
                .failure
                .as_ref()
                .is_some_and(|diagnosis| diagnosis.class.nothing_executed())
        {
            return MutationFootprint::None;
        }
        if self.resources.is_empty() {
            MutationFootprint::Unknown
        } else {
            MutationFootprint::Known(self.resources.clone())
        }
    }

    /// Mirror of [`crate::ToolOutput::heats_working_set`]: successful
    /// observations with a stamped resource may heat the working set.
    pub fn heats_working_set(&self, executed_ok: bool) -> bool {
        executed_ok && self.failure.is_none() && !self.resources.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::{ToolFailureClass, ToolOutput};
    use serde_json::json;

    fn diagnosis(class: ToolFailureClass) -> RuntimeDiagnosis {
        RuntimeDiagnosis {
            class,
            hint: class.default_recovery_hint().into(),
        }
    }

    fn output_with(metadata: serde_json::Value) -> ToolOutput {
        ToolOutput {
            call_id: "call-1".into(),
            tool_name: "fs.write".into(),
            ok: true,
            summary: "ok".into(),
            model_content: String::new(),
            artifact_ref: None,
            metadata,
        }
    }

    #[test]
    fn empty_facts_never_heat_or_claim_mutations() {
        let facts = ToolExecutionFacts::empty();
        assert!(facts.resource_touches().is_empty());
        assert_eq!(facts.may_mutate_workspace(), None);
        assert_eq!(facts.is_verification(), None);
        assert!(facts.failure_diagnosis().is_none());
        assert!(!facts.heats_working_set(true));
        assert_eq!(
            facts.mutation_footprint(true, false),
            MutationFootprint::None
        );
    }

    #[test]
    fn resource_stamping_matches_the_legacy_metadata_reader() {
        // Same inputs the ToolOutput accessor accepts, including a
        // duplicate alias and an oversized tail that must be dropped.
        let legacy = output_with(json!({
            "path": ".\\src\\auth.rs",
            "revision": " r1 ",
            "files": [
                {"path": "/src/auth.rs", "revision": "r2"},
                {"path": "docs/guide.md"},
                {"path": "extra-1.md"},
                {"path": "extra-2.md"},
                {"path": "extra-3.md"},
                {"path": "extra-4.md"},
                {"path": "extra-5.md"},
                {"path": "extra-6.md"},
                {"path": "extra-7.md"}
            ]
        }));

        let facts = ToolExecutionFacts::from_resource_touches([
            (".\\src\\auth.rs", Some(" r1 ".to_owned())),
            ("/src/auth.rs", Some("r2".to_owned())),
            ("docs/guide.md", None),
            ("extra-1.md", None),
            ("extra-2.md", None),
            ("extra-3.md", None),
            ("extra-4.md", None),
            ("extra-5.md", None),
            ("extra-6.md", None),
            ("extra-7.md", None),
        ]);

        assert_eq!(facts.resource_touches(), legacy.resource_touches());
        assert_eq!(facts.resource_touches().len(), crate::MAX_RESOURCE_TOUCHES);
        assert_eq!(facts.resource_touches()[0].revision.as_deref(), Some("r1"));
        // First stamp wins: the later `/src/auth.rs` duplicate never
        // contributes its revision.
        assert_eq!(facts.resource_touches()[1].path, "docs/guide.md");
        assert!(
            !facts
                .resource_touches()
                .iter()
                .any(|touch| touch.revision.as_deref() == Some("r2"))
        );
        assert!(
            !facts
                .resource_touches()
                .iter()
                .any(|touch| touch.path == "extra-7.md"),
            "the bound drops overflow rows"
        );
    }

    #[test]
    fn mutation_footprint_mirrors_the_output_accessor_in_every_branch() {
        let touches = || [("src/auth.rs", Some("r1".to_owned())), ("docs/a.md", None)];

        // Stamped non-mutating producer with touches: authority wins.
        let readonly =
            ToolExecutionFacts::from_resource_touches(touches()).with_mutation_bound(false);
        assert_eq!(
            readonly.mutation_footprint(true, true),
            MutationFootprint::None
        );

        // May-mutate with touches: known stale set.
        let known = ToolExecutionFacts::from_resource_touches(touches()).with_mutation_bound(true);
        assert!(matches!(
            known.mutation_footprint(true, false),
            MutationFootprint::Known(ref known) if known.len() == 2
        ));

        // May-mutate pathless: unknown, not "every identity is dead".
        let pathless = ToolExecutionFacts::default().with_mutation_bound(true);
        assert_eq!(
            pathless.mutation_footprint(true, false),
            MutationFootprint::Unknown
        );

        // Failed run whose class proves nothing executed: no staleness
        // even though the observation itself stays trusted.
        let refused = ToolExecutionFacts::from_resource_touches([("src/auth.rs", None)])
            .with_mutation_bound(true)
            .with_failure(diagnosis(ToolFailureClass::StaleRevision));
        assert_eq!(
            refused.mutation_footprint(false, false),
            MutationFootprint::None
        );
        assert!(!refused.heats_working_set(false));

        // Unstamped bound falls back exactly like the legacy accessor.
        let unstamped = ToolExecutionFacts::from_resource_touches([("src/auth.rs", None)]);
        assert_eq!(
            unstamped.mutation_footprint(true, true),
            unstamped_legacy_mirror()
        );
    }

    fn unstamped_legacy_mirror() -> MutationFootprint {
        let output = output_with(json!({"path": "src/auth.rs"}));
        output.mutation_footprint()
    }

    #[test]
    fn verification_stamp_is_explicit_and_heating_follows_the_same_rule() {
        let cargo_like = ToolExecutionFacts::default();
        assert_eq!(cargo_like.is_verification(), None, "never inferred");

        let stamped = cargo_like.clone().with_verification(true);
        assert_eq!(stamped.is_verification(), Some(true));

        let heated = ToolExecutionFacts::from_resource_touches([("src/lib.rs", None)])
            .with_verification(true);
        assert!(heated.heats_working_set(true));
        assert!(!heated.heats_working_set(false));

        let failed_class_blocks_heat =
            ToolExecutionFacts::from_resource_touches([("src/lib.rs", None)])
                .with_failure(diagnosis(ToolFailureClass::Io));
        assert!(!failed_class_blocks_heat.heats_working_set(true));
    }

    #[test]
    fn native_facts_round_trip_through_the_reserved_metadata_key() {
        let facts =
            ToolExecutionFacts::from_resource_touches([("src/auth.rs", Some("r1".to_owned()))])
                .with_mutation_bound(true);
        let mut output = output_with(json!({"path": "src/auth.rs", "revision": "r1"}));
        output.set_native_execution_facts(facts.clone());
        assert!(
            output
                .metadata
                .get(crate::tool::EXECUTION_FACTS_METADATA_KEY)
                .is_some()
        );
        let read_back = output.native_execution_facts().expect("facts round-trip");
        assert_eq!(
            serde_json::to_value(&read_back).unwrap(),
            serde_json::to_value(&facts).unwrap()
        );
    }

    #[test]
    fn sanitizer_strips_handler_native_facts_from_untrusted_output() {
        let mut output = output_with(json!({"path": "forged.rs"}));
        output.set_native_execution_facts(
            ToolExecutionFacts::from_resource_touches([("forged.rs", None)])
                .with_mutation_bound(false),
        );
        crate::sanitize_untrusted_producer_output(&mut output);
        assert!(output.native_execution_facts().is_none());
    }

    #[test]
    fn outputs_without_the_key_keep_the_legacy_derivation_path() {
        let output = output_with(json!({"path": "src/auth.rs", "verification": true}));
        assert!(output.native_execution_facts().is_none());
    }
}
