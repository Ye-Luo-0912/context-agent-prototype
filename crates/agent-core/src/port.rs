//! Narrow in-process authority port from the Platform actor to trusted Core.
//!
//! This is deliberately not a wire protocol and does not own task/turn
//! scheduling.  It hides Core's component authorities behind operation-shaped
//! calls, so Runtime cannot obtain an approval or effect authority object and
//! bypass the request checks. Core owns a monotonic authority
//! epoch and an optional synchronous operation journal that persists identity,
//! state and epoch transitions across restart. A composition-provided effect
//! reconciler may prove managed outcomes after a crash; unknown outcomes stay
//! fenced rather than being replayed.

use std::sync::Arc;

use agent_contracts::{
    AgentError, AgentResult, ApprovalGate, ArgumentDigest, AuthorityCheckpointMarker,
    AuthorityLease, AuthorityRecoveryStatus, CancellationToken, ContextConsumptionAck,
    ContextEngine, Effect, EffectId, EffectReceipt, EffectReconciler, EngineQuery, EventJournal,
    OperationId, OperationQueryResult, OperationSnapshot, RunId, RuntimeEvent,
    RuntimeEventEnvelope, TaskId, ToolCall, ToolDispatcher, ToolOperationIdentity, ToolOutcome,
    ToolOutput, ToolSpec, ToolSurfaceSnapshot, TurnId,
};
use async_trait::async_trait;
use tokio::sync::broadcast;

use crate::{ApprovalVerdict, CoreAuthorityConfig, kernel::CoreAuthority};

/// Construct one trusted Core instance and expose only its narrow port.
/// Composition retains no concrete facade or component-authority handles.
pub fn build_core_port(
    config: CoreAuthorityConfig,
    context: Arc<dyn ContextEngine>,
    tools: Arc<dyn ToolDispatcher>,
    approval: Arc<dyn ApprovalGate>,
    journal: Option<Arc<dyn EventJournal>>,
) -> Arc<dyn CorePort> {
    try_build_core_port(config, context, tools, approval, journal, None, None)
        .expect("in-memory Core construction cannot fail")
}

/// Fallible trusted-composition constructor used when Core authority state
/// is recovered from a durable operation journal.
pub fn try_build_core_port(
    config: CoreAuthorityConfig,
    context: Arc<dyn ContextEngine>,
    tools: Arc<dyn ToolDispatcher>,
    approval: Arc<dyn ApprovalGate>,
    journal: Option<Arc<dyn EventJournal>>,
    operation_journal: Option<Arc<dyn agent_contracts::OperationJournal>>,
    effect_reconciler: Option<Arc<dyn EffectReconciler>>,
) -> AgentResult<Arc<dyn CorePort>> {
    Ok(Arc::new(CoreAuthority::try_new(
        config,
        context,
        tools,
        approval,
        journal,
        operation_journal,
        effect_reconciler,
    )?))
}

/// A request to commit one already-prepared effect.
///
/// `turn_id` and `operation_id` are carried for audit/recovery continuity.
/// Core independently validates the current authority epoch, exact admitted
/// operation identity and its issued lease. Effect-specific crash
/// reconciliation remains PLAT-03 work.
pub struct EffectCommitRequest {
    pub run_id: RunId,
    pub turn_id: TurnId,
    pub operation_id: OperationId,
    pub effect_id: EffectId,
    pub argument_digest: ArgumentDigest,
    pub generation: u64,
    pub lease: Option<AuthorityLease>,
    pub effect: Box<dyn Effect>,
}

/// A request to release a prepared effect without applying it.
///
/// Rollback is cleanup, so Core always attempts it even when request identity
/// is invalid.  The returned error still exposes the rejected authority
/// identity to the caller.
pub struct EffectRollbackRequest {
    pub run_id: RunId,
    pub turn_id: TurnId,
    pub operation_id: OperationId,
    pub effect_id: Option<EffectId>,
    pub argument_digest: ArgumentDigest,
    pub generation: u64,
    pub lease: Option<AuthorityLease>,
    pub effect: Box<dyn Effect>,
    pub reason: String,
}

/// Core's disposition for one commit request. Authority rejection stays
/// distinct from an effect's own receipt so Runtime never parses strings to
/// explain an expired, missing, or foreign authorization.
#[derive(Debug, Clone)]
pub enum EffectCommitDisposition {
    Receipt(EffectReceipt),
    Rejected(EffectCommitRejection),
    /// The effect receipt is the best-known world-state truth, but Core
    /// failed to record the matching terminal authority state. Runtime must
    /// preserve the receipt and enter recovery rather than treating a
    /// truthful `NotApplied` receipt as ordinary success.
    AuthorityRecordFailed {
        receipt: EffectReceipt,
        error: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EffectCommitRejection {
    ForeignRun,
    StaleEpoch,
    MissingLease,
    InvalidLease,
    InvalidOperation,
}

/// Atomic Core result for Runtime's exact-current-operation cancellation.
/// `Cancelled` installs the new authority epoch and cancellation terminal in
/// one Core critical section. `AlreadySettled` means Core truth won the race;
/// Runtime must return it without advancing its scheduling mirror.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OperationCancelDisposition {
    Cancelled {
        effective_epoch: u64,
        result: OperationQueryResult,
    },
    AlreadySettled(OperationQueryResult),
}

pub struct CoreToolExecution {
    pub outcome: ToolOutcome,
    pub lease: Option<AuthorityLease>,
    pub effect_id: Option<EffectId>,
    pub argument_digest: ArgumentDigest,
    /// A dispatched, non-effect result remains `Executing` until Runtime has
    /// passed its current-turn fence and admitted the result/directive. Core
    /// refusals are already terminal and prepared effects terminate through
    /// commit/rollback instead.
    pub value_completion_pending: bool,
}

/// One-shot proof that Core durably admitted exactly one tool operation.
///
/// The fields are private, and this type intentionally implements neither
/// `Clone` nor serialization. It is an in-process linear capability, not a
/// bearer token or wire credential. It cannot execute a tool: Core must first
/// consume it while publishing the admitted identity and `ToolStarted`, then
/// return a [`PublishedToolPermit`].
///
/// The compiler enforces single consumption:
///
/// ```compile_fail
/// fn duplicate(permit: agent_core::AdmittedToolPermit) {
///     let _second = permit.clone();
/// }
/// ```
///
/// External callers also cannot forge a permit from an operation identity:
///
/// ```compile_fail
/// fn forge(identity: agent_contracts::ToolOperationIdentity) -> agent_core::AdmittedToolPermit {
///     agent_core::AdmittedToolPermit { identity }
/// }
/// ```
///
/// An admitted permit is deliberately not executable; publication changes
/// its type:
///
/// ```compile_fail
/// fn skip_publication(
///     permit: agent_core::AdmittedToolPermit,
/// ) -> agent_core::PublishedToolPermit {
///     permit
/// }
/// ```
#[derive(Debug)]
pub struct AdmittedToolPermit {
    pub(crate) identity: ToolOperationIdentity,
}

/// One-shot proof that the WAL-backed identity and tool start were published.
///
/// Only Core can upgrade an [`AdmittedToolPermit`] into this type, and only
/// after both lifecycle events succeed. It intentionally implements neither
/// `Clone` nor serialization, so dispatch cannot be duplicated or reached by
/// a caller that skipped publication.
#[derive(Debug)]
pub struct PublishedToolPermit {
    pub(crate) identity: ToolOperationIdentity,
}

/// Result of Core's WAL-first operation admission.
///
/// An exact retry observes `AlreadyKnown` and receives no permit, so it can
/// report/query existing truth but can never dispatch the operation again.
#[derive(Debug)]
pub enum ToolOperationAdmission {
    Accepted {
        snapshot: Box<OperationSnapshot>,
        permit: AdmittedToolPermit,
    },
    AlreadyKnown {
        snapshot: Box<OperationSnapshot>,
    },
}

/// The object-safe authority surface available to the sole RuntimeActor.
///
/// Context/model/tool scheduling remains in `agent-runtime::RuntimeServices`.
/// The live event sender and sequence are a temporary trusted-observability
/// capability used only for provider streaming; they grant no operation
/// authority and should eventually become a narrower live-event sink.
#[async_trait]
pub trait CorePort: Send + Sync {
    fn run_id(&self) -> RunId;

    fn current_authority_epoch(&self) -> u64;

    fn recovery_status(&self) -> AuthorityRecoveryStatus;

