use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{
    AgentResult, CancellationToken, ScopeId, ToolCall, ToolOutput, ToolResultDisposition, ToolSpec,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ModelRole {
    System,
    User,
    Assistant,
    Tool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelMessage {
    pub role: ModelRole,
    pub content: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Assistant tool calls attached to this message (role == Assistant).
    /// Part of the runtime turn frame, never part of the long-term working set.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ToolCall>,
    /// Which assistant tool call this result answers (role == Tool).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
}

impl ModelMessage {
    pub fn system(content: impl Into<String>) -> Self {
        Self {
            role: ModelRole::System,
            content: content.into(),
            name: None,
            tool_calls: Vec::new(),
            tool_call_id: None,
        }
    }

    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: ModelRole::User,
            content: content.into(),
            name: None,
            tool_calls: Vec::new(),
            tool_call_id: None,
        }
    }

    pub fn assistant(content: impl Into<String>) -> Self {
        Self {
            role: ModelRole::Assistant,
            content: content.into(),
            name: None,
            tool_calls: Vec::new(),
            tool_call_id: None,
        }
    }

    /// Assistant message that carries one or more tool calls (empty content).
    pub fn assistant_tool_calls(calls: Vec<ToolCall>) -> Self {
        Self {
            role: ModelRole::Assistant,
            content: String::new(),
            name: None,
            tool_calls: calls,
            tool_call_id: None,
        }
    }

    pub fn tool(name: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            role: ModelRole::Tool,
            content: content.into(),
            name: Some(name.into()),
            tool_calls: Vec::new(),
            tool_call_id: None,
        }
    }

    /// Tool result paired with the assistant call it answers.
    pub fn tool_result(
        call_id: impl Into<String>,
        name: impl Into<String>,
        content: impl Into<String>,
    ) -> Self {
        Self {
            role: ModelRole::Tool,
            content: content.into(),
            name: Some(name.into()),
            tool_calls: Vec::new(),
            tool_call_id: Some(call_id.into()),
        }
    }
}

/// One ordered step of the current turn's execution stack: either the
/// assistant's tool calls or the result that answers them.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum TurnFrameStep {
    AssistantToolCalls {
        calls: Vec<ToolCall>,
    },
    ToolResult {
        output: ToolOutput,
        /// The tool scope this result belongs to, opened by the runtime at
        /// tool start; the persisted observation is tagged with it.
        #[serde(default)]
        scope_id: Option<ScopeId>,
        /// Whether this result becomes a long-term observation at turn end.
        /// Context retrieval results are transient: they must not duplicate
        /// fetched evidence under a new item id.
        #[serde(default)]
        disposition: ToolResultDisposition,
        /// Typed host-trusted execution facts, captured from
        /// the dispatcher lane when the result landed. `None` marks frames
        /// that predate the channel — consumers fall back to the legacy
        /// metadata derivation for exactly those. Boxed to keep the enum
        /// variants within a word of each other.
        #[serde(default)]
        facts: Option<Box<crate::execution_facts::ToolExecutionFacts>>,
    },
}

/// The runtime-owned execution stack of one turn: the current user message
/// followed by every assistant tool call / tool result in order.
///
/// This is not long-term memory. It is held by the runtime and never scored,
/// garbage-collected or evicted while the turn is open; when the turn ends it
/// is dropped, and the observations it carried are persisted to the context
/// engine as the long-term record of the turn.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TurnFrame {
    pub user_message: String,
    pub steps: Vec<TurnFrameStep>,
}

impl TurnFrame {
    pub fn new(user_message: impl Into<String>) -> Self {
        Self {
            user_message: user_message.into(),
            steps: Vec::new(),
        }
    }

    pub fn push_tool_calls(&mut self, calls: Vec<ToolCall>) {
        if !calls.is_empty() {
            self.steps.push(TurnFrameStep::AssistantToolCalls { calls });
        }
    }

    pub fn push_tool_result(
        &mut self,
        output: ToolOutput,
        scope_id: Option<ScopeId>,
        facts: crate::execution_facts::ToolExecutionFacts,
    ) {
        self.push_tool_result_with(
            output,
            scope_id,
            ToolResultDisposition::PersistObservation,
            facts,
        );
    }

    /// Push a tool result with an explicit persist disposition: context
    /// retrieval results are `TransientNoPersist`.
    pub fn push_tool_result_with(
        &mut self,
        output: ToolOutput,
        scope_id: Option<ScopeId>,
        disposition: ToolResultDisposition,
        facts: crate::execution_facts::ToolExecutionFacts,
    ) {
        self.steps.push(TurnFrameStep::ToolResult {
            output,
            scope_id,
            disposition,
            facts: Some(Box::new(facts)),
        });
    }

    pub fn has_tool_steps(&self) -> bool {
        self.steps
            .iter()
            .any(|step| matches!(step, TurnFrameStep::AssistantToolCalls { .. }))
    }

    /// Persistable tool results already on this turn. A structurally empty
    /// model stop after these is a real "I'm done"; the same empty 0/0
    /// before any such delta is a transport/parser hole.
    pub fn has_persistable_tool_delta(&self) -> bool {
        self.steps.iter().any(|step| {
            matches!(
                step,
                TurnFrameStep::ToolResult {
                    disposition: ToolResultDisposition::PersistObservation,
                    ..
                }
            )
        })
    }

    /// Deterministic turn checkpointing (the Protocol Working Set):
    /// split the frame into the retained tail and the number of older
    /// completed exchanges compacted away from the model-facing wire
    /// view. An exchange is one assistant tool-call group plus the
    /// results answering it; whole groups are dropped so the wire
    /// protocol keeps every tool call paired with its result. The
    /// trailing region is always retained in full — including an
    /// in-flight last group. The full frame stays untouched for audit
    /// and turn-end persistence; only the wire projection shrinks.
    pub fn checkpoint_tail(&self, keep: usize) -> (TurnFrame, usize) {
        let Some((retain_from, compacted)) = self.checkpoint_boundary(keep) else {
            return (self.clone(), 0);
        };
        let mut tail = TurnFrame::new(self.user_message.clone());
        tail.steps = self.steps[retain_from..].to_vec();
        (tail, compacted)
    }

