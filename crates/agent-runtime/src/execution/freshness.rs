//! Mutation footprint application and BeforeModel revalidation.
//!
//! Authority (`may_mutate_workspace`) and knowledge freshness
//! ([`MutationFootprint`]) are not the same question. An unknown process
//! write bumps `workspace_revision` (old PASS is omitted) but keeps
//! `path@revision` facts and marks them `NeedsRevalidation`.

use agent_contracts::{MutationFootprint, ResourceFreshness, ResourceVersionOracle, ToolOutput};

use super::state::{
    ExecutionState, MAX_REVALIDATE_PER_ROUND, VerificationCause, VerificationCoverage, bound_item,
    is_command_tool, operation_identity, path_mentioned_in_query, same_operation,
};

impl ExecutionState {
    pub fn observe_tool(&mut self, output: &ToolOutput, anchor_revision: u64, turn: u64) {
        self.anchor_revision = anchor_revision;
        let footprint = output.mutation_footprint();
        match &footprint {
            MutationFootprint::None => {}
            MutationFootprint::Known(_) | MutationFootprint::Unknown => {
                self.workspace_revision = self.workspace_revision.saturating_add(1);
            }
        }
        match footprint {
            MutationFootprint::None => {}
            MutationFootprint::Unknown => {
                // PASS is stale via workspace_epoch. Do not drop facts.
                self.mark_facts_needs_revalidation();
                if self.last_evidence().is_some() {
                    self.verification.unknown_pending = true;
                }
            }
            MutationFootprint::Known(_) => {}
        }
        let identity = operation_identity(output);
        if output.ok {
            let touches = output.resource_touches();
            for touch in &touches {
                self.upsert_file(
                    &touch.path,
                    touch.revision.clone().unwrap_or_default(),
                    turn,
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
        } else if let Some(touch) = output.resource_touches().first() {
            self.push_failure(
                &identity,
                format!("failed observation {}", touch.path),
                turn,
            );
        } else if is_command_tool(&output.tool_name) {
            self.push_failure(&identity, output.summary.clone(), turn);
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
        self.cap();
        self.refresh_validity();
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