    /// Read-only reference to the current durable Core authority truth.
    /// `None` means this composition has no persistent operation journal.
    fn authority_checkpoint_marker(&self) -> AgentResult<Option<AuthorityCheckpointMarker>>;

    /// Prove that a marker names an ancestor of the current durable Core
    /// authority. This comparison never restores or rewinds Core state.
    fn validate_authority_checkpoint_marker(
        &self,
        expected: &AuthorityCheckpointMarker,
    ) -> AgentResult<()>;

    /// 折叠权威 WAL。无持久 journal 时返回 `None`。压缩不改 in-memory
    /// registry，只换代际存储。
    fn compact_authority_journal(&self) -> AgentResult<Option<AuthorityCheckpointMarker>>;

    fn advance_authority_epoch(&self, expected: u64) -> AgentResult<u64>;

    fn event_sender(&self) -> broadcast::Sender<RuntimeEventEnvelope>;

    /// Read-only durable journal cursor for live-only events. A live sink
    /// repeats this cursor and cannot consume a recoverable sequence number.
    fn event_sequence(&self) -> u64;

    async fn start(&self) -> AgentResult<()>;

    async fn stop(&self) -> AgentResult<()>;

    async fn emit_event(&self, event: RuntimeEvent) -> AgentResult<()>;

    async fn emit_event_durable(&self, event: RuntimeEvent) -> AgentResult<()>;

    async fn emit_warning(&self, message: String) -> AgentResult<()>;

    async fn authorize(
        &self,
        call: &ToolCall,
        spec: &ToolSpec,
        cancel: &CancellationToken,
    ) -> ApprovalVerdict;

    async fn acknowledge_context_consumption(&self, ack: ContextConsumptionAck) -> AgentResult<()>;

    async fn resolve_engine_query(&self, output: ToolOutput, query: EngineQuery) -> ToolOutput;

    /// Validate and durably admit a tool operation before Runtime publishes
    /// acceptance. Only a newly appended `Accepted` record carries the
    /// one-shot permit required by `publish_tool_operation`.
    fn admit_tool_operation(
        &self,
        identity: ToolOperationIdentity,
        call: &ToolCall,
        generation: u64,
    ) -> AgentResult<ToolOperationAdmission>;

    /// Consume an admitted permit, publish the exact WAL-backed identity and
    /// tool-start lifecycle events, and return the only permit executable by
    /// `execute_published_tool`.
    async fn publish_tool_operation(
        &self,
        permit: AdmittedToolPermit,
        call: &ToolCall,
    ) -> AgentResult<PublishedToolPermit>;

    /// Consume a Core-issued, publication-backed permit and execute that
    /// exact call.
    async fn execute_published_tool(
        &self,
        permit: PublishedToolPermit,
        call: ToolCall,
        cancel: CancellationToken,
        surface: &ToolSurfaceSnapshot,
    ) -> CoreToolExecution;

    fn query_operation(&self, operation_id: OperationId) -> OperationQueryResult;

    fn finish_value_operation(
        &self,
        operation_id: OperationId,
        argument_digest: ArgumentDigest,
        generation: u64,
    ) -> AgentResult<()>;

    fn cancel_operation(&self, identity: ToolOperationIdentity) -> AgentResult<()>;

    fn cancel_operation_and_advance(
        &self,
        identity: ToolOperationIdentity,
        expected_epoch: u64,
    ) -> AgentResult<OperationCancelDisposition>;

    async fn commit_effect(&self, request: EffectCommitRequest) -> EffectCommitDisposition;

    async fn rollback_effect(&self, request: EffectRollbackRequest) -> AgentResult<()>;

    async fn emit_diagnostics(&self) -> AgentResult<()>;

    async fn checkpoint(&self) -> AgentResult<serde_json::Value>;

    async fn restore(
        &self,
        data: serde_json::Value,
        expected_task_id: Option<TaskId>,
    ) -> AgentResult<()>;
}

#[async_trait]
impl CorePort for CoreAuthority {
    fn run_id(&self) -> RunId {
        CoreAuthority::run_id(self)
    }

    fn current_authority_epoch(&self) -> u64 {
        CoreAuthority::current_authority_epoch(self)
    }

    fn recovery_status(&self) -> AuthorityRecoveryStatus {
        CoreAuthority::recovery_status(self)
    }

    fn authority_checkpoint_marker(&self) -> AgentResult<Option<AuthorityCheckpointMarker>> {
        CoreAuthority::authority_checkpoint_marker(self)
    }

    fn validate_authority_checkpoint_marker(
        &self,
        expected: &AuthorityCheckpointMarker,
    ) -> AgentResult<()> {
        CoreAuthority::validate_authority_checkpoint_marker(self, expected)
    }

    fn compact_authority_journal(&self) -> AgentResult<Option<AuthorityCheckpointMarker>> {
        CoreAuthority::compact_authority_journal(self)
    }

    fn advance_authority_epoch(&self, expected: u64) -> AgentResult<u64> {
        CoreAuthority::advance_authority_epoch(self, expected)
    }

    fn event_sender(&self) -> broadcast::Sender<RuntimeEventEnvelope> {
        CoreAuthority::event_sender(self)
    }

    fn event_sequence(&self) -> u64 {
        CoreAuthority::event_sequence(self)
    }

    async fn start(&self) -> AgentResult<()> {
        CoreAuthority::start(self).await
    }

    async fn stop(&self) -> AgentResult<()> {
        CoreAuthority::stop(self).await
    }

    async fn emit_event(&self, event: RuntimeEvent) -> AgentResult<()> {
        CoreAuthority::emit_event(self, event).await
    }

    async fn emit_event_durable(&self, event: RuntimeEvent) -> AgentResult<()> {
        CoreAuthority::emit_event_durable(self, event).await
    }

    async fn emit_warning(&self, message: String) -> AgentResult<()> {
        CoreAuthority::emit_warning(self, message).await
    }

    async fn authorize(
        &self,
        call: &ToolCall,
        spec: &ToolSpec,
        cancel: &CancellationToken,
    ) -> ApprovalVerdict {
        if let Err(error) = self.ensure_mutation_allowed() {
            return ApprovalVerdict::Failed(error.to_string());
        }
        self.approval().authorize(call, spec, cancel).await
    }

    async fn acknowledge_context_consumption(&self, ack: ContextConsumptionAck) -> AgentResult<()> {
        CoreAuthority::acknowledge_context_consumption(self, ack).await
    }

    async fn resolve_engine_query(&self, output: ToolOutput, query: EngineQuery) -> ToolOutput {
        CoreAuthority::resolve_engine_query(self, output, query).await
    }

    fn admit_tool_operation(
        &self,
        identity: ToolOperationIdentity,
        call: &ToolCall,
        generation: u64,
    ) -> AgentResult<ToolOperationAdmission> {
        CoreAuthority::admit_tool_operation(self, identity, call, generation)
    }

    async fn publish_tool_operation(
        &self,
        permit: AdmittedToolPermit,
        call: &ToolCall,
    ) -> AgentResult<PublishedToolPermit> {
        CoreAuthority::publish_tool_operation(self, permit, call).await
    }

    async fn execute_published_tool(
        &self,
        permit: PublishedToolPermit,
        call: ToolCall,
        cancel: CancellationToken,
        surface: &ToolSurfaceSnapshot,
    ) -> CoreToolExecution {
        CoreAuthority::execute_published_tool(self, permit, call, cancel, surface).await
    }

    fn query_operation(&self, operation_id: OperationId) -> OperationQueryResult {
        CoreAuthority::query_operation(self, operation_id)
    }

    fn finish_value_operation(
        &self,
        operation_id: OperationId,
        argument_digest: ArgumentDigest,
        generation: u64,
    ) -> AgentResult<()> {
        CoreAuthority::finish_value_operation_if_current(
            self,
            generation,
            operation_id,
            argument_digest,
        )
    }

    fn cancel_operation(&self, identity: ToolOperationIdentity) -> AgentResult<()> {
        CoreAuthority::cancel_operation(self, identity)
    }

    fn cancel_operation_and_advance(
        &self,
        identity: ToolOperationIdentity,
        expected_epoch: u64,
    ) -> AgentResult<OperationCancelDisposition> {
        CoreAuthority::cancel_operation_and_advance(self, identity, expected_epoch)
    }

