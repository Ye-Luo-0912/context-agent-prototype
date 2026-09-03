//! Mutation footprint application and BeforeModel revalidation.
//!
//! Authority (`may_mutate_workspace`) and knowledge freshness
//! ([`MutationFootprint`]) are not the same question. An unknown process
//! write bumps `workspace_revision` (old PASS is omitted) but keeps
//! `path@revision` facts and marks them `NeedsRevalidation`.

use agent_contracts::{
    FrontierDelta, MutationFootprint, ResourceFreshness, ResourceVersionOracle, ToolOutput,
};

use super::ResourceProvenance;
use super::state::{
    ExecutionState, MAX_REVALIDATE_PER_ROUND, ObservationEvidence, ResourceObservation,
    RuntimeExecutionAttribution, VerificationCause, VerificationCoverage, bound_item,
    is_command_tool, operation_identity, path_mentioned_in_query, same_operation,
};

impl ExecutionState {
    /// Account a no-dispatch exact PASS reuse without duplicating the
    /// underlying verification fact. The prior fact remains the sole result
    /// authority; this action is redundant evidence at the current frontier.
    pub fn observe_reused_verification(
        &mut self,
        output: &ToolOutput,
        anchor_revision: u64,
        turn: u64,
    ) -> super::state::FrontierObservation {
        self.anchor_revision = anchor_revision;
        self.last_turn = turn;
        let identity = operation_identity(output, "", None);
        let delta = FrontierDelta::RedundantEvidence;
        self.update_convergence(&identity, None, delta);
        self.refresh_validity();
        super::state::FrontierObservation {
            delta,
            actions_since_frontier_advance: self.convergence.actions_since_frontier_advance,
            evidence_revision: self.convergence.evidence_revision,
            invalidated: 0,
            obligation_events: Vec::new(),
            negative_fact_events: Vec::new(),
            verification_pass_events: Vec::new(),
        }
    }

    /// 无 Runtime 参数摘要时的便捷入口（测试/旧路径）：证据身份退化为
    /// 参数摘要。生产主路径走 [`Self::observe_tool_with_digest`]。
    pub fn observe_tool(
        &mut self,
        output: &ToolOutput,
        anchor_revision: u64,
        turn: u64,
    ) -> super::state::FrontierObservation {
        self.observe_tool_with_digest(output, anchor_revision, turn, "")
    }

    /// 证据的 argument_digest 用 Runtime 在
    /// OperationCompletion 计算的真值，消除同 argv 不同 cwd/env、
    /// 同 path 不同 limit/cursor 的身份碰撞。
    pub fn observe_tool_with_digest(
        &mut self,
        output: &ToolOutput,
        anchor_revision: u64,
        turn: u64,
        argument_digest: &str,
    ) -> super::state::FrontierObservation {
        self.observe_tool_inner(
            output,
            anchor_revision,
            turn,
            argument_digest,
            None,
            output.is_verification(),
        )
    }

    /// Observation without pre-dispatch attribution: verification authority
    /// comes from facts captured on the dispatcher lane. Frames without a
    /// stamped claim fall back to the legacy metadata read, which yields
    /// identical values for every producer class today.
    pub fn observe_tool_facts(
        &mut self,
        output: &ToolOutput,
        anchor_revision: u64,
        turn: u64,
        argument_digest: &str,
        facts: &agent_contracts::ToolExecutionFacts,
    ) -> super::state::FrontierObservation {
        let is_verification = facts
            .is_verification()
            .unwrap_or_else(|| output.is_verification());
        self.observe_tool_inner(
            output,
            anchor_revision,
            turn,
            argument_digest,
            None,
            is_verification,
        )
    }

    /// Production observation path. Verification authority comes only from
    /// trusted pre-dispatch attribution; producer metadata cannot
    /// retroactively turn shell/process or a dynamic capability into a
    /// reusable verifier.
    pub fn observe_tool_attributed(
        &mut self,
        output: &ToolOutput,
        anchor_revision: u64,
        turn: u64,
        argument_digest: &str,
        attribution: &RuntimeExecutionAttribution,
    ) -> super::state::FrontierObservation {
        let is_verification = attribution.reusable_verification();
        self.observe_tool_inner(
            output,
            anchor_revision,
            turn,
            argument_digest,
            Some(attribution),
            is_verification,
        )
    }

