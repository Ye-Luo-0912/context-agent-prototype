use std::sync::{
    Arc, Mutex,
    atomic::{AtomicU64, Ordering},
};

use agent_contracts::{
    AgentError, AgentResult, ArgumentDigest, AuthorityLease, AuthorityRecoveryStatus,
    CONTEXT_SEARCH_MAX_LIMIT, CONTEXT_SEARCH_MAX_QUERY_CHARS, CancellationToken,
    ContextConsumptionAck, ContextEngine, ContextItemId, ContextMaintenanceTrigger,
    ContextResidency, ContextSearchQuery, DiscoveryMiss, EffectDurability, EffectId,
    EffectReconciler, EffectReconciliation, EngineQuery, OperationEffectContext, OperationId,
    OperationQueryResult, OperationSnapshot, OperationState, OperationTerminal, OutputBroker,
    ResourceDescriptor, RunId, RuntimeEvent, TaskId, ToolCall, ToolDispatcher,
    ToolExecutionRequest, ToolOperationIdentity, ToolOutcome, ToolOutput, ToolRisk,
    ToolSurfaceSnapshot, context_maintenance_events,
};

use crate::authority::{
    ApprovalAuthority, ApprovalVerdict, EffectAuthority, EventAuthority, OutputAuthority,
};
use crate::operation::{
    DEFAULT_OPERATION_REGISTRY_CAPACITY, OperationCancelTransition, OperationRegistry,
};

/// Default commit-time lease window for one side-effecting tool call (ACI
/// v2 §6): how long after approval a staged effect may still be committed.
/// Short by design — a tool computation that overruns this window is
/// rolled back at commit time instead of mutating the world.
pub const DEFAULT_LEASE_TTL_MS: u64 = 120_000;
/// Shadow authorization is observational in V1. It must never hold an
/// accepted operation forever or consume the bounded operation registry.
const SHADOW_VERDICT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

#[derive(Clone)]
pub struct CoreAuthorityConfig {
    pub system_prompt: String,
    pub context_budget_tokens: usize,
    pub max_tool_rounds: usize,
    /// Optional trusted output broker: bounds every model-facing field of a
    /// tool result and spills oversized content to an artifact before the
    /// `ToolOutcome` reaches the actor. `None` keeps the runtime's last-line
    /// truncation (bounded but no artifact spill).
    pub output_broker: Option<Arc<dyn OutputBroker>>,
    /// Optional v2 shadow gate (ACI v2 compatibility order step 4): the
    /// intent-derived verdict is computed beside the legacy approval gate
    /// and published as a `ShadowDecision` event, never enforced. `None`
    /// keeps the legacy approval path exactly as before.
    pub shadow_gate: Option<Arc<dyn agent_contracts::IntentShadowGate>>,
    /// Commit-time lease window in milliseconds (ACI v2 §6). `None` uses
    /// `DEFAULT_LEASE_TTL_MS`. A side-effecting call's lease expires this
    /// long after approval; the actor refuses to commit a staged effect
    /// whose lease has expired and rolls it back instead.
    pub lease_ttl_ms: Option<u64>,
    /// 宿主授权映射：组合根在此安装内置实现与已准入的插件绑定，并把
    /// 同一来源交给审批门。缺省时一切意图按声明风险的空界限推导，
    /// 永远匹配不到授权。
    pub host_policies: Option<Arc<dyn agent_contracts::HostToolPolicies>>,
    /// Reserved/dispatch/ack barrier for brokerable effects. `None`
    /// installs the local broker, which preserves today's inline commit
    /// behavior exactly while structuring the three phases for a future
    /// HTTP/gRPC coordinator.
    pub effect_broker: Option<Arc<dyn crate::port::EffectBroker>>,
}

impl std::fmt::Debug for CoreAuthorityConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CoreAuthorityConfig")
            .field("system_prompt", &self.system_prompt)
            .field("context_budget_tokens", &self.context_budget_tokens)
            .field("max_tool_rounds", &self.max_tool_rounds)
            .field("output_broker", &"<output broker>")
            .field("shadow_gate", &"<shadow gate>")
            .field("lease_ttl_ms", &self.lease_ttl_ms)
            .field("host_policies", &"<host policies>")
            .field("effect_broker", &"<effect broker>")
            .finish()
    }
}

impl Default for CoreAuthorityConfig {
    fn default() -> Self {
        Self {
            system_prompt: agent_contracts::DEFAULT_CODING_AGENT_SYSTEM_PROMPT.to_string(),
            context_budget_tokens: 24_000,
            max_tool_rounds: 16,
            output_broker: None,
            shadow_gate: None,
            lease_ttl_ms: None,
            host_policies: None,
            effect_broker: None,
        }
    }
}

/// The runtime's executor: stateless primitives over the engine contracts
/// (context, model, tools) plus the four authority seams (events, approval,
/// effects, output) and the event plumbing. The execution *state machine*
/// (turn frame, generation, what to commit) lives in the runtime actor —
/// this type owns no turn state and no locks for it. The authorities are
/// the durability seam: every event, approval verdict, effect commit/rollback
/// and bounded producer output passes through one named home, so a future
/// Trusted Core can replace the seam without rewriting the facade or the
/// actor.
pub(crate) struct CoreAuthority {
    run_id: RunId,
    /// Process-lifetime authority epoch. Runtime asks Core to advance this
    /// fence, but cannot choose or restore an older value. The recoverable
    /// operation journal that persists it across process restarts is its own follow-up slice.
    authority_epoch: AtomicU64,
    /// Linearizes epoch changes with operation dispatch/commit admission.
    /// The guard is never held across an async tool/effect body.
    authority_gate: Mutex<()>,
    operations: OperationRegistry,
    config: CoreAuthorityConfig,
    context: Arc<dyn ContextEngine>,
    tools: Arc<dyn ToolDispatcher>,
    event: EventAuthority,
    approval: ApprovalAuthority,
    effect: EffectAuthority,
    output: OutputAuthority,
    broker: Arc<dyn crate::port::EffectBroker>,
}

impl CoreAuthority {
    #[cfg(test)]
    pub(crate) fn new(
        config: CoreAuthorityConfig,
        context: Arc<dyn ContextEngine>,
        tools: Arc<dyn ToolDispatcher>,
        approval: Arc<dyn agent_contracts::ApprovalGate>,
        journal: Option<Arc<dyn agent_contracts::EventJournal>>,
        operation_journal: Option<Arc<dyn agent_contracts::OperationJournal>>,
    ) -> Self {
        Self::try_new(
            config,
            context,
            tools,
            approval,
            journal,
            operation_journal,
            None,
        )
        .expect("in-memory Core construction cannot fail")
    }

