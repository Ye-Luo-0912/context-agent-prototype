//! Typed tool-root derivation at the BeforeModel safe point.
//!
//! The round surface starts from the explicit task-owned requirement set
//! (exact tool names). On top of that, this policy derives typed roots from
//! execution need → [`ToolSemanticRole`] → catalog specs that exist this
//! round. Derivation is a pure function of the safe-point state —
//! deterministic, bounded, and explainable. It never touches the kernel,
//! the context engine, or any store.
//!
//! NeedVerify resolves declared `Verify` roles first, then catalog
//! discovery (`capability.manage`), and only then an `EscapeHatch`.
//! InspectDiff is not a verifier. Runtime does not encode "cargo uses
//! `shell.exec`".

use std::collections::HashSet;

use agent_contracts::{
    CONTEXT_MANAGE, ToolSemanticRole, ToolSpec, ToolSurfaceDemand, ToolSurfaceRequirement,
};

use crate::task::TaskAnchor;

/// Revision of this typed root-derivation policy. Bumped only when the
/// derivation rules change; recorded as the execution-policy source while
/// an active call pins its tool.
pub const TASK_ROOT_POLICY_REVISION: u64 = 9;

/// Hard cap on derived roots per round, so a pathological anchor can never
/// grow the requirement set past the explicit-set bound. The explicit
/// `TaskToolRequirementSet` stays the authority; derivation only adds.
pub const MAX_DERIVED_TOOL_ROOTS: usize = 16;

/// Everything the derivation needs from the safe point. All inputs are
/// immutable references; the function is a pure projection of them.
pub struct TaskRootInput<'a> {
    /// The active task's anchor, if a task is active.
    pub anchor: Option<&'a TaskAnchor>,
    /// The active focus goal text (the task goal when a task is active).
    pub focus_goal: Option<&'a str>,
    /// The tool currently executing, if any (active-call policy).
    pub active_tool: Option<&'a str>,
    /// Candidate catalog specs for this round (roles, not just names).
    pub catalog: &'a [ToolSpec],
    /// True when a verification obligation is due *this round* (failed
    /// verification, unmet obligation plus complete/coverage/soft-NL
    /// verify, never NL-alone). Not "acceptance is nonempty".
    pub verification_due: bool,
    /// Current user-turn directive. Not a planner for mutation, read, or search.
    pub turn_intent: Option<&'a str>,
    /// Open failed-command rows. A fact-gap, not a PreferSurface Mutate plan.
    pub has_failures: bool,
    /// Warm/Cold/Stored catalog or upcoming EXTERNAL CONTEXT refs.
    /// Drives evidence retrieval via `context.manage`.
    pub has_external_context: bool,
}

/// Deterministic execution needs for one BeforeModel round. No planner.
pub use crate::execution::derive_execution_needs as derive_needs;

/// The execution-policy source revision: present exactly while an active
/// call pins its tool (the policy is "the executing tool stays surfaced"),
/// recorded as the policy version. A round without an active call has no
/// execution-policy plane, so the source stays `None` rather than claiming
/// a revision that does not exist.
pub fn derive_execution_policy_revision(active_tool: Option<&str>) -> Option<u64> {
    active_tool.map(|_| TASK_ROOT_POLICY_REVISION)
}