    /// Model-facing checkpoint projection: the same retained protocol tail
    /// plus a tiny receipt index for persistable results that fell out of
    /// the tail. Receipts contain no tool body and do not claim currentness;
    /// they only prevent the generic count note from erasing which checks
    /// already completed.
    pub fn checkpoint(&self, keep: usize) -> (TurnFrame, TurnCheckpoint) {
        let Some((retain_from, compacted_exchanges)) = self.checkpoint_boundary(keep) else {
            return (self.clone(), TurnCheckpoint::default());
        };
        let mut tail = TurnFrame::new(self.user_message.clone());
        tail.steps = self.steps[retain_from..].to_vec();
        let receipts = checkpoint_receipts(&self.steps[..retain_from]);
        (
            tail,
            TurnCheckpoint {
                compacted_exchanges,
                receipts,
            },
        )
    }

    fn checkpoint_boundary(&self, keep: usize) -> Option<(usize, usize)> {
        let group_starts: Vec<usize> = self
            .steps
            .iter()
            .enumerate()
            .filter(|(_, step)| matches!(step, TurnFrameStep::AssistantToolCalls { .. }))
            .map(|(index, _)| index)
            .collect();
        let total = group_starts.len();
        if total <= keep {
            return None;
        }
        let retain_from = if keep == 0 {
            self.steps.len()
        } else {
            group_starts[total - keep]
        };
        Some((retain_from, total - keep))
    }

    /// The wire view for one model request: the retained tail plus the
    /// bounded checkpoint note when older exchanges were compacted.
    pub fn checkpointed_messages(&self, keep: usize) -> Vec<ModelMessage> {
        let (tail, checkpoint) = self.checkpoint(keep);
        let mut messages = tail.messages();
        if checkpoint.compacted_exchanges > 0 {
            messages.insert(
                1,
                ModelMessage::user(turn_checkpoint_note_with_receipts(&checkpoint)),
            );
        }
        messages
    }

    /// Render the stack as protocol messages: the user message first, then
    /// assistant(tool_calls) / tool(tool_call_id) pairs in execution order.
    pub fn messages(&self) -> Vec<ModelMessage> {
        let mut messages = vec![ModelMessage::user(self.user_message.clone())];
        for step in &self.steps {
            match step {
                TurnFrameStep::AssistantToolCalls { calls } => {
                    messages.push(ModelMessage::assistant_tool_calls(calls.clone()));
                }
                TurnFrameStep::ToolResult { output, .. } => {
                    messages.push(ModelMessage::tool_result(
                        &output.call_id,
                        &output.tool_name,
                        &output.model_content,
                    ));
                }
            }
        }
        messages
    }
}

/// How many completed tool exchanges of the current turn stay in the
/// model-facing protocol view. Older exchanges are compacted to a
/// bounded deterministic note; their reliable facts already live in
/// TASK PROGRESS, artifacts, and the run journal.
pub const TURN_FRAME_KEEP_EXCHANGES: usize = 6;

/// Maximum persistable outcome receipts retained in one checkpoint note.
/// This is an index, not a transcript: tool bodies and arguments stay out.
pub const MAX_TURN_CHECKPOINT_RECEIPTS: usize = 6;
/// Hard cap for one rendered receipt, including tool name and status.
pub const MAX_TURN_CHECKPOINT_RECEIPT_CHARS: usize = 96;
const MAX_TURN_CHECKPOINT_TOOL_NAME_CHARS: usize = 48;

/// 单轮最多回注的协议正文行数。与运行时当轮缓存的
/// 容量一致；回注内容只进 user-role 焦点层，不进 Context / 不持久。
pub const MAX_PROTOCOL_BODY_ROWS: usize = 4;

/// The original count-only checkpoint note. Kept as a stable public helper
/// for callers that do not carry a [`TurnCheckpoint`] receipt index.
pub fn turn_checkpoint_note(compacted_exchanges: usize) -> String {
    turn_checkpoint_note_with_receipts(&TurnCheckpoint {
        compacted_exchanges,
        receipts: Vec::new(),
    })
}

/// The bounded deterministic checkpoint note injected into the wire view
/// when older exchanges are compacted away. Rendering re-bounds every row
/// so older or externally deserialized checkpoints cannot inflate a prompt.
pub fn turn_checkpoint_note_with_receipts(checkpoint: &TurnCheckpoint) -> String {
    let mut note = format!(
        "TURN CHECKPOINT: {} earlier tool exchange(s) were compacted from this protocol view. Their reliable current facts are reflected in TASK PROGRESS; the full audit trail remains in the run journal. Do not assume the compacted exchanges are still pending.",
        checkpoint.compacted_exchanges
    );
    if !checkpoint.receipts.is_empty() {
        note.push_str("\nRECENT COMPACTED RECEIPTS (outcomes only; not bodies or currentness):");
        for receipt in checkpoint
            .receipts
            .iter()
            .take(MAX_TURN_CHECKPOINT_RECEIPTS)
        {
            note.push_str("\n- ");
            note.push_str(&bound_checkpoint_receipt(receipt));
        }
    }
    note
}

fn checkpoint_receipts(compacted_steps: &[TurnFrameStep]) -> Vec<String> {
    let mut newest_first = Vec::new();
    for step in compacted_steps.iter().rev() {
        let TurnFrameStep::ToolResult {
            output,
            disposition: ToolResultDisposition::PersistObservation,
            ..
        } = step
        else {
            continue;
        };
        let status = if output.ok { "ok" } else { "failed" };
        let tool_name = output
            .tool_name
            .chars()
            .take(MAX_TURN_CHECKPOINT_TOOL_NAME_CHARS)
            .collect::<String>();
        let raw = format!("{tool_name} {status}: {}", output.summary);
        let receipt = bound_checkpoint_receipt(&raw);
        if !newest_first.contains(&receipt) {
            newest_first.push(receipt);
            if newest_first.len() >= MAX_TURN_CHECKPOINT_RECEIPTS {
                break;
            }
        }
    }
    newest_first.reverse();
    newest_first
}

