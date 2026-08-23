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
    ExecutionState, MAX_REVALIDATE_PER_ROUND, ObservationEvidence, VerificationCause,
    VerificationCoverage, bound_item, is_command_tool, operation_identity, path_mentioned_in_query,
    same_operation,
};

impl ExecutionState {
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

    /// 评审第 17 条：证据的 argument_digest 用 Runtime 在
    /// OperationCompletion 计算的真值，消除同 argv 不同 cwd/env、
    /// 同 path 不同 limit/cursor 的身份碰撞。
    pub fn observe_tool_with_digest(
        &mut self,
        output: &ToolOutput,
        anchor_revision: u64,
        turn: u64,
        argument_digest: &str,
    ) -> super::state::FrontierObservation {
        self.anchor_revision = anchor_revision;
        self.last_turn = turn;
        // MOD-PROG-01 progress probe: capture the before-state so one
        // deterministic classification can answer "did this round move
        // the world or our knowledge of it?"
        let before_files = self.checked_files.len();
        let before_failures = self.failed_commands.len();
        let before_verifications = self.verifications.len();
        let before_last_evidence = self
            .verifications
            .last()
            .map(|row| (row.ok, row.summary.clone()));
        let mut observation_changed = false;
        let footprint = output.mutation_footprint();
        match &footprint {
            MutationFootprint::None => {}
            MutationFootprint::Known(_) | MutationFootprint::Unknown => {
                self.workspace_revision = self.workspace_revision.saturating_add(1);
            }
        }
        // Unknown mutation：无法知道改了什么，旧事实先全部降级为待复核；
        // 本输出自己盖的可信章随后照常入表。必须在 upsert 之前做。
        if matches!(footprint, MutationFootprint::Unknown) {
            self.mark_facts_needs_revalidation();
        }
        let identity = operation_identity(output);
        // Unknown mutation 的 PASS-stale 标记以观察前的验证史为准
        // （本输出若自带验证结果，不得把自己标成待复核）。
        let had_prior_verification_evidence = self.last_evidence().is_some();
        if output.ok {
            let touches = output.resource_touches();
            // Provenance is diagnostic only: which kind of trusted
            // observation last stamped this fact.
            let provenance = if output.is_verification() {
                ResourceProvenance::Verification
            } else if output.may_mutate_workspace() {
                ResourceProvenance::MutationResult
            } else {
                ResourceProvenance::Read
            };
            for touch in &touches {
                observation_changed |= self.upsert_file(
                    &touch.path,
                    touch.revision.clone().unwrap_or_default(),
                    turn,
                    provenance,
                );
            }
            self.failed_commands
                .retain(|row| !same_operation(row, &identity));
            if output.is_verification() {
                self.push_verification(
                    output.summary.clone(),
                    true,
                    turn,
                    output.artifact_ref.clone(),
                );
                self.verification.spec_revision = self.anchor_revision;
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
            // MOD-OBS-01: a refused mutation still observed the world —
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
                        observation_changed |= self.upsert_file(
                            &touch.path,
                            revision,
                            turn,
                            ResourceProvenance::MutationRefusal,
                        );
                    }
                }
            }
            if let Some(touch) = output.resource_touches().first() {
                self.push_failure(
                    &identity,
                    format!("failed observation {}", touch.path),
                    turn,
                );
            } else if is_command_tool(&output.tool_name) {
                self.push_failure(&identity, output.summary.clone(), turn);
            }
        }
        if output.is_verification() && !output.ok {
            self.push_verification(
                output.summary.clone(),
                false,
                turn,
                output.artifact_ref.clone(),
            );
            self.verification.cause = VerificationCause::FailureRepair;
            self.verification.spec_revision = self.anchor_revision;
            self.verification.failed_open = true;
        }
        if matches!(footprint, MutationFootprint::Unknown) && had_prior_verification_evidence {
            self.verification.unknown_pending = true;
        }
        // EXEC-EVID-01a：本轮可信事实已落表后再统一裁决证据现势性——
        // edit 的新 digest 必须当场杀死绑定旧 digest 的证据行，而不是
        // 等到下一轮。失效条数随事件上报。
        let invalidated = self.invalidate_stale_evidence();
        // 义务账本：先解析（本输出可能已解除旧义务），再登记新失败。
        self.resolve_obligations(output);
        self.record_obligation(output, &identity);
        self.cap();
        self.refresh_validity();
        // Deterministic frontier classification (CONV-01): a verification
        // result is always typed evidence; otherwise footprint decides —
        // Known is provable world change, Unknown is only invalidation,
        // and read-only rounds split into evidence gain vs redundant
        // repeat vs obligation resolution vs nothing.
        let facts_gained = observation_changed || self.checked_files.len() > before_files;
        let evidence_gained = self.verifications.len() > before_verifications
            && self
                .verifications
                .last()
                .map(|row| (row.ok, row.summary.clone()))
                != before_last_evidence;
        let failure_resolved = self.failed_commands.len() < before_failures;
        let obs_evidence =
            if output.ok && !output.is_verification() && !output.may_mutate_workspace() {
                // 评审第 17 条：证据身份用 Runtime 的真 ArgumentDigest，
                // 不在 ToolOutput 上反推；缺省时退化为参数摘要。
                self.record_observation_evidence(output, turn, argument_digest)
            } else {
                ObservationEvidence::None
            };
        let delta = if output.is_verification() && output.ok {
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
                    if facts_gained
                        || evidence_gained
                        || obs_evidence == ObservationEvidence::Advanced
                    {
                        FrontierDelta::EvidenceAdvanced
                    } else if failure_resolved {
                        FrontierDelta::ObligationResolved
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
        let mut pending: Vec<usize> = self
            .checked_files
            .iter()
            .enumerate()
            .filter(|(_, fact)| fact.freshness == ResourceFreshness::NeedsRevalidation)
            .map(|(index, _)| index)
            .collect();
        pending.sort_by_key(|&index| {
            let fact = &self.checked_files[index];
            (
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