    pub(crate) fn try_new(
        config: CoreAuthorityConfig,
        context: Arc<dyn ContextEngine>,
        tools: Arc<dyn ToolDispatcher>,
        approval: Arc<dyn agent_contracts::ApprovalGate>,
        journal: Option<Arc<dyn agent_contracts::EventJournal>>,
        operation_journal: Option<Arc<dyn agent_contracts::OperationJournal>>,
        effect_reconciler: Option<Arc<dyn EffectReconciler>>,
    ) -> AgentResult<Self> {
        let (event_tx, _) = tokio::sync::broadcast::channel(1_024);
        let seq = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let output_broker = config.output_broker.clone();
        let shadow_gate = config.shadow_gate.clone();
        let approval = ApprovalAuthority::new(approval);
        let approval = if let Some(shadow) = shadow_gate {
            approval.with_shadow(shadow)
        } else {
            approval
        };
        let operation_recovery = match &operation_journal {
            Some(journal) => journal.recover()?,
            None => agent_contracts::OperationJournalRecovery::default(),
        };
        let recovered_epoch = operation_recovery.authority_epoch;
        let operations = OperationRegistry::recover(
            DEFAULT_OPERATION_REGISTRY_CAPACITY,
            operation_journal.clone(),
            operation_recovery,
        )?;
        let authority_epoch = if operation_journal.is_some() {
            let next = recovered_epoch.checked_add(1).ok_or_else(|| {
                AgentError::RecoveryRequired("Core authority epoch is exhausted".into())
            })?;
            operations.persist_epoch_advance(recovered_epoch, next)?;
            next
        } else {
            recovered_epoch
        };
        // Fence old actors first, then fold only exact, durable effect
        // evidence. Unknown outcomes remain queryable and install a global
        // mutation fence; Core never schedules or blindly replays them.
        let broker = config
            .effect_broker
            .clone()
            .unwrap_or_else(|| Arc::new(crate::port::LocalEffectBroker));
        reconcile_recovered_operations(
            &operations,
            effect_reconciler.as_deref(),
            Some(broker.as_ref()),
        );
        Ok(Self {
            run_id: RunId::new(),
            authority_epoch: AtomicU64::new(authority_epoch),
            authority_gate: Mutex::new(()),
            operations,
            config,
            context,
            tools,
            event: EventAuthority::new(journal, event_tx, seq),
            approval,
            effect: EffectAuthority,
            output: OutputAuthority::new(output_broker),
            broker,
        })
    }

    pub(crate) fn run_id(&self) -> RunId {
        self.run_id
    }

    pub(crate) fn current_authority_epoch(&self) -> u64 {
        self.authority_epoch.load(Ordering::Acquire)
    }

    pub(crate) fn recovery_status(&self) -> AuthorityRecoveryStatus {
        self.operations.recovery_status()
    }

    pub(crate) fn authority_checkpoint_marker(
        &self,
    ) -> AgentResult<Option<agent_contracts::AuthorityCheckpointMarker>> {
        let _gate = self.authority_gate.lock().expect("authority gate poisoned");
        self.operations.authority_checkpoint_marker()
    }

    pub(crate) fn validate_authority_checkpoint_marker(
        &self,
        expected: &agent_contracts::AuthorityCheckpointMarker,
    ) -> AgentResult<()> {
        let _gate = self.authority_gate.lock().expect("authority gate poisoned");
        self.operations
            .validate_authority_checkpoint_marker(expected)
    }

    pub(crate) fn compact_authority_journal(
        &self,
    ) -> AgentResult<Option<agent_contracts::AuthorityCheckpointMarker>> {
        let _gate = self.authority_gate.lock().expect("authority gate poisoned");
        self.operations.compact_authority_journal()
    }

    pub(crate) fn ensure_mutation_allowed(&self) -> AgentResult<()> {
        self.operations.ensure_mutation_allowed()
    }

    /// Advance the commit fence exactly once from the caller's observed
    /// epoch. This is authority state, not a turn scheduler: Runtime remains
    /// the only component deciding when a lifecycle transition is attempted.
    pub(crate) fn advance_authority_epoch(&self, expected: u64) -> AgentResult<u64> {
        let _gate = self.authority_gate.lock().expect("authority gate poisoned");
        self.ensure_mutation_allowed()?;
        let next = expected.checked_add(1).ok_or_else(|| {
            AgentError::RecoveryRequired("Core authority epoch is exhausted".into())
        })?;
        let actual = self.current_authority_epoch();
        if actual != expected {
            return Err(AgentError::RecoveryRequired(format!(
                "Core authority epoch mismatch: Runtime expected {expected}, current epoch is {actual}"
            )));
        }
        self.operations.persist_epoch_advance(expected, next)?;
        self.authority_epoch.store(next, Ordering::Release);
        Ok(next)
    }

    pub(crate) fn query_operation(
        &self,
        operation_id: OperationId,
    ) -> agent_contracts::OperationQueryResult {
        self.operations.query(operation_id)
    }

    pub(crate) fn finish_value_operation_if_current(
        &self,
        expected_epoch: u64,
        operation_id: OperationId,
        argument_digest: ArgumentDigest,
    ) -> AgentResult<()> {
        let _gate = self.authority_gate.lock().expect("authority gate poisoned");
        self.ensure_mutation_allowed()?;
        let current = self.current_authority_epoch();
        if current != expected_epoch {
            return Err(AgentError::StaleEpoch {
                expected: expected_epoch,
                current,
            });
        }
        self.operations
            .finish_value(operation_id, argument_digest, expected_epoch)
    }

    pub(crate) fn cancel_operation(&self, identity: ToolOperationIdentity) -> AgentResult<()> {
        self.ensure_mutation_allowed()?;
        self.operations.cancel(identity)
    }

    pub(crate) fn cancel_operation_and_advance(
        &self,
        identity: ToolOperationIdentity,
        expected_epoch: u64,
    ) -> AgentResult<crate::port::OperationCancelDisposition> {
        identity.validate().map_err(AgentError::InvalidRequest)?;
        let _gate = self.authority_gate.lock().expect("authority gate poisoned");
        self.ensure_mutation_allowed()?;
        let actual = self.current_authority_epoch();
        if actual != expected_epoch {
            return Err(AgentError::RecoveryRequired(format!(
                "Core authority epoch mismatch: Runtime expected {expected_epoch}, current epoch is {actual}"
            )));
        }

        let next = expected_epoch.checked_add(1).ok_or_else(|| {
            AgentError::RecoveryRequired("Core authority epoch is exhausted".into())
        })?;
        match self
            .operations
            .cancel_and_persist_epoch(identity, expected_epoch, next)?
        {
            OperationCancelTransition::Cancelled(result) => {
                self.authority_epoch.store(next, Ordering::Release);
                Ok(crate::port::OperationCancelDisposition::Cancelled {
                    effective_epoch: next,
                    result,
                })
            }
            OperationCancelTransition::AlreadySettled(result) => Ok(
                crate::port::OperationCancelDisposition::AlreadySettled(result),
            ),
        }
    }

    pub(crate) fn begin_operation_commit(
        &self,
        operation_id: OperationId,
        effect_id: EffectId,
        argument_digest: ArgumentDigest,
    ) -> AgentResult<()> {
        self.ensure_mutation_allowed()?;
        let agent_contracts::OperationQueryResult::Found { snapshot } =
            self.operations.query(operation_id)
        else {
            return Err(AgentError::InvalidRequest(format!(
                "unknown or expired operation {operation_id}"
            )));
        };
        if snapshot.identity.argument_digest != argument_digest {
            return Err(AgentError::InvalidRequest(format!(
                "operation {operation_id} argument digest does not match admission"
            )));
        }
        self.operations.begin_commit(operation_id, effect_id)
    }

    pub(crate) fn begin_operation_commit_if_current(
        &self,
        expected_epoch: u64,
        operation_id: OperationId,
        effect_id: EffectId,
        argument_digest: ArgumentDigest,
    ) -> AgentResult<()> {
        let _gate = self.authority_gate.lock().expect("authority gate poisoned");
        self.ensure_mutation_allowed()?;
        let current = self.current_authority_epoch();
        if current != expected_epoch {
            return Err(AgentError::StaleEpoch {
                expected: expected_epoch,
                current,
            });
        }
        self.begin_operation_commit(operation_id, effect_id, argument_digest)
    }

    fn mark_operation_executing_if_current(
        &self,
        expected_epoch: u64,
        operation_id: OperationId,
        effect_id: Option<EffectId>,
    ) -> AgentResult<()> {
        let _gate = self.authority_gate.lock().expect("authority gate poisoned");
        self.ensure_mutation_allowed()?;
        let current = self.current_authority_epoch();
        if current != expected_epoch {
            return Err(AgentError::StaleEpoch {
                expected: expected_epoch,
                current,
            });
        }
        self.operations.mark_executing(operation_id, effect_id)
    }