    async fn commit_effect(&self, request: EffectCommitRequest) -> EffectCommitDisposition {
        let EffectCommitRequest {
            run_id,
            turn_id,
            operation_id,
            effect_id,
            argument_digest,
            generation,
            lease,
            effect,
        } = request;
        if let Err(error) = self.ensure_mutation_allowed() {
            effect
                .rollback(&format!(
                    "effect commit blocked by Core recovery fence (turn {turn_id}, operation {operation_id}, generation {generation}): {error}"
                ))
                .await;
            return EffectCommitDisposition::AuthorityRecordFailed {
                receipt: EffectReceipt::NotApplied {
                    error: "effect commit was blocked by the Core recovery fence".into(),
                },
                error: error.to_string(),
            };
        }
        let refusal = if run_id != self.run_id() {
            Some(EffectCommitRejection::ForeignRun)
        } else {
            match lease.as_ref() {
                None => Some(EffectCommitRejection::MissingLease),
                Some(lease)
                    if lease.decision != agent_contracts::ApprovalDecision::Allow
                        || !self.issued_lease_matches(operation_id, lease)
                        || !lease.valid_at(now_ms(), generation, operation_id, argument_digest) =>
                {
                    Some(EffectCommitRejection::InvalidLease)
                }
                Some(_) => None,
            }
        };

        if let Some(rejection) = refusal {
            effect
                .rollback(&format!(
                    "effect commit rejected ({rejection:?}; turn {turn_id}, operation {operation_id}, generation {generation})"
                ))
                .await;
            if let Err(error) =
                self.abort_prepared_operation(operation_id, effect_id, argument_digest)
            {
                tracing::debug!(
                    %error,
                    %operation_id,
                    "rejected effect was not a matching prepared registry entry"
                );
            }
            EffectCommitDisposition::Rejected(rejection)
        } else {
            if let Err(error) = self.begin_operation_commit_if_current(
                generation,
                operation_id,
                effect_id,
                argument_digest,
            ) {
                effect
                    .rollback(&format!("operation registry rejected commit: {error}"))
                    .await;
                let rejection = if error.to_string().contains("is stale; current Core epoch") {
                    EffectCommitRejection::StaleEpoch
                } else {
                    EffectCommitRejection::InvalidOperation
                };
                if rejection == EffectCommitRejection::StaleEpoch
                    && let Err(cleanup_error) =
                        self.abort_prepared_operation(operation_id, effect_id, argument_digest)
                {
                    return EffectCommitDisposition::AuthorityRecordFailed {
                        receipt: EffectReceipt::NotApplied {
                            error: format!(
                                "effect commit rejected by stale authority epoch: {error}"
                            ),
                        },
                        error: format!(
                            "stale prepared operation could not be terminalized: {cleanup_error}"
                        ),
                    };
                }
                return EffectCommitDisposition::Rejected(rejection);
            }
            let receipt = self.effect().commit(effect).await;
            if let Err(error) = self.finish_operation_effect(operation_id, &receipt) {
                tracing::error!(%error, %operation_id, "operation terminal record failed");
                return EffectCommitDisposition::AuthorityRecordFailed {
                    receipt,
                    error: format!("operation terminal record failed: {error}"),
                };
            }
            EffectCommitDisposition::Receipt(receipt)
        }
    }

