//! Headless evaluation driver: compose the real runtime (real model via
//! `OPENAI_*` env vars, no tools, one context engine), run the
//! constraint-retention turn script, and measure true provider usage via
//! `RuntimeEvent::ModelUsed` plus the final answer's correctness.

use std::sync::Arc;
use std::time::Duration;

use agent_contracts::{
    AgentError, AgentResult, ApprovalDecision, ApprovalGate, ContextEngine, ModelTransport,
    RuntimeEvent, RuntimeEventEnvelope, ToolCall, ToolDispatcher, ToolExecutionRequest,
    ToolOutcome, ToolSpec,
};
use agent_core::CoreAuthorityConfig;
use agent_runtime::{
    ApprovalModule, ContextModule, ModelModule, ModuleHost, RuntimeInstance, RuntimeServices,
};
use anyhow::Context as _;
use context_baselines::{AppendOnlyEngine, RollingConfig, RollingSummaryEngine};
use context_simple::{SimpleContextConfig, SimpleContextEngine};
use provider_openai::{OpenAiConfig, OpenAiProvider, RetryingTransport};
use tokio::sync::broadcast;

/// Per-engine measurement of one eval run.
pub struct EvalSummary {
    pub engine: &'static str,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub user_turns: u64,
    pub passed: bool,
    pub final_answer: String,
    pub note: String,
}

impl EvalSummary {
    fn new(engine: &'static str) -> Self {
        Self {
            engine,
            input_tokens: 0,
            output_tokens: 0,
            user_turns: 0,
            passed: false,
            final_answer: String::new(),
            note: String::new(),
        }
    }
}

/// Approval policy for the eval harness (no tools are ever invoked, but the
/// runtime still wants a gate).
struct AllowAllGate;

#[async_trait::async_trait]
impl ApprovalGate for AllowAllGate {
    async fn authorize(
        &self,
        _call: &ToolCall,
        _spec: &ToolSpec,
        _cancel: &agent_contracts::CancellationToken,
    ) -> AgentResult<ApprovalDecision> {
        Ok(ApprovalDecision::Allow)
    }
}

/// An empty tool surface: the default live task is context retention, so
/// the wire request carries `tools: []`. Coding live runs use `--fixture-live`
/// with the builtin dispatcher; dotted Core ids are mapped by the provider.
struct NoToolDispatcher;

#[async_trait::async_trait]
impl ToolDispatcher for NoToolDispatcher {
    fn specs(&self) -> Vec<ToolSpec> {
        Vec::new()
    }
    fn catalog(&self) -> Vec<agent_contracts::ToolCatalogEntry> {
        Vec::new()
    }
    async fn execute(&self, _request: ToolExecutionRequest) -> AgentResult<ToolOutcome> {
        Err(AgentError::Tool("no tools in the eval harness".into()))
    }
}

/// Build the model transport from `OPENAI_API_KEY` / `OPENAI_BASE_URL` /
/// `OPENAI_MODEL` (same contract the TUI composition root uses).
pub(crate) fn build_model() -> anyhow::Result<Arc<dyn ModelTransport>> {
    build_model_with_timeout(Duration::from_secs(120))
}

/// Live coding cells: one tool-using round on a reasoning model can exceed
/// the retention-smoke HTTP timeout.
pub(crate) fn build_live_coding_model() -> anyhow::Result<Arc<dyn ModelTransport>> {
    build_model_with_timeout(Duration::from_secs(300))
}

fn build_model_with_timeout(timeout: Duration) -> anyhow::Result<Arc<dyn ModelTransport>> {
    let api_key = crate::envfile::get("OPENAI_API_KEY").context(
        "OPENAI_API_KEY is not set — put it in eval.env (gitignored) or the process environment",
    )?;
    if api_key.trim().is_empty() {
        anyhow::bail!("OPENAI_API_KEY is empty — put it in eval.env, do not paste it into chat");
    }
    let base_url = crate::envfile::get("OPENAI_BASE_URL")
        .unwrap_or_else(|| "https://api.openai.com/v1".to_string());
    let model = crate::envfile::get("OPENAI_MODEL").unwrap_or_else(|| "gpt-4o-mini".to_string());
    let provider = OpenAiProvider::new(OpenAiConfig {
        api_key,
        base_url,
        model,
        max_output_tokens: 4096,
        timeout,
        send_stream_options: true,
        send_max_tokens: true,
        max_stream_bytes: provider_openai::DEFAULT_MAX_STREAM_BYTES,
    });
    Ok(Arc::new(RetryingTransport::new(
        provider,
        3,
        Duration::from_millis(500),
    )))
}

