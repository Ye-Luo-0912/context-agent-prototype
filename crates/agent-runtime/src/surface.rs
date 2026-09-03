//! Pure, per-model-round tool-surface planning.
//!
//! Catalog lifecycle decides what is currently available. Task requirements
//! express demand. This module projects both into one immutable round
//! snapshot; it never calls load/unload or changes authority.

use std::collections::{BTreeMap, HashSet};

use agent_contracts::{
    MAX_TOOL_REQUIREMENT_NAME_CHARS, MAX_TOOL_SURFACE_REPORT_BLOCKED,
    MAX_TOOL_SURFACE_REPORT_NAME_BYTES, MAX_TOOL_SURFACE_REPORT_OMITTED,
    MAX_TOOL_SURFACE_REPORT_SELECTED, SchemaProfile, ToolSpec, ToolSurfaceBlock,
    ToolSurfaceBlockReason, ToolSurfaceDemand, ToolSurfaceOmission, ToolSurfaceOmissionReason,
    ToolSurfaceOrigin, ToolSurfacePlanReport, ToolSurfacePlanStatus, ToolSurfaceRequirement,
    ToolSurfaceSelection, ToolSurfaceSnapshot, ToolSurfaceSourceRevisions, TurnId,
};

use crate::budget::{MAX_TOOL_SURFACE_TOKENS, approx_layer_tokens};

const MAX_SNAPSHOT_OMISSIONS: usize = 128;

/// Mutable only while one round is being prepared. It is consumed into the
/// final immutable `ToolSurfaceSnapshot` before `ModelStarted` is emitted.
pub(crate) struct RoundSurfacePlan {
    specs: Vec<ToolSpec>,
    demands: BTreeMap<String, ToolSurfaceDemand>,
    /// Per-tool authority source, so report rows can answer why the tool
    /// entered consideration (task intent vs dispatcher vs catalog load).
    origins: BTreeMap<String, ToolSurfaceOrigin>,
    omissions: Vec<ToolSurfaceOmission>,
    omitted_total: usize,
    mandatory: HashSet<String>,
    task_preferred: HashSet<String>,
    legacy_generation: u64,
    source_revisions: ToolSurfaceSourceRevisions,
}

#[derive(Clone, Copy)]
pub(crate) struct SurfaceReportContext {
    pub(crate) turn_id: TurnId,
    pub(crate) model_round: usize,
    pub(crate) surface_revision: u64,
    pub(crate) estimated_input_tokens: usize,
    pub(crate) input_budget_tokens: usize,
}

