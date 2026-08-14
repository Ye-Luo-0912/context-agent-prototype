//! Bounded Core-owned operation registry with an optional synchronous WAL.
//!
//! This is authority state, not a scheduler: Runtime decides when work is
//! attempted, while Core atomically records and validates identities and
//! effect transitions. The WAL restores exact authority state across process
//! restarts. A composition-provided reconciler may close managed effect
//! windows; unsupported or ambiguous effects remain fail-closed.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};

use agent_contracts::{
    AgentError, AgentResult, AuthorityCheckpointMarker, AuthorityRecoveryStatus, EffectDurability,
    EffectId, EffectReceipt, MAX_OPERATION_DIAGNOSTIC_BYTES, MAX_OPERATION_EVIDENCE_BYTES,
    OperationId, OperationJournal, OperationJournalRecovery, OperationJournalTransition,
    OperationQueryResult, OperationSnapshot, OperationState, OperationTerminal,
    ToolOperationIdentity,
};

pub(crate) const DEFAULT_OPERATION_REGISTRY_CAPACITY: usize = 1_024;
const SEEN_OPERATION_FILTER_BITS: usize = 1 << 21;
const SEEN_OPERATION_FILTER_WORDS: usize = SEEN_OPERATION_FILTER_BITS / u64::BITS as usize;

pub(crate) struct OperationRegistry {
    capacity: usize,
    /// Serializes journal-first authority transitions. The state mutex is
    /// deliberately released before calling the external journal so a
    /// journal implementation can never re-enter while Core state is locked.
    transition_gate: Mutex<()>,
    journal: Option<Arc<dyn OperationJournal>>,
    inner: Mutex<RegistryState>,
}

#[derive(Debug, Default)]
struct RegistryState {
    records: HashMap<OperationId, OperationSnapshot>,
    terminal_order: VecDeque<OperationId>,
    seen: SeenOperationFilter,
    leases: HashMap<OperationId, agent_contracts::AuthorityLease>,
    recovery_required: Option<String>,
}

#[derive(Debug)]
struct SeenOperationFilter {
    words: Box<[u64]>,
}

impl Default for SeenOperationFilter {
    fn default() -> Self {
        Self {
            words: vec![0; SEEN_OPERATION_FILTER_WORDS].into_boxed_slice(),
        }
    }
}

impl SeenOperationFilter {
    fn may_contain(&self, operation_id: OperationId) -> bool {
        seen_filter_indexes(operation_id)
            .into_iter()
            .all(|index| self.words[index / 64] & (1_u64 << (index % 64)) != 0)
    }

    fn insert(&mut self, operation_id: OperationId) {
        for index in seen_filter_indexes(operation_id) {
            self.words[index / 64] |= 1_u64 << (index % 64);
        }
    }
}

impl OperationRegistry {
    #[cfg(test)]
    pub(crate) fn new(capacity: usize) -> Self {
        Self::recover(capacity, None, OperationJournalRecovery::default())
            .expect("empty in-memory operation registry recovery is valid")
    }

    pub(crate) fn recover(
        capacity: usize,
        journal: Option<Arc<dyn OperationJournal>>,
        recovery: OperationJournalRecovery,
    ) -> AgentResult<Self> {
        assert!(capacity > 0, "operation registry capacity must be positive");
        let mut state = RegistryState::default();
        let unresolved = recovery
            .operations
            .iter()
            .filter(|snapshot| !matches!(snapshot.state, OperationState::Terminal { .. }))
            .count();
        if unresolved > capacity {
            return Err(AgentError::RecoveryRequired(format!(
                "operation journal recovered {unresolved} unresolved operations, exceeding Core capacity {capacity}"
            )));
        }
        for snapshot in recovery.operations {
            snapshot.validate().map_err(AgentError::InvalidRequest)?;
            let operation_id = snapshot.identity.operation_id;
            state.seen.insert(operation_id);
            if matches!(snapshot.state, OperationState::Terminal { .. }) {
                state.terminal_order.push_back(operation_id);
            }
            state.records.insert(operation_id, snapshot);
        }
        evict_terminal_until_fit(&mut state, capacity);
        let registry = Self {
            capacity,
            transition_gate: Mutex::new(()),
            journal,
            inner: Mutex::new(state),
        };
        Ok(registry)
    }

    pub(crate) fn persist_epoch_advance(&self, from: u64, to: u64) -> AgentResult<()> {
        let _transition = self
            .transition_gate
            .lock()
            .expect("operation transition gate poisoned");
        self.ensure_healthy()?;
        self.append_transition(OperationJournalTransition::EpochAdvanced { from, to })
    }

    /// Snapshot the bounded recovered authority set for deterministic
    /// startup reconciliation. This is read-only authority inspection, not
    /// scheduling; callers may only use the paired recovery transition below
    /// to publish a proven terminal state.
    pub(crate) fn recovered_snapshots(&self) -> Vec<OperationSnapshot> {
        let mut snapshots: Vec<_> = self
            .inner
            .lock()
            .expect("operation registry poisoned")
            .records
            .values()
            .cloned()
            .collect();
        snapshots.sort_by_key(|snapshot| snapshot.identity.operation_id.0);
        snapshots
    }