    pub(crate) fn finish_operation_effect(
        &self,
        operation_id: OperationId,
        receipt: &agent_contracts::EffectReceipt,
    ) -> AgentResult<()> {
        self.ensure_mutation_allowed()?;
        self.operations.finish_effect(operation_id, receipt)
    }

    /// Install Core's fail-closed mutation fence when an in-process effect
    /// cannot prove rollback settlement. Runtime still decides *when* to
    /// roll back; Core owns the authority fact that no later mutation may
    /// commit on top of unresolved preparation state.
    pub(crate) fn require_operation_recovery(&self, reason: impl AsRef<str>) {
        self.operations.require_recovery(reason);
    }

    pub(crate) fn abort_prepared_operation(
        &self,
        operation_id: OperationId,
        effect_id: EffectId,
        argument_digest: ArgumentDigest,
    ) -> AgentResult<()> {
        self.ensure_mutation_allowed()?;
        self.operations
            .abort_prepared(operation_id, effect_id, argument_digest)
    }

    pub(crate) fn issued_lease_matches(
        &self,
        operation_id: OperationId,
        lease: &AuthorityLease,
    ) -> bool {
        self.operations.issued_lease_matches(operation_id, lease)
    }

    /// 操作记录里的工具名：撤销围栏用它查当前绑定纪元，不信任提交方。
    pub(crate) fn operation_tool_name(&self, operation_id: OperationId) -> Option<String> {
        match self.operations.query(operation_id) {
            agent_contracts::OperationQueryResult::Found { snapshot } => {
                Some(snapshot.identity.tool_name)
            }
            _ => None,
        }
    }

    /// 工具当前准入绑定的纪元；None = 没有可围栏的准入绑定（内置
    /// 授权或从未准入）。
    pub(crate) fn current_binding_epoch(&self, tool_name: &str) -> Option<u64> {
        self.config
            .host_policies
            .as_ref()
            .and_then(|policies| policies.binding_epoch(tool_name))
    }

    /// The approval authority seam: policy verdicts.
    pub(crate) fn approval(&self) -> &ApprovalAuthority {
        &self.approval
    }

    /// The effect authority seam: commit/rollback of staged effects.
    pub(crate) fn effect(&self) -> &EffectAuthority {
        &self.effect
    }

    /// The broker seam: reserved/dispatch/ack barrier for brokerable
    /// effects. The local default preserves inline behavior.
    pub(crate) fn broker(&self) -> &Arc<dyn crate::port::EffectBroker> {
        &self.broker
    }

    /// The broadcast sender behind `subscribe`, for live event sinks.
    pub(crate) fn event_sender(
        &self,
    ) -> tokio::sync::broadcast::Sender<agent_contracts::RuntimeEventEnvelope> {
        self.event.sender()
    }

    /// Current durable journal cursor for a live-only model sink. The sink
    /// may repeat this value on `ModelDelta`, but cannot advance Core's
    /// journal sequence.
    pub(crate) fn event_sequence(&self) -> u64 {
        self.event.sequence_cursor()
    }

    pub(crate) async fn start(&self) -> AgentResult<()> {
        self.event
            .emit_batch_durable(
                self.run_id,
                vec![
                    RuntimeEvent::RunStarted,
                    RuntimeEvent::RuntimeCommitBarrier {
                        kind: agent_contracts::RuntimeCommitKind::RunStart,
                        checkpoint_sequence: None,
                    },
                ],
            )
            .await?;
        if let AuthorityRecoveryStatus::RecoveryRequired { reason } = self.recovery_status() {
            let prefix = "Core authority recovery is required: ";
            let max_reason_bytes =
                agent_contracts::MAX_OPERATION_DIAGNOSTIC_BYTES.saturating_sub(prefix.len());
            let reason = bound_utf8(&reason, max_reason_bytes);
            self.event
                .warning(self.run_id, format!("{prefix}{reason}"))
                .await?;
            self.event
                .emit(self.run_id, RuntimeEvent::RecoveryRequired)
                .await?;
        }
        Ok(())
    }

    pub(crate) async fn stop(&self) -> AgentResult<()> {
        self.event
            .emit(self.run_id, RuntimeEvent::RunCompleted)
            .await?;
        self.event.flush().await
    }

    /// Journal + broadcast one runtime event (the single write path).
    pub(crate) async fn emit_event(&self, event: RuntimeEvent) -> AgentResult<()> {
        self.event.emit(self.run_id, event).await
    }

    /// Journal + broadcast one runtime event *after a durability barrier*:
    /// the event is appended, then `flush()` guarantees every event
    /// appended before it (the channel is FIFO) has left the process before
    /// the event is broadcast. Used at the turn-commit boundary: a
    /// subscriber never sees `TurnCompleted` unless the mandatory state
    /// writes before it are durable. A failed barrier returns the error and
    /// broadcasts nothing — the caller fences the turn instead of claiming
    /// a commit that never landed.
    pub(crate) async fn emit_event_durable(&self, event: RuntimeEvent) -> AgentResult<()> {
        self.event.emit_durable(self.run_id, event).await
    }

    /// Append and flush one bounded runtime transaction before publishing
    /// any of its audit members.
    pub(crate) async fn emit_events_durable(&self, events: Vec<RuntimeEvent>) -> AgentResult<()> {
        self.event.emit_batch_durable(self.run_id, events).await
    }

    /// Surface a runtime-level warning through the normal event stream.
    pub(crate) async fn emit_warning(&self, message: String) -> AgentResult<()> {
        self.event.warning(self.run_id, message).await
    }

    /// Commit model-consumption reinforcement and its bounded audit record as
    /// one context transaction. If either the engine mutation or event append
    /// fails, restore the pre-ack checkpoint so GC never observes an
    /// unaudited/partial access stamp. Context scheduling (ingest, maintain,
    /// GC, scopes, focus) lives on `RuntimeServices`; this one stays on the
    /// kernel because it is an *authority transaction* — the access stamp
    /// plus its mandatory audit event commit or roll back together.
    pub(crate) async fn acknowledge_context_consumption(
        &self,
        ack: ContextConsumptionAck,
    ) -> AgentResult<()> {
        self.ensure_mutation_allowed()?;
        ack.validate()?;
        let checkpoint = self.context.checkpoint().await?;
        let result = async {
            self.context.acknowledge_consumption(ack.clone()).await?;
            self.emit_event(RuntimeEvent::ContextConsumed { ack }).await
        }
        .await;
        self.finish_context_transaction("acknowledge context consumption", checkpoint, result)
            .await
    }