impl RoundSurfacePlan {
    /// Build the initial 4096-token projection from the complete Loaded
    /// candidate set. Fail-closed dispatcher entries and Task MustSurface
    /// entries are mandatory; KeepReady stays catalog-ready but prompt-cold.
    pub(crate) fn build(
        candidates: ToolSurfaceSnapshot,
        requirements: &[ToolSurfaceRequirement],
        may_omit: impl Fn(&str) -> bool,
    ) -> Self {
        let mut requested = BTreeMap::new();
        for requirement in requirements {
            requested
                .entry(requirement.tool_name.clone())
                .and_modify(|demand: &mut ToolSurfaceDemand| {
                    if requirement.demand.rank() > demand.rank() {
                        *demand = requirement.demand;
                    }
                })
                .or_insert(requirement.demand);
        }

        let mut mandatory_specs = Vec::new();
        let mut optional_specs = Vec::new();
        let mut demands = BTreeMap::new();
        let mut origins = BTreeMap::new();
        let mut mandatory = HashSet::new();
        let mut task_preferred = HashSet::new();
        let mut omissions = candidates.omissions;
        let mut omitted_total = candidates.omitted_total.max(omissions.len());
        for omission in &mut omissions {
            omission.tool_name = bounded_snapshot_name(&omission.tool_name);
        }
        omissions.truncate(MAX_SNAPSHOT_OMISSIONS);

        for spec in candidates.specs {
            let dispatcher_mandatory = !may_omit(&spec.name);
            let requested_demand = requested.get(&spec.name).copied();
            let demand = if dispatcher_mandatory
                || requested_demand == Some(ToolSurfaceDemand::MustSurface)
            {
                ToolSurfaceDemand::MustSurface
            } else {
                requested_demand.unwrap_or(ToolSurfaceDemand::PreferSurface)
            };
            demands.insert(spec.name.clone(), demand);
            // Provenance: which authority put this tool into consideration.
            // A fail-closed dispatcher entry wins over task intent, and task
            // intent wins over a plain catalog load.
            let origin = if dispatcher_mandatory {
                ToolSurfaceOrigin::DispatcherRequired
            } else if requested_demand.is_some() {
                ToolSurfaceOrigin::TaskRequirement
            } else {
                ToolSurfaceOrigin::CatalogLoadedOptional
            };
            origins.insert(spec.name.clone(), origin);

            let spec = spec.compact_for_model_surface();

            if demand == ToolSurfaceDemand::KeepReady {
                let approx_tokens = approx_layer_tokens(&spec);
                push_omission(
                    &mut omissions,
                    &mut omitted_total,
                    ToolSurfaceOmission {
                        tool_name: spec.name,
                        demand,
                        origin,
                        reason: ToolSurfaceOmissionReason::KeepReady,
                        approx_tokens,
                    },
                );
            } else if demand == ToolSurfaceDemand::MustSurface {
                mandatory.insert(spec.name.clone());
                mandatory_specs.push(spec);
            } else {
                let explicitly_preferred =
                    requested_demand == Some(ToolSurfaceDemand::PreferSurface);
                if explicitly_preferred {
                    task_preferred.insert(spec.name.clone());
                }
                optional_specs.push((explicitly_preferred, spec));
            }
        }

        mandatory_specs.sort_by(|a, b| a.name.cmp(&b.name));
        optional_specs.sort_by(|(left_preferred, left), (right_preferred, right)| {
            right_preferred
                .cmp(left_preferred)
                .then_with(|| approx_layer_tokens(left).cmp(&approx_layer_tokens(right)))
                .then_with(|| left.name.cmp(&right.name))
        });

        let mut specs = mandatory_specs;
        let mut saturated_group = None;
        for (explicitly_preferred, spec) in optional_specs {
            let cost = approx_layer_tokens(&spec);
            if saturated_group != Some(explicitly_preferred) {
                specs.push(spec);
                if approx_layer_tokens(&specs) <= MAX_TOOL_SURFACE_TOKENS {
                    continue;
                }
                // Candidates are ordered by exact single-schema wire cost.
                // Once one no longer fits, every later candidate in this
                // priority group is at least as expensive. A lower-priority
                // group still gets one fresh fit check because it may start
                // with a smaller schema.
                saturated_group = Some(explicitly_preferred);
                let spec = specs.pop().expect("the just-pushed schema exists");
                let origin = origins
                    .get(&spec.name)
                    .copied()
                    .unwrap_or(ToolSurfaceOrigin::Unknown);
                push_omission(
                    &mut omissions,
                    &mut omitted_total,
                    ToolSurfaceOmission {
                        tool_name: spec.name,
                        demand: ToolSurfaceDemand::PreferSurface,
                        origin,
                        reason: ToolSurfaceOmissionReason::SchemaBudget,
                        approx_tokens: cost,
                    },
                );
                continue;
            }
            let origin = origins
                .get(&spec.name)
                .copied()
                .unwrap_or(ToolSurfaceOrigin::Unknown);
            push_omission(
                &mut omissions,
                &mut omitted_total,
                ToolSurfaceOmission {
                    tool_name: spec.name,
                    demand: ToolSurfaceDemand::PreferSurface,
                    origin,
                    reason: ToolSurfaceOmissionReason::SchemaBudget,
                    approx_tokens: cost,
                },
            );
        }
        specs.sort_by(|a, b| a.name.cmp(&b.name));

        let mut source_revisions = candidates.source_revisions;
        if source_revisions.builtin_catalog_generation == 0
            && source_revisions.capability_catalog_generation == 0
        {
            source_revisions.builtin_catalog_generation = candidates.generation;
        }

        Self {
            specs,
            demands,
            origins,
            omissions,
            omitted_total,
            mandatory,
            task_preferred,
            legacy_generation: candidates.generation,
            source_revisions,
        }
    }

    pub(crate) fn specs(&self) -> &[ToolSpec] {
        &self.specs
    }

    pub(crate) fn source_revisions_mut(&mut self) -> &mut ToolSurfaceSourceRevisions {
        &mut self.source_revisions
    }

    pub(crate) fn add_unavailable(&mut self, requirement: &ToolSurfaceRequirement) {
        push_omission(
            &mut self.omissions,
            &mut self.omitted_total,
            ToolSurfaceOmission {
                tool_name: requirement.tool_name.clone(),
                demand: requirement.demand,
                origin: ToolSurfaceOrigin::TaskRequirement,
                reason: ToolSurfaceOmissionReason::Unavailable,
                approx_tokens: 0,
            },
        );
    }

    /// Relabel requirements whose trust is a runtime-derived recovery
    /// signal. The model cannot mint `RecoverySurface`; only the typed
    /// recovery pipeline can mark an exact tool (for example `fs.mkdir`
    /// after a trusted `parent_path_not_found`).
    pub(crate) fn mark_recovery_tools(&mut self, tools: &HashSet<String>) {
        let mut relabel = Vec::new();
        for name in self.origins.keys() {
            if tools.contains(name) {
                relabel.push(name.clone());
            }
        }
        for name in relabel {
            self.origins
                .insert(name, ToolSurfaceOrigin::RecoverySurface);
        }
    }

    /// Convert this decision into a text-only completion finalization round.
    /// This is not budget degradation and does not unload any tool: Runtime
    /// has closed the bounded repair episode and is asking for an ordinary
    /// final answer. Every removed schema remains visible in the audit report
    /// with its original demand and authority origin.
    pub(crate) fn force_completion_finalization(&mut self) {
        let specs = std::mem::take(&mut self.specs);
        // The omission sample may already be full from earlier budget
        // decisions. Reserve one diagnostic slot so a text-only surface can
        // never lose the reason it became text-only; exact totals are kept
        // independently and remain unchanged by replacing a sample row.
        if !specs.is_empty() && self.omissions.len() >= MAX_SNAPSHOT_OMISSIONS {
            self.omissions.pop();
        }
        for spec in specs {
            let approx_tokens = approx_layer_tokens(&spec);
            let demand = self
                .demands
                .get(&spec.name)
                .copied()
                .unwrap_or(ToolSurfaceDemand::PreferSurface);
            let origin = self
                .origins
                .get(&spec.name)
                .copied()
                .unwrap_or(ToolSurfaceOrigin::Unknown);
            push_omission(
                &mut self.omissions,
                &mut self.omitted_total,
                ToolSurfaceOmission {
                    tool_name: spec.name,
                    demand,
                    origin,
                    reason: ToolSurfaceOmissionReason::CompletionFinalization,
                    approx_tokens,
                },
            );
        }
        self.mandatory.clear();
        self.task_preferred.clear();
    }