    async fn rollback_effect(&self, request: EffectRollbackRequest) -> AgentResult<()> {
        let EffectRollbackRequest {
            run_id,
            turn_id,
            operation_id,
            effect_id,
            argument_digest,
            generation,
            lease,
            effect,
            reason,
        } = request;
        let identity_error = (run_id != self.run_id()).then(|| {
            AgentError::InvalidRequest(format!(
                "effect rollback run {run_id} does not match Core run {}",
                self.run_id()
            ))
        });
        let lease_id = lease
            .as_ref()
            .map(|lease| lease.lease_id.as_str())
            .unwrap_or("none");
        self.effect()
            .rollback(
                effect,
                &format!(
                    "{reason} (turn {turn_id}, operation {operation_id}, generation {generation}, lease {lease_id})"
                ),
            )
            .await;
        if let Some(effect_id) = effect_id {
            self.abort_prepared_operation(operation_id, effect_id, argument_digest)?;
        }
        match identity_error {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    async fn emit_diagnostics(&self) -> AgentResult<()> {
        CoreAuthority::emit_diagnostics(self).await
    }

    async fn checkpoint(&self) -> AgentResult<serde_json::Value> {
        CoreAuthority::checkpoint(self).await
    }

    async fn restore(
        &self,
        data: serde_json::Value,
        expected_task_id: Option<TaskId>,
    ) -> AgentResult<()> {
        CoreAuthority::restore(self, data, expected_task_id).await
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_contracts::{
        ApprovalDecision, ArgumentDigest, AuthorityRecoveryStatus, ContextDiagnostics,
        ContextIngress, ContextMaintenanceReport, EffectDurability, EffectReconciler,
        EffectReconciliation, MaterializedContext, OperationEffectContext, OperationSnapshot,
        OperationState, OperationTerminal, ShadowVerdict, ToolExecutionRequest,
    };
    use std::sync::{
        Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    };

    struct RecoveredJournal {
        recovery: agent_contracts::OperationJournalRecovery,
        transitions: Mutex<Vec<agent_contracts::OperationJournalTransition>>,
    }

    struct ToggleFailJournal {
        fail: AtomicBool,
        transitions: Mutex<Vec<agent_contracts::OperationJournalTransition>>,
    }

    struct FixedReconciler {
        result: AgentResult<EffectReconciliation>,
        seen: Mutex<Vec<OperationEffectContext>>,
    }

    impl FixedReconciler {
        fn new(result: EffectReconciliation) -> Arc<Self> {
            Arc::new(Self {
                result: Ok(result),
                seen: Mutex::new(Vec::new()),
            })
        }
    }

    impl EffectReconciler for FixedReconciler {
        fn reconcile(&self, context: &OperationEffectContext) -> AgentResult<EffectReconciliation> {
            self.seen.lock().unwrap().push(context.clone());
            match &self.result {
                Ok(result) => Ok(result.clone()),
                Err(error) => Err(AgentError::RecoveryRequired(error.to_string())),
            }
        }
    }

    impl agent_contracts::OperationJournal for RecoveredJournal {
        fn append_and_sync(
            &self,
            transition: &agent_contracts::OperationJournalTransition,
        ) -> AgentResult<agent_contracts::OperationJournalRecord> {
            let mut transitions = self.transitions.lock().unwrap();
            transitions.push(transition.clone());
            Ok(agent_contracts::OperationJournalRecord {
                version: agent_contracts::OPERATION_JOURNAL_VERSION,
                seq: self.recovery.last_seq + transitions.len() as u64,
                transition: transition.clone(),
            })
        }

        fn recover(&self) -> AgentResult<agent_contracts::OperationJournalRecovery> {
            Ok(self.recovery.clone())
        }
    }

    impl agent_contracts::OperationJournal for ToggleFailJournal {
        fn append_and_sync(
            &self,
            transition: &agent_contracts::OperationJournalTransition,
        ) -> AgentResult<agent_contracts::OperationJournalRecord> {
            if self.fail.load(Ordering::SeqCst) {
                return Err(AgentError::Io("injected admission WAL failure".into()));
            }
            let mut transitions = self.transitions.lock().unwrap();
            transitions.push(transition.clone());
            Ok(agent_contracts::OperationJournalRecord {
                version: agent_contracts::OPERATION_JOURNAL_VERSION,
                seq: transitions.len() as u64,
                transition: transition.clone(),
            })
        }

        fn recover(&self) -> AgentResult<agent_contracts::OperationJournalRecovery> {
            Ok(agent_contracts::OperationJournalRecovery::default())
        }
    }

    #[derive(Debug)]
    struct StubContext;

    #[async_trait]
    impl ContextEngine for StubContext {
        async fn ingest(&self, _ingress: ContextIngress) -> AgentResult<()> {
            Ok(())
        }

        async fn maintain(
            &self,
            _trigger: agent_contracts::ContextMaintenanceTrigger,
        ) -> AgentResult<ContextMaintenanceReport> {
            Ok(ContextMaintenanceReport::default())
        }

        async fn materialize(
            &self,
            _query: agent_contracts::ContextQuery,
        ) -> AgentResult<MaterializedContext> {
            Ok(MaterializedContext {
                materialization_id: 0,
                focus: None,
                task: None,
                items: Vec::new(),
                external: Default::default(),
                selected: Vec::new(),
                approx_tokens: 0,
                foreground: Vec::new(),
                diagnostics: ContextDiagnostics::default(),
            })
        }

        async fn open_scope(
            &self,
            _kind: agent_contracts::ScopeKind,
            _parent: Option<agent_contracts::ScopeId>,
        ) -> AgentResult<agent_contracts::ScopeId> {
            Ok(agent_contracts::ScopeId::new())
        }

        async fn close_scope(
            &self,
            _scope_id: agent_contracts::ScopeId,
        ) -> AgentResult<Vec<agent_contracts::ContextStateTransition>> {
            Ok(Vec::new())
        }

        async fn diagnostics(&self) -> AgentResult<ContextDiagnostics> {
            Ok(ContextDiagnostics::default())
        }

        async fn inspect(
            &self,
            _limit: usize,
        ) -> AgentResult<Vec<agent_contracts::ContextItemSummary>> {
            Ok(Vec::new())
        }

        async fn checkpoint(&self) -> AgentResult<serde_json::Value> {
            Ok(serde_json::Value::Null)
        }

        async fn restore(&self, _data: serde_json::Value) -> AgentResult<()> {
            Ok(())
        }
    }

    #[derive(Debug)]
    struct StubTools;

    #[async_trait]
    impl ToolDispatcher for StubTools {
        fn specs(&self) -> Vec<ToolSpec> {
            Vec::new()
        }

        async fn execute(&self, _request: ToolExecutionRequest) -> AgentResult<ToolOutcome> {
            unreachable!("effect-port tests never execute a tool")
        }
    }

    struct PreparedEffectTools {
        state: Arc<Mutex<String>>,
        risk: agent_contracts::ToolRisk,
    }

    #[async_trait]
    impl ToolDispatcher for PreparedEffectTools {
        fn specs(&self) -> Vec<ToolSpec> {
            vec![ToolSpec {
                name: "prepared.effect".into(),
                description: "stages one recording effect".into(),
                input_schema: serde_json::json!({"type": "object"}),
                risk: self.risk,
                output_budget: None,
                roles: Vec::new(),
            }]
        }

        async fn execute(&self, request: ToolExecutionRequest) -> AgentResult<ToolOutcome> {
            request.validate().map_err(AgentError::InvalidRequest)?;
            match self.risk {
                agent_contracts::ToolRisk::ReadOnly => {
                    assert!(request.effect_context.is_none());
                }
                _ => {
                    request
                        .effect_context
                        .as_ref()
                        .expect("side-effecting dispatch receives a stable effect identity");
                }
            }
            Ok(ToolOutcome::PreparedEffect {
                output: ToolOutput {
                    call_id: request.call.id,
                    tool_name: request.call.name,
                    ok: true,
                    summary: "staged".into(),
                    model_content: "staged".into(),
                    artifact_ref: None,
                    metadata: serde_json::Value::Null,
                },
                effect: Box::new(RecordingEffect {
                    state: self.state.clone(),
                }),
            })
        }
    }

    struct Allow;

    #[async_trait]
    impl ApprovalGate for Allow {
        async fn authorize(
            &self,
            _call: &ToolCall,
            _spec: &ToolSpec,
            _cancel: &CancellationToken,
        ) -> AgentResult<ApprovalDecision> {
            Ok(ApprovalDecision::Allow)
        }
    }

    struct Deny;

    #[async_trait]
    impl ApprovalGate for Deny {
        async fn authorize(
            &self,
            _call: &ToolCall,
            _spec: &ToolSpec,
            _cancel: &CancellationToken,
        ) -> AgentResult<ApprovalDecision> {
            Ok(ApprovalDecision::Deny)
        }
    }

    struct NeverShadow;

    #[async_trait]
    impl agent_contracts::IntentShadowGate for NeverShadow {
        async fn shadow_verdict(&self, _call: &ToolCall, _spec: &ToolSpec) -> ShadowVerdict {
            std::future::pending().await
        }
    }

    struct CountingTools {
        executions: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl ToolDispatcher for CountingTools {
        fn specs(&self) -> Vec<ToolSpec> {
            vec![ToolSpec {
                name: "counting.read".into(),
                description: "counts dispatches".into(),
                input_schema: serde_json::json!({"type": "object"}),
                risk: agent_contracts::ToolRisk::ReadOnly,
                output_budget: None,
                roles: Vec::new(),
            }]
        }

        async fn execute(&self, request: ToolExecutionRequest) -> AgentResult<ToolOutcome> {
            self.executions.fetch_add(1, Ordering::SeqCst);
            Ok(ToolOutcome::Value(ToolOutput {
                call_id: request.call.id,
                tool_name: request.call.name,
                ok: true,
                summary: "executed".into(),
                model_content: "executed".into(),
                artifact_ref: None,
                metadata: serde_json::Value::Null,
            }))
        }
    }

    fn operation_identity(
        port: &dyn CorePort,
        call: &ToolCall,
        operation_id: OperationId,
        generation: u64,
    ) -> ToolOperationIdentity {
        ToolOperationIdentity {
            run_id: port.run_id(),
            task_id: None,
            turn_id: TurnId::new(),
            scope_id: None,
            operation_id,
            generation,
            call_id: call.id.clone(),
            tool_name: call.name.clone(),
            argument_digest: ArgumentDigest::from_json(&call.arguments),
        }
    }

    async fn admit_and_execute(
        port: &dyn CorePort,
        identity: ToolOperationIdentity,
        call: ToolCall,
        cancel: CancellationToken,
        surface: &ToolSurfaceSnapshot,
    ) -> CoreToolExecution {
        let generation = identity.generation;
        let ToolOperationAdmission::Accepted { permit, .. } = port
            .admit_tool_operation(identity, &call, generation)
            .expect("fresh test operation admission must succeed")
        else {
            panic!("fresh test operation must receive a permit")
        };
        let permit = port
            .publish_tool_operation(permit, &call)
            .await
            .expect("fresh test operation publication must succeed");
        port.execute_published_tool(permit, call, cancel, surface)
            .await
    }

    struct RecordingEffect {
        state: Arc<Mutex<String>>,
    }

    #[async_trait]
    impl Effect for RecordingEffect {
        fn describe(&self) -> String {
            "recording effect".into()
        }

        async fn commit(self: Box<Self>) -> EffectReceipt {
            *self.state.lock().unwrap() = "committed".into();
            EffectReceipt::Applied {
                durability: agent_contracts::EffectDurability::Durable,
                evidence: None,
            }
        }

        async fn rollback(self: Box<Self>, reason: &str) {
            *self.state.lock().unwrap() = format!("rolled back: {reason}");
        }
    }

    fn port() -> Arc<dyn CorePort> {
        build_core_port(
            CoreAuthorityConfig::default(),
            Arc::new(StubContext),
            Arc::new(StubTools),
            Arc::new(Allow),
            None,
        )
    }

    #[test]
    fn durable_core_startup_advances_recovered_epoch_before_publication() {
        let journal = Arc::new(RecoveredJournal {
            recovery: agent_contracts::OperationJournalRecovery {
                authority_epoch: 7,
                ..agent_contracts::OperationJournalRecovery::default()
            },
            transitions: Mutex::new(Vec::new()),
        });
        let core = try_build_core_port(
            CoreAuthorityConfig::default(),
            Arc::new(StubContext),
            Arc::new(StubTools),
            Arc::new(Allow),
            None,
            Some(journal.clone()),
            None,
        )
        .unwrap();

        assert_eq!(core.current_authority_epoch(), 8);
        assert_eq!(
            journal.transitions.lock().unwrap().as_slice(),
            &[agent_contracts::OperationJournalTransition::EpochAdvanced { from: 7, to: 8 }]
        );
    }

    fn recovered_snapshot(state: OperationState) -> OperationSnapshot {
        OperationSnapshot {
            identity: ToolOperationIdentity {
                run_id: RunId::new(),
                task_id: None,
                turn_id: TurnId::new(),
                scope_id: None,
                operation_id: OperationId::new(),
                generation: 7,
                call_id: "recovered-call".into(),
                tool_name: "fs.write".into(),
                argument_digest: ArgumentDigest::sha256_bytes(b"recovered-args"),
            },
            state,
        }
    }

    fn recovered_port(
        snapshot: OperationSnapshot,
        reconciler: Option<Arc<dyn EffectReconciler>>,
    ) -> (Arc<dyn CorePort>, Arc<RecoveredJournal>) {
        let journal = Arc::new(RecoveredJournal {
            recovery: agent_contracts::OperationJournalRecovery {
                authority_epoch: 7,
                operations: vec![snapshot],
                ..agent_contracts::OperationJournalRecovery::default()
            },
            transitions: Mutex::new(Vec::new()),
        });
        let core = try_build_core_port(
            CoreAuthorityConfig::default(),
            Arc::new(StubContext),
            Arc::new(StubTools),
            Arc::new(Allow),
            None,
            Some(journal.clone()),
            reconciler,
        )
        .unwrap();
        (core, journal)
    }

    #[test]
    fn restart_terminalizes_pre_effect_states_wal_first_without_a_reconciler() {
        for state in [
            OperationState::Accepted,
            OperationState::Executing { effect_id: None },
        ] {
            let recovered = recovered_snapshot(state);
            let operation_id = recovered.identity.operation_id;
            let (core, journal) = recovered_port(recovered, None);

            assert_eq!(core.recovery_status(), AuthorityRecoveryStatus::Ready);
            assert!(matches!(
                core.query_operation(operation_id),
                OperationQueryResult::Found { snapshot }
                    if snapshot.state == OperationState::Terminal {
                        effect_id: None,
                        terminal: OperationTerminal::CancelledBeforeCommit,
                    }
            ));
            assert!(matches!(
                journal.transitions.lock().unwrap().as_slice(),
                [
                    agent_contracts::OperationJournalTransition::EpochAdvanced { from: 7, to: 8 },
                    agent_contracts::OperationJournalTransition::OperationUpsert { snapshot }
                ] if matches!(snapshot.state, OperationState::Terminal {
                    terminal: OperationTerminal::CancelledBeforeCommit,
                    ..
                })
            ));
        }
    }

    #[test]
    fn restart_reconciles_commit_started_applied_evidence_to_terminal_truth() {
        let effect_id = EffectId::new();
        let recovered = recovered_snapshot(OperationState::CommitStarted { effect_id });
        let expected_identity = recovered.identity.clone();
        let operation_id = expected_identity.operation_id;
        let reconciler = FixedReconciler::new(EffectReconciliation::Applied {
            durability: EffectDurability::Durable,
            evidence: Some("workspace transaction committed".into()),
        });
        let (core, _) = recovered_port(recovered, Some(reconciler.clone()));

        assert_eq!(core.recovery_status(), AuthorityRecoveryStatus::Ready);
        assert_eq!(
            reconciler.seen.lock().unwrap().as_slice(),
            &[OperationEffectContext {
                identity: expected_identity,
                effect_id,
            }]
        );
        let OperationQueryResult::Found { snapshot } = core.query_operation(operation_id) else {
            panic!("reconciled operation must remain queryable")
        };
        assert!(matches!(
            snapshot.state,
            OperationState::Terminal {
                effect_id: Some(recorded),
                terminal: OperationTerminal::Applied {
                    durability: EffectDurability::Durable,
                    evidence: Some(ref evidence),
                },
            } if recorded == effect_id && evidence == "workspace transaction committed"
        ));
    }

    #[tokio::test]
    async fn early_applied_evidence_is_terminalized_honestly_but_keeps_core_fenced() {
        let effect_id = EffectId::new();
        let recovered = recovered_snapshot(OperationState::Prepared { effect_id });
        let operation_id = recovered.identity.operation_id;
        let reconciler = FixedReconciler::new(EffectReconciliation::Applied {
            durability: EffectDurability::Durable,
            evidence: Some("target hash matches after image".into()),
        });
        let (core, _) = recovered_port(recovered, Some(reconciler));

        assert!(matches!(
            core.recovery_status(),
            AuthorityRecoveryStatus::RecoveryRequired { .. }
        ));
        assert!(matches!(
            core.query_operation(operation_id),
            OperationQueryResult::Found { snapshot }
                if matches!(snapshot.state, OperationState::Terminal {
                    effect_id: Some(recorded),
                    terminal: OperationTerminal::Applied {
                        durability: EffectDurability::DurabilityFailed(_),
                        ..
                    },
                } if recorded == effect_id)
        ));
        assert!(matches!(
            core.advance_authority_epoch(core.current_authority_epoch()),
            Err(AgentError::RecoveryRequired(_))
        ));

        let mut events = core.event_sender().subscribe();
        core.start().await.unwrap();
        assert!(matches!(
            events.recv().await.unwrap().event,
            RuntimeEvent::RunStarted
        ));
        assert!(matches!(
            events.recv().await.unwrap().event,
            RuntimeEvent::Warning { .. }
        ));
        assert!(matches!(
            events.recv().await.unwrap().event,
            RuntimeEvent::RecoveryRequired
        ));
        core.stop().await.unwrap();
    }

    #[test]
    fn completed_value_process_recovery_terminalizes_executing_without_fencing() {
        let effect_id = EffectId::new();
        let recovered = recovered_snapshot(OperationState::Executing {
            effect_id: Some(effect_id),
        });
        let operation_id = recovered.identity.operation_id;
        let (core, _) = recovered_port(
            recovered,
            Some(FixedReconciler::new(EffectReconciliation::CompletedValue {
                evidence: Some("process-pid:7:exit:0".into()),
            })),
        );
        assert_eq!(core.recovery_status(), AuthorityRecoveryStatus::Ready);
        assert!(matches!(
            core.query_operation(operation_id),
            OperationQueryResult::Found { snapshot }
                if matches!(snapshot.state, OperationState::Terminal {
                    effect_id: Some(recorded),
                    terminal: OperationTerminal::CompletedValue,
                } if recorded == effect_id)
        ));
        assert!(
            core.advance_authority_epoch(core.current_authority_epoch())
                .is_ok()
        );
    }

    #[test]
    fn completed_value_from_commit_started_stays_unresolved_and_fences() {
        let effect_id = EffectId::new();
        let recovered = recovered_snapshot(OperationState::CommitStarted { effect_id });
        let operation_id = recovered.identity.operation_id;
        let (core, _) = recovered_port(
            recovered,
            Some(FixedReconciler::new(EffectReconciliation::CompletedValue {
                evidence: Some("process-pid:7:exit:0".into()),
            })),
        );
        assert!(matches!(
            core.recovery_status(),
            AuthorityRecoveryStatus::RecoveryRequired { .. }
        ));
        assert!(matches!(
            core.query_operation(operation_id),
            OperationQueryResult::Found { snapshot }
                if snapshot.state == OperationState::CommitStarted { effect_id }
        ));
    }

    #[test]
    fn ambiguous_or_unmanaged_recovery_keeps_original_state_and_fences_mutation() {
        for result in [
            EffectReconciliation::NotManaged,
            EffectReconciliation::Ambiguous {
                reason: "target matches neither before nor after digest".into(),
            },
        ] {
            let effect_id = EffectId::new();
            let recovered = recovered_snapshot(OperationState::CommitStarted { effect_id });
            let operation_id = recovered.identity.operation_id;
            let (core, _) = recovered_port(recovered, Some(FixedReconciler::new(result)));
            assert!(matches!(
                core.recovery_status(),
                AuthorityRecoveryStatus::RecoveryRequired { .. }
            ));
            assert!(matches!(
                core.query_operation(operation_id),
                OperationQueryResult::Found { snapshot }
                    if snapshot.state == OperationState::CommitStarted { effect_id }
            ));
        }
    }

    #[test]
    fn recovered_terminal_unknown_or_durability_failure_reinstalls_the_fence() {
        for terminal in [
            OperationTerminal::OutcomeUnknown {
                error: "commit crashed without world-state evidence".into(),
            },
            OperationTerminal::Applied {
                durability: EffectDurability::DurabilityFailed(
                    "target landed but directory sync failed".into(),
                ),
                evidence: Some("target hash matches after image".into()),
            },
        ] {
            let effect_id = EffectId::new();
            let recovered = recovered_snapshot(OperationState::Terminal {
                effect_id: Some(effect_id),
                terminal: terminal.clone(),
            });
            let operation_id = recovered.identity.operation_id;
            let (core, journal) = recovered_port(recovered, None);

            assert!(matches!(
                core.recovery_status(),
                AuthorityRecoveryStatus::RecoveryRequired { .. }
            ));
            assert!(matches!(
                core.query_operation(operation_id),
                OperationQueryResult::Found { snapshot }
                    if snapshot.state == OperationState::Terminal {
                        effect_id: Some(effect_id),
                        terminal,
                    }
            ));
            assert_eq!(
                journal.transitions.lock().unwrap().as_slice(),
                &[agent_contracts::OperationJournalTransition::EpochAdvanced { from: 7, to: 8 }],
                "terminal recovery derives a fence without rewriting truth"
            );
        }
    }

    #[test]
    fn early_applied_terminal_reinstalls_fence_on_a_second_restart() {
        let effect_id = EffectId::new();
        let recovered = recovered_snapshot(OperationState::Prepared { effect_id });
        let operation_id = recovered.identity.operation_id;
        let (first, _) = recovered_port(
            recovered,
            Some(FixedReconciler::new(EffectReconciliation::Applied {
                durability: EffectDurability::Durable,
                evidence: Some("after hash matched".into()),
            })),
        );
        let OperationQueryResult::Found { snapshot } = first.query_operation(operation_id) else {
            panic!("first recovery must terminalize the operation")
        };
        assert!(matches!(
            first.recovery_status(),
            AuthorityRecoveryStatus::RecoveryRequired { .. }
        ));

        let (second, _) = recovered_port(snapshot.as_ref().clone(), None);
        assert!(matches!(
            second.recovery_status(),
            AuthorityRecoveryStatus::RecoveryRequired { .. }
        ));
        assert_eq!(
            second.query_operation(operation_id),
            OperationQueryResult::Found { snapshot }
        );
    }

    fn lease(
        generation: u64,
        operation_id: OperationId,
        argument_digest: ArgumentDigest,
        expires_at_ms: u64,
    ) -> AuthorityLease {
        AuthorityLease {
            lease_id: "lease-test".into(),
            operation_id,
            argument_digest,
            operation_generation: generation,
            intent: agent_contracts::EffectIntent::WorkspaceWrite {
                path: "src/lib.rs".into(),
                content_bytes: 1,
            },
            grant_id: None,
            decision: ApprovalDecision::Allow,
            issued_at_ms: 0,
            expires_at_ms,
        }
    }

    fn commit_request(
        port: &dyn CorePort,
        generation: u64,
        operation_id: OperationId,
        lease: Option<AuthorityLease>,
        state: Arc<Mutex<String>>,
    ) -> EffectCommitRequest {
        let argument_digest = ArgumentDigest::sha256_bytes(b"args");
        EffectCommitRequest {
            run_id: port.run_id(),
            turn_id: TurnId::new(),
            operation_id,
            effect_id: EffectId::new(),
            argument_digest,
            generation,
            lease,
            effect: Box::new(RecordingEffect { state }),
        }
    }

    #[tokio::test]
    async fn commit_refuses_missing_lease_and_rolls_back() {
        let port = port();
        let generation = port.current_authority_epoch();
        let state = Arc::new(Mutex::new(String::new()));
        let operation_id = OperationId::new();
        let receipt = port
            .commit_effect(commit_request(
                port.as_ref(),
                generation,
                operation_id,
                None,
                state.clone(),
            ))
            .await;
        assert!(matches!(
            receipt,
            EffectCommitDisposition::Rejected(EffectCommitRejection::MissingLease)
        ));
        assert!(state.lock().unwrap().starts_with("rolled back:"));
    }

    #[tokio::test]
    async fn commit_refuses_foreign_run_even_with_valid_lease() {
        let port = port();
        let state = Arc::new(Mutex::new(String::new()));
        let operation_id = OperationId::new();
        let argument_digest = ArgumentDigest::sha256_bytes(b"args");
        let mut request = commit_request(
            port.as_ref(),
            4,
            operation_id,
            Some(lease(4, operation_id, argument_digest, u64::MAX)),
            state.clone(),
        );
        request.run_id = RunId::new();
        let receipt = port.commit_effect(request).await;
        assert!(matches!(
            receipt,
            EffectCommitDisposition::Rejected(EffectCommitRejection::ForeignRun)
        ));
        assert!(state.lock().unwrap().starts_with("rolled back:"));
    }

    #[tokio::test]
    async fn commit_refuses_a_forged_lease_for_an_unadmitted_operation() {
        let port = port();
        let generation = port.current_authority_epoch();
        let state = Arc::new(Mutex::new(String::new()));
        let operation_id = OperationId::new();
        let argument_digest = ArgumentDigest::sha256_bytes(b"args");
        let receipt = port
            .commit_effect(commit_request(
                port.as_ref(),
                generation,
                operation_id,
                Some(lease(generation, operation_id, argument_digest, u64::MAX)),
                state.clone(),
            ))
            .await;
        assert!(matches!(
            receipt,
            EffectCommitDisposition::Rejected(EffectCommitRejection::InvalidLease)
        ));
        assert!(state.lock().unwrap().starts_with("rolled back:"));
    }

    #[test]
    fn authority_epoch_is_core_owned_monotonic_and_compare_and_swapped() {
        let port = port();
        let first = port.current_authority_epoch();
        assert_ne!(first, 0);
        let second = port.advance_authority_epoch(first).unwrap();
        assert_eq!(second, first + 1);
        assert_eq!(port.current_authority_epoch(), second);
        assert!(matches!(
            port.advance_authority_epoch(first),
            Err(AgentError::RecoveryRequired(_))
        ));
        assert_eq!(port.current_authority_epoch(), second);
    }

    #[test]
    fn in_memory_core_does_not_synthesize_durable_checkpoint_authority() {
        let port = port();
        assert_eq!(port.authority_checkpoint_marker().unwrap(), None);
        assert_eq!(port.compact_authority_journal().unwrap(), None);
        let marker = AuthorityCheckpointMarker {
            journal_id: agent_contracts::AuthorityJournalId::new(),
            generation: 1,
            authority_epoch: 1,
            last_seq: 0,
            state_digest: agent_contracts::AuthorityStateDigest::sha256_bytes(b"foreign"),
        };
        assert!(matches!(
            port.validate_authority_checkpoint_marker(&marker),
            Err(AgentError::RecoveryRequired(_))
        ));
    }

    #[tokio::test]
    async fn split_admission_is_observable_before_dispatch_and_duplicate_has_no_permit() {
        let executions = Arc::new(AtomicUsize::new(0));
        let tools = Arc::new(CountingTools {
            executions: executions.clone(),
        });
        let port = build_core_port(
            CoreAuthorityConfig::default(),
            Arc::new(StubContext),
            tools.clone(),
            Arc::new(Allow),
            None,
        );
        let call = ToolCall {
            id: "split-admission".into(),
            name: "counting.read".into(),
            arguments: serde_json::json!({"path": "src/lib.rs"}),
        };
        let surface = ToolSurfaceSnapshot {
            specs: tools.specs(),
            ..ToolSurfaceSnapshot::default()
        };
        let generation = port.current_authority_epoch();
        let operation_id = OperationId::new();
        let identity = operation_identity(port.as_ref(), &call, operation_id, generation);

        let ToolOperationAdmission::Accepted { snapshot, permit } = port
            .admit_tool_operation(identity.clone(), &call, generation)
            .unwrap()
        else {
            panic!("fresh operation must receive a dispatch permit")
        };
        assert_eq!(snapshot.identity, identity);
        assert_eq!(snapshot.state, OperationState::Accepted);
        assert_eq!(executions.load(Ordering::SeqCst), 0);
        assert!(matches!(
            port.query_operation(operation_id),
            OperationQueryResult::Found { snapshot }
                if snapshot.state == OperationState::Accepted
        ));

        let ToolOperationAdmission::AlreadyKnown {
            snapshot: duplicate,
        } = port
            .admit_tool_operation(identity, &call, generation)
            .unwrap()
        else {
            panic!("an exact retry must not receive another permit")
        };
        assert_eq!(duplicate.state, OperationState::Accepted);
        assert_eq!(executions.load(Ordering::SeqCst), 0);

        let permit = port.publish_tool_operation(permit, &call).await.unwrap();
        let execution = port
            .execute_published_tool(permit, call.clone(), CancellationToken::new(), &surface)
            .await;
        assert!(execution.value_completion_pending);
        assert_eq!(executions.load(Ordering::SeqCst), 1);

        port.advance_authority_epoch(generation).unwrap();
        let ToolOperationAdmission::AlreadyKnown {
            snapshot: stale_retry,
        } = port
            .admit_tool_operation(snapshot.identity.clone(), &call, generation)
            .unwrap()
        else {
            panic!("an exact retry remains observable after its epoch advances")
        };
        assert!(matches!(
            stale_retry.state,
            OperationState::Executing { effect_id: None }
        ));
        assert_eq!(executions.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn split_admission_validates_request_and_current_generation_before_recording() {
        let port = port();
        let call = ToolCall {
            id: "validate-admission".into(),
            name: "counting.read".into(),
            arguments: serde_json::json!({"value": 1}),
        };
        let generation = port.current_authority_epoch();

        let mismatched_id = OperationId::new();
        let mut mismatched = operation_identity(port.as_ref(), &call, mismatched_id, generation);
        mismatched.argument_digest = ArgumentDigest::sha256_bytes(b"different arguments");
        assert!(matches!(
            port.admit_tool_operation(mismatched, &call, generation),
            Err(AgentError::InvalidRequest(_))
        ));
        assert_eq!(
            port.query_operation(mismatched_id),
            OperationQueryResult::NotFound
        );

        let stale_id = OperationId::new();
        let stale = operation_identity(port.as_ref(), &call, stale_id, generation);
        port.advance_authority_epoch(generation).unwrap();
        assert!(matches!(
            port.admit_tool_operation(stale, &call, generation),
            Err(AgentError::InvalidRequest(_))
        ));
        assert_eq!(
            port.query_operation(stale_id),
            OperationQueryResult::NotFound
        );
    }

    #[test]
    fn split_admission_wal_failure_returns_no_permit_and_fences_dispatch() {
        let executions = Arc::new(AtomicUsize::new(0));
        let tools = Arc::new(CountingTools {
            executions: executions.clone(),
        });
        let journal = Arc::new(ToggleFailJournal {
            fail: AtomicBool::new(false),
            transitions: Mutex::new(Vec::new()),
        });
        let port = try_build_core_port(
            CoreAuthorityConfig::default(),
            Arc::new(StubContext),
            tools,
            Arc::new(Allow),
            None,
            Some(journal.clone()),
            None,
        )
        .unwrap();
        let call = ToolCall {
            id: "wal-failure".into(),
            name: "counting.read".into(),
            arguments: serde_json::json!({}),
        };
        let generation = port.current_authority_epoch();
        let operation_id = OperationId::new();
        let identity = operation_identity(port.as_ref(), &call, operation_id, generation);
        journal.fail.store(true, Ordering::SeqCst);

        assert!(matches!(
            port.admit_tool_operation(identity, &call, generation),
            Err(AgentError::RecoveryRequired(_))
        ));
        assert_eq!(
            port.query_operation(operation_id),
            OperationQueryResult::NotFound
        );
        assert_eq!(executions.load(Ordering::SeqCst), 0);
        assert!(matches!(
            port.recovery_status(),
            AuthorityRecoveryStatus::RecoveryRequired { .. }
        ));
    }

    #[tokio::test]
    async fn commit_refuses_a_stale_epoch_and_rolls_back() {
        let state = Arc::new(Mutex::new(String::new()));
        let tools = Arc::new(PreparedEffectTools {
            state: state.clone(),
            risk: agent_contracts::ToolRisk::WorkspaceWrite,
        });
        let port = build_core_port(
            CoreAuthorityConfig::default(),
            Arc::new(StubContext),
            tools.clone(),
            Arc::new(Allow),
            None,
        );
        let call = ToolCall {
            id: "stale-write".into(),
            name: "prepared.effect".into(),
            arguments: serde_json::json!({"path": "src/lib.rs", "content": "x"}),
        };
        let surface = ToolSurfaceSnapshot {
            specs: tools.specs(),
            ..ToolSurfaceSnapshot::default()
        };
        let stale = port.current_authority_epoch();
        let operation_id = OperationId::new();
        let argument_digest = ArgumentDigest::from_json(&call.arguments);
        let execution = admit_and_execute(
            port.as_ref(),
            operation_identity(port.as_ref(), &call, operation_id, stale),
            call,
            CancellationToken::new(),
            &surface,
        )
        .await;
        let ToolOutcome::PreparedEffect { effect, .. } = execution.outcome else {
            panic!("fixture must prepare an effect")
        };
        let effect_id = execution.effect_id.expect("Core assigns an effect id");
        let issued_lease = execution.lease.expect("Core issues a write lease");
        port.advance_authority_epoch(stale).unwrap();
        let result = port
            .commit_effect(EffectCommitRequest {
                run_id: port.run_id(),
                turn_id: TurnId::new(),
                operation_id,
                effect_id,
                argument_digest,
                generation: stale,
                lease: Some(issued_lease),
                effect,
            })
            .await;
        assert!(matches!(
            result,
            EffectCommitDisposition::Rejected(EffectCommitRejection::StaleEpoch)
        ));
        assert!(state.lock().unwrap().starts_with("rolled back:"));
        let OperationQueryResult::Found { snapshot } = port.query_operation(operation_id) else {
            panic!("stale prepared operation must remain queryable")
        };
        assert!(matches!(
            snapshot.state,
            OperationState::Terminal {
                effect_id: Some(recorded),
                terminal: OperationTerminal::CancelledBeforeCommit,
            } if recorded == effect_id
        ));
    }

    #[tokio::test]
    async fn cancellation_bounds_a_never_returning_shadow_and_terminalizes_operation() {
        let executions = Arc::new(AtomicUsize::new(0));
        let tools = Arc::new(CountingTools {
            executions: executions.clone(),
        });
        let port = build_core_port(
            CoreAuthorityConfig {
                shadow_gate: Some(Arc::new(NeverShadow)),
                ..CoreAuthorityConfig::default()
            },
            Arc::new(StubContext),
            tools.clone(),
            Arc::new(Allow),
            None,
        );
        let call = ToolCall {
            id: "cancel-shadow".into(),
            name: "counting.read".into(),
            arguments: serde_json::json!({}),
        };
        let surface = ToolSurfaceSnapshot {
            specs: tools.specs(),
            ..ToolSurfaceSnapshot::default()
        };
        let generation = port.current_authority_epoch();
        let operation_id = OperationId::new();
        let cancel = CancellationToken::new();
        cancel.cancel();
        tokio::time::timeout(
            std::time::Duration::from_millis(250),
            admit_and_execute(
                port.as_ref(),
                operation_identity(port.as_ref(), &call, operation_id, generation),
                call,
                cancel,
                &surface,
            ),
        )
        .await
        .expect("cancellation must bound shadow evaluation");
        assert_eq!(executions.load(Ordering::SeqCst), 0);
        let OperationQueryResult::Found { snapshot } = port.query_operation(operation_id) else {
            panic!("cancelled operation must remain queryable")
        };
        assert!(matches!(
            snapshot.state,
            OperationState::Terminal {
                effect_id: None,
                terminal: OperationTerminal::CancelledBeforeCommit,
            }
        ));
    }

    #[tokio::test]
    async fn missing_surface_and_approval_denial_terminalize_without_filling_registry() {
        let port = build_core_port(
            CoreAuthorityConfig::default(),
            Arc::new(StubContext),
            Arc::new(StubTools),
            Arc::new(Deny),
            None,
        );
        let generation = port.current_authority_epoch();
        let denied_spec = ToolSpec {
            name: "denied.read".into(),
            description: "always denied".into(),
            input_schema: serde_json::json!({"type": "object"}),
            risk: agent_contracts::ToolRisk::ReadOnly,
            output_budget: None,
            roles: Vec::new(),
        };
        let denied_surface = ToolSurfaceSnapshot {
            specs: vec![denied_spec],
            ..ToolSurfaceSnapshot::default()
        };
        let missing_surface = ToolSurfaceSnapshot::default();
        let mut last_operation = None;
        for index in 0..1_100 {
            let call = if index % 2 == 0 {
                ToolCall {
                    id: format!("missing-{index}"),
                    name: "missing.read".into(),
                    arguments: serde_json::json!({}),
                }
            } else {
                ToolCall {
                    id: format!("denied-{index}"),
                    name: "denied.read".into(),
                    arguments: serde_json::json!({}),
                }
            };
            let surface = if index % 2 == 0 {
                &missing_surface
            } else {
                &denied_surface
            };
            let operation_id = OperationId::new();
            let result = admit_and_execute(
                port.as_ref(),
                operation_identity(port.as_ref(), &call, operation_id, generation),
                call,
                CancellationToken::new(),
                surface,
            )
            .await;
            assert!(!result.value_completion_pending);
            last_operation = Some(operation_id);
        }
        let OperationQueryResult::Found { snapshot } =
            port.query_operation(last_operation.expect("loop admits operations"))
        else {
            panic!("latest refusal must remain queryable")
        };
        assert!(matches!(
            snapshot.state,
            OperationState::Terminal {
                effect_id: None,
                terminal: OperationTerminal::Refused { .. },
            }
        ));
    }

    #[tokio::test]
    async fn foreign_run_rollback_still_cleans_up() {
        let port = port();
        let state = Arc::new(Mutex::new(String::new()));
        let result = port
            .rollback_effect(EffectRollbackRequest {
                run_id: RunId::new(),
                turn_id: TurnId::new(),
                operation_id: OperationId::new(),
                effect_id: None,
                argument_digest: ArgumentDigest::sha256_bytes(b"args"),
                generation: 4,
                lease: None,
                effect: Box::new(RecordingEffect {
                    state: state.clone(),
                }),
                reason: "stale".into(),
            })
            .await;
        assert!(matches!(result, Err(AgentError::InvalidRequest(_))));
        assert!(state.lock().unwrap().starts_with("rolled back: stale"));
    }

    #[tokio::test]
    async fn read_only_tool_cannot_smuggle_a_prepared_effect_without_a_lease() {
        let state = Arc::new(Mutex::new(String::new()));
        let port = build_core_port(
            CoreAuthorityConfig::default(),
            Arc::new(StubContext),
            Arc::new(PreparedEffectTools {
                state: state.clone(),
                risk: agent_contracts::ToolRisk::ReadOnly,
            }),
            Arc::new(Allow),
            None,
        );
        let call = ToolCall {
            id: "call-1".into(),
            name: "prepared.effect".into(),
            arguments: serde_json::json!({}),
        };
        let surface = ToolSurfaceSnapshot {
            specs: PreparedEffectTools {
                state: state.clone(),
                risk: agent_contracts::ToolRisk::ReadOnly,
            }
            .specs(),
            ..ToolSurfaceSnapshot::default()
        };
        let generation = port.current_authority_epoch();
        let operation_id = OperationId::new();
        let argument_digest = ArgumentDigest::from_json(&call.arguments);
        let execution = admit_and_execute(
            port.as_ref(),
            ToolOperationIdentity {
                run_id: port.run_id(),
                task_id: None,
                turn_id: TurnId::new(),
                scope_id: None,
                operation_id,
                generation,
                call_id: call.id.clone(),
                tool_name: call.name.clone(),
                argument_digest,
            },
            call,
            CancellationToken::new(),
            &surface,
        )
        .await;
        let CoreToolExecution { outcome, lease, .. } = execution;
        assert!(
            lease.is_none(),
            "read-only declarations mint no effect lease"
        );
        assert!(matches!(outcome, ToolOutcome::Value(ref output) if !output.ok));
        assert_eq!(execution.effect_id, None);
        assert!(!execution.value_completion_pending);
        assert!(state.lock().unwrap().starts_with("rolled back:"));
        let OperationQueryResult::Found { snapshot } = port.query_operation(operation_id) else {
            panic!("rejected smuggled effect remains queryable")
        };
        assert!(matches!(
            snapshot.state,
            OperationState::Terminal {
                effect_id: None,
                terminal: OperationTerminal::CancelledBeforeCommit,
            }
        ));
    }

    #[tokio::test]
    async fn forged_matching_lease_cannot_authorize_a_prepared_effect() {
        let state = Arc::new(Mutex::new(String::new()));
        let port = build_core_port(
            CoreAuthorityConfig::default(),
            Arc::new(StubContext),
            Arc::new(PreparedEffectTools {
                state: state.clone(),
                risk: agent_contracts::ToolRisk::WorkspaceWrite,
            }),
            Arc::new(Allow),
            None,
        );
        let call = ToolCall {
            id: "forged-lease".into(),
            name: "prepared.effect".into(),
            arguments: serde_json::json!({}),
        };
        let surface = ToolSurfaceSnapshot {
            specs: PreparedEffectTools {
                state: state.clone(),
                risk: agent_contracts::ToolRisk::WorkspaceWrite,
            }
            .specs(),
            ..ToolSurfaceSnapshot::default()
        };
        let generation = port.current_authority_epoch();
        let operation_id = OperationId::new();
        let argument_digest = ArgumentDigest::from_json(&call.arguments);
        let execution = admit_and_execute(
            port.as_ref(),
            ToolOperationIdentity {
                run_id: port.run_id(),
                task_id: None,
                turn_id: TurnId::new(),
                scope_id: None,
                operation_id,
                generation,
                call_id: call.id.clone(),
                tool_name: call.name.clone(),
                argument_digest,
            },
            call,
            CancellationToken::new(),
            &surface,
        )
        .await;
        let ToolOutcome::PreparedEffect { effect, .. } = execution.outcome else {
            panic!("fixture must stage an effect")
        };
        let effect_id = execution.effect_id.expect("Core assigns an effect id");
        let forged = lease(generation, operation_id, argument_digest, u64::MAX);
        let result = port
            .commit_effect(EffectCommitRequest {
                run_id: port.run_id(),
                turn_id: TurnId::new(),
                operation_id,
                effect_id,
                argument_digest,
                generation,
                lease: Some(forged),
                effect,
            })
            .await;
        assert!(matches!(
            result,
            EffectCommitDisposition::Rejected(EffectCommitRejection::InvalidLease)
        ));
        assert!(state.lock().unwrap().starts_with("rolled back:"));
    }

    #[tokio::test]
    async fn admitted_prepared_effect_commits_at_most_once_in_process() {
        let state = Arc::new(Mutex::new(String::new()));
        let port = build_core_port(
            CoreAuthorityConfig::default(),
            Arc::new(StubContext),
            Arc::new(PreparedEffectTools {
                state: state.clone(),
                risk: agent_contracts::ToolRisk::WorkspaceWrite,
            }),
            Arc::new(Allow),
            None,
        );
        let call = ToolCall {
            id: "write-1".into(),
            name: "prepared.effect".into(),
            arguments: serde_json::json!({"path": "src/lib.rs", "content": "x"}),
        };
        let surface = ToolSurfaceSnapshot {
            specs: PreparedEffectTools {
                state: state.clone(),
                risk: agent_contracts::ToolRisk::WorkspaceWrite,
            }
            .specs(),
            ..ToolSurfaceSnapshot::default()
        };
        let generation = port.current_authority_epoch();
        let operation_id = OperationId::new();
        let argument_digest = ArgumentDigest::from_json(&call.arguments);
        let execution = admit_and_execute(
            port.as_ref(),
            ToolOperationIdentity {
                run_id: port.run_id(),
                task_id: None,
                turn_id: TurnId::new(),
                scope_id: None,
                operation_id,
                generation,
                call_id: call.id.clone(),
                tool_name: call.name.clone(),
                argument_digest,
            },
            call,
            CancellationToken::new(),
            &surface,
        )
        .await;
        let CoreToolExecution {
            outcome,
            lease,
            effect_id,
            ..
        } = execution;
        let ToolOutcome::PreparedEffect { effect, .. } = outcome else {
            panic!("fixture must return a prepared effect")
        };
        let effect_id = effect_id.expect("Core assigns prepared effect identity");
        let lease = lease.expect("write-risk operation receives a lease");
        let first = port
            .commit_effect(EffectCommitRequest {
                run_id: port.run_id(),
                turn_id: TurnId::new(),
                operation_id,
                effect_id,
                argument_digest,
                generation,
                lease: Some(lease.clone()),
                effect,
            })
            .await;
        assert!(matches!(
            first,
            EffectCommitDisposition::Receipt(EffectReceipt::Applied { .. })
        ));
        assert_eq!(&*state.lock().unwrap(), "committed");
        let OperationQueryResult::Found { snapshot } = port.query_operation(operation_id) else {
            panic!("committed operation must remain queryable")
        };
        assert!(matches!(
            snapshot.state,
            agent_contracts::OperationState::Terminal {
                effect_id: Some(recorded),
                terminal: agent_contracts::OperationTerminal::Applied { .. },
            } if recorded == effect_id
        ));

        let duplicate_state = Arc::new(Mutex::new(String::new()));
        let duplicate = port
            .commit_effect(EffectCommitRequest {
                run_id: port.run_id(),
                turn_id: TurnId::new(),
                operation_id,
                effect_id,
                argument_digest,
                generation,
                lease: Some(lease),
                effect: Box::new(RecordingEffect {
                    state: duplicate_state.clone(),
                }),
            })
            .await;
        assert!(matches!(
            duplicate,
            EffectCommitDisposition::Rejected(EffectCommitRejection::InvalidLease)
        ));
        assert!(duplicate_state.lock().unwrap().starts_with("rolled back:"));
    }
}