    /// Resolve a tool's engine query (context.search/inspect/fetch) against
    /// the context engine and turn the engine's answer into the final tool
    /// output. Tools never touch the engine (invariant 3); the kernel —
    /// which owns the `ContextEngine` — services the query. The placeholder
    /// `output` (call id, tool name) is preserved; only the content is
    /// replaced. Errors become a failed output so the model learns the
    /// query did not land.
    pub(crate) async fn resolve_engine_query(
        &self,
        output: ToolOutput,
        query: EngineQuery,
    ) -> ToolOutput {
        let mut output = output;
        match query {
            EngineQuery::SearchExternal {
                query,
                kind,
                scope,
                task_id,
                label,
                limit,
            } => {
                // 查询上限在执行期强制，而不只依赖 JSON schema：恶意或过期的
                // limit 在到达引擎前就被钳制，模型永远无法要求无界命中集。
                // 0 表示保持引擎默认值。
                let limit = limit.min(CONTEXT_SEARCH_MAX_LIMIT);
                // 自由文本查询同样有硬上限：超长查询在到达引擎前按字符截断，
                // 避免模型用巨型查询字符串冲刷检索路径。
                let query: String = query.chars().take(CONTEXT_SEARCH_MAX_QUERY_CHARS).collect();
                // 空结果区分无过滤与带过滤，方便换条件；不在这里写工作集说明书。
                let has_filter =
                    kind.is_some() || scope.is_some() || task_id.is_some() || label.is_some();
                let search = ContextSearchQuery {
                    query,
                    kind,
                    scope,
                    task_id,
                    label,
                    limit,
                };
                match self.context.search_external(search).await {
                    Ok(hits) if hits.is_empty() => {
                        output.ok = true;
                        output.summary = "no catalog items match".into();
                        output.model_content = if has_filter {
                            "context.search: nothing matches within the requested filter.".into()
                        } else {
                            "context.search: no catalog items match.".into()
                        };
                        output.metadata = serde_json::json!({
                            "op": "search",
                            "kind": "context",
                            "descriptors": [],
                        });
                    }
                    Ok(hits) => {
                        output.ok = true;
                        output.summary = format!("{} catalog hit(s)", hits.len());
                        // 命中行只报事实：source / residency。下一步由 residency
                        // 自己表达，不在工具输出里写操作说明书。
                        output.model_content = hits
                            .iter()
                            .map(|entry| {
                                let path = entry
                                    .file_path
                                    .as_deref()
                                    .filter(|path| !path.is_empty())
                                    .map(|path| format!(" path={path}"))
                                    .unwrap_or_default();
                                format!(
                                    "{} | kind={:?} scope={:?} task={} source={}{path} residency={:?} | {}\n  tags: {}\n  entities: {}",
                                    entry.context_ref.uri,
                                    entry.kind,
                                    entry.scope,
                                    entry
                                        .task_id
                                        .map(|t| t.to_string())
                                        .unwrap_or_else(|| "-".into()),
                                    entry
                                        .source
                                        .as_deref()
                                        .unwrap_or("-"),
                                    entry.residency,
                                    entry.context_ref.summary,
                                    if entry.tags.is_empty() {
                                        "-".to_string()
                                    } else {
                                        entry
                                            .tags
                                            .iter()
                                            .map(|tag| tag.as_str().to_string())
                                            .collect::<Vec<_>>()
                                            .join(", ")
                                    },
                                    entry.entities.join(", "),
                                )
                            })
                            .collect::<Vec<_>>()
                            .join("\n");
                        output.metadata = serde_json::json!({
                            "op": "search",
                            "kind": "context",
                            "descriptors": hits
                                .iter()
                                .map(ResourceDescriptor::from_context)
                                .collect::<Vec<_>>(),
                        });
                    }
                    Err(error) => {
                        output.ok = false;
                        output.summary = "context.search failed".into();
                        output.model_content = format!("context.search failed: {error}");
                        output.metadata = DiscoveryMiss::ProviderUnavailable {
                            reason: error.to_string(),
                        }
                        .to_metadata();
                    }
                }
            }
            EngineQuery::InspectExternal { item_id } => {
                match self.context.inspect_external(item_id).await {
                    Ok(Some(entry)) => {
                        output.ok = true;
                        output.summary = "external ref metadata".into();
                        // inspect 是元数据视图：来源权威（source）与
                        // residency/semantic 并列展示，None 显示 "-"。
                        let path = entry
                            .file_path
                            .as_deref()
                            .filter(|path| !path.is_empty())
                            .map(|path| format!(" path={path}"))
                            .unwrap_or_default();
                        output.model_content = format!(
                            "{} | kind={:?} scope={:?} task={} source={}{path} residency={:?} semantic={:?}\nsummary: {}\ntags: {}\nentities: {}",
                            entry.context_ref.uri,
                            entry.kind,
                            entry.scope,
                            entry
                                .task_id
                                .map(|t| t.to_string())
                                .unwrap_or_else(|| "-".into()),
                            entry.source.as_deref().unwrap_or("-"),
                            entry.residency,
                            entry.semantic,
                            entry.context_ref.summary,
                            if entry.tags.is_empty() {
                                "-".to_string()
                            } else {
                                entry
                                    .tags
                                    .iter()
                                    .map(|tag| tag.as_str().to_string())
                                    .collect::<Vec<_>>()
                                    .join(", ")
                            },
                            entry.entities.join(", "),
                        );
                        output.metadata = serde_json::json!({
                            "op": "inspect",
                            "kind": "context",
                            "descriptor": ResourceDescriptor::from_context(&entry),
                        });
                    }
                    Ok(None) => {
                        let miss = context_inspect_miss(self.context.as_ref(), item_id).await;
                        output.ok = true;
                        output.summary = "no such catalog item".into();
                        output.model_content = match &miss {
                            DiscoveryMiss::EvidenceAbsent { reason } => format!(
                                "context.inspect: item {item_id} is not current evidence ({reason})."
                            ),
                            _ => {
                                format!("context.inspect: no catalog item with id {item_id}.")
                            }
                        };
                        output.metadata = miss.to_metadata();
                    }
                    Err(error) => {
                        output.ok = false;
                        output.summary = "context.inspect failed".into();
                        output.model_content = format!("context.inspect failed: {error}");
                        output.metadata = DiscoveryMiss::ProviderUnavailable {
                            reason: error.to_string(),
                        }
                        .to_metadata();
                    }
                }
            }
            EngineQuery::FetchExternal { item_id } => {
                match self.context.fetch_external(item_id).await {
                    Ok(Some(item)) => {
                        output.ok = true;
                        output.summary = "external item fetched".into();
                        // fetch 返回完整条目：来源权威（source）进头部行，
                        // None 显示 "-"；正文仍经 output.bound 截断 + spill。
                        let path = item
                            .file_path
                            .as_deref()
                            .filter(|path| !path.is_empty())
                            .map(|path| format!(" | path={path}"))
                            .unwrap_or_default();
                        output.model_content = format!(
                            "[{:?} | {:?} | id={} | source={}{path}]\n{}",
                            item.kind,
                            item.scope,
                            item.id,
                            item.source.as_deref().unwrap_or("-"),
                            item.content
                        );
                    }
                    Ok(None) => {
                        if let Ok(Some(entry)) = self.context.inspect_external(item_id).await
                            && matches!(
                                entry.residency,
                                ContextResidency::Resident | ContextResidency::Warm
                            )
                        {
                            output.ok = true;
                            output.summary = "item in catalog, not stored".into();
                            output.model_content = format!(
                                "context.fetch: item {item_id} is {:?}; body lives in the catalog, not the store. Catalog residency is not the selected working set.",
                                entry.residency
                            );
                            output.metadata = serde_json::json!({
                                "op": "fetch",
                                "kind": "context",
                                "descriptor": ResourceDescriptor::from_context(&entry),
                            });
                        } else {
                            let miss = context_inspect_miss(self.context.as_ref(), item_id).await;
                            output.ok = true;
                            output.summary = "no such external ref".into();
                            output.model_content = match &miss {
                                DiscoveryMiss::EvidenceAbsent { reason } => format!(
                                    "context.fetch: item {item_id} is not current evidence ({reason})."
                                ),
                                _ => format!(
                                    "context.fetch: no stored item with id {item_id} (it may have been deleted by storage GC)."
                                ),
                            };
                            output.metadata = miss.to_metadata();
                        }
                    }
                    Err(error) => {
                        output.ok = false;
                        output.summary = "context.fetch failed".into();
                        output.model_content = format!("context.fetch failed: {error}");
                        output.metadata = DiscoveryMiss::ProviderUnavailable {
                            reason: error.to_string(),
                        }
                        .to_metadata();
                    }
                }
            }
        }
        // Context fetches can return large stored content; the output
        // authority bounds it and spills the full item to an artifact before
        // the model ever sees a truncated middle. No tool spec applies here
        // (the result comes from the engine query path), so the global cap
        // rules.
        self.output.bound(self.run_id, None, output).await
    }