    /// Publish a startup-reconciled terminal state WAL-first. The exact
    /// recovered state must still match, preventing a stale recovery result
    /// from overwriting newer authority truth. This deliberately bypasses a
    /// *pre-existing* recovery fence because startup installs that fence only
    /// after all independently provable records have been folded.
    pub(crate) fn recover_terminal(
        &self,
        operation_id: OperationId,
        expected: &OperationState,
        effect_id: Option<EffectId>,
        terminal: OperationTerminal,
    ) -> AgentResult<()> {
        terminal.validate().map_err(AgentError::InvalidRequest)?;
        let expected_effect_id = match expected {
            OperationState::Accepted | OperationState::Executing { effect_id: None } => None,
            OperationState::Executing {
                effect_id: Some(expected),
            }
            | OperationState::Prepared {
                effect_id: expected,
            }
            | OperationState::CommitStarted {
                effect_id: expected,
            } => Some(*expected),
            OperationState::Terminal { .. } => {
                return Err(AgentError::InvalidRequest(
                    "startup recovery cannot rewrite a terminal operation".into(),
                ));
            }
        };
        if expected_effect_id != effect_id {
            return Err(AgentError::InvalidRequest(
                "startup recovery terminal effect id does not match recovered authority state"
                    .into(),
            ));
        }
        let _transition = self
            .transition_gate
            .lock()
            .expect("operation transition gate poisoned");
        let next = {
            let registry = self.inner.lock().expect("operation registry poisoned");
            let record = registry.records.get(&operation_id).ok_or_else(|| {
                AgentError::InvalidRequest(format!("unknown operation {operation_id}"))
            })?;
            if &record.state != expected {
                return Err(AgentError::RecoveryRequired(format!(
                    "operation {operation_id} changed while startup recovery was reconciling it"
                )));
            }
            OperationSnapshot {
                identity: record.identity.clone(),
                state: OperationState::Terminal {
                    effect_id,
                    terminal,
                },
            }
        };
        self.append_snapshot(&next)?;
        let mut registry = self.inner.lock().expect("operation registry poisoned");
        registry.records.insert(operation_id, next);
        registry.leases.remove(&operation_id);
        registry.terminal_order.push_back(operation_id);
        Ok(())
    }

    /// Install the process-wide mutation fence after startup has folded all
    /// provable records. Reasons are bounded before becoming public status.
    pub(crate) fn require_recovery(&self, reason: impl AsRef<str>) {
        let reason = bounded_text(reason.as_ref(), MAX_OPERATION_DIAGNOSTIC_BYTES);
        let mut registry = self.inner.lock().expect("operation registry poisoned");
        if registry.recovery_required.is_none() {
            registry.recovery_required = Some(reason);
        }
    }

    pub(crate) fn recovery_status(&self) -> AuthorityRecoveryStatus {
        match self
            .inner
            .lock()
            .expect("operation registry poisoned")
            .recovery_required
            .clone()
        {
            Some(reason) => AuthorityRecoveryStatus::RecoveryRequired { reason },
            None => AuthorityRecoveryStatus::Ready,
        }
    }

    pub(crate) fn ensure_mutation_allowed(&self) -> AgentResult<()> {
        self.ensure_healthy()
    }

    pub(crate) fn authority_checkpoint_marker(
        &self,
    ) -> AgentResult<Option<AuthorityCheckpointMarker>> {
        let _transition = self
            .transition_gate
            .lock()
            .expect("operation transition gate poisoned");
        self.ensure_healthy()?;
        self.journal
            .as_ref()
            .map(|journal| journal.authority_checkpoint_marker())
            .transpose()
    }

    pub(crate) fn validate_authority_checkpoint_marker(
        &self,
        expected: &AuthorityCheckpointMarker,
    ) -> AgentResult<()> {
        let _transition = self
            .transition_gate
            .lock()
            .expect("operation transition gate poisoned");
        self.ensure_healthy()?;
        let journal = self.journal.as_ref().ok_or_else(|| {
            AgentError::RecoveryRequired(
                "Core has no durable operation journal for authority checkpoint validation".into(),
            )
        })?;
        journal.validate_authority_checkpoint_marker(expected)
    }

    pub(crate) fn compact_authority_journal(
        &self,
    ) -> AgentResult<Option<AuthorityCheckpointMarker>> {
        let _transition = self
            .transition_gate
            .lock()
            .expect("operation transition gate poisoned");
        self.ensure_healthy()?;
        self.journal
            .as_ref()
            .map(|journal| journal.compact())
            .transpose()
    }