    /// Final provider-window degradation: remove only a non-mandatory entry
    /// from this local plan. Catalog lifecycle and generation are unreachable.
    pub(crate) fn omit_largest_for_provider_budget(&mut self) -> Option<ToolSpec> {
        let index = self
            .specs
            .iter()
            .enumerate()
            .filter(|(_, spec)| !self.mandatory.contains(&spec.name))
            .max_by(|(_, left), (_, right)| {
                (!self.task_preferred.contains(&left.name))
                    .cmp(&(!self.task_preferred.contains(&right.name)))
                    .then_with(|| {
                        approx_layer_tokens(*left)
                            .cmp(&approx_layer_tokens(*right))
                            .then_with(|| left.name.cmp(&right.name))
                    })
            })
            .map(|(index, _)| index)?;
        let spec = self.specs.remove(index);
        let demand = self
            .demands
            .get(&spec.name)
            .copied()
            .unwrap_or(ToolSurfaceDemand::PreferSurface);
        let origin = self
            .origins
            .get(&spec.name)
            .copied()
            .unwrap_or(ToolSurfaceOrigin::Unknown);
        push_omission(
            &mut self.omissions,
            &mut self.omitted_total,
            ToolSurfaceOmission {
                tool_name: spec.name.clone(),
                demand,
                origin,
                reason: ToolSurfaceOmissionReason::ProviderInputBudget,
                approx_tokens: approx_layer_tokens(&spec),
            },
        );
        Some(spec)
    }

    pub(crate) fn mandatory_schema_tokens(&self) -> usize {
        let specs: Vec<&ToolSpec> = self
            .specs
            .iter()
            .filter(|spec| self.mandatory.contains(&spec.name))
            .collect();
        // A text-only plan carries no schema cost at all: the empty-vector
        // JSON wrapper is an artifact of the approximation, not a token the
        // provider will bill for mandatory schemas.
        if specs.is_empty() {
            return 0;
        }
        approx_layer_tokens(&specs)
    }

    pub(crate) fn mandatory_blocks(&self, reason: ToolSurfaceBlockReason) -> Vec<ToolSurfaceBlock> {
        let mut blocks: Vec<_> = self
            .mandatory
            .iter()
            .map(|name| ToolSurfaceBlock {
                tool_name: bounded_name(name),
                demand: ToolSurfaceDemand::MustSurface,
                reason,
            })
            .collect();
        blocks.sort_by(|a, b| a.tool_name.cmp(&b.tool_name));
        blocks
    }

    pub(crate) fn ready_report(&self, context: SurfaceReportContext) -> ToolSurfacePlanReport {
        self.report(context, ToolSurfacePlanStatus::Ready, Vec::new())
    }

    pub(crate) fn unsatisfiable_report(
        &self,
        context: SurfaceReportContext,
        reason: ToolSurfaceBlockReason,
        mut blocked: Vec<ToolSurfaceBlock>,
    ) -> ToolSurfacePlanReport {
        blocked.sort_by(|a, b| a.tool_name.cmp(&b.tool_name));
        self.report(
            context,
            ToolSurfacePlanStatus::Unsatisfiable { reason },
            blocked,
        )
    }

    fn report(
        &self,
        context: SurfaceReportContext,
        status: ToolSurfacePlanStatus,
        blocked: Vec<ToolSurfaceBlock>,
    ) -> ToolSurfacePlanReport {
        let mut selected: Vec<_> = self
            .specs
            .iter()
            .map(|spec| ToolSurfaceSelection {
                tool_name: bounded_name(&spec.name),
                demand: self
                    .demands
                    .get(&spec.name)
                    .copied()
                    .unwrap_or(ToolSurfaceDemand::PreferSurface),
                origin: self
                    .origins
                    .get(&spec.name)
                    .copied()
                    .unwrap_or(ToolSurfaceOrigin::Unknown),
                approx_tokens: approx_layer_tokens(spec),
            })
            .collect();
        selected.sort_by(|a, b| {
            b.demand
                .rank()
                .cmp(&a.demand.rank())
                .then_with(|| a.tool_name.cmp(&b.tool_name))
        });
        let selected_total = selected.len();
        selected.truncate(MAX_TOOL_SURFACE_REPORT_SELECTED);

        let mut omitted = self.omissions.clone();
        omitted.sort_by(|a, b| {
            let a_final = a.reason == ToolSurfaceOmissionReason::CompletionFinalization;
            let b_final = b.reason == ToolSurfaceOmissionReason::CompletionFinalization;
            b_final
                .cmp(&a_final)
                .then_with(|| b.demand.rank().cmp(&a.demand.rank()))
                .then_with(|| a.reason.cmp(&b.reason))
                .then_with(|| a.tool_name.cmp(&b.tool_name))
        });
        for row in &mut omitted {
            row.tool_name = bounded_name(&row.tool_name);
        }
        omitted.truncate(MAX_TOOL_SURFACE_REPORT_OMITTED);

        let blocked_total = blocked.len();
        let mut blocked = blocked;
        for row in &mut blocked {
            row.tool_name = bounded_name(&row.tool_name);
        }
        blocked.truncate(MAX_TOOL_SURFACE_REPORT_BLOCKED);

        ToolSurfacePlanReport {
            turn_id: context.turn_id,
            model_round: context.model_round,
            surface_revision: context.surface_revision,
            source_revisions: self.source_revisions.clone(),
            status,
            selected,
            selected_total,
            omitted,
            omitted_total: self.omitted_total,
            blocked,
            blocked_total,
            selected_schema_tokens: approx_layer_tokens(&self.specs),
            mandatory_schema_tokens: self.mandatory_schema_tokens(),
            estimated_input_tokens: context.estimated_input_tokens,
            input_budget_tokens: context.input_budget_tokens,
        }
    }