    /// Validate and durably register one logical tool operation. The Core
    /// authority gate keeps the current epoch stable through the WAL-first
    /// `Accepted` append. Only that newly appended record receives a linear
    /// dispatch permit; an exact retry can observe existing truth but cannot
    /// dispatch twice.
    pub(crate) fn admit_tool_operation(
        &self,
        identity: ToolOperationIdentity,
        call: &ToolCall,
        generation: u64,
    ) -> AgentResult<crate::port::ToolOperationAdmission> {
        identity.validate().map_err(AgentError::InvalidRequest)?;
        let _gate = self.authority_gate.lock().expect("authority gate poisoned");
        self.ensure_mutation_allowed()?;
        let current_epoch = self.current_authority_epoch();
        if identity.run_id != self.run_id
            || identity.generation != generation
            || identity.call_id != call.id
            || identity.tool_name != call.name
            || identity.argument_digest != ArgumentDigest::from_json(&call.arguments)
        {
            return Err(AgentError::InvalidRequest(
                "tool admission rejected: operation identity does not match the request".into(),
            ));
        }
        if generation != current_epoch {
            // Idempotent observation remains available after the epoch has
            // advanced, but a stale generation can never append a new
            // Accepted record. The snapshot is intentionally read-only and
            // carries no dispatch permit.
            if let OperationQueryResult::Found { snapshot } =
                self.operations.query(identity.operation_id)
            {
                if snapshot.identity == identity {
                    return Ok(crate::port::ToolOperationAdmission::AlreadyKnown { snapshot });
                }
                return Err(AgentError::InvalidRequest(format!(
                    "operation {} was reused with a different identity or argument digest",
                    identity.operation_id
                )));
            }
            return Err(AgentError::InvalidRequest(format!(
                "tool admission rejected by the Core authority fence: operation epoch {generation}, current epoch {current_epoch}"
            )));
        }
        match self.operations.accept(identity.clone())? {
            crate::operation::OperationAdmission::Accepted => {
                let snapshot = OperationSnapshot {
                    identity: identity.clone(),
                    state: OperationState::Accepted,
                };
                Ok(crate::port::ToolOperationAdmission::Accepted {
                    snapshot: Box::new(snapshot),
                    permit: crate::port::AdmittedToolPermit { identity },
                })
            }
            crate::operation::OperationAdmission::Duplicate(snapshot) => {
                Ok(crate::port::ToolOperationAdmission::AlreadyKnown { snapshot })
            }
        }
    }

    /// Consume one Core-issued admission permit and publish the two lifecycle
    /// events that must precede dispatch. Returning a distinct linear permit
    /// makes the ordering part of the public CorePort type contract rather
    /// than a convention trusted callers can accidentally bypass.
    pub(crate) async fn publish_tool_operation(
        &self,
        permit: crate::port::AdmittedToolPermit,
        call: &ToolCall,
    ) -> AgentResult<crate::port::PublishedToolPermit> {
        let identity = permit.identity;
        if identity.run_id != self.run_id
            || identity.call_id != call.id
            || identity.tool_name != call.name
            || identity.argument_digest != ArgumentDigest::from_json(&call.arguments)
        {
            let error = AgentError::InvalidRequest(
                "tool publication rejected: call does not match the admitted identity".into(),
            );
            return Err(self.cancel_after_publication_failure(
                &identity,
                "tool-publication preflight",
                error,
            ));
        }
        let snapshot = match self.operations.query(identity.operation_id) {
            OperationQueryResult::Found { snapshot }
                if snapshot.identity == identity && snapshot.state == OperationState::Accepted =>
            {
                snapshot
            }
            other => {
                return Err(AgentError::RecoveryRequired(format!(
                    "tool publication rejected: admitted operation {} is no longer exactly Accepted ({other:?})",
                    identity.operation_id
                )));
            }
        };
        if let Err(error) = self
            .emit_event(RuntimeEvent::OperationAccepted { snapshot })
            .await
        {
            return Err(self.cancel_after_publication_failure(
                &identity,
                "OperationAccepted",
                error,
            ));
        }
        if let Err(error) = self
            .emit_event(RuntimeEvent::ToolStarted { call: call.clone() })
            .await
        {
            return Err(self.cancel_after_publication_failure(&identity, "ToolStarted", error));
        }
        Ok(crate::port::PublishedToolPermit { identity })
    }

    fn cancel_after_publication_failure(
        &self,
        identity: &ToolOperationIdentity,
        event: &str,
        event_error: AgentError,
    ) -> AgentError {
        match self.cancel_operation(identity.clone()) {
            Ok(()) => event_error,
            Err(cancel_error) => AgentError::RecoveryRequired(format!(
                "{event} publication failed for operation {}, and Core could not durably terminalize the undispatched operation: event={event_error}; cancellation={cancel_error}",
                identity.operation_id
            )),
        }
    }