    /// Register a new logical operation before dispatch. An exact duplicate
    /// returns the existing snapshot and never dispatches twice; reusing an
    /// id for different work is a protocol violation.
    pub(crate) fn accept(
        &self,
        identity: ToolOperationIdentity,
    ) -> AgentResult<OperationAdmission> {
        identity.validate().map_err(AgentError::InvalidRequest)?;
        let _transition = self.transition_guard()?;
        let snapshot = OperationSnapshot {
            identity: identity.clone(),
            state: OperationState::Accepted,
        };
        {
            let state = self.inner.lock().expect("operation registry poisoned");
            if let Some(existing) = state.records.get(&identity.operation_id) {
                return if existing.identity == identity {
                    Ok(OperationAdmission::Duplicate(Box::new(existing.clone())))
                } else {
                    Err(AgentError::InvalidRequest(format!(
                        "operation {} was reused with a different identity or argument digest",
                        identity.operation_id
                    )))
                };
            }
            if state.seen.may_contain(identity.operation_id) {
                return Err(AgentError::InvalidRequest(format!(
                    "operation {} was already admitted (or collided with Core's bounded fail-closed tombstone filter); query or use a new operation id instead of replaying it",
                    identity.operation_id
                )));
            }
            if !has_capacity_or_evictable_terminal(&state, self.capacity) {
                return Err(AgentError::RecoveryRequired(format!(
                    "operation registry is full with {} unresolved operation(s)",
                    state.records.len()
                )));
            }
        }
        self.append_snapshot(&snapshot)?;
        let mut state = self.inner.lock().expect("operation registry poisoned");
        evict_terminal_until_fit(&mut state, self.capacity);
        state.seen.insert(identity.operation_id);
        state.records.insert(identity.operation_id, snapshot);
        Ok(OperationAdmission::Accepted)
    }

    pub(crate) fn mark_executing(
        &self,
        operation_id: OperationId,
        effect_id: Option<EffectId>,
    ) -> AgentResult<()> {
        if effect_id.is_some_and(|id| id.0.is_nil()) {
            return Err(AgentError::InvalidRequest(
                "operation cannot reserve a nil effect id".into(),
            ));
        }
        self.transition(operation_id, |state| match state {
            OperationState::Accepted => Ok(OperationState::Executing { effect_id }),
            _ => Err("operation is not accepted"),
        })
    }

    pub(crate) fn record_lease(
        &self,
        operation_id: OperationId,
        lease: agent_contracts::AuthorityLease,
    ) -> AgentResult<()> {
        let _transition = self.transition_guard()?;
        let mut registry = self.inner.lock().expect("operation registry poisoned");
        let record = registry.records.get(&operation_id).ok_or_else(|| {
            AgentError::InvalidRequest(format!("unknown operation {operation_id}"))
        })?;
        if !matches!(record.state, OperationState::Accepted)
            || lease.operation_id != operation_id
            || lease.argument_digest != record.identity.argument_digest
            || lease.operation_generation != record.identity.generation
        {
            return Err(AgentError::InvalidRequest(format!(
                "operation {operation_id} cannot record this authority lease"
            )));
        }
        registry.leases.insert(operation_id, lease);
        Ok(())
    }

    pub(crate) fn issued_lease_matches(
        &self,
        operation_id: OperationId,
        lease: &agent_contracts::AuthorityLease,
    ) -> bool {
        self.inner
            .lock()
            .expect("operation registry poisoned")
            .leases
            .get(&operation_id)
            .is_some_and(|issued| issued == lease)
    }

    pub(crate) fn mark_prepared(
        &self,
        operation_id: OperationId,
        effect_id: EffectId,
    ) -> AgentResult<()> {
        self.transition(operation_id, |state| match state {
            OperationState::Executing {
                effect_id: Some(expected),
            } if expected == effect_id => Ok(OperationState::Prepared { effect_id }),
            OperationState::Executing { effect_id: Some(_) } => {
                Err("effect id does not match the executing operation")
            }
            OperationState::Executing { effect_id: None } => {
                Err("read-only operation has no reserved effect id")
            }
            _ => Err("operation is not executing"),
        })
    }

    /// Atomically claim the only commit slot for this effect. Concurrent or
    /// repeated commits cannot both leave `Prepared`.
    pub(crate) fn begin_commit(
        &self,
        operation_id: OperationId,
        effect_id: EffectId,
    ) -> AgentResult<()> {
        self.transition(operation_id, |state| match state {
            OperationState::Prepared {
                effect_id: expected,
            } if expected == effect_id => Ok(OperationState::CommitStarted { effect_id }),
            OperationState::Prepared { .. } => Err("effect id does not match prepared operation"),
            _ => Err("operation has no commit-ready prepared effect"),
        })
    }

    pub(crate) fn finish_value(
        &self,
        operation_id: OperationId,
        argument_digest: agent_contracts::ArgumentDigest,
        generation: u64,
    ) -> AgentResult<()> {
        self.require_identity(operation_id, argument_digest, Some(generation))?;
        self.finish_terminal(
            operation_id,
            None,
            OperationTerminal::CompletedValue,
            |state| matches!(state, OperationState::Executing { .. }),
        )
    }

    pub(crate) fn finish_refused(
        &self,
        operation_id: OperationId,
        error: impl AsRef<str>,
    ) -> AgentResult<()> {
        self.finish_terminal(
            operation_id,
            None,
            OperationTerminal::Refused {
                error: bounded_text(error.as_ref(), MAX_OPERATION_DIAGNOSTIC_BYTES),
            },
            |state| matches!(state, OperationState::Accepted),
        )
    }

