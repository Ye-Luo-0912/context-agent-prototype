use std::sync::Arc;

use agent_contracts::{
    AgentError, AgentResult, CONTEXT_SEARCH_MAX_LIMIT, CancellationToken, ContextConsumptionAck,
    ContextEngine, ContextGcReport, ContextIngress, ContextItemSummary, ContextKind,
    ContextMaintenanceReport, ContextMaintenanceTrigger, ContextQuery, ContextSearchQuery,
    ContextStateTransition, EngineQuery, FocusState, MaterializedContext, ModelCapabilities,
    ModelEventSink, ModelOutput, ModelRequest, ModelTransport, OutputBroker, RunId, RuntimeEvent,
    ScopeId, ScopeKind, TaskId, ToolCall, ToolCatalogEntry, ToolDispatcher, ToolExecutionRequest,
    ToolOutcome, ToolOutput, ToolSpec, ToolSurfaceSnapshot,
};

use crate::authority::{
    ApprovalAuthority, ApprovalVerdict, EffectAuthority, EventAuthority, OutputAuthority,
};

#[derive(Clone)]
pub struct AgentKernelConfig {
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
}

impl std::fmt::Debug for AgentKernelConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AgentKernelConfig")
            .field("system_prompt", &self.system_prompt)
            .field("context_budget_tokens", &self.context_budget_tokens)
            .field("max_tool_rounds", &self.max_tool_rounds)
            .field("output_broker", &"<output broker>")
            .field("shadow_gate", &"<shadow gate>")
            .finish()
    }
}

impl Default for AgentKernelConfig {
    fn default() -> Self {
        Self {
            system_prompt: concat!(
                "You are a focused coding agent. Work on the current task only. ",
                "Treat SELECTED WORKING CONTEXT as a bounded cache, not a complete transcript. ",
                "Use tools when needed. Do not assume omitted history is relevant."
            )
            .to_string(),
            context_budget_tokens: 24_000,
            max_tool_rounds: 16,
            output_broker: None,
            shadow_gate: None,
        }
    }
}

/// The runtime's executor: stateless primitives over the engine contracts
/// (context, model, tools) plus the four authority seams (events, approval,
/// effects, output) and the event plumbing. The execution *state machine*
/// (turn frame, generation, what to commit) lives in the runtime actor —
/// this type owns no turn state and no locks for it. The authorities are
/// the MOD-04 seam: every event, approval verdict, effect commit/rollback
/// and bounded producer output passes through one named home, so a future
/// Trusted Core can replace the seam without rewriting the facade or the
/// actor.
pub struct AgentKernel {
    run_id: RunId,
    config: AgentKernelConfig,
    context: Arc<dyn ContextEngine>,
    model: Arc<dyn ModelTransport>,
    tools: Arc<dyn ToolDispatcher>,
    event: EventAuthority,
    approval: ApprovalAuthority,
    effect: EffectAuthority,
    output: OutputAuthority,
}

impl AgentKernel {
    pub fn new(
        config: AgentKernelConfig,
        context: Arc<dyn ContextEngine>,
        model: Arc<dyn ModelTransport>,
        tools: Arc<dyn ToolDispatcher>,
        approval: Arc<dyn agent_contracts::ApprovalGate>,
        journal: Option<Arc<dyn agent_contracts::EventJournal>>,
    ) -> Self {
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
        Self {
            run_id: RunId::new(),
            config,
            context,
            model,
            tools,
            event: EventAuthority::new(journal, event_tx, seq),
            approval,
            effect: EffectAuthority,
            output: OutputAuthority::new(output_broker),
        }
    }

    pub fn run_id(&self) -> RunId {
        self.run_id
    }

    /// The event authority seam: identity, journaling, barriers.
    pub fn event(&self) -> &EventAuthority {
        &self.event
    }

    /// The approval authority seam: policy verdicts.
    pub fn approval(&self) -> &ApprovalAuthority {
        &self.approval
    }

    /// The effect authority seam: commit/rollback of staged effects.
    pub fn effect(&self) -> &EffectAuthority {
        &self.effect
    }

    /// The output authority seam: the broker path from producer to model.
    pub fn output(&self) -> &OutputAuthority {
        &self.output
    }

    pub fn subscribe(
        &self,
    ) -> tokio::sync::broadcast::Receiver<agent_contracts::RuntimeEventEnvelope> {
        self.event.subscribe()
    }

    /// The broadcast sender behind `subscribe`, for live event sinks.
    pub fn event_sender(
        &self,
    ) -> tokio::sync::broadcast::Sender<agent_contracts::RuntimeEventEnvelope> {
        self.event.sender()
    }

    /// The shared sequence counter, so live deltas and journaled events keep
    /// one consistent envelope order.
    pub fn seq(&self) -> Arc<std::sync::atomic::AtomicU64> {
        self.event.seq()
    }