    pub(crate) fn into_snapshot(mut self, surface_revision: u64) -> ToolSurfaceSnapshot {
        // Compile the bounded schema profile for every surfaced tool once
        // per revision. A schema that uses an unsupported keyword or exceeds
        // the compile bounds fails capability admission: the tool is not
        // presented to the model and its rejection is recorded for
        // diagnostics. The gate refuses rather than silently skipping
        // validation.
        let mut schema_profiles: BTreeMap<String, SchemaProfile> = BTreeMap::new();
        let mut schema_rejected: Vec<String> = Vec::new();
        for spec in &self.specs {
            match SchemaProfile::compile(&spec.input_schema) {
                Ok(profile) => {
                    schema_profiles.insert(spec.name.clone(), profile);
                }
                Err(error) if schema_rejected.len() < MAX_SCHEMA_REJECTED_ROWS => {
                    schema_rejected.push(format!("{}: {error}", bounded_schema_name(&spec.name)));
                }
                Err(_) => {}
            }
        }
        self.specs
            .retain(|spec| schema_profiles.contains_key(&spec.name));
        ToolSurfaceSnapshot {
            specs: self.specs,
            generation: self.legacy_generation,
            surface_revision,
            source_revisions: self.source_revisions,
            omissions: self.omissions,
            omitted_total: self.omitted_total,
            schema_profiles,
            schema_rejected,
        }
    }
}

/// Keep rejected-schema diagnostics bounded without truncating an exact
/// tool name below its identity.
fn bounded_schema_name(name: &str) -> String {
    if name.chars().count() > 96 {
        let preview: String = name.chars().take(96).collect();
        format!("{preview}...")
    } else {
        name.to_string()
    }
}

const MAX_SCHEMA_REJECTED_ROWS: usize = 16;

fn push_omission(
    rows: &mut Vec<ToolSurfaceOmission>,
    total: &mut usize,
    mut row: ToolSurfaceOmission,
) {
    *total = total.saturating_add(1);
    if rows.len() < MAX_SNAPSHOT_OMISSIONS {
        // The immutable round snapshot is also the execution-time identity
        // ledger. Keep every valid tool name exact (up to the registration
        // bound); only the event projection below uses the tighter display
        // cap. Otherwise two 65..=96 byte names with the same prefix become
        // indistinguishable when a model attempts an omitted call.
        row.tool_name = bounded_snapshot_name(&row.tool_name);
        rows.push(row);
    }
}

fn bounded_snapshot_name(name: &str) -> String {
    bounded_utf8(name, MAX_TOOL_REQUIREMENT_NAME_CHARS)
}

fn bounded_name(name: &str) -> String {
    bounded_utf8(name, MAX_TOOL_SURFACE_REPORT_NAME_BYTES)
}

fn bounded_utf8(name: &str, max_bytes: usize) -> String {
    let mut bounded = String::new();
    for character in name.chars() {
        if bounded.len() + character.len_utf8() > max_bytes {
            break;
        }
        bounded.push(character);
    }
    bounded
}

#[cfg(test)]
mod tests {
    use agent_contracts::{
        MAX_TOOL_SURFACE_REPORT_WIRE_BYTES, ToolRisk, ToolSurfaceSourceRevisions,
    };
    use serde_json::json;

    use super::*;