fn bound_checkpoint_receipt(receipt: &str) -> String {
    let mut bounded = String::with_capacity(MAX_TURN_CHECKPOINT_RECEIPT_CHARS);
    let mut chars = 0;
    let mut pending_space = false;
    let mut truncated = false;
    for ch in receipt.chars() {
        if ch.is_whitespace() {
            pending_space = chars > 0;
            continue;
        }
        let needed = usize::from(pending_space) + 1;
        if chars + needed > MAX_TURN_CHECKPOINT_RECEIPT_CHARS {
            truncated = true;
            break;
        }
        if pending_space {
            bounded.push(' ');
            chars += 1;
            pending_space = false;
        }
        bounded.push(ch);
        chars += 1;
    }
    if truncated {
        if chars == MAX_TURN_CHECKPOINT_RECEIPT_CHARS {
            bounded.pop();
        }
        bounded.push('…');
    }
    bounded
}

/// Message placement only: never a Context selection, authority, or budget
/// policy. Old serialized inputs retain their historical order.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PromptLayout {
    #[default]
    Legacy,
    CurrentStateLast,
}

/// The logical model-input layers assembled by the runtime for one request:
///
/// ```text
/// System Policy        - standing instructions for every request
/// Context Frame        - the long-term working set, from ContextEngine::materialize
/// Turn Frame           - the current turn's execution stack, owned by the runtime
/// Current State        - tool catalog plus the current Focus/TaskAnchor/TaskProgress
/// Active Tool Schemas  - tool definitions for this request (ModelRequest.tools)
/// ```
///
/// Layers are kept separate so the context engine never has to understand the
/// execution protocol, and the runtime never has to score long-term memory.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ModelInput {
    #[serde(default)]
    pub layout: PromptLayout,
    pub system_policy: Vec<ModelMessage>,
    /// Runtime-owned changing observations/policy (currently the tool
    /// catalog). Each message retains its original role and complete text.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub current_state_frame: Vec<ModelMessage>,
    pub focus_frame: Option<String>,
    pub context_frame: Vec<ModelMessage>,
    pub turn_frame: TurnFrame,
    pub tool_schemas: Vec<ToolSpec>,
    /// Deterministic turn checkpointing: when present, `turn_frame` is
    /// already the retained tail and this many older completed
    /// exchanges were compacted out of the wire view (audit and
    /// turn-end persistence still see the full frame).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_checkpoint: Option<TurnCheckpoint>,
}

/// How many completed exchanges a model input's turn frame compacted
/// away. `Default` is meaningful: an absent checkpoint is the
/// nothing-compacted case for pre-checkpoint traces.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct TurnCheckpoint {
    pub compacted_exchanges: usize,
    /// Latest persistable outcomes from the compacted prefix. Each row is
    /// bounded and contains no raw tool body or arguments.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub receipts: Vec<String>,
}

/// Schema-free checkpoint accounting carried by `ModelStarted`. The event
/// records only counts; receipt text remains in the ephemeral model input.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct TurnCheckpointStats {
    pub compacted_exchanges: u64,
    pub receipt_count: u64,
}

impl From<&TurnCheckpoint> for TurnCheckpointStats {
    fn from(checkpoint: &TurnCheckpoint) -> Self {
        Self {
            compacted_exchanges: checkpoint.compacted_exchanges as u64,
            receipt_count: checkpoint.receipts.len().min(MAX_TURN_CHECKPOINT_RECEIPTS) as u64,
        }
    }
}

impl ModelInput {
    /// Convert the final packed input into a request and bind one reusable
    /// prefix. Call only after packing/coverage checks, never on an earlier
    /// selection. Legacy stops before its changing state; CurrentStateLast
    /// can also include the actual retained evidence. The turn is excluded.
    pub fn into_request(self, metadata: Value, cancel: CancellationToken) -> ModelRequest {
        let prefix_len = self.system_policy.len()
            + if self.layout == PromptLayout::CurrentStateLast {
                self.context_frame.len()
            } else {
                0
            };
        let messages = self.into_messages();
        let prefix_len = messages[..prefix_len]
            .iter()
            .rposition(|message| !message.content.is_empty())
            .map_or(0, |index| index + 1);
        let mut request = ModelRequest {
            messages,
            tools: self.tool_schemas,
            metadata,
            cancel,
            max_output_tokens: None,
        };
        request.bind_prompt_reuse_boundary(prefix_len);
        request
    }

    /// The wire turn-frame view: the retained tail plus the bounded
    /// checkpoint note when older exchanges were compacted.
    pub fn turn_frame_wire_messages(&self) -> Vec<ModelMessage> {
        let mut messages = self.turn_frame.messages();
        if let Some(checkpoint) = self.turn_checkpoint.as_ref()
            && checkpoint.compacted_exchanges > 0
        {
            messages.insert(
                1,
                ModelMessage::user(turn_checkpoint_note_with_receipts(checkpoint)),
            );
        }
        messages
    }

    /// Flatten without changing any layer's contents, selected-item order,
    /// message roles, or tool call/result pairing. CurrentStateLast places
    /// the current catalog and Focus after the complete retained protocol
    /// stack, so progress updates do not rewrite the preceding evidence.
    /// Legacy is also used when decoding inputs that predate this layout.
    pub fn into_messages(&self) -> Vec<ModelMessage> {
        let mut messages = Vec::new();
        messages.extend(self.system_policy.iter().cloned());
        if self.layout == PromptLayout::Legacy {
            self.append_current_state(&mut messages);
        }
        messages.extend(self.context_frame.iter().cloned());
        messages.extend(self.turn_frame_wire_messages());
        if self.layout == PromptLayout::CurrentStateLast {
            self.append_current_state(&mut messages);
        }
        messages
    }

    fn append_current_state(&self, messages: &mut Vec<ModelMessage>) {
        messages.extend(self.current_state_frame.iter().cloned());
        if let Some(focus) = &self.focus_frame {
            messages.push(ModelMessage::system(focus));
        }
    }
}