    pub(crate) fn finish_effect(
        &self,
        operation_id: OperationId,
        receipt: &EffectReceipt,
    ) -> AgentResult<()> {
        let terminal = match receipt {
            EffectReceipt::NotApplied { error } => OperationTerminal::NotApplied {
                error: bounded_text(error, MAX_OPERATION_DIAGNOSTIC_BYTES),
            },
            EffectReceipt::Applied {
                durability,
                evidence,
            } => OperationTerminal::Applied {
                durability: match durability {
                    EffectDurability::Durable => EffectDurability::Durable,
                    EffectDurability::DurabilityFailed(error) => {
                        EffectDurability::DurabilityFailed(bounded_text(
                            error,
                            MAX_OPERATION_DIAGNOSTIC_BYTES,
                        ))
                    }
                },
                evidence: evidence
                    .as_deref()
                    .map(|value| bounded_text(value, MAX_OPERATION_EVIDENCE_BYTES)),
            },
            EffectReceipt::Unknown { error } => OperationTerminal::OutcomeUnknown {
                error: bounded_text(error, MAX_OPERATION_DIAGNOSTIC_BYTES),
            },
        };
        terminal.validate().map_err(AgentError::InvalidRequest)?;
        let effect_id = match self.query(operation_id) {
            OperationQueryResult::Found { snapshot } => match snapshot.state {
                OperationState::CommitStarted { effect_id } => Some(effect_id),
                _ => None,
            },
            _ => None,
        };
        self.finish_terminal(operation_id, effect_id, terminal, |state| {
            matches!(state, OperationState::CommitStarted { .. })
        })
    }

    /// Install an idempotent cancellation terminal, even when cancellation
    /// wins the race with `accept`. The complete identity reservation makes
    /// a delayed accept observe a cancelled duplicate instead of dispatching.
    pub(crate) fn cancel(&self, identity: ToolOperationIdentity) -> AgentResult<()> {
        identity.validate().map_err(AgentError::InvalidRequest)?;
        let operation_id = identity.operation_id;
        let _transition = self.transition_guard()?;
        let next = {
            let registry = self.inner.lock().expect("operation registry poisoned");
            if let Some(record) = registry.records.get(&operation_id) {
                if record.identity != identity {
                    return Err(AgentError::InvalidRequest(format!(
                        "operation {operation_id} cancellation identity does not match admission"
                    )));
                }
                let effect_id = match record.state {
                    OperationState::Accepted => None,
                    OperationState::Executing { effect_id } => effect_id,
                    OperationState::Prepared { effect_id } => Some(effect_id),
                    OperationState::Terminal {
                        terminal: OperationTerminal::CancelledBeforeCommit,
                        ..
                    } => return Ok(()),
                    _ => {
                        return Err(AgentError::InvalidRequest(format!(
                            "operation {operation_id} cannot be cancelled from {:?}",
                            record.state
                        )));
                    }
                };
                OperationSnapshot {
                    identity: record.identity.clone(),
                    state: OperationState::Terminal {
                        effect_id,
                        terminal: OperationTerminal::CancelledBeforeCommit,
                    },
                }
            } else {
                if registry.seen.may_contain(operation_id) {
                    return Err(AgentError::InvalidRequest(format!(
                        "operation {operation_id} was already admitted or conservatively seen"
                    )));
                }
                if !has_capacity_or_evictable_terminal(&registry, self.capacity) {
                    return Err(AgentError::RecoveryRequired(format!(
                        "operation registry is full with {} unresolved operation(s)",
                        registry.records.len()
                    )));
                }
                OperationSnapshot {
                    identity,
                    state: OperationState::Terminal {
                        effect_id: None,
                        terminal: OperationTerminal::CancelledBeforeCommit,
                    },
                }
            }
        };
        self.append_snapshot(&next)?;
        let mut registry = self.inner.lock().expect("operation registry poisoned");
        evict_terminal_until_fit(&mut registry, self.capacity);
        registry.seen.insert(operation_id);
        registry.records.insert(operation_id, next);
        registry.leases.remove(&operation_id);
        registry.terminal_order.push_back(operation_id);
        Ok(())
    }

