use super::*;
use crate::checkpoint::{RUNTIME_CHECKPOINT_VERSION, RunMetadata, TaskManagerSnapshot};
use agent_contracts::{
    ArgumentDigest, AuthorityRecoveryStatus, ContextDiagnostics, ContextEngine, ContextIngress,
    ContextItemSummary, ContextMaintenanceReport, EffectId, MaterializedContext, ModelCapabilities,
    ModelOutput, ModelTransport, OPERATION_JOURNAL_VERSION, OperationJournal,
    OperationJournalRecord, OperationJournalRecovery, OperationJournalTransition,
    OperationQueryResult, OperationSnapshot, OperationState, OperationTerminal, ToolCall,
    ToolDispatcher, ToolExecutionRequest, ToolOperationIdentity, ToolRisk, ToolSpec,
};
use agent_core::{CoreAuthorityConfig, PolicyApprovalGate};
use async_trait::async_trait;
use std::sync::{
    Mutex,
    atomic::{AtomicU64, AtomicUsize, Ordering},
};
use tokio::sync::{mpsc, oneshot};

#[derive(Debug)]
struct TestContext;

#[async_trait]
impl ContextEngine for TestContext {
    async fn ingest(&self, _ingress: ContextIngress) -> AgentResult<()> {
        Ok(())
    }
    async fn maintain(
        &self,
        _trigger: ContextMaintenanceTrigger,
    ) -> AgentResult<ContextMaintenanceReport> {
        Ok(ContextMaintenanceReport::default())
    }
    async fn materialize(&self, _query: ContextQuery) -> AgentResult<MaterializedContext> {
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
    async fn open_scope(&self, _kind: ScopeKind, _parent: Option<ScopeId>) -> AgentResult<ScopeId> {
        Ok(ScopeId::new())
    }
    async fn close_scope(
        &self,
        _scope_id: ScopeId,
    ) -> AgentResult<Vec<agent_contracts::ContextStateTransition>> {
        Ok(Vec::new())
    }
    async fn diagnostics(&self) -> AgentResult<ContextDiagnostics> {
        Ok(ContextDiagnostics::default())
    }
    async fn inspect(&self, _limit: usize) -> AgentResult<Vec<ContextItemSummary>> {
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
struct TestModel;

#[async_trait]
impl ModelTransport for TestModel {
    fn capabilities(&self) -> ModelCapabilities {
        ModelCapabilities::default()
    }
    async fn complete(&self, _request: ModelRequest) -> AgentResult<ModelOutput> {
        unreachable!("restore fencing test never reaches the model")
    }
}

#[derive(Debug)]
struct TestTools;

#[async_trait]
impl ToolDispatcher for TestTools {
    fn specs(&self) -> Vec<ToolSpec> {
        Vec::new()
    }
    async fn execute(&self, _request: ToolExecutionRequest) -> AgentResult<ToolOutcome> {
        unreachable!("restore fencing test never executes a tool")
    }
}

struct RecoveredOperationJournal {
    recovery: OperationJournalRecovery,
    seq: AtomicU64,
    transitions: Mutex<Vec<OperationJournalTransition>>,
}

impl OperationJournal for RecoveredOperationJournal {
    fn append_and_sync(
        &self,
        transition: &OperationJournalTransition,
    ) -> AgentResult<OperationJournalRecord> {
        let seq = self.seq.fetch_add(1, Ordering::AcqRel) + 1;
        self.transitions.lock().unwrap().push(transition.clone());
        Ok(OperationJournalRecord {
            version: OPERATION_JOURNAL_VERSION,
            seq,
            transition: transition.clone(),
        })
    }

    fn recover(&self) -> AgentResult<OperationJournalRecovery> {
        Ok(self.recovery.clone())
    }
}

struct FailAtOperationJournal {
    recovery: OperationJournalRecovery,
    seq: AtomicU64,
    fail_at: AtomicU64,
    transitions: Mutex<Vec<OperationJournalTransition>>,
}

impl OperationJournal for FailAtOperationJournal {
    fn append_and_sync(
        &self,
        transition: &OperationJournalTransition,
    ) -> AgentResult<OperationJournalRecord> {
        let seq = self.seq.fetch_add(1, Ordering::AcqRel) + 1;
        if self.fail_at.load(Ordering::Acquire) == seq {
            return Err(AgentError::Storage(
                "injected operation authority journal failure".into(),
            ));
        }
        self.transitions.lock().unwrap().push(transition.clone());
        Ok(OperationJournalRecord {
            version: OPERATION_JOURNAL_VERSION,
            seq,
            transition: transition.clone(),
        })
    }

    fn recover(&self) -> AgentResult<OperationJournalRecovery> {
        Ok(self.recovery.clone())
    }
}

#[derive(Debug, Default)]
struct OneToolModel {
    rounds: AtomicUsize,
}

#[async_trait]
impl ModelTransport for OneToolModel {
    fn capabilities(&self) -> ModelCapabilities {
        ModelCapabilities {
            tool_calls: true,
            ..ModelCapabilities::default()
        }
    }

    async fn complete(&self, _request: ModelRequest) -> AgentResult<ModelOutput> {
        if self.rounds.fetch_add(1, Ordering::SeqCst) == 0 {
            Ok(ModelOutput {
                content: String::new(),
                tool_calls: vec![ToolCall {
                    id: "late-value-call".into(),
                    name: "test.late_value".into(),
                    arguments: serde_json::json!({}),
                }],
                usage: Default::default(),
            })
        } else {
            Ok(ModelOutput {
                content: "done".into(),
                tool_calls: Vec::new(),
                usage: Default::default(),
            })
        }
    }
}

/// Deliberately violates cooperative cancellation: its result arrives
/// only after the test releases it, even if Runtime cancelled its token.
#[derive(Debug)]
struct LateValueTool {
    entered: Arc<tokio::sync::Notify>,
    release: Arc<tokio::sync::Notify>,
    request_identity: Option<Arc<Mutex<Option<ToolOperationIdentity>>>>,
    dispatch_completed: Option<Arc<tokio::sync::Notify>>,
    risk: ToolRisk,
}

#[async_trait]
impl ToolDispatcher for LateValueTool {
    fn specs(&self) -> Vec<ToolSpec> {
        vec![ToolSpec {
            name: "test.late_value".into(),
            description: "returns a late read-only value".into(),
            input_schema: serde_json::json!({"type": "object"}),
            risk: self.risk,
            output_budget: None,
            roles: Vec::new(),
        }]
    }

    async fn execute(&self, request: ToolExecutionRequest) -> AgentResult<ToolOutcome> {
        if let Some(identity) = &self.request_identity {
            *identity.lock().unwrap() = request
                .effect_context
                .as_ref()
                .map(|context| context.identity.clone());
        }
        self.entered.notify_one();
        self.release.notified().await;
        if let Some(completed) = &self.dispatch_completed {
            completed.notify_one();
        }
        Ok(ToolOutcome::Value(ToolOutput {
            call_id: request.call.id,
            tool_name: request.call.name,
            ok: true,
            summary: "late value".into(),
            model_content: "late value".into(),
            artifact_ref: None,
            metadata: serde_json::Value::Null,
        }))
    }
}

fn checkpoint(run_id: RunId) -> RuntimeCheckpoint {
    RuntimeCheckpoint {
        version: RUNTIME_CHECKPOINT_VERSION,
        run_metadata: RunMetadata {
            run_id,
            created_at_ms: 0,
        },
        tasks: TaskManagerSnapshot {
            tasks: Vec::new(),
            active: None,
            completed: Vec::new(),
        },
        current_task_id: None,
        focus_revision: 0,
        last_surface_revision: 0,
        context: serde_json::Value::Null,
        capabilities: Vec::new(),
        authority: None,
        snapshot_sequence: 0,
    }
}

async fn process_command(actor: &mut RuntimeActor, command: RuntimeCommand) {
    let (op_tx, _op_rx) = mpsc::channel(1);
    actor.process(command, &op_tx).await;
}

#[test]
fn actor_starts_fenced_when_core_recovery_is_unresolved() {
    let effect_id = EffectId::new();
    let operation_id = OperationId::new();
    let journal = Arc::new(RecoveredOperationJournal {
        recovery: OperationJournalRecovery {
            authority_epoch: 4,
            operations: vec![OperationSnapshot {
                identity: ToolOperationIdentity {
                    run_id: RunId::new(),
                    task_id: None,
                    turn_id: TurnId::new(),
                    scope_id: None,
                    operation_id,
                    generation: 4,
                    call_id: "generic-process".into(),
                    tool_name: "process.run".into(),
                    argument_digest: ArgumentDigest::sha256_bytes(b"process args"),
                },
                state: OperationState::Executing {
                    effect_id: Some(effect_id),
                },
            }],
            ..OperationJournalRecovery::default()
        },
        seq: AtomicU64::new(0),
        transitions: Mutex::new(Vec::new()),
    });
    let services = Arc::new(
        RuntimeServices::try_new(
            CoreAuthorityConfig::default(),
            Arc::new(TestContext),
            Arc::new(TestModel),
            Arc::new(TestTools),
            Arc::new(PolicyApprovalGate::read_only()),
            None,
            crate::services::AuthorityRecoveryServices::new(journal, None),
        )
        .unwrap(),
    );
    let core = services.core_port();
    assert!(matches!(
        core.recovery_status(),
        AuthorityRecoveryStatus::RecoveryRequired { .. }
    ));

    let actor = RuntimeActor::new(core, services);
    assert!(actor.state.recovery_required);
    assert_eq!(actor.state.generation, 5);
}

#[tokio::test]
async fn operation_query_remains_available_behind_the_recovery_fence() {
    let effect_id = EffectId::new();
    let operation_id = OperationId::new();
    let identity = ToolOperationIdentity {
        run_id: RunId::new(),
        task_id: None,
        turn_id: TurnId::new(),
        scope_id: None,
        operation_id,
        generation: 4,
        call_id: "query-recovery".into(),
        tool_name: "process.run".into(),
        argument_digest: ArgumentDigest::sha256_bytes(b"query recovery"),
    };
    let journal = Arc::new(RecoveredOperationJournal {
        recovery: OperationJournalRecovery {
            authority_epoch: 4,
            operations: vec![OperationSnapshot {
                identity: identity.clone(),
                state: OperationState::Executing {
                    effect_id: Some(effect_id),
                },
            }],
            ..OperationJournalRecovery::default()
        },
        seq: AtomicU64::new(0),
        transitions: Mutex::new(Vec::new()),
    });
    let services = Arc::new(
        RuntimeServices::try_new(
            CoreAuthorityConfig::default(),
            Arc::new(TestContext),
            Arc::new(TestModel),
            Arc::new(TestTools),
            Arc::new(PolicyApprovalGate::read_only()),
            None,
            crate::services::AuthorityRecoveryServices::new(journal, None),
        )
        .unwrap(),
    );
    let mut actor = RuntimeActor::new(services.core_port(), services);
    assert!(actor.state.recovery_required);

    let (query_tx, query_rx) = oneshot::channel();
    process_command(
        &mut actor,
        RuntimeCommand::QueryOperation {
            operation_id,
            reply: query_tx,
        },
    )
    .await;
    let OperationQueryResult::Found { snapshot } = query_rx.await.unwrap().unwrap() else {
        panic!("recovery tooling must retain exact operation truth")
    };
    assert_eq!(snapshot.identity, identity);

    let (cancel_tx, cancel_rx) = oneshot::channel();
    process_command(
        &mut actor,
        RuntimeCommand::CancelOperation {
            identity,
            reply: cancel_tx,
        },
    )
    .await;
    assert!(matches!(
        cancel_rx.await.unwrap(),
        Err(AgentError::RecoveryRequired(_))
    ));
}

#[tokio::test]
async fn prepared_restore_stays_fenced_and_unpublished_until_finalize() {
    let services = Arc::new(RuntimeServices::new(
        CoreAuthorityConfig::default(),
        Arc::new(TestContext),
        Arc::new(TestModel),
        Arc::new(TestTools),
        Arc::new(PolicyApprovalGate::read_only()),
        None,
    ));
    let core = services.core_port();
    let mut events = core.event_sender().subscribe();
    let run_id = core.run_id();
    let mut actor = RuntimeActor::new(core, services);

    let (prepare_tx, prepare_rx) = oneshot::channel();
    process_command(
        &mut actor,
        RuntimeCommand::PrepareRestore {
            checkpoint: checkpoint(run_id),
            reply: prepare_tx,
        },
    )
    .await;
    let restore_id = prepare_rx.await.unwrap().unwrap();

    assert!(actor.state.recovery_required);
    assert!(actor.state.pending_restore.is_some());
    assert!(events.try_recv().is_err(), "prepare must publish no event");

    let (mutation_tx, mutation_rx) = oneshot::channel();
    process_command(
        &mut actor,
        RuntimeCommand::SetFocus {
            goal: "must remain fenced".into(),
            reply: mutation_tx,
        },
    )
    .await;
    assert!(matches!(
        mutation_rx.await.unwrap(),
        Err(AgentError::RecoveryRequired(_))
    ));

    // A newer prepare replaces the pending attempt. Its token prevents
    // a delayed finalize from the old caller from committing/unfencing
    // this newer actor state.
    let (new_prepare_tx, new_prepare_rx) = oneshot::channel();
    process_command(
        &mut actor,
        RuntimeCommand::PrepareRestore {
            checkpoint: checkpoint(run_id),
            reply: new_prepare_tx,
        },
    )
    .await;
    let new_restore_id = new_prepare_rx.await.unwrap().unwrap();
    assert_ne!(restore_id, new_restore_id);

    let (stale_tx, stale_rx) = oneshot::channel();
    process_command(
        &mut actor,
        RuntimeCommand::FinalizeRestore {
            restore_id,
            capabilities_applied: true,
            reply: stale_tx,
        },
    )
    .await;
    assert!(matches!(
        stale_rx.await.unwrap(),
        Err(AgentError::InvalidRequest(_))
    ));
    assert!(actor.state.recovery_required);
    assert_eq!(
        actor
            .state
            .pending_restore
            .as_ref()
            .map(|pending| pending.restore_id),
        Some(new_restore_id)
    );
    assert!(
        events.try_recv().is_err(),
        "stale finalize publishes nothing"
    );

    let (finalize_tx, finalize_rx) = oneshot::channel();
    process_command(
        &mut actor,
        RuntimeCommand::FinalizeRestore {
            restore_id: new_restore_id,
            capabilities_applied: true,
            reply: finalize_tx,
        },
    )
    .await;
    finalize_rx.await.unwrap().unwrap();
    assert!(!actor.state.recovery_required);
    assert!(actor.state.pending_restore.is_none());
    let restored = events.try_recv().expect("finalize publishes restore event");
    assert!(matches!(
        restored.event,
        RuntimeEvent::RuntimeRestored {
            capabilities_applied: true,
            ..
        }
    ));
}

#[tokio::test]
async fn actor_epoch_mismatch_poison_is_fail_closed() {
    let services = Arc::new(RuntimeServices::new(
        CoreAuthorityConfig::default(),
        Arc::new(TestContext),
        Arc::new(TestModel),
        Arc::new(TestTools),
        Arc::new(PolicyApprovalGate::read_only()),
        None,
    ));
    let core = services.core_port();
    let mut actor = RuntimeActor::new(core.clone(), services);
    let actor_epoch = actor.state.generation;
    core.advance_authority_epoch(actor_epoch).unwrap();

    assert!(matches!(
        actor.bump_generation(),
        Err(AgentError::RecoveryRequired(_))
    ));
    assert!(actor.state.recovery_required);
    assert_eq!(actor.state.generation, actor_epoch);

    let (mutation_tx, mutation_rx) = oneshot::channel();
    process_command(
        &mut actor,
        RuntimeCommand::SetFocus {
            goal: "must remain fenced".into(),
            reply: mutation_tx,
        },
    )
    .await;
    assert!(matches!(
        mutation_rx.await.unwrap(),
        Err(AgentError::RecoveryRequired(_))
    ));
}

#[tokio::test]
async fn cancelled_late_tool_value_stays_cancelled_in_core_operation_truth() {
    let entered = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let services = Arc::new(RuntimeServices::new(
        CoreAuthorityConfig::default(),
        Arc::new(TestContext),
        Arc::new(OneToolModel::default()),
        Arc::new(LateValueTool {
            entered: entered.clone(),
            release: release.clone(),
            request_identity: None,
            dispatch_completed: None,
            risk: ToolRisk::ReadOnly,
        }),
        Arc::new(PolicyApprovalGate::read_only()),
        None,
    ));
    let core = services.core_port();
    let (handle, task) = spawn_runtime(services);
    let mut events = handle.subscribe();
    handle.start().await.unwrap();
    handle
        .user_message("run the late tool".into())
        .await
        .unwrap();

    tokio::time::timeout(Duration::from_secs(2), entered.notified())
        .await
        .expect("tool did not enter execution");
    let cancelled_operation = match handle.cancel_turn().await.unwrap() {
        TurnCancelAck::Cancelled {
            operation_id: Some(operation_id),
            ..
        } => operation_id,
        acknowledgement => {
            panic!("active tool cancellation must name its operation: {acknowledgement:?}")
        }
    };
    let OperationQueryResult::Found { snapshot } = core.query_operation(cancelled_operation) else {
        panic!("the cancelled operation must remain queryable after turn cancellation")
    };
    assert!(matches!(
        snapshot.state,
        OperationState::Terminal {
            effect_id: None,
            terminal: OperationTerminal::CancelledBeforeCommit,
        }
    ));

    release.notify_one();
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let envelope = events.recv().await.unwrap();
            if matches!(
                envelope.event,
                RuntimeEvent::Warning { ref message }
                    if message.contains("stale tool result dropped")
            ) {
                break;
            }
        }
    })
    .await
    .expect("actor did not process the late tool completion");