/// Derive typed tool roots from the safe-point state.
///
/// Rules (explicit, deterministic, in priority order; each root names the
/// need that produced it):
///
/// 1. Active-call policy: the executing tool becomes `MustSurface` so the
///    round that consumes its result still offers it.
/// 2. `derive_needs` → fact-gaps, never an action plan.
///    VerificationDue: `Verify` → capability search → `EscapeHatch`.
///    InspectDiff is never pulled in as a verifier.
///    EvidenceNeeded / OpenLoopNeedsEvidence: PreferSurface `context.manage`.
///    UnresolvedFailure is a fact for the prompt, not PreferSurface Mutate.
/// 3. A focus goal or user instruction never PreferSurfaces Read/Search/Mutate.
///
/// Roots are de-duplicated by tool name, only name tools that exist in the
/// catalog, and never exceed `MAX_DERIVED_TOOL_ROOTS`.
pub fn derive_task_roots(input: TaskRootInput<'_>) -> Vec<ToolSurfaceRequirement> {
    let mut roots: Vec<ToolSurfaceRequirement> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();

    macro_rules! push_root {
        ($demand:expr, $name:expr, $reason:expr) => {{
            if catalog_has(input.catalog, $name)
                && !seen.contains($name)
                && roots.len() < MAX_DERIVED_TOOL_ROOTS
            {
                seen.insert($name.to_string());
                roots.push(ToolSurfaceRequirement {
                    tool_name: $name.to_string(),
                    demand: $demand,
                    reason: $reason.to_string(),
                });
            }
        }};
    }

    // 1. Active-call policy.
    if let Some(active) = input.active_tool {
        push_root!(
            ToolSurfaceDemand::MustSurface,
            active,
            "active call continues"
        );
    }

    // 2. Execution need → semantic role → catalog.
    let needs: crate::execution::ExecutionNeeds = derive_needs(
        input.turn_intent,
        input.focus_goal,
        input.anchor,
        input.verification_due,
        input.has_failures,
        input.has_external_context,
    );

    if needs.verification_due {
        let (specs, reason) = verification_surface(input.catalog);
        for spec in specs {
            push_root!(ToolSurfaceDemand::PreferSurface, spec.name.as_str(), reason);
        }
    }
    if needs.evidence_needed || needs.open_loop_needs_evidence {
        push_root!(
            ToolSurfaceDemand::PreferSurface,
            CONTEXT_MANAGE,
            "EXTERNAL CONTEXT / NeedEvidence needs catalog retrieval"
        );
    }

    roots
}

fn catalog_has(catalog: &[ToolSpec], name: &str) -> bool {
    catalog.iter().any(|spec| spec.name == name)
}

/// NeedVerify: declared verifiers, else catalog discovery, else escape hatch.
fn verification_surface(catalog: &[ToolSpec]) -> (Vec<&ToolSpec>, &'static str) {
    let verify = named_sorted(
        catalog
            .iter()
            .filter(|spec| spec.has_role(ToolSemanticRole::Verify)),
    );
    if !verify.is_empty() {
        return (verify, "verification capability is available");
    }
    let search = named_sorted(catalog.iter().filter(|spec| spec.is_capability_search()));
    if !search.is_empty() {
        return (search, "no verification capability; search the catalog");
    }
    (
        named_sorted(
            catalog
                .iter()
                .filter(|spec| spec.has_role(ToolSemanticRole::EscapeHatch)),
        ),
        "no verification capability; escape hatch last",
    )
}