/// Per-request prompt-layer token accounting. Sums across `ModelStarted`
/// events tell whether C grew because of historical context or because
/// TaskProgress / Focus itself got longer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct PromptLayerCosts {
    pub system_tokens: u64,
    pub runtime_facts_tokens: u64,
    pub task_anchor_tokens: u64,
    pub task_progress_tokens: u64,
    pub current_focus_tokens: u64,
    /// Selected working-set context. Restored protocol bodies are recorded
    /// separately (`restored_protocol_tokens`), so growth in one layer is
    /// not attributed to the other.
    pub historical_context_tokens: u64,
    /// Token cost of rehydrated protocol bodies the checkpoint spilled
    /// (`RESTORED TURN BODIES`). Older events without the field decode as
    /// zero and keep their historical accounting unchanged.
    #[serde(default)]
    pub restored_protocol_tokens: u64,
    pub turn_frame_tokens: u64,
    pub tool_schema_tokens: u64,
    /// Bounded TOOL CATALOG index (names not on this round's schema surface).
    #[serde(default)]
    pub tool_catalog_index_tokens: u64,
}

impl PromptLayerCosts {
    pub fn saturating_add(self, other: Self) -> Self {
        Self {
            system_tokens: self.system_tokens.saturating_add(other.system_tokens),
            runtime_facts_tokens: self
                .runtime_facts_tokens
                .saturating_add(other.runtime_facts_tokens),
            task_anchor_tokens: self
                .task_anchor_tokens
                .saturating_add(other.task_anchor_tokens),
            task_progress_tokens: self
                .task_progress_tokens
                .saturating_add(other.task_progress_tokens),
            current_focus_tokens: self
                .current_focus_tokens
                .saturating_add(other.current_focus_tokens),
            historical_context_tokens: self
                .historical_context_tokens
                .saturating_add(other.historical_context_tokens),
            restored_protocol_tokens: self
                .restored_protocol_tokens
                .saturating_add(other.restored_protocol_tokens),
            turn_frame_tokens: self
                .turn_frame_tokens
                .saturating_add(other.turn_frame_tokens),
            tool_schema_tokens: self
                .tool_schema_tokens
                .saturating_add(other.tool_schema_tokens),
            tool_catalog_index_tokens: self
                .tool_catalog_index_tokens
                .saturating_add(other.tool_catalog_index_tokens),
        }
    }
}

/// Provider capability declaration. The kernel/UI can branch on this without
/// vendor-specific knowledge.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ModelCapabilities {
    pub streaming: bool,
    pub tool_calls: bool,
    pub max_output_tokens: usize,
    /// The provider's declared context window in tokens. When absent the
    /// runtime falls back to its configured budget — the context engine only
    /// ever sees the derived context-frame share either way.
    #[serde(default)]
    pub context_window: Option<usize>,
}

/// A bounded chunk of a streaming model response, normalized by the provider
/// adapter. The kernel forwards these to live UI subscribers; the final
/// `ModelOutput` remains the source of truth for the model turn.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ModelChunk {
    TextDelta {
        delta: String,
    },
    ToolCallDelta {
        call_id: String,
        #[serde(default)]
        name: Option<String>,
        #[serde(default)]
        arguments_delta: String,
    },
    /// A bounded progress marker emitted between retryable provider
    /// attempts. It carries no model content and is never prompt history.
    Retrying {
        attempt: u32,
        delay_ms: u64,
    },
    Done,
}

/// Receives streaming chunks. Implementations must be cheap: this runs on the
/// model hot path.
#[async_trait]
pub trait ModelEventSink: Send + Sync {
    /// Whether successfully delivering this chunk creates externally visible
    /// state that makes replaying the model request unsafe. The default is
    /// fail-closed because an arbitrary sink may publish every chunk.
    ///
    /// A sink may return `false` for protocol-internal chunks that it consumes
    /// without exposing them. Retry wrappers use this signal only to decide
    /// whether a failed attempt can be discarded and reissued; it never makes
    /// a published text delta rewindable.
    fn creates_replay_barrier(&self, _chunk: &ModelChunk) -> bool {
        true
    }

    async fn on_chunk(&self, chunk: ModelChunk) -> AgentResult<()>;
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ModelRequest {
    pub messages: Vec<ModelMessage>,
    pub tools: Vec<ToolSpec>,
    #[serde(default)]
    pub metadata: Value,
    /// COST-4 (D02): an optional per-request output ceiling. A caller with a
    /// bounded-output contract (the compactor's 512-char cap) states it on
    /// the request so the provider stops generating at the bound instead of
    /// the transport's profile default — generation cost is limited, not
    /// merely truncated afterwards. Honored only where the transport
    /// negotiates a max-output field (`send_max_tokens`); `None` keeps the
    /// profile behavior byte-for-byte.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<u32>,
    /// Cooperative cancellation handle for this request. Not serialized.
    #[serde(skip)]
    pub cancel: CancellationToken,
}

/// One model round's token counters. COST-6 (R2-10): the cache buckets are
/// the provider's own observations with endpoint-defined containment —
/// `input_tokens` is the provider's total and the cache buckets typically
/// partition or subset it, but the relation is per endpoint/protocol and is
/// never repaired or re-derived here: no counter is filled by arithmetic on
/// the others, and an unreported counter stays `None` (never an invented
/// zero). Summing `input_tokens` with any cache bucket double-counts.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ModelUsage {
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    /// Provider-reported prompt tokens served from a prefix cache
    /// (`input_tokens_details.cached_tokens` for Responses,
    /// `prompt_tokens_details.cached_tokens` for Chat Completions, with the
    /// DeepSeek top-level `prompt_cache_hit_tokens` as fallback).
    /// `None` when the provider did not report them.
    #[serde(default)]
    pub cached_input_tokens: Option<u64>,
    /// COST-2 (E05.4): provider-reported prompt tokens WRITTEN to the
    /// prefix cache, filled only from an EXPLICIT provider write counter
    /// (`input_tokens_details.cache_write_tokens` /
    /// `prompt_tokens_details.cache_write_tokens`). A cache miss is not a
    /// write: the uncached input is reported separately. `None` when the
    /// provider does not report one — the uncached remainder of the input
    /// stays derivable as `input_tokens - cached_input_tokens` and must
    /// never be invented here.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_write_input_tokens: Option<u64>,
    /// COST-6 (R2-10): provider-reported input tokens that MISSED the
    /// prefix cache (DeepSeek's top-level `prompt_cache_miss_tokens`). This
    /// is the uncached part of the input — a read-side observation, not
    /// evidence of a cache write and not billed as one. `None` when the
    /// provider did not report it. Events emitted before COST-6 may carry a
    /// DeepSeek miss count in `cache_write_input_tokens`; those historical
    /// records are preserved as-is and must not be silently re-read as
    /// trusted write costs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_miss_input_tokens: Option<u64>,
    /// Transport attempts that produced this output. `0` on legacy events
    /// means unknown (treat as one successful attempt). Failed attempts
    /// usually report no usage, so recorded tokens are a lower bound when
    /// `retries > 0`.
    #[serde(default)]
    pub attempts: u32,
    /// `attempts.saturating_sub(1)` when known.
    #[serde(default)]
    pub retries: u32,
}

