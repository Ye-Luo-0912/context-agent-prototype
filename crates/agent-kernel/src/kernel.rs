use std::{
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{SystemTime, UNIX_EPOCH},
};

use agent_contracts::{
    AgentResult, ApprovalDecision, ApprovalGate, CancellationToken, ContextEngine, ContextGcReport,
    ContextIngress, ContextItemSummary, ContextKind, ContextMaintenanceReport,
    ContextMaintenanceTrigger, ContextQuery, ContextSearchQuery, ContextStateTransition,
    EngineQuery, EventJournal, FocusState, MaterializedContext, ModelCapabilities, ModelEventSink,
    ModelOutput, ModelRequest, ModelTransport, RunId, RuntimeEvent, RuntimeEventEnvelope, ScopeId,
    ScopeKind, TaskId, ToolCall, ToolDispatcher, ToolExecutionRequest, ToolOutcome, ToolOutput,
    ToolSpec, ToolSurfaceSnapshot,
};
use tokio::sync::broadcast;

#[derive(Debug, Clone)]
pub struct AgentKernelConfig {
    pub system_prompt: String,
    pub context_budget_tokens: usize,
    pub max_tool_rounds: usize,
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
        }
    }
}

/// The runtime's executor: stateless primitives over the engine contracts
/// (context, model, tools, approval, journal) plus the event plumbing. The
/// execution *state machine* (turn frame, generation, what to commit) lives
/// in the runtime actor — this type owns no turn state and no locks for it.
pub struct AgentKernel {
    run_id: RunId,
    config: AgentKernelConfig,
    context: Arc<dyn ContextEngine>,
    model: Arc<dyn ModelTransport>,
    tools: Arc<dyn ToolDispatcher>,
    approval: Arc<dyn ApprovalGate>,
    journal: Option<Arc<dyn EventJournal>>,
    event_tx: broadcast::Sender<RuntimeEventEnvelope>,
    seq: Arc<AtomicU64>,
}

impl AgentKernel {
    pub fn new(
        config: AgentKernelConfig,
        context: Arc<dyn ContextEngine>,
        model: Arc<dyn ModelTransport>,
        tools: Arc<dyn ToolDispatcher>,
        approval: Arc<dyn ApprovalGate>,
        journal: Option<Arc<dyn EventJournal>>,
    ) -> Self {
        let (event_tx, _) = broadcast::channel(1_024);
        Self {
            run_id: RunId::new(),
            config,
            context,
            model,
            tools,
            approval,
            journal,
            event_tx,
            seq: Arc::new(AtomicU64::new(0)),
        }
    }

    pub fn run_id(&self) -> RunId {
        self.run_id
    }

    pub fn subscribe(&self) -> broadcast::Receiver<RuntimeEventEnvelope> {
        self.event_tx.subscribe()
    }

    /// The broadcast sender behind `subscribe`, for live event sinks.
    pub fn event_sender(&self) -> broadcast::Sender<RuntimeEventEnvelope> {
        self.event_tx.clone()
    }