    fn observe_tool_inner(
        &mut self,
        output: &ToolOutput,
        anchor_revision: u64,
        turn: u64,
        argument_digest: &str,
        attribution: Option<&RuntimeExecutionAttribution>,
        is_verification: bool,
    ) -> super::state::FrontierObservation {
        self.anchor_revision = anchor_revision;
        self.last_turn = turn;
        // progress probe: capture the before-state so one
        // deterministic classification can answer "did this round move
        // the world or our knowledge of it?"
        let before_failures = self.unresolved_failed_command_count();
        let before_verifications = self.verifications.len();
        let before_last_evidence = self
            .verifications
            .last()
            .map(|row| (row.ok, row.summary.clone()));
        let mut resource_observation = ResourceObservation::None;
        let mut negative_fact_events = Vec::new();
        let mut verification_pass_events = Vec::new();
        let footprint = output.mutation_footprint();
        match &footprint {
            MutationFootprint::None => {}
            MutationFootprint::Known(_) | MutationFootprint::Unknown => {
                self.workspace_revision = self.workspace_revision.saturating_add(1);
                negative_fact_events.extend(self.invalidate_negative_facts_for_world_change());
            }
        }
        // Unknown mutation：无法知道改了什么，旧事实先全部降级为待复核；
        // 本输出自己盖的可信章随后照常入表。必须在 upsert 之前做。
        if matches!(footprint, MutationFootprint::Unknown) {
            self.mark_facts_needs_revalidation();
        }
        let identity = operation_identity(output, argument_digest, attribution);
        // Unknown mutation 的 PASS-stale 标记以观察前的验证史为准
        // （本输出若自带验证结果，不得把自己标成待复核）。
        let had_prior_verification_evidence = self.last_evidence().is_some();
        if output.ok {
            let touches = output.resource_touches();
            // Provenance is diagnostic only: which kind of trusted
            // observation last stamped this fact.
            let provenance = if is_verification {
                ResourceProvenance::Verification
            } else if output.may_mutate_workspace() {
                ResourceProvenance::MutationResult
            } else {
                ResourceProvenance::Read
            };
            for touch in &touches {
                resource_observation.merge(self.upsert_file(
                    &touch.path,
                    touch.revision.clone().unwrap_or_default(),
                    turn,
                    provenance,
                ));
            }
            // Exact operation identity is sufficient only for an untyped or
            // NonDeterministic blocker. Deterministic domains are resolved
            // below by the same domain/scope/precondition predicate as the
            // obligation ledger; clearing them here would let the two views
            // disagree when a resolver epoch changed.
            self.failed_commands.retain(|row| {
                row.domain != agent_contracts::ToolFailureDomain::NonDeterministic
                    || !same_operation(row, &identity)
            });
            if is_verification {
                if let Some(event) =
                    self.push_verification(output, argument_digest, attribution, turn)
                {
                    verification_pass_events.push(event);
                }
                // Keep the verification basis (spec_revision) as tracked by
                // TaskAnchor.verification_revision; a progress-only anchor
                // CAS must not move it. Authority changes already bump the
                // basis via TaskManager commit.
                self.verification.cause = VerificationCause::None;
                self.verification.source_changed = false;
                self.verification.unknown_pending = false;
                self.verification.failed_open = false;
                if matches!(
                    self.verification.coverage,
                    VerificationCoverage::Unspecified
                ) {
                    self.verification.coverage = VerificationCoverage::Workspace;
                }
            }
        } else {
            // a refused mutation still observed the world —
            // the tool read the target to refuse it, so its trusted
            // path+revision stamp is real world truth even though the
            // write did not apply. Consuming it here clears
            // NeedsRevalidation without a model-driven re-read. A
            // failed *read* saw nothing and stays out of the fact
            // table.
            if output.may_mutate_workspace() {
                for touch in output.resource_touches() {
                    if let Some(revision) = touch
                        .revision
                        .clone()
                        .filter(|revision| !revision.is_empty())
                    {
                        resource_observation.merge(self.upsert_file(
                            &touch.path,
                            revision,
                            turn,
                            ResourceProvenance::MutationRefusal,
                        ));
                    }
                }
            }
            if let Some(touch) = output.resource_touches().first() {
                // A typed precondition refusal on a mutating call (no exact
                // anchor, ambiguous anchor, stale revision, path boundary)
                // proves the trusted handler applied nothing and spawned
                // nothing. It stays visible in the result and the
                // negative-fact table but is an attempt incident, not
                // completion debt: retry churn must not compound the
                // completion-gate blocker set. A failed read keeps its row
                // when the target is task-rooted (or when legacy callers do
                // not provide attribution). An attributed exploratory miss
                // remains a negative fact, but it is not unfinished task
                // work: otherwise one speculative path can permanently
                // prevent an unrelated, completed task from closing.
                let proven_no_effect = output.may_mutate_workspace()
                    && output
                        .failure_class()
                        .is_some_and(|class| class.proves_no_effect());
                let unrooted_exploratory_read = !output.may_mutate_workspace()
                    && attribution.is_some_and(|attribution| {
                        attribution.speculative_negative_target(output).is_some()
                    });
                if !proven_no_effect && !unrooted_exploratory_read {
                    self.push_failure(
                        output,
                        &identity,
                        format!("failed observation {}", touch.path),
                        turn,
                    );
                }
            } else if is_command_tool(&output.tool_name) {
                self.push_failure(output, &identity, output.summary.clone(), turn);
            }
        }
        if is_verification && !output.ok {
            let _ = self.push_verification(output, argument_digest, attribution, turn);
            self.verification.cause = VerificationCause::FailureRepair;
            self.verification.failed_open = true;
        }
        if matches!(footprint, MutationFootprint::Unknown) && had_prior_verification_evidence {
            self.verification.unknown_pending = true;
        }
        self.record_verification_source(&output.tool_name, argument_digest, attribution);
        let (speculative_negative, negative_transitions) =
            self.observe_negative_fact(output, argument_digest, turn, attribution);
        negative_fact_events.extend(negative_transitions);
        // 本轮可信事实已落表后再统一裁决证据现势性——
        // edit 的新 digest 必须当场杀死绑定旧 digest 的证据行，而不是
        // 等到下一轮。失效条数随事件上报。
        let invalidated = self.invalidate_stale_evidence();
        // 义务账本：先解析（本输出可能已解除旧义务或推进 epoch），再
        // 登记新失败；账目事件随 FrontierObservation 出账（ ）。
        let mut obligation_events = Vec::new();
        self.resolve_failure_blockers(output, attribution, &mut obligation_events);
        self.record_obligation(
            output,
            &identity,
            speculative_negative,
            &mut obligation_events,
        );
        self.cap(&mut obligation_events);
        self.refresh_validity();
        // Deterministic frontier classification: a verification
        // result is always typed evidence; otherwise footprint decides —
        // Known is provable world change, Unknown is only invalidation,
        // and read-only rounds split into evidence gain vs redundant
        // repeat vs obligation resolution vs nothing.
        let facts_gained = resource_observation == ResourceObservation::Advanced;
        let facts_reconfirmed = resource_observation == ResourceObservation::Reconfirmed;
        let evidence_gained = self.verifications.len() > before_verifications
            && self
                .verifications
                .last()
                .map(|row| (row.ok, row.summary.clone()))
                != before_last_evidence;
        let failure_resolved = self.unresolved_failed_command_count() < before_failures;
        let obs_evidence = if output.ok && !is_verification && !output.may_mutate_workspace() {
            // 证据身份用 Runtime 的真 ArgumentDigest，
            // 不在 ToolOutput 上反推；缺省时退化为参数摘要。
            self.record_observation_evidence(output, turn, argument_digest)
        } else {
            ObservationEvidence::None
        };
        let rooted_observation = output.ok
            && attribution.is_some_and(|attribution| !attribution.rooted_targets.is_empty());
        let read_only_evidence_gained =
            facts_gained || evidence_gained || obs_evidence == ObservationEvidence::Advanced;
        // A targeted directive must not appear to converge forever by
        // discovering unrelated, globally novel evidence. Keep every fact in
        // the bounded evidence tables, but advance the task frontier only for
        // rooted evidence once an exact current target is known. Directives
        // without a known target retain broad exploration semantics.
        let evidence_advances_task = read_only_evidence_gained
            && (!self.directive_has_rooted_evidence || rooted_observation);
        if rooted_observation {
            self.directive_has_rooted_evidence = true;
        }
        let delta = if is_verification && output.ok {
            if evidence_gained {
                FrontierDelta::EvidenceAdvanced
            } else {
                FrontierDelta::RedundantEvidence
            }
        } else {
            match &footprint {
                MutationFootprint::Known(_) => FrontierDelta::ObservedWorldChange,
                MutationFootprint::Unknown => {
                    // 每个未知足迹轮都推进世界时钟，"同版本重复"对
                    // 命令运行不成立；失效本身不是进展。
                    FrontierDelta::WorldInvalidatedUnknown
                }
                MutationFootprint::None => {
                    if evidence_advances_task {
                        FrontierDelta::EvidenceAdvanced
                    } else if failure_resolved {
                        FrontierDelta::ObligationResolved
                    } else if facts_reconfirmed
                        || matches!(obs_evidence, ObservationEvidence::Reconfirmed)
                    {
                        FrontierDelta::EvidenceReconfirmed
                    } else if matches!(obs_evidence, ObservationEvidence::Repeated) {
                        FrontierDelta::RedundantEvidence
                    } else {
                        FrontierDelta::NoProgress
                    }
                }
            }
        };
        self.update_convergence(&identity, output.failure_class(), delta);
        super::state::FrontierObservation {
            delta,
            actions_since_frontier_advance: self.convergence.actions_since_frontier_advance,
            evidence_revision: self.convergence.evidence_revision,
            invalidated,
            obligation_events,
            negative_fact_events,
            verification_pass_events,
        }
    }