/// The evidence identity of one model-call role's token record (CORE-4):
/// every accounting row must say where its numbers came from, because the
/// three classes aggregate differently and none may masquerade as another.
/// Unknown is never zero — a lost usage is still a real cost.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UsageIdentity {
    /// Exact evidence: the counters come from the provider's own report,
    /// or the role made no model call at all so zero is a fact.
    #[default]
    Observed,
    /// At least partly derived by the runtime (bounded approximation over
    /// rendered text). Never presented as provider-reported.
    Estimated,
    /// No usable evidence (aborted round, usage-less failure, legacy
    /// record predating typed usage). The numeric fields are a lower
    /// bound of zero and must not be summed as observed consumption.
    Unknown,
}

impl ModelUsage {
    /// The evidence identity of one provider round: both counters reported
    /// means observed; any missing counter means unknown (the reported
    /// side stays a lower bound, never a complete bill).
    pub fn usage_identity(&self) -> UsageIdentity {
        match (self.input_tokens, self.output_tokens) {
            (Some(_), Some(_)) => UsageIdentity::Observed,
            _ => UsageIdentity::Unknown,
        }
    }

    /// COST-7 (R3-12): true when the provider reported at least one
    /// counter — the minimum for a failure to travel wrapped in
    /// [`crate::AgentError::FailedWithUsage`]. An empty envelope is not
    /// evidence.
    pub fn has_any_reported(&self) -> bool {
        self.input_tokens.is_some()
            || self.output_tokens.is_some()
            || self.cached_input_tokens.is_some()
            || self.cache_write_input_tokens.is_some()
            || self.cache_miss_input_tokens.is_some()
    }
}

/// Which call lane a usage row belongs to (COST-7, R2-11): the main model
/// rounds and the compactor's maintenance calls are different cost centers,
/// so a row that only says "unknown" must still say WHERE the unknown cost
/// sits. Legacy rows default to the main lane.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelCallRole {
    /// The task's main model rounds.
    #[default]
    Main,
    /// A bounded compaction/maintenance call (compressor, distiller) —
    /// possibly on the independent maintenance transport.
    Maintenance,
}

/// Whether a model round is a usable completion or a transport/parser hole.
/// An empty assistant with no tool calls is only an anomaly when usage is
/// also missing (`0/0`); a billed empty stop is a real "I'm done".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelCompletionValidity {
    Meaningful,
    StructurallyEmpty,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelOutput {
    pub content: String,
    #[serde(default)]
    pub tool_calls: Vec<ToolCall>,
    #[serde(default)]
    pub usage: ModelUsage,
}

/// Hard safety bound for one provider-produced parallel tool batch. This
/// limits the actor queue, TurnFrame and result-delivery root set; it is not a
/// convergence target and never tells the model how many calls it should use.
pub const MAX_MODEL_TOOL_CALLS_PER_ROUND: usize = 32;

impl ModelOutput {
    pub fn completion_validity(&self) -> ModelCompletionValidity {
        completion_validity(&self.content, &self.tool_calls, &self.usage)
    }
}

/// `content` empty, no tool calls, and both usage counters absent or zero.
pub fn completion_validity(
    content: &str,
    tool_calls: &[ToolCall],
    usage: &ModelUsage,
) -> ModelCompletionValidity {
    let input = usage.input_tokens.unwrap_or(0);
    let output = usage.output_tokens.unwrap_or(0);
    if content.trim().is_empty() && tool_calls.is_empty() && input == 0 && output == 0 {
        ModelCompletionValidity::StructurallyEmpty
    } else {
        ModelCompletionValidity::Meaningful
    }
}

#[async_trait]
pub trait ModelTransport: Send + Sync {
    fn capabilities(&self) -> ModelCapabilities;

    async fn complete(&self, request: ModelRequest) -> AgentResult<ModelOutput>;