    /// Configuration accessors the actor drives the turn loop with.
    pub fn system_prompt(&self) -> String {
        self.config.system_prompt.clone()
    }

    pub fn context_budget_tokens(&self) -> usize {
        self.config.context_budget_tokens
    }

    pub fn max_tool_rounds(&self) -> usize {
        self.config.max_tool_rounds
    }

    pub fn model_capabilities(&self) -> ModelCapabilities {
        self.model.capabilities()
    }

    pub fn tool_specs(&self) -> Vec<ToolSpec> {
        self.tools.specs()
    }

    /// Capture the tool surface for one model round: the runtime calls this
    /// once per round right after `tool_gc()`, then threads the snapshot
    /// through the budget, the prompt and tool-call validation so the model
    /// always sees — and the runtime always validates against — the same
    /// surface.
    pub fn tool_snapshot(&self) -> ToolSurfaceSnapshot {
        self.tools.snapshot()
    }

    /// Pure classification used by the runtime's final input guard. A true
    /// result permits a schema to be omitted from the current round only;
    /// it does not authorize unloading or any catalog lifecycle mutation.
    pub fn tool_may_omit_from_round(&self, name: &str) -> bool {
        self.tools.may_omit_from_round(name)
    }

    /// Run the tool lifecycle GC at a runtime safe point. `specs()` is pure;
    /// the actor ages the tool catalog exactly once per model round, before
    /// the surface is captured for the budget and the prompt.
    pub fn tool_gc(&self) {
        self.tools.gc();
    }

    /// Pure discovery state used by the runtime's Task requirement safe
    /// point. The dispatcher remains unaware of TaskManager/Focus policy.
    pub fn tool_catalog(&self) -> Vec<ToolCatalogEntry> {
        self.tools.catalog()
    }

    /// Re-activate one exact Task-rooted tool at the BeforeModel safe point.
    /// This is a lifecycle decision caused by a Task requirement, never by
    /// provider token pressure and never an authority/approval grant.
    pub fn tool_load(&self, name: &str) -> AgentResult<()> {
        self.tools.load_tool(name)
    }

    /// Explicitly unload an optional tool from the catalog surface. Provider
    /// input budgeting never calls this: budget pressure only omits schemas
    /// from its immutable per-round snapshot. Core tools refuse to unload.
    pub fn tool_unload(&self, name: &str) -> AgentResult<()> {
        self.tools.unload_tool(name)
    }

    pub async fn start(&self) -> AgentResult<()> {
        self.event.emit(self.run_id, RuntimeEvent::RunStarted).await
    }

    pub async fn stop(&self) -> AgentResult<()> {
        self.event
            .emit(self.run_id, RuntimeEvent::RunCompleted)
            .await?;
        self.event.flush().await
    }