    /// Runtime hash check at BeforeModel. Cap N=8; no file body enters the
    /// prompt. Same digest stays Fresh; a real change updates the fact and
    /// marks verification stale; a missing path is recorded as Missing.
    ///
    /// After an Unknown mutation, [`VerificationCoverage::Resources`] may
    /// return to Current when every covered path revalidates
    /// identity-unchanged. [`VerificationCoverage::Workspace`] and
    /// [`VerificationCoverage::Unspecified`] cannot: untracked files may
    /// have changed, so a new Verify is required.
    pub async fn revalidate(&mut self, oracle: &dyn ResourceVersionOracle, query: &str) {
        self.revalidate_with_priority(oracle, query, &[]).await;
    }

    /// Same bounded hash-only revalidation, with exact body identities that
    /// the protocol checkpoint is about to spill ranked first. Verification
    /// coverage and current directive mentions follow; the cap remains 8.
    pub async fn revalidate_with_priority(
        &mut self,
        oracle: &dyn ResourceVersionOracle,
        query: &str,
        priority_body_identities: &[String],
    ) {
        let mut pending: Vec<usize> = self
            .checked_files
            .iter()
            .enumerate()
            .filter(|(_, fact)| fact.freshness == ResourceFreshness::NeedsRevalidation)
            .map(|(index, _)| index)
            .collect();
        pending.sort_by_key(|&index| {
            let fact = &self.checked_files[index];
            let body_rank = agent_contracts::file_body_identity(&fact.path, &fact.digest)
                .and_then(|identity| {
                    priority_body_identities
                        .iter()
                        .take(agent_contracts::MAX_VISIBLE_BODY_HINTS)
                        .position(|candidate| candidate == &identity)
                })
                .unwrap_or(usize::MAX);
            let verification_covered = matches!(
                &self.verification.coverage,
                VerificationCoverage::Resources(paths)
                    if paths.iter().any(|path| path == &fact.path)
            );
            (
                body_rank,
                std::cmp::Reverse(verification_covered),
                std::cmp::Reverse(path_mentioned_in_query(query, &fact.path)),
                std::cmp::Reverse(fact.turn),
                index,
            )
        });
        pending.truncate(MAX_REVALIDATE_PER_ROUND);
        for index in pending {
            let key = self.checked_files[index].path.clone();
            let prior = self.checked_files[index].digest.clone();
            match oracle.revision(&key).await {
                Ok(Some(revision)) => {
                    if !prior.is_empty() && revision != prior {
                        self.mark_source_changed(&key);
                        self.verification.unknown_pending = false;
                    }
                    self.checked_files[index].digest = bound_item(&revision);
                    self.checked_files[index].freshness = ResourceFreshness::Fresh;
                    self.checked_files[index].provenance = ResourceProvenance::Read;
                }
                Ok(None) => {
                    self.checked_files[index].freshness = ResourceFreshness::Missing;
                }
                Err(_) => {}
            }
        }
        if self.verification.unknown_pending
            && !self.verification.source_changed
            && let VerificationCoverage::Resources(paths) = &self.verification.coverage
            && !paths.is_empty()
            && self.covered_resources_identity_confirmed(paths)
        {
            self.verification.unknown_pending = false;
        }
        self.refresh_validity();
    }
}