    let OperationQueryResult::Found { snapshot } = core.query_operation(cancelled_operation) else {
        panic!("the terminal operation must remain queryable")
    };
    assert!(matches!(
        snapshot.state,
        OperationState::Terminal {
            effect_id: None,
            terminal: OperationTerminal::CancelledBeforeCommit,
        }
    ));

    handle.stop().await.unwrap();
    task.await.unwrap();
}

#[tokio::test]
async fn precise_operation_cancel_rejects_identity_drift_and_returns_core_truth() {
    let entered = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let observed_identity = Arc::new(Mutex::new(None));
    let services = Arc::new(RuntimeServices::new(
        CoreAuthorityConfig::default(),
        Arc::new(TestContext),
        Arc::new(OneToolModel::default()),
        Arc::new(LateValueTool {
            entered: entered.clone(),
            release: release.clone(),
            request_identity: Some(observed_identity.clone()),
            dispatch_completed: None,
            risk: ToolRisk::WorkspaceWrite,
        }),
        Arc::new(PolicyApprovalGate::permissive()),
        None,
    ));
    let (handle, task) = spawn_runtime(services);
    handle.start().await.unwrap();
    handle
        .user_message("run the precise-cancel tool".into())
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(2), entered.notified())
        .await
        .expect("tool did not enter execution");
    let identity = observed_identity
        .lock()
        .unwrap()
        .clone()
        .expect("Core must pass the admitted operation identity to the dispatcher");

    let OperationQueryResult::Found { snapshot } =
        handle.query_operation(identity.operation_id).await.unwrap()
    else {
        panic!("the active operation must be queryable")
    };
    assert_eq!(snapshot.identity, identity);
    assert!(matches!(
        snapshot.state,
        OperationState::Executing { effect_id: Some(_) }
    ));

    let mut drifted = identity.clone();
    drifted.call_id.push_str("-forged");
    assert!(matches!(
        handle.cancel_operation(drifted).await,
        Err(AgentError::InvalidRequest(_))
    ));
    assert!(matches!(
        handle.query_operation(identity.operation_id).await.unwrap(),
        OperationQueryResult::Found { ref snapshot }
            if matches!(snapshot.state, OperationState::Executing { .. })
    ));

    let OperationQueryResult::Found { snapshot } =
        handle.cancel_operation(identity.clone()).await.unwrap()
    else {
        panic!("successful precise cancellation must return retained Core truth")
    };
    assert_eq!(snapshot.identity, identity);
    assert!(matches!(
        snapshot.state,
        OperationState::Terminal {
            effect_id: Some(_),
            terminal: OperationTerminal::CancelledBeforeCommit,
        }
    ));

    assert!(matches!(
        handle.cancel_operation(identity).await,
        Err(AgentError::InvalidRequest(_))
    ));
    release.notify_one();
    handle.stop().await.unwrap();
    task.await.unwrap();
}