    /// Journal + broadcast one runtime event (the single write path).
    pub async fn emit_event(&self, event: RuntimeEvent) -> AgentResult<()> {
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
    pub async fn emit_event_durable(&self, event: RuntimeEvent) -> AgentResult<()> {
        self.event.emit_durable(self.run_id, event).await
    }

    /// Surface a runtime-level warning through the normal event stream.
    pub async fn emit_warning(&self, message: String) -> AgentResult<()> {
        self.event.warning(self.run_id, message).await
    }

    /// Context primitives: the actor decides when they run.
    pub async fn context_ingest(&self, ingress: ContextIngress) -> AgentResult<()> {
        self.context.ingest(ingress).await
    }

    pub async fn context_maintain(
        &self,
        trigger: ContextMaintenanceTrigger,
    ) -> AgentResult<ContextMaintenanceReport> {
        self.context.maintain(trigger).await
    }

    /// Run a full GC pass (mark roots, sweep, reversible eviction). Called
    /// by the actor at turn boundaries; engines without a GC pass return an
    /// empty report.
    pub async fn context_gc(&self) -> AgentResult<ContextGcReport> {
        self.context.gc().await
    }

    /// Materialize the working set for one model request. The result is
    /// structured items; prompt assembly happens in the runtime actor.
    pub async fn context_materialize(
        &self,
        query: ContextQuery,
    ) -> AgentResult<MaterializedContext> {
        self.context.materialize(query).await
    }

    /// Commit model-consumption reinforcement and its bounded audit record as
    /// one context transaction. If either the engine mutation or event append
    /// fails, restore the pre-ack checkpoint so GC never observes an
    /// unaudited/partial access stamp.
    pub async fn acknowledge_context_consumption(
        &self,
        ack: ContextConsumptionAck,
    ) -> AgentResult<()> {
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

    /// Open a scope (runtime-driven, e.g. a tool scope at tool start).
    pub async fn context_open_scope(
        &self,
        kind: ScopeKind,
        parent: Option<ScopeId>,
    ) -> AgentResult<ScopeId> {
        self.context.open_scope(kind, parent).await
    }

    /// Close a scope the runtime opened; returns the close transitions.
    pub async fn context_close_scope(
        &self,
        scope_id: ScopeId,
    ) -> AgentResult<Vec<ContextStateTransition>> {
        self.context.close_scope(scope_id).await
    }

    /// Resolve a tool's engine query (context.search/inspect/fetch) against
    /// the context engine and turn the engine's answer into the final tool
    /// output. Tools never touch the engine (invariant 3); the kernel —
    /// which owns the `ContextEngine` — services the query. The placeholder
    /// `output` (call id, tool name) is preserved; only the content is
    /// replaced. Errors become a failed output so the model learns the
    /// query did not land.
    pub async fn resolve_engine_query(&self, output: ToolOutput, query: EngineQuery) -> ToolOutput {
        let mut output = output;
        match query {
            EngineQuery::SearchExternal {
                query,
                kind,
                scope,
                task_id,
                limit,
            } => {
                // Query limits are enforced in execution, not only by the
                // JSON schema: a hostile or stale limit is clamped before it
                // reaches the engine, so the model can never ask for an
                // unbounded hit set. 0 keeps the engine default.
                let limit = limit.min(CONTEXT_SEARCH_MAX_LIMIT);
                let search = ContextSearchQuery {
                    query,
                    kind,
                    scope,
                    task_id,
                    limit,
                };
                match self.context.search_external(search).await {
                    Ok(hits) if hits.is_empty() => {
                        output.ok = true;
                        output.summary = "no external refs match".into();
                        output.model_content =
                            "context.search: no externalized items match the query.".into();
                    }
                    Ok(hits) => {
                        output.ok = true;
                        output.summary = format!("{} external ref(s) match", hits.len());
                        output.model_content = hits
                            .iter()
                            .map(|entry| {
                                format!(
                                    "{} | kind={:?} scope={:?} task={} | {}\n  tags: {}\n  entities: {}",
                                    entry.context_ref.uri,
                                    entry.kind,
                                    entry.scope,
                                    entry
                                        .task_id
                                        .map(|t| t.to_string())
                                        .unwrap_or_else(|| "-".into()),
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
                    }
                    Err(error) => {
                        output.ok = false;
                        output.summary = "context.search failed".into();
                        output.model_content = format!("context.search failed: {error}");
                    }
                }
            }
            EngineQuery::InspectExternal { item_id } => {
                match self.context.inspect_external(item_id).await {
                    Ok(Some(entry)) => {
                        output.ok = true;
                        output.summary = "external ref metadata".into();
                        output.model_content = format!(
                            "{} | kind={:?} scope={:?} task={} residency={:?} semantic={:?}\nsummary: {}\ntags: {}\nentities: {}",
                            entry.context_ref.uri,
                            entry.kind,
                            entry.scope,
                            entry
                                .task_id
                                .map(|t| t.to_string())
                                .unwrap_or_else(|| "-".into()),
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
                    }
                    Ok(None) => {
                        output.ok = true;
                        output.summary = "no such external ref".into();
                        output.model_content =
                            format!("context.inspect: no externalized item with id {item_id}.");
                    }
                    Err(error) => {
                        output.ok = false;
                        output.summary = "context.inspect failed".into();
                        output.model_content = format!("context.inspect failed: {error}");
                    }
                }
            }
            EngineQuery::FetchExternal { item_id } => {
                match self.context.fetch_external(item_id).await {
                    Ok(Some(item)) => {
                        output.ok = true;
                        output.summary = "external item fetched".into();
                        output.model_content = format!(
                            "[{:?} | {:?} | id={}]\n{}",
                            item.kind, item.scope, item.id, item.content
                        );
                    }
                    Ok(None) => {
                        output.ok = true;
                        output.summary = "no such external ref".into();
                        output.model_content = format!(
                            "context.fetch: no externalized item with id {item_id} (it may have been deleted by storage GC)."
                        );
                    }
                    Err(error) => {
                        output.ok = false;
                        output.summary = "context.fetch failed".into();
                        output.model_content = format!("context.fetch failed: {error}");
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

    /// One model round: stream the request to the provider. The result is a
    /// value for the actor to validate and commit — nothing is committed
    /// here.
    pub async fn run_model_round(
        &self,
        request: ModelRequest,
        sink: &dyn ModelEventSink,
    ) -> AgentResult<ModelOutput> {
        self.model.complete_stream(request, sink).await
    }

    /// Execute one tool call: validate it against the round's tool surface
    /// snapshot (the same surface the model saw and the budget used), run
    /// approval, dispatch. Emits nothing — ToolStarted/ToolFinished are
    /// committed by the actor. Returns the tool outcome: either a plain
    /// value or a staged effect the actor commits/rolls back after the
    /// generation fence.
    pub async fn execute_tool(
        &self,
        call: ToolCall,
        cancel: CancellationToken,
        surface: &ToolSurfaceSnapshot,
    ) -> ToolOutcome {
        let spec = surface
            .specs
            .iter()
            .find(|spec| spec.name == call.name)
            .cloned();
        let Some(spec) = spec else {
            return ToolOutcome::Value(tool_error_output(
                &call,
                tool_not_on_surface_message(&call, surface),
            ));
        };

        let verdict = self.approval.authorize(&call, &spec, &cancel).await;
        let legacy_allowed = matches!(verdict, ApprovalVerdict::Allowed);

        // Shadow mode (ACI v2 step 4): record what the v2 intent-derived
        // gate would decide beside the legacy decision — for allowed and
        // denied calls alike, so the invariant trace (granted/denied/
        // reason) can be compared against the legacy path. Best-effort
        // observability: a failed journal append must not turn a granted
        // call into an error.
        if self.approval.has_shadow()
            && let Some(shadow) = self.approval.shadow_verdict(&call, &spec).await
        {
            let _ = self
                .emit_event(RuntimeEvent::ShadowDecision {
                    call_name: call.name.clone(),
                    legacy_allowed,
                    shadow,
                })
                .await;
        }

        match verdict {
            ApprovalVerdict::Allowed => {}
            ApprovalVerdict::Denied(message) | ApprovalVerdict::Failed(message) => {
                return ToolOutcome::Value(tool_error_output(&call, message));
            }
        }

        let outcome = match self
            .tools
            .execute(ToolExecutionRequest {
                run_id: self.run_id,
                call: call.clone(),
                cancel,
            })
            .await
        {
            Ok(outcome) => outcome,
            Err(error) => ToolOutcome::Value(tool_error_output(&call, error.to_string())),
        };

        // Trusted output authority: bound every model-facing field and spill
        // oversized content before the outcome reaches the actor. Engine
        // queries carry a placeholder output here (their real content is
        // produced by `resolve_engine_query`, which bounds it there), so the
        // authority runs on all four variants uniformly. The declaring
        // tool's own budget (`ToolSpec::output_budget`) is enforced here — a
        // verbose tool spills sooner, a quiet tool never exceeds its cap.
        let budget = spec.output_budget;
        match outcome {
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
        }
    }

    /// Switch the runtime's focus to a task's goal. The task id comes from
    /// the runtime's `TaskManager` — re-focusing an existing task resumes
    /// its scopes in the context engine (suspension/resume is keyed on the
    /// task id), while a fresh task id opens a fresh task scope.
    pub async fn set_focus(
        &self,
        task_id: TaskId,
        goal: String,
    ) -> AgentResult<ContextMaintenanceReport> {
        let checkpoint = self.context.checkpoint().await?;
        let focus = FocusState::for_task(task_id, goal.clone());
        let transition = async {
            self.context
                .ingest(ContextIngress::FocusChanged { focus })
                .await?;
            self.context
                .maintain(ContextMaintenanceTrigger::FocusChanged)
                .await
        }
        .await;
        self.finish_context_transaction("set focus", checkpoint, transition)
            .await
    }

    /// Suspend the current focus without completing the task: the engine
    /// clears its focus and suspends the active task's scopes, so a later
    /// `set_focus` with the same task id resumes them.
    pub async fn clear_focus(&self) -> AgentResult<ContextMaintenanceReport> {
        let checkpoint = self.context.checkpoint().await?;
        let transition = async {
            self.context.ingest(ContextIngress::FocusCleared).await?;
            self.context
                .maintain(ContextMaintenanceTrigger::FocusChanged)
                .await
        }
        .await;
        self.finish_context_transaction("clear focus", checkpoint, transition)
            .await
    }

    pub async fn pin(&self, content: String) -> AgentResult<ContextMaintenanceReport> {
        let checkpoint = self.context.checkpoint().await?;
        let transition = async {
            self.context
                .ingest(ContextIngress::Pin {
                    content,
                    kind: ContextKind::Constraint,
                })
                .await?;
            self.context
                .maintain(ContextMaintenanceTrigger::FocusChanged)
                .await
        }
        .await;
        self.finish_context_transaction("pin context", checkpoint, transition)
            .await
    }

    pub async fn complete_current_task(
        &self,
        task_id: TaskId,
        summary: String,
    ) -> AgentResult<ContextMaintenanceReport> {
        let checkpoint = self.context.checkpoint().await?;
        let transition = async {
            self.context
                .ingest(ContextIngress::TaskCompleted {
                    task_id: Some(task_id),
                    summary,
                })
                .await?;
            self.context
                .maintain(ContextMaintenanceTrigger::TaskCompleted)
                .await
        }
        .await;
        self.finish_context_transaction("complete task", checkpoint, transition)
            .await
    }

    /// Complete a context-only transaction. Context engines are replaceable
    /// and their mutation methods are fallible, so the stateless core takes a
    /// portable checkpoint before a multi-step transition and restores it if
    /// either ingest or maintenance fails. Task state is committed by the
    /// runtime actor only after this method returns `Ok`.
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

    pub async fn emit_diagnostics(&self) -> AgentResult<()> {
        let diagnostics = self.context.diagnostics().await?;
        self.emit_event(RuntimeEvent::Diagnostics { diagnostics })
            .await
    }

    pub async fn inspect_context(&self, limit: usize) -> AgentResult<Vec<ContextItemSummary>> {
        self.context.inspect(limit).await
    }

    pub async fn checkpoint(&self) -> AgentResult<serde_json::Value> {
        let report = self
            .context
            .maintain(ContextMaintenanceTrigger::Checkpoint)
            .await?;
        self.emit_event(RuntimeEvent::ContextMaintained {
            trigger: ContextMaintenanceTrigger::Checkpoint,
            report,
        })
        .await?;
        self.context.checkpoint().await
    }

    /// Restore and verify the context half of a runtime checkpoint before
    /// the actor exposes its task-table half. A fallible engine restore may
    /// partially mutate, so this uses the same snapshot rollback as focus
    /// transitions. The materialized focus is the contract-level authority
    /// check; callers cannot assume opaque context JSON has a matching task.
    pub async fn restore(
        &self,
        data: serde_json::Value,
        expected_task_id: Option<TaskId>,
    ) -> AgentResult<()> {
        let checkpoint = self.context.checkpoint().await?;
        let verification_restore = data.clone();
        let restored = async {
            self.context.restore(data).await?;
            let actual_task_id = self
                .context
                .materialize(ContextQuery {
                    current_input: String::new(),
                    budget_tokens: 0,
                    hints: agent_contracts::ContextHints {
                        max_selected_items: Some(0),
                    },
                })
                .await?
                .focus
                .map(|focus| focus.task_id);
            if actual_task_id != expected_task_id {
                return Err(AgentError::InvalidRequest(format!(
                    "checkpoint context focus {actual_task_id:?} does not match current task {expected_task_id:?}"
                )));
            }
            // `materialize` is the only implementation-agnostic way to read
            // focus today and may stamp access/tick metadata. Re-applying
            // the same replacement checkpoint removes that verification
            // observation, so restore remains an exact state replacement.
            self.context.restore(verification_restore).await?;
            Ok(())
        }
        .await;
        self.finish_context_transaction("restore context", checkpoint, restored)
            .await
    }
}

fn tool_error_output(call: &ToolCall, message: String) -> ToolOutput {
    ToolOutput {
        call_id: call.id.clone(),
        tool_name: call.name.clone(),
        ok: false,
        summary: message.clone(),
        model_content: format!("tool error: {message}"),
        artifact_ref: None,
        metadata: serde_json::Value::Null,
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use agent_contracts::{
        ApprovalDecision, ApprovalGate, AttentionState, ContextDiagnostics, ContextItem,
        ContextItemId, ContextResidency, ContextRetention, ContextScope, ExternalizedContext,
        SemanticState, ToolRisk, ToolSurfaceDemand, ToolSurfaceOmission, ToolSurfaceOmissionReason,
    };

    fn call(name: &str) -> ToolCall {
        ToolCall {
            id: "call-1".into(),
            name: name.into(),
            arguments: serde_json::json!({}),
        }
    }

    #[test]
    fn off_surface_rejection_uses_only_the_captured_omission_reason() {
        let surface = ToolSurfaceSnapshot {
            surface_revision: 9,
            omissions: vec![ToolSurfaceOmission {
                tool_name: "optional.large".into(),
                demand: ToolSurfaceDemand::PreferSurface,
                origin: agent_contracts::ToolSurfaceOrigin::CatalogLoadedOptional,
                reason: ToolSurfaceOmissionReason::ProviderInputBudget,
                approx_tokens: 2_500,
            }],
            omitted_total: 1,
            ..ToolSurfaceSnapshot::default()
        };

        let message = tool_not_on_surface_message(&call("optional.large"), &surface);

        assert!(message.contains("model surface revision 9"));
        assert!(message.contains("provider input budget"));
        assert!(!message.contains("capability.manage"));
        assert!(!message.contains("load"));
    }

    #[test]
    fn unrecorded_off_surface_call_is_rejected_without_live_catalog_claims() {
        let surface = ToolSurfaceSnapshot {
            surface_revision: 12,
            omitted_total: 7,
            ..ToolSurfaceSnapshot::default()
        };

        let message = tool_not_on_surface_message(&call("unlisted.tool"), &surface);

        assert!(message.contains("model surface revision 12"));
        assert!(message.contains("only schemas in that captured surface may be called"));
        assert!(!message.contains("unknown tool"));
        assert!(!message.contains("loaded"));
    }

    // --- CORE-04: trusted output broker + execution-enforced query limits ---

    #[derive(Default)]
    struct RecordingBroker {
        calls: std::sync::Mutex<usize>,
        last_output: std::sync::Mutex<Option<ToolOutput>>,
    }

    #[async_trait::async_trait]
    impl OutputBroker for RecordingBroker {
        async fn bound(
            &self,
            _run_id: RunId,
            _budget: Option<usize>,
            output: ToolOutput,
        ) -> ToolOutput {
            *self.calls.lock().unwrap() += 1;
            *self.last_output.lock().unwrap() = Some(output.clone());
            output
        }
    }

    struct BigOutputDispatcher {
        output: ToolOutput,
    }

    #[async_trait::async_trait]
    impl ToolDispatcher for BigOutputDispatcher {
        fn specs(&self) -> Vec<ToolSpec> {
            vec![ToolSpec {
                name: "big.tool".into(),
                description: "returns oversized output".into(),
                input_schema: serde_json::json!({}),
                risk: ToolRisk::ReadOnly,
                output_budget: None,
            }]
        }
        async fn execute(&self, request: ToolExecutionRequest) -> AgentResult<ToolOutcome> {
            assert_eq!(request.call.name, "big.tool");
            Ok(ToolOutcome::Value(self.output.clone()))
        }
    }

    struct AllowAllApproval;

    #[async_trait::async_trait]
    impl ApprovalGate for AllowAllApproval {
        async fn authorize(
            &self,
            _call: &ToolCall,
            _spec: &ToolSpec,
            _cancel: &CancellationToken,
        ) -> AgentResult<ApprovalDecision> {
            Ok(ApprovalDecision::Allow)
        }
    }

    struct UnusedModel;

    #[async_trait::async_trait]
    impl ModelTransport for UnusedModel {
        fn capabilities(&self) -> ModelCapabilities {
            ModelCapabilities::default()
        }
        async fn complete(&self, _request: ModelRequest) -> AgentResult<ModelOutput> {
            unimplemented!("tests never run a model round")
        }
    }

    struct RecordingEngine {
        searched_limits: std::sync::Mutex<Vec<usize>>,
        fetched: std::sync::Mutex<Option<ContextItem>>,
    }

    #[async_trait::async_trait]
    impl ContextEngine for RecordingEngine {
        async fn ingest(&self, _ingress: ContextIngress) -> AgentResult<()> {
            unimplemented!()
        }
        async fn maintain(
            &self,
            _trigger: ContextMaintenanceTrigger,
        ) -> AgentResult<ContextMaintenanceReport> {
            unimplemented!()
        }
        async fn materialize(&self, _query: ContextQuery) -> AgentResult<MaterializedContext> {
            unimplemented!()
        }
        async fn open_scope(
            &self,
            _kind: ScopeKind,
            _parent: Option<ScopeId>,
        ) -> AgentResult<ScopeId> {
            unimplemented!()
        }
        async fn close_scope(
            &self,
            _scope_id: ScopeId,
        ) -> AgentResult<Vec<ContextStateTransition>> {
            unimplemented!()
        }
        async fn diagnostics(&self) -> AgentResult<ContextDiagnostics> {
            unimplemented!()
        }
        async fn inspect(&self, _limit: usize) -> AgentResult<Vec<ContextItemSummary>> {
            Ok(Vec::new())
        }
        async fn search_external(
            &self,
            query: ContextSearchQuery,
        ) -> AgentResult<Vec<ExternalizedContext>> {
            self.searched_limits.lock().unwrap().push(query.limit);
            Ok(Vec::new())
        }
        async fn fetch_external(
            &self,
            _item_id: ContextItemId,
        ) -> AgentResult<Option<ContextItem>> {
            Ok(self.fetched.lock().unwrap().clone())
        }
        async fn checkpoint(&self) -> AgentResult<serde_json::Value> {
            Ok(serde_json::Value::Null)
        }
        async fn restore(&self, _data: serde_json::Value) -> AgentResult<()> {
            Ok(())
        }
    }

    fn test_item(content: String) -> ContextItem {
        ContextItem {
            id: ContextItemId::new(),
            task_id: None,
            scope_id: None,
            content,
            kind: ContextKind::Note,
            scope: ContextScope::Task,
            retention: ContextRetention::Working,
            attention: AttentionState::Active,
            semantic: SemanticState::Live,
            importance: 0.0,
            relevance: 0.0,
            created_tick: 0,
            last_access_tick: 0,
            access_count: 0,
            created_turn: 0,
            last_access_turn: 0,
            last_selected_turn: 0,
            dependencies: Vec::new(),
            tags: Vec::new(),
            keep_alive: false,
            lease_until_turn: None,
            source: None,
            residency: ContextResidency::Resident,
            gc_generation: 0,
            evicted_at_tick: None,
            entities: Vec::new(),
        }
    }

    fn test_kernel(
        engine: Arc<dyn ContextEngine>,
        dispatcher: Arc<dyn ToolDispatcher>,
        broker: Option<Arc<dyn OutputBroker>>,
    ) -> Arc<AgentKernel> {
        Arc::new(AgentKernel::new(
            AgentKernelConfig {
                output_broker: broker,
                ..AgentKernelConfig::default()
            },
            engine,
            Arc::new(UnusedModel),
            dispatcher,
            Arc::new(AllowAllApproval),
            None,
        ))
    }

    fn surface_with(name: &str) -> ToolSurfaceSnapshot {
        ToolSurfaceSnapshot {
            specs: vec![ToolSpec {
                name: name.into(),
                description: "x".into(),
                input_schema: serde_json::json!({}),
                risk: ToolRisk::ReadOnly,
                output_budget: None,
            }],
            ..ToolSurfaceSnapshot::default()
        }
    }

    #[tokio::test]
    async fn output_broker_bounds_tool_results_before_the_actor() {
        let broker = Arc::new(RecordingBroker::default());
        let dispatcher = Arc::new(BigOutputDispatcher {
            output: ToolOutput {
                call_id: "c1".into(),
                tool_name: "big.tool".into(),
                ok: true,
                summary: "done".into(),
                model_content: "x".repeat(100_000),
                artifact_ref: None,
                metadata: serde_json::Value::Null,
            },
        });
        let kernel = test_kernel(
            Arc::new(RecordingEngine {
                searched_limits: Default::default(),
                fetched: Default::default(),
            }),
            dispatcher,
            Some(broker.clone()),
        );
        let outcome = kernel
            .execute_tool(
                call("big.tool"),
                CancellationToken::new(),
                &surface_with("big.tool"),
            )
            .await;
        assert_eq!(*broker.calls.lock().unwrap(), 1, "broker must run once");
        let ToolOutcome::Value(output) = outcome else {
            panic!("expected a plain value");
        };
        assert_eq!(output.model_content.len(), 100_000);
        let seen = broker
            .last_output
            .lock()
            .unwrap()
            .clone()
            .expect("broker saw the output");
        assert_eq!(seen.model_content, "x".repeat(100_000));
    }

    #[tokio::test]
    async fn no_broker_keeps_the_outcome_untouched() {
        let dispatcher = Arc::new(BigOutputDispatcher {
            output: ToolOutput {
                call_id: "c1".into(),
                tool_name: "big.tool".into(),
                ok: true,
                summary: "done".into(),
                model_content: "x".repeat(100_000),
                artifact_ref: None,
                metadata: serde_json::Value::Null,
            },
        });
        let kernel = test_kernel(
            Arc::new(RecordingEngine {
                searched_limits: Default::default(),
                fetched: Default::default(),
            }),
            dispatcher,
            None,
        );
        let outcome = kernel
            .execute_tool(
                call("big.tool"),
                CancellationToken::new(),
                &surface_with("big.tool"),
            )
            .await;
        let ToolOutcome::Value(output) = outcome else {
            panic!("expected a plain value");
        };
        assert_eq!(output.model_content.len(), 100_000);
    }

    #[tokio::test]
    async fn context_fetch_results_are_bounded_after_resolve() {
        let broker = Arc::new(RecordingBroker::default());
        let engine = Arc::new(RecordingEngine {
            searched_limits: Default::default(),
            fetched: std::sync::Mutex::new(Some(test_item("big".repeat(200_000)))),
        });
        let kernel = test_kernel(
            engine,
            Arc::new(BigOutputDispatcher {
                output: ToolOutput {
                    call_id: "c1".into(),
                    tool_name: "context.manage".into(),
                    ok: true,
                    summary: "placeholder".into(),
                    model_content: "placeholder".into(),
                    artifact_ref: None,
                    metadata: serde_json::Value::Null,
                },
            }),
            Some(broker.clone()),
        );
        let placeholder = ToolOutput {
            call_id: "c1".into(),
            tool_name: "context.manage".into(),
            ok: true,
            summary: "placeholder".into(),
            model_content: "placeholder".into(),
            artifact_ref: None,
            metadata: serde_json::Value::Null,
        };
        let output = kernel
            .resolve_engine_query(
                placeholder,
                EngineQuery::FetchExternal {
                    item_id: ContextItemId::new(),
                },
            )
            .await;
        assert_eq!(
            *broker.calls.lock().unwrap(),
            1,
            "broker must bound the fetch result"
        );
        assert!(output.model_content.contains("big"));
        let seen = broker
            .last_output
            .lock()
            .unwrap()
            .clone()
            .expect("broker saw the output");
        assert!(
            seen.model_content.contains("big"),
            "the full fetched content reaches the broker"
        );
    }

    #[tokio::test]
    async fn search_limit_is_clamped_in_execution() {
        let engine = Arc::new(RecordingEngine {
            searched_limits: Default::default(),
            fetched: Default::default(),
        });
        let kernel = test_kernel(
            engine.clone(),
            Arc::new(BigOutputDispatcher {
                output: ToolOutput {
                    call_id: "c1".into(),
                    tool_name: "context.manage".into(),
                    ok: true,
                    summary: "placeholder".into(),
                    model_content: "placeholder".into(),
                    artifact_ref: None,
                    metadata: serde_json::Value::Null,
                },
            }),
            None,
        );
        let placeholder = ToolOutput {
            call_id: "c1".into(),
            tool_name: "context.manage".into(),
            ok: true,
            summary: "placeholder".into(),
            model_content: "placeholder".into(),
            artifact_ref: None,
            metadata: serde_json::Value::Null,
        };
        let _ = kernel
            .resolve_engine_query(
                placeholder.clone(),
                EngineQuery::SearchExternal {
                    query: "x".into(),
                    kind: None,
                    scope: None,
                    task_id: None,
                    limit: 1_000_000,
                },
            )
            .await;
        let limits = engine.searched_limits.lock().unwrap();
        assert_eq!(limits.as_slice(), &[CONTEXT_SEARCH_MAX_LIMIT]);
    }

    #[tokio::test]
    async fn search_limit_zero_keeps_the_engine_default() {
        let engine = Arc::new(RecordingEngine {
            searched_limits: Default::default(),
            fetched: Default::default(),
        });
        let kernel = test_kernel(
            engine.clone(),
            Arc::new(BigOutputDispatcher {
                output: ToolOutput {
                    call_id: "c1".into(),
                    tool_name: "context.manage".into(),
                    ok: true,
                    summary: "placeholder".into(),
                    model_content: "placeholder".into(),
                    artifact_ref: None,
                    metadata: serde_json::Value::Null,
                },
            }),
            None,
        );
        let placeholder = ToolOutput {
            call_id: "c1".into(),
            tool_name: "context.manage".into(),
            ok: true,
            summary: "placeholder".into(),
            model_content: "placeholder".into(),
            artifact_ref: None,
            metadata: serde_json::Value::Null,
        };
        let _ = kernel
            .resolve_engine_query(
                placeholder,
                EngineQuery::SearchExternal {
                    query: "x".into(),
                    kind: None,
                    scope: None,
                    task_id: None,
                    limit: 0,
                },
            )
            .await;
        let limits = engine.searched_limits.lock().unwrap();
        assert_eq!(
            limits.as_slice(),
            &[0],
            "0 must stay 0 so the engine default applies"
        );
    }

    // --- ACI v2 shadow mode (IntentShadowGate) ---

    /// A deterministic shadow gate for the kernel integration test.
    struct FixedShadowGate(agent_contracts::ShadowVerdict);

    #[async_trait::async_trait]
    impl agent_contracts::IntentShadowGate for FixedShadowGate {
        async fn shadow_verdict(
            &self,
            _call: &ToolCall,
            _spec: &ToolSpec,
        ) -> agent_contracts::ShadowVerdict {
            self.0.clone()
        }
    }

    #[tokio::test]
    async fn execute_tool_publishes_the_shadow_decision_event() {
        let shadow = Arc::new(FixedShadowGate(agent_contracts::ShadowVerdict::Denied {
            reason: "no live standing grant matches the derived intent (workspace write to 'x')"
                .into(),
        }));
        let kernel = Arc::new(AgentKernel::new(
            AgentKernelConfig {
                shadow_gate: Some(shadow),
                ..AgentKernelConfig::default()
            },
            Arc::new(RecordingEngine {
                searched_limits: Default::default(),
                fetched: Default::default(),
            }),
            Arc::new(UnusedModel),
            Arc::new(BigOutputDispatcher {
                output: ToolOutput {
                    call_id: "c1".into(),
                    tool_name: "big.tool".into(),
                    ok: true,
                    summary: "done".into(),
                    model_content: "ok".into(),
                    artifact_ref: None,
                    metadata: serde_json::Value::Null,
                },
            }),
            Arc::new(AllowAllApproval),
            None,
        ));
        let mut events = kernel.subscribe();

        let outcome = kernel
            .execute_tool(
                call("big.tool"),
                CancellationToken::new(),
                &surface_with("big.tool"),
            )
            .await;
        assert!(
            matches!(outcome, ToolOutcome::Value(_)),
            "the legacy gate still runs and the call executes"
        );

        // The shadow comparison is published for the allowed call.
        let envelope = tokio::time::timeout(std::time::Duration::from_secs(2), events.recv())
            .await
            .expect("a ShadowDecision event must be published")
            .expect("stream open");
        let RuntimeEvent::ShadowDecision {
            call_name,
            legacy_allowed,
            shadow,
        } = envelope.event
        else {
            panic!("expected ShadowDecision, got {:?}", envelope.event);
        };
        assert_eq!(call_name, "big.tool");
        assert!(legacy_allowed, "the legacy AllowAll gate allowed the call");
        assert!(
            matches!(shadow, agent_contracts::ShadowVerdict::Denied { .. }),
            "the shadow gate recorded its v2 refusal"
        );
    }
}