    /// The shared sequence counter, so live deltas and journaled events keep
    /// one consistent envelope order.
    pub fn seq(&self) -> Arc<AtomicU64> {
        self.seq.clone()
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

    /// Run the tool lifecycle GC at a runtime safe point. `specs()` is pure;
    /// the actor ages the tool catalog exactly once per model round, before
    /// the surface is captured for the budget and the prompt.
    pub fn tool_gc(&self) {
        self.tools.gc();
    }

    /// Unload an optional tool from the model surface (used by the final
    /// budget guard when the fixed layers overshoot the input budget).
    /// Core tools refuse to unload — they are part of the minimal surface.
    pub fn tool_unload(&self, name: &str) -> AgentResult<()> {
        self.tools.unload_tool(name)
    }

    pub async fn start(&self) -> AgentResult<()> {
        self.emit(RuntimeEvent::RunStarted).await
    }

    pub async fn stop(&self) -> AgentResult<()> {
        self.emit(RuntimeEvent::RunCompleted).await?;
        if let Some(journal) = &self.journal {
            journal.flush().await?;
        }
        Ok(())
    }

    /// Journal + broadcast one runtime event (the single write path).
    pub async fn emit_event(&self, event: RuntimeEvent) -> AgentResult<()> {
        self.emit(event).await
    }

    /// Surface a runtime-level warning through the normal event stream.
    pub async fn emit_warning(&self, message: String) -> AgentResult<()> {
        self.emit(RuntimeEvent::Warning { message }).await
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
        output
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
                format!("unknown tool: {}", call.name),
            ));
        };

        match self.approval.authorize(&call, &spec).await {
            Ok(ApprovalDecision::Allow) => {}
            Ok(ApprovalDecision::Deny) => {
                return ToolOutcome::Value(tool_error_output(
                    &call,
                    format!("tool denied by approval policy: {}", call.name),
                ));
            }
            Err(error) => {
                return ToolOutcome::Value(tool_error_output(
                    &call,
                    format!("approval check failed: {error}"),
                ));
            }
        }

        match self
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
        }
    }

    /// Switch the runtime's focus to a task's goal. The task id comes from
    /// the runtime's `TaskManager` — re-focusing an existing task resumes
    /// its scopes in the context engine (suspension/resume is keyed on the
    /// task id), while a fresh task id opens a fresh task scope.
    pub async fn set_focus(&self, task_id: TaskId, goal: String) -> AgentResult<()> {
        let focus = FocusState::for_task(task_id, goal.clone());
        self.context
            .ingest(ContextIngress::FocusChanged { focus })
            .await?;
        self.emit(RuntimeEvent::FocusChanged { task_id, goal })
            .await?;
        let report = self
            .context
            .maintain(ContextMaintenanceTrigger::FocusChanged)
            .await?;
        self.emit(RuntimeEvent::ContextMaintained {
            trigger: ContextMaintenanceTrigger::FocusChanged,
            report,
        })
        .await?;
        Ok(())
    }

    /// Suspend the current focus without completing the task: the engine
    /// clears its focus and suspends the active task's scopes, so a later
    /// `set_focus` with the same task id resumes them.
    pub async fn clear_focus(&self) -> AgentResult<()> {
        self.context.ingest(ContextIngress::FocusCleared).await?;
        self.emit(RuntimeEvent::FocusCleared).await?;
        let report = self
            .context
            .maintain(ContextMaintenanceTrigger::FocusChanged)
            .await?;
        self.emit(RuntimeEvent::ContextMaintained {
            trigger: ContextMaintenanceTrigger::FocusChanged,
            report,
        })
        .await?;
        Ok(())
    }

    pub async fn pin(&self, content: String) -> AgentResult<()> {
        self.context
            .ingest(ContextIngress::Pin {
                content: content.clone(),
                kind: ContextKind::Constraint,
            })
            .await?;
        self.emit(RuntimeEvent::Pinned { content }).await?;
        let report = self
            .context
            .maintain(ContextMaintenanceTrigger::FocusChanged)
            .await?;
        self.emit(RuntimeEvent::ContextMaintained {
            trigger: ContextMaintenanceTrigger::FocusChanged,
            report,
        })
        .await
    }

    pub async fn complete_current_task(&self, summary: String) -> AgentResult<()> {
        self.context
            .ingest(ContextIngress::TaskCompleted {
                task_id: None,
                summary: summary.clone(),
            })
            .await?;
        self.emit(RuntimeEvent::TaskCompleted { summary }).await?;
        let report = self
            .context
            .maintain(ContextMaintenanceTrigger::TaskCompleted)
            .await?;
        self.emit(RuntimeEvent::ContextMaintained {
            trigger: ContextMaintenanceTrigger::TaskCompleted,
            report,
        })
        .await
    }

    pub async fn emit_diagnostics(&self) -> AgentResult<()> {
        let diagnostics = self.context.diagnostics().await?;
        self.emit(RuntimeEvent::Diagnostics { diagnostics }).await
    }

    pub async fn inspect_context(&self, limit: usize) -> AgentResult<Vec<ContextItemSummary>> {
        self.context.inspect(limit).await
    }

    pub async fn checkpoint(&self) -> AgentResult<serde_json::Value> {
        let report = self
            .context
            .maintain(ContextMaintenanceTrigger::Checkpoint)
            .await?;
        self.emit(RuntimeEvent::ContextMaintained {
            trigger: ContextMaintenanceTrigger::Checkpoint,
            report,
        })
        .await?;
        self.context.checkpoint().await
    }

    /// Restore the context engine from a checkpoint's context payload.
    /// The runtime actor restores its own state (task table, current task)
    /// around this call, so a restored run has both planes back.
    pub async fn restore(&self, data: serde_json::Value) -> AgentResult<()> {
        self.context.restore(data).await
    }

    async fn emit(&self, event: RuntimeEvent) -> AgentResult<()> {
        let envelope = RuntimeEventEnvelope {
            run_id: self.run_id,
            seq: self.seq.fetch_add(1, Ordering::Relaxed) + 1,
            timestamp_ms: now_ms(),
            event,
        };

        if let Some(journal) = &self.journal {
            journal.append(&envelope).await?;
        }
        let _ = self.event_tx.send(envelope);
        Ok(())
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

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or_default()
}