    /// Under one operation-transition guard, either observe an already
    /// settled operation or durably append both the authority-epoch fence and
    /// the exact cancellation terminal. This closes the query-then-cancel
    /// race without moving any scheduling decision into Core.
    pub(crate) fn cancel_and_persist_epoch(
        &self,
        identity: ToolOperationIdentity,
        from_epoch: u64,
        to_epoch: u64,
    ) -> AgentResult<OperationCancelTransition> {
        identity.validate().map_err(AgentError::InvalidRequest)?;
        if from_epoch == 0 || to_epoch != from_epoch.checked_add(1).unwrap_or(0) {
            return Err(AgentError::InvalidRequest(
                "operation cancellation requires one non-zero authority-epoch advance".into(),
            ));
        }
        let operation_id = identity.operation_id;
        let _transition = self.transition_guard()?;
        let next = {
            let registry = self.inner.lock().expect("operation registry poisoned");
            match registry.records.get(&operation_id) {
                Some(record) if record.identity != identity => {
                    return Err(AgentError::InvalidRequest(format!(
                        "operation {operation_id} cancellation identity does not match admission"
                    )));
                }
                Some(record)
                    if matches!(
                        record.state,
                        OperationState::CommitStarted { .. } | OperationState::Terminal { .. }
                    ) =>
                {
                    return Ok(OperationCancelTransition::AlreadySettled(
                        OperationQueryResult::Found {
                            snapshot: Box::new(record.clone()),
                        },
                    ));
                }
                Some(record) => {
                    let effect_id = match record.state {
                        OperationState::Accepted => None,
                        OperationState::Executing { effect_id } => effect_id,
                        OperationState::Prepared { effect_id } => Some(effect_id),
                        OperationState::CommitStarted { .. } | OperationState::Terminal { .. } => {
                            unreachable!("settled operation states returned immediately above")
                        }
                    };
                    OperationSnapshot {
                        identity: record.identity.clone(),
                        state: OperationState::Terminal {
                            effect_id,
                            terminal: OperationTerminal::CancelledBeforeCommit,
                        },
                    }
                }
                None if registry.seen.may_contain(operation_id) => {
                    return Ok(OperationCancelTransition::AlreadySettled(
                        OperationQueryResult::ExpiredOrPossiblySeen,
                    ));
                }
                None => {
                    if !has_capacity_or_evictable_terminal(&registry, self.capacity) {
                        return Err(AgentError::RecoveryRequired(format!(
                            "operation registry is full with {} unresolved operation(s)",
                            registry.records.len()
                        )));
                    }
                    OperationSnapshot {
                        identity,
                        state: OperationState::Terminal {
                            effect_id: None,
                            terminal: OperationTerminal::CancelledBeforeCommit,
                        },
                    }
                }
            }
        };

        self.append_transition(OperationJournalTransition::EpochAdvanced {
            from: from_epoch,
            to: to_epoch,
        })?;
        self.append_snapshot(&next)?;
        let mut registry = self.inner.lock().expect("operation registry poisoned");
        evict_terminal_until_fit(&mut registry, self.capacity);
        registry.seen.insert(operation_id);
        registry.records.insert(operation_id, next.clone());
        registry.leases.remove(&operation_id);
        registry.terminal_order.push_back(operation_id);
        Ok(OperationCancelTransition::Cancelled(
            OperationQueryResult::Found {
                snapshot: Box::new(next),
            },
        ))
    }

    pub(crate) fn abort_prepared(
        &self,
        operation_id: OperationId,
        effect_id: EffectId,
        argument_digest: agent_contracts::ArgumentDigest,
    ) -> AgentResult<()> {
        let OperationQueryResult::Found { snapshot } = self.query(operation_id) else {
            return Err(AgentError::InvalidRequest(format!(
                "unknown or expired operation {operation_id}"
            )));
        };
        if snapshot.identity.argument_digest != argument_digest {
            return Err(AgentError::InvalidRequest(format!(
                "operation {operation_id} prepared-effect identity does not match"
            )));
        }
        match snapshot.state {
            OperationState::Prepared {
                effect_id: expected,
            } if expected == effect_id => self.cancel(snapshot.identity),
            OperationState::Terminal {
                effect_id: Some(expected),
                terminal: OperationTerminal::CancelledBeforeCommit,
            } if expected == effect_id => Ok(()),
            _ => Err(AgentError::InvalidRequest(format!(
                "operation {operation_id} prepared-effect identity does not match"
            ))),
        }
    }

    pub(crate) fn query(&self, operation_id: OperationId) -> OperationQueryResult {
        let registry = self.inner.lock().expect("operation registry poisoned");
        match registry.records.get(&operation_id) {
            Some(snapshot) => OperationQueryResult::Found {
                snapshot: Box::new(snapshot.clone()),
            },
            None if registry.seen.may_contain(operation_id) => {
                OperationQueryResult::ExpiredOrPossiblySeen
            }
            None => OperationQueryResult::NotFound,
        }
    }

    fn transition(
        &self,
        operation_id: OperationId,
        move_state: impl FnOnce(OperationState) -> Result<OperationState, &'static str>,
    ) -> AgentResult<()> {
        let _transition = self.transition_guard()?;
        let next = {
            let registry = self.inner.lock().expect("operation registry poisoned");
            let record = registry.records.get(&operation_id).ok_or_else(|| {
                AgentError::InvalidRequest(format!("unknown operation {operation_id}"))
            })?;
            let next_state = move_state(record.state.clone()).map_err(|message| {
                AgentError::InvalidRequest(format!("operation {operation_id}: {message}"))
            })?;
            OperationSnapshot {
                identity: record.identity.clone(),
                state: next_state,
            }
        };
        self.append_snapshot(&next)?;
        self.inner
            .lock()
            .expect("operation registry poisoned")
            .records
            .insert(operation_id, next);
        Ok(())
    }

    fn require_identity(
        &self,
        operation_id: OperationId,
        argument_digest: agent_contracts::ArgumentDigest,
        generation: Option<u64>,
    ) -> AgentResult<()> {
        let registry = self.inner.lock().expect("operation registry poisoned");
        let record = registry.records.get(&operation_id).ok_or_else(|| {
            AgentError::InvalidRequest(format!("unknown operation {operation_id}"))
        })?;
        if record.identity.argument_digest != argument_digest
            || generation.is_some_and(|value| record.identity.generation != value)
        {
            return Err(AgentError::InvalidRequest(format!(
                "operation {operation_id} identity does not match admission"
            )));
        }
        Ok(())
    }

