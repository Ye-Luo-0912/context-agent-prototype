//! Typed tool-root derivation at the BeforeModel safe point.
//!
//! The round surface starts from the explicit task-owned requirement set
//! (exact tool names). On top of that, this policy derives typed roots from
//! the task anchor's structured fields, the focus goal, and the active-call
//! state: anchor fields map to explicit tool families, which resolve to
//! catalog tool names when present. Derivation is a pure function of the
//! safe-point state — deterministic, bounded, and explainable ("entered
//! because acceptance criteria need verification tools"). It never touches
//! the kernel, the context engine, or any store: it reads immutable inputs
//! and returns a bounded requirement list.

use std::collections::HashSet;

use agent_contracts::{ToolSurfaceDemand, ToolSurfaceRequirement};

use crate::task::{RootClaimRole, TaskAnchor};

/// Revision of this typed root-derivation policy. Bumped only when the
/// derivation rules change; recorded as the execution-policy source while
/// an active call pins its tool.
pub const TASK_ROOT_POLICY_REVISION: u64 = 1;

/// Hard cap on derived roots per round, so a pathological anchor can never
/// grow the requirement set past the explicit-set bound. The explicit
/// `TaskToolRequirementSet` stays the authority; derivation only adds.
pub const MAX_DERIVED_TOOL_ROOTS: usize = 16;

/// Tool families the derivation can name. Each family maps to concrete
/// catalog tool names; a family only contributes roots for tools that exist
/// in the catalog, so derivation never demands an absent tool.
const EXPLORE_FAMILY: &[&str] = &["fs.list", "fs.read", "search.grep"];
const MUTATE_FAMILY: &[&str] = &["fs.write", "edit.replace"];
const VERIFY_FAMILY: &[&str] = &["git.status", "git.diff", "shell.exec"];

