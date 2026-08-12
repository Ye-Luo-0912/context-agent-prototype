use std::sync::Arc;

use agent_contracts::{
    AgentError, AgentResult, AuthorityLease, CONTEXT_SEARCH_MAX_LIMIT,
    CONTEXT_SEARCH_MAX_QUERY_CHARS, CancellationToken, ContextConsumptionAck, ContextEngine,
    ContextMaintenanceTrigger, ContextQuery, ContextSearchQuery, EngineQuery, OutputBroker, RunId,
    RuntimeEvent, TaskId, ToolCall, ToolDispatcher, ToolExecutionRequest, ToolOutcome, ToolOutput,
    ToolRisk, ToolSurfaceSnapshot, derive_effect_intent,
};

use crate::authority::{
    ApprovalAuthority, ApprovalVerdict, EffectAuthority, EventAuthority, OutputAuthority,
};

/// Default commit-time lease window for one side-effecting tool call (ACI
/// v2 §6): how long after approval a staged effect may still be committed.
/// Short by design — a tool computation that overruns this window is
/// rolled back at commit time instead of mutating the world.
pub const DEFAULT_LEASE_TTL_MS: u64 = 120_000;

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
            .finish()
    }
}

impl Default for CoreAuthorityConfig {
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
            lease_ttl_ms: None,
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
pub struct CoreAuthority {
    run_id: RunId,
    config: CoreAuthorityConfig,
    context: Arc<dyn ContextEngine>,
    tools: Arc<dyn ToolDispatcher>,
    event: EventAuthority,
    approval: ApprovalAuthority,
    effect: EffectAuthority,
    output: OutputAuthority,
}

impl CoreAuthority {
    pub fn new(
        config: CoreAuthorityConfig,
        context: Arc<dyn ContextEngine>,
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

    /// Commit model-consumption reinforcement and its bounded audit record as
    /// one context transaction. If either the engine mutation or event append
    /// fails, restore the pre-ack checkpoint so GC never observes an
    /// unaudited/partial access stamp. Context scheduling (ingest, maintain,
    /// GC, scopes, focus) lives on `RuntimeServices`; this one stays on the
    /// kernel because it is an *authority transaction* — the access stamp
    /// plus its mandatory audit event commit or roll back together.
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
                // 查询上限在执行期强制，而不只依赖 JSON schema：恶意或过期的
                // limit 在到达引擎前就被钳制，模型永远无法要求无界命中集。
                // 0 表示保持引擎默认值。
                let limit = limit.min(CONTEXT_SEARCH_MAX_LIMIT);
                // 自由文本查询同样有硬上限：超长查询在到达引擎前按字符截断，
                // 避免模型用巨型查询字符串冲刷检索路径。
                let query: String = query.chars().take(CONTEXT_SEARCH_MAX_QUERY_CHARS).collect();
                // 空结果需要区分"确实没有外部化证据"（无过滤）与"当前过滤条件下
                // 未命中"（带过滤）：前者提示模型放弃检索，后者提示证据可能
                // 存在于别的过滤条件下，值得换个条件重试。
                let has_filter = kind.is_some() || scope.is_some() || task_id.is_some();
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
                        output.model_content = if has_filter {
                            "context.search: nothing matches within the requested filter — evidence may exist under a different filter.".into()
                        } else {
                            "context.search: no externalized items match the query.".into()
                        };
                    }
                    Ok(hits) => {
                        output.ok = true;
                        output.summary = format!("{} external ref(s) match", hits.len());
                        // 每行命中都带上来源权威（source）：检索结果让模型
                        // 直接看到条目来自哪里（工具/用户/派生等），None 显示
                        // "-" 与 task 占位风格一致；这是权威校验的可观察基础。
                        output.model_content = hits
                            .iter()
                            .map(|entry| {
                                format!(
                                    "{} | kind={:?} scope={:?} task={} source={} | {}\n  tags: {}\n  entities: {}",
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
                        // inspect 是元数据视图：来源权威（source）与
                        // residency/semantic 并列展示，None 显示 "-"。
                        output.model_content = format!(
                            "{} | kind={:?} scope={:?} task={} source={} residency={:?} semantic={:?}\nsummary: {}\ntags: {}\nentities: {}",
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
                        // fetch 返回完整条目：来源权威（source）进头部行，
                        // None 显示 "-"；正文仍经 output.bound 截断 + spill。
                        output.model_content = format!(
                            "[{:?} | {:?} | id={} | source={}]\n{}",
                            item.kind,
                            item.scope,
                            item.id,
                            item.source.as_deref().unwrap_or("-"),
                            item.content
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

    /// Execute one tool call: validate it against the round's tool surface
    /// snapshot (the same surface the model saw and the budget used), run
    /// approval, mint the commit-time authority lease for side-effecting
    /// calls, dispatch. Emits nothing — ToolStarted/ToolFinished are
    /// committed by the actor. Returns the tool outcome (a plain value or
    /// a staged effect the actor commits/rolls back after the generation
    /// fence) plus the lease, when one was minted.
    pub async fn execute_tool(
        &self,
        call: ToolCall,
        cancel: CancellationToken,
        surface: &ToolSurfaceSnapshot,
        generation: u64,
    ) -> (ToolOutcome, Option<AuthorityLease>) {
        let spec = surface
            .specs
            .iter()
            .find(|spec| spec.name == call.name)
            .cloned();
        let Some(spec) = spec else {
            return (
                ToolOutcome::Value(tool_error_output(
                    &call,
                    tool_not_on_surface_message(&call, surface),
                )),
                None,
            );
        };

        let verdict = self.approval.authorize(&call, &spec, &cancel).await;
        let legacy_allowed = matches!(verdict, ApprovalVerdict::Allowed);

        // Shadow mode (ACI v2 step 4): the v2 intent-derived verdict is
        // computed once and reused for the audit event and the lease's
        // covering grant, so the comparison and the lease can never drift.
        // Best-effort observability: a failed journal append must not turn
        // a granted call into an error.
        let shadow = if self.approval.has_shadow() {
            self.approval.shadow_verdict(&call, &spec).await
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

        match verdict {
            ApprovalVerdict::Allowed => {}
            ApprovalVerdict::Denied(message) | ApprovalVerdict::Failed(message) => {
                return (ToolOutcome::Value(tool_error_output(&call, message)), None);
            }
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
                operation_generation: generation,
                intent: derive_effect_intent(&call, &spec),
                grant_id,
                decision: agent_contracts::ApprovalDecision::Allow,
                issued_at_ms,
                expires_at_ms: issued_at_ms.saturating_add(ttl),
            };
            let _ = self
                .emit_event(RuntimeEvent::LeaseIssued {
                    lease_id: lease.lease_id.clone(),
                    call_name: call.name.clone(),
                    grant_id: lease.grant_id.clone(),
                    expires_at_ms: lease.expires_at_ms,
                })
                .await;
            Some(lease)
        } else {
            None
        };

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
        (outcome, lease)
    }

    pub async fn emit_diagnostics(&self) -> AgentResult<()> {
        let diagnostics = self.context.diagnostics().await?;
        self.emit_event(RuntimeEvent::Diagnostics { diagnostics })
            .await
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
        ApprovalDecision, ApprovalGate, AttentionState, ContextDiagnostics, ContextIngress,
        ContextItem, ContextItemId, ContextItemSummary, ContextKind, ContextMaintenanceReport,
        ContextRef, ContextResidency, ContextRetention, ContextScope, ContextStateTransition,
        ExternalizedContext, MaterializedContext, ScopeId, ScopeKind, SemanticState, ToolRisk,
        ToolSpec, ToolSurfaceDemand, ToolSurfaceOmission, ToolSurfaceOmissionReason,
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

    /// Returns a fixed output for any call — for tests that exercise the
    /// approval/lease path without caring about the dispatched value.
    struct EchoDispatcher {
        output: ToolOutput,
    }

    #[async_trait::async_trait]
    impl ToolDispatcher for EchoDispatcher {
        fn specs(&self) -> Vec<ToolSpec> {
            Vec::new()
        }
        async fn execute(&self, _request: ToolExecutionRequest) -> AgentResult<ToolOutcome> {
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

    struct RecordingEngine {
        searched_limits: std::sync::Mutex<Vec<usize>>,
        searched_queries: std::sync::Mutex<Vec<String>>,
        search_hits: std::sync::Mutex<Vec<ExternalizedContext>>,
        inspect_external_entry: std::sync::Mutex<Option<ExternalizedContext>>,
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
            self.searched_queries
                .lock()
                .unwrap()
                .push(query.query.clone());
            Ok(self.search_hits.lock().unwrap().clone())
        }
        async fn inspect_external(
            &self,
            _item_id: ContextItemId,
        ) -> AgentResult<Option<ExternalizedContext>> {
            Ok(self.inspect_external_entry.lock().unwrap().clone())
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
    ) -> Arc<CoreAuthority> {
        Arc::new(CoreAuthority::new(
            CoreAuthorityConfig {
                output_broker: broker,
                ..CoreAuthorityConfig::default()
            },
            engine,
            dispatcher,
            Arc::new(AllowAllApproval),
            None,
        ))
    }

    /// 构造一个带指定来源权威（source）的外部化条目，用于检索输出渲染测试。
    fn external_entry(source: Option<&str>) -> ExternalizedContext {
        let item_id = ContextItemId::new();
        ExternalizedContext {
            item_id,
            task_id: None,
            scope_id: None,
            kind: ContextKind::Note,
            scope: ContextScope::Task,
            retention: ContextRetention::Working,
            attention: AttentionState::Archived,
            semantic: SemanticState::Live,
            context_ref: ContextRef {
                uri: format!("context://run/{item_id}"),
                item_id,
                kind: ContextKind::Note,
                scope: ContextScope::Task,
                summary: "a past tool capture".into(),
                created_tick: 0,
            },
            externalized_at_tick: 0,
            last_access_tick: 0,
            residency: ContextResidency::Cold,
            entities: Vec::new(),
            tags: Vec::new(),
            dependencies: Vec::new(),
            keep_alive: false,
            lease_until_turn: None,
            last_access_gc_epoch: Some(0),
            blob_checksum: None,
            source: source.map(|s| s.to_string()),
            importance: 0.0,
            relevance: 0.0,
            created_tick: 0,
            created_turn: 0,
            last_access_turn: 0,
            last_selected_turn: 0,
            access_count: 0,
            gc_generation: 0,
            evicted_at_tick: None,
        }
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
                searched_queries: Default::default(),
                search_hits: Default::default(),
                inspect_external_entry: Default::default(),
                fetched: Default::default(),
            }),
            dispatcher,
            Some(broker.clone()),
        );
        let (outcome, lease) = kernel
            .execute_tool(
                call("big.tool"),
                CancellationToken::new(),
                &surface_with("big.tool"),
                0,
            )
            .await;
        assert!(
            lease.is_none(),
            "a read-only call carries no commit-time lease"
        );
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
                searched_queries: Default::default(),
                search_hits: Default::default(),
                inspect_external_entry: Default::default(),
                fetched: Default::default(),
            }),
            dispatcher,
            None,
        );
        let (outcome, lease) = kernel
            .execute_tool(
                call("big.tool"),
                CancellationToken::new(),
                &surface_with("big.tool"),
                0,
            )
            .await;
        assert!(
            lease.is_none(),
            "a read-only call carries no commit-time lease"
        );
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
            searched_queries: Default::default(),
            search_hits: Default::default(),
            inspect_external_entry: Default::default(),
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
            searched_queries: Default::default(),
            search_hits: Default::default(),
            inspect_external_entry: Default::default(),
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
            searched_queries: Default::default(),
            search_hits: Default::default(),
            inspect_external_entry: Default::default(),
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

    #[tokio::test]
    async fn search_query_length_is_bounded_in_execution() {
        // 超长查询在执行期被截断到 CONTEXT_SEARCH_MAX_QUERY_CHARS：
        // 引擎只收到有界长度的查询字符串，模型无法用巨型查询冲刷检索路径。
        let engine = Arc::new(RecordingEngine {
            searched_limits: Default::default(),
            searched_queries: Default::default(),
            search_hits: Default::default(),
            inspect_external_entry: Default::default(),
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
                    query: "x".repeat(CONTEXT_SEARCH_MAX_QUERY_CHARS * 4),
                    kind: None,
                    scope: None,
                    task_id: None,
                    limit: 10,
                },
            )
            .await;
        let queries = engine.searched_queries.lock().unwrap();
        assert_eq!(queries.len(), 1);
        assert_eq!(
            queries[0].chars().count(),
            CONTEXT_SEARCH_MAX_QUERY_CHARS,
            "the engine must receive a query truncated to the execution cap"
        );
    }

    #[tokio::test]
    async fn empty_search_distinguishes_no_evidence_from_filter_miss() {
        // 无过滤的空结果说明确实没有外部化证据；带过滤的空结果提示
        // 证据可能存在于别的过滤条件下——模型据此决定放弃还是换过滤重试。
        let engine = Arc::new(RecordingEngine {
            searched_limits: Default::default(),
            searched_queries: Default::default(),
            search_hits: Default::default(),
            inspect_external_entry: Default::default(),
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
        let no_filter = kernel
            .resolve_engine_query(
                placeholder.clone(),
                EngineQuery::SearchExternal {
                    query: "x".into(),
                    kind: None,
                    scope: None,
                    task_id: None,
                    limit: 10,
                },
            )
            .await;
        assert!(
            no_filter
                .model_content
                .contains("no externalized items match"),
            "no filter must report that there is genuinely no externalized evidence"
        );
        let filtered = kernel
            .resolve_engine_query(
                placeholder,
                EngineQuery::SearchExternal {
                    query: "x".into(),
                    kind: Some(ContextKind::Note),
                    scope: None,
                    task_id: None,
                    limit: 10,
                },
            )
            .await;
        assert!(
            filtered.model_content.contains("different filter"),
            "a filter miss must hint that evidence may exist under another filter"
        );
    }

    #[tokio::test]
    async fn search_hits_render_the_source_authority() {
        // 检索命中行携带来源权威：带 source 的条目显示真实来源，
        // 无来源的条目显示 "-"，与 task 占位风格一致。
        let engine = Arc::new(RecordingEngine {
            searched_limits: Default::default(),
            searched_queries: Default::default(),
            search_hits: std::sync::Mutex::new(vec![
                external_entry(Some("tool-capture")),
                external_entry(None),
            ]),
            inspect_external_entry: Default::default(),
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
        let output = kernel
            .resolve_engine_query(
                ToolOutput {
                    call_id: "c1".into(),
                    tool_name: "context.manage".into(),
                    ok: true,
                    summary: "placeholder".into(),
                    model_content: "placeholder".into(),
                    artifact_ref: None,
                    metadata: serde_json::Value::Null,
                },
                EngineQuery::SearchExternal {
                    query: "x".into(),
                    kind: None,
                    scope: None,
                    task_id: None,
                    limit: 10,
                },
            )
            .await;
        assert!(
            output.model_content.contains("source=tool-capture"),
            "a hit with a known source must render it: {}",
            output.model_content
        );
        assert!(
            output.model_content.contains("source=-"),
            "a hit without a source must render the dash placeholder"
        );
    }

    #[tokio::test]
    async fn inspect_renders_the_source_authority() {
        // inspect 元数据视图与 residency/semantic 并列展示来源权威。
        let engine = Arc::new(RecordingEngine {
            searched_limits: Default::default(),
            searched_queries: Default::default(),
            search_hits: Default::default(),
            inspect_external_entry: std::sync::Mutex::new(Some(external_entry(Some(
                "tool-session",
            )))),
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
        let output = kernel
            .resolve_engine_query(
                ToolOutput {
                    call_id: "c1".into(),
                    tool_name: "context.manage".into(),
                    ok: true,
                    summary: "placeholder".into(),
                    model_content: "placeholder".into(),
                    artifact_ref: None,
                    metadata: serde_json::Value::Null,
                },
                EngineQuery::InspectExternal {
                    item_id: ContextItemId::new(),
                },
            )
            .await;
        assert!(
            output.model_content.contains("source=tool-session"),
            "inspect must render the source authority: {}",
            output.model_content
        );
    }

    #[tokio::test]
    async fn fetch_renders_the_source_authority() {
        // fetch 的头部行携带来源权威，正文仍走有界输出。
        let mut item = test_item("stored body".into());
        item.source = Some("tool-capture".into());
        let engine = Arc::new(RecordingEngine {
            searched_limits: Default::default(),
            searched_queries: Default::default(),
            search_hits: Default::default(),
            inspect_external_entry: Default::default(),
            fetched: std::sync::Mutex::new(Some(item)),
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
        let output = kernel
            .resolve_engine_query(
                ToolOutput {
                    call_id: "c1".into(),
                    tool_name: "context.manage".into(),
                    ok: true,
                    summary: "placeholder".into(),
                    model_content: "placeholder".into(),
                    artifact_ref: None,
                    metadata: serde_json::Value::Null,
                },
                EngineQuery::FetchExternal {
                    item_id: ContextItemId::new(),
                },
            )
            .await;
        assert!(
            output.model_content.contains("source=tool-capture"),
            "fetch must render the source authority in the header: {}",
            output.model_content
        );
        assert!(output.model_content.contains("stored body"));
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
        let kernel = Arc::new(CoreAuthority::new(
            CoreAuthorityConfig {
                shadow_gate: Some(shadow),
                ..CoreAuthorityConfig::default()
            },
            Arc::new(RecordingEngine {
                searched_limits: Default::default(),
                searched_queries: Default::default(),
                search_hits: Default::default(),
                inspect_external_entry: Default::default(),
                fetched: Default::default(),
            }),
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

        let (outcome, lease) = kernel
            .execute_tool(
                call("big.tool"),
                CancellationToken::new(),
                &surface_with("big.tool"),
                0,
            )
            .await;
        assert!(
            lease.is_none(),
            "a read-only call carries no commit-time lease"
        );
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

    /// Read the next `LeaseIssued` audit row, skipping any events published
    /// before it (the shadow comparison lands first).
    async fn next_lease_issued(
        events: &mut tokio::sync::broadcast::Receiver<agent_contracts::RuntimeEventEnvelope>,
    ) -> agent_contracts::RuntimeEvent {
        for _ in 0..4 {
            let envelope = tokio::time::timeout(std::time::Duration::from_secs(2), events.recv())
                .await
                .expect("a lease audit event must be published")
                .expect("stream open");
            if let agent_contracts::RuntimeEvent::LeaseIssued { .. } = envelope.event {
                return envelope.event;
            }
        }
        panic!("no LeaseIssued event published");
    }

    #[tokio::test]
    async fn execute_tool_mints_a_commit_time_lease_for_side_effecting_calls() {
        let shadow = Arc::new(FixedShadowGate(agent_contracts::ShadowVerdict::Granted {
            grant_id: "g-1".into(),
            reason: "workspace write inside grant g-1".into(),
        }));
        let kernel = Arc::new(CoreAuthority::new(
            CoreAuthorityConfig {
                shadow_gate: Some(shadow),
                lease_ttl_ms: Some(5_000),
                ..CoreAuthorityConfig::default()
            },
            Arc::new(RecordingEngine {
                searched_limits: Default::default(),
                searched_queries: Default::default(),
                search_hits: Default::default(),
                inspect_external_entry: Default::default(),
                fetched: Default::default(),
            }),
            Arc::new(EchoDispatcher {
                output: ToolOutput {
                    call_id: "c1".into(),
                    tool_name: "fs.write".into(),
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

        let surface = ToolSurfaceSnapshot {
            specs: vec![ToolSpec {
                name: "fs.write".into(),
                description: "write".into(),
                input_schema: serde_json::json!({}),
                risk: ToolRisk::WorkspaceWrite,
                output_budget: None,
            }],
            ..ToolSurfaceSnapshot::default()
        };
        let write_call = ToolCall {
            id: "c1".into(),
            name: "fs.write".into(),
            arguments: serde_json::json!({"path": "src/main.rs", "content": "fn main() {}"}),
        };
        let (outcome, lease) = kernel
            .execute_tool(write_call, CancellationToken::new(), &surface, 5)
            .await;
        assert!(matches!(outcome, ToolOutcome::Value(_)));
        let lease = lease.expect("a side-effecting call must mint a lease");
        assert_eq!(
            lease.operation_generation, 5,
            "the lease is bound to the operation generation"
        );
        assert_eq!(
            lease.grant_id.as_deref(),
            Some("g-1"),
            "the covering grant from the v2 shadow verdict is recorded"
        );
        assert_eq!(
            lease.intent,
            agent_contracts::EffectIntent::WorkspaceWrite {
                path: "src/main.rs".into(),
                content_bytes: "fn main() {}".len() as u64,
            }
        );
        let now = now_ms();
        assert!(
            lease.issued_at_ms <= now && now <= lease.expires_at_ms,
            "the lease window contains the present instant"
        );
        assert_eq!(
            lease.expires_at_ms - lease.issued_at_ms,
            5_000,
            "the configured TTL bounds the lease window"
        );

        // The bounded audit row is published beside the shadow comparison.
        let RuntimeEvent::LeaseIssued {
            lease_id,
            call_name,
            grant_id,
            expires_at_ms,
        } = next_lease_issued(&mut events).await
        else {
            panic!("expected LeaseIssued");
        };
        assert_eq!(lease_id, lease.lease_id);
        assert_eq!(call_name, "fs.write");
        assert_eq!(grant_id.as_deref(), Some("g-1"));
        assert_eq!(expires_at_ms, lease.expires_at_ms);
    }

    #[tokio::test]
    async fn lease_is_minted_even_when_the_shadow_gate_denies() {
        // Shadow is observational: the legacy gate allowed the call, so it
        // executes and mints a lease. The lease records that no v2 grant
        // covered the intent — the audit truth, not an enforcement stop.
        let shadow = Arc::new(FixedShadowGate(agent_contracts::ShadowVerdict::Denied {
            reason: "no live standing grant matches the derived intent".into(),
        }));
        let kernel = Arc::new(CoreAuthority::new(
            CoreAuthorityConfig {
                shadow_gate: Some(shadow),
                ..CoreAuthorityConfig::default()
            },
            Arc::new(RecordingEngine {
                searched_limits: Default::default(),
                searched_queries: Default::default(),
                search_hits: Default::default(),
                inspect_external_entry: Default::default(),
                fetched: Default::default(),
            }),
            Arc::new(EchoDispatcher {
                output: ToolOutput {
                    call_id: "c1".into(),
                    tool_name: "shell.exec".into(),
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

        let surface = ToolSurfaceSnapshot {
            specs: vec![ToolSpec {
                name: "shell.exec".into(),
                description: "run".into(),
                input_schema: serde_json::json!({}),
                risk: ToolRisk::ProcessExecution,
                output_budget: None,
            }],
            ..ToolSurfaceSnapshot::default()
        };
        let (_, lease) = kernel
            .execute_tool(
                ToolCall {
                    id: "c1".into(),
                    name: "shell.exec".into(),
                    arguments: serde_json::json!({"command": "cargo test"}),
                },
                CancellationToken::new(),
                &surface,
                1,
            )
            .await;
        let lease = lease.expect("a side-effecting call mints a lease");
        assert_eq!(
            lease.grant_id, None,
            "a shadow-denied call records that no v2 grant covered it"
        );
        assert_eq!(
            lease.intent,
            agent_contracts::EffectIntent::ProcessRun {
                command: "cargo test".into()
            }
        );
    }
}