    fn finish_terminal(
        &self,
        operation_id: OperationId,
        effect_id: Option<EffectId>,
        terminal: OperationTerminal,
        allowed: impl FnOnce(&OperationState) -> bool,
    ) -> AgentResult<()> {
        let _transition = self.transition_guard()?;
        let next = {
            let registry = self.inner.lock().expect("operation registry poisoned");
            let record = registry.records.get(&operation_id).ok_or_else(|| {
                AgentError::InvalidRequest(format!("unknown operation {operation_id}"))
            })?;
            if !allowed(&record.state) {
                return Err(AgentError::InvalidRequest(format!(
                    "operation {operation_id} cannot become terminal from {:?}",
                    record.state
                )));
            }
            OperationSnapshot {
                identity: record.identity.clone(),
                state: OperationState::Terminal {
                    effect_id,
                    terminal,
                },
            }
        };
        self.append_snapshot(&next)?;
        let mut registry = self.inner.lock().expect("operation registry poisoned");
        registry.records.insert(operation_id, next);
        registry.leases.remove(&operation_id);
        registry.terminal_order.push_back(operation_id);
        Ok(())
    }

    fn transition_guard(&self) -> AgentResult<std::sync::MutexGuard<'_, ()>> {
        let guard = self
            .transition_gate
            .lock()
            .expect("operation transition gate poisoned");
        self.ensure_healthy()?;
        Ok(guard)
    }

    fn ensure_healthy(&self) -> AgentResult<()> {
        if let Some(error) = self
            .inner
            .lock()
            .expect("operation registry poisoned")
            .recovery_required
            .clone()
        {
            Err(AgentError::RecoveryRequired(error))
        } else {
            Ok(())
        }
    }

    fn append_snapshot(&self, snapshot: &OperationSnapshot) -> AgentResult<()> {
        self.append_transition(OperationJournalTransition::OperationUpsert {
            snapshot: Box::new(snapshot.clone()),
        })
    }

    fn append_transition(&self, transition: OperationJournalTransition) -> AgentResult<()> {
        let Some(journal) = &self.journal else {
            return Ok(());
        };
        if let Err(error) = journal.append_and_sync(&transition) {
            let message = format!("operation authority journal failed: {error}");
            self.inner
                .lock()
                .expect("operation registry poisoned")
                .recovery_required = Some(message.clone());
            return Err(AgentError::RecoveryRequired(message));
        }
        Ok(())
    }
}