    /// Stream the response into `sink` and return the final assembled output.
    ///
    /// The default implementation bridges a non-streaming `complete` into a
    /// single delta, so every transport can be used with the streaming kernel
    /// loop. Streaming-capable providers override this to emit real deltas.
    async fn complete_stream(
        &self,
        request: ModelRequest,
        sink: &dyn ModelEventSink,
    ) -> AgentResult<ModelOutput> {
        let output = self.complete(request).await?;
        if !output.content.is_empty() {
            sink.on_chunk(ModelChunk::TextDelta {
                delta: output.content.clone(),
            })
            .await?;
        }
        for call in &output.tool_calls {
            sink.on_chunk(ModelChunk::ToolCallDelta {
                call_id: call.id.clone(),
                name: Some(call.name.clone()),
                arguments_delta: call.arguments.to_string(),
            })
            .await?;
        }
        sink.on_chunk(ModelChunk::Done).await?;
        Ok(output)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn tool_call(id: &str) -> ToolCall {
        ToolCall {
            id: id.into(),
            name: "fs.read".into(),
            arguments: json!({"path": "src/main.rs"}),
        }
    }

    #[test]
    fn turn_frame_renders_protocol_order() {
        let mut turn = TurnFrame::new("list the files");
        turn.push_tool_calls(vec![tool_call("call-1")]);
        turn.push_tool_result(
            ToolOutput {
                call_id: "call-1".into(),
                tool_name: "fs.read".into(),
                ok: true,
                summary: "read".into(),
                model_content: "fn main() {}".into(),
                artifact_ref: None,
                metadata: json!({}),
            },
            None,
            crate::ToolExecutionFacts::empty(),
        );

        let messages = turn.messages();
        assert_eq!(messages.len(), 3);
        assert_eq!(messages[0].role, ModelRole::User);
        assert_eq!(messages[0].content, "list the files");

        assert_eq!(messages[1].role, ModelRole::Assistant);
        assert!(messages[1].content.is_empty());
        assert_eq!(messages[1].tool_calls.len(), 1);
        assert_eq!(messages[1].tool_calls[0].id, "call-1");

        assert_eq!(messages[2].role, ModelRole::Tool);
        assert_eq!(messages[2].tool_call_id.as_deref(), Some("call-1"));
        assert_eq!(messages[2].content, "fn main() {}");
    }

    #[test]
    fn model_input_flattens_five_layers_in_order() {
        let mut turn = TurnFrame::new("continue");
        turn.push_tool_calls(vec![tool_call("c2")]);
        turn.push_tool_result(
            ToolOutput {
                call_id: "c2".into(),
                tool_name: "fs.read".into(),
                ok: true,
                summary: "read".into(),
                model_content: "content".into(),
                artifact_ref: None,
                metadata: json!({}),
            },
            None,
            crate::ToolExecutionFacts::empty(),
        );
        let input = ModelInput {
            layout: PromptLayout::Legacy,
            system_policy: vec![ModelMessage::system("policy")],
            current_state_frame: Vec::new(),
            focus_frame: Some("goal text".into()),
            context_frame: vec![ModelMessage::user("SELECTED WORKING CONTEXT")],
            turn_frame: turn,
            tool_schemas: Vec::new(),
            turn_checkpoint: None,
        };

        let messages = input.into_messages();
        assert_eq!(
            messages
                .iter()
                .map(|m| (m.role, m.content.as_str()))
                .collect::<Vec<_>>(),
            vec![
                (ModelRole::System, "policy"),
                (ModelRole::System, "goal text"),
                (ModelRole::User, "SELECTED WORKING CONTEXT"),
                (ModelRole::User, "continue"),
                (ModelRole::Assistant, ""),
                (ModelRole::Tool, "content"),
            ]
        );
    }

    #[test]
    fn current_state_last_preserves_complete_tool_groups_and_legacy_decoding() {
        let mut input = ModelInput {
            layout: PromptLayout::CurrentStateLast,
            system_policy: vec![ModelMessage::system("policy")],
            current_state_frame: vec![ModelMessage::system("catalog")],
            focus_frame: Some("current focus".into()),
            context_frame: vec![ModelMessage::user("evidence")],
            turn_frame: framed_turn(2),
            ..Default::default()
        };
        let current = input.into_messages();
        assert_eq!(current[0].content, "policy");
        assert_eq!(current[1].content, "evidence");
        assert_eq!(current[current.len() - 2].content, "catalog");
        assert_eq!(current.last().unwrap().content, "current focus");
        assert_eq!(current.last().unwrap().role, ModelRole::System);
        for pair in current[3..7].chunks_exact(2) {
            assert_eq!(pair[0].role, ModelRole::Assistant);
            assert_eq!(pair[1].role, ModelRole::Tool);
            assert_eq!(
                Some(&pair[0].tool_calls[0].id),
                pair[1].tool_call_id.as_ref()
            );
        }

        // Old records had the catalog inside system_policy and no layout
        // marker. Decoding must retain that exact historical wire order.
        input
            .system_policy
            .push(input.current_state_frame.remove(0));
        let mut legacy_json = serde_json::to_value(&input).unwrap();
        legacy_json.as_object_mut().unwrap().remove("layout");
        legacy_json
            .as_object_mut()
            .unwrap()
            .remove("current_state_frame");
        let legacy: ModelInput = serde_json::from_value(legacy_json).unwrap();
        assert_eq!(legacy.layout, PromptLayout::Legacy);
        let legacy = legacy.into_messages();
        assert_eq!(legacy[0].content, "policy");
        assert_eq!(legacy[1].content, "catalog");
        assert_eq!(legacy[2].content, "current focus");
        assert_eq!(legacy[3].content, "evidence");
        assert_eq!(legacy.last().unwrap().role, ModelRole::Tool);
    }

    /// A frame with `groups` complete exchanges (call + result pairs).
    fn framed_turn(groups: usize) -> TurnFrame {
        let mut turn = TurnFrame::new("fix the bug");
        for index in 0..groups {
            let id = format!("call-{index}");
            turn.push_tool_calls(vec![tool_call(&id)]);
            turn.push_tool_result(
                ToolOutput {
                    call_id: id,
                    tool_name: "fs.read".into(),
                    ok: true,
                    summary: "read".into(),
                    model_content: format!("content {index}"),
                    artifact_ref: None,
                    metadata: json!({}),
                },
                None,
                crate::ToolExecutionFacts::empty(),
            );
        }
        turn
    }

    #[test]
    fn checkpoint_tail_keeps_the_last_exchanges_and_counts_the_rest() {
        let turn = framed_turn(10);
        let (tail, compacted) = turn.checkpoint_tail(6);
        assert_eq!(compacted, 4);
        assert_eq!(tail.user_message, "fix the bug");
        // Six exchanges remain: 6 calls + 6 results.
        assert_eq!(tail.steps.len(), 12);
        // The retained tail starts at exchange 4, not 0.
        match &tail.steps[0] {
            TurnFrameStep::AssistantToolCalls { calls } => {
                assert_eq!(calls[0].id, "call-4");
            }
            other => panic!("tail must start with an assistant tool-call group: {other:?}"),
        }
        // The source frame is untouched (audit/persistence view).
        assert_eq!(turn.steps.len(), 20);

        // At or below the keep threshold nothing is compacted.
        let (same, compacted) = framed_turn(6).checkpoint_tail(6);
        assert_eq!(compacted, 0);
        assert_eq!(same.steps.len(), 12);

        // `keep = 0` is a valid fully compacted wire projection.
        let (empty, compacted) = framed_turn(2).checkpoint_tail(0);
        assert_eq!(compacted, 2);
        assert!(empty.steps.is_empty());
    }

    #[test]
    fn checkpoint_receipts_are_bounded_and_exclude_transient_or_raw_data() {
        let mut turn = TurnFrame::new("finish the task");
        for index in 0..9 {
            let call_id = format!("call-{index}");
            let tool_name = format!("tool.check_{index}");
            turn.push_tool_calls(vec![ToolCall {
                id: call_id.clone(),
                name: tool_name.clone(),
                arguments: json!({"secret_argument": format!("path-secret-{index}")}),
            }]);
            let disposition = if index == 2 {
                ToolResultDisposition::TransientNoPersist
            } else {
                ToolResultDisposition::PersistObservation
            };
            turn.push_tool_result_with(
                ToolOutput {
                    call_id,
                    tool_name,
                    ok: index != 6,
                    summary: if index == 7 {
                        format!("checked {index} {}", "x".repeat(200))
                    } else {
                        format!("checked\nitem {index}")
                    },
                    model_content: format!("raw-body-secret-{index}"),
                    artifact_ref: None,
                    metadata: json!({}),
                },
                None,
                disposition,
                crate::ToolExecutionFacts::empty(),
            );
        }

        let (tail, checkpoint) = turn.checkpoint(1);
        assert_eq!(checkpoint.compacted_exchanges, 8);
        assert_eq!(tail.steps.len(), 2);
        assert_eq!(checkpoint.receipts.len(), MAX_TURN_CHECKPOINT_RECEIPTS);
        assert!(checkpoint.receipts[0].starts_with("tool.check_1 ok:"));
        assert!(checkpoint.receipts[5].starts_with("tool.check_7 ok:"));
        assert!(
            checkpoint
                .receipts
                .iter()
                .any(|receipt| receipt.starts_with("tool.check_6 failed:"))
        );
        assert!(
            checkpoint
                .receipts
                .iter()
                .all(|receipt| receipt.chars().count() <= MAX_TURN_CHECKPOINT_RECEIPT_CHARS)
        );

        let note = turn_checkpoint_note_with_receipts(&checkpoint);
        assert!(!note.contains("tool.check_2"), "transient reads stay out");
        assert!(!note.contains("raw-body-secret"), "tool bodies stay out");
        assert!(!note.contains("path-secret"), "tool arguments stay out");
        assert!(!note.contains("\nitem"), "receipt rows are single-line");
    }

    #[test]
    fn checkpoint_note_rebounds_deserialized_receipts() {
        let checkpoint = TurnCheckpoint {
            compacted_exchanges: 99,
            receipts: (0..10)
                .map(|index| format!("row {index}\n{}", "x".repeat(200)))
                .collect(),
        };
        let note = turn_checkpoint_note_with_receipts(&checkpoint);
        let rows: Vec<&str> = note.lines().filter(|line| line.starts_with("- ")).collect();
        assert_eq!(rows.len(), MAX_TURN_CHECKPOINT_RECEIPTS);
        assert!(rows.iter().all(|row| {
            row.trim_start_matches("- ").chars().count() <= MAX_TURN_CHECKPOINT_RECEIPT_CHARS
        }));
        assert_eq!(turn_checkpoint_note(3).lines().count(), 1);
    }

    #[test]
    fn checkpointed_wire_view_pairs_every_tool_call_with_its_result() {
        let turn = framed_turn(9);
        let messages = turn.checkpointed_messages(6);
        // user message + checkpoint note + 6 × (assistant + tool).
        assert_eq!(messages.len(), 2 + 12);
        assert_eq!(messages[0].content, "fix the bug");
        assert!(
            messages[1]
                .content
                .starts_with("TURN CHECKPOINT: 3 earlier"),
            "the bounded note must render right after the directive: {}",
            messages[1].content
        );
        // Protocol invariant: every assistant tool call keeps its result.
        let mut open_calls: Vec<String> = Vec::new();
        for message in &messages {
            match message.role {
                ModelRole::Assistant => {
                    open_calls.extend(message.tool_calls.iter().map(|c| c.id.clone()))
                }
                ModelRole::Tool => {
                    let id = message
                        .tool_call_id
                        .clone()
                        .expect("tool result carries its call id");
                    assert!(
                        open_calls.contains(&id),
                        "result {id} must answer a retained call"
                    );
                    open_calls.retain(|open| open != &id);
                }
                _ => {}
            }
        }
        assert!(
            open_calls.is_empty(),
            "no retained call may lose its result: {open_calls:?}"
        );
    }

    #[test]
    fn model_input_wire_view_renders_the_checkpoint_note() {
        let full = framed_turn(8);
        let (tail, compacted) = full.checkpoint_tail(TURN_FRAME_KEEP_EXCHANGES);
        assert_eq!(compacted, 2);
        let input = ModelInput {
            layout: PromptLayout::Legacy,
            system_policy: Vec::new(),
            current_state_frame: Vec::new(),
            focus_frame: None,
            context_frame: Vec::new(),
            turn_frame: tail,
            tool_schemas: Vec::new(),
            turn_checkpoint: Some(TurnCheckpoint {
                compacted_exchanges: compacted,
                receipts: Vec::new(),
            }),
        };
        let messages = input.turn_frame_wire_messages();
        assert_eq!(messages.len(), 1 + 1 + 12);
        assert!(
            messages[1]
                .content
                .starts_with("TURN CHECKPOINT: 2 earlier")
        );
        // An input without a checkpoint renders the plain frame.
        let plain = ModelInput {
            turn_frame: framed_turn(2),
            turn_checkpoint: None,
            ..Default::default()
        };
        assert_eq!(plain.turn_frame_wire_messages().len(), 5);
    }

    #[test]
    fn message_serde_roundtrips_and_old_format_parses() {
        let message = ModelMessage::tool_result("call-9", "fs.read", "ok");
        let json = serde_json::to_string(&message).unwrap();
        let parsed: ModelMessage = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.tool_call_id.as_deref(), Some("call-9"));

        // A message serialized before tool frames existed still parses.
        // ModelRole derives PascalCase wire names (e.g. "User", not "user").
        let old = r#"{"role":"User","content":"hello"}"#;
        let parsed: ModelMessage = serde_json::from_str(old).unwrap();
        assert_eq!(parsed.role, ModelRole::User);
        assert!(parsed.tool_calls.is_empty());
        assert!(parsed.tool_call_id.is_none());

        // Checkpoints written before receipt indexes existed remain valid.
        let checkpoint: TurnCheckpoint =
            serde_json::from_str(r#"{"compacted_exchanges":3}"#).unwrap();
        assert_eq!(checkpoint.compacted_exchanges, 3);
        assert!(checkpoint.receipts.is_empty());
    }

    #[test]
    fn empty_zero_usage_is_structurally_empty() {
        let usage = ModelUsage::default();
        assert_eq!(
            completion_validity("", &[], &usage),
            ModelCompletionValidity::StructurallyEmpty
        );
        let billed = ModelUsage {
            input_tokens: Some(12),
            output_tokens: Some(0),
            cached_input_tokens: Some(4),
            cache_write_input_tokens: None,
            attempts: 1,
            retries: 0,
            ..Default::default()
        };
        assert_eq!(
            completion_validity("", &[], &billed),
            ModelCompletionValidity::Meaningful
        );
        assert_eq!(
            completion_validity("ok", &[], &usage),
            ModelCompletionValidity::Meaningful
        );
        assert_eq!(
            completion_validity("", &[tool_call("c1")], &usage),
            ModelCompletionValidity::Meaningful
        );
    }

    #[test]
    fn restored_protocol_tokens_default_zero_and_round_trip() {
        // Older events without the layer keep zero and unchanged accounting.
        let legacy: PromptLayerCosts =
            serde_json::from_str(r#"{"system_tokens":1,"runtime_facts_tokens":1,"task_anchor_tokens":1,"task_progress_tokens":1,"current_focus_tokens":1,"historical_context_tokens":40,"turn_frame_tokens":1,"tool_schema_tokens":1}"#)
                .expect("legacy prompt layers decode");
        assert_eq!(legacy.restored_protocol_tokens, 0);
        assert_eq!(legacy.historical_context_tokens, 40);

        let current = PromptLayerCosts {
            system_tokens: 1,
            runtime_facts_tokens: 1,
            task_anchor_tokens: 1,
            task_progress_tokens: 1,
            current_focus_tokens: 1,
            historical_context_tokens: 30,
            restored_protocol_tokens: 10,
            turn_frame_tokens: 1,
            tool_schema_tokens: 1,
            tool_catalog_index_tokens: 0,
        };
        let wire = serde_json::to_value(current).unwrap();
        assert_eq!(wire["restored_protocol_tokens"], 10);
        let back: PromptLayerCosts = serde_json::from_value(wire).unwrap();
        assert_eq!(back, current);

        let summed = current.saturating_add(current);
        assert_eq!(summed.historical_context_tokens, 60);
        assert_eq!(summed.restored_protocol_tokens, 20);
    }
    /// CORE-4: the evidence identity of a round derives from the report.
    #[test]
    fn usage_identity_follows_the_provider_report() {
        let full = ModelUsage {
            input_tokens: Some(10),
            output_tokens: Some(2),
            cached_input_tokens: None,
            cache_write_input_tokens: None,
            attempts: 1,
            retries: 0,
            ..Default::default()
        };
        assert_eq!(full.usage_identity(), UsageIdentity::Observed);
        // A missing counter means unknown even when the other side is
        // reported: the bill is incomplete, never fully observed.
        let partial = ModelUsage {
            input_tokens: Some(10),
            output_tokens: None,
            ..full.clone()
        };
        assert_eq!(partial.usage_identity(), UsageIdentity::Unknown);
        let none = ModelUsage::default();
        assert_eq!(none.usage_identity(), UsageIdentity::Unknown);
    }

    /// CORE-4: legacy ModelUsed JSON (pre-identity) decodes with the
    /// honest `unknown` default — old zeros must not read as observed
    /// consumption.
    #[test]
    fn legacy_model_used_events_default_to_unknown_identity() {
        let legacy = r#"{"type":"model_used","input_tokens":900,"output_tokens":30,
            "cached_input_tokens":100,"attempts":1,"retries":0}"#;
        let event: crate::RuntimeEvent = serde_json::from_str(legacy).unwrap();
        let crate::RuntimeEvent::ModelUsed {
            usage_identity,
            usage,
            input_tokens,
            ..
        } = event
        else {
            panic!("wrong variant");
        };
        assert_eq!(usage_identity, UsageIdentity::Unknown);
        assert!(usage.is_none(), "legacy rows carry no typed report");
        assert_eq!(input_tokens, 900);
    }

    /// COST-6 (R2-10): a `ModelUsage` written before the miss field existed
    /// decodes with `cache_miss_input_tokens = None` (not zero), and the
    /// unreported cache buckets stay off the wire so historical bytes stay
    /// byte-stable.
    #[test]
    fn legacy_usage_json_decodes_with_an_unreported_miss_counter() {
        let legacy = r#"{"input_tokens":100,"output_tokens":5,"cached_input_tokens":80}"#;
        let usage: ModelUsage = serde_json::from_str(legacy).unwrap();
        assert_eq!(usage.cached_input_tokens, Some(80));
        assert_eq!(usage.cache_write_input_tokens, None);
        assert_eq!(usage.cache_miss_input_tokens, None);

        let wire = serde_json::to_string(&usage).unwrap();
        assert!(
            !wire.contains("cache_miss_input_tokens"),
            "an unreported miss counter must not appear on the wire: {wire}"
        );
        assert!(
            !wire.contains("cache_write_input_tokens"),
            "an unreported write counter must not appear on the wire: {wire}"
        );

        let full = ModelUsage {
            cache_write_input_tokens: Some(10),
            cache_miss_input_tokens: Some(20),
            ..usage
        };
        let wire = serde_json::to_value(full).unwrap();
        assert_eq!(wire["cache_write_input_tokens"], 10);
        assert_eq!(wire["cache_miss_input_tokens"], 20);
    }
}