    /// Consume one Core-issued publication permit, validate the call against
    /// the round's tool surface (the same surface the model saw and the
    /// budget used), run approval, mint any commit-time authority lease, and
    /// dispatch. Emits nothing — Runtime commits lifecycle events.
    pub(crate) async fn execute_published_tool(
        &self,
        permit: crate::port::PublishedToolPermit,
        call: ToolCall,
        cancel: CancellationToken,
        surface: &ToolSurfaceSnapshot,
    ) -> crate::port::CoreToolExecution {
        let identity = permit.identity;
        let generation = identity.generation;
        let argument_digest = identity.argument_digest;
        let refused = |outcome| crate::port::CoreToolExecution {
            outcome,
            lease: None,
            effect_id: None,
            argument_digest,
            value_completion_pending: false,
            recovery_required: None,
        };
        if let Err(error) = self.ensure_mutation_allowed() {
            return refused(ToolOutcome::Value(tool_error_output(
                &call,
                error.to_string(),
            )));
        }
        if identity.run_id != self.run_id
            || identity.call_id != call.id
            || identity.tool_name != call.name
            || identity.argument_digest != ArgumentDigest::from_json(&call.arguments)
        {
            let message =
                "tool dispatch rejected: operation identity does not match the admitted request"
                    .to_string();
            if let Err(error) = self
                .operations
                .finish_refused(identity.operation_id, &message)
            {
                return refused(ToolOutcome::Value(tool_error_output(
                    &call,
                    error.to_string(),
                )));
            }
            return refused(ToolOutcome::Value(tool_error_output(&call, message)));
        }
        let current_epoch = self.current_authority_epoch();
        if generation != current_epoch {
            let _ = self.operations.cancel(identity.clone());
            return refused(ToolOutcome::Value(tool_error_output(
                &call,
                format!(
                    "tool dispatch rejected by the Core authority fence: operation epoch {generation}, current epoch {current_epoch}"
                ),
            )));
        }
        let spec = surface
            .specs
            .iter()
            .find(|spec| spec.name == call.name)
            .cloned();
        let Some(spec) = spec else {
            let message = tool_not_on_surface_message(&call, surface);
            if let Err(error) = self
                .operations
                .finish_refused(identity.operation_id, &message)
            {
                return refused(ToolOutcome::Value(tool_error_output(
                    &call,
                    error.to_string(),
                )));
            }
            return refused(ToolOutcome::Value(tool_error_output(&call, message)));
        };

        let verdict = self.approval.authorize(&call, &spec, &cancel).await;
        let legacy_allowed = matches!(verdict, ApprovalVerdict::Allowed);

        match verdict {
            ApprovalVerdict::Allowed => {}
            ApprovalVerdict::Denied(message) | ApprovalVerdict::Failed(message) => {
                if let Err(error) = self
                    .operations
                    .finish_refused(identity.operation_id, &message)
                {
                    return refused(ToolOutcome::Value(tool_error_output(
                        &call,
                        error.to_string(),
                    )));
                }
                return refused(ToolOutcome::Value(tool_error_output(&call, message)));
            }
        }

        // Shadow mode (ACI v2 step 4): the v2 intent-derived verdict is
        // computed once and reused for the audit event and the lease's
        // covering grant, so the comparison and the lease can never drift.
        // Best-effort observability: a failed journal append must not turn
        // a granted call into an error.
        let shadow = if self.approval.has_shadow() {
            tokio::select! {
                _ = cancel.cancelled() => {
                    let _ = self.operations.cancel(identity.clone());
                    return refused(ToolOutcome::Value(tool_error_output(
                        &call,
                        "tool dispatch cancelled while evaluating shadow authorization".into(),
                    )));
                }
                result = tokio::time::timeout(
                    SHADOW_VERDICT_TIMEOUT,
                    self.approval.shadow_verdict(&call, &spec),
                ) => match result {
                    Ok(shadow) => shadow,
                    Err(_) => {
                        let _ = self.emit_warning(format!(
                            "shadow authorization for {} exceeded {:?}; legacy approval remains authoritative",
                            call.name, SHADOW_VERDICT_TIMEOUT,
                        )).await;
                        None
                    }
                }
            }
        } else {
            None
        };
        if let Some(shadow) = &shadow {
            let _ = self
                .emit_event(RuntimeEvent::ShadowDecision {
                    call_name: call.name.clone(),
                    legacy_allowed,
                    shadow: shadow.clone(),
                })
                .await;
        }

        // Approval can await user or policy work while Runtime concurrently
        // cancels the operation and advances the Core-owned epoch. Recheck at
        // the last common point before lease minting and dispatch so an
        // operation cancelled during approval never starts afterward.
        let current_epoch = self.current_authority_epoch();
        if generation != current_epoch {
            let _ = self.operations.cancel(identity.clone());
            return refused(ToolOutcome::Value(tool_error_output(
                &call,
                format!(
                    "tool dispatch rejected after approval because operation epoch {generation} is stale; current Core epoch is {current_epoch}"
                ),
            )));
        }

        // Mint the short-lived authority lease (ACI v2 §6) for
        // side-effecting calls before dispatch: the approved concrete
        // intent, the covering grant (when the v2 shadow gate granted it),
        // and a bounded TTL. The lease travels with the operation; the
        // actor validates it again at commit time — stale generation or
        // expiry means rollback, never commit — so an operation that
        // overran its authorization window cannot mutate the world.
        // Read-only calls carry no lease: there is no commit to enforce.
        let lease = if spec.risk != ToolRisk::ReadOnly {
            let grant_id = match &shadow {
                Some(agent_contracts::ShadowVerdict::Granted { grant_id, .. }) => {
                    Some(grant_id.clone())
                }
                _ => None,
            };
            let issued_at_ms = now_ms();
            let ttl = self.config.lease_ttl_ms.unwrap_or(DEFAULT_LEASE_TTL_MS);
            let lease = AuthorityLease {
                lease_id: format!("lease-{}", RunId::new()),
                operation_id: identity.operation_id,
                argument_digest: identity.argument_digest,
                operation_generation: generation,
                intent: match &self.config.host_policies {
                    Some(policies) => policies.effect_intent(&call, &spec),
                    None => agent_contracts::unbound_effect_intent(&spec),
                },
                grant_id,
                decision: agent_contracts::ApprovalDecision::Allow,
                issued_at_ms,
                expires_at_ms: issued_at_ms.saturating_add(ttl),
                // 记录本意图依据的策略版本（可审计）与当时该工具绑定
                // 的撤销纪元；提交期按后者做撤销围栏。
                policy_revision: self
                    .config
                    .host_policies
                    .as_ref()
                    .and_then(|policies| policies.policy_revision()),
                binding_epoch: self
                    .config
                    .host_policies
                    .as_ref()
                    .and_then(|policies| policies.binding_epoch(&call.name)),
            };
            if let Err(error) = self
                .operations
                .record_lease(identity.operation_id, lease.clone())
            {
                return refused(ToolOutcome::Value(tool_error_output(
                    &call,
                    error.to_string(),
                )));
            }
            Some(lease)
        } else {
            None
        };

        // Reserve the recovery identity before a side-effecting tool starts.
        // The Executing transition is WAL-first, so a broker can durably bind
        // staged evidence to the same id even if Core crashes before Prepared.
        let reserved_effect_id = (spec.risk != ToolRisk::ReadOnly).then(EffectId::new);
        if let Err(error) = self.mark_operation_executing_if_current(
            generation,
            identity.operation_id,
            reserved_effect_id,
        ) {
            let _ = self.operations.cancel(identity.clone());
            return refused(ToolOutcome::Value(tool_error_output(
                &call,
                error.to_string(),
            )));
        }

        if let Some(lease) = &lease {
            let _ = self
                .emit_event(RuntimeEvent::LeaseIssued {
                    lease_id: lease.lease_id.clone(),
                    call_name: call.name.clone(),
                    grant_id: lease.grant_id.clone(),
                    expires_at_ms: lease.expires_at_ms,
                })
                .await;
        }

        let request = ToolExecutionRequest {
            run_id: self.run_id,
            call: call.clone(),
            effect_context: reserved_effect_id.map(|effect_id| OperationEffectContext {
                identity: identity.clone(),
                effect_id,
            }),
            cancel,
        };
        if let Err(error) = request.validate() {
            let _ = self.operations.cancel(identity.clone());
            return refused(ToolOutcome::Value(tool_error_output(
                &call,
                format!("tool dispatch rejected: {error}"),
            )));
        }
        let (outcome, mut recovery_required) = match self.tools.execute(request).await {
            Ok(outcome) => (outcome, None),
            Err(error @ AgentError::RecoveryRequired(_)) => {
                let message = bound_utf8(
                    &error.to_string(),
                    agent_contracts::MAX_OPERATION_DIAGNOSTIC_BYTES,
                )
                .to_string();
                self.operations.require_recovery(&message);
                (
                    ToolOutcome::Value(tool_error_output(&call, message.clone())),
                    Some(message),
                )
            }
            Err(error) => (
                ToolOutcome::Value(tool_error_output(&call, error.to_string())),
                None,
            ),
        };

        let (outcome, effect_id, value_completion_pending) = match outcome {
            ToolOutcome::PreparedEffect { output, effect } => {
                let prepared = reserved_effect_id
                    .ok_or_else(|| {
                        AgentError::InvalidRequest(
                            "read-only operation unexpectedly returned a prepared effect".into(),
                        )
                    })
                    .and_then(|effect_id| {
                        self.operations
                            .mark_prepared(identity.operation_id, effect_id)
                            .map(|()| effect_id)
                    });
                match prepared {
                    Ok(effect_id) => (
                        ToolOutcome::PreparedEffect { output, effect },
                        Some(effect_id),
                        false,
                    ),
                    Err(error) => {
                        let rollback = self
                            .effect
                            .rollback(
                                effect,
                                &format!("operation registry rejected preparation: {error}"),
                            )
                            .await;
                        let terminal = if rollback.is_ok() {
                            self.operations.cancel(identity.clone())
                        } else {
                            Ok(())
                        };
                        if let Err(cleanup_error) = rollback.and(terminal) {
                            let message = bound_utf8(
                                &format!(
                                    "operation {} preparation rollback could not be confirmed: {cleanup_error}",
                                    identity.operation_id
                                ),
                                agent_contracts::MAX_OPERATION_DIAGNOSTIC_BYTES,
                            )
                            .to_string();
                            self.operations.require_recovery(&message);
                            recovery_required = Some(message);
                        }
                        (
                            ToolOutcome::Value(tool_error_output(&call, error.to_string())),
                            None,
                            false,
                        )
                    }
                }
            }
            outcome => (outcome, None, true),
        };

        // Trusted output authority: bound every model-facing field and spill
        // oversized content before the outcome reaches the actor. Engine
        // queries carry a placeholder output here (their real content is
        // produced by `resolve_engine_query`, which bounds it there), so the
        // authority runs on all four variants uniformly. The declaring
        // tool's own budget (`ToolSpec::output_budget`) is enforced here — a
        // verbose tool spills sooner, a quiet tool never exceeds its cap.
        let budget = spec.output_budget;
        let outcome = match outcome {
            ToolOutcome::Value(output) => {
                ToolOutcome::Value(self.output.bound(self.run_id, budget, output).await)
            }
            ToolOutcome::PreparedEffect { output, effect } => ToolOutcome::PreparedEffect {
                output: self.output.bound(self.run_id, budget, output).await,
                effect,
            },
            ToolOutcome::RuntimeDirective { output, directive } => ToolOutcome::RuntimeDirective {
                output: self.output.bound(self.run_id, budget, output).await,
                directive,
            },
            ToolOutcome::EngineQuery { output, query } => ToolOutcome::EngineQuery {
                output: self.output.bound(self.run_id, budget, output).await,
                query,
            },
        };
        crate::port::CoreToolExecution {
            outcome,
            lease,
            effect_id,
            argument_digest,
            value_completion_pending,
            recovery_required,
        }
    }