#[tokio::test]
async fn operation_cancel_losing_to_a_core_terminal_returns_truth_without_fencing() {
    let entered = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let dispatch_completed = Arc::new(tokio::sync::Notify::new());
    let observed_identity = Arc::new(Mutex::new(None));
    let services = Arc::new(RuntimeServices::new(
        CoreAuthorityConfig::default(),
        Arc::new(TestContext),
        Arc::new(OneToolModel::default()),
        Arc::new(LateValueTool {
            entered: entered.clone(),
            release: release.clone(),
            request_identity: Some(observed_identity.clone()),
            dispatch_completed: Some(dispatch_completed.clone()),
            risk: ToolRisk::WorkspaceWrite,
        }),
        Arc::new(PolicyApprovalGate::permissive()),
        None,
    ));
    let core = services.core_port();
    let mut actor = RuntimeActor::new(core.clone(), services);
    let (op_tx, mut op_rx) = mpsc::channel(1);

    let (focus_tx, focus_rx) = oneshot::channel();
    actor
        .process(
            RuntimeCommand::SetFocus {
                goal: "late cancel race".into(),
                reply: focus_tx,
            },
            &op_tx,
        )
        .await;
    focus_rx.await.unwrap().unwrap();
    let (message_tx, message_rx) = oneshot::channel();
    actor
        .process(
            RuntimeCommand::UserMessage {
                content: "run the late tool".into(),
                reply: message_tx,
            },
            &op_tx,
        )
        .await;
    message_rx.await.unwrap().unwrap();
    actor
        .on_operation_completed(op_rx.recv().await.unwrap(), &op_tx)
        .await;
    tokio::time::timeout(Duration::from_secs(2), entered.notified())
        .await
        .expect("tool did not enter execution");
    let identity = observed_identity
        .lock()
        .unwrap()
        .clone()
        .expect("side-effecting dispatch carries the admitted identity");

    release.notify_one();
    tokio::time::timeout(Duration::from_secs(2), dispatch_completed.notified())
        .await
        .expect("tool dispatch did not finish");
    let completion = op_rx.recv().await.expect("tool completion must be queued");
    core.cancel_operation(identity.clone()).unwrap();
    let epoch_before = actor.state.generation;

    let (cancel_tx, cancel_rx) = oneshot::channel();
    actor
        .process(
            RuntimeCommand::CancelOperation {
                identity: identity.clone(),
                reply: cancel_tx,
            },
            &op_tx,
        )
        .await;
    let OperationQueryResult::Found { snapshot } = cancel_rx.await.unwrap().unwrap() else {
        panic!("a cancellation race loser must receive retained Core truth")
    };
    assert_eq!(snapshot.identity, identity);
    assert!(matches!(
        snapshot.state,
        OperationState::Terminal {
            terminal: OperationTerminal::CancelledBeforeCommit,
            ..
        }
    ));
    assert_eq!(actor.state.generation, epoch_before);
    assert!(!actor.state.recovery_required);
    assert!(actor.state.turn.is_some());

    actor.on_operation_completed(completion, &op_tx).await;
}

