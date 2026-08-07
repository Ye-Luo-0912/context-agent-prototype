use std::{
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{SystemTime, UNIX_EPOCH},
};

use agent_contracts::{
    AgentError, AgentResult, ApprovalDecision, ApprovalGate, CancellationToken,
    ContextBuildRequest, ContextEngine, ContextIngress, ContextItemSummary, ContextKind,
    ContextMaintenanceTrigger, EventJournal, FocusState, ModelChunk, ModelEventSink, ModelRequest,
    ModelTransport, RunId, RuntimeEvent, RuntimeEventEnvelope, ToolCall, ToolDispatcher,
    ToolExecutionRequest, ToolOutput,
};
use serde_json::json;
use tokio::sync::{Mutex, broadcast};

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
    turn_cancel: Mutex<Option<CancellationToken>>,
    turn_lock: Mutex<()>,
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
            turn_cancel: Mutex::new(None),
            turn_lock: Mutex::new(()),
        }
    }

    pub fn run_id(&self) -> RunId {
        self.run_id
    }

    pub fn subscribe(&self) -> broadcast::Receiver<RuntimeEventEnvelope> {
        self.event_tx.subscribe()
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

    /// Cancel the in-flight turn (if any). The model provider observes the
    /// token and aborts its stream; the turn loop then stops cleanly.
    pub async fn cancel_current_turn(&self) {
        if let Some(token) = self.turn_cancel.lock().await.take() {
            token.cancel();
        }
    }

    pub async fn handle_user_message(&self, content: String) -> AgentResult<()> {
        let result = self.handle_user_message_inner(content).await;
        if let Err(error) = &result {
            let _ = self
                .emit(RuntimeEvent::Error {
                    message: error.to_string(),
                })
                .await;
        }
        result
    }

    async fn handle_user_message_inner(&self, content: String) -> AgentResult<()> {
        let _guard = self.turn_lock.lock().await;
        if content.trim().is_empty() {
            return Ok(());
        }

        self.emit(RuntimeEvent::UserMessageAccepted {
            content: content.clone(),
        })
        .await?;
        self.context
            .ingest(ContextIngress::UserMessage {
                content: content.clone(),
            })
            .await?;
        let report = self
            .context
            .maintain(ContextMaintenanceTrigger::UserInput)
            .await?;
        self.emit(RuntimeEvent::ContextMaintained {
            trigger: ContextMaintenanceTrigger::UserInput,
            report,
        })
        .await?;

        let cancel = CancellationToken::new();
        *self.turn_cancel.lock().await = Some(cancel.clone());
        let result = self.run_turn(&content, &cancel).await;
        self.turn_cancel.lock().await.take();
        result
    }

    async fn run_turn(&self, content: &str, cancel: &CancellationToken) -> AgentResult<()> {
        for round in 0..=self.config.max_tool_rounds {
            if cancel.is_cancelled() {
                self.emit(RuntimeEvent::Warning {
                    message: "turn cancelled".into(),
                })
                .await?;
                self.emit(RuntimeEvent::TurnCompleted).await?;
                return Ok(());
            }

            if round == self.config.max_tool_rounds {
                let message = format!(
                    "tool round budget exhausted after {} rounds",
                    self.config.max_tool_rounds
                );
                self.emit(RuntimeEvent::Warning {
                    message: message.clone(),
                })
                .await?;
                return Err(AgentError::Internal(message));
            }

            let report = self
                .context
                .maintain(ContextMaintenanceTrigger::BeforeModel)
                .await?;
            self.emit(RuntimeEvent::ContextMaintained {
                trigger: ContextMaintenanceTrigger::BeforeModel,
                report,
            })
            .await?;

            let snapshot = self
                .context
                .build_snapshot(ContextBuildRequest {
                    system_prompt: self.config.system_prompt.clone(),
                    current_input: content.to_string(),
                    budget_tokens: self.config.context_budget_tokens,
                })
                .await?;
            self.emit(RuntimeEvent::ContextPrepared {
                diagnostics: snapshot.diagnostics.clone(),
                selected: snapshot.selected.clone(),
            })
            .await?;

            self.emit(RuntimeEvent::ModelStarted).await?;
            let sink = KernelSink {
                event_tx: self.event_tx.clone(),
                seq: self.seq.clone(),
                run_id: self.run_id,
            };
            let output = match self
                .model
                .complete_stream(
                    ModelRequest {
                        messages: snapshot.messages,
                        tools: self.tools.specs(),
                        metadata: json!({
                            "run_id": self.run_id.to_string(),
                            "context_selected": snapshot.selected.len(),
                            "context_approx_tokens": snapshot.approx_tokens,
                        }),
                        cancel: cancel.clone(),
                    },
                    &sink,
                )
                .await
            {
                Ok(output) => output,
                Err(error) => {
                    if cancel.is_cancelled() {
                        self.emit(RuntimeEvent::Warning {
                            message: "turn cancelled".into(),
                        })
                        .await?;
                        self.emit(RuntimeEvent::TurnCompleted).await?;
                        return Ok(());
                    }
                    return Err(error);
                }
            };

            if output.tool_calls.is_empty() {
                self.context
                    .ingest(ContextIngress::AssistantMessage {
                        content: output.content.clone(),
                    })
                    .await?;
                self.emit(RuntimeEvent::AssistantMessage {
                    content: output.content,
                })
                .await?;
                let report = self
                    .context
                    .maintain(ContextMaintenanceTrigger::AfterModel)
                    .await?;
                self.emit(RuntimeEvent::ContextMaintained {
                    trigger: ContextMaintenanceTrigger::AfterModel,
                    report,
                })
                .await?;
                self.emit(RuntimeEvent::TurnCompleted).await?;
                return Ok(());
            }

            for call in output.tool_calls {
                let tool_output = self.execute_tool(call, cancel).await;
                self.emit(RuntimeEvent::ToolFinished {
                    output: tool_output.clone(),
                })
                .await?;
                self.context
                    .ingest(ContextIngress::ToolObservation {
                        output: tool_output,
                    })
                    .await?;
                let report = self
                    .context
                    .maintain(ContextMaintenanceTrigger::AfterTool)
                    .await?;
                self.emit(RuntimeEvent::ContextMaintained {
                    trigger: ContextMaintenanceTrigger::AfterTool,
                    report,
                })
                .await?;
            }
        }

        Ok(())
    }

    pub async fn set_focus(&self, goal: String) -> AgentResult<()> {
        let focus = FocusState::new(goal.clone());
        self.context
            .ingest(ContextIngress::FocusChanged { focus })
            .await?;
        self.emit(RuntimeEvent::FocusChanged { goal }).await?;
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

    async fn execute_tool(&self, call: ToolCall, cancel: &CancellationToken) -> ToolOutput {
        if let Err(error) = self
            .emit(RuntimeEvent::ToolStarted { call: call.clone() })
            .await
        {
            tracing::warn!(%error, "failed to emit tool-start event");
        }

        let spec = self
            .tools
            .specs()
            .into_iter()
            .find(|spec| spec.name == call.name);
        let Some(spec) = spec else {
            return tool_error_output(&call, format!("unknown tool: {}", call.name));
        };

        match self.approval.authorize(&call, &spec).await {
            Ok(ApprovalDecision::Allow) => {}
            Ok(ApprovalDecision::Deny) => {
                return tool_error_output(
                    &call,
                    format!("tool denied by approval policy: {}", call.name),
                );
            }
            Err(error) => {
                return tool_error_output(&call, format!("approval check failed: {error}"));
            }
        }

        match self
            .tools
            .execute(ToolExecutionRequest {
                run_id: self.run_id,
                call: call.clone(),
                cancel: cancel.clone(),
            })
            .await
        {
            Ok(output) => output,
            Err(error) => tool_error_output(&call, error.to_string()),
        }
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

/// Forwards streamed text deltas to live subscribers without journaling them
/// (the final `AssistantMessage` carries the complete content for replay).
#[derive(Clone)]
struct KernelSink {
    event_tx: broadcast::Sender<RuntimeEventEnvelope>,
    seq: Arc<AtomicU64>,
    run_id: RunId,
}

impl KernelSink {
    fn emit_live(&self, event: RuntimeEvent) {
        let envelope = RuntimeEventEnvelope {
            run_id: self.run_id,
            seq: self.seq.fetch_add(1, Ordering::Relaxed) + 1,
            timestamp_ms: now_ms(),
            event,
        };
        let _ = self.event_tx.send(envelope);
    }
}

#[async_trait::async_trait]
impl ModelEventSink for KernelSink {
    async fn on_chunk(&self, chunk: ModelChunk) -> AgentResult<()> {
        match chunk {
            ModelChunk::TextDelta { delta } => {
                self.emit_live(RuntimeEvent::ModelDelta { delta });
                Ok(())
            }
            // Tool-call argument deltas are internal to the model round; the
            // kernel surfaces tool execution via ToolStarted/ToolFinished.
            ModelChunk::ToolCallDelta { .. } | ModelChunk::Done => Ok(()),
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

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or_default()
}