    fn spec(name: &str, payload_chars: usize) -> ToolSpec {
        // Pad the schema payload, not the description: round-surface compact
        // truncates descriptions and strips nested `description` keys.
        ToolSpec {
            name: name.into(),
            description: "tool".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "pad": {"type": "string", "enum": ["x".repeat(payload_chars)]}
                }
            }),
            risk: ToolRisk::ReadOnly,
            output_budget: None,
            roles: Vec::new(),
        }
    }

    fn requirement(name: &str, demand: ToolSurfaceDemand) -> ToolSurfaceRequirement {
        ToolSurfaceRequirement {
            tool_name: name.into(),
            demand,
            reason: String::new(),
        }
    }

    #[test]
    fn must_is_never_trimmed_and_keep_ready_never_enters_the_prompt() {
        let candidates = ToolSurfaceSnapshot {
            specs: vec![
                spec("optional.large", 20_000),
                spec("required.large", 20_000),
                spec("ready.only", 10),
            ],
            generation: 7,
            source_revisions: ToolSurfaceSourceRevisions {
                builtin_catalog_generation: 7,
                ..Default::default()
            },
            ..Default::default()
        };
        let requirements = vec![
            requirement("required.large", ToolSurfaceDemand::MustSurface),
            requirement("ready.only", ToolSurfaceDemand::KeepReady),
        ];
        let plan = RoundSurfacePlan::build(candidates, &requirements, |_| true);

        let names: Vec<_> = plan.specs().iter().map(|spec| spec.name.as_str()).collect();
        assert_eq!(names, ["required.large"]);
        assert!(plan.omissions.iter().any(|row| {
            row.tool_name == "ready.only" && row.reason == ToolSurfaceOmissionReason::KeepReady
        }));
        assert!(plan.omissions.iter().any(|row| {
            row.tool_name == "optional.large"
                && row.reason == ToolSurfaceOmissionReason::SchemaBudget
        }));
    }

    #[test]
    fn provider_degradation_is_deterministic_and_cannot_remove_must() {
        let candidates = ToolSurfaceSnapshot {
            specs: vec![
                spec("z.optional", 100),
                spec("a.optional", 100),
                spec("must", 1),
            ],
            ..Default::default()
        };
        let requirements = vec![requirement("must", ToolSurfaceDemand::MustSurface)];
        let mut plan = RoundSurfacePlan::build(candidates, &requirements, |_| true);

        let first = plan.omit_largest_for_provider_budget().unwrap();
        let second = plan.omit_largest_for_provider_budget().unwrap();
        assert_eq!(first.name, "z.optional");
        assert_eq!(second.name, "a.optional");
        assert!(plan.omit_largest_for_provider_budget().is_none());
        assert_eq!(plan.specs()[0].name, "must");
    }

    #[test]
    fn completion_finalization_is_a_text_only_audited_surface() {
        let candidates = ToolSurfaceSnapshot {
            specs: vec![spec("task.complete", 10), spec("edit.patch", 10)],
            ..Default::default()
        };
        let requirements = vec![requirement("task.complete", ToolSurfaceDemand::MustSurface)];
        let mut plan = RoundSurfacePlan::build(candidates, &requirements, |_| false);
        plan.force_completion_finalization();

        assert!(plan.specs().is_empty());
        assert_eq!(plan.omitted_total, 2);
        assert!(
            plan.omissions
                .iter()
                .all(|row| { row.reason == ToolSurfaceOmissionReason::CompletionFinalization })
        );
        assert!(plan.omissions.iter().any(|row| {
            row.tool_name == "task.complete" && row.demand == ToolSurfaceDemand::MustSurface
        }));
    }

    #[test]
    fn completion_finalization_keeps_a_reason_when_the_sample_is_full() {
        let candidates = ToolSurfaceSnapshot {
            specs: vec![spec("task.complete", 10)],
            omissions: (0..MAX_SNAPSHOT_OMISSIONS)
                .map(|index| ToolSurfaceOmission {
                    tool_name: format!("old.{index}"),
                    demand: ToolSurfaceDemand::PreferSurface,
                    origin: ToolSurfaceOrigin::CatalogLoadedOptional,
                    reason: ToolSurfaceOmissionReason::SchemaBudget,
                    approx_tokens: 10,
                })
                .collect(),
            omitted_total: MAX_SNAPSHOT_OMISSIONS,
            ..Default::default()
        };
        let requirements = vec![requirement("task.complete", ToolSurfaceDemand::MustSurface)];
        let mut plan = RoundSurfacePlan::build(candidates, &requirements, |_| false);
        plan.force_completion_finalization();

        assert!(plan.specs().is_empty());
        assert_eq!(plan.omitted_total, MAX_SNAPSHOT_OMISSIONS + 1);
        assert!(plan.omissions.iter().any(|row| {
            row.reason == ToolSurfaceOmissionReason::CompletionFinalization
                && row.tool_name == "task.complete"
        }));
        let report = plan.ready_report(SurfaceReportContext {
            turn_id: TurnId::new(),
            model_round: 1,
            surface_revision: 1,
            estimated_input_tokens: 0,
            input_budget_tokens: 0,
        });
        assert!(report.omitted.iter().any(|row| {
            row.reason == ToolSurfaceOmissionReason::CompletionFinalization
                && row.tool_name == "task.complete"
        }));
    }

    #[test]
    fn task_prefer_packs_before_catalog_defaults() {
        let candidates = ToolSurfaceSnapshot {
            specs: vec![spec("catalog.default", 4_000), spec("task.prefer", 14_000)],
            ..Default::default()
        };
        let requirements = vec![requirement("task.prefer", ToolSurfaceDemand::PreferSurface)];
        let plan = RoundSurfacePlan::build(candidates, &requirements, |_| true);

        assert!(plan.specs().iter().any(|spec| spec.name == "task.prefer"));
        assert!(
            plan.omissions
                .iter()
                .any(|row| row.tool_name == "catalog.default")
        );
    }

    #[test]
    fn oversized_prefer_does_not_waste_space_available_to_a_smaller_default() {
        let candidates = ToolSurfaceSnapshot {
            specs: vec![spec("catalog.small", 10), spec("task.too-large", 20_000)],
            ..Default::default()
        };
        let requirements = vec![requirement(
            "task.too-large",
            ToolSurfaceDemand::PreferSurface,
        )];
        let plan = RoundSurfacePlan::build(candidates, &requirements, |_| true);

        assert!(plan.specs().iter().any(|spec| spec.name == "catalog.small"));
        assert!(
            plan.omissions
                .iter()
                .any(|row| row.tool_name == "task.too-large")
        );
    }

    #[test]
    fn report_rows_are_bounded_but_totals_are_exact() {
        let candidates = ToolSurfaceSnapshot {
            specs: (0..200)
                .map(|index| spec(&format!("optional.{index:03}"), 2_000))
                .collect(),
            ..Default::default()
        };
        let plan = RoundSurfacePlan::build(candidates, &[], |_| true);
        let report = plan.ready_report(SurfaceReportContext {
            turn_id: TurnId::new(),
            model_round: 1,
            surface_revision: 1,
            estimated_input_tokens: 1_000,
            input_budget_tokens: 1_000,
        });
        assert!(report.selected.len() <= MAX_TOOL_SURFACE_REPORT_SELECTED);
        assert!(report.omitted.len() <= MAX_TOOL_SURFACE_REPORT_OMITTED);
        assert_eq!(report.selected_total + report.omitted_total, 200);
        assert!(serde_json::to_vec(&report).unwrap().len() < MAX_TOOL_SURFACE_REPORT_WIRE_BYTES);
    }

    #[test]
    fn hostile_dispatcher_omissions_are_bounded_before_entering_turn_state() {
        let candidates = ToolSurfaceSnapshot {
            omissions: (0..1_000)
                .map(|index| ToolSurfaceOmission {
                    tool_name: format!("{}-{index}", "x".repeat(1_000)),
                    demand: ToolSurfaceDemand::PreferSurface,
                    origin: ToolSurfaceOrigin::DispatcherRequired,
                    reason: ToolSurfaceOmissionReason::SchemaBudget,
                    approx_tokens: usize::MAX,
                })
                .collect(),
            ..Default::default()
        };
        let plan = RoundSurfacePlan::build(candidates, &[], |_| true);
        let report = plan.ready_report(SurfaceReportContext {
            turn_id: TurnId::new(),
            model_round: 1,
            surface_revision: 1,
            estimated_input_tokens: 0,
            input_budget_tokens: 0,
        });
        assert_eq!(report.omitted_total, 1_000);
        assert_eq!(report.omitted.len(), MAX_TOOL_SURFACE_REPORT_OMITTED);

        let snapshot = plan.into_snapshot(1);
        assert_eq!(snapshot.omitted_total, 1_000);
        assert_eq!(snapshot.omissions.len(), MAX_SNAPSHOT_OMISSIONS);
        assert!(
            snapshot
                .omissions
                .iter()
                .all(|row| row.tool_name.len() <= MAX_TOOL_REQUIREMENT_NAME_CHARS)
        );
    }

    #[test]
    fn locally_generated_omission_names_are_defensively_bounded() {
        let candidates = ToolSurfaceSnapshot {
            specs: vec![spec(&"z".repeat(1_000), 20_000)],
            ..Default::default()
        };
        let snapshot = RoundSurfacePlan::build(candidates, &[], |_| true).into_snapshot(1);
        assert_eq!(snapshot.omissions.len(), 1);
        assert_eq!(
            snapshot.omissions[0].tool_name.len(),
            MAX_TOOL_REQUIREMENT_NAME_CHARS
        );
    }

    #[test]
    fn snapshot_keeps_a_valid_long_tool_identity_while_the_event_stays_bounded() {
        let exact_name = format!("{}tail", "x".repeat(MAX_TOOL_REQUIREMENT_NAME_CHARS - 4));
        assert!(exact_name.len() > MAX_TOOL_SURFACE_REPORT_NAME_BYTES);
        let candidates = ToolSurfaceSnapshot {
            specs: vec![spec(&exact_name, 20_000)],
            ..Default::default()
        };
        let plan = RoundSurfacePlan::build(candidates, &[], |_| true);
        let report = plan.ready_report(SurfaceReportContext {
            turn_id: TurnId::new(),
            model_round: 1,
            surface_revision: 1,
            estimated_input_tokens: 0,
            input_budget_tokens: 0,
        });
        assert_eq!(
            report.omitted[0].tool_name.len(),
            MAX_TOOL_SURFACE_REPORT_NAME_BYTES
        );

        let snapshot = plan.into_snapshot(1);
        assert_eq!(snapshot.omissions[0].tool_name, exact_name);
    }

    #[test]
    fn maximal_multibyte_report_stays_below_the_wire_cap() {
        let long_name = "🦀".repeat(300);
        let candidates = ToolSurfaceSnapshot {
            specs: (0..MAX_TOOL_SURFACE_REPORT_SELECTED)
                .map(|index| spec(&format!("{long_name}{index}"), 1))
                .collect(),
            omissions: (0..MAX_TOOL_SURFACE_REPORT_OMITTED)
                .map(|index| ToolSurfaceOmission {
                    tool_name: format!("{long_name}{index}"),
                    demand: ToolSurfaceDemand::PreferSurface,
                    origin: ToolSurfaceOrigin::CatalogLoadedOptional,
                    reason: ToolSurfaceOmissionReason::ProviderInputBudget,
                    approx_tokens: usize::MAX,
                })
                .collect(),
            ..Default::default()
        };
        let plan = RoundSurfacePlan::build(candidates, &[], |_| false);
        let blocked = (0..MAX_TOOL_SURFACE_REPORT_BLOCKED)
            .map(|index| ToolSurfaceBlock {
                tool_name: format!("{long_name}{index}"),
                demand: ToolSurfaceDemand::MustSurface,
                reason: ToolSurfaceBlockReason::ProviderInputBudget,
            })
            .collect();
        let report = plan.unsatisfiable_report(
            SurfaceReportContext {
                turn_id: TurnId::new(),
                model_round: usize::MAX,
                surface_revision: u64::MAX,
                estimated_input_tokens: usize::MAX,
                input_budget_tokens: usize::MAX,
            },
            ToolSurfaceBlockReason::ProviderInputBudget,
            blocked,
        );

        assert_eq!(report.selected.len(), MAX_TOOL_SURFACE_REPORT_SELECTED);
        assert_eq!(report.omitted.len(), MAX_TOOL_SURFACE_REPORT_OMITTED);
        assert_eq!(report.blocked.len(), MAX_TOOL_SURFACE_REPORT_BLOCKED);
        let wire_len = serde_json::to_vec(&report).unwrap().len();
        assert!(
            wire_len <= MAX_TOOL_SURFACE_REPORT_WIRE_BYTES,
            "the documented event wire bound must hold at all row/name maxima \
             (wire_len={wire_len}, bound={MAX_TOOL_SURFACE_REPORT_WIRE_BYTES})"
        );
    }

    #[test]
    fn selected_rows_distinguish_task_intent_from_catalog_loads() {
        // Two candidates with the same surface demand: one task-preferred,
        // one a legacy catalog-loaded optional. The report must say which
        // authority put each into consideration.
        let candidates = ToolSurfaceSnapshot {
            specs: vec![
                spec("task.pick", 100),
                spec("catalog.pick", 100),
                spec("core.mandatory", 100),
            ],
            ..Default::default()
        };
        let requirements = vec![requirement("task.pick", ToolSurfaceDemand::PreferSurface)];
        let plan =
            RoundSurfacePlan::build(candidates, &requirements, |name| name != "core.mandatory");
        let report = plan.ready_report(SurfaceReportContext {
            turn_id: TurnId::new(),
            model_round: 1,
            surface_revision: 1,
            estimated_input_tokens: 0,
            input_budget_tokens: 0,
        });
        let by_name = |name: &str| {
            report
                .selected
                .iter()
                .find(|row| row.tool_name == name)
                .unwrap_or_else(|| panic!("{name} must be selected"))
        };
        let task_row = by_name("task.pick");
        let catalog_row = by_name("catalog.pick");
        let core_row = by_name("core.mandatory");
        assert_eq!(task_row.demand, ToolSurfaceDemand::PreferSurface);
        assert_eq!(task_row.origin, ToolSurfaceOrigin::TaskRequirement);
        assert_eq!(catalog_row.demand, ToolSurfaceDemand::PreferSurface);
        assert_eq!(catalog_row.origin, ToolSurfaceOrigin::CatalogLoadedOptional);
        assert_eq!(core_row.demand, ToolSurfaceDemand::MustSurface);
        assert_eq!(core_row.origin, ToolSurfaceOrigin::DispatcherRequired);
    }

    #[test]
    fn omitted_rows_carry_their_authority_origin() {
        // Budget omission must keep provenance too: a task-preferred
        // candidate and a catalog-loaded candidate omitted for the same
        // schema-budget reason remain distinguishable.
        let candidates = ToolSurfaceSnapshot {
            specs: vec![spec("task.big", 40_000), spec("catalog.big", 40_000)],
            ..Default::default()
        };
        let requirements = vec![requirement("task.big", ToolSurfaceDemand::PreferSurface)];
        let plan = RoundSurfacePlan::build(candidates, &requirements, |_| true);
        let report = plan.ready_report(SurfaceReportContext {
            turn_id: TurnId::new(),
            model_round: 1,
            surface_revision: 1,
            estimated_input_tokens: 0,
            input_budget_tokens: 0,
        });
        let by_name = |name: &str| {
            report
                .omitted
                .iter()
                .find(|row| row.tool_name == name)
                .unwrap_or_else(|| panic!("{name} must be omitted"))
        };
        let task_row = by_name("task.big");
        let catalog_row = by_name("catalog.big");
        assert_eq!(task_row.demand, ToolSurfaceDemand::PreferSurface);
        assert_eq!(task_row.origin, ToolSurfaceOrigin::TaskRequirement);
        assert_eq!(task_row.reason, ToolSurfaceOmissionReason::SchemaBudget);
        assert_eq!(catalog_row.demand, ToolSurfaceDemand::PreferSurface);
        assert_eq!(catalog_row.origin, ToolSurfaceOrigin::CatalogLoadedOptional);
        assert_eq!(catalog_row.reason, ToolSurfaceOmissionReason::SchemaBudget);
    }

    #[test]
    fn keep_ready_omissions_name_their_authority() {
        let candidates = ToolSurfaceSnapshot {
            specs: vec![spec("ready.only", 10)],
            ..Default::default()
        };
        let requirements = vec![requirement("ready.only", ToolSurfaceDemand::KeepReady)];
        let plan = RoundSurfacePlan::build(candidates, &requirements, |_| true);
        let report = plan.ready_report(SurfaceReportContext {
            turn_id: TurnId::new(),
            model_round: 1,
            surface_revision: 1,
            estimated_input_tokens: 0,
            input_budget_tokens: 0,
        });
        let row = report
            .omitted
            .iter()
            .find(|row| row.tool_name == "ready.only")
            .expect("keep-ready must be omitted");
        assert_eq!(row.reason, ToolSurfaceOmissionReason::KeepReady);
        assert_eq!(row.origin, ToolSurfaceOrigin::TaskRequirement);
    }

    #[test]
    fn legacy_omission_rows_default_to_unknown_origin() {
        // Old journal events lack provenance; deserializing them must yield
        // `Unknown`, never a fabricated authority claim.
        let json = serde_json::json!({
            "tool_name": "legacy.tool",
            "demand": "prefer_surface",
            "reason": "schema_budget",
            "approx_tokens": 100
        });
        let row: ToolSurfaceOmission = serde_json::from_value(json).unwrap();
        assert_eq!(row.origin, ToolSurfaceOrigin::Unknown);
    }

    #[test]
    fn recovery_mark_relabels_only_the_exact_tool() {
        let candidates = ToolSurfaceSnapshot {
            specs: vec![
                spec("fs.mkdir", 100),
                spec("task.pick", 100),
                spec("catalog.pick", 100),
                spec("core.mandatory", 100),
            ],
            ..Default::default()
        };
        let requirements = vec![
            requirement("fs.mkdir", ToolSurfaceDemand::PreferSurface),
            requirement("task.pick", ToolSurfaceDemand::PreferSurface),
        ];
        let mut plan =
            RoundSurfacePlan::build(candidates, &requirements, |name| name != "core.mandatory");
        plan.mark_recovery_tools(&std::collections::HashSet::from(["fs.mkdir".to_string()]));

        assert_eq!(
            plan.origins.get("fs.mkdir"),
            Some(&ToolSurfaceOrigin::RecoverySurface)
        );
        assert_eq!(
            plan.origins.get("task.pick"),
            Some(&ToolSurfaceOrigin::TaskRequirement)
        );
        assert_eq!(
            plan.origins.get("catalog.pick"),
            Some(&ToolSurfaceOrigin::CatalogLoadedOptional)
        );
        assert_eq!(
            plan.origins.get("core.mandatory"),
            Some(&ToolSurfaceOrigin::DispatcherRequired)
        );

        // The report row answers "why" truthfully: provenance is
        // runtime-derived recovery, not a task pin or a catalog load.
        let report = plan.ready_report(SurfaceReportContext {
            turn_id: TurnId::new(),
            model_round: 1,
            surface_revision: 1,
            estimated_input_tokens: 0,
            input_budget_tokens: 0,
        });
        let mkdir_row = report
            .selected
            .iter()
            .find(|row| row.tool_name == "fs.mkdir")
            .expect("fs.mkdir stays selected after relabel");
        assert_eq!(mkdir_row.origin, ToolSurfaceOrigin::RecoverySurface);
        assert_eq!(mkdir_row.demand, ToolSurfaceDemand::PreferSurface);
    }

    #[test]
    fn recovery_mark_never_touches_absent_or_unrelated_tools() {
        let candidates = ToolSurfaceSnapshot {
            specs: vec![spec("fs.read", 100), spec("fs.write", 100)],
            ..Default::default()
        };
        let mut plan = RoundSurfacePlan::build(candidates, &[], |_| true);
        plan.mark_recovery_tools(&std::collections::HashSet::from(["fs.mkdir".to_string()]));

        // `fs.mkdir` is not in this plan at all; marking an absent tool must
        // not fabricate an origin entry. `fs.write` here is a plain catalog
        // load and must not be relabeled by a recovery claim either.
        assert_eq!(
            plan.origins.get("fs.mkdir"),
            None,
            "marking an absent tool must not fabricate an origin entry"
        );
        assert_eq!(
            plan.origins.get("fs.write"),
            Some(&ToolSurfaceOrigin::CatalogLoadedOptional)
        );
        assert_eq!(
            plan.origins.get("fs.read"),
            Some(&ToolSurfaceOrigin::CatalogLoadedOptional)
        );
    }

    /// A tool whose schema uses an unsupported JSON-schema keyword fails
    /// capability admission: it is not presented on the round surface and
    /// the rejection is recorded, so the model can neither see nor call it.
    #[test]
    fn unsupported_schema_keywords_exclude_the_tool_from_the_surface() {
        let mut broken = spec("plugin.broken", 10);
        broken.input_schema = json!({
            "type": "object",
            "properties": {"x": {"anyOf": [{"type": "string"}]}}
        });
        let snapshot = RoundSurfacePlan::build(
            ToolSurfaceSnapshot {
                specs: vec![spec("fs.read", 10), broken],
                ..Default::default()
            },
            &[],
            |_| true,
        )
        .into_snapshot(7);
        let names: Vec<&str> = snapshot
            .specs
            .iter()
            .map(|spec| spec.name.as_str())
            .collect();
        assert!(
            !names.contains(&"plugin.broken"),
            "a schema-rejected tool must not be surfaced: {names:?}"
        );
        assert!(names.contains(&"fs.read"));
        assert_eq!(snapshot.surface_revision, 7);
        assert!(
            snapshot
                .schema_rejected
                .iter()
                .any(|row| row.contains("plugin.broken")),
            "the rejection must be recorded: {:?}",
            snapshot.schema_rejected
        );
        assert!(!snapshot.schema_profiles.contains_key("plugin.broken"));
        assert!(snapshot.schema_profiles.contains_key("fs.read"));
    }
}