#[tokio::test]
async fn partial_atomic_cancel_wal_failure_fences_actor_and_stays_queryable() {
    let entered = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let observed_identity = Arc::new(Mutex::new(None));
    let journal = Arc::new(FailAtOperationJournal {
        recovery: OperationJournalRecovery::default(),
        seq: AtomicU64::new(0),
        fail_at: AtomicU64::new(u64::MAX),
        transitions: Mutex::new(Vec::new()),
    });
    let services = Arc::new(
        RuntimeServices::try_new(
            CoreAuthorityConfig::default(),
            Arc::new(TestContext),
            Arc::new(OneToolModel::default()),
            Arc::new(LateValueTool {
                entered: entered.clone(),
                release: release.clone(),
                request_identity: Some(observed_identity.clone()),
                dispatch_completed: None,
                risk: ToolRisk::WorkspaceWrite,
            }),
            Arc::new(PolicyApprovalGate::permissive()),
            None,
            crate::services::AuthorityRecoveryServices::new(journal.clone(), None),
        )
        .unwrap(),
    );
    let core = services.core_port();
    let mut actor = RuntimeActor::new(core.clone(), services);
    let (op_tx, mut op_rx) = mpsc::channel(1);

    let (focus_tx, focus_rx) = oneshot::channel();
    actor
        .process(
            RuntimeCommand::SetFocus {
                goal: "atomic cancellation WAL fault".into(),
                reply: focus_tx,
            },
            &op_tx,
        )
        .await;
    focus_rx.await.unwrap().unwrap();
    let (message_tx, message_rx) = oneshot::channel();
    actor
        .process(
            RuntimeCommand::UserMessage {
                content: "run the tool".into(),
                reply: message_tx,
            },
            &op_tx,
        )
        .await;
    message_rx.await.unwrap().unwrap();
    actor
        .on_operation_completed(op_rx.recv().await.unwrap(), &op_tx)
        .await;
    tokio::time::timeout(Duration::from_secs(2), entered.notified())
        .await
        .expect("tool did not enter execution");
    let identity = observed_identity
        .lock()
        .unwrap()
        .clone()
        .expect("side-effecting dispatch carries the admitted identity");

    // The atomic cancel writes EpochAdvanced first and the exact
    // cancellation terminal second. Fail only the second record: Core
    // must keep its in-memory epoch/state unpublished and both layers
    // must become observably fenced.
    let next = journal.seq.load(Ordering::Acquire) + 2;
    journal.fail_at.store(next, Ordering::Release);
    let mut events = core.event_sender().subscribe();
    let generation_before = actor.state.generation;
    let (cancel_tx, cancel_rx) = oneshot::channel();
    actor
        .process(
            RuntimeCommand::CancelOperation {
                identity: identity.clone(),
                reply: cancel_tx,
            },
            &op_tx,
        )
        .await;
    assert!(matches!(
        cancel_rx.await.unwrap(),
        Err(AgentError::RecoveryRequired(_))
    ));
    assert!(actor.state.recovery_required);
    assert_eq!(actor.state.generation, generation_before);
    assert_eq!(core.current_authority_epoch(), generation_before);
    assert!(matches!(
        core.recovery_status(),
        AuthorityRecoveryStatus::RecoveryRequired { .. }
    ));
    assert!(matches!(
        core.query_operation(identity.operation_id),
        OperationQueryResult::Found { ref snapshot }
            if matches!(snapshot.state, OperationState::Executing { .. })
    ));
    assert!(matches!(
        tokio::time::timeout(Duration::from_secs(1), events.recv())
            .await
            .unwrap()
            .unwrap()
            .event,
        RuntimeEvent::RecoveryRequired
    ));

    release.notify_one();
    let completion = tokio::time::timeout(Duration::from_secs(2), op_rx.recv())
        .await
        .expect("late tool completion timed out")
        .expect("late tool completion channel closed");
    actor.on_operation_completed(completion, &op_tx).await;
}