fn named_sorted<'a, I>(specs: I) -> Vec<&'a ToolSpec>
where
    I: Iterator<Item = &'a ToolSpec>,
{
    let mut specs: Vec<&ToolSpec> = specs.collect();
    specs.sort_by(|a, b| a.name.cmp(&b.name));
    specs
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::task::RootClaimRole;

    fn catalog(names: &[&str]) -> Vec<ToolSpec> {
        names
            .iter()
            .map(|name| ToolSpec {
                name: (*name).to_string(),
                ..ToolSpec::default()
            })
            .collect()
    }

    fn catalog_with(entries: &[(&str, Vec<ToolSemanticRole>)]) -> Vec<ToolSpec> {
        entries
            .iter()
            .map(|(name, roles)| ToolSpec {
                name: (*name).to_string(),
                roles: roles.clone(),
                ..ToolSpec::default()
            })
            .collect()
    }

    fn anchor_with(
        acceptance: bool,
        open_loops: bool,
        plan_progress: bool,
        working_refs: bool,
    ) -> TaskAnchor {
        TaskAnchor {
            revision: 7,
            original_goal: "goal".into(),
            current_interpretation: "goal".into(),
            constraints: Vec::new(),
            acceptance_criteria: if acceptance {
                vec!["tests pass".into()]
            } else {
                Vec::new()
            },
            plan_progress: if plan_progress {
                vec!["step 1".into()]
            } else {
                Vec::new()
            },
            open_loops: if open_loops {
                vec!["why?".into()]
            } else {
                Vec::new()
            },
            working_refs: if working_refs {
                vec![crate::task::ContextRootClaim {
                    item_ref: "art:1".into(),
                    role: RootClaimRole::WorkingArtifact,
                    strength: crate::task::RootClaimStrength::ResidentRequired,
                    source_field_id: "working_refs".into(),
                }]
            } else {
                Vec::new()
            },
            evidence_refs: Vec::new(),
        }
    }

    fn names(roots: &[ToolSurfaceRequirement]) -> Vec<&str> {
        roots.iter().map(|r| r.tool_name.as_str()).collect()
    }

    #[test]
    fn active_call_pins_its_tool_as_must_surface() {
        let catalog = catalog(&["fs.read", "search.grep", "shell.exec"]);
        let roots = derive_task_roots(TaskRootInput {
            anchor: None,
            focus_goal: None,
            active_tool: Some("search.grep"),
            catalog: &catalog,
            verification_due: false,
            turn_intent: None,
            has_failures: false,
            has_external_context: false,
        });
        assert_eq!(roots.len(), 1);
        assert_eq!(roots[0].tool_name, "search.grep");
        assert_eq!(roots[0].demand, ToolSurfaceDemand::MustSurface);
        assert_eq!(roots[0].reason, "active call continues");
        assert_eq!(
            derive_execution_policy_revision(Some("search.grep")),
            Some(TASK_ROOT_POLICY_REVISION)
        );
        assert_eq!(derive_execution_policy_revision(None), None);
    }

    #[test]
    fn anchor_fields_derive_typed_families() {
        let catalog = catalog(&[
            "fs.list",
            "fs.read",
            "fs.write",
            "edit.replace",
            "git.status",
            "git.diff",
            "shell.exec",
            "search.grep",
        ]);
        let anchor = anchor_with(true, true, true, true);
        let roots = derive_task_roots(TaskRootInput {
            anchor: Some(&anchor),
            focus_goal: Some("goal"),
            active_tool: None,
            catalog: &catalog,
            verification_due: false,
            turn_intent: None,
            has_failures: false,
            has_external_context: false,
        });
        assert!(
            roots.is_empty(),
            "open loops without context.manage must not PreferSurface Read/Search/Mutate: {roots:?}"
        );
    }

    #[test]
    fn derivation_never_names_absent_tools() {
        // The catalog only has a custom capability tool; no family member
        // exists, so even a full anchor derives nothing.
        let catalog = catalog(&["demo.one"]);
        let anchor = anchor_with(true, true, true, true);
        let roots = derive_task_roots(TaskRootInput {
            anchor: Some(&anchor),
            focus_goal: Some("goal"),
            active_tool: Some("demo.one"),
            catalog: &catalog,
            verification_due: true,
            turn_intent: None,
            has_failures: false,
            has_external_context: false,
        });
        // The active call still pins the one real tool.
        assert_eq!(names(&roots), vec!["demo.one"]);
        assert_eq!(roots[0].demand, ToolSurfaceDemand::MustSurface);
    }

    #[test]
    fn focus_without_anchor_derives_no_action_plan() {
        let catalog = catalog(&["fs.read", "search.grep", "git.status"]);
        let roots = derive_task_roots(TaskRootInput {
            anchor: None,
            focus_goal: Some("understand the code"),
            active_tool: None,
            catalog: &catalog,
            verification_due: false,
            turn_intent: None,
            has_failures: false,
            has_external_context: false,
        });
        assert!(
            roots.is_empty(),
            "a focus goal is not NeedExplore: {roots:?}"
        );
    }

    #[test]
    fn verification_due_prefers_declared_verify_role() {
        let catalog = catalog_with(&[
            ("tests.run", vec![ToolSemanticRole::Verify]),
            ("git.diff", vec![ToolSemanticRole::InspectDiff]),
            ("shell.exec", vec![ToolSemanticRole::EscapeHatch]),
            (
                agent_contracts::CAPABILITY_MANAGE,
                vec![ToolSemanticRole::Search],
            ),
        ]);
        let anchor = anchor_with(true, false, false, false);
        let roots = derive_task_roots(TaskRootInput {
            anchor: Some(&anchor),
            focus_goal: Some("goal"),
            active_tool: None,
            catalog: &catalog,
            verification_due: true,
            turn_intent: None,
            has_failures: false,
            has_external_context: false,
        });
        assert_eq!(names(&roots), vec!["tests.run"]);
        assert_eq!(roots[0].reason, "verification capability is available");
    }

    #[test]
    fn verification_due_prefers_capability_search_when_no_verifier() {
        let catalog = catalog(&[
            "git.status",
            "git.diff",
            "shell.exec",
            agent_contracts::CAPABILITY_MANAGE,
        ]);
        let anchor = anchor_with(true, false, false, false);
        let roots = derive_task_roots(TaskRootInput {
            anchor: Some(&anchor),
            focus_goal: Some("goal"),
            active_tool: None,
            catalog: &catalog,
            verification_due: true,
            turn_intent: None,
            has_failures: false,
            has_external_context: false,
        });
        assert_eq!(names(&roots), vec![agent_contracts::CAPABILITY_MANAGE]);
        assert_eq!(
            roots[0].reason,
            "no verification capability; search the catalog"
        );
        assert!(
            !names(&roots).iter().any(|name| {
                *name == "git.diff" || *name == "git.status" || *name == "shell.exec"
            })
        );
    }

    #[test]
    fn unknown_plugin_is_not_an_escape_hatch_for_need_verify() {
        let catalog = catalog(&["git.status", "git.diff", "plugin.generated", "fs.delete"]);
        let anchor = anchor_with(true, false, false, false);
        let roots = derive_task_roots(TaskRootInput {
            anchor: Some(&anchor),
            focus_goal: Some("goal"),
            active_tool: None,
            catalog: &catalog,
            verification_due: true,
            turn_intent: None,
            has_failures: false,
            has_external_context: false,
        });
        assert!(
            roots.is_empty(),
            "unstamped plugins and fs.delete must not become verifiers: {roots:?}"
        );
    }

    #[test]
    fn verification_due_uses_escape_hatch_last() {
        let catalog = catalog(&["git.status", "git.diff", "shell.exec", "fs.read"]);
        let anchor = anchor_with(true, false, false, false);
        let roots = derive_task_roots(TaskRootInput {
            anchor: Some(&anchor),
            focus_goal: Some("goal"),
            active_tool: None,
            catalog: &catalog,
            verification_due: true,
            turn_intent: None,
            has_failures: false,
            has_external_context: false,
        });
        assert_eq!(names(&roots), vec!["shell.exec"]);
        assert_eq!(
            roots[0].reason,
            "no verification capability; escape hatch last"
        );
    }

    #[test]
    fn acceptance_without_due_does_not_prefer_verify_tools() {
        let catalog = catalog(&["git.status", "git.diff", "shell.exec"]);
        let anchor = anchor_with(true, false, false, false);
        let roots = derive_task_roots(TaskRootInput {
            anchor: Some(&anchor),
            focus_goal: Some("goal"),
            active_tool: None,
            catalog: &catalog,
            verification_due: false,
            turn_intent: None,
            has_failures: false,
            has_external_context: false,
        });
        assert!(roots.is_empty());
    }

    #[test]
    fn derivation_is_bounded_and_deterministic() {
        let catalog = catalog(&[
            "fs.list",
            "fs.read",
            "fs.write",
            "edit.replace",
            "git.status",
            "git.diff",
            "shell.exec",
            "search.grep",
        ]);
        let anchor = anchor_with(true, true, true, true);
        let a = derive_task_roots(TaskRootInput {
            anchor: Some(&anchor),
            focus_goal: Some("goal"),
            active_tool: None,
            catalog: &catalog,
            verification_due: true,
            turn_intent: None,
            has_failures: false,
            has_external_context: false,
        });
        let b = derive_task_roots(TaskRootInput {
            anchor: Some(&anchor),
            focus_goal: Some("goal"),
            active_tool: None,
            catalog: &catalog,
            verification_due: true,
            turn_intent: None,
            has_failures: false,
            has_external_context: false,
        });
        assert_eq!(a, b);
        assert!(a.len() <= MAX_DERIVED_TOOL_ROOTS);
    }

    #[test]
    fn empty_safe_point_derives_nothing() {
        let catalog = catalog(&["fs.read"]);
        let roots = derive_task_roots(TaskRootInput {
            anchor: None,
            focus_goal: None,
            active_tool: None,
            catalog: &catalog,
            verification_due: false,
            turn_intent: None,
            has_failures: false,
            has_external_context: false,
        });
        assert!(roots.is_empty());
    }

    #[test]
    fn unresolved_failure_does_not_prefer_mutate() {
        let catalog = catalog(&["fs.write", "edit.patch", "fs.read"]);
        let roots = derive_task_roots(TaskRootInput {
            anchor: None,
            focus_goal: Some("goal"),
            active_tool: None,
            catalog: &catalog,
            verification_due: false,
            turn_intent: None,
            has_failures: true,
            has_external_context: false,
        });
        assert!(
            roots.is_empty(),
            "unresolved failure is a fact-gap, not PreferSurface Mutate: {roots:?}"
        );
    }

    #[test]
    fn open_loops_prefer_context_manage() {
        let catalog = catalog(&[CONTEXT_MANAGE, "fs.read", "search.grep"]);
        let anchor = anchor_with(false, true, false, false);
        let roots = derive_task_roots(TaskRootInput {
            anchor: Some(&anchor),
            focus_goal: Some("goal"),
            active_tool: None,
            catalog: &catalog,
            verification_due: false,
            turn_intent: None,
            has_failures: false,
            has_external_context: false,
        });
        assert_eq!(names(&roots), vec![CONTEXT_MANAGE]);
    }

    #[test]
    fn note_turn_intent_does_not_prefer_mutate() {
        let catalog = catalog(&[
            "fs.write",
            "edit.replace",
            "git.status",
            "git.diff",
            "shell.exec",
            "fs.read",
        ]);
        let anchor = anchor_with(true, false, false, false);
        let roots = derive_task_roots(TaskRootInput {
            anchor: Some(&anchor),
            focus_goal: Some("fix util.py; do not create other files yet"),
            active_tool: None,
            catalog: &catalog,
            verification_due: false,
            turn_intent: Some("Append to src/scratch.md: HDMI is in drawer 3"),
            has_failures: false,
            has_external_context: false,
        });
        assert!(
            roots.is_empty(),
            "a note turn must not surface mutation apps: {roots:?}"
        );
    }

    #[test]
    fn question_intent_does_not_prefer_mutate() {
        let catalog = catalog(&["fs.write", "edit.replace", "fs.read"]);
        let anchor = anchor_with(true, false, false, false);
        let roots = derive_task_roots(TaskRootInput {
            anchor: Some(&anchor),
            focus_goal: Some("goal"),
            active_tool: None,
            catalog: &catalog,
            verification_due: false,
            turn_intent: Some("what is the current status of this file?"),
            has_failures: false,
            has_external_context: false,
        });
        assert!(roots.is_empty());
    }

    #[test]
    fn need_evidence_prefers_context_manage() {
        let catalog = catalog(&[CONTEXT_MANAGE, "fs.read", "search.grep"]);
        let roots = derive_task_roots(TaskRootInput {
            anchor: None,
            focus_goal: None,
            active_tool: None,
            catalog: &catalog,
            verification_due: false,
            turn_intent: None,
            has_failures: false,
            has_external_context: true,
        });
        assert_eq!(names(&roots), vec![CONTEXT_MANAGE]);
        assert_eq!(roots[0].demand, ToolSurfaceDemand::PreferSurface);
        assert_eq!(
            roots[0].reason,
            "EXTERNAL CONTEXT / NeedEvidence needs catalog retrieval"
        );
    }

    #[test]
    fn evidence_refs_prefer_context_manage_without_external_catalog() {
        let catalog = catalog(&[CONTEXT_MANAGE, "fs.read"]);
        let mut anchor = anchor_with(false, false, false, false);
        anchor.evidence_refs = vec![crate::task::ContextRootClaim {
            item_ref: "context://run/evidence".into(),
            role: RootClaimRole::Verification,
            strength: crate::task::RootClaimStrength::StorageRequired,
            source_field_id: "evidence_refs".into(),
        }];
        let roots = derive_task_roots(TaskRootInput {
            anchor: Some(&anchor),
            focus_goal: Some("goal"),
            active_tool: None,
            catalog: &catalog,
            verification_due: false,
            turn_intent: None,
            has_failures: false,
            has_external_context: false,
        });
        assert_eq!(names(&roots), vec![CONTEXT_MANAGE]);
    }

    #[test]
    fn no_need_evidence_does_not_prefer_context_manage() {
        let catalog = catalog(&[CONTEXT_MANAGE, "fs.read"]);
        let roots = derive_task_roots(TaskRootInput {
            anchor: None,
            focus_goal: None,
            active_tool: None,
            catalog: &catalog,
            verification_due: false,
            turn_intent: None,
            has_failures: false,
            has_external_context: false,
        });
        assert!(
            roots.is_empty(),
            "catalog-only context.manage must stay off without NeedEvidence: {roots:?}"
        );
    }
}