    /// Test-only composition of WAL-first admission and admitted execution.
    /// Production callers use `CorePort`'s split methods so no public facade
    /// can dispatch without first publishing the accepted identity.
    #[cfg(test)]
    pub(crate) async fn execute_tool(
        &self,
        identity: ToolOperationIdentity,
        call: ToolCall,
        cancel: CancellationToken,
        surface: &ToolSurfaceSnapshot,
        generation: u64,
    ) -> crate::port::CoreToolExecution {
        let argument_digest = identity.argument_digest;
        let refused = |message: String| crate::port::CoreToolExecution {
            outcome: ToolOutcome::Value(tool_error_output(&call, message)),
            lease: None,
            effect_id: None,
            argument_digest,
            value_completion_pending: false,
            recovery_required: None,
        };
        match self.admit_tool_operation(identity, &call, generation) {
            Ok(crate::port::ToolOperationAdmission::Accepted { permit, .. }) => {
                let permit = match self.publish_tool_operation(permit, &call).await {
                    Ok(permit) => permit,
                    Err(error) => return refused(error.to_string()),
                };
                self.execute_published_tool(permit, call, cancel, surface)
                    .await
            }
            Ok(crate::port::ToolOperationAdmission::AlreadyKnown { snapshot }) => refused(format!(
                "tool dispatch rejected: operation {} is already {:?}",
                snapshot.identity.operation_id, snapshot.state
            )),
            Err(error) => refused(error.to_string()),
        }
    }

    pub(crate) async fn emit_diagnostics(&self) -> AgentResult<()> {
        let diagnostics = self.context.diagnostics().await?;
        self.emit_event(RuntimeEvent::Diagnostics { diagnostics })
            .await
    }

    pub(crate) async fn checkpoint(&self) -> AgentResult<serde_json::Value> {
        // A fenced Core remains inspectable. Context maintenance is a
        // mutation, so recovery-mode checkpoints snapshot the current engine
        // exactly as-is and publish no maintenance claim.
        if matches!(
            self.recovery_status(),
            AuthorityRecoveryStatus::RecoveryRequired { .. }
        ) {
            return self.context.checkpoint().await;
        }
        let report = self
            .context
            .maintain(ContextMaintenanceTrigger::Checkpoint)
            .await?;
        for event in context_maintenance_events(ContextMaintenanceTrigger::Checkpoint, report) {
            self.emit_event(event).await?;
        }
        self.context.checkpoint().await
    }

    /// Restore and verify the context half of a runtime checkpoint before
    /// the actor exposes its task-table half. A fallible engine restore may
    /// partially mutate, so this uses the same snapshot rollback as focus
    /// transitions. Engine-owned `diagnostics.focus_task_id` is the
    /// authority check; `MaterializedContext.focus` is assembler-owned and
    /// left empty by production engines.
    pub(crate) async fn restore(
        &self,
        data: serde_json::Value,
        expected_task_id: Option<TaskId>,
    ) -> AgentResult<()> {
        self.ensure_mutation_allowed()?;
        let checkpoint = self.context.checkpoint().await?;
        let restored = async {
            // Stub engines persist `Null` and do not track focus. Production
            // engines persist a non-null state blob; after restore their
            // `diagnostics.focus_task_id` must match the runtime task.
            let check_focus = !data.is_null();
            self.context.restore(data).await?;
            if check_focus {
                let actual_task_id = self.context.diagnostics().await?.focus_task_id;
                if actual_task_id != expected_task_id {
                    return Err(AgentError::InvalidRequest(format!(
                        "checkpoint context focus {actual_task_id:?} does not match current task {expected_task_id:?}"
                    )));
                }
            }
            Ok(())
        }
        .await;
        self.finish_context_transaction("restore context", checkpoint, restored)
            .await
    }

    /// Complete an authority transaction. Context engines are replaceable
    /// and their mutation methods are fallible, so the kernel takes a
    /// portable checkpoint before a multi-step transition and restores it
    /// if the engine mutation fails. Task state is committed by the runtime
    /// actor only after the authority transaction returns `Ok`.
    async fn finish_context_transaction<T>(
        &self,
        operation: &'static str,
        checkpoint: serde_json::Value,
        result: AgentResult<T>,
    ) -> AgentResult<T> {
        match result {
            Ok(value) => Ok(value),
            Err(error) => match self.context.restore(checkpoint).await {
                Ok(()) => Err(error),
                Err(rollback_error) => Err(AgentError::RecoveryRequired(format!(
                    "{operation} failed ({error}); rollback failed ({rollback_error})"
                ))),
            },
        }
    }
}

/// Fold restart evidence after the durable epoch fence is published. This is
/// a finite pass over Core's bounded operation registry. It never invokes an
/// effect body and never retries a mutation.
fn reconcile_recovered_operations(
    operations: &OperationRegistry,
    reconciler: Option<&dyn EffectReconciler>,
    broker: Option<&dyn crate::port::EffectBroker>,
) {
    for snapshot in operations.recovered_snapshots() {
        let effect_id = match snapshot.state {
            OperationState::Terminal {
                terminal:
                    OperationTerminal::OutcomeUnknown { ref error }
                    | OperationTerminal::Applied {
                        durability: EffectDurability::DurabilityFailed(ref error),
                        ..
                    },
                ..
            } => {
                operations.require_recovery(format!(
                    "operation {} recovered terminal truth that still requires recovery: {error}",
                    snapshot.identity.operation_id
                ));
                continue;
            }
            OperationState::Terminal { .. } => continue,
            OperationState::Accepted | OperationState::Executing { effect_id: None } => {
                if let Err(error) = operations.recover_terminal(
                    snapshot.identity.operation_id,
                    &snapshot.state,
                    None,
                    OperationTerminal::CancelledBeforeCommit,
                ) {
                    operations.require_recovery(format!(
                        "startup could not terminalize operation {} before commit: {error}",
                        snapshot.identity.operation_id
                    ));
                    // A WAL failure is sticky. Do not consult or append any
                    // later recovery result after authority persistence has
                    // stopped accepting transitions.
                    break;
                }
                continue;
            }
            OperationState::Executing {
                effect_id: Some(effect_id),
            }
            | OperationState::Prepared { effect_id }
            | OperationState::CommitStarted { effect_id } => effect_id,
        };
        if !reconcile_recovered_effect(operations, reconciler, broker, snapshot, effect_id) {
            break;
        }
    }
    if let Some(reconciler) = reconciler
        && let Err(error) = reconciler.recover_orphans()
    {
        operations.require_recovery(format!(
            "startup could not contain leftover process trees: {error}"
        ));
    }
}