/// Run one engine through the constraint-retention script and measure the
/// outcome. `prompts` is the turn script; `verify` scores the final answer.
pub async fn run_eval(
    engine: &'static str,
    policy: &str,
    prompts: &[String],
    verify: impl Fn(&str) -> bool,
) -> anyhow::Result<EvalSummary> {
    let context_engine: Arc<dyn ContextEngine> = match policy {
        "append" => Arc::new(AppendOnlyEngine::new()),
        "rolling" => Arc::new(RollingSummaryEngine::with_config(RollingConfig::default())),
        "dynamic" => Arc::new(SimpleContextEngine::new(SimpleContextConfig::default())),
        other => anyhow::bail!("unknown --engine policy: {other}"),
    };

    let model = build_model()?;
    let approval: Arc<dyn ApprovalGate> = Arc::new(AllowAllGate);

    let mut host = ModuleHost::new();
    host.add_module(Arc::new(ContextModule::new(context_engine)))?;
    host.add_module(Arc::new(ModelModule::new(model)))?;
    host.add_module(Arc::new(agent_runtime::ToolModule::new(Arc::new(
        NoToolDispatcher,
    ))))?;
    host.add_module(Arc::new(ApprovalModule::new(approval)))?;
    host.start().await?;

    let services = RuntimeServices::from_registry(host.registry(), CoreAuthorityConfig::default())?;
    let runtime = RuntimeInstance::spawn(host, services);
    // Subscribe before start so no event is missed (the broadcast channel
    // only delivers what is sent after subscription).
    let mut events = runtime.handle().subscribe();
    runtime.start().await?;

    let mut summary = EvalSummary::new(engine);
    let mut passed = false;
    let mut final_answer = String::new();

    for (index, prompt) in prompts.iter().enumerate() {
        runtime.handle().user_message(prompt.clone()).await?;
        summary.user_turns += 1;
        if let Err(message) = wait_for_turn(&mut events, &mut summary, &mut final_answer).await {
            summary.note = message;
            break;
        }
        // Only the final turn's answer is graded.
        if index + 1 == prompts.len() && verify(&final_answer) {
            passed = true;
        }
    }
    if !passed && summary.note.is_empty() {
        summary.note = format!("no correct answer after {} turns", prompts.len());
    }

    runtime.shutdown().await?;
    summary.passed = passed;
    summary.final_answer = final_answer;
    Ok(summary)
}

/// Read events until the current turn ends, accumulating usage, tool counts
/// and the turn's final assistant message. Returns `Err` with a reason when
/// the turn cannot finish (timeout, commit failure, runtime error).
async fn wait_for_turn(
    events: &mut broadcast::Receiver<RuntimeEventEnvelope>,
    summary: &mut EvalSummary,
    final_answer: &mut String,
) -> Result<(), String> {
    loop {
        match tokio::time::timeout(Duration::from_secs(600), events.recv()).await {
            Err(_) => return Err("turn timed out".into()),
            Ok(Err(broadcast::error::RecvError::Lagged(_))) => continue,
            Ok(Err(broadcast::error::RecvError::Closed)) => {
                return Err("event stream closed".into());
            }
            Ok(Ok(envelope)) => match envelope.event {
                RuntimeEvent::ModelUsed {
                    input_tokens,
                    output_tokens,
                } => {
                    summary.input_tokens += input_tokens;
                    summary.output_tokens += output_tokens;
                }
                RuntimeEvent::AssistantMessage { content } => {
                    final_answer.clear();
                    final_answer.push_str(&content);
                }
                RuntimeEvent::TurnCompleted => return Ok(()),
                RuntimeEvent::TurnCommitFailed { message, .. } => {
                    return Err(format!("turn commit failed: {message}"));
                }
                RuntimeEvent::Error { message } => return Err(format!("runtime error: {message}")),
                _ => {}
            },
        }
    }
}

/// Render the A/B/C comparison table.
pub fn render_comparison(results: &[EvalSummary]) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "  {:18} {:>11} {:>11} {:>10} {:>7}  note\n",
        "engine", "input_tok", "output_tok", "user_turns", "passed"
    ));
    for result in results {
        out.push_str(&format!(
            "  {:18} {:>11} {:>11} {:>10} {:>7}  {}\n",
            result.engine,
            result.input_tokens,
            result.output_tokens,
            result.user_turns,
            if result.passed { "yes" } else { "no" },
            result.note,
        ));
        if !result.final_answer.is_empty() {
            let preview: String = result.final_answer.chars().take(160).collect();
            out.push_str(&format!("    final: {preview}\n"));
        }
    }
    out
}
