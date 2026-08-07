use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::{AgentResult, ContextItemId, ModelMessage, TaskId, ToolOutput};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ContextKind {
    Goal,
    Constraint,
    Decision,
    UserMessage,
    AssistantMessage,
    ToolObservation,
    FileObservation,
    Error,
    Summary,
    Note,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ContextScope {
    Message,
    Turn,
    Task,
    Session,
    Pinned,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ContextRetention {
    Ephemeral,
    Working,
    Durable,
    Pinned,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ContextState {
    Active,
    Cooling,
    Archived,
    Dropped,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextItem {
    pub id: ContextItemId,
    pub task_id: Option<TaskId>,
    pub content: String,
    pub kind: ContextKind,
    pub scope: ContextScope,
    pub retention: ContextRetention,
    pub state: ContextState,
    pub importance: f32,
    pub relevance: f32,
    pub created_tick: u64,
    pub last_access_tick: u64,
    pub access_count: u32,
    #[serde(default)]
    pub created_turn: u64,
    #[serde(default)]
    pub last_access_turn: u64,
    #[serde(default)]
    pub dependencies: Vec<ContextItemId>,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FocusState {
    pub task_id: TaskId,
    pub goal: String,
    pub current_query: String,
    pub phase: String,
    #[serde(default)]
    pub active_entities: Vec<String>,
    pub generation: u64,
}

impl FocusState {
    pub fn new(goal: impl Into<String>) -> Self {
        let goal = goal.into();
        Self {
            task_id: TaskId::new(),
            current_query: goal.clone(),
            goal,
            phase: "working".to_string(),
            active_entities: Vec::new(),
            generation: 0,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContextIngress {
    UserMessage {
        content: String,
    },
    AssistantMessage {
        content: String,
    },
    ToolObservation {
        output: ToolOutput,
    },
    FocusChanged {
        focus: FocusState,
    },
    Pin {
        content: String,
        kind: ContextKind,
    },
    TaskCompleted {
        task_id: Option<TaskId>,
        summary: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum ContextMaintenanceTrigger {
    #[default]
    UserInput,
    BeforeModel,
    AfterModel,
    AfterTool,
    FocusChanged,
    TaskCompleted,
    Checkpoint,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextBuildRequest {
    pub system_prompt: String,
    pub current_input: String,
    pub budget_tokens: usize,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ScoreBreakdown {
    pub importance: f32,
    pub focus_match: f32,
    pub recency: f32,
    pub access: f32,
    pub scope_bonus: f32,
    pub retention_bonus: f32,
    /// P4: reward for an item whose entities are hot in the current working
    /// set (user message + recent tool observations).
    #[serde(default)]
    pub entity_affinity: f32,
    pub total: f32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextSelection {
    pub item_id: ContextItemId,
    pub score: f32,
    pub approx_tokens: usize,
    pub reason: String,
    #[serde(default)]
    pub breakdown: ScoreBreakdown,
}

/// A single observed lifecycle state transition produced by one maintenance pass.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextStateTransition {
    pub item_id: ContextItemId,
    pub kind: ContextKind,
    pub scope: ContextScope,
    pub from: ContextState,
    pub to: ContextState,
    pub turn: u64,
    pub reason: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ContextDiagnostics {
    pub total_items: usize,
    pub active_items: usize,
    pub cooling_items: usize,
    pub archived_items: usize,
    pub dropped_items: usize,
    pub approx_active_tokens: usize,
    pub focus_generation: u64,
    #[serde(default)]
    pub turn: u64,
    #[serde(default)]
    pub tool_round: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextSnapshot {
    pub messages: Vec<ModelMessage>,
    pub selected: Vec<ContextSelection>,
    pub approx_tokens: usize,
    pub diagnostics: ContextDiagnostics,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ContextMaintenanceReport {
    pub promoted: usize,
    pub cooled: usize,
    pub archived: usize,
    pub dropped: usize,
    #[serde(default)]
    pub turn: u64,
    #[serde(default)]
    pub transitions: Vec<ContextStateTransition>,
    pub diagnostics: ContextDiagnostics,
}

/// A bounded, UI/replay-friendly projection of one context item.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextItemSummary {
    pub id: ContextItemId,
    pub kind: ContextKind,
    pub scope: ContextScope,
    pub state: ContextState,
    pub importance: f32,
    pub relevance: f32,
    pub created_tick: u64,
    pub created_turn: u64,
    pub last_access_turn: u64,
    pub access_count: u32,
    /// P4: ids of prior items this item explicitly depends on (shared entities).
    #[serde(default)]
    pub dependencies: Vec<ContextItemId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
}

#[async_trait]
pub trait ContextEngine: Send + Sync {
    async fn ingest(&self, ingress: ContextIngress) -> AgentResult<()>;

    async fn maintain(
        &self,
        trigger: ContextMaintenanceTrigger,
    ) -> AgentResult<ContextMaintenanceReport>;

    async fn build_snapshot(&self, request: ContextBuildRequest) -> AgentResult<ContextSnapshot>;

    async fn diagnostics(&self) -> AgentResult<ContextDiagnostics>;

    /// Bounded projection of live items, oldest first, capped at `limit`.
    async fn inspect(&self, limit: usize) -> AgentResult<Vec<ContextItemSummary>>;

    /// Export the current runtime state (separate from the event journal).
    async fn checkpoint(&self) -> AgentResult<serde_json::Value>;

    /// Replace runtime state from a previously exported checkpoint.
    async fn restore(&self, data: serde_json::Value) -> AgentResult<()>;
}