/// Everything the derivation needs from the safe point. All inputs are
/// immutable references; the function is a pure projection of them.
pub struct TaskRootInput<'a> {
    /// The active task's anchor, if a task is active.
    pub anchor: Option<&'a TaskAnchor>,
    /// The active focus goal text (the task goal when a task is active).
    pub focus_goal: Option<&'a str>,
    /// The tool currently executing, if any (active-call policy).
    pub active_tool: Option<&'a str>,
    /// Names of every tool currently in the candidate catalog.
    pub catalog_names: &'a HashSet<String>,
}

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
/// anchor field or policy that produced it):
///
/// 1. Active-call policy: the executing tool becomes `MustSurface` so the
///    round that consumes its result still offers it.
/// 2. Anchor acceptance criteria -> verification family.
/// 3. Anchor open loops -> exploration family.
/// 4. Anchor plan progress -> mutation family.
/// 5. Anchor working refs (artifact/verification roles) -> exploration
///    family.
/// 6. Focus goal without an anchor (no active task) -> exploration family,
///    so a goal-driven read still gets its tools.
///
/// Roots are de-duplicated by tool name, only name tools that exist in the
/// catalog, and never exceed `MAX_DERIVED_TOOL_ROOTS`.
pub fn derive_task_roots(input: TaskRootInput<'_>) -> Vec<ToolSurfaceRequirement> {
    let mut roots: Vec<ToolSurfaceRequirement> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();

    macro_rules! push_root {
        ($demand:expr, $name:expr, $reason:expr) => {{
            if input.catalog_names.contains($name)
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

    // 2-5. Anchor typed fields -> tool families.
    let anchor = input.anchor;
    let needs_verification = anchor
        .map(|a| !a.acceptance_criteria.is_empty())
        .unwrap_or(false);
    let needs_exploration = anchor.map(|a| !a.open_loops.is_empty()).unwrap_or(false);
    let needs_mutation = anchor.map(|a| !a.plan_progress.is_empty()).unwrap_or(false);
    let needs_artifact_access = anchor
        .map(|a| {
            a.working_refs.iter().any(|claim| {
                matches!(
                    claim.role,
                    RootClaimRole::WorkingArtifact | RootClaimRole::Verification
                )
            })
        })
        .unwrap_or(false);

    if needs_verification {
        for name in VERIFY_FAMILY {
            push_root!(
                ToolSurfaceDemand::PreferSurface,
                *name,
                "acceptance criteria need verification tools"
            );
        }
    }
    if needs_exploration {
        for name in EXPLORE_FAMILY {
            push_root!(
                ToolSurfaceDemand::PreferSurface,
                *name,
                "open loops need exploration tools"
            );
        }
    }
    if needs_mutation {
        for name in MUTATE_FAMILY {
            push_root!(
                ToolSurfaceDemand::PreferSurface,
                *name,
                "plan in progress needs mutation tools"
            );
        }
    }
    if needs_artifact_access {
        for name in EXPLORE_FAMILY {
            push_root!(
                ToolSurfaceDemand::PreferSurface,
                *name,
                "working refs need artifact access"
            );
        }
    }

    // 6. Focus goal without a task anchor.
    if anchor.is_none()
        && input
            .focus_goal
            .map(|goal| !goal.is_empty())
            .unwrap_or(false)
    {
        for name in EXPLORE_FAMILY {
            push_root!(
                ToolSurfaceDemand::PreferSurface,
                *name,
                "focus goal needs exploration tools"
            );
        }
    }

    roots
}

#[cfg(test)]
mod tests {
    use super::*;

    fn catalog(names: &[&str]) -> HashSet<String> {
        names.iter().map(|name| name.to_string()).collect()
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
        let catalog_names = catalog(&["fs.read", "search.grep", "shell.exec"]);
        let roots = derive_task_roots(TaskRootInput {
            anchor: None,
            focus_goal: None,
            active_tool: Some("search.grep"),
            catalog_names: &catalog_names,
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
        let catalog_names = catalog(&[
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
            catalog_names: &catalog_names,
        });
        // Verification family (3) + exploration (3) + mutation (2), no
        // duplicates across the overlapping exploration rules.
        let mut expected: Vec<&str> = vec![
            "git.status",
            "git.diff",
            "shell.exec", // verification
            "fs.list",
            "fs.read",
            "search.grep", // exploration (open loops)
            "fs.write",
            "edit.replace", // mutation
        ];
        expected.sort_unstable();
        let mut actual = names(&roots);
        actual.sort_unstable();
        assert_eq!(actual, expected);
        assert!(
            roots
                .iter()
                .all(|r| r.demand == ToolSurfaceDemand::PreferSurface)
        );
        assert!(roots.iter().all(|r| !r.reason.is_empty()));
    }

    #[test]
    fn derivation_never_names_absent_tools() {
        // The catalog only has a custom capability tool; no family member
        // exists, so even a full anchor derives nothing.
        let catalog_names = catalog(&["demo.one"]);
        let anchor = anchor_with(true, true, true, true);
        let roots = derive_task_roots(TaskRootInput {
            anchor: Some(&anchor),
            focus_goal: Some("goal"),
            active_tool: Some("demo.one"),
            catalog_names: &catalog_names,
        });
        // The active call still pins the one real tool.
        assert_eq!(names(&roots), vec!["demo.one"]);
        assert_eq!(roots[0].demand, ToolSurfaceDemand::MustSurface);
    }

    #[test]
    fn focus_without_anchor_derives_exploration() {
        let catalog_names = catalog(&["fs.read", "search.grep", "git.status"]);
        let roots = derive_task_roots(TaskRootInput {
            anchor: None,
            focus_goal: Some("understand the code"),
            active_tool: None,
            catalog_names: &catalog_names,
        });
        let mut actual = names(&roots);
        actual.sort_unstable();
        assert_eq!(actual, vec!["fs.read", "search.grep"]);
    }

    #[test]
    fn derivation_is_bounded_and_deterministic() {
        let catalog_names = catalog(&[
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
            catalog_names: &catalog_names,
        });
        let b = derive_task_roots(TaskRootInput {
            anchor: Some(&anchor),
            focus_goal: Some("goal"),
            active_tool: None,
            catalog_names: &catalog_names,
        });
        assert_eq!(a, b);
        assert!(a.len() <= MAX_DERIVED_TOOL_ROOTS);
    }

    #[test]
    fn empty_safe_point_derives_nothing() {
        let catalog_names = catalog(&["fs.read"]);
        let roots = derive_task_roots(TaskRootInput {
            anchor: None,
            focus_goal: None,
            active_tool: None,
            catalog_names: &catalog_names,
        });
        assert!(roots.is_empty());
    }
}