fn bounded_text(value: &str, max_bytes: usize) -> String {
    let sanitized: String = value
        .chars()
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .collect();
    let mut end = sanitized.len().min(max_bytes);
    while end > 0 && !sanitized.is_char_boundary(end) {
        end -= 1;
    }
    let clipped = sanitized[..end].trim();
    if clipped.is_empty() {
        "unspecified".into()
    } else {
        clipped.into()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum OperationAdmission {
    Accepted,
    Duplicate(Box<OperationSnapshot>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum OperationCancelTransition {
    Cancelled(OperationQueryResult),
    AlreadySettled(OperationQueryResult),
}

fn evict_terminal_until_fit(state: &mut RegistryState, capacity: usize) {
    while state.records.len() >= capacity {
        let Some(operation_id) = state.terminal_order.pop_front() else {
            break;
        };
        if state
            .records
            .get(&operation_id)
            .is_some_and(|snapshot| matches!(snapshot.state, OperationState::Terminal { .. }))
        {
            state.records.remove(&operation_id);
            state.leases.remove(&operation_id);
        }
    }
}

fn has_capacity_or_evictable_terminal(state: &RegistryState, capacity: usize) -> bool {
    state.records.len() < capacity
        || state.terminal_order.iter().any(|operation_id| {
            state
                .records
                .get(operation_id)
                .is_some_and(|snapshot| matches!(snapshot.state, OperationState::Terminal { .. }))
        })
}

fn seen_filter_indexes(operation_id: OperationId) -> [usize; 4] {
    const SEEDS: [u64; 4] = [
        0xcbf2_9ce4_8422_2325,
        0x9e37_79b9_7f4a_7c15,
        0xd6e8_feb8_6659_fd93,
        0xa076_1d64_78bd_642f,
    ];
    let bytes = operation_id.0.as_bytes();
    SEEDS.map(|seed| {
        let mut hash = seed;
        for byte in bytes {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
        (hash as usize) & (SEEN_OPERATION_FILTER_BITS - 1)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_contracts::{
        ArgumentDigest, OPERATION_JOURNAL_VERSION, OperationJournalRecord, RunId, TurnId,
    };
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

    #[derive(Default)]
    struct RecordingJournal {
        transitions: Mutex<Vec<OperationJournalTransition>>,
        sequence: AtomicU64,
        fail: AtomicBool,
    }

    impl OperationJournal for RecordingJournal {
        fn append_and_sync(
            &self,
            transition: &OperationJournalTransition,
        ) -> AgentResult<OperationJournalRecord> {
            if self.fail.load(Ordering::Acquire) {
                return Err(AgentError::Storage("injected journal failure".into()));
            }
            let seq = self.sequence.fetch_add(1, Ordering::AcqRel) + 1;
            self.transitions.lock().unwrap().push(transition.clone());
            Ok(OperationJournalRecord {
                version: OPERATION_JOURNAL_VERSION,
                seq,
                transition: transition.clone(),
            })
        }

        fn recover(&self) -> AgentResult<OperationJournalRecovery> {
            Ok(OperationJournalRecovery::default())
        }
    }

    fn identity(operation_id: OperationId, value: u8) -> ToolOperationIdentity {
        ToolOperationIdentity {
            run_id: RunId::new(),
            task_id: None,
            turn_id: TurnId::new(),
            scope_id: None,
            operation_id,
            generation: 1,
            call_id: "call-1".into(),
            tool_name: "fs.read".into(),
            argument_digest: ArgumentDigest::sha256_bytes(&[value]),
        }
    }

    #[test]
    fn duplicate_identity_is_idempotent_but_conflict_is_rejected() {
        let registry = OperationRegistry::new(2);
        let operation_id = OperationId::new();
        let first = identity(operation_id, 1);
        assert_eq!(
            registry.accept(first.clone()).unwrap(),
            OperationAdmission::Accepted
        );
        assert!(matches!(
            registry.accept(first).unwrap(),
            OperationAdmission::Duplicate(_)
        ));
        assert!(matches!(
            registry.accept(identity(operation_id, 2)),
            Err(AgentError::InvalidRequest(_))
        ));
    }

    #[test]
    fn commit_slot_is_claimed_once_and_terminal_is_monotonic() {
        let registry = OperationRegistry::new(2);
        let operation_id = OperationId::new();
        registry.accept(identity(operation_id, 1)).unwrap();
        let effect_id = EffectId::new();
        registry
            .mark_executing(operation_id, Some(effect_id))
            .unwrap();
        registry.mark_prepared(operation_id, effect_id).unwrap();
        registry.begin_commit(operation_id, effect_id).unwrap();
        assert!(registry.begin_commit(operation_id, effect_id).is_err());
        registry
            .finish_effect(
                operation_id,
                &EffectReceipt::Applied {
                    durability: agent_contracts::EffectDurability::Durable,
                    evidence: None,
                },
            )
            .unwrap();
        assert!(registry.cancel(identity(operation_id, 1)).is_err());
    }

    #[test]
    fn executing_wal_reserves_the_exact_effect_identity_before_preparation() {
        let journal = Arc::new(RecordingJournal::default());
        let registry = OperationRegistry::recover(
            2,
            Some(journal.clone()),
            OperationJournalRecovery::default(),
        )
        .unwrap();
        let operation_id = OperationId::new();
        registry.accept(identity(operation_id, 1)).unwrap();
        let effect_id = EffectId::new();
        registry
            .mark_executing(operation_id, Some(effect_id))
            .unwrap();

        let transitions = journal.transitions.lock().unwrap();
        assert!(matches!(
            transitions.last(),
            Some(OperationJournalTransition::OperationUpsert { snapshot })
                if snapshot.state == OperationState::Executing {
                    effect_id: Some(effect_id),
                }
        ));
        drop(transitions);

        assert!(
            registry
                .mark_prepared(operation_id, EffectId::new())
                .is_err()
        );
        assert!(matches!(
            registry.query(operation_id),
            OperationQueryResult::Found { snapshot }
                if snapshot.state == OperationState::Executing {
                    effect_id: Some(effect_id),
                }
        ));
        registry.mark_prepared(operation_id, effect_id).unwrap();
    }

    #[test]
    fn side_effecting_value_terminal_does_not_claim_an_applied_effect() {
        let registry = OperationRegistry::new(2);
        let operation_id = OperationId::new();
        registry.accept(identity(operation_id, 1)).unwrap();
        registry
            .mark_executing(operation_id, Some(EffectId::new()))
            .unwrap();
        registry
            .finish_value(operation_id, ArgumentDigest::sha256_bytes(&[1]), 1)
            .unwrap();
        assert!(matches!(
            registry.query(operation_id),
            OperationQueryResult::Found { snapshot }
                if snapshot.state == OperationState::Terminal {
                    effect_id: None,
                    terminal: OperationTerminal::CompletedValue,
                }
        ));
    }

    #[test]
    fn cancellation_reservation_wins_a_delayed_accept() {
        let registry = OperationRegistry::new(2);
        let operation_id = OperationId::new();
        let cancelled = identity(operation_id, 1);

        registry.cancel(cancelled.clone()).unwrap();
        let OperationAdmission::Duplicate(snapshot) = registry.accept(cancelled.clone()).unwrap()
        else {
            panic!("the delayed accept must observe the cancellation reservation")
        };
        assert!(matches!(
            snapshot.as_ref(),
            OperationSnapshot {
                state: OperationState::Terminal {
                    effect_id: None,
                    terminal: OperationTerminal::CancelledBeforeCommit,
                },
                ..
            }
        ));
        assert!(matches!(
            registry.accept(identity(operation_id, 2)),
            Err(AgentError::InvalidRequest(_))
        ));
    }

    #[test]
    fn atomic_cancellation_persists_epoch_and_terminal_before_publication() {
        let journal = Arc::new(RecordingJournal::default());
        let registry = OperationRegistry::recover(
            2,
            Some(journal.clone()),
            OperationJournalRecovery::default(),
        )
        .unwrap();
        let operation_id = OperationId::new();
        let admitted = identity(operation_id, 1);
        registry.accept(admitted.clone()).unwrap();
        registry.mark_executing(operation_id, None).unwrap();

        let transition = registry
            .cancel_and_persist_epoch(admitted.clone(), 1, 2)
            .unwrap();
        assert!(matches!(
            transition,
            OperationCancelTransition::Cancelled(OperationQueryResult::Found { ref snapshot })
                if snapshot.identity == admitted
                    && matches!(snapshot.state, OperationState::Terminal {
                        effect_id: None,
                        terminal: OperationTerminal::CancelledBeforeCommit,
                    })
        ));
        let transitions = journal.transitions.lock().unwrap();
        assert!(matches!(
            transitions.as_slice(),
            [
                OperationJournalTransition::OperationUpsert { .. },
                OperationJournalTransition::OperationUpsert { .. },
                OperationJournalTransition::EpochAdvanced { from: 1, to: 2 },
                OperationJournalTransition::OperationUpsert { snapshot },
            ] if matches!(snapshot.state, OperationState::Terminal {
                terminal: OperationTerminal::CancelledBeforeCommit,
                ..
            })
        ));
    }

    #[test]
    fn atomic_cancellation_returns_existing_terminal_without_advancing_epoch() {
        let journal = Arc::new(RecordingJournal::default());
        let registry = OperationRegistry::recover(
            2,
            Some(journal.clone()),
            OperationJournalRecovery::default(),
        )
        .unwrap();
        let operation_id = OperationId::new();
        let admitted = identity(operation_id, 1);
        registry.accept(admitted.clone()).unwrap();
        registry
            .finish_refused(operation_id, "permission denied")
            .unwrap();
        let before = journal.transitions.lock().unwrap().len();

        let transition = registry.cancel_and_persist_epoch(admitted, 1, 2).unwrap();
        assert!(matches!(
            transition,
            OperationCancelTransition::AlreadySettled(OperationQueryResult::Found {
                snapshot
            }) if matches!(snapshot.state, OperationState::Terminal {
                effect_id: None,
                terminal: OperationTerminal::Refused { .. },
            })
        ));
        assert_eq!(journal.transitions.lock().unwrap().len(), before);
    }

    #[test]
    fn journal_failure_does_not_publish_state_and_is_sticky() {
        let journal = Arc::new(RecordingJournal::default());
        let registry = OperationRegistry::recover(
            2,
            Some(journal.clone()),
            OperationJournalRecovery::default(),
        )
        .unwrap();
        let operation_id = OperationId::new();
        registry.accept(identity(operation_id, 1)).unwrap();
        assert!(matches!(
            registry.query(operation_id),
            OperationQueryResult::Found { snapshot }
                if snapshot.state == OperationState::Accepted
        ));

        journal.fail.store(true, Ordering::Release);
        assert!(matches!(
            registry.mark_executing(operation_id, None),
            Err(AgentError::RecoveryRequired(_))
        ));
        assert!(matches!(
            registry.query(operation_id),
            OperationQueryResult::Found { snapshot }
                if snapshot.state == OperationState::Accepted
        ));

        journal.fail.store(false, Ordering::Release);
        assert!(matches!(
            registry.mark_executing(operation_id, None),
            Err(AgentError::RecoveryRequired(_))
        ));
        assert_eq!(journal.transitions.lock().unwrap().len(), 1);
    }

    #[test]
    fn failed_admission_does_not_evict_a_published_terminal() {
        let journal = Arc::new(RecordingJournal::default());
        let registry = OperationRegistry::recover(
            1,
            Some(journal.clone()),
            OperationJournalRecovery::default(),
        )
        .unwrap();
        let first = OperationId::new();
        registry.accept(identity(first, 1)).unwrap();
        registry.mark_executing(first, None).unwrap();
        registry
            .finish_value(first, ArgumentDigest::sha256_bytes(&[1]), 1)
            .unwrap();

        journal.fail.store(true, Ordering::Release);
        let second = OperationId::new();
        assert!(matches!(
            registry.accept(identity(second, 2)),
            Err(AgentError::RecoveryRequired(_))
        ));
        assert!(matches!(
            registry.query(first),
            OperationQueryResult::Found { snapshot }
                if matches!(snapshot.state, OperationState::Terminal { .. })
        ));
        assert_eq!(registry.query(second), OperationQueryResult::NotFound);
    }

    #[test]
    fn capacity_evicts_terminal_but_never_unresolved() {
        let registry = OperationRegistry::new(1);
        let first = OperationId::new();
        registry.accept(identity(first, 1)).unwrap();
        assert!(matches!(
            registry.accept(identity(OperationId::new(), 2)),
            Err(AgentError::RecoveryRequired(_))
        ));
        registry.mark_executing(first, None).unwrap();
        registry
            .finish_value(first, ArgumentDigest::sha256_bytes(&[1]), 1)
            .unwrap();
        let second = OperationId::new();
        assert!(matches!(
            registry.accept(identity(second, 3)).unwrap(),
            OperationAdmission::Accepted
        ));
        assert_eq!(
            registry.query(first),
            OperationQueryResult::ExpiredOrPossiblySeen
        );
        assert!(matches!(
            registry.accept(identity(first, 1)),
            Err(AgentError::InvalidRequest(_))
        ));
    }
}