fn reconcile_recovered_effect(
    operations: &OperationRegistry,
    reconciler: Option<&dyn EffectReconciler>,
    broker: Option<&dyn crate::port::EffectBroker>,
    snapshot: OperationSnapshot,
    effect_id: EffectId,
) -> bool {
    let operation_id = snapshot.identity.operation_id;
    let context = OperationEffectContext {
        identity: snapshot.identity.clone(),
        effect_id,
    };
    // 工作区对账器不管理该效果时，先问经纪的持久预留面；两边都给
    // 不出证据才围栏。经纪分类直接复用同一对账枚举与校验。
    let result = match reconciler.map(|reconciler| reconciler.reconcile(&context)) {
        Some(Ok(EffectReconciliation::NotManaged)) | None => {
            match broker.map(|broker| broker.reconcile_reservation(&context)) {
                None | Some(Ok(None)) => {
                    if reconciler.is_none() {
                        operations.require_recovery(format!(
                            "operation {operation_id} has unresolved effect {effect_id} and no recovery adapter"
                        ));
                    } else {
                        operations.require_recovery(format!(
                            "operation {operation_id} effect {effect_id} is not managed by the configured recovery adapter"
                        ));
                    }
                    return true;
                }
                Some(Ok(Some(candidate))) => match candidate.validate() {
                    Ok(()) => candidate,
                    Err(error) => {
                        operations.require_recovery(format!(
                            "operation {operation_id} broker reconciliation returned invalid evidence: {error}"
                        ));
                        return true;
                    }
                },
                Some(Err(error)) => {
                    operations.require_recovery(format!(
                        "operation {operation_id} effect reconciliation failed: {error}"
                    ));
                    return true;
                }
            }
        }
        Some(other) => match other {
            Ok(result) => result,
            Err(error) => {
                operations.require_recovery(format!(
                    "operation {operation_id} effect reconciliation failed: {error}"
                ));
                return true;
            }
        },
    };
    let (terminal, keep_fenced) = match result {
        EffectReconciliation::NotManaged => {
            operations.require_recovery(format!(
                "operation {operation_id} effect {effect_id} is not managed by the configured recovery adapter"
            ));
            return true;
        }
        EffectReconciliation::Ambiguous { reason } => {
            operations.require_recovery(format!(
                "operation {operation_id} effect {effect_id} is ambiguous: {reason}"
            ));
            return true;
        }
        EffectReconciliation::NotApplied { evidence } => {
            let error = match evidence {
                Some(evidence) => {
                    format!("startup reconciliation proved the effect was not applied ({evidence})")
                }
                None => "startup reconciliation proved the effect was not applied".into(),
            };
            (OperationTerminal::NotApplied { error }, false)
        }
        EffectReconciliation::Applied {
            durability,
            evidence,
        } => {
            let early_application = matches!(
                snapshot.state,
                OperationState::Executing { .. } | OperationState::Prepared { .. }
            );
            let reconciler_reported_durability_failure =
                matches!(durability, EffectDurability::DurabilityFailed(_));
            let durability = if early_application {
                EffectDurability::DurabilityFailed(
                    "recovery evidence shows the effect applied before Core durably recorded CommitStarted"
                        .into(),
                )
            } else {
                durability
            };
            (
                OperationTerminal::Applied {
                    durability,
                    evidence,
                },
                early_application || reconciler_reported_durability_failure,
            )
        }
        EffectReconciliation::CompletedValue { evidence: _ } => {
            if !matches!(snapshot.state, OperationState::Executing { .. }) {
                operations.require_recovery(format!(
                    "operation {operation_id} effect {effect_id} reported CompletedValue from {:?}; that settlement is only valid from Executing for non-transactional process or remote tools",
                    snapshot.state
                ));
                return true;
            }
            (OperationTerminal::CompletedValue, false)
        }
    };
    if let Err(error) =
        operations.recover_terminal(operation_id, &snapshot.state, Some(effect_id), terminal)
    {
        operations.require_recovery(format!(
            "operation {operation_id} reconciliation could not be recorded: {error}"
        ));
        return false;
    }
    if keep_fenced {
        operations.require_recovery(format!(
            "operation {operation_id} effect {effect_id} applied before the Core commit boundary"
        ));
    }
    true
}

fn tool_error_output(call: &ToolCall, message: String) -> ToolOutput {
    let class = agent_contracts::failure_class_from_message(&message);
    let mut metadata = serde_json::json!({});
    agent_contracts::attach_failure_class(&mut metadata, class);
    ToolOutput {
        call_id: call.id.clone(),
        tool_name: call.name.clone(),
        ok: false,
        summary: message.clone(),
        model_content: format!("tool error: {message}"),
        artifact_ref: None,
        metadata,
    }
}

fn bound_utf8(value: &str, max_bytes: usize) -> &str {
    let mut end = value.len().min(max_bytes);
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    &value[..end]
}

/// Explain a rejected call strictly from the immutable surface the model
/// received. Looking at the live catalog here would race lifecycle changes
/// after capture and could misclassify a round-local omission; live authority
/// checks still happen inside the dispatcher for calls that are on-surface.
fn tool_not_on_surface_message(call: &ToolCall, surface: &ToolSurfaceSnapshot) -> String {
    let captured_surface = if surface.surface_revision == 0 {
        "this round's captured model surface".to_string()
    } else {
        format!("model surface revision {}", surface.surface_revision)
    };
    match surface
        .omissions
        .iter()
        .find(|omission| omission.tool_name == call.name)
    {
        Some(omission) => format!(
            "tool '{}' was not exposed on {captured_surface}: {}",
            call.name,
            omission.reason.as_str()
        ),
        None => format!(
            "tool '{}' was not exposed on {captured_surface}; only schemas in that captured surface may be called",
            call.name
        ),
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or_default()
}

/// Classify an inspect/fetch miss: a catalog peek that still shows a
/// terminal semantic is `evidence_absent`; a missing id is `not_found`.
/// Provider errors stay `provider_unavailable`. Inspect-by-id does not
/// take a revision yet, so `stale_revision` is not produced here.
async fn context_inspect_miss(
    context: &dyn ContextEngine,
    item_id: ContextItemId,
) -> DiscoveryMiss {
    match context.inspect(usize::MAX).await {
        Ok(summaries) => {
            if let Some(item) = summaries
                .iter()
                .find(|item| item.id == item_id && item.semantic.is_dead())
            {
                DiscoveryMiss::EvidenceAbsent {
                    reason: format!("{:?}", item.semantic),
                }
            } else {
                DiscoveryMiss::NotFound
            }
        }
        Err(error) => DiscoveryMiss::ProviderUnavailable {
            reason: error.to_string(),
        },
    }
}

#[cfg(test)]
mod tests;
